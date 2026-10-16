//! Generic OIDC behavior, used by every OIDC type unless it overrides it.

use std::collections::{BTreeMap, HashSet};

use anyhow::{Context, Result, bail, ensure};
use semver::VersionReq;
use serde::{Deserialize, Serialize};

use super::KIND;
use crate::config::app::AppConfig;
use crate::connector::shell_quote;
use crate::shared::{
    AppCtx, Contribution, Exposed, ExposedRoute, ItemWriter, Resolution, Role, SharedAppConfig,
    connectors_dir, generate_password, is_variable_name, resolve_versioned,
};
pub use crate::shared::{PublicUrl, parse_url};

/// Values a link can map to app variables.
const LINK_VALUES: [&str; 5] = [
    "oidc_issuer",
    "oidc_internal_url",
    "oidc_realm",
    "oidc_client_id",
    "oidc_client_secret",
];

/// Docker network of one provider; apps linked to it join this network.
pub fn network_name(provider: &str) -> String {
    format!("compost-oidc-{provider}")
}

/// An entry under `services.oidc` in compost.yaml.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OidcLink {
    pub name: String,
    #[serde(rename = "type")]
    pub type_name: String,
    /// Semver requirement on the provider's major version, e.g. `>=26`.
    #[serde(default = "any_version")]
    pub version: String,
    /// Use this shared app, instead of picking one by type and version.
    #[serde(default)]
    pub instance: Option<String>,
    /// Default `<app>-<prefix>`.
    #[serde(default)]
    pub realm: Option<String>,
    /// Default `<app>-<prefix>-<entry>`.
    #[serde(default)]
    pub client_id: Option<String>,
    /// The realm's own public URL, e.g. `https://login.example.com`, served by
    /// an ingress route with `link: oidc/<entry>`. Default: the provider's URL.
    #[serde(default)]
    pub public_url: Option<String>,
    /// A client without a secret, e.g. a single-page app.
    #[serde(default)]
    pub public_client: bool,
    #[serde(default)]
    pub redirect_uris: Vec<String>,
    #[serde(default)]
    pub web_origins: Vec<String>,
    /// Default: generated on the first deploy, then reused.
    #[serde(default)]
    pub client_secret: Option<String>,
    /// Users to create in the realm, e.g. test users of a staging instance.
    #[serde(default)]
    pub users: Vec<OidcUser>,
    /// `app_services` that receive the variables and join the provider network.
    #[serde(default)]
    pub targets: Vec<String>,
    /// App variable name → link value (one of [`LINK_VALUES`]).
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OidcUser {
    pub username: String,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub first_name: Option<String>,
    #[serde(default)]
    pub last_name: Option<String>,
    /// Set on every deploy when given.
    #[serde(default)]
    pub password: Option<String>,
}

fn any_version() -> String {
    "*".to_string()
}

/// `connectors/oidc/<provider>/<instance>-<entry>.yaml`. Secret.
#[derive(Debug, Serialize, Deserialize)]
pub struct OidcRecord {
    pub app: String,
    pub prefix: String,
    pub entry: String,
    pub realm: String,
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    /// Users the link created.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub users: Vec<String>,
    /// The realm's own public URL, if the link sets one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_url: Option<String>,
}

/// Where a provider is reached: by browsers, and by apps on its network.
pub struct Endpoint {
    /// e.g. `https://auth.example.com`
    pub public_url: String,
    /// e.g. `http://keycloak:8080`
    pub internal_url: String,
}

pub fn validate_config(config: &SharedAppConfig) -> Result<()> {
    if let Some(image) = &config.image {
        ensure!(!image.trim().is_empty(), "image must not be empty");
    }
    Ok(())
}

pub fn validate_link(link: &OidcLink, app: &AppConfig) -> Result<()> {
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
    let (realm, client_id) = names(app, link);
    for (key, value) in [("realm", &realm), ("client_id", &client_id)] {
        ensure!(
            is_identifier(value),
            "{key} `{value}` must match ^[a-z0-9][a-z0-9_-]{{0,63}}$"
        );
    }
    if let Some(url) = &link.public_url {
        parse_url(url).context("public_url")?;
    }
    ensure!(
        !link.redirect_uris.is_empty(),
        "redirect_uris: at least one is required (set them in the deployment overlay)"
    );
    for uri in link.redirect_uris.iter().chain(&link.web_origins) {
        ensure!(
            !uri.is_empty() && !uri.contains(char::is_whitespace),
            "`{uri}` is not a valid URI"
        );
    }
    ensure!(
        !(link.public_client && link.client_secret.is_some()),
        "client_secret is not used with `public_client: true`"
    );
    if let Some(secret) = &link.client_secret {
        ensure!(!secret.is_empty(), "client_secret must not be empty");
    }
    let mut usernames = HashSet::new();
    for user in &link.users {
        ensure!(
            is_username(&user.username),
            "users: `{}` must match ^[a-z0-9][a-z0-9._@-]{{0,254}}$",
            user.username
        );
        ensure!(
            usernames.insert(&user.username),
            "users: `{}` is listed twice",
            user.username
        );
        if let Some(password) = &user.password {
            ensure!(
                !password.is_empty(),
                "users: password of `{}` must not be empty",
                user.username
            );
        }
    }
    Ok(())
}

