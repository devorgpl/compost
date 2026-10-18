//! Generic workflow behavior, used by every workflow type unless it overrides it.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail, ensure};
use semver::VersionReq;
use serde::{Deserialize, Serialize};

use super::KIND;
use crate::config::app::AppConfig;
use crate::connector::shell_quote;
use crate::shared::{
    AppCtx, Contribution, ItemWriter, Resolution, Role, SharedAppConfig, connectors_dir,
    is_variable_name, resolve_versioned,
};

/// Values a link can map to app variables.
const LINK_VALUES: [&str; 4] = ["wf_host", "wf_port", "wf_address", "wf_namespace"];

/// Kept when a link gives no `retention_days`.
pub const DEFAULT_RETENTION_DAYS: u32 = 3;

/// Docker network of one workflow server; apps linked to it join this network.
pub fn network_name(server: &str) -> String {
    format!("compost-wf-{server}")
}

/// An entry under `services.workflow` in compost.yaml.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowLink {
    pub name: String,
    #[serde(rename = "type")]
    pub type_name: String,
    /// Semver requirement on the server's major version.
    #[serde(default = "any_version")]
    pub version: String,
    /// Use this shared app, instead of picking one by type and version.
    #[serde(default)]
    pub instance: Option<String>,
    /// Where the app's workflows live. Default `<app>-<prefix>`.
    #[serde(default)]
    pub namespace: Option<String>,
    /// How long closed workflows are kept. Default [`DEFAULT_RETENTION_DAYS`].
    #[serde(default)]
    pub retention_days: Option<u32>,
    /// `app_services` that receive the variables and join the server network.
    #[serde(default)]
    pub targets: Vec<String>,
    /// App variable name → link value (one of [`LINK_VALUES`]).
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
}

fn any_version() -> String {
    "*".to_string()
}

/// `connectors/workflow/<server>/<instance>-<entry>.yaml`.
#[derive(Debug, Serialize, Deserialize)]
pub struct WorkflowRecord {
    pub app: String,
    pub prefix: String,
    pub entry: String,
    pub namespace: String,
    pub retention_days: u32,
}

/// Where apps reach a server on its network.
pub struct Endpoint {
    pub host: String,
    pub port: u16,
}

pub fn validate_config(config: &SharedAppConfig) -> Result<()> {
    if let Some(image) = &config.image {
        ensure!(!image.trim().is_empty(), "image must not be empty");
    }
    Ok(())
}

pub fn validate_link(link: &WorkflowLink, app: &AppConfig) -> Result<()> {
    VersionReq::parse(&link.version)
        .with_context(|| format!("version `{}` is not a valid requirement", link.version))?;
    ensure!(
        !link.targets.is_empty(),
        "targets: at least one app service is required"
    );
    for target in &link.targets {
        ensure!(
            app.app_service(target).is_some(),
            "target `{target}` is not in app_services"
        );
    }
    for (variable, value) in &link.environment {
        ensure!(
            is_variable_name(variable),
            "`{variable}` is not a valid variable name"
        );
        ensure!(
            LINK_VALUES.contains(&value.as_str()),
            "environment.{variable}: unknown value `{value}` (known: {})",
            LINK_VALUES.join(", ")
        );
    }
    let namespace = namespace(app, link);
    ensure!(
        is_namespace(&namespace),
        "namespace `{namespace}` must match ^[a-z0-9][a-z0-9._-]{{0,99}}$"
    );
    if let Some(days) = link.retention_days {
        ensure!(days >= 1, "retention_days must be at least 1");
    }
    Ok(())
}

/// `^[a-z0-9][a-z0-9._-]{0,99}$`: safe in single-quoted shell words.
pub fn is_namespace(value: &str) -> bool {
    let mut chars = value.chars();
    value.len() <= 100
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._-".contains(c))
}

/// The link's namespace: from the link, or `<app>-<prefix>`.
pub fn namespace(app: &AppConfig, link: &WorkflowLink) -> String {
    link.namespace.clone().unwrap_or_else(|| app.instance())
}

