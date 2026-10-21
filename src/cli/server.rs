//! `compost server …` commands (spec §10.3, §10.4, §10.6, §10.7).

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Args;

use super::{BackupOutputArgs, DeployArgs};
use crate::config::local::{CompostHome, ConnectorConfig, DeploySettings, ServerEntry, TraceLevel};
use crate::config::{check_name, server::ServerConfig};
use crate::connector::{self, ConnectOptions};
use crate::error::{CompostError, config_err};
use crate::server::backup::{self, BackupOptions, RestoreOptions};
use crate::server::deploy::{self, Options};
use crate::server::links;
use crate::server::secrets as server_secrets;
use crate::server::state::{self, Status};

const SERVER_FILE: &str = "compost-server.yaml";
const SERVER_SECRETS_FILE: &str = ".compost-server.secrets.yaml";

#[derive(Debug, Args)]
pub struct DefineArgs {
    /// Server name used in other commands
    pub name: String,
    /// Host name or IP address (SSH)
    #[arg(long, required_unless_present = "local", conflicts_with = "local")]
    pub address: Option<String>,
    /// SSH user
    #[arg(long, default_value = "compostuser", conflicts_with = "local")]
    pub user: String,
    /// SSH port
    #[arg(long, conflicts_with = "local")]
    pub port: Option<u16>,
    /// SSH private key (default: ssh config, agent, ~/.ssh/id_*)
    #[arg(long, conflicts_with = "local")]
    pub identity_file: Option<String>,
    /// compost directory on the server (default ~/.compost-server)
    #[arg(long)]
    pub server_home: Option<String>,
    /// Manage this machine instead of a remote server
    #[arg(long)]
    pub local: bool,
    /// Replace an existing server with the same name
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Args)]
pub struct ServerDeployArgs {
    #[command(flatten)]
    pub deploy: DeployArgs,
    /// Overlay with secrets (e.g. .compost-server.secrets.yaml) to store on the server
    /// and deploy with, replacing the stored one; later deploys use it without the file
    #[arg(long, value_name = "FILE")]
    pub secret: Option<PathBuf>,
    /// Only this shared app (repeatable or comma-separated; `kind/name` when two kinds
    /// have one with the name): the others, and their part of the stored configuration,
    /// stay as they were deployed. `--pull` and `--recreate` apply to it only
    #[arg(long, value_delimiter = ',', conflicts_with = "secret")]
    pub shared: Vec<String>,
}

#[derive(Debug, Args)]
pub struct ServerBackupArgs {
    /// Only this shared app (repeatable or comma-separated)
    #[arg(long, value_delimiter = ',')]
    pub shared: Vec<String>,
    #[command(flatten)]
    pub output: BackupOutputArgs,
    /// Remove a lock left behind by an interrupted compost command
    #[arg(long)]
    pub force_unlock: bool,
}

#[derive(Debug, Args)]
pub struct ServerPruneArgs {
    /// Show the disk usage and what would be removed, without removing anything
    #[arg(long)]
    pub dry_run: bool,
    /// Do not ask for confirmation
    #[arg(long)]
    pub yes: bool,
    /// Remove a lock left behind by an interrupted compost command
    #[arg(long)]
    pub force_unlock: bool,
}

#[derive(Debug, Args)]
pub struct ServerRestoreArgs {
    /// Archive written by `compost server backup` (with `--source server`: a file in the server's backup folder)
    pub archive: String,
    /// Only this shared app (repeatable or comma-separated)
    #[arg(long, value_delimiter = ',')]
    pub shared: Vec<String>,
    /// Where the archive is: `local`, `server` or a name from `backup.targets`
    /// (default: `backup.target` of compost-server.yaml, else local)
    #[arg(long, value_name = "TARGET")]
    pub source: Option<String>,
    /// Do not ask for confirmation
    #[arg(long)]
    pub yes: bool,
    /// Remove a lock left behind by an interrupted compost command
    #[arg(long)]
    pub force_unlock: bool,
}

