//! Shared applications: the kind registry and the interface core uses (spec §9.1).
//!
//! Layers: core → kind (`ingress`, `database`, `oidc`, `monitoring`) → type (`caddy`,
//! `postgres`, `keycloak`, `openobserve`).
//! Core only talks to the [`Kind`] trait; each kind looks up its types and calls them.

pub mod database;
pub mod ingress;
pub mod monitoring;
pub mod oidc;

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use minijinja::{Environment, UndefinedBehavior};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_yaml_ng::Value;

pub use crate::app::contribution::Contribution;
pub use crate::backup::{ItemReader, ItemWriter, Role};
use crate::config::app::{AppConfig, AppService};
use crate::connector::Connector;
use semver::{Version, VersionReq};

/// One entry under `shared.<kind>` in compost-server.yaml.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SharedAppConfig {
    pub name: String,
    #[serde(rename = "type")]
    pub type_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Type-specific settings, parsed by the type itself.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub configuration: Value,
    /// Set for a server compost does not run, e.g. an existing database
    /// container or a hosted service; parsed by the kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external: Option<Value>,
    /// Send the shared app's output and metrics to a monitoring shared app.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monitoring: Option<SharedMonitoring>,
}

/// `monitoring` of a shared app: becomes a `services.monitoring` link of it.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SharedMonitoring {
    /// The monitoring shared app; needed when there are several.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    /// Collect the docker output of its containers.
    #[serde(default = "yes")]
    pub logs: bool,
    /// Scrape the metrics it serves, if its type serves any.
    #[serde(default = "yes")]
    pub metrics: bool,
}

fn yes() -> bool {
    true
}

/// What a monitoring link of a shared app watches (see [`Kind::monitored`]).
#[derive(Debug, Default)]
pub struct Monitored {
    /// Compose services of the shared app.
    pub services: Vec<&'static str>,
    /// Prometheus endpoints it serves.
    pub metrics: Vec<MetricsEndpoint>,
}

#[derive(Debug)]
pub struct MetricsEndpoint {
    pub service: &'static str,
    pub port: u16,
    pub path: &'static str,
}

impl SharedAppConfig {
    /// compost does not run it: no compose project, nothing to start or stop.
    pub fn is_external(&self) -> bool {
        self.external.is_some()
    }

    /// Parse the `configuration` block into a type's own struct.
    pub fn configuration<T: DeserializeOwned + Default>(&self) -> Result<T> {
        if self.configuration.is_null() {
            return Ok(T::default());
        }
        serde_yaml_ng::from_value(self.configuration.clone())
            .map_err(|e| anyhow::anyhow!("configuration: {e}"))
    }
}

/// Access to the server while rendering. Paths are relative to COMPOST_SERVER_HOME.
pub struct ServerCtx<'a> {
    pub connector: &'a dyn Connector,
    /// COMPOST_SERVER_HOME as an absolute path.
    pub home: String,
}

impl ServerCtx<'_> {
    pub fn path(&self, relative: &str) -> String {
        format!("{}/{relative}", self.home)
    }

    /// Read a file that an earlier deploy wrote, e.g. a generated secret.
    pub fn read(&self, relative: &str) -> Result<Option<String>> {
        self.connector.read_file(&self.path(relative))
    }
}

/// What a link hook knows: the server, the application and the shared app the
/// link was resolved to.
pub struct AppCtx<'a> {
    pub server: &'a ServerCtx<'a>,
    pub app: &'a AppConfig,
    pub shared: &'a SharedAppConfig,
    /// All links of the app being deployed, e.g. for an ingress route to
    /// another link. Empty outside deploy.
    pub peers: &'a [Peer<'a>],
}

/// A link of the app, resolved to a shared app.
pub struct Peer<'a> {
    pub kind: &'static dyn Kind,
    pub name: &'a str,
    pub entry: &'a Value,
    pub shared: &'a SharedAppConfig,
}

/// Paths a link puts behind an ingress (see [`Kind::exposed_routes`]).
#[derive(Debug)]
pub struct Exposed {
    pub routes: Vec<ExposedRoute>,
    /// The domain the link expects to be served on, if it has one; an
    /// ingress link routing to it must serve that domain.
    pub domain: Option<String>,
}