/// A server of the requested type whose major version matches.
pub fn resolve<'s>(
    link: &WorkflowLink,
    available: &'s [SharedAppConfig],
) -> Result<Resolution<'s>> {
    resolve_versioned(
        "workflow server",
        &link.type_name,
        &link.version,
        link.instance.as_deref(),
        available,
    )
    .map(Resolution::Use)
}

pub fn record(ctx: &AppCtx, link: &WorkflowLink) -> WorkflowRecord {
    WorkflowRecord {
        app: ctx.app.name.clone(),
        prefix: ctx.app.prefix.clone(),
        entry: link.name.clone(),
        namespace: namespace(ctx.app, link),
        retention_days: link.retention_days.unwrap_or(DEFAULT_RETENTION_DAYS),
    }
}

fn records_dir(ctx: &AppCtx) -> String {
    ctx.server.path(&connectors_dir(KIND, &ctx.shared.name))
}

fn record_path(ctx: &AppCtx, entry: &str) -> String {
    format!("{}/{}.yaml", records_dir(ctx), ctx.link_id(entry))
}

pub fn read_record(ctx: &AppCtx, entry: &str) -> Result<Option<WorkflowRecord>> {
    let path = record_path(ctx, entry);
    match ctx.server.connector.read_file(&path)? {
        None => Ok(None),
        Some(text) => Ok(Some(
            serde_yaml_ng::from_str(&text).with_context(|| format!("invalid {path}"))?,
        )),
    }
}

/// The link records of this server, except the one of `entry`.
fn other_records(ctx: &AppCtx, entry: &str) -> Result<Vec<WorkflowRecord>> {
    let dir = records_dir(ctx);
    let own = format!("{}.yaml", ctx.link_id(entry));
    let mut records = Vec::new();
    for file in ctx.server.connector.list_dir(&dir)? {
        if file == own || !file.ends_with(".yaml") {
            continue;
        }
        let path = format!("{dir}/{file}");
        let Some(text) = ctx.server.connector.read_file(&path)? else {
            continue;
        };
        records.push(serde_yaml_ng::from_str(&text).with_context(|| format!("invalid {path}"))?);
    }
    Ok(records)
}

/// Another link record of this server uses `namespace`.
pub fn namespace_used_elsewhere(ctx: &AppCtx, entry: &str, namespace: &str) -> Result<bool> {
    Ok(other_records(ctx, entry)?
        .iter()
        .any(|other| other.namespace == namespace))
}

/// A namespace has one retention: links sharing it must agree on it.
fn check_retention(ctx: &AppCtx, record: &WorkflowRecord) -> Result<()> {
    for other in other_records(ctx, &record.entry)? {
        if ctx.is_dropped(KIND, &other.app, &other.prefix, &other.entry) {
            continue;
        }
        if other.namespace == record.namespace && other.retention_days != record.retention_days {
            bail!(
                "namespace `{}` has retention_days {} in {}/{} ({}), here {}",
                record.namespace,
                other.retention_days,
                other.app,
                other.prefix,
                other.entry,
                record.retention_days
            );
        }
    }
    Ok(())
}

/// e.g. `namespace myapp-staging, retention 3 days`.
pub fn describe_link(ctx: &AppCtx, entry: &str) -> Result<String> {
    Ok(match read_record(ctx, entry)? {
        Some(record) => format!(
            "namespace {}, retention {} days",
            record.namespace, record.retention_days
        ),
        None => "no link record".to_string(),
    })
}

