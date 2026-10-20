//! Generic monitoring behavior: what a link adds to an app and the files the
//! collector reads.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_yaml_ng::{Mapping, Value};

use super::KIND;
use crate::config::app::AppConfig;
use crate::connector::shell_quote;
use crate::shared::{
    AppCtx, Contribution, ItemWriter, Resolution, Role, SharedAppConfig, check_link_environment,
    connectors_dir, link_value, shared_dir,
};

/// Values a link can map to app variables.
const LINK_VALUES: [&str; 5] = [
    "otlp_endpoint",
    "otlp_grpc_endpoint",
    "otlp_protocol",
    "service_name",
    "resource_attributes",
];

/// Docker network of one monitoring app; linked services join it.
pub fn network_name(name: &str) -> String {
    format!("compost-monitoring-{name}")
}

/// Folder of the scrape target files, one per link, read by the collector.
pub fn targets_dir(name: &str) -> String {
    format!("{}/collector/targets", shared_dir(KIND, name))
}

/// Where apps send data to a monitoring app.
pub struct Endpoint {
    /// OTLP over HTTP, e.g. `http://monitoring:4318`.
    pub otlp_http: String,
    /// OTLP over gRPC, e.g. `http://monitoring:4317`.
    pub otlp_grpc: String,
    /// Port on the host's loopback interface where the docker log driver
    /// delivers output (fluentd forward protocol).
    pub forward_port: u16,
}

/// An entry under `services.monitoring` in compost.yaml.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MonitoringLink {
    pub name: String,
    /// Use this shared app; needed when the server has several.
    #[serde(default)]
    pub instance: Option<String>,
    /// Skip the link when the server runs no monitoring app.
    #[serde(default)]
    pub optional: bool,
    /// `app_services` whose output is collected and which receive the variables.
    #[serde(default)]
    pub targets: Vec<String>,
    /// Collect the docker output of the targets.
    #[serde(default = "yes")]
    pub logs: bool,
    /// Prometheus endpoints to scrape.
    #[serde(default)]
    pub metrics: Vec<Metrics>,
    /// App variable name → link value (one of [`LINK_VALUES`]).
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Metrics {
    /// `app_services` name.
    pub target: String,
    pub port: u16,
    #[serde(default = "metrics_path")]
    pub path: String,
}

fn yes() -> bool {
    true
}

fn metrics_path() -> String {
    "/metrics".to_string()
}

/// `connectors/monitoring/<name>/<instance>-<entry>.yaml`.
#[derive(Debug, Serialize, Deserialize)]
pub struct MonitoringRecord {
    pub app: String,
    pub prefix: String,
    pub entry: String,
    /// Compose services whose output is collected.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub logs: Vec<String>,
    /// Scraped endpoints, `<host>:<port><path>`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub metrics: Vec<String>,
}

pub fn validate_link(link: &MonitoringLink, app: &AppConfig) -> Result<()> {
    let check_target = |target: &str| {
        ensure!(
            app.app_service(target).is_some(),
            "target `{target}` is not in app_services"
        );
        Ok(())
    };
    let mut targets = HashSet::new();
    for target in &link.targets {
        check_target(target)?;
        ensure!(targets.insert(target), "target `{target}` is listed twice");
    }
    ensure!(
        !link.targets.is_empty() || !link.metrics.is_empty(),
        "nothing to monitor: give `targets`, `metrics` or both"
    );
    ensure!(
        link.environment.is_empty() || !link.targets.is_empty(),
        "environment: variables are set on `targets`, which is empty"
    );
    let mut endpoints = HashSet::new();
    for metrics in &link.metrics {
        check_target(&metrics.target).context("metrics")?;
        ensure!(metrics.port > 0, "metrics: port must not be 0");
        ensure!(
            metrics.path.starts_with('/') && !metrics.path.contains(char::is_whitespace),
            "metrics: path `{}` must start with `/`",
            metrics.path
        );
        ensure!(
            endpoints.insert((&metrics.target, metrics.port, &metrics.path)),
            "metrics: {}:{}{} is listed twice",
            metrics.target,
            metrics.port,
            metrics.path
        );
    }
    check_link_environment(&link.environment, &LINK_VALUES)?;
    Ok(())
}

