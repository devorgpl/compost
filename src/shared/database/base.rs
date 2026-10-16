//! Generic database behavior, used by every database type unless it overrides it.

use std::collections::BTreeMap;

use anyhow::{Context, Result, ensure};
use semver::VersionReq;
use serde::{Deserialize, Serialize};

use super::KIND;
use crate::config::app::AppConfig;
use crate::connector::shell_quote;
use crate::shared::{
    AppCtx, Contribution, Resolution, SharedAppConfig, connectors_dir, generate_password,
    is_network_name, is_variable_name, resolve_versioned,
};

/// Values a link can map to app variables (spec §6.1).
const LINK_VALUES: [&str; 6] = [
    "db_host",
    "db_port",
    "db_name",
    "db_user",
    "db_password",
    "db_url",
];

/// Docker network of one database; apps linked to it join this network.
pub fn network_name(database: &str) -> String {
    format!("compost-db-{database}")
}

/// An entry under `services.database` in compost.yaml (spec §6.1).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseLink {
    pub name: String,
    #[serde(rename = "type")]
    pub type_name: String,
    /// Semver requirement on the database's major version, e.g. `>=16`.
    #[serde(default = "any_version")]
    pub version: String,
    /// Use this shared app, instead of picking one by type and version.
    #[serde(default)]
    pub instance: Option<String>,
    /// `app_services` that receive the variables and join the database network.
    #[serde(default)]
    pub targets: Vec<String>,
    /// App variable name → link value (one of [`LINK_VALUES`]).
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    #[serde(default)]
    pub db_name: Option<String>,
    #[serde(default)]
    pub db_user: Option<String>,
    #[serde(default)]
    pub db_password: Option<String>,
}

fn any_version() -> String {
    "*".to_string()
}

/// `connectors/database/<database>/<instance>-<entry>.yaml` (spec §8.3). Secret.
#[derive(Debug, Serialize, Deserialize)]
pub struct DatabaseRecord {
    pub app: String,
    pub prefix: String,
    pub entry: String,
    pub db_name: String,
    pub db_user: String,
    pub db_password: String,
}

/// How a database type is reached; used to build `db_port` and `db_url`.
pub struct Endpoint {
    pub url_scheme: &'static str,
    pub port: u16,
}

/// `external` of a database compost does not run: an existing container or
/// a hosted service such as AWS RDS.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct External {
    pub host: String,
    /// Default: the type's port.
    #[serde(default)]
    pub port: Option<u16>,
    /// Existing docker network to reach it on; apps linked to it join it.
    /// Without one, containers reach it like any host (e.g. RDS).
    #[serde(default)]
    pub network: Option<String>,
    /// e.g. `require`: added to `db_url` and used by compost's own clients.
    #[serde(default)]
    pub sslmode: Option<String>,
    /// `--purge database` may drop databases on it (needs root credentials).
    #[serde(default)]
    pub allow_purge: bool,
}

const SSL_MODES: [&str; 6] = [
    "disable",
    "allow",
    "prefer",
    "require",
    "verify-ca",
    "verify-full",
];

/// The `external` block of a database, if it is external.
pub fn external(config: &SharedAppConfig) -> Result<Option<External>> {
    match &config.external {
        None => Ok(None),
        Some(value) => serde_yaml_ng::from_value(value.clone())
            .map(Some)
            .map_err(|e| anyhow::anyhow!("external: {e}")),
    }
}

/// Where linked app services connect.
pub struct Target {
    pub host: String,
    pub port: u16,
    /// Docker network the app services join, if any.
    pub network: Option<String>,
    pub sslmode: Option<String>,
}

/// compost's own database is `<name>` on `compost-db-<name>`; an external one
/// is where its `external` block says.
pub fn target(shared: &SharedAppConfig, endpoint: &Endpoint) -> Result<Target> {
    Ok(match external(shared)? {
        None => Target {
            host: shared.name.clone(),
            port: endpoint.port,
            network: Some(network_name(&shared.name)),
            sslmode: None,
        },
        Some(external) => Target {
            host: external.host,
            port: external.port.unwrap_or(endpoint.port),
            network: external.network,
            sslmode: external.sslmode,
        },
    })
}

