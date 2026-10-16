//! OpenObserve monitoring type: OpenObserve for storage and its UI, and an
//! OpenTelemetry Collector that receives what apps send.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail, ensure};
use minijinja::context;
use serde::Deserialize;
use serde_yaml_ng::Value;

use super::base::{self, Endpoint};
use super::{KIND, MonitoringType};
use crate::config::app::AppService;
use crate::connector::shell_quote;
use crate::shared::{
    Generated, ItemReader, ItemWriter, PublicUrl, Rendered, RenderedFile, Role, ServerCtx,
    SharedAppConfig, SharedLinks, connectors_dir, generate_password, parse_url, project_name,
    render_template, shared_dir,
};

pub struct OpenObserve;

const DEFAULT_VERSION: u32 = 1;
/// Image tags by major version.
const IMAGE_TAGS: &[(u32, &str)] = &[(1, "v1.1.0")];
const COLLECTOR_IMAGE: &str = "otel/opentelemetry-collector-contrib:0.162.0";
const SERVICE: &str = "openobserve";
const HTTP_PORT: u16 = 5080;
const DEFAULT_FORWARD_PORT: u16 = 24224;
const EMAIL_VARIABLE: &str = "ZO_ROOT_USER_EMAIL";
const PASSWORD_VARIABLE: &str = "ZO_ROOT_USER_PASSWORD";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct OpenObserveConfig {
    /// Public URL of the UI, e.g. `https://logs.example.com`; served by the ingress.
    url: Option<String>,
    root_email: String,
    /// Secret; generated on first deploy when absent.
    root_password: Option<String>,
    /// Port on the host's loopback interface for the docker log driver.
    forward_port: u16,
    /// CPU, memory and network of every container, read from the docker socket.
    container_metrics: bool,
    /// Days OpenObserve keeps data. Default: OpenObserve's own.
    retention_days: Option<u32>,
    collector_image: Option<String>,
}

impl Default for OpenObserveConfig {
    fn default() -> Self {
        Self {
            url: None,
            root_email: "admin@compost.local".to_string(),
            root_password: None,
            forward_port: DEFAULT_FORWARD_PORT,
            container_metrics: true,
            retention_days: None,
            collector_image: None,
        }
    }
}

impl OpenObserveConfig {
    fn public_url(&self) -> Result<Option<PublicUrl>> {
        self.url.as_deref().map(parse_url).transpose()
    }
}

