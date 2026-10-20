//! `compost dev …`: run an app's stack on this machine for development.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use serde_yaml_ng::Value;

use super::DeployArgs;
use super::app::{AppDeployArgs, DevDeploy, check_command_name, config_files, instance_id};
use crate::app::instance::Selector;
use crate::app::{remove, reset, stop};
use crate::config::app::AppConfig;
use crate::config::local::{CompostHome, ServerEntry};
use crate::config::merge;
use crate::connector::{self, ConnectOptions, Connector, shell_quote};
use crate::error::config_err;
use crate::server::state::{self, Status};
use crate::server::{deploy as server_deploy, schedule, secrets as server_secrets};
use crate::shared::{self, ServerCtx};

/// Overlay `dev` commands add after compost.yaml when it exists.
const LOCAL_OVERLAY: &str = ".compost-local.yaml";
/// Prefix of the instance when the files set none.
const DEFAULT_PREFIX: &str = "local";

#[derive(Debug, Subcommand)]
pub enum DevCommand {
    /// Run the app on this machine: add the shared apps its links need and
    /// the server lacks, give it its own databases, buckets and logins on
    /// them, and start it
    Up(DevUpArgs),
    /// Delete the data of the app's links (databases, buckets, key-value
    /// databases, realms, namespaces) and start it again with empty ones;
    /// passwords and keys stay the same
    Reset(DevResetArgs),
    /// Print the app's variables for running it outside Docker, with shared
    /// apps on 127.0.0.1 (after `dev up --host`)
    Env(DevEnvArgs),
    /// List the app's commands, or run one by name, like `app cmd`: its
    /// `commands`, and `<link>.import`/`<link>.export` of database links
    Cmd(DevCmdArgs),
    /// Stop the app's containers; data and shared apps are kept, and `dev up`
    /// starts it again
    Stop(DevStopArgs),
    /// Remove the app's containers; with --purge also its data and volumes
    Down(DevDownArgs),
    /// Remove every app and shared app on this machine with all their data,
    /// compost's networks and its folder (~/.compost-server), to start again
    /// from nothing with `dev up`; ~/.compost stays
    ResetAll(DevResetAllArgs),
}

/// The app's config files and instance prefix.
#[derive(Debug, Args)]
pub struct ProjectArgs {
    /// Config files merged in order (repeatable or comma-separated; default:
    /// compost.yaml, and .compost-local.yaml if it exists)
    #[arg(short = 'f', long = "file", value_delimiter = ',')]
    pub files: Vec<PathBuf>,
    /// Instance prefix (default: the one from the config files, else `local`)
    #[arg(long)]
    pub prefix: Option<String>,
}

#[derive(Debug, Args)]
pub struct DevUpArgs {
    #[command(flatten)]
    pub project: ProjectArgs,
    /// Run the app outside Docker, e.g. from an IDE: set up its links only,
    /// stop its containers, and publish the shared apps it uses on
    /// 127.0.0.1 (see `dev env`)
    #[arg(long)]
    pub host: bool,
    /// Show what would change, without changing anything
    #[arg(long)]
    pub dry_run: bool,
    /// Pull images again, and build on newer base images
    #[arg(long)]
    pub pull: bool,
    /// Recreate every container of the app, even unchanged ones; data in volumes is kept
    #[arg(long)]
    pub recreate: bool,
    /// Remove a lock left behind by an interrupted compost command
    #[arg(long)]
    pub force_unlock: bool,
}

#[derive(Debug, Args)]
pub struct DevResetArgs {
    #[command(flatten)]
    pub project: ProjectArgs,
    /// Only links of these kinds, e.g. `--kind database` (default: all that hold data)
    #[arg(long, value_delimiter = ',')]
    pub kind: Vec<String>,
    /// Only this link: `<entry>` or `<kind>:<entry>` (repeatable)
    #[arg(long)]
    pub link: Vec<String>,
    /// Generate new passwords and keys instead of keeping them
    #[arg(long)]
    pub new_credentials: bool,
    /// Do not ask for confirmation
    #[arg(long)]
    pub yes: bool,
    /// Remove a lock left behind by an interrupted compost command
    #[arg(long)]
    pub force_unlock: bool,
}