pub struct Credentials {
    pub name: String,
    pub user: String,
    pub password: String,
}

pub fn validate_config(config: &SharedAppConfig) -> Result<()> {
    // Apps pick a database by version, so every database must declare one.
    ensure!(
        config.version.is_some(),
        "version is required for databases"
    );
    if let Some(image) = &config.image {
        ensure!(!image.trim().is_empty(), "image must not be empty");
    }
    if let Some(external) = external(config)? {
        ensure!(
            config.image.is_none(),
            "image is not used with `external`: compost does not run the database"
        );
        ensure!(
            !external.host.trim().is_empty()
                && !external
                    .host
                    .contains(|c: char| c.is_whitespace() || c == '='),
            "external.host must be a host name or address"
        );
        if let Some(network) = &external.network {
            ensure!(
                is_network_name(network),
                "external.network: `{network}` is not a docker network name"
            );
        }
        if let Some(mode) = &external.sslmode {
            ensure!(
                SSL_MODES.contains(&mode.as_str()),
                "external.sslmode: unknown value `{mode}` (known: {})",
                SSL_MODES.join(", ")
            );
        }
    }
    Ok(())
}

pub fn validate_link(link: &DatabaseLink, app: &AppConfig) -> Result<()> {
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
    Ok(())
}

/// Database name and user: from the link, or `<app>_<prefix>_<entry>`.
pub fn names(app: &AppConfig, link: &DatabaseLink) -> (String, String) {
    let default = format!("{}_{}_{}", app.name, app.prefix, link.name).replace('-', "_");
    (
        link.db_name.clone().unwrap_or_else(|| default.clone()),
        link.db_user.clone().unwrap_or(default),
    )
}

/// A database of the requested type whose major version matches. External
/// databases are only used when the link names them with `instance`, so
/// registering one never changes which database other links get.
pub fn resolve<'s>(
    link: &DatabaseLink,
    available: &'s [SharedAppConfig],
) -> Result<Resolution<'s>> {
    let pin = link.instance.as_deref();
    let candidates: Vec<SharedAppConfig> = available
        .iter()
        .filter(|app| !app.is_external() || pin == Some(app.name.as_str()))
        .cloned()
        .collect();
    let found = resolve_versioned("database", &link.type_name, &link.version, pin, &candidates)?;
    let found = available
        .iter()
        .find(|app| app.name == found.name)
        .context("resolved database is configured")?;
    Ok(Resolution::Use(found))
}

/// Credentials from the link, else from the existing link record, else defaults
/// with a generated password. Reusing the record keeps passwords stable.
/// Without `provisioned` (compost cannot create databases on the server), the
/// database and user must exist: the link names them and gives the password.
pub fn credentials(ctx: &AppCtx, link: &DatabaseLink, provisioned: bool) -> Result<Credentials> {
    if !provisioned {
        ensure!(
            link.db_name.is_some() && link.db_user.is_some(),
            "{} is external and compost has no root credentials for it, so it cannot create databases: give the existing db_name, db_user and db_password (usually in the deployment overlay)",
            ctx.shared.name
        );
    }
    let (name, user) = names(ctx.app, link);
    let password = match &link.db_password {
        Some(password) => password.clone(),
        None => match read_record(ctx, &link.name)? {
            Some(record) => record.db_password,
            None if provisioned => generate_password(),
            None => anyhow::bail!(
                "db_password is required for an existing database on {}",
                ctx.shared.name
            ),
        },
    };
    Ok(Credentials {
        name,
        user,
        password,
    })
}

fn record_path(ctx: &AppCtx, entry: &str) -> String {
    let dir = connectors_dir(KIND, &ctx.shared.name);
    ctx.server
        .path(&format!("{dir}/{}.yaml", ctx.link_id(entry)))
}

