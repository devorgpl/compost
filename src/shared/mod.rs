//! Shared applications: the kind registry and the interface core uses (spec §9.1).
//!
//! Layers: core → kind (`ingress`, `database`) → type (`caddy`, `postgres`).
//! Core only talks to the [`Kind`] trait; each kind looks up its types and calls them.

pub mod database;
pub mod ingress;

use anyhow::{Context, Result};
use minijinja::{Environment, UndefinedBehavior};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_yaml_ng::Value;

pub use crate::app::contribution::Contribution;
use crate::config::app::AppConfig;
use crate::connector::Connector;

/// One entry under `shared.<kind>` in depap-server.yaml.
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
}

impl SharedAppConfig {
    /// Parse the `configuration` block into a type's own struct.
    pub fn configuration<T: DeserializeOwned + Default>(&self) -> Result<T> {
        if self.configuration.is_null() {
            return Ok(T::default());
        }
        serde_yaml_ng::from_value(self.configuration.clone())
            .map_err(|e| anyhow::anyhow!("configuration: {e}"))
    }
}

/// Access to the server while rendering. Paths are relative to DEPAP_SERVER_HOME.
pub struct ServerCtx<'a> {
    pub connector: &'a dyn Connector,
    /// DEPAP_SERVER_HOME as an absolute path.
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
}

impl AppCtx<'_> {
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
    /// External docker networks the shared app joins.
    pub networks: Vec<String>,
}

#[derive(Debug)]
pub struct RenderedFile {
    /// Relative to DEPAP_SERVER_HOME.
    pub path: String,
    pub contents: String,
    /// Unix permissions; 0o600 marks a file holding secrets.
    pub mode: u32,
}

impl RenderedFile {
    pub fn new(path: String, contents: String) -> Self {
        Self { path, contents, mode: 0o644 }
    }

    pub fn secret(path: String, contents: String) -> Self {
        Self { path, contents, mode: 0o600 }
    }

    pub fn is_secret(&self) -> bool {
        self.mode & 0o077 == 0
    }
}

/// What core needs from a kind. Methods grow with the spec (hooks, …).
pub trait Kind: Sync {
    /// Key under `shared:` / `services:`; also the value accepted by `--purge`.
    fn name(&self) -> &'static str;

    /// Validate one shared app of this kind.
    fn validate_shared(&self, config: &SharedAppConfig) -> Result<()>;

    /// Validate all shared apps of this kind together. Default: no rules.
    fn validate_instances(&self, _configs: &[SharedAppConfig]) -> Result<()> {
        Ok(())
    }

    /// Keys of `configuration` that are secrets and must not be stored in plain config.
    fn secret_keys(&self, config: &SharedAppConfig) -> Result<&'static [&'static str]>;

    /// Files, directories and networks for one shared app.
    fn render(&self, ctx: &ServerCtx, config: &SharedAppConfig) -> Result<Rendered>;

    // --- Links from applications (entries under `services.<kind>` in depap.yaml).

    /// Hooks run in ascending order on deploy (and reverse order on remove).
    fn apply_order(&self) -> i32;

    /// Keys of a link entry that are secrets.
    fn link_secret_keys(&self) -> &'static [&'static str] {
        &[]
    }

    /// Validate one link entry (spec §6.3).
    fn validate_link(&self, link: &Value, app: &AppConfig) -> Result<()>;

    /// Pick the shared app for a link from the ones deployed and running.
    fn resolve<'s>(&self, link: &Value, available: &'s [SharedAppConfig]) -> Result<Resolution<'s>>;

    /// Render phase: add networks and variables, write link records.
    fn prepare_application(&self, ctx: &AppCtx, link: &Value, out: &mut Contribution) -> Result<()>;

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

    /// Undo a link on `app remove`; with `purge`, also delete the app's data.
    /// Gets the link's name only: the app's config files may not be at hand.
    fn remove_application(&self, ctx: &AppCtx, entry: &str, purge: bool) -> Result<()>;
}

/// Parse a link entry into a kind's own struct.
pub fn parse_link<T: DeserializeOwned>(link: &Value) -> Result<T> {
    serde_yaml_ng::from_value(link.clone()).map_err(|e| anyhow::anyhow!("{e}"))
}

static KINDS: &[&dyn Kind] = &[&database::DatabaseKind, &ingress::IngressKind];

pub fn find_kind(name: &str) -> Option<&'static dyn Kind> {
    KINDS.iter().copied().find(|kind| kind.name() == name)
}

pub fn kind_names() -> Vec<&'static str> {
    KINDS.iter().map(|kind| kind.name()).collect()
}

/// `shared/<kind>/<name>`: the shared app's folder and compose project directory.
pub fn shared_dir(kind: &str, name: &str) -> String {
    format!("shared/{kind}/{name}")
}

/// `connectors/<kind>/<name>`: link records of a shared app.
pub fn connectors_dir(kind: &str, name: &str) -> String {
    format!("connectors/{kind}/{name}")
}

/// Docker compose project of a shared app, e.g. `depap-database-postgres16`.
pub fn project_name(kind: &str, name: &str) -> String {
    format!("depap-{kind}-{name}")
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
        .context("template rendering failed (this is a bug in depap)")
}

/// A new random password: 32 hexadecimal characters (122 random bits).
pub fn generate_password() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}
