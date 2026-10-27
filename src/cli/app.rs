//! `compost app …` commands (spec §10.8–§10.10, §10.12).

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use serde::Deserialize;

use super::{BackupOutputArgs, DeployArgs};
use crate::app::backup::{self, Component, Which};
use crate::app::instance::Selector;
use crate::app::{deploy, exec, list, remove, restore, rollback, secrets, state};
use crate::config::app::AppConfig;
use crate::config::local::{CompostHome, ServerEntry};
use crate::config::{check_name, merge};
use crate::connector::{self, ConnectOptions};
use crate::error::config_err;
use crate::server::schedule;

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
    /// Show the output of an application instance's containers
    Logs(AppLogsArgs),
    /// Run a command in a service's running container, e.g. a shell
    Exec(AppExecArgs),
    /// Run a command in a new one-off container of a service, e.g. a migration
    Run(AppRunArgs),
    /// List the commands of the instance's compost.yaml, or run one by name
    Cmd(AppCmdArgs),
    /// Remove an application instance
    Remove(RemoveArgs),
    /// Back up an application instance (or all of them)
    Backup(AppBackupArgs),
    /// Restore an application instance from a compost backup
    Restore(AppRestoreArgs),
    /// List the releases of an application instance kept for rollback
    Releases(InstanceArgs),
    /// Deploy an earlier release of an application instance again, with the images it ran
    Rollback(AppRollbackArgs),
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
    /// Instance prefix (overrides the one from the config files; default: the `prefix` setting, else `main`)
    #[arg(long)]
    pub prefix: Option<String>,
}

