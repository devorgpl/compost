//! Temporal workflow type. Keeps its data in two postgres databases of the
//! server; its web UI is served by the ingress and logs in through an OIDC
//! provider. Each link gets a namespace.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail, ensure};
use minijinja::context;
use serde::Deserialize;
use serde_yaml_ng::{Mapping, Value};

use super::base::{self, Endpoint, WorkflowLink, WorkflowRecord};
use super::{KIND, WorkflowType};
use crate::connector::shell_quote;
use crate::shared::{
    AppCtx, Generated, ItemReader, ItemWriter, MetricsEndpoint, Monitored, Rendered, RenderedFile,
    Role, ServerCtx, SharedAppConfig, SharedLinks, app_service, parse_url, project_name,
    render_template, service_name, shared_dir,
};

pub struct Temporal;

const DEFAULT_VERSION: u32 = 1;
/// Image tags of the server and its admin tools by major version.
const IMAGE_TAGS: &[(u32, &str)] = &[(1, "1.32.1")];
const UI_IMAGE: &str = "temporalio/ui:2.56.0";
/// The database Temporal needs: its schema creates `btree_gin`, which a
/// database owner may create since postgres 13.
const DATABASE_TYPE: &str = "postgres";
const DATABASE_VERSION: &str = ">=13";
/// App services of the shared app, as its own links name them.
const SERVICE: &str = "temporal";
const SCHEMA: &str = "schema";
const UI: &str = "ui";
const PORT: u16 = 7233;
const UI_PORT: u16 = 8080;
const METRICS_PORT: u16 = 9090;
const DEFAULT_HISTORY_SHARDS: u32 = 4;
const SHARDS_VARIABLE: &str = "NUM_HISTORY_SHARDS";
const CLUSTER_ITEM: &str = "cluster.env";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct TemporalConfig {
    /// Fixed by the first deploy: Temporal cannot change it later.
    history_shards: u32,
    /// Default: `temporalio/admin-tools` with the tag of the server image.
    admin_tools_image: Option<String>,
    database: DatabaseChoice,
    /// Settings Temporal reads from its dynamic config file, e.g. limits.
    dynamic_config: Mapping,
    ui: Option<UiConfig>,
}