impl MonitoringType for OpenObserve {
    fn type_name(&self) -> &'static str {
        "openobserve"
    }

    fn default_version(&self) -> Option<u32> {
        Some(DEFAULT_VERSION)
    }

    fn secret_keys(&self) -> &'static [&'static str] {
        &["root_password"]
    }

    fn generate(&self) -> Generated {
        Generated {
            local: include_str!("templates/generate/local.yaml"),
            prod: Some(include_str!("templates/generate/prod.yaml")),
            secrets: Some(include_str!("templates/generate/secrets.yaml")),
            notes: &[
                "openobserve: add `monitoring: {}` to other shared apps, and a `services.monitoring` link to apps, to collect their output and metrics.",
                "openobserve: locally, open http://logs.localhost:18080/ and log in with root_email and root_password from connectors/monitoring/monitoring/root-connector.yaml on the server.",
            ],
            requires: &["ingress/caddy"],
        }
    }

    fn validate_config(&self, config: &SharedAppConfig) -> Result<()> {
        if let Some(image) = &config.image {
            ensure!(!image.trim().is_empty(), "image must not be empty");
        }
        image(config)?;
        let o2: OpenObserveConfig = config.configuration()?;
        o2.public_url().context("url")?;
        ensure!(
            is_email(&o2.root_email),
            "root_email `{}` is not an email address",
            o2.root_email
        );
        if let Some(password) = &o2.root_password {
            check_password(password)?;
        }
        ensure!(o2.forward_port > 0, "forward_port must not be 0");
        ensure!(
            o2.retention_days != Some(0),
            "retention_days must be at least 1"
        );
        if let Some(image) = &o2.collector_image {
            ensure!(
                !image.trim().is_empty(),
                "collector_image must not be empty"
            );
        }
        Ok(())
    }

    fn render(&self, ctx: &ServerCtx, config: &SharedAppConfig) -> Result<Rendered> {
        let o2: OpenObserveConfig = config.configuration()?;
        let version = config.version.unwrap_or(DEFAULT_VERSION);
        let dir = shared_dir(KIND, &config.name);
        let secrets_path = secrets_path(&config.name);

        // OpenObserve creates the root user on its first start only, so the
        // email and password are kept from then on.
        let existing = ctx.read(&secrets_path)?;
        let existing_value = |name| existing.as_deref().and_then(|t| read_variable(t, name));
        if let Some(email) = existing_value(EMAIL_VARIABLE) {
            ensure!(
                email == o2.root_email,
                "root_email differs from the one already on the server ({email}); changing it is not supported"
            );
        }
        let password = match (o2.root_password.clone(), existing_value(PASSWORD_VARIABLE)) {
            (Some(new), Some(old)) if new != old => bail!(
                "root_password differs from the one already on the server; \
                 changing it is not supported yet (change it in OpenObserve)"
            ),
            (Some(password), _) | (None, Some(password)) => password,
            (None, None) => strong_password(),
        };

        // Run as the deploy user, so the files OpenObserve writes belong to it.
        let ids = ctx.connector.exec("id -u && id -g")?;
        let user = ids.split_whitespace().collect::<Vec<_>>().join(":");
        ensure!(
            user.split(':').count() == 2,
            "unexpected output of `id`: {ids}"
        );
        let socket_group = if o2.container_metrics {
            let group = ctx
                .connector
                .exec("stat -c %g /var/run/docker.sock")
                .context(
                    "container_metrics reads /var/run/docker.sock, which was not found; set `container_metrics: false`",
                )?;
            Some(group.trim().to_string())
        } else {
            None
        };

        let network = base::network_name(&config.name);
        let compose = render_template(
            include_str!("templates/compose.yaml.j2"),
            context! {
                image => image(config)?,
                collector_image => o2.collector_image.as_deref().unwrap_or(COLLECTOR_IMAGE),
                user,
                socket_group,
                forward_port => o2.forward_port,
                retention_days => o2.retention_days,
                network => network.as_str(),
                host => config.name.as_str(),
            },
        )?;
        let collector = render_template(
            include_str!("templates/collector.yaml.j2"),
            context! { container_metrics => o2.container_metrics },
        )?;
        let endpoint = self.endpoint(config)?;
        let root_connector = render_template(
            include_str!("templates/root-connector.yaml.j2"),
            context! {
                name => config.name.as_str(),
                version,
                network => network.as_str(),
                url => o2.public_url()?.map_or("none".to_string(), |u| u.url),
                otlp_endpoint => endpoint.otlp_http,
                root_email => o2.root_email.as_str(),
                password => password.as_str(),
            },
        )?;
        let o2_env = format!(
            "{EMAIL_VARIABLE}='{}'\n{PASSWORD_VARIABLE}='{password}'\n",
            o2.root_email
        );
        let collector_env = format!(
            "OPENOBSERVE_USER='{}'\nOPENOBSERVE_PASSWORD='{password}'\n",
            o2.root_email
        );

        Ok(Rendered {
            files: vec![
                RenderedFile::new(format!("{dir}/compose.yaml"), compose),
                RenderedFile::new(format!("{dir}/collector/config.yaml"), collector),
                RenderedFile::secret(secrets_path, o2_env),
                RenderedFile::secret(format!("{dir}/secrets/collector.env"), collector_env),
                RenderedFile::secret(
                    format!("{}/root-connector.yaml", connectors_dir(KIND, &config.name)),
                    root_connector,
                ),
            ],
            dirs: vec![format!("{dir}/data")],
            ..Default::default()
        })
    }

    /// The domain of `url` on the ingress, for the UI.
    fn shared_links(&self, config: &SharedAppConfig) -> Result<Option<SharedLinks>> {
        let o2: OpenObserveConfig = config.configuration()?;
        let Some(url) = o2.public_url()? else {
            return Ok(None);
        };
        let ingress: Value = serde_yaml_ng::from_str(&format!(
            "{{name: web, domains: ['{}'], routes: [{{name: openobserve, target: {SERVICE}, port: {HTTP_PORT}}}]}}",
            url.domain
        ))?;
        Ok(Some(SharedLinks {
            app_services: vec![AppService {
                name: SERVICE.to_string(),
                service: SERVICE.to_string(),
                environment: BTreeMap::new(),
            }],
            services: BTreeMap::from([("ingress".to_string(), vec![ingress])]),
        }))
    }

    /// The collector, by the shared app's name on the monitoring network.
    fn endpoint(&self, config: &SharedAppConfig) -> Result<Endpoint> {
        let o2: OpenObserveConfig = config.configuration()?;
        Ok(Endpoint {
            otlp_http: format!("http://{}:4318", config.name),
            otlp_grpc: format!("http://{}:4317", config.name),
            forward_port: o2.forward_port,
        })
    }

    /// Stored data and the root credentials it belongs to. OpenObserve keeps
    /// recent data in memory, so it is stopped while its folder is copied
    /// (the collector retries meanwhile). Its write-ahead log files are
    /// mostly holes, which GNU tar keeps small with `--sparse`.
    fn backup_shared(
        &self,
        ctx: &ServerCtx,
        config: &SharedAppConfig,
        out: &mut ItemWriter,
    ) -> Result<()> {
        let dir = shell_quote(&ctx.path(&shared_dir(KIND, &config.name)));
        let compose = format!(
            "docker compose -p {}",
            shell_quote(&project_name(KIND, &config.name))
        );
        out.stream(
            BACKUP_ITEM,
            Role::Data,
            &format!(
                "cd {dir} || exit 1; running=$({compose} ps --status running -q {SERVICE}); \
                 sparse=; if tar --version 2>/dev/null | grep -q GNU; then sparse=--sparse; fi; \
                 if [ -n \"$running\" ]; then {compose} stop {SERVICE} >/dev/null 2>&1; fi; \
                 tar $sparse -cf - {}; status=$?; \
                 if [ -n \"$running\" ]; then {compose} start {SERVICE} >/dev/null 2>&1; fi; \
                 exit $status",
                BACKUP_DIRS.join(" ")
            ),
        )
    }

    /// Unpack next to the folders first, then swap them in.
    fn restore_shared_files(
        &self,
        ctx: &ServerCtx,
        config: &SharedAppConfig,
        from: &ItemReader,
    ) -> Result<()> {
        if !from.has(BACKUP_ITEM) {
            return Ok(());
        }
        let dir = shell_quote(&ctx.path(&shared_dir(KIND, &config.name)));
        let mut script = format!(
            "mkdir -p -- {dir} && cd {dir} && rm -rf .restore && mkdir .restore && tar -xf - -C .restore"
        );
        for name in BACKUP_DIRS {
            script.push_str(&format!(
                " && if [ -d .restore/{name} ]; then rm -rf {name} && mv .restore/{name} {name}; fi"
            ));
        }
        script.push_str(" && rm -rf .restore");
        from.feed(BACKUP_ITEM, b"", &script)
    }
}

