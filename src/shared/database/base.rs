//! Generic database behavior, used by every database type unless it overrides it.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail, ensure};
use semver::VersionReq;
use serde::{Deserialize, Serialize};

use super::KIND;
use crate::config::app::AppConfig;
use crate::connector::shell_quote;
use crate::shared::{
    AppCtx, Contribution, Resolution, SharedAppConfig, check_link_environment, connectors_dir,
    generate_password, host_address, is_network_name, link_value, resolve_versioned,
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
    /// `db_url` carries an external server's `sslmode` (`?sslmode=…`), as
    /// Postgres URLs do. MySQL drivers each have their own parameter.
    pub sslmode_in_url: bool,
    /// Longest user name the server takes; longer default names are shortened.
    pub user_max: usize,
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
    check_link_environment(&link.environment, &LINK_VALUES)?;
    Ok(())
}

/// Database name and user: from the link, or `<app>_<prefix>_<entry>`. A
/// default user name longer than `user_max` is cut and ends in a short hash
/// of the whole name, so it stays unique.
pub fn names(app: &AppConfig, link: &DatabaseLink, user_max: usize) -> (String, String) {
    let default = format!("{}_{}_{}", app.name, app.prefix, link.name).replace('-', "_");
    let user = match &link.db_user {
        Some(user) => user.clone(),
        None if default.len() > user_max => {
            use sha2::{Digest, Sha256};
            let hash: String = Sha256::digest(default.as_bytes())[..4]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            format!("{}_{hash}", &default[..user_max - hash.len() - 1])
        }
        None => default.clone(),
    };
    (link.db_name.clone().unwrap_or(default), user)
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
///
/// Links may share a user (`db_user`): one user has one password, so a link
/// without `db_password` takes the password of another link of the user, and
/// one with it must agree with the links of other instances.
pub fn credentials(
    ctx: &AppCtx,
    link: &DatabaseLink,
    provisioned: bool,
    endpoint: &Endpoint,
) -> Result<Credentials> {
    if !provisioned {
        ensure!(
            link.db_name.is_some() && link.db_user.is_some(),
            "{} is external and compost has no root credentials for it, so it cannot create databases: give the existing db_name, db_user and db_password (usually in the deployment overlay)",
            ctx.shared.name
        );
    }
    let (name, user) = names(ctx.app, link, endpoint.user_max);
    let sharing: Vec<DatabaseRecord> = other_records(ctx, &link.name)?
        .into_iter()
        .filter(|other| other.db_user == user)
        .collect();
    let password = match &link.db_password {
        Some(password) => {
            // Links of this instance are checked together (see `validate_shared_users`).
            let other_instance = sharing.iter().find(|other| {
                (other.app != ctx.app.name || other.prefix != ctx.app.prefix)
                    && other.db_password != *password
            });
            if let Some(other) = other_instance {
                bail!(
                    "user `{user}` is also the user of link {} of {}-{} on {}, with another password: links that share a user need the same db_password",
                    other.entry,
                    other.app,
                    other.prefix,
                    ctx.shared.name
                );
            }
            password.clone()
        }
        None => match read_record(ctx, &link.name)? {
            Some(record) if record.db_user == user => record.db_password,
            _ => match sharing.first() {
                Some(other) => other.db_password.clone(),
                None if provisioned => generate_password(),
                None => bail!(
                    "db_password is required for an existing database on {}",
                    ctx.shared.name
                ),
            },
        },
    };
    Ok(Credentials {
        name,
        user,
        password,
    })
}

fn records_dir(ctx: &AppCtx) -> String {
    ctx.server.path(&connectors_dir(KIND, &ctx.shared.name))
}

fn record_path(ctx: &AppCtx, entry: &str) -> String {
    format!("{}/{}.yaml", records_dir(ctx), ctx.link_id(entry))
}

/// The link records of this database server, except the one of `entry`.
pub fn other_records(ctx: &AppCtx, entry: &str) -> Result<Vec<DatabaseRecord>> {
    let dir = records_dir(ctx);
    let own = format!("{}.yaml", ctx.link_id(entry));
    let mut records = Vec::new();
    for file in ctx.server.connector.list_dir(&dir)? {
        // `root-connector.yaml` holds the root credentials of some types.
        if file == own || file == "root-connector.yaml" || !file.ends_with(".yaml") {
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

/// What purging a link deletes: its database and its user, unless another
/// link of the server still uses them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Purge {
    pub database: bool,
    pub user: bool,
}

/// What to delete when purging `record`, and say what is kept and why.
pub fn purge_of(ctx: &AppCtx, record: &DatabaseRecord) -> Result<Purge> {
    let others = other_records(ctx, &record.entry)?;
    let user_of = others.iter().find(|other| other.db_user == record.db_user);
    let database_of = others.iter().find(|other| other.db_name == record.db_name);
    for (what, name, other) in [
        ("database", &record.db_name, database_of),
        ("user", &record.db_user, user_of),
    ] {
        if let Some(other) = other {
            out!(
                "  kept {what} `{name}` on {}: link {} of {}-{} uses it too",
                ctx.shared.name,
                other.entry,
                other.app,
                other.prefix
            );
        }
    }
    Ok(Purge {
        database: database_of.is_none(),
        user: user_of.is_none(),
    })
}

/// Links of one app that set the same `db_user` must set the same
/// `db_password`, or none: one user has one password.
pub fn validate_shared_users(app: &AppConfig, links: &[DatabaseLink]) -> Result<()> {
    for (i, link) in links.iter().enumerate() {
        let Some(user) = &link.db_user else { continue };
        for other in &links[..i] {
            if other.db_user.as_ref() == Some(user) && other.db_password != link.db_password {
                bail!(
                    "{} and {} share user `{user}` of app {}, so they need the same db_password (or neither sets it)",
                    other.name,
                    link.name,
                    app.name
                );
            }
        }
    }
    Ok(())
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
    let credentials = credentials(ctx, link, provisioned, endpoint)?;
    let target = match target(ctx.shared, endpoint)? {
        Target {
            network: Some(_), ..
        } if out.is_for_host() && !ctx.shared.is_external() => {
            let (host, port) = host_address(ctx.shared)?;
            Target {
                host,
                port,
                network: None,
                sslmode: None,
            }
        }
        target => target,
    };
    let query = target
        .sslmode
        .as_ref()
        .filter(|_| endpoint.sslmode_in_url)
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
            out.set_env(service, variable, link_value(value, &values)?)?;
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
        let (name, user) = names(&app(), &link("{name: system-data, type: postgres}"), 63);
        assert_eq!(name, "my_app_staging_system_data");
        assert_eq!(user, name);
        // Too long for the server: cut, with a hash of the whole name.
        let long = link("{name: uploaded-attachments, type: mysql}");
        let (name, user) = names(&app(), &long, 32);
        assert_eq!(name, "my_app_staging_uploaded_attachments");
        assert_eq!(user.len(), 32);
        assert!(user.starts_with("my_app_staging_uploaded_"), "{user}");
        assert_eq!(names(&app(), &long, 32).1, user, "stable");
    }

    #[test]
    fn validates_targets_and_environment() {
        let ok = "{name: d, type: postgres, targets: [backend], environment: {PG_URL: db_url, \
                  POSTGRES_URL: '{db_host}:{db_port}', JDBC: 'jdbc:postgresql://{db_host}:{db_port}/{db_name}', \
                  JSON: '{{\"db\": \"{db_name}\"}}'}}";
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
                "{name: d, type: postgres, targets: [backend], environment: {X: '{db_host}:{db_prt}'}}",
                "environment.X: unknown value `db_prt` (known: db_host,",
            ),
            (
                "{name: d, type: postgres, targets: [backend], environment: {X: '{db_host'}}",
                "environment.X: `{` without `}`",
            ),
            (
                "{name: d, type: postgres, targets: [backend], environment: {X: 'a}b'}}",
                "environment.X: `}` without `{`",
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
    fn templates_combine_link_values() {
        let values = BTreeMap::from([
            ("db_host", "db".to_string()),
            ("db_port", "5432".to_string()),
            ("db_name", "shop".to_string()),
        ]);
        let value = |template: &str| link_value(template, &values).unwrap();
        assert_eq!(value("db_port"), "5432");
        assert_eq!(value("{db_host}:{db_port}"), "db:5432");
        assert_eq!(
            value("jdbc:postgresql://{db_host}:{db_port}/{db_name}?sslmode=require"),
            "jdbc:postgresql://db:5432/shop?sslmode=require"
        );
        assert_eq!(value("{{\"db\": \"{db_name}\"}}"), "{\"db\": \"shop\"}");
        assert_eq!(value("{{db_name}}"), "{db_name}");
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
            sslmode_in_url: true,
            user_max: 63,
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
