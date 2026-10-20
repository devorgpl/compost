//! Command-line interface: argument definitions and dispatch to command modules.

mod agent;
mod app;
mod config;
mod context;
mod dev;
mod generate;
mod init;
mod secret;
mod server;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

use crate::backup::Format;
use crate::backup::profile::Flags;
use crate::config::local::{CompostHome, ServerEntry};
use crate::connector::ConnectOptions;
use crate::error::config_err;

#[derive(Debug, Parser)]
#[command(
    name = "compost",
    version,
    about = "Manage docker-based server deployments"
)]
pub struct Cli {
    /// Target server (default: `COMPOST_SERVER`, then the current context)
    #[arg(short, long, global = true, env = "COMPOST_SERVER")]
    pub server: Option<String>,

    /// Print every command run on the server
    #[arg(short, long, global = true)]
    pub verbose: bool,

    /// SSH private key for this command, instead of the server's `identity_file`
    #[arg(long, global = true, env = "COMPOST_SSH_KEY_FILE", value_name = "PATH")]
    pub ssh_key_file: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create the local compost home with a new (or the given) manager key
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
    /// Run an app with the shared apps it needs on this machine, for development
    #[command(subcommand)]
    Dev(dev::DevCommand),
    /// Inspect links between applications and shared apps
    #[command(subcommand)]
    Connector(ConnectorCommand),
    /// Run backup profiles, list and prune backups
    #[command(subcommand)]
    Backup(BackupCommand),
    /// Send notifications through the server's communication apps
    #[command(subcommand)]
    Notify(NotifyCommand),
    /// Store application secrets on the server for `app deploy`
    #[command(subcommand)]
    Secret(secret::SecretCommand),
    /// Set and show preferences, e.g. `deploy.apply`
    #[command(subcommand)]
    Config(config::ConfigCommand),
    /// Documentation for AI agents: llms.txt, llms-full.txt and an Agent Skill
    #[command(subcommand)]
    Agent(agent::AgentCommand),
}

#[derive(Debug, Subcommand)]
pub enum ServerCommand {
    /// Write starter compost-server.yaml, compost-server.prod.yaml and a secrets example
    Generate(generate::ServerGenerateArgs),
    /// Add or replace a server in the local configuration
    Define(server::DefineArgs),
    /// List defined servers
    List,
    /// Check and display server state
    State,
    /// Deploy shared applications from compost-server.yaml
    Deploy(server::ServerDeployArgs),
    /// Back up shared applications (data, configuration and secrets)
    Backup(server::ServerBackupArgs),
    /// Restore shared applications from a server backup
    Restore(server::ServerRestoreArgs),
    /// Show the output of a shared application's containers
    Logs(server::ServerLogsArgs),
    /// Run a command in a shared application's container, e.g. a shell
    Exec(server::ServerExecArgs),
    /// List the commands of compost-server.yaml, or run one by name in its shared app's container
    Cmd(server::ServerCmdArgs),
    /// Show disk usage, then remove images no container uses (except those kept for
    /// `app rollback`), dangling images and the build cache
    Prune(server::ServerPruneArgs),
}