#[derive(Debug)]
pub struct ExposedRoute {
    /// Added to the route name, e.g. `realm`.
    pub name: &'static str,
    pub path: String,
    /// `<host>:<port>` on the ingress network.
    pub upstream: String,
}

impl<'a> AppCtx<'a> {
    /// A link record of this instance for an entry it no longer deploys: the
    /// deploy removes it, so conflict checks skip it. Only known during a
    /// deploy, when `peers` are set.
    pub fn is_dropped(&self, kind: &str, app: &str, prefix: &str, entry: &str) -> bool {
        !self.peers.is_empty()
            && app == self.app.name
            && prefix == self.app.prefix
            && !self
                .peers
                .iter()
                .any(|peer| peer.kind.name() == kind && peer.name == entry)
    }

    /// The context of another link of the app (`kind/entry`), if it is deployed with it.
    pub fn peer(&self, kind: &str, entry: &str) -> Option<(&'a Peer<'a>, AppCtx<'a>)> {
        let peer = self
            .peers
            .iter()
            .find(|peer| peer.kind.name() == kind && peer.name == entry)?;
        Some((
            peer,
            AppCtx {
                server: self.server,
                app: self.app,
                shared: peer.shared,
                peers: self.peers,
            },
        ))
    }

    /// Compose service behind an `app_services` name (checked during validation).
    pub fn compose_service(&self, app_service: &str) -> Result<&str> {
        self.app
            .app_service(app_service)
            .map(|s| s.service.as_str())
            .with_context(|| format!("`{app_service}` is not in app_services"))
    }

    /// Unique name of an app service on shared networks, e.g. `myapp-staging-backend`.
    pub fn service_alias(&self, compose_service: &str) -> String {
        format!("{}-{compose_service}", self.app.instance())
    }

    /// Name of a link's records on the server, e.g. `myapp-staging-system-data`.
    pub fn link_id(&self, entry: &str) -> String {
        format!("{}-{entry}", self.app.instance())
    }
}

/// Which shared app a link uses.
pub enum Resolution<'s> {
    Use(&'s SharedAppConfig),
    /// The link is not used in this deployment; the text says why.
    Skip(String),
}

/// Everything a shared app needs on the server, produced by `render`.
#[derive(Debug, Default)]
pub struct Rendered {
    pub files: Vec<RenderedFile>,
    /// Directories to create (relative), e.g. data folders mounted into containers.
    pub dirs: Vec<String>,
    /// External docker networks the shared app joins; compost creates them.
    pub networks: Vec<String>,
    /// Docker networks the shared app joins that must exist already, e.g.
    /// networks of containers compost does not manage. Never created.
    pub existing_networks: Vec<String>,
    /// Directories (relative) whose files are exactly the rendered ones:
    /// deploy deletes the other files in them.
    pub owned_dirs: Vec<String>,
}

#[derive(Debug)]
pub struct RenderedFile {
    /// Relative to COMPOST_SERVER_HOME.
    pub path: String,
    pub contents: String,
    /// Unix permissions; 0o600 marks a file holding secrets.
    pub mode: u32,
}

impl RenderedFile {
    pub fn new(path: String, contents: String) -> Self {
        Self {
            path,
            contents,
            mode: 0o644,
        }
    }

    pub fn secret(path: String, contents: String) -> Self {
        Self {
            path,
            contents,
            mode: 0o600,
        }
    }

    pub fn is_secret(&self) -> bool {
        self.mode & 0o077 == 0
    }
}

/// Starter configuration of a shared app type, for `server generate`.
/// Snippets are YAML list items, indented for `shared.<kind>`.
#[derive(Debug, Default)]
pub struct Generated {
    /// For compost-server.yaml: a setup that works on this machine.
    pub local: &'static str,
    /// For compost-server.prod.yaml: what a real server changes.
    pub prod: Option<&'static str>,
    /// For .compost-server.secrets.example.yaml.
    pub secrets: Option<&'static str>,
    /// Shown after the files are written.
    pub notes: &'static [&'static str],
    /// Other types (`kind/type`) the type needs; `server generate` adds them.
    pub requires: &'static [&'static str],
}