pub fn read_record(ctx: &AppCtx, entry: &str) -> Result<Option<DatabaseRecord>> {
    let path = record_path(ctx, entry);
    match ctx.server.connector.read_file(&path)? {
        None => Ok(None),
        Some(text) => Ok(Some(
            serde_yaml_ng::from_str(&text).with_context(|| format!("invalid {path}"))?,
        )),
    }
}

/// e.g. `database mystagingappdb, user myappuser` (never the password).
pub fn describe_link(ctx: &AppCtx, entry: &str) -> Result<String> {
    Ok(match read_record(ctx, entry)? {
        Some(record) => format!("database {}, user {}", record.db_name, record.db_user),
        None => "no link record".to_string(),
    })
}

/// Delete the link record. The database itself stays unless purged.
pub fn remove_application(ctx: &AppCtx, entry: &str) -> Result<()> {
    let path = shell_quote(&record_path(ctx, entry));
    ctx.server.connector.exec(&format!("rm -f -- {path}"))?;
    Ok(())
}

/// Join the database network, set the mapped variables, write the link record.
/// `provisioned`: compost creates the database and user (see [`credentials`]).
pub fn prepare_application(
    ctx: &AppCtx,
    link: &DatabaseLink,
    endpoint: &Endpoint,
    provisioned: bool,
    out: &mut Contribution,
) -> Result<()> {
    let credentials = credentials(ctx, link, provisioned)?;
    let target = target(ctx.shared, endpoint)?;
    let query = target
        .sslmode
        .as_ref()
        .map(|mode| format!("?sslmode={mode}"))
        .unwrap_or_default();
    let url = format!(
        "{}://{}:{}@{}:{}/{}{query}",
        endpoint.url_scheme,
        percent_encode(&credentials.user),
        percent_encode(&credentials.password),
        target.host,
        target.port,
        percent_encode(&credentials.name)
    );
    let values: BTreeMap<&str, String> = BTreeMap::from([
        ("db_host", target.host.clone()),
        ("db_port", target.port.to_string()),
        ("db_name", credentials.name.clone()),
        ("db_user", credentials.user.clone()),
        ("db_password", credentials.password.clone()),
        ("db_url", url),
    ]);

    for app_service in &link.targets {
        let service = ctx.compose_service(app_service)?;
        if let Some(network) = &target.network {
            out.join_network(service, network, None);
        }
        for (variable, value) in &link.environment {
            out.set_env(service, variable, values[value.as_str()].clone())?;
        }
    }

    let record = DatabaseRecord {
        app: ctx.app.name.clone(),
        prefix: ctx.app.prefix.clone(),
        entry: link.name.clone(),
        db_name: credentials.name,
        db_user: credentials.user,
        db_password: credentials.password,
    };
    let text = serde_yaml_ng::to_string(&record)?;
    ctx.server
        .connector
        .write_file(&record_path(ctx, &link.name), text.as_bytes(), 0o600)
}