/// `^[a-z0-9][a-z0-9_-]{0,63}$`: safe in URLs and in single-quoted shell words.
pub fn is_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    value.len() <= 64
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_-".contains(c))
}

/// `^[a-z0-9][a-z0-9._@-]{0,254}$`; providers keep usernames in lower case.
pub fn is_username(value: &str) -> bool {
    let mut chars = value.chars();
    value.len() <= 255
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._@-".contains(c))
}

/// Realm and client ID: from the link, or `<app>-<prefix>` and `<app>-<prefix>-<entry>`.
pub fn names(app: &AppConfig, link: &OidcLink) -> (String, String) {
    let instance = app.instance();
    (
        link.realm.clone().unwrap_or_else(|| instance.clone()),
        link.client_id
            .clone()
            .unwrap_or_else(|| format!("{instance}-{}", link.name)),
    )
}

/// A provider of the requested type whose major version matches.
pub fn resolve<'s>(link: &OidcLink, available: &'s [SharedAppConfig]) -> Result<Resolution<'s>> {
    resolve_versioned(
        "oidc provider",
        &link.type_name,
        &link.version,
        link.instance.as_deref(),
        available,
    )
    .map(Resolution::Use)
}

/// The link's record: realm, client and its secret. The secret comes from the
/// link, else from the existing record, else it is generated.
pub fn record(ctx: &AppCtx, link: &OidcLink) -> Result<OidcRecord> {
    let (realm, client_id) = names(ctx.app, link);
    let client_secret = if link.public_client {
        None
    } else {
        Some(match &link.client_secret {
            Some(secret) => secret.clone(),
            None => match read_record(ctx, &link.name)?.and_then(|r| r.client_secret) {
                Some(secret) => secret,
                None => generate_password(),
            },
        })
    };
    Ok(OidcRecord {
        app: ctx.app.name.clone(),
        prefix: ctx.app.prefix.clone(),
        entry: link.name.clone(),
        realm,
        client_id,
        client_secret,
        users: link.users.iter().map(|u| u.username.clone()).collect(),
        public_url: match &link.public_url {
            Some(url) => Some(parse_url(url)?.url),
            None => None,
        },
    })
}

fn records_dir(ctx: &AppCtx) -> String {
    ctx.server.path(&connectors_dir(KIND, &ctx.shared.name))
}

fn record_path(ctx: &AppCtx, entry: &str) -> String {
    format!("{}/{}.yaml", records_dir(ctx), ctx.link_id(entry))
}

pub fn read_record(ctx: &AppCtx, entry: &str) -> Result<Option<OidcRecord>> {
    let path = record_path(ctx, entry);
    match ctx.server.connector.read_file(&path)? {
        None => Ok(None),
        Some(text) => Ok(Some(
            serde_yaml_ng::from_str(&text).with_context(|| format!("invalid {path}"))?,
        )),
    }
}