pub fn define(home: &CompostHome, args: DefineArgs) -> Result<()> {
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
        deploy: DeploySettings::default(),
        trace: TraceLevel::None,
    };

    match servers.servers.iter_mut().find(|s| s.name == entry.name) {
        Some(_) if !args.force => {
            return Err(config_err(format!(
                "server `{}` is already defined (use --force to replace it)",
                entry.name
            )));
        }
        Some(existing) => {
            // Settings made with `compost config` survive a redefinition.
            let deploy = std::mem::take(&mut existing.deploy);
            *existing = ServerEntry {
                deploy,
                trace: existing.trace,
                ..entry.clone()
            };
        }
        None => servers.servers.push(entry.clone()),
    }
    home.save_servers(&servers)?;
    out!(
        "Defined server {} ({})",
        entry.name,
        entry.connector.describe()
    );
    Ok(())
}

pub fn list(home: &CompostHome) -> Result<()> {
    let servers = home.servers()?;
    if servers.servers.is_empty() {
        out!("No servers defined. Add one with `compost server define`.");
        return Ok(());
    }

    let current = home.current_context()?;
    let width = servers
        .servers
        .iter()
        .map(|s| s.name.len())
        .max()
        .unwrap_or(0);
    for server in &servers.servers {
        let marker = if current.as_deref() == Some(server.name.as_str()) {
            "*"
        } else {
            " "
        };
        out!(
            "{marker} {:<width$}  {}  home: {}",
            server.name,
            server.connector.describe(),
            server.server_home()
        );
    }
    Ok(())
}

#[derive(Debug, Args)]
pub struct ServerLogsArgs {
    /// Shared app, as in compost-server.yaml, e.g. `caddy-main` (`<kind>/<name>` if two kinds use the name)
    pub name: String,
    #[command(flatten)]
    pub logs: super::LogsArgs,
}

pub fn logs(
    home: &CompostHome,
    server: Option<&str>,
    args: ServerLogsArgs,
    connect: &ConnectOptions,
) -> Result<()> {
    let manager = home.require_manager()?;
    let server = home.resolve_server(server)?;
    let connector = connector::open(&server, home, connect)?;
    crate::server::logs::logs(
        connector.as_ref(),
        server.server_home(),
        &manager.manager_key,
        &args.name,
        &args.logs.options(Vec::new()),
    )
}

#[derive(Debug, Args)]
pub struct ServerExecArgs {
    /// Shared app, as in compost-server.yaml, e.g. `postgres18` (`<kind>/<name>` if two kinds use the name)
    pub name: String,
    /// Another service of the shared app, e.g. `crowdsec` of a caddysecurebuild ingress
    /// (default: its main container)
    #[arg(long)]
    pub service: Option<String>,
    /// The command and its arguments, after `--`, e.g. `-- sh` or `-- psql -U postgres`
    #[arg(last = true, required = true, value_name = "COMMAND")]
    pub command: Vec<String>,
}

pub fn exec(
    home: &CompostHome,
    server: Option<&str>,
    args: ServerExecArgs,
    connect: &ConnectOptions,
) -> Result<()> {
    if let Some(service) = &args.service {
        check_name("service", service)?;
    }
    let manager = home.require_manager()?;
    let server = home.resolve_server(server)?;
    let connector = connector::open(&server, home, connect)?;
    crate::server::exec::exec(
        connector.as_ref(),
        server.server_home(),
        &manager.manager_key,
        &args.name,
        args.service.as_deref(),
        &args.command,
    )
}

#[derive(Debug, Args)]
pub struct ServerCmdArgs {
    /// Command from `commands` of compost-server.yaml, or compost's own as
    /// `<shared app>.<command>` (e.g. `caddy-main.unban`), then its arguments, in order or as
    /// `--<arg> <value>`, and `--yes`, `--dry-run` or `--help`. Without a command, lists them;
    /// `--history` shows the last ones run
    #[arg(
        value_name = "COMMAND] [ARGS",
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    pub words: Vec<String>,
}

