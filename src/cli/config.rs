//! `compost config …`: preferences in `COMPOST_HOME`, globally or per server.

use anyhow::Result;
use clap::Subcommand;

use crate::config::check_name;
use crate::config::local::{CompostHome, DeploySettings};
use crate::error::config_err;

const DEPLOY_APPLY: &str = "deploy.apply";
const DEV_SERVER: &str = "dev.server";
const KEYS: &[&str] = &[DEPLOY_APPLY, DEV_SERVER];

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Set a preference, e.g. `compost config set deploy.apply true` or
    /// `compost config set dev.server laptop`
    Set {
        key: String,
        value: String,
        /// Only for this server (overrides the global value)
        #[arg(long)]
        for_server: Option<String>,
    },
    /// Remove a preference, so the global value (or the default) applies
    Unset {
        key: String,
        /// Only for this server
        #[arg(long)]
        for_server: Option<String>,
    },
    /// Show the preferences
    Show,
}

pub fn run(home: &CompostHome, command: ConfigCommand) -> Result<()> {
    match command {
        ConfigCommand::Set {
            key,
            value,
            for_server,
        } if key == DEV_SERVER => update_dev_server(home, for_server.as_deref(), Some(value)),
        ConfigCommand::Set {
            key,
            value,
            for_server,
        } => {
            let value = parse_bool(&key, &value)?;
            update(home, &key, for_server.as_deref(), Some(value))
        }
        ConfigCommand::Unset { key, for_server } if key == DEV_SERVER => {
            update_dev_server(home, for_server.as_deref(), None)
        }
        ConfigCommand::Unset { key, for_server } => update(home, &key, for_server.as_deref(), None),
        ConfigCommand::Show => show(home),
    }
}

/// Set (`Some`) or remove (`None`) `dev.server`: a global setting naming a
/// local server.
fn update_dev_server(
    home: &CompostHome,
    server: Option<&str>,
    value: Option<String>,
) -> Result<()> {
    if server.is_some() {
        return Err(config_err(format!(
            "{DEV_SERVER} is global; leave out --for-server"
        )));
    }
    home.require_manager()?;
    if let Some(name) = &value {
        check_name("server name", name)?;
        let entry = home.find_server(name)?;
        if !matches!(
            entry.connector,
            crate::config::local::ConnectorConfig::Local
        ) {
            return Err(config_err(format!(
                "{DEV_SERVER}: `{name}` is an SSH server; dev commands only manage local servers"
            )));
        }
    }
    let mut settings = home.settings()?;
    settings.dev.server.clone_from(&value);
    home.save_settings(&settings)?;
    match value {
        Some(name) => out!("{DEV_SERVER} = {name}"),
        None => out!("{DEV_SERVER} unset"),
    }
    Ok(())
}

fn check_key(key: &str) -> Result<()> {
    if KEYS.contains(&key) {
        Ok(())
    } else {
        Err(config_err(format!(
            "unknown setting `{key}` (known: {})",
            KEYS.join(", ")
        )))
    }
}

fn parse_bool(key: &str, value: &str) -> Result<bool> {
    check_key(key)?;
    match value {
        "true" | "yes" | "on" => Ok(true),
        "false" | "no" | "off" => Ok(false),
        _ => Err(config_err(format!(
            "{key}: `{value}` is not a boolean (true or false)"
        ))),
    }
}

/// Set (`Some`) or remove (`None`) `deploy.apply`, globally or for one server.
fn update(home: &CompostHome, key: &str, server: Option<&str>, value: Option<bool>) -> Result<()> {
    check_key(key)?;
    home.require_manager()?;
    let scope = if let Some(name) = server {
        let mut servers = home.servers()?;
        let entry = servers
            .servers
            .iter_mut()
            .find(|s| s.name == name)
            .ok_or_else(|| config_err(format!("unknown server `{name}`")))?;
        entry.deploy.apply = value;
        home.save_servers(&servers)?;
        format!(" for {name}")
    } else {
        let mut settings = home.settings()?;
        settings.deploy.apply = value;
        home.save_settings(&settings)?;
        String::new()
    };
    if let Some(value) = value {
        out!("{key} = {value}{scope}");
    } else {
        out!("{key} unset{scope}");
    }
    Ok(())
}

fn show(home: &CompostHome) -> Result<()> {
    let settings = home.settings()?;
    out!(
        "deploy.apply = {}",
        describe(&settings.deploy, "false (default)")
    );
    for server in home.servers()?.servers {
        if !server.deploy.is_empty() {
            out!(
                "deploy.apply = {}  (server {})",
                describe(&server.deploy, "-"),
                server.name
            );
        }
    }
    out!(
        "{DEV_SERVER} = {}",
        settings.dev.server.as_deref().unwrap_or("- (default)")
    );
    Ok(())
}

fn describe(settings: &DeploySettings, unset: &str) -> String {
    settings
        .apply
        .map_or_else(|| unset.to_string(), |v| v.to_string())
}
