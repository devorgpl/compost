//! Generic database behavior, used by every database type unless it overrides it.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail, ensure};
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};

use super::KIND;
use crate::config::app::AppConfig;
use crate::connector::shell_quote;
use crate::shared::{
    AppCtx, Contribution, Resolution, SharedAppConfig, connectors_dir, generate_password,
};

/// Values a link can map to app variables (spec §6.1).
const LINK_VALUES: [&str; 6] = ["db_host", "db_port", "db_name", "db_user", "db_password", "db_url"];

/// Docker network of one database; apps linked to it join this network.
pub fn network_name(database: &str) -> String {
    format!("depap-db-{database}")
}

/// An entry under `services.database` in depap.yaml (spec §6.1).
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

pub struct Credentials {
    pub name: String,
    pub user: String,
    pub password: String,
}

pub fn validate_config(config: &SharedAppConfig) -> Result<()> {
    // Apps pick a database by version, so every database must declare one.
    ensure!(config.version.is_some(), "version is required for databases");
    if let Some(image) = &config.image {
        ensure!(!image.trim().is_empty(), "image must not be empty");
    }
    Ok(())
}

pub fn validate_link(link: &DatabaseLink, app: &AppConfig) -> Result<()> {
    VersionReq::parse(&link.version)
        .with_context(|| format!("version `{}` is not a valid requirement", link.version))?;
    ensure!(!link.targets.is_empty(), "targets: at least one app service is required");
    for target in &link.targets {
        ensure!(app.app_service(target).is_some(), "target `{target}` is not in app_services");
    }
    for (variable, value) in &link.environment {
        ensure!(is_variable_name(variable), "`{variable}` is not a valid variable name");
        ensure!(
            LINK_VALUES.contains(&value.as_str()),
            "environment.{variable}: unknown value `{value}` (known: {})",
            LINK_VALUES.join(", ")
        );
    }
    Ok(())
}

fn is_variable_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Database name and user: from the link, or `<app>_<prefix>_<entry>`.
pub fn names(app: &AppConfig, link: &DatabaseLink) -> (String, String) {
    let default = format!("{}_{}_{}", app.name, app.prefix, link.name).replace('-', "_");
    (
        link.db_name.clone().unwrap_or_else(|| default.clone()),
        link.db_user.clone().unwrap_or(default),
    )
}

/// A database of the requested type whose major version matches.
pub fn resolve<'s>(link: &DatabaseLink, available: &'s [SharedAppConfig]) -> Result<Resolution<'s>> {
    let requirement = VersionReq::parse(&link.version)?;
    let matching: Vec<&SharedAppConfig> = available
        .iter()
        .filter(|db| db.type_name == link.type_name)
        .filter(|db| db.version.is_some_and(|v| requirement.matches(&Version::new(v.into(), 0, 0))))
        .filter(|db| link.instance.as_ref().is_none_or(|pin| *pin == db.name))
        .collect();
    match matching.as_slice() {
        [database] => Ok(Resolution::Use(database)),
        [] => {
            let pin = link.instance.as_ref().map(|p| format!(" named `{p}`")).unwrap_or_default();
            bail!(
                "no running {} database{pin} with version `{}` on the server",
                link.type_name,
                link.version
            )
        }
        several => {
            let names: Vec<&str> = several.iter().map(|db| db.name.as_str()).collect();
            bail!("several databases match ({}); choose one with `instance`", names.join(", "))
        }
    }
}

/// Credentials from the link, else from the existing link record, else defaults
/// with a generated password. Reusing the record keeps passwords stable.
pub fn credentials(ctx: &AppCtx, link: &DatabaseLink) -> Result<Credentials> {
    let (name, user) = names(ctx.app, link);
    let password = match &link.db_password {
        Some(password) => password.clone(),
        None => match read_record(ctx, &link.name)? {
            Some(record) => record.db_password,
            None => generate_password(),
        },
    };
    Ok(Credentials { name, user, password })
}

fn record_path(ctx: &AppCtx, entry: &str) -> String {
    let dir = connectors_dir(KIND, &ctx.shared.name);
    ctx.server.path(&format!("{dir}/{}.yaml", ctx.link_id(entry)))
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
pub fn prepare_application(
    ctx: &AppCtx,
    link: &DatabaseLink,
    endpoint: &Endpoint,
    out: &mut Contribution,
) -> Result<()> {
    let credentials = credentials(ctx, link)?;
    let host = ctx.shared.name.as_str();
    let url = format!(
        "{}://{}:{}@{host}:{}/{}",
        endpoint.url_scheme,
        percent_encode(&credentials.user),
        percent_encode(&credentials.password),
        endpoint.port,
        percent_encode(&credentials.name)
    );
    let values: BTreeMap<&str, String> = BTreeMap::from([
        ("db_host", host.to_string()),
        ("db_port", endpoint.port.to_string()),
        ("db_name", credentials.name.clone()),
        ("db_user", credentials.user.clone()),
        ("db_password", credentials.password.clone()),
        ("db_url", url),
    ]);

    let network = network_name(&ctx.shared.name);
    for target in &link.targets {
        let service = ctx.compose_service(target)?;
        out.join_network(service, &network, None);
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
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
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
            ("{name: d, type: postgres, targets: [x]}", "not in app_services"),
            ("{name: d, type: postgres, targets: [backend], environment: {X: db_secret}}", "unknown value"),
            ("{name: d, type: postgres, targets: [backend], version: 'abc'}", "not a valid requirement"),
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
        assert!(pick("{name: d, type: postgres, version: '>=16'}").contains("several databases match (pg16, pg17)"));
        assert_eq!(pick("{name: d, type: postgres, version: '>=16', instance: pg17}"), "pg17");
        assert!(pick("{name: d, type: postgres, version: '18'}").contains("no running postgres database"));
    }

    #[test]
    fn percent_encodes_url_parts() {
        assert_eq!(percent_encode("a b@c:d/é"), "a%20b%40c%3Ad%2F%C3%A9");
    }
}