/// The pinned monitoring app, or the only one.
pub fn resolve<'s>(
    link: &MonitoringLink,
    available: &'s [SharedAppConfig],
) -> Result<Resolution<'s>> {
    let matching: Vec<&SharedAppConfig> = available
        .iter()
        .filter(|app| link.instance.as_ref().is_none_or(|pin| *pin == app.name))
        .collect();
    match matching.as_slice() {
        [found] => Ok(Resolution::Use(found)),
        [] if link.optional => Ok(Resolution::Skip(
            "optional and no monitoring app is running".to_string(),
        )),
        [] => match &link.instance {
            Some(pin) => bail!("no running monitoring app named `{pin}` on the server"),
            None => bail!(
                "no monitoring app is running on the server; define one in compost-server.yaml and run `compost server deploy --apply`"
            ),
        },
        several => {
            let names: Vec<&str> = several.iter().map(|app| app.name.as_str()).collect();
            bail!(
                "several monitoring apps run ({}); choose one with `instance`",
                names.join(", ")
            )
        }
    }
}

/// The `logging` section that sends a service's output to the collector.
/// Async: containers start even when the collector does not run, and
/// `docker logs` keeps working through docker's local cache.
fn logging(endpoint: &Endpoint, tag: &str) -> Mapping {
    let options: Mapping = [
        (
            "fluentd-address",
            format!("127.0.0.1:{}", endpoint.forward_port),
        ),
        ("fluentd-async", "true".to_string()),
        ("tag", tag.to_string()),
        (
            "labels",
            "com.docker.compose.project,com.docker.compose.service".to_string(),
        ),
    ]
    .into_iter()
    .map(|(key, value)| (Value::from(key), Value::from(value)))
    .collect();
    Mapping::from_iter([
        ("driver".into(), "fluentd".into()),
        ("options".into(), Value::Mapping(options)),
    ])
}

/// Join the monitoring network, set the mapped variables and the log
/// driver, write the link record.
pub fn prepare_application(
    ctx: &AppCtx,
    link: &MonitoringLink,
    endpoint: &Endpoint,
    out: &mut Contribution,
) -> Result<()> {
    let network = network_name(&ctx.shared.name);
    let instance = ctx.app.instance();
    let mut joined = BTreeSet::new();
    for target in link
        .targets
        .iter()
        .chain(link.metrics.iter().map(|m| &m.target))
    {
        joined.insert(ctx.compose_service(target)?);
    }
    for service in &joined {
        out.join_network(service, &network, Some(ctx.service_alias(service)));
    }

    let mut logs = Vec::new();
    for target in &link.targets {
        let service = ctx.compose_service(target)?;
        let values = BTreeMap::from([
            ("otlp_endpoint", endpoint.otlp_http.clone()),
            ("otlp_grpc_endpoint", endpoint.otlp_grpc.clone()),
            ("otlp_protocol", "http/protobuf".to_string()),
            ("service_name", service.to_string()),
            (
                "resource_attributes",
                format!(
                    "service.namespace={instance},deployment.environment={}",
                    ctx.app.prefix
                ),
            ),
        ]);
        for (variable, value) in &link.environment {
            out.set_env(service, variable, link_value(value, &values)?)?;
        }
        if link.logs {
            let label = ctx.service_label(target)?;
            out.set_logging(service, logging(endpoint, &format!("{instance}.{label}")))?;
            logs.push(label);
        }
    }

    let record = MonitoringRecord {
        app: ctx.app.name.clone(),
        prefix: ctx.app.prefix.clone(),
        entry: link.name.clone(),
        logs,
        metrics: scrape_targets(ctx, link)?
            .iter()
            .map(|t| format!("{}{}", t.address, t.path))
            .collect(),
    };
    let text = serde_yaml_ng::to_string(&record)?;
    ctx.server
        .connector
        .write_file(&record_path(ctx, &link.name), text.as_bytes(), 0o644)
}

struct ScrapeTarget {
    service: String,
    /// `<alias>:<port>` on the monitoring network.
    address: String,
    path: String,
}

fn scrape_targets(ctx: &AppCtx, link: &MonitoringLink) -> Result<Vec<ScrapeTarget>> {
    link.metrics
        .iter()
        .map(|metrics| {
            let service = ctx.compose_service(&metrics.target)?;
            Ok(ScrapeTarget {
                service: ctx.service_label(&metrics.target)?,
                address: format!("{}:{}", ctx.service_alias(service), metrics.port),
                path: metrics.path.clone(),
            })
        })
        .collect()
}

/// Write the scrape targets once the services run; the collector picks the
/// file up by itself (Prometheus file-based discovery).
pub fn after_apply_application(ctx: &AppCtx, link: &MonitoringLink) -> Result<()> {
    let path = targets_path(ctx, &link.name);
    let targets = scrape_targets(ctx, link)?;
    if targets.is_empty() {
        ctx.server
            .connector
            .exec(&format!("rm -f -- {}", shell_quote(&path)))?;
        return Ok(());
    }
    let groups: Vec<Value> = targets
        .iter()
        .map(|target| {
            let labels = BTreeMap::from([
                ("__metrics_path__", target.path.clone()),
                ("compost_app", ctx.app.name.clone()),
                ("compost_prefix", ctx.app.prefix.clone()),
                ("compost_service", target.service.clone()),
            ]);
            serde_yaml_ng::to_value(BTreeMap::from([
                ("targets", serde_yaml_ng::to_value([&target.address])?),
                ("labels", serde_yaml_ng::to_value(labels)?),
            ]))
        })
        .collect::<Result<_, _>>()?;
    let text = format!(
        "# Generated by compost for {} ({}). Do not edit; run `compost app deploy`.\n{}",
        ctx.app.instance(),
        link.name,
        serde_yaml_ng::to_string(&groups)?
    );
    ctx.server
        .connector
        .write_file(&path, text.as_bytes(), 0o644)
}