#[derive(Debug, Args)]
pub struct AppDeployArgs {
    #[command(flatten)]
    pub deploy: DeployArgs,
    /// Instance prefix (overrides the one from the config files; default: the `prefix` setting, else `main`)
    #[arg(long)]
    pub prefix: Option<String>,
    /// Also delete the data of links dropped from the config, for these kinds, e.g. `--purge database`
    #[arg(long, value_delimiter = ',')]
    pub purge: Vec<String>,
    /// Overlay with secrets (e.g. .compost.secrets.yaml) to store on the server and
    /// deploy with, replacing the stored one; later deploys use it without the file
    #[arg(long, value_name = "FILE")]
    pub secret: Option<PathBuf>,
    /// With --apply, back up the deployed instance with this profile first
    /// (default: `deployment.backup`); a failed backup stops the deploy
    #[arg(long, value_name = "PROFILE", conflicts_with = "no_backup")]
    pub backup: Option<String>,
    /// Skip the backup that `deployment.backup` takes before the deploy
    #[arg(long)]
    pub no_backup: bool,
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
    /// Back up every instance on the server: into one archive, or with --profile one archive each
    #[arg(long, conflicts_with_all = ["instance", "files", "app", "prefix"])]
    pub all: bool,
    /// Parts to save (default: the profile's, else links,sources,volumes; only links with --link)
    #[arg(long, value_enum, value_delimiter = ',')]
    pub include: Vec<Component>,
    /// Only this link: `<entry>` or `<kind>:<entry>` (repeatable)
    #[arg(long)]
    pub link: Vec<String>,
    #[command(flatten)]
    pub output: BackupOutputArgs,
    /// Stop every service while volumes are copied, for a consistent copy (default: those in `backup.stop`)
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
    /// Where the archive is: `local`, `server` or a name from `backup.targets`
    /// (default: `backup.target` of compost-server.yaml, else local)
    #[arg(long, value_name = "TARGET")]
    pub source: Option<String>,
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

#[derive(Debug, Args)]
pub struct AppLogsArgs {
    #[command(flatten)]
    pub instance: InstanceArgs,
    /// Only this compose service (repeatable or comma-separated; default: all)
    #[arg(long, value_delimiter = ',')]
    pub service: Vec<String>,
    #[command(flatten)]
    pub logs: super::LogsArgs,
}

#[derive(Debug, Args)]
pub struct AppCmdArgs {
    /// Instance as shown by `app list`, then the command's name and its arguments;
    /// with -f, --app or --prefix, the instance is left out. Without a command, lists them;
    /// `--history` in place of the command shows the last ones run.
    /// Options of the command (`--<arg> <value>`, `--yes`, `--dry-run`, `--help`)
    /// come after its name
    #[arg(
        value_name = "INSTANCE] [COMMAND] [ARGS",
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    pub words: Vec<String>,
    /// Config files merged in order, to select the instance (repeatable or comma-separated);
    /// the commands are those of the deployed instance
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
    /// Instance prefix (overrides the one from the config files; default: the `prefix` setting, else `main`)
    #[arg(long)]
    pub prefix: Option<String>,
}

#[derive(Debug, Args)]
pub struct AppExecArgs {
    #[command(flatten)]
    pub instance: InstanceArgs,
    /// Compose service whose container runs the command
    #[arg(long, required = true)]
    pub service: String,
    /// The command and its arguments, after `--`, e.g. `-- sh` or `-- ls -la /data`
    #[arg(last = true, required = true, value_name = "COMMAND")]
    pub command: Vec<String>,
}

#[derive(Debug, Args)]
pub struct AppRunArgs {
    #[command(flatten)]
    pub exec: AppExecArgs,
    /// Don't start the services it depends on (`depends_on`)
    #[arg(long)]
    pub no_deps: bool,
}

#[derive(Debug, Args)]
pub struct AppRollbackArgs {
    #[command(flatten)]
    pub instance: InstanceArgs,
    /// Release to deploy, as `app releases` shows it (default: the one before the current release)
    #[arg(long, value_name = "RELEASE")]
    pub to: Option<u32>,
    /// Show what would change without changing anything
    #[arg(long)]
    pub dry_run: bool,
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
    prefix: Option<String>,
}

pub fn run(
    home: &CompostHome,
    server: Option<&str>,
    command: AppCommand,
    connect: &ConnectOptions,
) -> Result<()> {
    // Works without `compost init` or a server.
    if let AppCommand::Generate(args) = command {
        return super::generate::app(args);
    }
    let manager = home.require_manager()?;
    let server = home.resolve_server(server)?;
    let defaults = home.defaults(&server)?;
    match command {
        AppCommand::Generate(_) => unreachable!("handled above"),
        AppCommand::List => {
            let connector = connector::open(&server, home, connect)?;
            list::list(
                connector.as_ref(),
                server.server_home(),
                &manager.manager_key,
            )
        }
        AppCommand::Deploy(args) => deploy(
            home,
            &manager.manager_key,
            &server,
            args,
            DevDeploy::default(),
            connect,
        ),
        AppCommand::State(args) => {
            let selector = selector(&args, &defaults.prefix)?;
            let connector = connector::open(&server, home, connect)?;
            state::state(
                connector.as_ref(),
                server.server_home(),
                &manager.manager_key,
                &selector,
            )
        }
        AppCommand::Remove(args) => {
            let selector = selector(&args.instance, &defaults.prefix)?;
            let connector = connector::open(&server, home, connect)?;
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
            )?;
            schedule::refresh(
                connector.as_ref(),
                server.server_home(),
                &manager.manager_key,
                &server.name,
            )
        }
        AppCommand::Backup(args) => {
            let which = if args.all {
                Which::All
            } else {
                Which::One(selector(&args.instance, &defaults.prefix)?)
            };
            let options = backup::Options {
                flags: args.output.flags(args.include, args.link)?,
                output: args.output.output,
                stop: args.stop,
                force_unlock: args.force_unlock,
            };
            let connector = connector::open(&server, home, connect)?;
            backup::backup(
                connector.as_ref(),
                &server.name,
                server.server_home(),
                &manager.manager_key,
                &which,
                &options,
            )
            .map(drop)
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
            let connector = connector::open(&server, home, connect)?;
            restore::restore(
                connector.as_ref(),
                server.server_home(),
                &manager.manager_key,
                &options,
            )
        }
        AppCommand::Logs(args) => {
            let selector = selector(&args.instance, &defaults.prefix)?;
            let connector = connector::open(&server, home, connect)?;
            crate::app::logs::logs(
                connector.as_ref(),
                server.server_home(),
                &manager.manager_key,
                &selector,
                &args.logs.options(args.service, &defaults),
            )
        }
        AppCommand::Exec(args) => exec(
            home,
            &manager.manager_key,
            &server,
            args,
            exec::Mode::Exec,
            connect,
        ),
        AppCommand::Run(args) => {
            let mode = exec::Mode::Run {
                no_deps: args.no_deps,
            };
            exec(
                home,
                &manager.manager_key,
                &server,
                args.exec,
                mode,
                connect,
            )
        }
        AppCommand::Cmd(args) => {
            let mut words = args.words.into_iter();
            let by_name = !args.files.is_empty() || args.app.is_some() || args.prefix.is_some();
            let instance = if by_name { None } else { words.next() };
            let selector = selector(
                &InstanceArgs {
                    instance,
                    files: args.files,
                    app: args.app,
                    prefix: args.prefix,
                },
                &defaults.prefix,
            )?;
            let name = words.next();
            if let Some(name) = name.as_ref() {
                check_command_name(name)?;
            }
            let words: Vec<String> = words.collect();
            let connector = connector::open(&server, home, connect)?;
            exec::template(
                connector.as_ref(),
                server.server_home(),
                &manager.manager_key,
                &selector,
                name.as_deref(),
                &words,
            )
        }
        AppCommand::Releases(args) => {
            let selector = selector(&args, &defaults.prefix)?;
            let connector = connector::open(&server, home, connect)?;
            rollback::releases(
                connector.as_ref(),
                server.server_home(),
                &manager.manager_key,
                &selector,
            )
        }
        AppCommand::Rollback(args) => {
            let selector = selector(&args.instance, &defaults.prefix)?;
            let options = rollback::Options {
                to: args.to,
                dry_run: args.dry_run,
                yes: args.yes,
                force_unlock: args.force_unlock,
            };
            let connector = connector::open(&server, home, connect)?;
            rollback::rollback(
                connector.as_ref(),
                server.server_home(),
                &manager.manager_key,
                &selector,
                &options,
            )
        }
    }
}

