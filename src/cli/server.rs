//! `depap server …` commands (spec §10.3, §10.4, §10.6, §10.7).

use std::path::PathBuf;

use anyhow::Result;
use clap::Args;

use super::DeployArgs;
use crate::config::local::{Connector, DepapHome, ServerEntry};
use crate::config::{check_name, server::ServerConfig};
use crate::error::{DepapError, config_err};

const SERVER_FILE: &str = "depap-server.yaml";
const SERVER_SECRETS_FILE: &str = ".depap-server.secrets.yaml";

#[derive(Debug, Args)]
pub struct DefineArgs {
    /// Server name used in other commands
    pub name: String,
    /// Host name or IP address (SSH)
    #[arg(long, required_unless_present = "local", conflicts_with = "local")]
    pub address: Option<String>,
    /// SSH user
    #[arg(long, default_value = "depapuser", conflicts_with = "local")]
    pub user: String,
    /// SSH port
    #[arg(long, conflicts_with = "local")]
    pub port: Option<u16>,
    /// SSH private key (default: ssh config, agent, ~/.ssh/id_*)
    #[arg(long, conflicts_with = "local")]
    pub identity_file: Option<String>,
    /// depap directory on the server (default ~/.depap-server)
    #[arg(long)]
    pub server_home: Option<String>,
    /// Manage this machine instead of a remote server
    #[arg(long)]
    pub local: bool,
    /// Replace an existing server with the same name
    #[arg(long)]
    pub force: bool,
}

pub fn define(home: &DepapHome, args: DefineArgs) -> Result<()> {
    check_name("server name", &args.name)?;
    let mut servers = home.servers()?;

    let connector = match args.address {
        Some(address) => Connector::Ssh {
            address,
            port: args.port,
            user: Some(args.user),
            identity_file: args.identity_file,
        },
        None => Connector::Local,
    };
    let entry = ServerEntry {
        name: args.name,
        connector,
        server_home: args.server_home,
    };

    match servers.servers.iter_mut().find(|s| s.name == entry.name) {
        Some(_) if !args.force => {
            return Err(config_err(format!(
                "server `{}` is already defined (use --force to replace it)",
                entry.name
            )));
        }
        Some(existing) => *existing = entry.clone(),
        None => servers.servers.push(entry.clone()),
    }
    home.save_servers(&servers)?;
    println!("Defined server {} ({})", entry.name, entry.connector.describe());
    Ok(())
}

pub fn list(home: &DepapHome) -> Result<()> {
    let servers = home.servers()?;
    if servers.servers.is_empty() {
        println!("No servers defined. Add one with `depap server define`.");
        return Ok(());
    }

    let current = home.current_context()?;
    let width = servers.servers.iter().map(|s| s.name.len()).max().unwrap_or(0);
    for server in &servers.servers {
        let marker = if current.as_deref() == Some(server.name.as_str()) { "*" } else { " " };
        println!(
            "{marker} {:<width$}  {}  home: {}",
            server.name,
            server.connector.describe(),
            server.server_home()
        );
    }
    Ok(())
}

pub fn state(home: &DepapHome, server: Option<&str>) -> Result<()> {
    home.resolve_server(server)?;
    Err(DepapError::NotImplemented("server state").into())
}

pub fn deploy(home: &DepapHome, server: Option<&str>, args: DeployArgs) -> Result<()> {
    let server = home.resolve_server(server)?;
    let files = if args.files.is_empty() { default_files()? } else { args.files };
    let config = ServerConfig::load(&files)?;

    if !args.dry_run {
        return Err(DepapError::NotImplemented("server deploy (use --dry-run)").into());
    }
    let mode = if args.apply { "deploy --apply" } else { "deploy" };
    println!("Configuration is valid. `{mode}` on {} would manage:", server.name);
    for (kind, app) in config.shared_apps() {
        let version = app.version.map_or_else(|| "-".to_string(), |v| v.to_string());
        println!("  {kind:<9} {:<16} {} {version}", app.name, app.type_name);
    }
    Ok(())
}

/// `depap-server.yaml` plus the secrets file when it exists (spec §7).
fn default_files() -> Result<Vec<PathBuf>> {
    let main = PathBuf::from(SERVER_FILE);
    if !main.exists() {
        return Err(config_err(format!(
            "{SERVER_FILE} not found in the current directory (or pass files with -f)"
        )));
    }
    let mut files = vec![main];
    let secrets = PathBuf::from(SERVER_SECRETS_FILE);
    if secrets.exists() {
        files.push(secrets);
    }
    Ok(files)
}