pub fn cmd(
    home: &CompostHome,
    server: Option<&str>,
    args: ServerCmdArgs,
    connect: &ConnectOptions,
) -> Result<()> {
    let mut words = args.words.into_iter();
    let name = words.next();
    // A built-in command is `<shared app>.<command>`, found by its parts.
    if let Some(name) = name
        .as_ref()
        .filter(|n| *n != "--history" && !n.contains('.'))
    {
        check_name("command name", name)?;
    }
    let words: Vec<String> = words.collect();
    let manager = home.require_manager()?;
    let server = home.resolve_server(server)?;
    let connector = connector::open(&server, home, connect)?;
    crate::server::exec::cmd(
        connector.as_ref(),
        &server.name,
        server.server_home(),
        &manager.manager_key,
        name.as_deref(),
        &words,
    )
}

pub fn state(home: &CompostHome, server: Option<&str>, connect: &ConnectOptions) -> Result<()> {
    let manager = home.require_manager()?;
    let server = home.resolve_server(server)?;
    let connector = connector::open(&server, home, connect)?;
    let mut report = state::check(
        connector.as_ref(),
        server.server_home(),
        &manager.manager_key,
    )?;
    let external = state::check_external(connector.as_ref(), &mut report)?;
    state::details(connector.as_ref(), &mut report)?;

    let summary = format!(
        "{} ({}): {}",
        server.name,
        connector.target(),
        report.status
    );
    let detail = report.detail.as_deref().unwrap_or_default();
    match report.status {
        Status::Unreachable | Status::NoDocker | Status::Foreign => {
            return Err(CompostError::ServerNotReady(format!("{summary}\n{detail}")).into());
        }
        Status::Uninitialized | Status::Degraded | Status::Ok => {}
    }

    out!("{summary}");
    if let (Some(docker), Some(server_home)) = (&report.docker, &report.home) {
        out!("  {docker}, home {server_home}");
    }
    if report.shared.is_empty() {
        out!("  no shared applications");
    }
    for app in &report.shared {
        let version = app
            .version
            .map_or_else(|| "-".to_string(), |v| v.to_string());
        let checked = external
            .iter()
            .find(|(kind, name, _)| *kind == app.kind && *name == app.name);
        let running = match checked {
            Some((_, _, Ok(answer))) => format!("external, {answer}"),
            Some((_, _, Err(err))) => format!("external, not reachable: {err:#}"),
            None if app.on_demand => "on demand".to_string(),
            None if app.running => "running".to_string(),
            None => "not running".to_string(),
        };
        out!(
            "  {:<10} {:<16} {} {version}  {running}",
            app.kind,
            app.name,
            app.type_name
        );
        for line in &app.details {
            out!("  {:<10} {:<16} {line}", "", "");
        }
    }

    if let (Some(server_home), false) = (&report.home, report.status == Status::Uninitialized) {
        let ctx = crate::shared::ServerCtx {
            connector: connector.as_ref(),
            home: server_home.clone(),
        };
        let state_file = crate::server::state_file::StateFile::load(&ctx)?;
        let open: Vec<_> = state_file.alerts.iter().filter(|a| a.alerting).collect();
        if !open.is_empty() {
            out!("  alerts (notified):");
            for alert in open {
                let detail = alert
                    .detail
                    .as_deref()
                    .and_then(|d| d.lines().next())
                    .unwrap_or_default();
                out!("    {}  since {}  {detail}", alert.key, alert.since);
            }
        }
        let records = state_file.backups;
        if !records.is_empty() {
            out!("  scheduled backups:");
            let owner = records.iter().map(|r| r.owner.len()).max().unwrap_or(0);
            let profile = records.iter().map(|r| r.profile.len()).max().unwrap_or(0);
            for record in &records {
                let removed = record.owner != crate::server::schedule::SERVER_OWNER
                    && !state_file
                        .apps
                        .iter()
                        .any(|app| format!("{}-{}", app.name, app.prefix) == record.owner);
                let text = if removed {
                    record.describe_removed()
                } else {
                    record.describe()
                };
                out!(
                    "    {:<owner$}  {:<profile$}  {text}",
                    record.owner,
                    record.profile
                );
            }
        }
    }

    if report.status == Status::Degraded {
        return Err(CompostError::Unhealthy(format!(
            "{}: some shared applications are not running",
            server.name
        ))
        .into());
    }
    Ok(())
}