const BACKUP_DIRS: [&str; 2] = ["data", "secrets"];
const BACKUP_ITEM: &str = "openobserve.tar";

/// The configured image, or the official one of the version.
fn image(config: &SharedAppConfig) -> Result<String> {
    if let Some(image) = &config.image {
        return Ok(image.clone());
    }
    let version = config.version.unwrap_or(DEFAULT_VERSION);
    match IMAGE_TAGS.iter().find(|(major, _)| *major == version) {
        Some((_, tag)) => Ok(format!("openobserve/openobserve:{tag}")),
        None => bail!(
            "set `image` for openobserve version {version}, e.g. openobserve/openobserve:v{version}.0.0"
        ),
    }
}

fn secrets_path(name: &str) -> String {
    format!("{}/secrets/openobserve.env", shared_dir(KIND, name))
}

/// The value of a `NAME='…'` line written by `render`.
fn read_variable(env_file: &str, name: &str) -> Option<String> {
    env_file.lines().find_map(|line| {
        let value = line.strip_prefix(name)?.strip_prefix('=')?;
        Some(value.trim().trim_matches('\'').to_string())
    })
}

/// Written single-quoted into env files and YAML.
fn is_email(value: &str) -> bool {
    value.split_once('@').is_some_and(|(user, domain)| {
        !user.is_empty() && !domain.is_empty() && !domain.contains('@')
    }) && !value.contains(|c: char| c.is_whitespace() || c == '\'')
}