#[derive(Debug, Subcommand)]
pub enum BackupCommand {
    /// Back up with a profile: the shared apps (when compost-server.yaml defines the
    /// profile) and every instance it applies to, each into its own archive
    Run {
        /// Profile from `backup.profiles` of compost-server.yaml or compost.yaml
        profile: String,
        /// Remove a lock left behind by an interrupted compost command
        #[arg(long)]
        force_unlock: bool,
    },
    /// Run the scheduled backups that are due; cron runs it on the server
    #[command(hide = true)]
    Tick {
        /// The server's compost folder
        #[arg(long)]
        server_home: String,
        /// The server's name, for archive names
        #[arg(long)]
        server_name: String,
    },
    /// Delete the archives of backup profiles beyond the number their `keep` sets
    Prune {
        /// Only this profile's archives (default: every profile)
        #[arg(long)]
        profile: Option<String>,
        /// Look at this target instead of each profile's: `local` (the current folder),
        /// `server` or a name from `backup.targets`
        #[arg(long, value_name = "TARGET")]
        target: Option<String>,
        /// Keep this many archives of each instance and profile, instead of the profile's `keep`
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
        keep: Option<u32>,
        /// Do not ask for confirmation
        #[arg(long)]
        yes: bool,
    },
    /// List the archives in the server's backup folder
    List {
        /// Where backups are: `server` or a name from `backup.targets` (default: `backup.target` of compost-server.yaml)
        #[arg(long, value_name = "TARGET")]
        source: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum NotifyCommand {
    /// Send a test message to every communication app, or to one
    Test {
        /// Only this communication app
        #[arg(long, value_name = "NAME")]
        to: Option<String>,
        /// Text of the message
        #[arg(long)]
        message: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum ConnectorCommand {
    /// Show shared apps and the applications linked to them
    List,
}

/// Arguments shared by `app logs` and `server logs`.
#[derive(Debug, Args)]
pub struct LogsArgs {
    /// Keep streaming new output until Ctrl-C
    #[arg(long)]
    pub follow: bool,
    /// Lines to show from the end of each container's output, or `all`
    #[arg(long, default_value = "100")]
    pub tail: String,
    /// Only output since this time: a duration like `10m` or a time like `2026-10-17T12:00:00`
    #[arg(long)]
    pub since: Option<String>,
    /// Show the time of every line
    #[arg(long)]
    pub timestamps: bool,
}

impl LogsArgs {
    pub fn options(self, services: Vec<String>) -> crate::app::logs::Options {
        crate::app::logs::Options {
            services,
            follow: self.follow,
            tail: self.tail,
            since: self.since,
            timestamps: self.timestamps,
        }
    }
}

/// Arguments shared by `server deploy` and `app deploy`.
#[derive(Debug, Args)]
pub struct DeployArgs {
    /// Also start or restart services (default: `deploy.apply`, see `compost config`)
    #[arg(long, conflicts_with = "no_apply")]
    pub apply: bool,
    /// Only upload files, even when `deploy.apply` is set
    #[arg(long)]
    pub no_apply: bool,
    /// Config files merged in order (repeatable or comma-separated)
    #[arg(short = 'f', long = "file", value_delimiter = ',')]
    pub files: Vec<PathBuf>,
    /// Validate and show what would change on the server, without changing it
    #[arg(long)]
    pub dry_run: bool,
    /// With --dry-run: also show the changed lines of files without secrets
    #[arg(long, requires = "dry_run")]
    pub diff: bool,
    /// Remove a lock left behind by an interrupted compost command
    #[arg(long)]
    pub force_unlock: bool,
    /// Pull images again, even when their tags did not change, and build images that compost builds on newer base images (needs `--apply`)
    #[arg(long)]
    pub pull: bool,
    /// Recreate every container, even unchanged ones; data in volumes is kept (needs `--apply`)
    #[arg(long)]
    pub recreate: bool,
}

impl DeployArgs {
    /// `--apply`/`--no-apply`, else the configured default.
    pub fn apply(&self, home: &CompostHome, server: &ServerEntry) -> Result<bool> {
        let apply = if self.apply || self.no_apply {
            self.apply
        } else {
            home.apply_default(server)?
        };
        if !apply && (self.pull || self.recreate) {
            return Err(config_err(
                "--pull and --recreate start services: add --apply",
            ));
        }
        Ok(apply)
    }
}

/// Where and how a backup is written.
#[derive(Debug, Args)]
pub struct BackupOutputArgs {
    /// Backup profile from `backup.profiles`: what to save, where, and how many archives to keep.
    /// The other options override it
    #[arg(long, value_name = "NAME")]
    pub profile: Option<String>,
    /// `compost` (restorable archive, the default) or `raw` (plain files: one as is, several as tar.gz)
    #[arg(long, value_enum)]
    pub format: Option<Format>,
    /// Output file or folder (with a target on the server: a file name in its folder)
    #[arg(short, long)]
    pub output: Option<PathBuf>,
    /// Where to write: `local`, `server` or a name from `backup.targets`
    /// (default: the profile's, the app's `backup.target`, then the server's, else local)
    #[arg(long, value_name = "TARGET")]
    pub target: Option<String>,
}

impl BackupOutputArgs {
    /// The flags that override a profile; `include` and `links` are `app backup`'s.
    pub fn flags(
        &self,
        include: Vec<crate::backup::Component>,
        links: Vec<String>,
    ) -> Result<Flags> {
        if let Some(profile) = &self.profile {
            crate::config::check_name("backup profile", profile)?;
        }
        Ok(Flags {
            profile: self.profile.clone(),
            include,
            links,
            format: self.format,
            target: self.target.clone(),
        })
    }
}

pub fn run(cli: Cli) -> Result<()> {
    let home = CompostHome::locate()?;
    let server = cli.server.as_deref();
    let connect = ConnectOptions {
        verbose: cli.verbose,
        ssh_key_file: cli.ssh_key_file,
    };
    match cli.command {
        Command::Init(args) => init::run(&home, args),
        Command::Server(ServerCommand::Generate(args)) => generate::server(args),
        Command::Server(ServerCommand::Define(args)) => server::define(&home, args),
        Command::Server(ServerCommand::List) => server::list(&home),
        Command::Server(ServerCommand::State) => server::state(&home, server, &connect),
        Command::Server(ServerCommand::Deploy(args)) => {
            server::deploy(&home, server, args, &connect)
        }
        Command::Server(ServerCommand::Logs(args)) => server::logs(&home, server, args, &connect),
        Command::Server(ServerCommand::Exec(args)) => server::exec(&home, server, args, &connect),
        Command::Server(ServerCommand::Cmd(args)) => server::cmd(&home, server, args, &connect),
        Command::Server(ServerCommand::Prune(args)) => server::prune(&home, server, args, &connect),
        Command::Secret(command) => secret::run(&home, server, command, &connect),
        Command::Config(command) => config::run(&home, command),
        Command::Agent(command) => agent::run(command),
        Command::Connect { name } => context::connect(&home, &name),
        Command::Disconnect => context::disconnect(&home),
        Command::App(command) => app::run(&home, server, command, &connect),
        Command::Dev(command) => dev::run(&home, server, command, &connect),
        Command::Connector(ConnectorCommand::List) => {
            server::connector_list(&home, server, &connect)
        }
        Command::Server(ServerCommand::Backup(args)) => {
            server::backup(&home, server, args, &connect)
        }
        Command::Server(ServerCommand::Restore(args)) => {
            server::restore(&home, server, args, &connect)
        }
        Command::Backup(BackupCommand::Run {
            profile,
            force_unlock,
        }) => server::backup_run(&home, server, &profile, force_unlock, &connect),
        Command::Backup(BackupCommand::Tick {
            server_home,
            server_name,
        }) => server::backup_tick(&server_home, &server_name),
        Command::Backup(BackupCommand::Prune {
            profile,
            target,
            keep,
            yes,
        }) => {
            let options = crate::server::backup::PruneOptions {
                profile,
                target,
                keep,
                yes,
            };
            server::backup_prune(&home, server, &options, &connect)
        }
        Command::Notify(NotifyCommand::Test { to, message }) => {
            server::notify_test(&home, server, to.as_deref(), message.as_deref(), &connect)
        }
        Command::Backup(BackupCommand::List { source }) => {
            server::backup_list(&home, server, source.as_deref(), &connect)
        }
    }
}