fn record_path(ctx: &AppCtx, entry: &str) -> String {
    ctx.server.path(&format!(
        "{}/{}.yaml",
        connectors_dir(KIND, &ctx.shared.name),
        ctx.link_id(entry)
    ))
}

fn targets_path(ctx: &AppCtx, entry: &str) -> String {
    ctx.server.path(&format!(
        "{}/{}.yaml",
        targets_dir(&ctx.shared.name),
        ctx.link_id(entry)
    ))
}

pub fn read_record(ctx: &AppCtx, entry: &str) -> Result<Option<MonitoringRecord>> {
    let path = record_path(ctx, entry);
    match ctx.server.connector.read_file(&path)? {
        None => Ok(None),
        Some(text) => Ok(Some(
            serde_yaml_ng::from_str(&text).with_context(|| format!("invalid {path}"))?,
        )),
    }
}

/// e.g. `logs backend, web; metrics myapp-main-backend:8000/metrics`.
pub fn describe_link(ctx: &AppCtx, entry: &str) -> Result<String> {
    let Some(record) = read_record(ctx, entry)? else {
        return Ok("no link record".to_string());
    };
    let mut parts = Vec::new();
    if !record.logs.is_empty() {
        parts.push(format!("logs {}", record.logs.join(", ")));
    }
    if !record.metrics.is_empty() {
        parts.push(format!("metrics {}", record.metrics.join(", ")));
    }
    if parts.is_empty() {
        parts.push("variables only".to_string());
    }
    Ok(parts.join("; "))
}

/// `<entry>.yaml`: the link record.
pub fn backup_link(ctx: &AppCtx, entry: &str, out: &mut ItemWriter) -> Result<()> {
    let record = ctx
        .server
        .connector
        .read_file(&record_path(ctx, entry))?
        .with_context(|| format!("no link record for `{entry}`; deploy the app again"))?;
    out.file(&format!("{entry}.yaml"), Role::Meta, record.as_bytes())
}