#[derive(Debug, Args)]
pub struct DevEnvArgs {
    #[command(flatten)]
    pub project: ProjectArgs,
    /// Only the variables of this service (an `app_services` name or a compose
    /// service; default: all, which must not set a variable differently)
    #[arg(long)]
    pub service: Option<String>,
    /// `dotenv` (KEY="value"), `shell` (export KEY='value') or `json`
    #[arg(long, value_enum, default_value = "dotenv")]
    pub format: EnvFormat,
    /// Write to this file (mode 0600) instead of printing
    #[arg(short, long)]
    pub output: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct DevCmdArgs {
    #[command(flatten)]
    pub project: ProjectArgs,
    /// The command's name and its arguments, e.g. `main.import --file dump.sql`;
    /// `--history` shows the last ones run
    #[arg(
        value_name = "COMMAND] [ARGS",
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    pub words: Vec<String>,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum EnvFormat {
    Dotenv,
    Shell,
    Json,
}

#[derive(Debug, Args)]
pub struct DevStopArgs {
    #[command(flatten)]
    pub project: ProjectArgs,
    /// Remove a lock left behind by an interrupted compost command
    #[arg(long)]
    pub force_unlock: bool,
}

#[derive(Debug, Args)]
pub struct DevResetAllArgs {
    /// Do not ask for confirmation
    #[arg(long)]
    pub yes: bool,
    /// Remove a lock left behind by an interrupted compost command
    #[arg(long)]
    pub force_unlock: bool,
}

#[derive(Debug, Args)]
pub struct DevDownArgs {
    #[command(flatten)]
    pub project: ProjectArgs,
    /// Also delete the app's data: its databases, buckets and other link
    /// data, and the compose project's volumes
    #[arg(long)]
    pub purge: bool,
    /// Do not ask for confirmation (only --purge asks)
    #[arg(long)]
    pub yes: bool,
    /// Remove a lock left behind by an interrupted compost command
    #[arg(long)]
    pub force_unlock: bool,
}

/// The app's files, name and prefix.
struct Project {
    files: Vec<PathBuf>,
    name: String,
    prefix: String,
}

impl Project {
    fn find(args: &ProjectArgs) -> Result<Self> {
        let mut files = config_files(&args.files)?;
        if args.files.is_empty() && Path::new(LOCAL_OVERLAY).exists() {
            files.push(PathBuf::from(LOCAL_OVERLAY));
        }
        let prefix = match &args.prefix {
            Some(prefix) => prefix.clone(),
            None => merge::load_merged(&files)?
                .get("prefix")
                .and_then(Value::as_str)
                .unwrap_or(DEFAULT_PREFIX)
                .to_string(),
        };
        let (name, prefix) = instance_id(&files, Some(&prefix))?;
        Ok(Self {
            files,
            name,
            prefix,
        })
    }

    fn selector(&self) -> Selector {
        Selector::Name {
            name: self.name.clone(),
            prefix: self.prefix.clone(),
        }
    }
}

pub fn run(
    home: &CompostHome,
    server: Option<&str>,
    command: DevCommand,
    connect: &ConnectOptions,
) -> Result<()> {
    if home.manager()?.is_none() {
        super::init::run(
            home,
            super::init::InitArgs {
                key: None,
                force: false,
            },
        )?;
    }
    let manager_key = home.require_manager()?.manager_key;
    let server = crate::dev::target(home, server)?;
    match command {
        DevCommand::Up(args) => {
            let project = Project::find(&args.project)?;
            up(home, &manager_key, &server, &project, &args, connect)
        }
        DevCommand::Reset(args) => {
            let project = Project::find(&args.project)?;
            let connector = connector::open(&server, home, connect)?;
            let options = reset::Options {
                kinds: args.kind,
                links: args.link,
                new_credentials: args.new_credentials,
                yes: args.yes,
                force_unlock: args.force_unlock,
            };
            let done = reset::reset(
                connector.as_ref(),
                server.server_home(),
                &manager_key,
                &project.name,
                &project.prefix,
                &options,
            )?;
            if !done {
                return Ok(());
            }
            // In the mode it was deployed in.
            let host = provisioned(home, &server, &manager_key, &project, connect)?;
            let up_args = DevUpArgs {
                project: args.project,
                host,
                dry_run: false,
                pull: false,
                recreate: false,
                force_unlock: false,
            };
            up(home, &manager_key, &server, &project, &up_args, connect)
        }
        DevCommand::ResetAll(args) => {
            let connector = connector::open(&server, home, connect)?;
            let options = crate::server::teardown::Options {
                yes: args.yes,
                force_unlock: args.force_unlock,
            };
            crate::server::teardown::teardown(
                connector.as_ref(),
                &server.name,
                server.server_home(),
                &manager_key,
                &options,
            )?;
            Ok(())
        }
        DevCommand::Env(args) => {
            let project = Project::find(&args.project)?;
            env(home, &manager_key, &server, &project, &args, connect)
        }
        DevCommand::Cmd(args) => {
            let project = Project::find(&args.project)?;
            let mut words = args.words.into_iter();
            let name = words.next();
            if let Some(name) = &name {
                check_command_name(name)?;
            }
            let words: Vec<String> = words.collect();
            let selector = Selector::Name {
                name: project.name.clone(),
                prefix: project.prefix.clone(),
            };
            let connector = connector::open(&server, home, connect)?;
            crate::app::exec::template(
                connector.as_ref(),
                server.server_home(),
                &manager_key,
                &selector,
                name.as_deref(),
                &words,
            )
        }
        DevCommand::Stop(args) => {
            let project = Project::find(&args.project)?;
            let connector = connector::open(&server, home, connect)?;
            stop::stop(
                connector.as_ref(),
                server.server_home(),
                &manager_key,
                &project.name,
                &project.prefix,
                args.force_unlock,
            )
        }
        DevCommand::Down(args) => {
            let project = Project::find(&args.project)?;
            let connector = connector::open(&server, home, connect)?;
            let purge = if args.purge {
                let config = AppConfig::load_with(&project.files, None, Some(&project.prefix))?;
                purgeable_kinds(&config.config)
            } else {
                Vec::new()
            };
            let options = remove::Options {
                volumes: args.purge,
                purge,
                yes: args.yes || !args.purge,
                force_unlock: args.force_unlock,
            };
            remove::remove(
                connector.as_ref(),
                server.server_home(),
                &manager_key,
                &project.selector(),
                &options,
            )?;
            schedule::refresh(
                connector.as_ref(),
                server.server_home(),
                &manager_key,
                &server.name,
            )
        }
    }
}

/// Kinds of the app's links that hold data.
fn purgeable_kinds(app: &AppConfig) -> Vec<String> {
    let mut kinds: Vec<String> = app
        .links()
        .filter(|(kind, _)| shared::find_kind(kind).is_some_and(|k| k.supports_purge()))
        .map(|(kind, _)| kind.to_string())
        .collect();
    kinds.sort();
    kinds.dedup();
    kinds
}

fn up(
    home: &CompostHome,
    manager_key: &str,
    server: &ServerEntry,
    project: &Project,
    args: &DevUpArgs,
    connect: &ConnectOptions,
) -> Result<()> {
    let connector = connector::open(server, home, connect)?;
    let (ready, overlay) = shared_apps(connector.as_ref(), manager_key, server, project, args)?;
    drop(connector);
    if !ready {
        out!("The app's dry run needs those shared apps; run `compost dev up` to add them.");
        return Ok(());
    }
    let deploy = AppDeployArgs {
        deploy: DeployArgs {
            apply: true,
            no_apply: false,
            files: project.files.clone(),
            dry_run: args.dry_run,
            diff: false,
            force_unlock: args.force_unlock,
            pull: args.pull,
            recreate: args.recreate,
        },
        prefix: Some(project.prefix.clone()),
        purge: Vec::new(),
        secret: None,
        backup: None,
        no_backup: true,
    };
    let dev = DevDeploy {
        links_only: args.host,
        overlay,
    };
    super::app::deploy(home, manager_key, server, deploy, dev, connect)?;
    if args.host && !args.dry_run {
        out!();
        out!("Run it outside Docker with the variables of `compost dev env`, e.g.");
        out!("  compost dev env -o .env");
    }
    Ok(())
}

/// The instance was deployed with `dev up --host`.
fn provisioned(
    home: &CompostHome,
    server: &ServerEntry,
    manager_key: &str,
    project: &Project,
    connect: &ConnectOptions,
) -> Result<bool> {
    let connector = connector::open(server, home, connect)?;
    let ctx = crate::app::state::initialized_server(
        connector.as_ref(),
        server.server_home(),
        manager_key,
    )?;
    let state_file = crate::server::state_file::StateFile::load(&ctx)?;
    Ok(state_file
        .app(&project.name, &project.prefix)
        .is_some_and(|r| r.status == crate::server::state_file::DeployStatus::Provisioned))
}

fn env(
    home: &CompostHome,
    manager_key: &str,
    server: &ServerEntry,
    project: &Project,
    args: &DevEnvArgs,
    connect: &ConnectOptions,
) -> Result<()> {
    let connector = connector::open(server, home, connect)?;
    let stored = crate::app::secrets::read(
        connector.as_ref(),
        server.server_home(),
        &project.name,
        &project.prefix,
    )?
    .map(|(_, value)| value);
    let ctx = crate::app::state::initialized_server(
        connector.as_ref(),
        server.server_home(),
        manager_key,
    )?;
    let server_config = crate::app::instance::server_config(&ctx)?;
    let unpinned = AppConfig::load_with(&project.files, stored.clone(), Some(&project.prefix))?;
    let repins = crate::dev::shared::repins(&server_config, &unpinned.config)?;
    let loaded = AppConfig::load_layers(
        &project.files,
        stored,
        crate::dev::shared::dev_overlay(&repins, &unpinned.config),
        Some(&project.prefix),
    )?;
    let services = crate::app::deploy::host_env(
        connector.as_ref(),
        server.server_home(),
        manager_key,
        &loaded,
    )?;
    let variables = select_env(&loaded.config, services, args.service.as_deref())?;
    let text = format_env(&variables, args.format)?;
    match &args.output {
        None => {
            out_part!("{text}");
            Ok(())
        }
        Some(path) => write_env(path, &text),
    }
}

/// The variables of one service, or of all of them when they agree.
fn select_env(
    app: &AppConfig,
    services: BTreeMap<String, BTreeMap<String, String>>,
    service: Option<&str>,
) -> Result<BTreeMap<String, String>> {
    if let Some(wanted) = service {
        let compose = app
            .app_service(wanted)
            .map_or(wanted, |s| s.service.as_str());
        return services
            .get(compose)
            .cloned()
            .ok_or_else(|| config_err(format!("--service {wanted}: no variables for it")));
    }
    let mut merged: BTreeMap<String, String> = BTreeMap::new();
    for (service, variables) in &services {
        for (key, value) in variables {
            match merged.get(key) {
                Some(other) if other != value => {
                    return Err(config_err(format!(
                        "services set {key} differently (e.g. {service}); choose one with --service"
                    )));
                }
                _ => {
                    merged.insert(key.clone(), value.clone());
                }
            }
        }
    }
    Ok(merged)
}

fn format_env(variables: &BTreeMap<String, String>, format: EnvFormat) -> Result<String> {
    Ok(match format {
        EnvFormat::Json => format!("{}\n", serde_json::to_string_pretty(variables)?),
        EnvFormat::Shell => variables
            .iter()
            .map(|(key, value)| format!("export {key}={}\n", shell_quote(value)))
            .collect(),
        EnvFormat::Dotenv => variables
            .iter()
            .map(|(key, value)| format!("{key}={}\n", dotenv_quote(value)))
            .collect(),
    })
}

/// A dotenv value: as is when that is safe, else in double quotes.
fn dotenv_quote(value: &str) -> String {
    let plain = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-.,:/@%+=".contains(c));
    if plain {
        return value.to_string();
    }
    let escaped: String = value
        .chars()
        .map(|c| match c {
            '\\' => "\\\\".to_string(),
            '"' => "\\\"".to_string(),
            '$' => "\\$".to_string(),
            '\n' => "\\n".to_string(),
            c => c.to_string(),
        })
        .collect();
    format!("\"{escaped}\"")
}

/// Write the variables with mode 0600; warn when git would commit them.
fn write_env(path: &Path, text: &str) -> Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("cannot write {}", path.display()))?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(text.as_bytes())
        .with_context(|| format!("cannot write {}", path.display()))?;
    out!("Wrote {} (it holds passwords)", path.display());
    // 1: in a repository and not ignored; other codes: ignored, no repository or no git.
    let ignored = std::process::Command::new("git")
        .args(["check-ignore", "-q", "--"])
        .arg(path)
        .stderr(std::process::Stdio::null())
        .status();
    if ignored.is_ok_and(|status| status.code() == Some(1)) {
        eprintln!(
            "warning: git does not ignore {}; add it to .gitignore",
            path.display()
        );
    }
    Ok(())
}

