//! Command-line interface: argument definitions and dispatch to command modules.

mod app;
mod config;
mod context;
mod generate;
mod init;
mod secret;
mod server;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

use crate::backup::Format;
use crate::config::local::{DepapHome, ServerEntry};
use crate::config::server::BackupTarget;

#[derive(Debug, Parser)]
#[command(
    name = "depap",
    version,
    about = "Manage docker-based server deployments"
)]
pub struct Cli {
    /// Target server (default: DEPAP_SERVER, then the current context)
    #[arg(short, long, global = true, env = "DEPAP_SERVER")]
    pub server: Option<String>,

    /// Print every command run on the server
    #[arg(short, long, global = true)]
    pub verbose: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create the local depap home with a new (or the given) manager key
    Init(init::InitArgs),
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
    /// Inspect backups
    #[command(subcommand)]
    Backup(BackupCommand),
    /// Store application secrets on the server for `app deploy`
    #[command(subcommand)]
    Secret(secret::SecretCommand),
    /// Set and show preferences, e.g. `deploy.apply`
    #[command(subcommand)]
    Config(config::ConfigCommand),
}

#[derive(Debug, Subcommand)]
pub enum ServerCommand {
    /// Write starter depap-server.yaml, depap-server.prod.yaml and a secrets example
    Generate(generate::ServerGenerateArgs),
    /// Add or replace a server in the local configuration
    Define(server::DefineArgs),
    /// List defined servers
    List,
    /// Check and display server state
    State,
    /// Deploy shared applications from depap-server.yaml
    Deploy(DeployArgs),
    /// Back up shared applications (data, configuration and secrets)
    Backup(server::ServerBackupArgs),
    /// Restore shared applications from a server backup
    Restore(server::ServerRestoreArgs),
}

#[derive(Debug, Subcommand)]
pub enum BackupCommand {
    /// List the archives in the server's backup folder
    List {
        /// Where backups are (default: `backup.target` of depap-server.yaml)
        #[arg(long, value_enum)]
        source: Option<BackupTarget>,
    },
}

#[derive(Debug, Subcommand)]
pub enum ConnectorCommand {
    /// Show shared apps and the applications linked to them
    List,
}

/// Arguments shared by `server deploy` and `app deploy`.
#[derive(Debug, Args)]
pub struct DeployArgs {
    /// Also start or restart services (default: `deploy.apply`, see `depap config`)
    #[arg(long, conflicts_with = "no_apply")]
    pub apply: bool,
    /// Only upload files, even when `deploy.apply` is set
    #[arg(long)]
    pub no_apply: bool,
    /// Config files merged in order (repeatable or comma-separated)
    #[arg(short = 'f', long = "file", value_delimiter = ',')]
    pub files: Vec<PathBuf>,
    /// Validate and show what would happen without changing the server
    #[arg(long)]
    pub dry_run: bool,
    /// Remove a lock left behind by an interrupted depap command
    #[arg(long)]
    pub force_unlock: bool,
}

impl DeployArgs {
    /// `--apply`/`--no-apply`, else the configured default.
    pub fn apply(&self, home: &DepapHome, server: &ServerEntry) -> Result<bool> {
        if self.apply || self.no_apply {
            return Ok(self.apply);
        }
        home.apply_default(server)
    }
}

/// Where and how a backup is written.
#[derive(Debug, Args)]
pub struct BackupOutputArgs {
    /// `depap` (restorable archive) or `raw` (plain files: one as is, several as tar.gz)
    #[arg(long, value_enum, default_value_t = Format::Depap)]
    pub format: Format,
    /// Output file or folder (with `--target server`: a file name in the server's backup folder)
    #[arg(short, long)]
    pub output: Option<PathBuf>,
    /// Where to write (default: `backup.target` of depap-server.yaml, else local)
    #[arg(long, value_enum)]
    pub target: Option<BackupTarget>,
}

pub fn run(cli: Cli) -> Result<()> {
    let home = DepapHome::locate()?;
    let server = cli.server.as_deref();
    match cli.command {
        Command::Init(args) => init::run(&home, args),
        Command::Server(ServerCommand::Generate(args)) => generate::server(args),
        Command::Server(ServerCommand::Define(args)) => server::define(&home, args),
        Command::Server(ServerCommand::List) => server::list(&home),
        Command::Server(ServerCommand::State) => server::state(&home, server, cli.verbose),
        Command::Server(ServerCommand::Deploy(args)) => {
            server::deploy(&home, server, args, cli.verbose)
        }
        Command::Secret(command) => secret::run(&home, server, command, cli.verbose),
        Command::Config(command) => config::run(&home, command),
        Command::Connect { name } => context::connect(&home, &name),
        Command::Disconnect => context::disconnect(&home),
        Command::App(command) => app::run(&home, server, command, cli.verbose),
        Command::Connector(ConnectorCommand::List) => {
            server::connector_list(&home, server, cli.verbose)
        }
        Command::Server(ServerCommand::Backup(args)) => {
            server::backup(&home, server, args, cli.verbose)
        }
        Command::Server(ServerCommand::Restore(args)) => {
            server::restore(&home, server, args, cli.verbose)
        }
        Command::Backup(BackupCommand::List { source }) => {
            server::backup_list(&home, server, source, cli.verbose)
        }
    }
}