/// Encode everything except RFC 3986 unreserved characters.
fn percent_encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> AppConfig {
        serde_yaml_ng::from_str(
            "{name: my-app, prefix: staging, app_services: [{name: backend, service: backend-webapp}]}",
        )
        .unwrap()
    }

    fn link(text: &str) -> DatabaseLink {
        serde_yaml_ng::from_str(text).unwrap()
    }

    fn shared(text: &str) -> Vec<SharedAppConfig> {
        serde_yaml_ng::from_str(text).unwrap()
    }

    #[test]
    fn default_names_include_app_and_prefix() {
        let (name, user) = names(&app(), &link("{name: system-data, type: postgres}"));
        assert_eq!(name, "my_app_staging_system_data");
        assert_eq!(user, name);
    }

    #[test]
    fn validates_targets_and_environment() {
        let ok = "{name: d, type: postgres, targets: [backend], environment: {PG_URL: db_url}}";
        validate_link(&link(ok), &app()).unwrap();
        for (text, expected) in [
            ("{name: d, type: postgres}", "at least one"),
            (
                "{name: d, type: postgres, targets: [x]}",
                "not in app_services",
            ),
            (
                "{name: d, type: postgres, targets: [backend], environment: {X: db_secret}}",
                "unknown value",
            ),
            (
                "{name: d, type: postgres, targets: [backend], version: 'abc'}",
                "not a valid requirement",
            ),
        ] {
            let err = validate_link(&link(text), &app()).unwrap_err().to_string();
            assert!(err.contains(expected), "{text}: {err}");
        }
    }

    #[test]
    fn resolves_by_type_version_and_pin() {
        let available = shared(
            "[{name: pg15, type: postgres, version: 15}, {name: pg16, type: postgres, version: 16}, {name: pg17, type: postgres, version: 17}]",
        );
        let pick = |text: &str| match resolve(&link(text), &available) {
            Ok(Resolution::Use(db)) => db.name.clone(),
            Ok(Resolution::Skip(_)) => "skipped".into(),
            Err(err) => err.to_string(),
        };
        assert_eq!(pick("{name: d, type: postgres, version: '16'}"), "pg16");
        assert_eq!(pick("{name: d, type: postgres, version: '>16'}"), "pg17");
        assert!(
            pick("{name: d, type: postgres, version: '>=16'}")
                .contains("several databases match (pg16, pg17)")
        );
        assert_eq!(
            pick("{name: d, type: postgres, version: '>=16', instance: pg17}"),
            "pg17"
        );
        assert!(
            pick("{name: d, type: postgres, version: '18'}")
                .contains("no running postgres database")
        );
    }

    #[test]
    fn external_databases_are_used_only_when_pinned() {
        let available = shared(
            "[{name: pg16, type: postgres, version: 16}, {name: legacy, type: postgres, version: 16, external: {host: db}}]",
        );
        let pick = |text: &str| match resolve(&link(text), &available) {
            Ok(Resolution::Use(db)) => db.name.clone(),
            Ok(Resolution::Skip(_)) => "skipped".into(),
            Err(err) => err.to_string(),
        };
        assert_eq!(pick("{name: d, type: postgres}"), "pg16");
        assert_eq!(
            pick("{name: d, type: postgres, instance: legacy}"),
            "legacy"
        );
    }

    #[test]
    fn validates_external() {
        let validate = |text: &str| {
            let config: SharedAppConfig = serde_yaml_ng::from_str(text).unwrap();
            validate_config(&config).map_err(|e| e.to_string())
        };
        validate("{name: rds, type: postgres, version: 16, external: {host: a.rds.amazonaws.com, sslmode: require}}")
            .unwrap();
        for (external, expected) in [
            ("{host: ''}", "external.host"),
            ("{host: db, network: '-x'}", "not a docker network name"),
            ("{host: db, sslmode: always}", "unknown value `always`"),
            ("{host: db, user: x}", "unknown field `user`"),
        ] {
            let err = validate(&format!(
                "{{name: x, type: postgres, version: 16, external: {external}}}"
            ))
            .unwrap_err();
            assert!(err.contains(expected), "{external}: {err}");
        }
        let err =
            validate("{name: x, type: postgres, version: 16, image: p, external: {host: db}}")
                .unwrap_err();
        assert!(err.contains("image is not used with `external`"), "{err}");
    }

    #[test]
    fn external_target() {
        let endpoint = Endpoint {
            url_scheme: "postgres",
            port: 5432,
        };
        let config: Vec<SharedAppConfig> = shared(
            "[{name: pg16, type: postgres, version: 16}, {name: rds, type: postgres, version: 16, external: {host: a.example.com, port: 6432, sslmode: require}}]",
        );
        let own = target(&config[0], &endpoint).unwrap();
        assert_eq!(
            (own.host.as_str(), own.port, own.network.as_deref()),
            ("pg16", 5432, Some("compost-db-pg16"))
        );
        let rds = target(&config[1], &endpoint).unwrap();
        assert_eq!(
            (
                rds.host.as_str(),
                rds.port,
                rds.network,
                rds.sslmode.as_deref()
            ),
            ("a.example.com", 6432, None, Some("require"))
        );
    }

    #[test]
    fn percent_encodes_url_parts() {
        assert_eq!(percent_encode("a b@c:d/é"), "a%20b%40c%3Ad%2F%C3%A9");
    }
}
