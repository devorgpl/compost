//! `compost app …` commands (spec §10.8–§10.10, §10.12).

use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Subcommand};
use serde::Deserialize;

use super::{BackupOutputArgs, DeployArgs};
use crate::app::backup::{self, Component, Which};
use crate::app::instance::Selector;
use crate::app::{deploy, list, remove, restore, secrets, state};
use crate::config::app::{AppConfig, default_prefix};
use crate::config::local::{CompostHome, ServerEntry};
use crate::config::server::BackupTarget;
use crate::config::{check_name, merge};
use crate::connector;
use crate::error::config_err;

const APP_FILE: &str = "compost.yaml";

#[derive(Debug, Subcommand)]
pub enum AppCommand {
    /// Write a starter compost.yaml and .compost.secrets.example.yaml from the compose file
    Generate(super::generate::AppGenerateArgs),
    /// List the application instances deployed on the server
    List,
    /// Deploy the application instance described by the given files
    Deploy(AppDeployArgs),
    /// Show the state of an application instance
    State(InstanceArgs),
    /// Remove an application instance
    Remove(RemoveArgs),
    /// Back up an application instance (or all of them)
    Backup(AppBackupArgs),
    /// Restore an application instance from a compost backup
    Restore(AppRestoreArgs),
}

/// Selects an instance from config files, by name and prefix, or as `app list` shows it.
#[derive(Debug, Args)]
pub struct InstanceArgs {
    /// Instance as shown by `app list`, e.g. `myapp-staging`
    #[arg(conflicts_with_all = ["files", "app", "prefix"])]
    pub instance: Option<String>,
    /// Config files merged in order (repeatable or comma-separated)
    #[arg(
        short = 'f',
        long = "file",
        value_delimiter = ',',
        conflicts_with = "app"
    )]
    pub files: Vec<PathBuf>,
    /// Application name (instead of config files)
    #[arg(long, requires = "prefix")]
    pub app: Option<String>,
    /// Instance prefix (overrides the one from the config files)
    #[arg(long)]
    pub prefix: Option<String>,
}

#[derive(Debug, Args)]
pub struct AppDeployArgs {
    #[command(flatten)]
    pub deploy: DeployArgs,
    /// Instance prefix (overrides the one from the config files)
    #[arg(long)]
    pub prefix: Option<String>,
    /// Also delete the data of links dropped from the config, for these kinds, e.g. `--purge database`
    #[arg(long, value_delimiter = ',')]
    pub purge: Vec<String>,
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
    /// Remove a lock left behind by an interrupted compost command
    #[arg(long)]
    pub force_unlock: bool,
}

#[derive(Debug, Args)]
pub struct AppBackupArgs {
    #[command(flatten)]
    pub instance: InstanceArgs,
    /// Back up every instance on the server into one archive
    #[arg(long, conflicts_with_all = ["instance", "files", "app", "prefix"])]
    pub all: bool,
    /// Parts to save (default: links,sources,volumes; only links with --link)
    #[arg(long, value_enum, value_delimiter = ',')]
    pub include: Vec<Component>,
    /// Only this link: `<entry>` or `<kind>:<entry>` (repeatable)
    #[arg(long)]
    pub link: Vec<String>,
    #[command(flatten)]
    pub output: BackupOutputArgs,
    /// Stop the services while volumes are copied, for a consistent copy
    #[arg(long)]
    pub stop: bool,
    /// Remove a lock left behind by an interrupted compost command
    #[arg(long)]
    pub force_unlock: bool,
}

