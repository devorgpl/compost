//! `depap app …` commands (spec §10.8–§10.10).

use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Subcommand};
use serde::Deserialize;

use super::DeployArgs;
use crate::app::{deploy, remove, state};
use crate::config::app::{AppConfig, default_prefix};
use crate::config::local::{DepapHome, ServerEntry};
use crate::config::merge;
use crate::connector;
use crate::error::config_err;

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
pub struct InstanceArgs {
    /// Config files merged in order (repeatable or comma-separated)
    #[arg(
        short = 'f',
        long = "file",
        value_delimiter = ',',
        conflicts_with = "app"
    )]
    pub files: Vec<PathBuf>,
    /// Application name
    #[arg(long, requires = "prefix")]
    pub app: Option<String>,
    /// Instance prefix
    #[arg(long, requires = "app")]
    pub prefix: Option<String>,
}

#[derive(Debug, Args)]
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
    /// Remove a lock left behind by an interrupted depap command
    #[arg(long)]
    pub force_unlock: bool,
}

/// Just enough of the app config to identify an instance.
#[derive(Deserialize)]
struct InstanceId {
    name: String,
    #[serde(default = "default_prefix")]
    prefix: String,
}

pub fn run(
    home: &DepapHome,
    server: Option<&str>,
    command: AppCommand,
    verbose: bool,
) -> Result<()> {
    let manager = home.require_manager()?;
    let server = home.resolve_server(server)?;
    match command {
        AppCommand::Deploy(args) => deploy(home, &manager.manager_key, &server, args, verbose),
        AppCommand::State(args) => {
            let (name, prefix) = instance_id(&args)?;
            let connector = connector::open(&server, home, verbose)?;
            state::state(
                connector.as_ref(),
                server.server_home(),
                &manager.manager_key,
                &name,
                &prefix,
            )
        }
        AppCommand::Remove(args) => {
            let (name, prefix) = instance_id(&args.instance)?;
            let connector = connector::open(&server, home, verbose)?;
            let options = remove::Options {
                volumes: args.volumes,
                purge: args.purge,
                yes: args.yes,
                force_unlock: args.force_unlock,
            };
            remove::remove(
                connector.as_ref(),
                server.server_home(),
                &manager.manager_key,
                &name,
                &prefix,
                &options,
            )
        }
    }
}

/// Name and prefix from `--app`/`--prefix`, or from the config files.
fn instance_id(args: &InstanceArgs) -> Result<(String, String)> {
    if let (Some(app), Some(prefix)) = (&args.app, &args.prefix) {
        return Ok((app.clone(), prefix.clone()));
    }
    let files = config_files(&args.files)?;
    let merged = merge::load_merged(&files)?;
    let id: InstanceId = serde_yaml_ng::from_value(merged)
        .map_err(|e| config_err(format!("app configuration: {e}")))?;
    Ok((id.name, id.prefix))
}

/// The given files, or `depap.yaml` in the current directory.
fn config_files(files: &[PathBuf]) -> Result<Vec<PathBuf>> {
    if !files.is_empty() {
        return Ok(files.to_vec());
    }
    let default = PathBuf::from(APP_FILE);
    if !default.exists() {
        return Err(config_err(format!(
            "{APP_FILE} not found in the current directory (pass files with -f, or --app and --prefix)"
        )));
    }
    Ok(vec![default])
}

fn deploy(
    home: &DepapHome,
    manager_key: &str,
    server: &ServerEntry,
    args: DeployArgs,
    verbose: bool,
) -> Result<()> {
    let files = config_files(&args.files)?;
    let loaded = AppConfig::load(&files)?;

    let connector = connector::open(server, home, verbose)?;
    out!(
        "Deploying {} to {} ({})",
        loaded.config.instance(),
        server.name,
        connector.target()
    );
    let options = deploy::Options {
        apply: args.apply,
        dry_run: args.dry_run,
        force_unlock: args.force_unlock,
    };
    deploy::deploy(
        connector.as_ref(),
        server.server_home(),
        manager_key,
        &loaded,
        &files,
        &options,
    )
}
