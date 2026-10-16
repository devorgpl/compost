//! `compost server backup`, `server restore` and `backup list` (spec §10.15–§10.17).

use std::path::PathBuf;

use anyhow::{Context, Result};

use super::deploy;
use super::lock::Lock;
use super::state::{self, Status};
use super::state_file::StateFile;
use crate::app::{self, backup::output, instance, restore::open_archive};
use crate::backup::{
    self, Backup, Format, ItemReader, ItemWriter, Manifest, Role, Scope, SharedRef,
};
use crate::config::server::{BackupTarget, ServerConfig};
use crate::connector::{Connector, shell_quote};
use crate::error::{CompostError, config_err};
use crate::shared::{self, ServerCtx, SharedAppConfig, connectors_dir};

const CONFIG_ITEM: &str = "server/compost-server.yaml";
const CONNECTORS_ITEM: &str = "server/connectors.tar";

pub struct BackupOptions {
    /// Only these shared apps (default: all).
    pub shared: Vec<String>,
    pub format: Format,
    pub target: Option<BackupTarget>,
    pub output: Option<PathBuf>,
    pub force_unlock: bool,
}

pub struct RestoreOptions {
    pub archive: String,
    pub source: Option<BackupTarget>,
    /// Only these shared apps (default: all in the archive).
    pub shared: Vec<String>,
    pub yes: bool,
    pub force_unlock: bool,
}

/// Shared apps with the given names (all when none are given).
fn select<'c>(
    config: &'c ServerConfig,
    names: &[String],
) -> Result<Vec<(&'c str, &'c SharedAppConfig)>> {
    for name in names {
        if !config.shared_apps().any(|(_, app)| app.name == *name) {
            return Err(config_err(format!(
                "--shared {name}: no such shared app (see `compost server state`)"
            )));
        }
    }
    Ok(config
        .shared_apps()
        .filter(|(_, app)| names.is_empty() || names.contains(&app.name))
        .collect())
}

pub fn backup(
    connector: &dyn Connector,
    server_name: &str,
    server_home: &str,
    manager_key: &str,
    options: &BackupOptions,
) -> Result<()> {
    let ctx = app::state::initialized_server(connector, server_home, manager_key)?;
    let config = instance::server_config(&ctx)?;
    let selected = select(&config, &options.shared)?;
    if selected.is_empty() {
        return Err(config_err(
            "the server has no shared applications to back up",
        ));
    }
    let output = output(&ctx, &config, options.target, options.output.as_ref())?;

    let _lock = Lock::acquire(connector, &ctx.home, options.force_unlock)?;
    let mut backup = Backup::new(
        connector,
        options.format,
        Manifest::new(Scope::Server, server_name),
        output,
    )?;
    out!("Backing up {server_name}:");
    let stored = ctx
        .read(super::SERVER_CONFIG_FILE)?
        .context("the server has no compost-server.yaml")?;
    backup.file(CONFIG_ITEM, Role::Meta, stored.as_bytes())?;

    // Link records hold the apps' database credentials: restored apps keep them.
    let mut records = Vec::new();
    for (kind, app) in &selected {
        let dir = connectors_dir(kind, &app.name);
        if !connector.list_dir(&ctx.path(&dir))?.is_empty() {
            records.push(shell_quote(&dir));
        }
    }
    if !records.is_empty() {
        backup.stream(
            CONNECTORS_ITEM,
            Role::Meta,
            &format!(
                "cd {} && tar -cf - {}",
                shell_quote(&ctx.home),
                records.join(" ")
            ),
        )?;
    }

    for (kind_name, app) in &selected {
        let kind = shared::find_kind(kind_name).context("kinds are checked on load")?;
        let id = SharedRef {
            kind: kind_name.to_string(),
            name: app.name.clone(),
        };
        if app.is_external() {
            out!(
                "  {kind_name} {}: external, its data is not saved (`compost app backup` saves the apps' databases)",
                app.name
            );
            continue;
        }
        let dir = id.dir();
        backup.manifest_mut().shared.push(id);
        let mut writer = ItemWriter::new(&mut backup, dir);
        kind.backup_shared(&ctx, app, &mut writer)
            .with_context(|| format!("{kind_name} {}", app.name))?;
    }

    let stem = match selected.as_slice() {
        [(_, one)] if !options.shared.is_empty() => {
            format!("{server_name}-{}-{}", one.name, backup::timestamp())
        }
        _ => format!("{server_name}-{}", backup::timestamp()),
    };
    let location = backup.finish(&stem)?;
    out!("Backup written to {location}");
    out!("It holds secrets (passwords of the shared apps and their links): keep it safe.");
    Ok(())
}