/// The link records of this provider, except the one of `entry`.
fn other_records(ctx: &AppCtx, entry: &str) -> Result<Vec<OidcRecord>> {
    let dir = records_dir(ctx);
    let own = format!("{}.yaml", ctx.link_id(entry));
    let mut records = Vec::new();
    for file in ctx.server.connector.list_dir(&dir)? {
        if file == own || file.starts_with("root-") || !file.ends_with(".yaml") {
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

/// Another link record of this provider uses `realm`.
pub fn realm_used_elsewhere(ctx: &AppCtx, entry: &str, realm: &str) -> Result<bool> {
    Ok(other_records(ctx, entry)?
        .iter()
        .any(|other| other.realm == realm))
}

/// A realm has one public URL: links sharing a realm must agree on it.
fn check_public_url(ctx: &AppCtx, record: &OidcRecord) -> Result<()> {
    for other in other_records(ctx, &record.entry)? {
        if ctx.is_dropped(KIND, &other.app, &other.prefix, &other.entry) {
            continue;
        }
        if other.realm == record.realm && other.public_url != record.public_url {
            let show = |url: &Option<String>| url.clone().unwrap_or_else(|| "none".to_string());
            bail!(
                "realm `{}` has public_url {} in {}/{} ({}), here {}",
                record.realm,
                show(&other.public_url),
                other.app,
                other.prefix,
                other.entry,
                show(&record.public_url)
            );
        }
    }
    Ok(())
}

/// e.g. `realm myapp-staging, client myapp-staging-login` (never the secret).
pub fn describe_link(ctx: &AppCtx, entry: &str) -> Result<String> {
    Ok(match read_record(ctx, entry)? {
        Some(record) => format!("realm {}, client {}", record.realm, record.client_id),
        None => "no link record".to_string(),
    })
}

/// Join the provider network, set the mapped variables, write the link record.
pub fn prepare_application(
    ctx: &AppCtx,
    link: &OidcLink,
    endpoint: &Endpoint,
    out: &mut Contribution,
) -> Result<()> {
    let record = record(ctx, link)?;
    check_public_url(ctx, &record)?;
    let public_url = record.public_url.as_deref().unwrap_or(&endpoint.public_url);
    let values: BTreeMap<&str, String> = BTreeMap::from([
        (
            "oidc_issuer",
            format!("{public_url}/realms/{}", record.realm),
        ),
        (
            "oidc_internal_url",
            format!("{}/realms/{}", endpoint.internal_url, record.realm),
        ),
        ("oidc_realm", record.realm.clone()),
        ("oidc_client_id", record.client_id.clone()),
        (
            "oidc_client_secret",
            record.client_secret.clone().unwrap_or_default(),
        ),
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
        .write_file(&record_path(ctx, &link.name), text.as_bytes(), 0o600)
}

/// The realm on its own domain: its paths, and `/resources` for the shared
/// static files of the login pages. `upstream` is the provider on the ingress
/// network.
pub fn exposed_routes(ctx: &AppCtx, link: &OidcLink, upstream: &str) -> Result<Exposed> {
    let Some(url) = &link.public_url else {
        bail!(
            "services.oidc[{}] needs `public_url` to be served on its own domain",
            link.name
        );
    };
    let (realm, _) = names(ctx.app, link);
    Ok(Exposed {
        routes: vec![
            ExposedRoute {
                name: "realm",
                path: format!("/realms/{realm}"),
                upstream: upstream.to_string(),
            },
            ExposedRoute {
                name: "resources",
                path: "/resources".to_string(),
                upstream: upstream.to_string(),
            },
        ],
        domain: Some(parse_url(url)?.domain),
    })
}

/// `<entry>.yaml`: the link record. It holds the client secret.
pub fn backup_link(ctx: &AppCtx, entry: &str, out: &mut ItemWriter) -> Result<()> {
    let record = ctx
        .server
        .connector
        .read_file(&record_path(ctx, entry))?
        .with_context(|| format!("no link record for `{entry}`; deploy the app again"))?;
    out.file(&format!("{entry}.yaml"), Role::Meta, record.as_bytes())
}

/// Delete the link record. Realm, client and users stay unless purged.
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
            "{name: my-app, prefix: staging, app_services: [{name: backend, service: backend-webapp}]}",
        )
        .unwrap()
    }

    fn link(text: &str) -> OidcLink {
        serde_yaml_ng::from_str(text).unwrap()
    }

    const BASE: &str = "name: login, type: keycloak, targets: [backend], redirect_uris: ['https://a.example.com/*']";

    #[test]
    fn default_names_include_app_and_prefix() {
        let (realm, client) = names(&app(), &link(&format!("{{{BASE}}}")));
        assert_eq!(realm, "my-app-staging");
        assert_eq!(client, "my-app-staging-login");
    }

    #[test]
    fn validates_link() {
        validate_link(
            &link(&format!(
                "{{{BASE}, environment: {{ISSUER: oidc_issuer}}, users: [{{username: alice@x.com, password: p}}]}}"
            )),
            &app(),
        )
        .unwrap();
        // The base entry with `extra`, which may replace targets or redirect_uris.
        let text = |extra: &str| {
            let mut text = format!("{{name: login, type: keycloak{extra}");
            if !extra.contains("targets:") {
                text.push_str(", targets: [backend]");
            }
            if !extra.contains("redirect_uris:") {
                text.push_str(", redirect_uris: ['https://a.example.com/*']");
            }
            text + "}"
        };
        for (extra, expected) in [
            (", targets: [x]", "not in app_services"),
            (", web_origins: ['a b']", "not a valid URI"),
            (", environment: {X: db_url}", "unknown value"),
            (", realm: My_Realm", "realm `My_Realm`"),
            (", redirect_uris: []", "redirect_uris"),
            (", public_client: true, client_secret: s", "public_client"),
            (", users: [{username: a}, {username: a}]", "listed twice"),
            (", users: [{username: 'a b'}]", "users: `a b`"),
            (", version: abc", "not a valid requirement"),
            (", public_url: login.example.com", "public_url"),
        ] {
            let text = text(extra);
            let err = validate_link(&link(&text), &app()).unwrap_err().to_string();
            assert!(err.contains(expected), "{text}: {err}");
        }
    }

    #[test]
    fn resolves_by_type_version_and_pin() {
        let available: Vec<SharedAppConfig> = serde_yaml_ng::from_str(
            "[{name: kc25, type: keycloak, version: 25}, {name: kc26, type: keycloak, version: 26}]",
        )
        .unwrap();
        let pick = |extra: &str| match resolve(&link(&format!("{{{BASE}{extra}}}")), &available) {
            Ok(Resolution::Use(found)) => found.name.clone(),
            Ok(Resolution::Skip(_)) => "skipped".into(),
            Err(err) => err.to_string(),
        };
        assert_eq!(pick(", version: '>=26'"), "kc26");
        assert_eq!(pick(", instance: kc25"), "kc25");
        assert!(pick("").contains("several oidc providers match (kc25, kc26)"));
        assert!(pick(", version: '27'").contains("no running keycloak oidc provider"));
    }
}