/// Join the server network, set the mapped variables, write the link record.
pub fn prepare_application(
    ctx: &AppCtx,
    link: &WorkflowLink,
    endpoint: &Endpoint,
    out: &mut Contribution,
) -> Result<()> {
    let record = record(ctx, link);
    check_retention(ctx, &record)?;
    let values: BTreeMap<&str, String> = BTreeMap::from([
        ("wf_host", endpoint.host.clone()),
        ("wf_port", endpoint.port.to_string()),
        ("wf_address", format!("{}:{}", endpoint.host, endpoint.port)),
        ("wf_namespace", record.namespace.clone()),
    ]);

    let network = network_name(&ctx.shared.name);
    for target in &link.targets {
        let service = ctx.compose_service(target)?;
        out.join_network(service, &network, None);
        for (variable, value) in &link.environment {
            out.set_env(service, variable, values[value.as_str()].clone())?;
        }
    }

    let text = serde_yaml_ng::to_string(&record)?;
    ctx.server
        .connector
        .write_file(&record_path(ctx, &link.name), text.as_bytes(), 0o644)
}

/// `<entry>.yaml`: the link record, for review. Workflows are in the
/// server's database and are saved with it.
pub fn backup_link(ctx: &AppCtx, entry: &str, out: &mut ItemWriter) -> Result<()> {
    let record = ctx
        .server
        .connector
        .read_file(&record_path(ctx, entry))?
        .with_context(|| format!("no link record for `{entry}`; deploy the app again"))?;
    out.file(&format!("{entry}.yaml"), Role::Meta, record.as_bytes())
}

/// Delete the link record. The namespace stays unless purged.
pub fn remove_application(ctx: &AppCtx, entry: &str) -> Result<()> {
    let path = shell_quote(&record_path(ctx, entry));
    ctx.server.connector.exec(&format!("rm -f -- {path}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> AppConfig {
        serde_yaml_ng::from_str(
            "{name: my-app, prefix: staging, app_services: [{name: worker, service: worker}]}",
        )
        .unwrap()
    }

    fn link(text: &str) -> WorkflowLink {
        serde_yaml_ng::from_str(text).unwrap()
    }

    const BASE: &str = "name: jobs, type: temporal, targets: [worker]";

    #[test]
    fn default_namespace_is_the_instance() {
        assert_eq!(
            namespace(&app(), &link(&format!("{{{BASE}}}"))),
            "my-app-staging"
        );
        assert_eq!(
            namespace(&app(), &link(&format!("{{{BASE}, namespace: billing}}"))),
            "billing"
        );
    }

    #[test]
    fn validates_link() {
        validate_link(
            &link(&format!(
                "{{{BASE}, retention_days: 30, environment: {{TEMPORAL_ADDRESS: wf_address}}}}"
            )),
            &app(),
        )
        .unwrap();
        for (extra, expected) in [
            (", targets: [x]", "not in app_services"),
            (", targets: []", "at least one"),
            (", environment: {X: db_url}", "unknown value"),
            (", namespace: My_NS", "namespace `My_NS`"),
            (", namespace: \"a'b\"", "namespace `a'b`"),
            (", retention_days: 0", "at least 1"),
            (", version: abc", "not a valid requirement"),
        ] {
            let targets = if extra.contains("targets:") {
                ""
            } else {
                ", targets: [worker]"
            };
            let text = format!("{{name: jobs, type: temporal{targets}{extra}}}");
            let err = validate_link(&link(&text), &app()).unwrap_err().to_string();
            assert!(err.contains(expected), "{text}: {err}");
        }
    }

    #[test]
    fn resolves_by_type_version_and_pin() {
        let available: Vec<SharedAppConfig> = serde_yaml_ng::from_str(
            "[{name: t1, type: temporal, version: 1}, {name: t2, type: temporal, version: 1}]",
        )
        .unwrap();
        let pick = |extra: &str| match resolve(&link(&format!("{{{BASE}{extra}}}")), &available) {
            Ok(Resolution::Use(found)) => found.name.clone(),
            Ok(Resolution::Skip(_)) => "skipped".into(),
            Err(err) => err.to_string(),
        };
        assert_eq!(pick(", instance: t2"), "t2");
        assert!(pick("").contains("several workflow servers match (t1, t2)"));
        assert!(pick(", version: '2'").contains("no running temporal workflow server"));
    }
}