pub fn restore(
    connector: &dyn Connector,
    server_home: &str,
    manager_key: &str,
    options: &RestoreOptions,
) -> Result<()> {
    let report = state::check(connector, server_home, manager_key)?;
    let home = match (report.status, report.home) {
        (Status::Ok | Status::Degraded | Status::Uninitialized, Some(home)) => home,
        (status, _) => {
            return Err(CompostError::ServerNotReady(format!(
                "server is {status}: {}",
                report.detail.unwrap_or_default()
            ))
            .into());
        }
    };
    let ctx = ServerCtx { connector, home };
    let current = instance::server_config(&ctx)?;
    let archive = open_archive(&ctx, &current, options.source, &options.archive)?;
    if archive.manifest.scope != Scope::Server {
        return Err(config_err(format!(
            "{} is an application backup; restore it with `compost app restore`",
            options.archive
        )));
    }
    let archived = ServerConfig::parse(&archive.read(CONFIG_ITEM)?)
        .context("invalid compost-server.yaml in the archive")?;
    let selected: Vec<(String, SharedAppConfig)> = select(&archived, &options.shared)?
        .into_iter()
        .filter(|(kind, app)| {
            archive
                .manifest
                .shared
                .iter()
                .any(|s| s.kind == *kind && s.name == app.name)
        })
        .map(|(kind, app)| (kind.to_string(), app.clone()))
        .collect();
    if selected.is_empty() {
        return Err(config_err(
            "the archive has no shared applications to restore",
        ));
    }

    // Loading a dump into a running database fails half-way; only new shared apps are restored.
    let state_file = StateFile::load(&ctx)?;
    for (kind, app) in &selected {
        if state_file.shared(kind, &app.name).is_some() {
            return Err(CompostError::ServerNotReady(format!(
                "{kind} {} is already deployed on this server; restore onto a server without it, or choose others with --shared",
                app.name
            ))
            .into());
        }
    }
    let config = merged_config(&current, &archived, &selected)?;

    out!("Restoring on {}:", connector.target());
    for (kind, app) in &selected {
        out!("  {kind:<10} {}", app.name);
    }
    if !options.yes && !app::confirm()? {
        out!("Cancelled.");
        return Ok(());
    }

    // 1. Files the shared apps need before they start, and the link records.
    connector.exec(&format!("mkdir -p -- {}", shell_quote(&ctx.home)))?;
    {
        let _lock = Lock::acquire(connector, &ctx.home, options.force_unlock)?;
        if archive.has(CONNECTORS_ITEM) {
            restore_records(&ctx, &archive, &selected)?;
        }
        for (kind_name, app) in selected.iter().filter(|(_, app)| !app.is_external()) {
            let kind = shared::find_kind(kind_name).context("kinds are checked on load")?;
            let reader = reader(&archive, kind_name, &app.name);
            kind.restore_shared_files(&ctx, app, &reader)
                .with_context(|| format!("{kind_name} {}", app.name))?;
        }
    }

    // 2. Deploy and start them. Shared apps with links (e.g. keycloak) wait:
    //    their databases are in the data loaded next. Monitoring is set up
    //    with them.
    let deploy_options = deploy::Options {
        apply: true,
        dry_run: false,
        force_unlock: false,
    };
    let providers = without_consumers(&config, &selected)?;
    let later = providers.shared_apps().count() != config.shared_apps().count()
        || config
            .shared_apps()
            .any(|(_, app)| app.monitoring.is_some());
    deploy::deploy(
        connector,
        server_home,
        manager_key,
        &providers,
        &deploy_options,
    )?;

    // 3. Load their data.
    {
        let _lock = Lock::acquire(connector, &ctx.home, false)?;
        for (kind_name, app) in selected.iter().filter(|(_, app)| !app.is_external()) {
            let kind = shared::find_kind(kind_name).context("kinds are checked on load")?;
            out!("  loading data of {kind_name} {} ...", app.name);
            let reader = reader(&archive, kind_name, &app.name);
            kind.restore_shared_data(&ctx, app, &reader)
                .with_context(|| format!("{kind_name} {}", app.name))?;
        }
    }

    // 4. Then the shared apps with links.
    if later {
        deploy::deploy(
            connector,
            server_home,
            manager_key,
            &config,
            &deploy_options,
        )?;
    }
    out!("Restored. Restore applications with `compost app restore`.");
    Ok(())
}

