//! `depap app …` commands (spec §10.8–§10.10). Arguments are defined so `--help`
//! is complete; the commands themselves are not implemented yet.

// Fields are read once the commands are implemented.
#![allow(dead_code)]

use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Subcommand};

use super::DeployArgs;
use crate::config::local::DepapHome;
use crate::error::DepapError;

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

pub fn run(home: &DepapHome, server: Option<&str>, command: AppCommand) -> Result<()> {
    home.resolve_server(server)?;
    let name = match command {
        AppCommand::Deploy(_) => "app deploy",
        AppCommand::State(_) => "app state",
        AppCommand::Remove(_) => "app remove",
    };
    Err(DepapError::NotImplemented(name).into())
}