/// Links a shared app needs to other shared apps (spec §9.1). Core deploys the
/// shared app like a small application: [`consumer_app`] with these links.
#[derive(Debug, Default)]
pub struct SharedLinks {
    /// Compose services of the shared app that links can target.
    pub app_services: Vec<AppService>,
    /// Link entries by kind, as an app would write them under `services`.
    pub services: BTreeMap<String, Vec<Value>>,
}

/// What core needs from a kind. Methods grow with the spec (hooks, …).
pub trait Kind: Sync {
    /// Key under `shared:` / `services:`; also the value accepted by `--purge`.
    fn name(&self) -> &'static str;

    /// Types of this kind, e.g. `caddy`.
    fn type_names(&self) -> Vec<&'static str>;

    /// Starter configuration of one type.
    fn generate(&self, type_name: &str) -> Result<Generated>;

    /// Fill in defaults the type defines, e.g. a version. Unknown types are
    /// left alone; validation reports them.
    fn apply_defaults(&self, config: &mut SharedAppConfig);

    /// Validate one shared app of this kind.
    fn validate_shared(&self, config: &SharedAppConfig) -> Result<()>;

    /// Shared apps of this kind can be `external` servers compost does not run.
    fn supports_external(&self) -> bool {
        false
    }

    /// Whether an external shared app answers, e.g. `accepting connections`.
    fn check_external(&self, _ctx: &ServerCtx, _config: &SharedAppConfig) -> Result<String> {
        bail!("{} servers cannot be external", self.name())
    }

    /// Validate all shared apps of this kind together. Default: no rules.
    fn validate_instances(&self, _configs: &[SharedAppConfig]) -> Result<()> {
        Ok(())
    }