/// The config without the restored shared apps that need others running
/// (e.g. keycloak and its database), and without `monitoring`, whose
/// monitoring app may be one of them. Their links are not validated: the
/// config is only deployed, and the full config was validated before.
fn without_consumers(
    config: &ServerConfig,
    selected: &[(String, SharedAppConfig)],
) -> Result<ServerConfig> {
    let mut providers = config.clone();
    for (kind_name, app) in selected {
        let waits = config
            .consumer(kind_name, app)?
            .is_some_and(|c| c.links.iter().any(|l| l.kind.orders_start()));
        if waits {
            if let Some(apps) = providers.shared.get_mut(kind_name) {
                apps.retain(|a| a.name != app.name);
            }
        }
    }
    providers.shared.retain(|_, apps| !apps.is_empty());
    for apps in providers.shared.values_mut() {
        for app in apps {
            app.monitoring = None;
        }
    }
    Ok(providers)
}

fn reader<'a, 'c>(archive: &'a backup::Archive<'c>, kind: &str, name: &str) -> ItemReader<'a, 'c> {
    let id = SharedRef {
        kind: kind.to_string(),
        name: name.to_string(),
    };
    ItemReader::new(archive, id.dir())
}

/// The server's current config plus the restored shared apps. The archived
/// `backup` settings are used when the server has none.
fn merged_config(
    current: &ServerConfig,
    archived: &ServerConfig,
    selected: &[(String, SharedAppConfig)],
) -> Result<ServerConfig> {
    let mut config = current.clone();
    for (kind, app) in selected {
        let apps = config.shared.entry(kind.clone()).or_default();
        apps.retain(|a| a.name != app.name);
        apps.push(app.clone());
    }
    if config.backup.is_none() {
        config.backup = archived.backup.clone();
    }
    // Validate the result, e.g. a second ingress.
    ServerConfig::parse(&serde_yaml_ng::to_string(&config)?)
}

/// Unpack the link records of the selected shared apps into `connectors/`.
fn restore_records(
    ctx: &ServerCtx,
    archive: &backup::Archive,
    selected: &[(String, SharedAppConfig)],
) -> Result<()> {
    let staging = ".restore-connectors";
    let mut script = format!(
        "cd {} && rm -rf {staging} && mkdir {staging} && tar -xf - -C {staging}",
        shell_quote(&ctx.home)
    );
    for (kind, app) in selected {
        let dir = shell_quote(&connectors_dir(kind, &app.name));
        script.push_str(&format!(
            " && if [ -d {staging}/{dir} ]; then mkdir -p \"$(dirname {dir})\" && rm -rf {dir} && mv {staging}/{dir} {dir}; fi"
        ));
    }
    script.push_str(&format!(" && rm -rf {staging}"));
    archive.feed(CONNECTORS_ITEM, b"", &script)
}

/// `compost backup list`: archives in the server's backup folder.
pub fn list(
    connector: &dyn Connector,
    server_home: &str,
    manager_key: &str,
    source: Option<BackupTarget>,
) -> Result<()> {
    let ctx = app::state::initialized_server(connector, server_home, manager_key)?;
    let config = instance::server_config(&ctx)?;
    let settings = config.backup.as_ref();
    if backup::resolve_target(source, settings)? != BackupTarget::Server {
        return Err(config_err(
            "backups go to this machine (target local); pass --source server to list the server's backup folder",
        ));
    }
    let dir = backup::server_dir(&ctx, settings);
    let listing = connector.exec(&format!(
        "cd {} 2>/dev/null || exit 0; for f in *; do [ -f \"$f\" ] && printf '%s\\t%s\\n' \"$(wc -c < \"$f\")\" \"$f\"; done; true",
        shell_quote(&dir)
    ))?;
    let mut rows: Vec<(u64, &str)> = listing
        .lines()
        .filter_map(|line| {
            let (size, name) = line.split_once('\t')?;
            Some((size.trim().parse().ok()?, name))
        })
        .collect();
    if rows.is_empty() {
        out!("No backups in {dir}");
        return Ok(());
    }
    rows.sort_by(|a, b| a.1.cmp(b.1));
    out!("{dir}:");
    let width = rows.iter().map(|(_, n)| n.len()).max().unwrap_or(0);
    for (size, name) in rows {
        out!("  {name:<width$}  {}", backup::human_size(size));
    }
    Ok(())
}
