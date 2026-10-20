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
    check_link_environment, connectors_dir, generate_password, link_value, resolve_versioned,
};
pub use crate::shared::{PublicUrl, parse_url};

/// Values a link can map to app variables.
const LINK_VALUES: [&str; 9] = [
    "oidc_issuer",
    "oidc_internal_url",
    "oidc_base_url",
    "oidc_internal_base_url",
    "oidc_origin",
    "oidc_jwks_uri",
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
    /// Skip the link when the server runs no provider of its type, e.g. on
    /// production, where the app logs in through another provider.
    #[serde(default)]
    pub optional: bool,
    /// Default `<app>-<prefix>`.
    #[serde(default)]
    pub realm: Option<String>,
    /// Default `<app>-<prefix>-<entry>`.
    #[serde(default)]
    pub client_id: Option<String>,
    /// The realm's own public URL, e.g. `https://login.example.com` or
    /// `https://app.example.com/auth`, served by an ingress route with
    /// `link: oidc/<entry>`. Default: the provider's URL.
    #[serde(default)]
    pub public_url: Option<String>,
    /// A client without a secret, e.g. a single-page app.
    #[serde(default)]
    pub public_client: bool,
    #[serde(default)]
    pub redirect_uris: Vec<String>,
    #[serde(default)]
    pub web_origins: Vec<String>,
    /// Default: the redirect URIs (`+` in Keycloak).
    #[serde(default)]
    pub post_logout_redirect_uris: Option<Vec<String>>,
    /// PKCE method the client requires; none when unset.
    #[serde(default)]
    pub pkce: Option<Pkce>,
    /// Logging in with a password sent by the client itself (resource owner password grant).
    #[serde(default)]
    pub direct_access_grants: bool,
    /// The client's default scopes. Unset: the ones it has (on creation, the realm's defaults).
    #[serde(default)]
    pub default_scopes: Option<Vec<String>>,
    /// Realm settings set on every deploy; unset ones keep their value.
    #[serde(default)]
    pub realm_settings: RealmSettings,
    /// Realm roles, created when missing.
    #[serde(default)]
    pub roles: Vec<String>,
    /// Client scopes of the realm, created or updated on every deploy and
    /// added to the client's default scopes.
    #[serde(default)]
    pub client_scopes: Vec<ClientScope>,
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
    /// Default `true` when `email` is given.
    #[serde(default)]
    pub email_verified: Option<bool>,
    /// The user's ID, used only when the user is created.
    #[serde(default)]
    pub id: Option<String>,
    /// Realm roles of the user, from the link's `roles`; added, never removed.
    #[serde(default)]
    pub roles: Vec<String>,
    /// Set on every deploy when given, unless `temporary`.
    #[serde(default)]
    pub password: Option<String>,
    /// The password is set when the user is created, and must be changed at the first login.
    #[serde(default)]
    pub temporary: bool,
    /// Attributes set on every deploy, e.g. `company: ACME`; a list for several values.
    #[serde(default, deserialize_with = "attribute_values")]
    pub attributes: BTreeMap<String, Vec<String>>,
}

/// Attribute values: a scalar, or a list of them, as strings.
fn attribute_values<'de, D>(deserializer: D) -> Result<BTreeMap<String, Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let scalar = |value: serde_yaml_ng::Value| match value {
        serde_yaml_ng::Value::String(s) => Ok(s),
        serde_yaml_ng::Value::Number(n) => Ok(n.to_string()),
        serde_yaml_ng::Value::Bool(b) => Ok(b.to_string()),
        _ => Err(D::Error::custom(
            "attribute values are strings, numbers, booleans or lists of them",
        )),
    };
    let map = BTreeMap::<String, serde_yaml_ng::Value>::deserialize(deserializer)?;
    map.into_iter()
        .map(|(name, value)| {
            let values = match value {
                serde_yaml_ng::Value::Sequence(items) => {
                    items.into_iter().map(scalar).collect::<Result<_, _>>()?
                }
                value => vec![scalar(value)?],
            };
            Ok((name, values))
        })
        .collect()
}