/// Add the shared apps the app's links need and start the ones that don't
/// run. Returns `false` when a dry run would add some, and the overlay that
/// points links at other shared apps than the ones they pin.
fn shared_apps(
    connector: &dyn Connector,
    manager_key: &str,
    server: &ServerEntry,
    project: &Project,
    args: &DevUpArgs,
) -> Result<(bool, Option<Value>)> {
    let report = state::check(connector, server.server_home(), manager_key)?;
    let current = match report.status {
        Status::Ok | Status::Degraded => {
            let ctx = ServerCtx {
                connector,
                home: report
                    .home
                    .clone()
                    .context("server home was not resolved")?,
            };
            crate::app::instance::server_config(&ctx)?
        }
        // `server deploy` reports the other states.
        _ => Default::default(),
    };
    let loaded = AppConfig::load_with(&project.files, None, Some(&project.prefix))?;
    let free = |port: u16| std::net::TcpListener::bind(("127.0.0.1", port)).is_ok();
    let host: Option<&dyn Fn(u16) -> bool> = if args.host { Some(&free) } else { None };
    let plan = crate::dev::shared::plan(&current, &loaded.config, host)?;
    for repin in &plan.repins {
        out!(
            "services.{}[{}]: {} has no `{}`; using {}",
            repin.kind,
            repin.entry,
            server.name,
            repin.from,
            repin.to
        );
    }
    for profile in crate::dev::shared::scheduled_profiles(&loaded.config) {
        out!(
            "backup.profiles.{profile}: not scheduled on this machine (`compost app backup --profile {profile}` runs it)"
        );
    }
    let overlay = crate::dev::shared::dev_overlay(&plan.repins, &loaded.config);
    if !plan.changes() && report.status == Status::Ok {
        return Ok((true, overlay));
    }
    out!("Shared apps on {} ({})", server.name, connector.target());
    for added in &plan.added {
        let version = added
            .app
            .version
            .map(|v| format!(" {v}"))
            .unwrap_or_default();
        out!(
            "  adding {} {} ({}{version}) for {}",
            added.kind,
            added.app.name,
            added.app.type_name,
            added.for_link
        );
    }
    for configured in &plan.configured {
        let value = serde_yaml_ng::to_string(&configured.value)?;
        out!(
            "  setting {}: {} on {} {} for {}",
            configured.key,
            value.trim_end(),
            configured.kind,
            configured.name,
            configured.for_link
        );
    }
    for published in &plan.published {
        let recreated = if published.existing {
            "; recreates its container, data is kept"
        } else {
            ""
        };
        out!(
            "  publishing {} {} on 127.0.0.1:{}{recreated}",
            published.kind,
            published.name,
            published.port
        );
    }
    let stored = server_secrets::read(connector, server.server_home())?.map(|(_, value)| value);
    let config = plan.config.with_stored(stored)?;
    let options = server_deploy::Options {
        apply: true,
        dry_run: args.dry_run,
        diff: false,
        force_unlock: args.force_unlock,
        pull: false,
        recreate: false,
        secrets: None,
    };
    server_deploy::deploy(
        connector,
        server.server_home(),
        manager_key,
        &config,
        &options,
    )?;
    if args.dry_run {
        return Ok((plan.added.is_empty(), overlay));
    }
    schedule::refresh(connector, server.server_home(), manager_key, &server.name)?;
    out!();
    Ok((true, overlay))
}
