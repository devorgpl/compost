//! `depap config …`: preferences in DEPAP_HOME, globally or per server.

use anyhow::Result;
use clap::Subcommand;

use crate::config::local::{DepapHome, DeploySettings};
use crate::error::config_err;

const KEYS: &[&str] = &["deploy.apply"];

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Set a preference, e.g. `depap config set deploy.apply true`
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

pub fn run(home: &DepapHome, command: ConfigCommand) -> Result<()> {
    match command {
        ConfigCommand::Set {
            key,
            value,
            for_server,
        } => {
            let value = parse_bool(&key, &value)?;
            update(home, &key, for_server.as_deref(), Some(value))
        }
        ConfigCommand::Unset { key, for_server } => update(home, &key, for_server.as_deref(), None),
        ConfigCommand::Show => show(home),
    }
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
fn update(home: &DepapHome, key: &str, server: Option<&str>, value: Option<bool>) -> Result<()> {
    check_key(key)?;
    home.require_manager()?;
    let scope = match server {
        Some(name) => {
            let mut servers = home.servers()?;
            let entry = servers
                .servers
                .iter_mut()
                .find(|s| s.name == name)
                .ok_or_else(|| config_err(format!("unknown server `{name}`")))?;
            entry.deploy.apply = value;
            home.save_servers(&servers)?;
            format!(" for {name}")
        }
        None => {
            let mut settings = home.settings()?;
            settings.deploy.apply = value;
            home.save_settings(&settings)?;
            String::new()
        }
    };
    match value {
        Some(value) => out!("{key} = {value}{scope}"),
        None => out!("{key} unset{scope}"),
    }
    Ok(())
}

fn show(home: &DepapHome) -> Result<()> {
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
    Ok(())
}

fn describe(settings: &DeploySettings, unset: &str) -> String {
    settings
        .apply
        .map_or_else(|| unset.to_string(), |v| v.to_string())
}