/// OpenObserve refuses to start with a weak root password.
fn check_password(password: &str) -> Result<()> {
    ensure!(
        !password.contains(['\n', '\r', '\'']),
        "root_password must be a single line without `'`"
    );
    ensure!(
        (8..=128).contains(&password.chars().count())
            && password.chars().any(|c| c.is_ascii_lowercase())
            && password.chars().any(|c| c.is_ascii_uppercase())
            && password.chars().any(|c| c.is_ascii_digit())
            && password.chars().any(|c| !c.is_ascii_alphanumeric()),
        "root_password must have 8 to 128 characters with a lowercase and an uppercase letter, a digit and a special character, as OpenObserve requires"
    );
    Ok(())
}

/// A random password with the character classes OpenObserve requires.
fn strong_password() -> String {
    format!("Compost-{}", generate_password())
}

#[cfg(test)]
mod tests {
    use super::*;

    const O2: &str = "{name: o2, type: openobserve, version: 1, configuration: {url: 'https://logs.example.com', container_metrics: false}}";

    fn config(text: &str) -> SharedAppConfig {
        serde_yaml_ng::from_str(text).unwrap()
    }

    fn validate(text: &str) -> Result<()> {
        OpenObserve.validate_config(&config(text))
    }

    #[test]
    fn accepts_defaults_and_full_configuration() {
        validate("{name: o2, type: openobserve}").unwrap();
        validate(O2).unwrap();
        validate(
            "{name: o2, type: openobserve, configuration: {root_email: me@example.com, root_password: 'Secret-pass1', forward_port: 24225, retention_days: 30, collector_image: x}}",
        )
        .unwrap();
        check_password(&strong_password()).unwrap();
    }

    #[test]
    fn checks_own_configuration() {
        for (configuration, expected) in [
            ("{url: logs.example.com}", "must start with"),
            ("{root_email: admin}", "not an email address"),
            ("{root_password: short}", "8 to 128 characters"),
            ("{root_password: alllowercase1-}", "uppercase"),
            ("{root_password: \"It's-a-pass1\"}", "without `'`"),
            ("{forward_port: 0}", "forward_port"),
            ("{retention_days: 0}", "retention_days"),
            ("{realm: x}", "unknown field `realm`"),
        ] {
            let text = format!("{{name: o2, type: openobserve, configuration: {configuration}}}");
            let err = format!("{:#}", validate(&text).unwrap_err());
            assert!(err.contains(expected), "{configuration}: {err}");
        }
        let err = validate("{name: o2, type: openobserve, version: 2}").unwrap_err();
        assert!(err.to_string().contains("set `image`"), "{err}");
    }

    #[test]
    fn links_to_the_ingress_only_with_url() {
        let links = OpenObserve.shared_links(&config(O2)).unwrap().unwrap();
        let ingress = &links.services["ingress"][0];
        assert_eq!(ingress["domains"][0], "logs.example.com");
        assert_eq!(ingress["routes"][0]["target"], "openobserve");
        assert_eq!(ingress["routes"][0]["port"], 5080);
        assert!(
            OpenObserve
                .shared_links(&config("{name: o2, type: openobserve}"))
                .unwrap()
                .is_none()
        );
    }

    mod render {
        use super::*;
        use crate::connector::Connector;
        use crate::connector::local::Local;
        use tempfile::TempDir;

        const LOCAL: Local = Local { verbose: false };

        fn render(home: &TempDir, text: &str) -> Result<Rendered> {
            let ctx = ServerCtx {
                connector: &LOCAL,
                home: home.path().to_str().unwrap().to_string(),
            };
            OpenObserve.render(&ctx, &config(text))
        }