impl Default for TemporalConfig {
    fn default() -> Self {
        Self {
            history_shards: DEFAULT_HISTORY_SHARDS,
            admin_tools_image: None,
            database: DatabaseChoice::default(),
            dynamic_config: Mapping::new(),
            ui: None,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct DatabaseChoice {
    /// Pin a database when several match; both databases are on it.
    #[serde(default)]
    instance: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UiConfig {
    /// Public URL, e.g. `https://temporal.example.com`.
    url: String,
    #[serde(default)]
    image: Option<String>,
    /// Log in through an OIDC provider of the server.
    #[serde(default)]
    oidc: Option<UiOidc>,
    /// Serve the UI without a login: anyone who reaches it controls every
    /// workflow.
    #[serde(default)]
    anonymous: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UiOidc {
    #[serde(rename = "type", default = "keycloak")]
    type_name: String,
    #[serde(default)]
    instance: Option<String>,
    /// Realm of the users. Default `compost-workflow-<name>`.
    #[serde(default)]
    realm: Option<String>,
}

fn keycloak() -> String {
    "keycloak".to_string()
}

impl WorkflowType for Temporal {
    fn type_name(&self) -> &'static str {
        "temporal"
    }

    fn default_version(&self) -> Option<u32> {
        Some(DEFAULT_VERSION)
    }

    fn monitored(&self, config: &SharedAppConfig) -> Result<Monitored> {
        let temporal: TemporalConfig = config.configuration()?;
        let mut services = vec![app_service(SERVICE, service_name(&config.name))];
        if temporal.ui.is_some() {
            services.push(app_service(UI, ui_service(&config.name)));
        }
        Ok(Monitored {
            services,
            metrics: vec![MetricsEndpoint {
                service: SERVICE,
                port: METRICS_PORT,
                path: "/metrics",
            }],
        })
    }

    fn generate(&self) -> Generated {
        Generated {
            local: include_str!("templates/generate/local.yaml"),
            prod: Some(include_str!("templates/generate/prod.yaml")),
            secrets: None,
            notes: &[
                "temporal: link apps with `services.workflow: [{name: jobs, type: temporal, targets: […], environment: {TEMPORAL_ADDRESS: wf_address, TEMPORAL_NAMESPACE: wf_namespace}}]`; each gets its own namespace.",
                "temporal: UI users log in from keycloak realm `temporal`: create them in Keycloak's admin console. Locally, open http://temporal.localhost:18080/.",
            ],
            requires: &["database/postgres", "ingress/caddy", "oidc/keycloak"],
        }
    }

    /// Extends the generic validation with Temporal's own settings.
    fn validate_config(&self, config: &SharedAppConfig) -> Result<()> {
        base::validate_config(config)?;
        let temporal: TemporalConfig = config.configuration()?;
        images(config, &temporal)?;
        ensure!(
            (1..=65536).contains(&temporal.history_shards),
            "history_shards must be between 1 and 65536"
        );
        if let Some(ui) = &temporal.ui {
            parse_url(&ui.url).context("ui.url")?;
            if let Some(image) = &ui.image {
                ensure!(!image.trim().is_empty(), "ui.image must not be empty");
            }
            match (&ui.oidc, ui.anonymous) {
                (Some(_), true) => bail!("ui: `oidc` and `anonymous: true` exclude each other"),
                (None, false) => bail!(
                    "ui: give `oidc` for a login, or `anonymous: true` to serve it without one \
                     (anyone who reaches it can then change every workflow)"
                ),
                _ => {}
            }
        }
        Ok(())
    }

    fn render(&self, ctx: &ServerCtx, config: &SharedAppConfig) -> Result<Rendered> {
        let temporal: TemporalConfig = config.configuration()?;
        let (image, admin_tools_image) = images(config, &temporal)?;
        let dir = shared_dir(KIND, &config.name);
        let cluster_path = cluster_path(&config.name);

        // The shard count is part of Temporal's data: it cannot change.
        if let Some(existing) = ctx
            .read(&cluster_path)?
            .and_then(|text| read_variable(&text, SHARDS_VARIABLE))
        {
            ensure!(
                existing == temporal.history_shards.to_string(),
                "history_shards is {existing} on the server; Temporal cannot change it"
            );
        }

        let ui = match &temporal.ui {
            Some(ui) => {
                let url = parse_url(&ui.url)?;
                Some(context! {
                    image => ui.image.as_deref().unwrap_or(UI_IMAGE),
                    insecure_cookie => url.url.starts_with("http://"),
                    url => url.url,
                })
            }
            None => None,
        };
        let script = include_str!("templates/schema.sh");
        let compose = render_template(
            include_str!("templates/compose.yaml.j2"),
            context! {
                image,
                admin_tools_image,
                service => service_name(&config.name),
                schema_service => schema_service(&config.name),
                admin_service => admin_service(&config.name),
                ui_service => ui_service(&config.name),
                container => project_name(KIND, &config.name),
                network => base::network_name(&config.name),
                host => config.name.as_str(),
                port => PORT,
                metrics_port => METRICS_PORT,
                script_hash => &sha256_hex(script)[..12],
                oidc => temporal.ui.as_ref().is_some_and(|ui| ui.oidc.is_some()),
                ui,
            },
        )?;
        let dynamic_config = if temporal.dynamic_config.is_empty() {
            String::new()
        } else {
            serde_yaml_ng::to_string(&temporal.dynamic_config)?
        };

        Ok(Rendered {
            files: vec![
                RenderedFile::new(format!("{dir}/compose.yaml"), compose),
                RenderedFile::new(format!("{dir}/schema.sh"), script.to_string()),
                RenderedFile::new(
                    cluster_path,
                    format!("{SHARDS_VARIABLE}={}\n", temporal.history_shards),
                ),
                RenderedFile::new(
                    format!("{dir}/dynamicconfig/docker.yaml"),
                    format!(
                        "# Generated by compost from `dynamic_config`. Temporal reads changes without a restart.\n{dynamic_config}"
                    ),
                ),
            ],
            dirs: vec![format!("{dir}/dynamicconfig")],
            ..Default::default()
        })
    }

    /// Its dynamic config: Temporal reads the file again by itself. The
    /// schema script and the shard count are in the compose file.
    fn reload_shared(&self, _ctx: &ServerCtx, _config: &SharedAppConfig) -> Result<bool> {
        Ok(true)
    }

    fn reloads(&self, _config: &SharedAppConfig) -> bool {
        true
    }

    /// Two postgres databases (>= 13); with a UI, its domain on the ingress
    /// and its login on an OIDC provider.
    fn shared_links(&self, config: &SharedAppConfig) -> Result<Option<SharedLinks>> {
        let temporal: TemporalConfig = config.configuration()?;
        let database = |name: &str, prefix: &str| {
            let mut entry = Mapping::new();
            entry.insert("name".into(), name.into());
            entry.insert("type".into(), DATABASE_TYPE.into());
            entry.insert("version".into(), DATABASE_VERSION.into());
            if let Some(instance) = &temporal.database.instance {
                entry.insert("instance".into(), instance.as_str().into());
            }
            entry.insert(
                "targets".into(),
                vec![Value::from(SERVICE), Value::from(SCHEMA)].into(),
            );
            entry.insert(
                "environment".into(),
                environment(&[
                    (&format!("{prefix}POSTGRES_SEEDS"), "db_host"),
                    (&format!("{prefix}DB_PORT"), "db_port"),
                    (&format!("{prefix}DBNAME"), "db_name"),
                    (&format!("{prefix}POSTGRES_USER"), "db_user"),
                    (&format!("{prefix}POSTGRES_PWD"), "db_password"),
                ]),
            );
            Value::Mapping(entry)
        };

        let mut app_services = vec![
            app_service(SERVICE, service_name(&config.name)),
            app_service(SCHEMA, schema_service(&config.name)),
        ];
        let mut services = BTreeMap::from([(
            "database".to_string(),
            vec![database("db", ""), database("visibility", "VISIBILITY_")],
        )]);

        if let Some(ui) = &temporal.ui {
            let url = parse_url(&ui.url)?;
            app_services.push(app_service(UI, ui_service(&config.name)));
            let ingress: Value = serde_yaml_ng::from_str(&format!(
                "{{name: ui, domains: ['{}'], routes: [{{name: ui, target: {UI}, port: {UI_PORT}}}]}}",
                url.domain
            ))?;
            services.insert("ingress".to_string(), vec![ingress]);
            if let Some(oidc) = &ui.oidc {
                let mut entry = Mapping::new();
                entry.insert("name".into(), UI.into());
                entry.insert("type".into(), oidc.type_name.as_str().into());
                if let Some(instance) = &oidc.instance {
                    entry.insert("instance".into(), instance.as_str().into());
                }
                if let Some(realm) = &oidc.realm {
                    entry.insert("realm".into(), realm.as_str().into());
                }
                entry.insert("targets".into(), vec![Value::from(UI)].into());
                entry.insert(
                    "redirect_uris".into(),
                    vec![Value::from(format!("{}/auth/sso/callback", url.url))].into(),
                );
                entry.insert("web_origins".into(), vec![Value::from(url.url)].into());
                // The UI's server reads the provider on the provider network;
                // tokens name its public URL as their issuer.
                entry.insert(
                    "environment".into(),
                    environment(&[
                        ("TEMPORAL_AUTH_PROVIDER_URL", "oidc_internal_url"),
                        ("TEMPORAL_AUTH_ISSUER_URL", "oidc_issuer"),
                        ("TEMPORAL_AUTH_CLIENT_ID", "oidc_client_id"),
                        ("TEMPORAL_AUTH_CLIENT_SECRET", "oidc_client_secret"),
                    ]),
                );
                services.insert("oidc".to_string(), vec![Value::Mapping(entry)]);
            }
        }

        Ok(Some(SharedLinks {
            app_services,
            services,
        }))
    }

    /// The server by the shared app's name on the workflow network.
    fn endpoint(&self, config: &SharedAppConfig) -> Endpoint {
        Endpoint {
            host: config.name.clone(),
            port: PORT,
        }
    }

    fn before_apply_application(&self, ctx: &AppCtx, link: &WorkflowLink) -> Result<()> {
        let record = base::record(ctx, link);
        let script = namespace_script(&record, false)?;
        run_script(ctx, &script)
            .with_context(|| format!("creating namespace `{}` failed", record.namespace))
    }

    fn purge(&self, ctx: &AppCtx, record: &WorkflowRecord) -> Result<()> {
        let script = namespace_script(record, true)?;
        run_script(ctx, &script)
            .with_context(|| format!("deleting namespace `{}` failed", record.namespace))
    }

    /// The shard count, which the restored databases were written with.
    fn backup_shared(
        &self,
        ctx: &ServerCtx,
        config: &SharedAppConfig,
        out: &mut ItemWriter,
    ) -> Result<()> {
        let cluster = ctx
            .read(&cluster_path(&config.name))?
            .context("cluster.env is missing; deploy the server again")?;
        out.file(CLUSTER_ITEM, Role::Meta, cluster.as_bytes())
    }

    fn restore_shared_files(
        &self,
        ctx: &ServerCtx,
        config: &SharedAppConfig,
        from: &ItemReader,
    ) -> Result<()> {
        if from.has(CLUSTER_ITEM) {
            ctx.connector.write_file(
                &ctx.path(&cluster_path(&config.name)),
                from.read(CLUSTER_ITEM)?.as_bytes(),
                0o644,
            )?;
        }
        Ok(())
    }
}

/// The server image and its admin tools: the configured ones, or the
/// official ones of the version.
fn images(config: &SharedAppConfig, temporal: &TemporalConfig) -> Result<(String, String)> {
    let version = config.version.unwrap_or(DEFAULT_VERSION);
    let tag = IMAGE_TAGS
        .iter()
        .find(|(major, _)| *major == version)
        .map(|(_, tag)| *tag);
    let image = match (&config.image, tag) {
        (Some(image), _) => image.clone(),
        (None, Some(tag)) => format!("temporalio/server:{tag}"),
        (None, None) => bail!(
            "set `image` for temporal version {version}, e.g. temporalio/server:{version}.0.0"
        ),
    };
    if let Some(admin_tools) = &temporal.admin_tools_image {
        ensure!(
            !admin_tools.trim().is_empty(),
            "admin_tools_image must not be empty"
        );
        return Ok((image, admin_tools.clone()));
    }
    // The tag of `image`, e.g. `1.32.1` of `temporalio/server:1.32.1`.
    let tag = match image.rsplit_once(':') {
        Some((_, tag)) if !tag.contains('/') => tag,
        _ => bail!("set configuration.admin_tools_image: `{image}` has no tag to take"),
    };
    Ok((image.clone(), format!("temporalio/admin-tools:{tag}")))
}

fn environment(pairs: &[(&str, &str)]) -> Value {
    Value::Mapping(
        pairs
            .iter()
            .map(|(k, v)| (Value::from(*k), Value::from(*v)))
            .collect(),
    )
}

fn schema_service(name: &str) -> String {
    format!("{}-schema", service_name(name))
}

fn admin_service(name: &str) -> String {
    format!("{}-admin", service_name(name))
}

fn ui_service(name: &str) -> String {
    format!("{}-ui", service_name(name))
}

fn cluster_path(name: &str) -> String {
    format!("{}/cluster.env", shared_dir(KIND, name))
}

/// The value of a `NAME=…` line written by `render`.
fn read_variable(env_file: &str, name: &str) -> Option<String> {
    env_file.lines().find_map(|line| {
        let value = line.strip_prefix(name)?.strip_prefix('=')?;
        Some(value.trim().to_string())
    })
}

fn sha256_hex(text: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Create or update (or with `purge`, delete) the record's namespace. The
/// namespace goes into the script single-quoted; it was checked on deploy,
/// but the record is a file on the server.
fn namespace_script(record: &WorkflowRecord, purge: bool) -> Result<String> {
    ensure!(
        base::is_namespace(&record.namespace),
        "unexpected namespace in the link record"
    );
    render_template(
        include_str!("templates/namespace.sh.j2"),
        context! {
            purge,
            namespace => record.namespace.as_str(),
            retention => format!("{}h", u64::from(record.retention_days) * 24),
            description => format!("compost: {}/{} {}", record.app, record.prefix, record.entry),
        },
    )
}

/// Run a script with sh inside the admin container.
fn run_script(ctx: &AppCtx, script: &str) -> Result<()> {
    let command = format!(
        "cd {} && docker compose -p {} exec -T {} sh -s",
        shell_quote(&ctx.server.path(&shared_dir(KIND, &ctx.shared.name))),
        shell_quote(&project_name(KIND, &ctx.shared.name)),
        shell_quote(&admin_service(&ctx.shared.name))
    );
    let output = ctx
        .server
        .connector
        .run(&command, Some(script.as_bytes()))?;
    if !output.success() {
        bail!("{}", output.stderr.trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEMPORAL: &str = "{name: temporal, type: temporal, version: 1, configuration: {ui: {url: 'https://temporal.example.com', oidc: {realm: staff}}}}";

    fn config(text: &str) -> SharedAppConfig {
        serde_yaml_ng::from_str(text).unwrap()
    }

    fn validate(text: &str) -> Result<()> {
        Temporal.validate_config(&config(text))
    }

    #[test]
    fn accepts_ui_with_login_or_without_ui() {
        validate(TEMPORAL).unwrap();
        validate("{name: t, type: temporal, version: 1}").unwrap();
        validate("{name: t, type: temporal, version: 1, configuration: {ui: {url: 'http://t.localhost:18080', anonymous: true}}}")
            .unwrap();
    }

    #[test]
    fn checks_own_configuration() {
        for (text, expected) in [
            (
                "{name: t, type: temporal, version: 1, configuration: {ui: {url: 'https://t.example.com'}}}",
                "give `oidc` for a login",
            ),
            (
                "{name: t, type: temporal, version: 1, configuration: {ui: {url: 'https://t.example.com', anonymous: true, oidc: {}}}}",
                "exclude each other",
            ),
            (
                "{name: t, type: temporal, version: 1, configuration: {ui: {url: t.example.com, anonymous: true}}}",
                "must start with",
            ),
            (
                "{name: t, type: temporal, version: 1, configuration: {history_shards: 0}}",
                "history_shards",
            ),
            (
                "{name: t, type: temporal, version: 2}",
                "set `image` for temporal version 2",
            ),
            (
                "{name: t, type: temporal, version: 1, image: 'registry:5000/temporal'}",
                "has no tag",
            ),
            (
                "{name: t, type: temporal, version: 1, configuration: {shards: 4}}",
                "unknown field `shards`",
            ),
        ] {
            let err = format!("{:#}", validate(text).unwrap_err());
            assert!(err.contains(expected), "{text}: {err}");
        }
    }

    #[test]
    fn admin_tools_follow_the_server_image() {
        let pick = |text: &str| {
            let config = config(text);
            images(&config, &config.configuration().unwrap()).unwrap()
        };
        assert_eq!(
            pick("{name: t, type: temporal, version: 1}"),
            (
                "temporalio/server:1.32.1".to_string(),
                "temporalio/admin-tools:1.32.1".to_string()
            )
        );
        assert_eq!(
            pick("{name: t, type: temporal, version: 1, image: 'temporalio/server:1.31.4'}").1,
            "temporalio/admin-tools:1.31.4"
        );
        assert_eq!(
            pick("{name: t, type: temporal, version: 1, image: 'r:5000/s:x', configuration: {admin_tools_image: 'r:5000/a:x'}}").1,
            "r:5000/a:x"
        );
    }

    #[test]
    fn links_to_two_databases_the_ingress_and_oidc() {
        let links = Temporal.shared_links(&config(TEMPORAL)).unwrap().unwrap();
        let databases = &links.services["database"];
        assert_eq!(databases[0]["name"], "db");
        assert_eq!(databases[0]["version"], ">=13");
        assert_eq!(databases[0]["environment"]["POSTGRES_PWD"], "db_password");
        assert_eq!(databases[1]["name"], "visibility");
        assert_eq!(databases[1]["environment"]["VISIBILITY_DBNAME"], "db_name");
        assert_eq!(databases[1]["targets"][1], "schema");
        let ingress = &links.services["ingress"][0];
        assert_eq!(ingress["domains"][0], "temporal.example.com");
        assert_eq!(ingress["routes"][0]["target"], "ui");
        let oidc = &links.services["oidc"][0];
        assert_eq!(oidc["type"], "keycloak");
        assert_eq!(oidc["realm"], "staff");
        assert_eq!(
            oidc["redirect_uris"][0],
            "https://temporal.example.com/auth/sso/callback"
        );
        let names: Vec<_> = links.app_services.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["temporal", "schema", "ui"]);

        let bare = Temporal
            .shared_links(&config("{name: t, type: temporal, version: 1}"))
            .unwrap()
            .unwrap();
        assert_eq!(bare.services.keys().collect::<Vec<_>>(), ["database"]);
    }

    #[test]
    fn namespace_script_quotes_and_converts_retention() {
        let mut record = WorkflowRecord {
            app: "a".into(),
            prefix: "main".into(),
            entry: "jobs".into(),
            namespace: "a-main".into(),
            retention_days: 7,
        };
        let script = namespace_script(&record, false).unwrap();
        assert!(script.contains("namespace='a-main'"), "{script}");
        assert!(script.contains("--retention '168h'"), "{script}");
        assert!(!script.contains("delete"), "{script}");
        let purge = namespace_script(&record, true).unwrap();
        assert!(purge.contains("namespace delete"), "{purge}");
        record.namespace = "x'; rm -rf /".into();
        assert!(namespace_script(&record, false).is_err());
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
            Temporal.render(&ctx, &config(text))
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
        fn renders_compose_and_files() {
            let home = TempDir::new().unwrap();
            let rendered = render(&home, TEMPORAL).unwrap();
            let compose = file(&rendered, "compose.yaml");
            assert!(
                compose.contains("image: temporalio/server:1.32.1\n"),
                "{compose}"
            );
            assert!(compose.contains("compost-wf-temporal:\n        aliases: [temporal]"));
            assert!(
                compose.contains("TEMPORAL_AUTH_ENABLED: \"true\""),
                "{compose}"
            );
            assert!(!compose.contains("COOKIE_INSECURE"), "https: {compose}");
            let parsed = crate::config::app::ComposeFile::parse(compose).unwrap();
            let services: Vec<_> = parsed.services.keys().map(String::as_str).collect();
            assert_eq!(
                services,
                [
                    "compost-shared-temporal",
                    "compost-shared-temporal-admin",
                    "compost-shared-temporal-schema",
                    "compost-shared-temporal-ui"
                ]
            );
            let paths: Vec<_> = rendered.files.iter().map(|f| f.path.as_str()).collect();
            assert_eq!(
                paths,
                [
                    "shared/workflow/temporal/compose.yaml",
                    "shared/workflow/temporal/schema.sh",
                    "shared/workflow/temporal/cluster.env",
                    "shared/workflow/temporal/dynamicconfig/docker.yaml",
                ]
            );
            assert_eq!(file(&rendered, "cluster.env"), "NUM_HISTORY_SHARDS=4\n");

            let bare = render(
                &home,
                "{name: temporal, type: temporal, version: 1, configuration: {dynamic_config: {limit.maxIDLength: [{value: 255}]}}}",
            )
            .unwrap();
            let compose = file(&bare, "compose.yaml");
            assert!(!compose.contains("-ui"), "{compose}");
            assert!(
                file(&bare, "docker.yaml").contains("limit.maxIDLength:\n- value: 255\n"),
                "{}",
                file(&bare, "docker.yaml")
            );
        }

        #[test]
        fn history_shards_cannot_change() {
            let home = TempDir::new().unwrap();
            let first = render(&home, TEMPORAL).unwrap();
            for f in &first.files {
                let path = home.path().join(&f.path);
                LOCAL
                    .write_file(path.to_str().unwrap(), f.contents.as_bytes(), f.mode)
                    .unwrap();
            }
            render(&home, TEMPORAL).unwrap();
            let err = render(
                &home,
                "{name: temporal, type: temporal, version: 1, configuration: {history_shards: 512}}",
            )
            .unwrap_err();
            assert!(err.to_string().contains("history_shards is 4"), "{err}");
        }
    }
}