    /// Keys of `configuration` that are secrets and must not be stored in plain config.
    fn secret_keys(&self, config: &SharedAppConfig) -> Result<&'static [&'static str]>;

    /// Files, directories and networks for one shared app.
    fn render(&self, ctx: &ServerCtx, config: &SharedAppConfig) -> Result<Rendered>;

    /// Links to other shared apps, e.g. a database. Default: none.
    fn shared_links(&self, _config: &SharedAppConfig) -> Result<Option<SharedLinks>> {
        Ok(None)
    }

    /// Services and metrics endpoints of a shared app, for its `monitoring`.
    fn monitored(&self, _config: &SharedAppConfig) -> Result<Monitored> {
        bail!("{} shared apps cannot be monitored", self.name())
    }

    /// A shared app linking to a shared app of this kind starts after it.
    /// `false` for links that work while the other side is not running yet,
    /// which lets two shared apps link to each other (e.g. an ingress sending
    /// its logs to a monitoring app that the ingress serves).
    fn orders_start(&self) -> bool {
        true
    }

    /// How long `server deploy` waits for a shared app of this kind to run.
    fn start_timeout(&self) -> Duration {
        Duration::from_secs(60)
    }

    /// Apply changed configuration files to the running shared app without
    /// recreating it. Called when files other than its compose files changed.
    /// `Ok(false)`: not supported or not running, so deploy recreates it.
    fn reload_shared(&self, _ctx: &ServerCtx, _config: &SharedAppConfig) -> Result<bool> {
        Ok(false)
    }

    // --- Links from applications (entries under `services.<kind>` in compost.yaml).

    /// Hooks run in ascending order on deploy (and reverse order on remove).
    fn apply_order(&self) -> i32;

    /// Keys of a link entry that are secrets.
    fn link_secret_keys(&self) -> &'static [&'static str] {
        &[]
    }

    /// Validate one link entry (spec §6.3).
    fn validate_link(&self, link: &Value, app: &AppConfig) -> Result<()>;

    /// Pick the shared app for a link from the ones deployed and running.
    fn resolve<'s>(&self, link: &Value, available: &'s [SharedAppConfig])
    -> Result<Resolution<'s>>;

    /// Render phase: add networks and variables, write link records.
    fn prepare_application(&self, ctx: &AppCtx, link: &Value, out: &mut Contribution)
    -> Result<()>;

    /// Before the application's containers start.
    fn before_apply_application(&self, ctx: &AppCtx, link: &Value) -> Result<()>;

    /// After the application's containers started.
    fn after_apply_application(&self, ctx: &AppCtx, link: &Value) -> Result<()>;

    /// One line about a deployed link, for `app state`.
    fn describe_link(&self, ctx: &AppCtx, entry: &str) -> Result<String>;

    /// `--purge <kind>` is allowed: the kind holds data of the app.
    fn supports_purge(&self) -> bool {
        false
    }

    /// Links of this kind can be targets of ingress routes (`link: <kind>/<entry>`).
    fn exposes_routes(&self) -> bool {
        false
    }

    /// Paths and upstreams a link puts behind an ingress route.
    fn exposed_routes(&self, _ctx: &AppCtx, _link: &Value) -> Result<Exposed> {
        bail!(
            "{} links cannot be the target of an ingress route",
            self.name()
        )
    }

    /// Undo a link on `app remove`; with `purge`, also delete the app's data.
    /// Gets the link's name only: the app's config files may not be at hand.
    fn remove_application(&self, ctx: &AppCtx, entry: &str, purge: bool) -> Result<()>;

    // --- Backup and restore. Defaults: the kind has nothing to save.

    /// Save a shared app's data and the secrets needed to restore it.
    fn backup_shared(
        &self,
        _ctx: &ServerCtx,
        _config: &SharedAppConfig,
        _out: &mut ItemWriter,
    ) -> Result<()> {
        Ok(())
    }

    /// Before the shared app is deployed: put back files its render and start
    /// depend on (e.g. a generated password).
    fn restore_shared_files(
        &self,
        _ctx: &ServerCtx,
        _config: &SharedAppConfig,
        _from: &ItemReader,
    ) -> Result<()> {
        Ok(())
    }

    /// After the shared app started: load its data.
    fn restore_shared_data(
        &self,
        _ctx: &ServerCtx,
        _config: &SharedAppConfig,
        _from: &ItemReader,
    ) -> Result<()> {
        Ok(())
    }

    /// Save what a link holds for the app (data, or configuration for review).
    /// Item names start with the entry name.
    fn backup_link(&self, _ctx: &AppCtx, _entry: &str, _out: &mut ItemWriter) -> Result<()> {
        Ok(())
    }

    /// Load a link's saved data while the app is stopped. Existing data is
    /// only replaced with `replace`.
    fn restore_link(
        &self,
        _ctx: &AppCtx,
        _entry: &str,
        _from: &ItemReader,
        _replace: bool,
    ) -> Result<()> {
        Ok(())
    }
}

/// Parse a link entry into a kind's own struct.
pub fn parse_link<T: DeserializeOwned>(link: &Value) -> Result<T> {
    serde_yaml_ng::from_value(link.clone()).map_err(|e| anyhow::anyhow!("{e}"))
}

static KINDS: &[&dyn Kind] = &[
    &database::DatabaseKind,
    &ingress::IngressKind,
    &monitoring::MonitoringKind,
    &oidc::OidcKind,
];

pub fn find_kind(name: &str) -> Option<&'static dyn Kind> {
    KINDS.iter().copied().find(|kind| kind.name() == name)
}

pub fn kind_names() -> Vec<&'static str> {
    KINDS.iter().map(|kind| kind.name()).collect()
}

/// App names starting with this are reserved for shared apps with links.
pub const CONSUMER_PREFIX: &str = "compost-";

/// A shared app with links, seen as an application: name `compost-<kind>`,
/// prefix `<name>`, so its instance is the shared app's compose project.
pub fn consumer_app(kind: &str, config: &SharedAppConfig, links: SharedLinks) -> AppConfig {
    AppConfig {
        name: format!("{CONSUMER_PREFIX}{kind}"),
        prefix: config.name.clone(),
        deployment: Default::default(),
        services: links.services,
        app_services: links.app_services,
        backup: Default::default(),
    }
}

/// The kind of a shared app whose link records use `app`, if `app` is one.
pub fn consumer_kind(app: &str) -> Option<&'static str> {
    let kind = app.strip_prefix(CONSUMER_PREFIX)?;
    find_kind(kind).map(|kind| kind.name())
}

