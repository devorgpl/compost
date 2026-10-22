//! `compost dev …`: development environments on this machine.

pub mod shared;

use anyhow::Result;

use crate::config::local::{
    CompostHome, ConnectorConfig, DeploySettings, LogsSettings, ServerEntry, TraceLevel,
};
use crate::error::config_err;

/// Name of the local server `dev` defines when there is none.
pub const DEFAULT_SERVER: &str = "local";

/// The local server dev commands use: `--server`/`COMPOST_SERVER`, then the
/// `dev.server` setting, then the only local server (or the current context,
/// if it is one of several), else a new one named `local`. Never an SSH
/// server, and never the current context when it is one.
pub fn target(home: &CompostHome, flag: Option<&str>) -> Result<ServerEntry> {
    let named = match flag {
        Some(name) => Some((name.to_string(), "--server")),
        None => home
            .settings()?
            .dev
            .server
            .map(|name| (name, "the dev.server setting")),
    };
    if let Some((name, source)) = named {
        let server = home.find_server(&name)?;
        return require_local(server, source);
    }

    let mut servers = home.servers()?;
    let local: Vec<&ServerEntry> = servers
        .servers
        .iter()
        .filter(|s| matches!(s.connector, ConnectorConfig::Local))
        .collect();
    match local.as_slice() {
        [server] => return Ok((*server).clone()),
        [] => {}
        several => {
            let current = home.current_context()?;
            if let Some(server) = several.iter().find(|s| Some(&s.name) == current.as_ref()) {
                return Ok((*server).clone());
            }
            let names: Vec<&str> = several.iter().map(|s| s.name.as_str()).collect();
            return Err(config_err(format!(
                "several local servers ({}); choose one with `compost config set dev.server <name>`",
                names.join(", ")
            )));
        }
    }
    if servers.servers.iter().any(|s| s.name == DEFAULT_SERVER) {
        return Err(config_err(format!(
            "server `{DEFAULT_SERVER}` is not local; define a local server with `compost server define <name> --local`"
        )));
    }
    let server = ServerEntry {
        name: DEFAULT_SERVER.to_string(),
        connector: ConnectorConfig::Local,
        server_home: None,
        prefix: None,
        deploy: DeploySettings::default(),
        logs: LogsSettings::default(),
        trace: TraceLevel::None,
    };
    servers.servers.push(server.clone());
    home.save_servers(&servers)?;
    out!(
        "Defined server {DEFAULT_SERVER}: this machine, compost's folder {}",
        server.server_home()
    );
    Ok(server)
}

fn require_local(server: ServerEntry, source: &str) -> Result<ServerEntry> {
    match server.connector {
        ConnectorConfig::Local => Ok(server),
        ConnectorConfig::Ssh { .. } => Err(config_err(format!(
            "dev commands only manage this machine, and `{}` (from {source}) is an SSH server",
            server.name
        ))),
    }
}