pub fn deploy(
    home: &CompostHome,
    server: Option<&str>,
    args: ServerDeployArgs,
    connect: &ConnectOptions,
) -> Result<()> {
    let ServerDeployArgs {
        deploy: args,
        secret,
        shared,
    } = args;
    let manager = home.require_manager()?;
    let server = home.resolve_server(server)?;
    let apply = args.apply(home, &server)?;
    let mut files = if args.files.is_empty() {
        default_files()?
    } else {
        args.files
    };
    // --secret takes the place of the stored overlay, right after the first
    // file; merging it again later would change nothing.
    let text = match &secret {
        Some(file) => {
            let mut index = 0;
            files.retain(|f| {
                index += 1;
                index == 1 || f != file
            });
            Some(
                fs::read_to_string(file)
                    .with_context(|| format!("cannot read {}", file.display()))?,
            )
        }
        None => None,
    };

    let connector = connector::open(&server, home, connect)?;
    let (stored_path, stored) = match (&secret, &text) {
        (Some(file), Some(text)) => (
            Some(file.display().to_string()),
            Some(server_secrets::parse(text)?),
        ),
        _ => server_secrets::read(connector.as_ref(), server.server_home())?.unzip(),
    };
    let (config, skipped) = ServerConfig::load_with(&files, stored)?;

    out!("Deploying to {} ({})", server.name, connector.target());
    if let Some(path) = stored_path {
        let what = if secret.is_some() {
            "secrets"
        } else {
            "stored secrets"
        };
        out!("  using {what} {path}");
    }
    for app in skipped {
        out!("  secrets of {app} not used: it is not in the configuration");
    }
    let options = Options {
        apply,
        dry_run: args.dry_run,
        diff: args.diff,
        force_unlock: args.force_unlock,
        pull: args.pull,
        recreate: args.recreate,
        secrets: text.as_deref(),
        shared: &shared,
    };
    deploy::deploy(
        connector.as_ref(),
        server.server_home(),
        &manager.manager_key,
        &config,
        &options,
    )?;
    if !args.dry_run {
        crate::server::schedule::refresh(
            connector.as_ref(),
            server.server_home(),
            &manager.manager_key,
            &server.name,
        )?;
    }

    if apply && !args.dry_run {
        out!();
        return state(home, Some(&server.name), connect);
    }
    Ok(())
}

/// `compost connector list` (spec §10.11).
pub fn connector_list(
    home: &CompostHome,
    server: Option<&str>,
    connect: &ConnectOptions,
) -> Result<()> {
    let manager = home.require_manager()?;
    let server = home.resolve_server(server)?;
    let connector = connector::open(&server, home, connect)?;
    links::list(
        connector.as_ref(),
        server.server_home(),
        &manager.manager_key,
    )
}

pub fn backup(
    home: &CompostHome,
    server: Option<&str>,
    args: ServerBackupArgs,
    connect: &ConnectOptions,
) -> Result<()> {
    let manager = home.require_manager()?;
    let server = home.resolve_server(server)?;
    let connector = connector::open(&server, home, connect)?;
    let options = BackupOptions {
        shared: args.shared,
        flags: args.output.flags(Vec::new(), Vec::new())?,
        output: args.output.output,
        force_unlock: args.force_unlock,
    };
    backup::backup(
        connector.as_ref(),
        &server.name,
        server.server_home(),
        &manager.manager_key,
        &options,
    )
}