/// Delete the record and the scrape targets. Collected data stays.
pub fn remove_application(ctx: &AppCtx, entry: &str) -> Result<()> {
    let paths = [record_path(ctx, entry), targets_path(ctx, entry)];
    let paths: Vec<String> = paths.iter().map(|p| shell_quote(p)).collect();
    ctx.server
        .connector
        .exec(&format!("rm -f -- {}", paths.join(" ")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::local::Local;
    use crate::shared::ServerCtx;
    use tempfile::TempDir;

    fn app() -> AppConfig {
        serde_yaml_ng::from_str(
            "{name: my-app, prefix: staging, app_services: [{name: backend, service: api}, {name: web, service: web}]}",
        )
        .unwrap()
    }

    fn link(text: &str) -> MonitoringLink {
        serde_yaml_ng::from_str(text).unwrap()
    }

    #[test]
    fn validates_link() {
        validate_link(
            &link(
                "{name: m, targets: [backend, web], metrics: [{target: backend, port: 9100}], environment: {OTEL_EXPORTER_OTLP_ENDPOINT: otlp_endpoint}}",
            ),
            &app(),
        )
        .unwrap();
        validate_link(
            &link("{name: m, metrics: [{target: web, port: 80}]}"),
            &app(),
        )
        .unwrap();
        for (text, expected) in [
            ("{name: m}", "nothing to monitor"),
            (
                "{name: m, targets: [x]}",
                "target `x` is not in app_services",
            ),
            ("{name: m, targets: [web, web]}", "listed twice"),
            (
                "{name: m, metrics: [{target: x, port: 1}]}",
                "not in app_services",
            ),
            ("{name: m, metrics: [{target: web, port: 0}]}", "port"),
            (
                "{name: m, metrics: [{target: web, port: 1, path: metrics}]}",
                "must start with `/`",
            ),
            (
                "{name: m, targets: [web], environment: {X: db_url}}",
                "unknown value",
            ),
            (
                "{name: m, targets: [web], environment: {'1X': otlp_endpoint}}",
                "not a valid variable name",
            ),
            (
                "{name: m, metrics: [{target: web, port: 1}], environment: {X: otlp_endpoint}}",
                "`targets`, which is empty",
            ),
        ] {
            let err = format!("{:#}", validate_link(&link(text), &app()).unwrap_err());
            assert!(err.contains(expected), "{text}: {err}");
        }
    }

    #[test]
    fn resolves_pinned_only_or_skips_optional() {
        let available: Vec<SharedAppConfig> =
            serde_yaml_ng::from_str("[{name: a, type: openobserve}, {name: b, type: openobserve}]")
                .unwrap();
        let pick = |text: &str, available: &[SharedAppConfig]| match resolve(&link(text), available)
        {
            Ok(Resolution::Use(found)) => found.name.clone(),
            Ok(Resolution::Skip(_)) => "skipped".into(),
            Err(err) => err.to_string(),
        };
        assert_eq!(pick("{name: m, instance: b}", &available), "b");
        assert_eq!(pick("{name: m}", &available[..1]), "a");
        assert!(pick("{name: m}", &available).contains("several monitoring apps run (a, b)"));
        assert_eq!(pick("{name: m, optional: true}", &[]), "skipped");
        assert!(pick("{name: m}", &[]).contains("no monitoring app is running"));
        assert!(pick("{name: m, instance: c}", &available).contains("named `c`"));
    }

    const LOCAL: Local = Local { verbose: false };

    #[test]
    fn prepares_networks_variables_logging_and_files() {
        let home = TempDir::new().unwrap();
        let server = ServerCtx {
            connector: &LOCAL,
            home: home.path().to_str().unwrap().to_string(),
        };
        let shared: SharedAppConfig =
            serde_yaml_ng::from_str("{name: o2, type: openobserve}").unwrap();
        let app = app();
        let ctx = AppCtx {
            server: &server,
            app: &app,
            shared: &shared,
            peers: &[],
        };
        let endpoint = Endpoint {
            otlp_http: "http://o2:4318".into(),
            otlp_grpc: "http://o2:4317".into(),
            forward_port: 24224,
        };
        let link = link(
            "{name: telemetry, targets: [web], metrics: [{target: backend, port: 9100}], environment: {OTEL_EXPORTER_OTLP_ENDPOINT: otlp_endpoint, OTEL_SERVICE_NAME: service_name, OTEL_RESOURCE_ATTRIBUTES: resource_attributes}}",
        );
        let mut out = Contribution::default();
        prepare_application(&ctx, &link, &endpoint, &mut out).unwrap();
        let compose =
            crate::config::app::ComposeFile::parse("services: {api: {image: x}, web: {image: x}}")
                .unwrap();
        let yaml: Value = serde_yaml_ng::from_str(&out.override_yaml(&compose).unwrap()).unwrap();
        let web = &yaml["services"]["web"];
        assert_eq!(
            web["networks"]["compost-monitoring-o2"]["aliases"][0],
            "my-app-staging-web"
        );
        assert_eq!(
            web["environment"]["OTEL_EXPORTER_OTLP_ENDPOINT"],
            "http://o2:4318"
        );
        assert_eq!(web["environment"]["OTEL_SERVICE_NAME"], "web");
        assert_eq!(
            web["environment"]["OTEL_RESOURCE_ATTRIBUTES"],
            "service.namespace=my-app-staging,deployment.environment=staging"
        );
        assert_eq!(web["logging"]["driver"], "fluentd");
        assert_eq!(
            web["logging"]["options"]["fluentd-address"],
            "127.0.0.1:24224"
        );
        assert_eq!(web["logging"]["options"]["tag"], "my-app-staging.web");
        // Scraped only: on the network, no logging, no variables.
        let api = &yaml["services"]["api"];
        assert!(api["networks"]["compost-monitoring-o2"].is_mapping());
        assert!(api.get("logging").is_none() && api.get("environment").is_none());

        assert_eq!(
            describe_link(&ctx, "telemetry").unwrap(),
            "logs web; metrics my-app-staging-api:9100/metrics"
        );
        after_apply_application(&ctx, &link).unwrap();
        let targets = home
            .path()
            .join("shared/monitoring/o2/collector/targets/my-app-staging-telemetry.yaml");
        let text = std::fs::read_to_string(&targets).unwrap();
        let groups: Value = serde_yaml_ng::from_str(&text).unwrap();
        assert_eq!(groups[0]["targets"][0], "my-app-staging-api:9100");
        assert_eq!(groups[0]["labels"]["__metrics_path__"], "/metrics");
        assert_eq!(groups[0]["labels"]["compost_service"], "api");

        remove_application(&ctx, "telemetry").unwrap();
        assert!(!targets.exists());
        assert!(read_record(&ctx, "telemetry").unwrap().is_none());
    }
}