/// A client scope of the realm with its protocol mappers.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientScope {
    pub name: String,
    /// The scope's name is listed in the `scope` claim of tokens.
    #[serde(default = "yes")]
    pub include_in_token_scope: bool,
    #[serde(default)]
    pub mappers: Vec<Mapper>,
}

/// A protocol mapper: puts a value of the user into the tokens.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mapper {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: MapperKind,
    /// The user attribute, for `user_attribute`.
    pub attribute: String,
    /// The claim; dots nest it, e.g. `custom.company`.
    pub claim: String,
    /// All values of the attribute, as a list.
    #[serde(default)]
    pub multivalued: bool,
    #[serde(default = "yes")]
    pub id_token: bool,
    #[serde(default = "yes")]
    pub access_token: bool,
    #[serde(default = "yes")]
    pub userinfo: bool,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MapperKind {
    UserAttribute,
}

fn yes() -> bool {
    true
}

/// Client scopes Keycloak creates in every realm: compost would replace their mappers.
const BUILT_IN_SCOPES: [&str; 14] = [
    "acr",
    "address",
    "basic",
    "email",
    "microprofile-jwt",
    "offline_access",
    "organization",
    "phone",
    "profile",
    "role_list",
    "roles",
    "saml_organization",
    "service_account",
    "web-origins",
];