fn exec(
    home: &CompostHome,
    manager_key: &str,
    server: &ServerEntry,
    args: AppExecArgs,
    mode: exec::Mode,
    connect: &ConnectOptions,
) -> Result<()> {
    let selector = selector(&args.instance, &home.defaults(server)?.prefix)?;
    let connector = connector::open(server, home, connect)?;
    let options = exec::Options {
        service: args.service,
        command: args.command,
        mode,
    };
    exec::exec(
        connector.as_ref(),
        server.server_home(),
        manager_key,
        &selector,
        &options,
    )
}

/// A command of `commands`, or `<entry>.<command>` of a link, checked by its parts.
pub(super) fn check_command_name(name: &str) -> Result<()> {
    if name == "--history" {
        return Ok(());
    }
    match name.split_once('.') {
        Some((entry, command)) => {
            let entry = entry.split_once('/').map_or(entry, |(_, entry)| entry);
            check_name("link name", entry)?;
            check_name("command name", command)
        }
        None => check_name("command name", name),
    }
}

/// The positional instance, name and prefix from `--app`/`--prefix`, or the
/// config files (with `--prefix` replacing theirs, and `fallback` used when
/// they have none).
fn selector(args: &InstanceArgs, fallback: &str) -> Result<Selector> {
    if let Some(instance) = &args.instance {
        return Ok(Selector::Instance(instance.clone()));
    }
    let (name, prefix) = match (&args.app, &args.prefix) {
        (Some(app), Some(prefix)) => {
            check_name("app name", app)?;
            check_name("prefix", prefix)?;
            (app.clone(), prefix.clone())
        }
        _ => instance_id(
            &config_files(&args.files)?,
            args.prefix.as_deref(),
            fallback,
        )?,
    };
    Ok(Selector::Name { name, prefix })
}

/// App name and prefix from the config files; `prefix` replaces theirs, and
/// `fallback` is used when they have none (the `prefix` setting, else `main`).
pub(super) fn instance_id(
    files: &[PathBuf],
    prefix: Option<&str>,
    fallback: &str,
) -> Result<(String, String)> {
    let merged = merge::load_merged(files)?;
    let id: InstanceId = serde_yaml_ng::from_value(merged)
        .map_err(|e| config_err(format!("app configuration: {e}")))?;
    let prefix = match prefix {
        Some(prefix) => prefix.to_string(),
        None => id.prefix.unwrap_or_else(|| fallback.to_string()),
    };
    check_name("app name", &id.name)?;
    check_name("prefix", &prefix)?;
    Ok((id.name, prefix))
}

