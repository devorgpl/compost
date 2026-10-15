//! `depap server …` commands (spec §10.3, §10.4, §10.6, §10.7).

use std::path::PathBuf;

use anyhow::Result;
use clap::Args;

use super::DeployArgs;
use crate::config::local::{ConnectorConfig, DepapHome, ServerEntry};
use crate::config::{check_name, server::ServerConfig};
use crate::connector;
use crate::error::{DepapError, config_err};
use crate::server::deploy::{self, Options};
use crate::server::state::{self, Status};

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
        Some(address) => ConnectorConfig::Ssh {
            address,
            port: args.port,
            user: Some(args.user),
            identity_file: args.identity_file,
        },
        None => ConnectorConfig::Local,
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

pub fn state(home: &DepapHome, server: Option<&str>, verbose: bool) -> Result<()> {
    let manager = home.require_manager()?;
    let server = home.resolve_server(server)?;
    let connector = connector::open(&server, verbose);
    let report = state::check(connector.as_ref(), server.server_home(), &manager.manager_key)?;

    let summary = format!("{} ({}): {}", server.name, connector.target(), report.status);
    let detail = report.detail.as_deref().unwrap_or_default();
    match report.status {
        Status::Unreachable | Status::NoDocker | Status::Foreign => {
            return Err(DepapError::ServerNotReady(format!("{summary}\n{detail}")).into());
        }
        Status::Uninitialized | Status::Degraded | Status::Ok => {}
    }

    println!("{summary}");
    if let (Some(docker), Some(server_home)) = (&report.docker, &report.home) {
        println!("  {docker}, home {server_home}");
    }
    if report.shared.is_empty() {
        println!("  no shared applications");
    }
    for app in &report.shared {
        let version = app.version.map_or_else(|| "-".to_string(), |v| v.to_string());
        let running = if app.running { "running" } else { "not running" };
        println!(
            "  {:<9} {:<16} {} {version}  {running}",
            app.kind, app.name, app.type_name
        );
    }

    if report.status == Status::Degraded {
        return Err(DepapError::Unhealthy(format!(
            "{}: some shared applications are not running",
            server.name
        ))
        .into());
    }
    Ok(())
}

pub fn deploy(home: &DepapHome, server: Option<&str>, args: DeployArgs, verbose: bool) -> Result<()> {
    let manager = home.require_manager()?;
    let server = home.resolve_server(server)?;
    let files = if args.files.is_empty() { default_files()? } else { args.files };
    let config = ServerConfig::load(&files)?;

    let connector = connector::open(&server, verbose);
    println!("Deploying to {} ({})", server.name, connector.target());
    let options = Options {
        apply: args.apply,
        dry_run: args.dry_run,
        force_unlock: args.force_unlock,
    };
    deploy::deploy(
        connector.as_ref(),
        server.server_home(),
        &manager.manager_key,
        &config,
        &options,
    )?;

    if args.apply && !args.dry_run {
        println!();
        return state(home, Some(&server.name), verbose);
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