        fn file<'a>(rendered: &'a Rendered, suffix: &str) -> &'a str {
            &rendered
                .files
                .iter()
                .find(|f| f.path.ends_with(suffix))
                .unwrap()
                .contents
        }

        #[test]
        fn renders_compose_collector_and_secrets() {
            let home = TempDir::new().unwrap();
            let rendered = render(&home, O2).unwrap();
            let compose = file(&rendered, "compose.yaml");
            assert!(
                compose.contains("image: openobserve/openobserve:v1.1.0\n"),
                "{compose}"
            );
            assert!(compose.contains(r#""127.0.0.1:24224:24224""#), "{compose}");
            assert!(
                compose.contains("compost-monitoring-o2:\n        aliases: [o2]"),
                "{compose}"
            );
            assert!(!compose.contains("docker.sock"), "{compose}");
            let parsed = crate::config::app::ComposeFile::parse(compose).unwrap();
            assert!(!parsed.services["openobserve"].has_networks);
            assert!(parsed.services["collector"].has_networks);

            let collector = file(&rendered, "collector/config.yaml");
            let collector_yaml: Value = serde_yaml_ng::from_str(collector).unwrap();
            assert!(collector_yaml["receivers"].get("docker_stats").is_none());
            assert_eq!(
                collector_yaml["service"]["pipelines"]["metrics"]["receivers"],
                serde_yaml_ng::from_str::<Value>("[otlp, prometheus]").unwrap()
            );

            let paths: Vec<_> = rendered
                .files
                .iter()
                .map(|f| (f.path.as_str(), f.is_secret()))
                .collect();
            assert_eq!(
                paths,
                [
                    ("shared/monitoring/o2/compose.yaml", false),
                    ("shared/monitoring/o2/collector/config.yaml", false),
                    ("shared/monitoring/o2/secrets/openobserve.env", true),
                    ("shared/monitoring/o2/secrets/collector.env", true),
                    ("connectors/monitoring/o2/root-connector.yaml", true),
                ]
            );
            let env = file(&rendered, "openobserve.env");
            assert!(
                env.starts_with("ZO_ROOT_USER_EMAIL='admin@compost.local'\n"),
                "{env}"
            );
            let password = read_variable(env, PASSWORD_VARIABLE).unwrap();
            check_password(&password).unwrap();
            assert!(file(&rendered, "collector.env").contains(&password));
        }

        #[test]
        fn container_metrics_mount_the_docker_socket() {
            if !std::path::Path::new("/var/run/docker.sock").exists() {
                return;
            }
            let home = TempDir::new().unwrap();
            let rendered = render(&home, "{name: o2, type: openobserve}").unwrap();
            let compose = file(&rendered, "compose.yaml");
            assert!(
                compose.contains("/var/run/docker.sock:/var/run/docker.sock:ro"),
                "{compose}"
            );
            assert!(compose.contains("group_add:"), "{compose}");
            assert!(file(&rendered, "collector/config.yaml").contains("docker_stats:"));
        }

        #[test]
        fn credentials_are_kept_and_cannot_change() {
            let home = TempDir::new().unwrap();
            let first = render(&home, O2).unwrap();
            for f in &first.files {
                let path = home.path().join(&f.path);
                LOCAL
                    .write_file(path.to_str().unwrap(), f.contents.as_bytes(), f.mode)
                    .unwrap();
            }
            let second = render(&home, O2).unwrap();
            assert_eq!(
                file(&first, "openobserve.env"),
                file(&second, "openobserve.env")
            );
            for (configuration, expected) in [
                (
                    "{root_password: Other-pass1, container_metrics: false}",
                    "root_password differs",
                ),
                (
                    "{root_email: x@y.z, container_metrics: false}",
                    "root_email differs",
                ),
            ] {
                let text =
                    format!("{{name: o2, type: openobserve, configuration: {configuration}}}");
                let err = render(&home, &text).unwrap_err().to_string();
                assert!(err.contains(expected), "{err}");
            }
        }
    }
}