/// The given files, or `compost.yaml` in the current directory.
pub(super) fn config_files(files: &[PathBuf]) -> Result<Vec<PathBuf>> {
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

/// What `compost dev` changes in a deploy.
#[derive(Default)]
pub(super) struct DevDeploy {
    /// Set up the links, but don't start the services (`dev up --host`).
    pub links_only: bool,
    /// Merged over the files, e.g. links pointed at this server's shared apps.
    pub overlay: Option<serde_yaml_ng::Value>,
}

pub(super) fn deploy(
    home: &CompostHome,
    manager_key: &str,
    server: &ServerEntry,
    args: AppDeployArgs,
    dev: DevDeploy,
    connect: &ConnectOptions,
) -> Result<()> {
    let AppDeployArgs {
        deploy: args,
        prefix,
        purge,
        secret,
        backup,
        no_backup,
    } = args;
    if let Some(profile) = &backup {
        check_name("backup profile", profile)?;
    }
    let defaults = home.defaults(server)?;
    let mut files = config_files(&args.files)?;
    // The name comes from the files; the prefix may come from --secret, which
    // takes the place of the stored overlay: right after the first file.
    let (name, mut instance_prefix) = instance_id(&files, prefix.as_deref(), &defaults.prefix)?;
    if secret.is_some() {
        let mut id_files = files.clone();
        id_files.splice(1..1, secret.clone());
        instance_prefix = instance_id(&id_files, prefix.as_deref(), &defaults.prefix)?.1;
    }
    let overlay = match &secret {
        Some(file) => {
            let text = fs::read_to_string(file)
                .with_context(|| format!("cannot read {}", file.display()))?;
            Some(secrets::for_instance(&text, &name, &instance_prefix)?)
        }
        None => None,
    };
    let connector = connector::open(server, home, connect)?;
    let (stored_path, stored) = match (&overlay, &secret) {
        (Some(overlay), Some(file)) => (
            Some(file.display().to_string()),
            Some(serde_yaml_ng::from_str(&overlay.text)?),
        ),
        _ => secrets::read(
            connector.as_ref(),
            server.server_home(),
            &name,
            &instance_prefix,
        )?
        .unzip(),
    };
    // The prefix is resolved above, so files without one get the `prefix`
    // setting rather than `main`.
    let loaded = AppConfig::load_layers(&files, stored, dev.overlay, Some(&instance_prefix))?;
    // Like the -f files, the secrets file is never uploaded with the sources.
    files.extend(secret);

    out!(
        "Deploying {} to {} ({})",
        loaded.config.instance(),
        server.name,
        connector.target()
    );
    if let Some(path) = stored_path {
        let what = if overlay.is_some() {
            "secrets"
        } else {
            "stored secrets"
        };
        out!("  using {what} {path}");
    }
    // --no-backup, --backup, `deploy.backup`, then the app's `deployment.backup`.
    let backup_profile = if no_backup {
        None
    } else {
        match (backup.as_deref(), &defaults.backup) {
            (Some(profile), _) => Some(profile),
            (None, Some(setting)) => setting.profile(),
            (None, None) => loaded.config.deployment.backup.as_deref(),
        }
    };
    let mode = args.mode_with(&defaults)?;
    let options = deploy::Options {
        apply: mode.apply,
        dry_run: args.dry_run,
        diff: args.diff,
        force_unlock: args.force_unlock,
        pull: mode.pull,
        recreate: mode.recreate,
        restore: None,
        purge,
        secrets: overlay.as_ref(),
        pinned: None,
        links_only: dev.links_only,
        backup: backup_profile.map(|profile| deploy::PreDeployBackup {
            server_name: &server.name,
            profile,
        }),
    };
    let dry_run = options.dry_run;
    deploy::deploy(
        connector.as_ref(),
        server.server_home(),
        manager_key,
        &loaded,
        &files,
        &options,
    )?;
    if dry_run {
        return Ok(());
    }
    schedule::refresh(
        connector.as_ref(),
        server.server_home(),
        manager_key,
        &server.name,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_prefix_from_flag_files_or_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("compost.yaml");
        fs::write(&plain, "name: shop\n").unwrap();
        let overlay = dir.path().join(".compost-review.yaml");
        fs::write(&overlay, "prefix: review\n").unwrap();

        let id = |files: &[PathBuf], prefix| instance_id(files, prefix, "staging").unwrap();
        assert_eq!(
            id(std::slice::from_ref(&plain), None),
            ("shop".into(), "staging".into())
        );
        assert_eq!(
            id(&[plain.clone(), overlay.clone()], None),
            ("shop".into(), "review".into())
        );
        assert_eq!(
            id(&[plain, overlay], Some("other")),
            ("shop".into(), "other".into())
        );
    }
}
