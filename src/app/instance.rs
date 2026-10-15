//! An application instance on the server: its folder and the files deploy writes there
//! (spec §8). `app state` and `app remove` work from these files alone.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::app::AppConfig;
use crate::config::server::ServerConfig;
use crate::connector::shell_quote;
use crate::shared::{ServerCtx, SharedAppConfig};

pub const OVERRIDE_FILE: &str = "compose/depap.override.yaml";
/// The effective app config, secrets removed.
pub const CONFIG_FILE: &str = "depap.yaml";
/// The shared apps the instance's links use.
pub const LINKS_FILE: &str = "depap-env.yaml";

/// `apps/<app>/<prefix>`, relative to DEPAP_SERVER_HOME.
pub fn dir(name: &str, prefix: &str) -> String {
    format!("apps/{name}/{prefix}")
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PinnedLinks {
    pub links: Vec<PinnedLink>,
}

/// One link and the shared app it was resolved to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PinnedLink {
    pub kind: String,
    pub entry: String,
    /// Name of the shared app.
    pub instance: String,
    #[serde(rename = "type")]
    pub type_name: String,
    pub version: Option<u32>,
}

impl PinnedLink {
    /// The shared app from the server config, if it is still configured.
    pub fn shared_app(&self, server_config: &ServerConfig) -> Option<SharedAppConfig> {
        server_config
            .shared
            .get(&self.kind)?
            .iter()
            .find(|app| app.name == self.instance)
            .cloned()
    }
}

/// What deploy stored for an instance.
pub struct Instance {
    pub app: AppConfig,
    pub links: Vec<PinnedLink>,
}

/// The stored instance, or `None` if it was never deployed (or is removed).
pub fn load(ctx: &ServerCtx, name: &str, prefix: &str) -> Result<Option<Instance>> {
    let dir = dir(name, prefix);
    let config_path = format!("{dir}/{CONFIG_FILE}");
    let Some(text) = ctx.read(&config_path)? else {
        return Ok(None);
    };
    let app: AppConfig = serde_yaml_ng::from_str(&text)
        .with_context(|| format!("invalid {} on the server", ctx.path(&config_path)))?;
    let links = match ctx.read(&format!("{dir}/{LINKS_FILE}"))? {
        Some(text) => serde_yaml_ng::from_str::<PinnedLinks>(&text)?.links,
        None => Vec::new(),
    };
    Ok(Some(Instance { app, links }))
}

/// The server's config of shared apps, or an empty one.
pub fn server_config(ctx: &ServerCtx) -> Result<ServerConfig> {
    match ctx.read(crate::server::SERVER_CONFIG_FILE)? {
        Some(text) => ServerConfig::parse(&text),
        None => Ok(ServerConfig::default()),
    }
}

/// `docker compose` for the instance, with the user's file and depap's override.
pub fn compose_command(ctx: &ServerCtx, app: &AppConfig) -> String {
    format!(
        "cd {} && docker compose -p {} --project-directory sources -f {} -f {OVERRIDE_FILE}",
        shell_quote(&ctx.path(&dir(&app.name, &app.prefix))),
        shell_quote(&app.instance()),
        shell_quote(&format!("sources/{}", app.deployment.compose_file)),
    )
}