#[derive(Debug, Clone, Copy, Deserialize)]
pub enum Pkce {
    S256,
    #[serde(rename = "plain")]
    Plain,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RealmSettings {
    pub registration: Option<bool>,
    pub reset_password: Option<bool>,
    pub login_with_email: Option<bool>,
    pub verify_email: Option<bool>,
    pub brute_force_protection: Option<bool>,
    pub ssl_required: Option<SslRequired>,
    /// Durations, e.g. `30m` or `3h`.
    pub sso_session_idle_timeout: Option<String>,
    pub sso_session_max_lifespan: Option<String>,
    pub access_token_lifespan: Option<String>,
}

impl RealmSettings {
    /// The durations that are set, as Keycloak's keys and seconds.
    pub fn durations(&self) -> Result<Vec<(&'static str, u64)>> {
        let mut durations = Vec::new();
        for (key, name, value) in [
            (
                "ssoSessionIdleTimeout",
                "sso_session_idle_timeout",
                &self.sso_session_idle_timeout,
            ),
            (
                "ssoSessionMaxLifespan",
                "sso_session_max_lifespan",
                &self.sso_session_max_lifespan,
            ),
            (
                "accessTokenLifespan",
                "access_token_lifespan",
                &self.access_token_lifespan,
            ),
        ] {
            let Some(value) = value else { continue };
            let seconds = humantime::parse_duration(value)
                .ok()
                .map(|d| d.as_secs())
                .filter(|&s| s > 0);
            let Some(seconds) = seconds else {
                bail!("realm_settings.{name}: `{value}` is not a duration like 30m or 3h");
            };
            durations.push((key, seconds));
        }
        Ok(durations)
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SslRequired {
    None,
    External,
    All,
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
    check_link_environment(&link.environment, &LINK_VALUES)?;
    let (realm, client_id) = names(app, link);
    for (key, value) in [("realm", &realm), ("client_id", &client_id)] {
        ensure!(
            is_identifier(value),
            "{key} `{value}` must match ^[a-z0-9][a-z0-9_-]{{0,63}}$"
        );
    }
    if let Some(url) = &link.public_url {
        split_public_url(url).context("public_url")?;
    }
    // An optional link may be skipped, so it needs them only when deployed.
    if !link.optional {
        check_redirect_uris(link)?;
    }
    let post_logout = link.post_logout_redirect_uris.iter().flatten();
    for uri in link
        .redirect_uris
        .iter()
        .chain(&link.web_origins)
        .chain(post_logout)
    {
        // Keycloak joins post-logout URIs with `##`.
        ensure!(
            !uri.is_empty() && !uri.contains(char::is_whitespace) && !uri.contains("##"),
            "`{uri}` is not a valid URI"
        );
    }
    link.realm_settings.durations()?;
    let mut scopes = HashSet::new();
    for scope in &link.client_scopes {
        ensure!(
            is_role_name(&scope.name),
            "client_scopes: `{}` must match ^[A-Za-z0-9][A-Za-z0-9._:-]{{0,254}}$",
            scope.name
        );
        ensure!(
            !BUILT_IN_SCOPES.contains(&scope.name.as_str()),
            "client_scopes: `{}` is a scope Keycloak creates itself; pick another name",
            scope.name
        );
        ensure!(
            scopes.insert(&scope.name),
            "client_scopes: `{}` is listed twice",
            scope.name
        );
        let mut mappers = HashSet::new();
        for mapper in &scope.mappers {
            let path = format!("client_scopes[{}]: mapper `{}`", scope.name, mapper.name);
            ensure!(
                is_role_name(&mapper.name),
                "{path} must match ^[A-Za-z0-9][A-Za-z0-9._:-]{{0,254}}$"
            );
            ensure!(mappers.insert(&mapper.name), "{path} is listed twice");
            ensure!(
                !mapper.attribute.is_empty() && !mapper.claim.is_empty(),
                "{path}: `attribute` and `claim` must not be empty"
            );
        }
    }
    for (key, names) in [
        ("roles", &link.roles),
        (
            "default_scopes",
            link.default_scopes.as_ref().unwrap_or(&Vec::new()),
        ),
    ] {
        let mut seen = HashSet::new();
        for name in names {
            ensure!(
                is_role_name(name),
                "{key}: `{name}` must match ^[A-Za-z0-9][A-Za-z0-9._:-]{{0,254}}$"
            );
            ensure!(seen.insert(name), "{key}: `{name}` is listed twice");
        }
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
        ensure!(
            !user.temporary || user.password.is_some(),
            "users: `{}` has `temporary: true` and no password",
            user.username
        );
        if let Some(id) = &user.id {
            ensure!(
                !id.is_empty()
                    && id.len() <= 36
                    && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
                "users: id `{id}` of `{}` must be a UUID like 11111111-1111-1111-1111-111111111111",
                user.username
            );
        }
        for role in &user.roles {
            ensure!(
                link.roles.contains(role),
                "users: role `{role}` of `{}` is not in `roles`",
                user.username
            );
        }
        for name in user.attributes.keys() {
            ensure!(
                is_role_name(name),
                "users: attribute `{name}` of `{}` must match ^[A-Za-z0-9][A-Za-z0-9._:-]{{0,254}}$",
                user.username
            );
        }
    }
    Ok(())
}

fn check_redirect_uris(link: &OidcLink) -> Result<()> {
    ensure!(
        !link.redirect_uris.is_empty(),
        "redirect_uris: at least one is required (set them in the deployment overlay)"
    );
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

/// `^[A-Za-z0-9][A-Za-z0-9._:-]{0,254}$`: realm roles and client scopes,
/// safe in URLs and in single-quoted shell words.
pub fn is_role_name(value: &str) -> bool {
    let mut chars = value.chars();
    value.len() <= 255
        && chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || "._:-".contains(c))
}

/// A realm's `public_url` split into the URL of its domain and its path,
/// e.g. `https://app.example.com` and `/auth`; no path at the root.
pub fn split_public_url(url: &str) -> Result<(PublicUrl, Option<String>)> {
    let trimmed = url.trim_end_matches('/');
    let start = trimmed.find("://").map_or(0, |i| i + 3);
    let Some(slash) = trimmed[start..].find('/') else {
        return Ok((parse_url(url)?, None));
    };
    let (base, path) = trimmed.split_at(start + slash);
    ensure!(
        crate::shared::ingress::is_route_path(path),
        "url `{url}`: path `{path}` may use only letters, digits and `/-_.~`"
    );
    Ok((parse_url(base)?, Some(path.to_string())))
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

/// A provider of the requested type whose major version matches. An optional
/// link is skipped when no provider of its type runs; one that runs and
/// doesn't match is still an error.
pub fn resolve<'s>(link: &OidcLink, available: &'s [SharedAppConfig]) -> Result<Resolution<'s>> {
    if link.optional && !available.iter().any(|app| app.type_name == link.type_name) {
        return Ok(Resolution::Skip(format!(
            "optional and no {} is running",
            link.type_name
        )));
    }
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
            Some(url) => {
                let (base, path) = split_public_url(url)?;
                Some(base.url + path.as_deref().unwrap_or(""))
            }
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
    check_redirect_uris(link)?;
    let record = record(ctx, link)?;
    check_public_url(ctx, &record)?;
    let public_url = record.public_url.as_deref().unwrap_or(&endpoint.public_url);
    let internal_issuer = format!("{}/realms/{}", endpoint.internal_url, record.realm);
    let values: BTreeMap<&str, String> = BTreeMap::from([
        (
            "oidc_issuer",
            format!("{public_url}/realms/{}", record.realm),
        ),
        ("oidc_internal_url", internal_issuer.clone()),
        ("oidc_base_url", public_url.to_string()),
        ("oidc_internal_base_url", endpoint.internal_url.clone()),
        ("oidc_origin", origin(public_url)),
        (
            "oidc_jwks_uri",
            format!("{internal_issuer}/protocol/openid-connect/certs"),
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
            out.set_env(service, variable, link_value(value, &values)?)?;
        }
    }

    let text = serde_yaml_ng::to_string(&record)?;
    ctx.server
        .connector
        .write_file(&record_path(ctx, &link.name), text.as_bytes(), 0o600)
}

/// `https://auth.example.com` of `https://auth.example.com/some/path`.
fn origin(url: &str) -> String {
    let start = url.find("://").map_or(0, |i| i + 3);
    match url[start..].find('/') {
        Some(end) => url[..start + end].to_string(),
        None => url.to_string(),
    }
}

/// The realm on its own domain, or under a path of one: its paths, and
/// `/resources` for the shared static files of the login pages, with the path
/// removed before they reach the provider. `upstream` is the provider on the
/// ingress network.
pub fn exposed_routes(ctx: &AppCtx, link: &OidcLink, upstream: &str) -> Result<Exposed> {
    let Some(url) = &link.public_url else {
        bail!(
            "services.oidc[{}] needs `public_url` to be served on its own domain",
            link.name
        );
    };
    let (realm, _) = names(ctx.app, link);
    let (base, path) = split_public_url(url)?;
    let prefix = path.as_deref().unwrap_or("");
    let route = |name, below: String| ExposedRoute {
        name,
        path: format!("{prefix}{below}"),
        upstream: upstream.to_string(),
        strip_path: path.clone(),
    };
    Ok(Exposed {
        routes: vec![
            route("realm", format!("/realms/{realm}")),
            route("resources", "/resources".to_string()),
        ],
        domain: Some(base.domain),
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
            if !extra.contains(" redirect_uris:") {
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
            (", post_logout_redirect_uris: ['a##b']", "not a valid URI"),
            (", roles: ['a b']", "roles: `a b`"),
            (", roles: [x, x]", "roles: `x` is listed twice"),
            (", default_scopes: [\"it's\"]", "default_scopes: `it's`"),
            (", users: [{username: a, temporary: true}]", "no password"),
            (
                ", realm_settings: {access_token_lifespan: 2 hours later}",
                "realm_settings.access_token_lifespan: `2 hours later` is not a duration",
            ),
            (
                ", realm_settings: {sso_session_idle_timeout: 0s}",
                "is not a duration",
            ),
            (
                ", client_scopes: [{name: profile}]",
                "a scope Keycloak creates itself",
            ),
            (
                ", client_scopes: [{name: a}, {name: a}]",
                "`a` is listed twice",
            ),
            (
                ", client_scopes: [{name: 'a b'}]",
                "client_scopes: `a b` must match",
            ),
            (
                ", client_scopes: [{name: a, mappers: [{name: m, type: user_attribute, attribute: x, claim: ''}]}]",
                "client_scopes[a]: mapper `m`: `attribute` and `claim` must not be empty",
            ),
            (
                ", client_scopes: [{name: a, mappers: [{name: m, type: user_attribute, attribute: x, claim: y}, {name: m, type: user_attribute, attribute: x, claim: z}]}]",
                "mapper `m` is listed twice",
            ),
            (
                ", users: [{username: a, attributes: {'a b': x}}]",
                "users: attribute `a b` of `a` must match",
            ),
            (", users: [{username: a, id: \"1'1\"}]", "must be a UUID"),
            (
                ", users: [{username: a, roles: [admin]}]",
                "role `admin` of `a` is not in `roles`",
            ),
        ] {
            let text = text(extra);
            let err = validate_link(&link(&text), &app()).unwrap_err().to_string();
            assert!(err.contains(expected), "{text}: {err}");
        }
    }

    #[test]
    fn validates_realm_settings_roles_and_users() {
        validate_link(
            &link(&format!(
                "{{{BASE}, pkce: S256, post_logout_redirect_uris: ['https://a.example.com/'], \
                 default_scopes: [basic, web-origins], roles: [platform_admin, user], \
                 realm_settings: {{registration: true, ssl_required: external}}, \
                 users: [{{username: admin@x.org, id: 11111111-1111-1111-1111-111111111111, \
                 roles: [platform_admin], password: p, temporary: true}}]}}"
            )),
            &app(),
        )
        .unwrap();
    }

    #[test]
    fn rejects_unknown_settings_when_parsed() {
        for (extra, expected) in [
            ("pkce: S512", "unknown variant `S512`"),
            (
                "realm_settings: {ssl_required: some}",
                "unknown variant `some`",
            ),
            (
                "realm_settings: {registraton: true}",
                "unknown field `registraton`",
            ),
            (
                "client_scopes: [{name: a, mappers: [{name: m, type: hardcoded, attribute: x, claim: y}]}]",
                "unknown variant `hardcoded`",
            ),
            (
                "users: [{username: a, attributes: {x: {y: z}}}]",
                "attribute values are strings, numbers, booleans or lists of them",
            ),
        ] {
            let err = serde_yaml_ng::from_str::<OidcLink>(&format!("{{{BASE}, {extra}}}"))
                .unwrap_err()
                .to_string();
            assert!(err.contains(expected), "{extra}: {err}");
        }
    }

    #[test]
    fn public_url_may_have_a_path() {
        let split = |url: &str| {
            let (base, path) = split_public_url(url).unwrap();
            (base.url, base.domain, path)
        };
        assert_eq!(
            split("https://login.example.com/"),
            (
                "https://login.example.com".into(),
                "login.example.com".into(),
                None
            )
        );
        assert_eq!(
            split("https://app.example.com:8443/id/auth/"),
            (
                "https://app.example.com:8443".into(),
                "app.example.com".into(),
                Some("/id/auth".into())
            )
        );
        let err = split_public_url("https://a.example.com/auth?x=1")
            .err()
            .unwrap();
        assert!(err.to_string().contains("may use only"), "{err}");
        assert!(split_public_url("https://app.example.com/a b").is_err());
        assert!(split_public_url("app.example.com/auth").is_err());
    }

    #[test]
    fn origin_drops_the_path() {
        assert_eq!(
            origin("https://auth.example.com"),
            "https://auth.example.com"
        );
        assert_eq!(
            origin("https://app.example.com:8443/auth"),
            "https://app.example.com:8443"
        );
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
        // Optional: skipped only when no keycloak runs at all.
        assert!(
            pick(", optional: true, version: '27'").contains("no running keycloak oidc provider")
        );
        assert_eq!(pick(", optional: true, instance: kc25"), "kc25");
        let skipped = match resolve(&link(&format!("{{{BASE}, optional: true}}")), &[]) {
            Ok(Resolution::Skip(reason)) => reason,
            _ => panic!("not skipped"),
        };
        assert_eq!(skipped, "optional and no keycloak is running");
    }

    #[test]
    fn optional_links_need_redirect_uris_only_when_deployed() {
        let text = "{name: login, type: keycloak, targets: [backend], optional: true}";
        validate_link(&link(text), &app()).unwrap();
        let err = check_redirect_uris(&link(text)).unwrap_err();
        assert!(err.to_string().contains("redirect_uris"), "{err}");
    }
}