#[derive(Debug, Args)]
pub struct AppRestoreArgs {
    /// Archive written by `compost app backup` (with `--source server`: a file in the server's backup folder)
    pub archive: String,
    /// Instance in the archive, e.g. `myapp-staging` (needed when it has several)
    #[arg(long, conflicts_with = "all")]
    pub instance: Option<String>,
    /// Restore every instance in the archive
    #[arg(long)]
    pub all: bool,
    /// Restore under this prefix, e.g. to make a copy
    #[arg(long)]
    pub prefix: Option<String>,
    /// Overlays merged over the archived config, e.g. other domains (repeatable or comma-separated)
    #[arg(short = 'f', long = "file", value_delimiter = ',')]
    pub files: Vec<PathBuf>,
    /// Parts to restore (default: all in the archive; only links with --link).
    /// Without `sources`, data is loaded into the deployed instance.
    #[arg(long, value_enum, value_delimiter = ',')]
    pub include: Vec<Component>,
    /// Only this link: `<entry>` or `<kind>:<entry>` (repeatable)
    #[arg(long)]
    pub link: Vec<String>,
    /// Where the archive is (default: `backup.target` of compost-server.yaml, else local)
    #[arg(long, value_enum)]
    pub source: Option<BackupTarget>,
    /// Overwrite databases and volumes that already hold data
    #[arg(long)]
    pub replace: bool,
    /// Do not ask for confirmation
    #[arg(long)]
    pub yes: bool,
    /// Remove a lock left behind by an interrupted compost command
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
    home: &CompostHome,
    server: Option<&str>,
    command: AppCommand,
    verbose: bool,
) -> Result<()> {
    // Works without `compost init` or a server.
    if let AppCommand::Generate(args) = command {
        return super::generate::app(args);
    }
    let manager = home.require_manager()?;
    let server = home.resolve_server(server)?;
    match command {
        AppCommand::Generate(_) => unreachable!("handled above"),
        AppCommand::List => {
            let connector = connector::open(&server, home, verbose)?;
            list::list(
                connector.as_ref(),
                server.server_home(),
                &manager.manager_key,
            )
        }
        AppCommand::Deploy(args) => deploy(home, &manager.manager_key, &server, args, verbose),
        AppCommand::State(args) => {
            let selector = selector(&args)?;
            let connector = connector::open(&server, home, verbose)?;
            state::state(
                connector.as_ref(),
                server.server_home(),
                &manager.manager_key,
                &selector,
            )
        }
        AppCommand::Remove(args) => {
            let selector = selector(&args.instance)?;
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
                &selector,
                &options,
            )
        }
        AppCommand::Backup(args) => {
            let which = if args.all {
                Which::All
            } else {
                Which::One(selector(&args.instance)?)
            };
            let options = backup::Options {
                include: args.include,
                links: args.link,
                format: args.output.format,
                target: args.output.target,
                output: args.output.output,
                stop: args.stop,
                force_unlock: args.force_unlock,
            };
            let connector = connector::open(&server, home, verbose)?;
            backup::backup(
                connector.as_ref(),
                &server.name,
                server.server_home(),
                &manager.manager_key,
                &which,
                &options,
            )
        }
        AppCommand::Restore(args) => {
            if let Some(prefix) = &args.prefix {
                check_name("prefix", prefix)?;
            }
            let options = restore::Options {
                archive: args.archive,
                source: args.source,
                instance: args.instance,
                all: args.all,
                prefix: args.prefix,
                files: args.files,
                include: args.include,
                links: args.link,
                replace: args.replace,
                yes: args.yes,
                force_unlock: args.force_unlock,
            };
            let connector = connector::open(&server, home, verbose)?;
            restore::restore(
                connector.as_ref(),
                server.server_home(),
                &manager.manager_key,
                &options,
            )
        }
    }
}

/// The positional instance, name and prefix from `--app`/`--prefix`, or the
/// config files (with `--prefix` replacing theirs).
fn selector(args: &InstanceArgs) -> Result<Selector> {
    if let Some(instance) = &args.instance {
        return Ok(Selector::Instance(instance.clone()));
    }
    let (name, prefix) = match (&args.app, &args.prefix) {
        (Some(app), Some(prefix)) => {
            check_name("app name", app)?;
            check_name("prefix", prefix)?;
            (app.clone(), prefix.clone())
        }
        _ => instance_id(&config_files(&args.files)?, args.prefix.as_deref())?,
    };
    Ok(Selector::Name { name, prefix })
}

/// App name and prefix from the config files; `prefix` replaces theirs.
fn instance_id(files: &[PathBuf], prefix: Option<&str>) -> Result<(String, String)> {
    let merged = merge::load_merged(files)?;
    let id: InstanceId = serde_yaml_ng::from_value(merged)
        .map_err(|e| config_err(format!("app configuration: {e}")))?;
    let prefix = prefix.map_or(id.prefix, str::to_string);
    check_name("app name", &id.name)?;
    check_name("prefix", &prefix)?;
    Ok((id.name, prefix))
}

/// The given files, or `compost.yaml` in the current directory.
fn config_files(files: &[PathBuf]) -> Result<Vec<PathBuf>> {
    if !files.is_empty() {
        return Ok(files.to_vec());
    }
    let default = PathBuf::from(APP_FILE);
    if !default.exists() {
        return Err(config_err(format!(
            "{APP_FILE} not found in the current directory (pass files with -f, an instance from `compost app list`, or --app and --prefix)"
        )));
    }
    Ok(vec![default])
}

fn deploy(
    home: &CompostHome,
    manager_key: &str,
    server: &ServerEntry,
    args: AppDeployArgs,
    verbose: bool,
) -> Result<()> {
    let AppDeployArgs {
        deploy: args,
        prefix,
        purge,
    } = args;
    let files = config_files(&args.files)?;
    let (name, instance_prefix) = instance_id(&files, prefix.as_deref())?;
    let connector = connector::open(server, home, verbose)?;
    let stored = secrets::read(
        connector.as_ref(),
        server.server_home(),
        &name,
        &instance_prefix,
    )?;
    let (stored_path, stored) = stored.unzip();
    let loaded = AppConfig::load_with(&files, stored, prefix.as_deref())?;

    out!(
        "Deploying {} to {} ({})",
        loaded.config.instance(),
        server.name,
        connector.target()
    );
    if let Some(path) = stored_path {
        out!("  using stored secrets {path}");
    }
    let options = deploy::Options {
        apply: args.apply(home, server)?,
        dry_run: args.dry_run,
        force_unlock: args.force_unlock,
        restore: None,
        purge,
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