pub fn restore(
    home: &CompostHome,
    server: Option<&str>,
    args: ServerRestoreArgs,
    connect: &ConnectOptions,
) -> Result<()> {
    let manager = home.require_manager()?;
    let server = home.resolve_server(server)?;
    let connector = connector::open(&server, home, connect)?;
    let options = RestoreOptions {
        archive: args.archive,
        source: args.source,
        shared: args.shared,
        yes: args.yes,
        force_unlock: args.force_unlock,
    };
    backup::restore(
        connector.as_ref(),
        server.server_home(),
        &manager.manager_key,
        &options,
    )?;
    crate::server::schedule::refresh(
        connector.as_ref(),
        server.server_home(),
        &manager.manager_key,
        &server.name,
    )?;
    out!();
    state(home, Some(&server.name), connect)
}

/// `compost backup list`
pub fn backup_list(
    home: &CompostHome,
    server: Option<&str>,
    source: Option<&str>,
    connect: &ConnectOptions,
) -> Result<()> {
    let manager = home.require_manager()?;
    let server = home.resolve_server(server)?;
    let connector = connector::open(&server, home, connect)?;
    backup::list(
        connector.as_ref(),
        server.server_home(),
        &manager.manager_key,
        source,
    )
}

/// `compost server prune`
pub fn prune(
    home: &CompostHome,
    server: Option<&str>,
    args: ServerPruneArgs,
    connect: &ConnectOptions,
) -> Result<()> {
    let manager = home.require_manager()?;
    let server = home.resolve_server(server)?;
    let connector = connector::open(&server, home, connect)?;
    let options = crate::server::prune::Options {
        dry_run: args.dry_run,
        yes: args.yes,
        force_unlock: args.force_unlock,
    };
    crate::server::prune::prune(
        connector.as_ref(),
        server.server_home(),
        &manager.manager_key,
        &options,
    )
}

/// `compost notify test`
pub fn notify_test(
    home: &CompostHome,
    server: Option<&str>,
    to: Option<&str>,
    message: Option<&str>,
    connect: &ConnectOptions,
) -> Result<()> {
    let manager = home.require_manager()?;
    let server = home.resolve_server(server)?;
    let connector = connector::open(&server, home, connect)?;
    crate::notify::test(
        connector.as_ref(),
        &server.name,
        server.server_home(),
        &manager.manager_key,
        to,
        message,
    )
}

/// `compost backup run`
pub fn backup_run(
    home: &CompostHome,
    server: Option<&str>,
    profile: &str,
    force_unlock: bool,
    connect: &ConnectOptions,
) -> Result<()> {
    check_name("backup profile", profile)?;
    let manager = home.require_manager()?;
    let server = home.resolve_server(server)?;
    let connector = connector::open(&server, home, connect)?;
    backup::run_profile(
        connector.as_ref(),
        &server.name,
        server.server_home(),
        &manager.manager_key,
        profile,
        force_unlock,
    )
}

/// `compost backup tick`, run by cron on the server.
pub fn backup_tick(server_home: &str, server_name: &str) -> Result<()> {
    crate::server::schedule::tick(server_home, server_name)
}

/// `compost backup prune`
pub fn backup_prune(
    home: &CompostHome,
    server: Option<&str>,
    options: &backup::PruneOptions,
    connect: &ConnectOptions,
) -> Result<()> {
    let manager = home.require_manager()?;
    let server = home.resolve_server(server)?;
    let connector = connector::open(&server, home, connect)?;
    backup::prune(
        connector.as_ref(),
        &server.name,
        server.server_home(),
        &manager.manager_key,
        options,
    )
}

/// `compost-server.yaml` plus the secrets file when it exists (spec §7).
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