/// The one shared app of `type_name` whose major version matches `version`
/// (a semver requirement), optionally pinned by name. `what` names it in
/// errors, e.g. `database`.
pub fn resolve_versioned<'s>(
    what: &str,
    type_name: &str,
    version: &str,
    instance: Option<&str>,
    available: &'s [SharedAppConfig],
) -> Result<&'s SharedAppConfig> {
    let requirement = VersionReq::parse(version)?;
    let matching: Vec<&SharedAppConfig> = available
        .iter()
        .filter(|app| app.type_name == type_name)
        .filter(|app| {
            app.version
                .is_some_and(|v| requirement.matches(&Version::new(v.into(), 0, 0)))
        })
        .filter(|app| instance.is_none_or(|pin| pin == app.name))
        .collect();
    match matching.as_slice() {
        [found] => Ok(found),
        [] => {
            let pin = instance
                .map(|p| format!(" named `{p}`"))
                .unwrap_or_default();
            bail!("no running {type_name} {what}{pin} with version `{version}` on the server")
        }
        several => {
            let names: Vec<&str> = several.iter().map(|app| app.name.as_str()).collect();
            bail!(
                "several {what}s match ({}); choose one with `instance`",
                names.join(", ")
            )
        }
    }
}

/// `shared/<kind>/<name>`: the shared app's folder and compose project directory.
pub fn shared_dir(kind: &str, name: &str) -> String {
    format!("shared/{kind}/{name}")
}

/// `connectors/<kind>/<name>`: link records of a shared app.
pub fn connectors_dir(kind: &str, name: &str) -> String {
    format!("connectors/{kind}/{name}")
}

/// Docker compose project of a shared app, e.g. `compost-database-postgres16`.
pub fn project_name(kind: &str, name: &str) -> String {
    format!("compost-{kind}-{name}")
}

/// Render an embedded template. Using an undefined variable is an error.
pub fn render_template(source: &str, context: minijinja::Value) -> Result<String> {
    let mut env = Environment::new();
    env.set_undefined_behavior(UndefinedBehavior::Strict);
    // Generated files end with a newline, like their templates.
    env.set_syntax(
        minijinja::syntax::SyntaxConfig::builder()
            .keep_trailing_newline(true)
            .build()
            .context("invalid template syntax configuration")?,
    );
    env.render_str(source, context)
        .context("template rendering failed (this is a bug in compost)")
}

/// `http(s)://<domain>[:port]`, split into its parts.
pub struct PublicUrl {
    /// The URL without a trailing `/`.
    pub url: String,
    pub domain: String,
}

pub fn parse_url(url: &str) -> Result<PublicUrl> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"));
    let Some(rest) = rest else {
        bail!("url `{url}` must start with https:// or http://");
    };
    let authority = rest.strip_suffix('/').unwrap_or(rest);
    let (domain, port) = match authority.split_once(':') {
        Some((domain, port)) => (domain, Some(port)),
        None => (authority, None),
    };
    ensure!(
        is_domain(domain) && !domain.starts_with("*."),
        "url `{url}`: `{domain}` is not a valid domain name (a path is not supported)"
    );
    if let Some(port) = port {
        ensure!(
            port.parse::<u16>().is_ok_and(|p| p > 0),
            "url `{url}`: invalid port `{port}`"
        );
    }
    Ok(PublicUrl {
        url: url.trim_end_matches('/').to_string(),
        domain: domain.to_string(),
    })
}

/// Letters, digits, `-` and `.`, optionally starting with `*.`.
pub fn is_domain(domain: &str) -> bool {
    let name = domain.strip_prefix("*.").unwrap_or(domain);
    !name.is_empty()
        && name.split('.').all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && label
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        })
}

/// Letters, digits, `_`, `.` and `-`, starting with a letter or digit, as docker allows.
pub fn is_network_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
}

/// An environment variable name: letters, digits and `_`, not starting with a digit.
pub fn is_variable_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A new random password: 32 hexadecimal characters (122 random bits).
pub fn generate_password() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}
