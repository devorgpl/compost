//! `depap app …` commands (spec §10.8–§10.10).

use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Subcommand};

use super::DeployArgs;
use crate::app::deploy::{self, Options};
use crate::config::app::AppConfig;
use crate::config::local::DepapHome;
use crate::connector;
use crate::error::{DepapError, config_err};

const APP_FILE: &str = "depap.yaml";

#[derive(Debug, Subcommand)]
pub enum AppCommand {
    /// Deploy the application instance described by the given files
    Deploy(DeployArgs),
    /// Show the state of an application instance
    State(InstanceArgs),
    /// Remove an application instance
    Remove(RemoveArgs),
}

/// Selects an instance either from config files or by name and prefix.
#[derive(Debug, Args)]
#[allow(dead_code)] // read once `app state` and `app remove` are implemented
pub struct InstanceArgs {
    /// Config files merged in order (repeatable or comma-separated)
    #[arg(short = 'f', long = "file", value_delimiter = ',', conflicts_with = "app")]
    pub files: Vec<PathBuf>,
    /// Application name
    #[arg(long, requires = "prefix")]
    pub app: Option<String>,
    /// Instance prefix
    #[arg(long, requires = "app")]
    pub prefix: Option<String>,
}

#[derive(Debug, Args)]
#[allow(dead_code)] // read once `app remove` is implemented
pub struct RemoveArgs {
    #[command(flatten)]
    pub instance: InstanceArgs,
    /// Also remove the compose project's volumes
    #[arg(long)]
    pub volumes: bool,
    /// Delete the instance's data held by these kinds, e.g. `--purge database`
    #[arg(long, value_delimiter = ',')]
    pub purge: Vec<String>,
    /// Do not ask for confirmation
    #[arg(long)]
    pub yes: bool,
}

pub fn run(home: &DepapHome, server: Option<&str>, command: AppCommand, verbose: bool) -> Result<()> {
    match command {
        AppCommand::Deploy(args) => deploy(home, server, args, verbose),
        AppCommand::State(_) => {
            home.resolve_server(server)?;
            Err(DepapError::NotImplemented("app state").into())
        }
        AppCommand::Remove(_) => {
            home.resolve_server(server)?;
            Err(DepapError::NotImplemented("app remove").into())
        }
    }
}

fn deploy(home: &DepapHome, server: Option<&str>, args: DeployArgs, verbose: bool) -> Result<()> {
    let manager = home.require_manager()?;
    let server = home.resolve_server(server)?;
    let files = if args.files.is_empty() {
        let default = PathBuf::from(APP_FILE);
        if !default.exists() {
            return Err(config_err(format!(
                "{APP_FILE} not found in the current directory (or pass files with -f)"
            )));
        }
        vec![default]
    } else {
        args.files
    };
    let loaded = AppConfig::load(&files)?;

    let connector = connector::open(&server, verbose);
    println!(
        "Deploying {} to {} ({})",
        loaded.config.instance(),
        server.name,
        connector.target()
    );
    let options = Options {
        apply: args.apply,
        dry_run: args.dry_run,
        force_unlock: args.force_unlock,
    };
    deploy::deploy(
        connector.as_ref(),
        server.server_home(),
        &manager.manager_key,
        &loaded,
        &files,
        &options,
    )
}
