//! Command-line interface: argument definitions and dispatch to command modules.

mod app;
mod context;
mod init;
mod server;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

use crate::config::local::DepapHome;
use crate::error::DepapError;

#[derive(Debug, Parser)]
#[command(name = "depap", version, about = "Manage docker-based server deployments")]
pub struct Cli {
    /// Target server (default: DEPAP_SERVER, then the current context)
    #[arg(short, long, global = true, env = "DEPAP_SERVER")]
    pub server: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create the local depap home with a new manager key
    Init,
    /// Define, inspect and deploy servers
    #[command(subcommand)]
    Server(ServerCommand),
    /// Use <NAME> as the default server for following commands
    Connect { name: String },
    /// Forget the default server
    Disconnect,
    /// Deploy and inspect user applications
    #[command(subcommand)]
    App(app::AppCommand),
    /// Inspect links between applications and shared apps
    #[command(subcommand)]
    Connector(ConnectorCommand),
}

#[derive(Debug, Subcommand)]
pub enum ServerCommand {
    /// Add or replace a server in the local configuration
    Define(server::DefineArgs),
    /// List defined servers
    List,
    /// Check and display server state
    State,
    /// Deploy shared applications from depap-server.yaml
    Deploy(DeployArgs),
}

#[derive(Debug, Subcommand)]
pub enum ConnectorCommand {
    /// Show shared apps and the applications linked to them
    List,
}

/// Arguments shared by `server deploy` and `app deploy`.
#[derive(Debug, Args)]
pub struct DeployArgs {
    /// Also start or restart services
    #[arg(long)]
    pub apply: bool,
    /// Config files merged in order (repeatable or comma-separated)
    #[arg(short = 'f', long = "file", value_delimiter = ',')]
    pub files: Vec<PathBuf>,
    /// Validate and show what would happen without changing the server
    #[arg(long)]
    pub dry_run: bool,
}

pub fn run(cli: Cli) -> Result<()> {
    let home = DepapHome::locate()?;
    let server = cli.server.as_deref();
    match cli.command {
        Command::Init => init::run(&home),
        Command::Server(ServerCommand::Define(args)) => server::define(&home, args),
        Command::Server(ServerCommand::List) => server::list(&home),
        Command::Server(ServerCommand::State) => server::state(&home, server),
        Command::Server(ServerCommand::Deploy(args)) => server::deploy(&home, server, args),
        Command::Connect { name } => context::connect(&home, &name),
        Command::Disconnect => context::disconnect(&home),
        Command::App(command) => app::run(&home, server, command),
        Command::Connector(ConnectorCommand::List) => {
            Err(DepapError::NotImplemented("connector list").into())
        }
    }
}
