//! `compost server backup`, `server restore` and `backup list` (spec §10.15–§10.17).

use std::fmt::Write as _;
use std::path::PathBuf;

use anyhow::{Context, Result};

use super::deploy;
use super::lock::Lock;
use super::secrets as server_secrets;
use super::state::{self, Status};
use super::state_file::StateFile;
use crate::app::{self, backup::output, instance, restore::open_archive};
use crate::backup::profile::{self, Flags};
use crate::backup::retention::{self, Store};
use crate::backup::{
    self, Backup, Destination, ItemReader, ItemWriter, Manifest, Role, Scope, SharedRef,
};
use crate::config::server::ServerConfig;
use crate::connector::{Connector, shell_quote};
use crate::error::{CompostError, config_err};
use crate::shared::{self, ServerCtx, SharedAppConfig, connectors_dir};

const CONFIG_ITEM: &str = "server/compost-server.yaml";
const CONNECTORS_ITEM: &str = "server/connectors.tar";
/// The shared apps' overlay stored by `server deploy --secret`, if any.
const SECRETS_ITEM: &str = "server/secrets.yaml";

pub struct BackupOptions {
    /// Only these shared apps (default: all).
    pub shared: Vec<String>,
    /// `--profile`, `--format` and `--target`.
    pub flags: Flags,
    pub output: Option<PathBuf>,
    pub force_unlock: bool,
}

pub struct RestoreOptions {
    pub archive: String,
    pub source: Option<String>,
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
    let flags = &options.flags;
    let run = profile::server_run(config.backup.as_ref(), flags)
        .ok_or_else(|| profile::not_for_server(flags.profile.as_deref().unwrap_or_default()))?;
    let (output, sink) = output(
        &ctx,
        &config,
        run.target.as_deref(),
        options.output.as_ref(),
    )?;

    let _lock = Lock::acquire(connector, &ctx.home, options.force_unlock)?;
    let mut manifest = Manifest::new(Scope::Server, server_name);
    manifest.profile = flags.profile.clone();
    let mut backup = Backup::new(connector, run.format, manifest, output)?;
    match &flags.profile {
        Some(profile) => out!("Backing up {server_name} (profile {profile}):"),
        None => out!("Backing up {server_name}:"),
    }
    let stored = ctx
        .read(super::SERVER_CONFIG_FILE)?
        .context("the server has no compost-server.yaml")?;
    backup.file(CONFIG_ITEM, Role::Meta, stored.as_bytes())?;
    if let Some(secrets) = ctx.read(server_secrets::PATH)? {
        backup.file(SECRETS_ITEM, Role::Meta, secrets.as_bytes())?;
    }

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

    // A profile's archives hold every shared app: retention compares them by name.
    let owner = match selected.as_slice() {
        [(_, one)] if !options.shared.is_empty() => format!("{server_name}-{}", one.name),
        _ => server_name.to_string(),
    };
    let stem = match &flags.profile {
        Some(profile) => format!("{owner}-{profile}-{}", backup::timestamp()),
        None => format!("{owner}-{}", backup::timestamp()),
    };
    let location = backup.finish(&stem)?;
    let (location, store) = sink.deliver(location)?;
    out!("Backup written to {location}");
    out!("It holds secrets (passwords of the shared apps and their links): keep it safe.");
    if let (Some(profile), Some(keep), true) = (&flags.profile, run.keep, options.shared.is_empty())
    {
        retention::apply(&store, &owner, profile, keep)?;
    }
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
    let archive = open_archive(&ctx, &current, options.source.as_deref(), &options.archive)?;
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
        if archive.has(SECRETS_ITEM) {
            if ctx.read(server_secrets::PATH)?.is_some() {
                out!("  kept the stored secrets of the shared apps already on the server");
            } else {
                let path = server_secrets::store(&ctx, &archive.read(SECRETS_ITEM)?)?;
                out!("  stored secrets in {path}");
            }
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
        diff: false,
        force_unlock: false,
        pull: false,
        recreate: false,
        secrets: None,
        shared: &[],
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
        config.backup.clone_from(&archived.backup);
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
        let _ = write!(
            script,
            " && if [ -d {staging}/{dir} ]; then mkdir -p \"$(dirname {dir})\" && rm -rf {dir} && mv {staging}/{dir} {dir}; fi"
        );
    }
    let _ = write!(script, " && rm -rf {staging}");
    archive.feed(CONNECTORS_ITEM, b"", &script)
}

/// `compost backup list`: archives at the server's backup folder or another target there.
pub fn list(
    connector: &dyn Connector,
    server_home: &str,
    manager_key: &str,
    source: Option<&str>,
) -> Result<()> {
    let ctx = app::state::initialized_server(connector, server_home, manager_key)?;
    let config = instance::server_config(&ctx)?;
    let destination = backup::resolve_target(&ctx, config.backup.as_ref(), source)?;
    if destination == Destination::Local {
        return Err(config_err(
            "backups go to this machine (target local); pass --source server or a name from backup.targets",
        ));
    }
    let store = destination.store(connector);
    let rows = store.list()?;
    if rows.is_empty() {
        out!("No backups in {store}");
        return Ok(());
    }
    out!("{store}:");
    let width = rows.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
    for (name, size) in rows {
        out!("  {name:<width$}  {}", backup::human_size(size));
    }
    Ok(())
}

/// `compost backup run`: back up the shared apps (when the server defines the
/// profile for them) and every instance the profile applies to, each into its
/// own archive. Carries on after a failure and reports all of them.
pub fn run_profile(
    connector: &dyn Connector,
    server_name: &str,
    server_home: &str,
    manager_key: &str,
    profile: &str,
    force_unlock: bool,
) -> Result<()> {
    let ctx = app::state::initialized_server(connector, server_home, manager_key)?;
    let config = instance::server_config(&ctx)?;
    let settings = config.backup.as_ref();
    let records = StateFile::load(&ctx)?.apps;
    let mut apps = Vec::new();
    for record in &records {
        if let Some(found) = instance::load(&ctx, &record.name, &record.prefix)? {
            apps.push(found.app);
        }
    }
    let app_refs: Vec<_> = apps.iter().collect();
    if !profile::defined(settings, &app_refs, profile) {
        return Err(profile::unknown_profile(profile));
    }
    let flags = Flags {
        profile: Some(profile.to_string()),
        ..Flags::default()
    };

    let mut failed = Vec::new();
    let server_applies = profile::server_run(settings, &flags).is_some()
        && settings.is_some_and(|s| s.profiles.contains_key(profile))
        && config.shared_apps().next().is_some();
    if server_applies {
        let options = BackupOptions {
            shared: Vec::new(),
            flags: flags.clone(),
            output: None,
            force_unlock,
        };
        if let Err(err) = backup(connector, server_name, server_home, manager_key, &options) {
            eprintln!("error: shared apps: {err:#}");
            failed.push("the shared apps".to_string());
        }
    }
    if !records.is_empty() {
        let options = app::backup::Options {
            flags,
            output: None,
            stop: false,
            force_unlock,
        };
        let which = app::backup::Which::All;
        if let Err(err) = app::backup::backup(
            connector,
            server_name,
            server_home,
            manager_key,
            &which,
            &options,
        ) {
            eprintln!("error: {err:#}");
            failed.push("applications".to_string());
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "backup run {profile} failed for {}",
            failed.join(" and ")
        ))
    }
}

pub struct PruneOptions {
    /// Only this profile's archives (default: every profile with `keep`).
    pub profile: Option<String>,
    /// Look at this target instead of each profile's.
    pub target: Option<String>,
    /// Keep this many instead of each profile's `keep`.
    pub keep: Option<u32>,
    pub yes: bool,
}

/// `compost backup prune`: delete the archives of profiles beyond their `keep`.
pub fn prune(
    connector: &dyn Connector,
    server_name: &str,
    server_home: &str,
    manager_key: &str,
    options: &PruneOptions,
) -> Result<()> {
    let ctx = app::state::initialized_server(connector, server_home, manager_key)?;
    let config = instance::server_config(&ctx)?;
    let settings = config.backup.as_ref();
    let mut apps = Vec::new();
    for record in StateFile::load(&ctx)?.apps {
        if let Some(found) = instance::load(&ctx, &record.name, &record.prefix)? {
            apps.push(found.app);
        }
    }
    let empty = std::collections::BTreeMap::new();
    let server_profiles = settings.map_or(&empty, |s| &s.profiles);
    let mut names: Vec<&str> = server_profiles
        .keys()
        .chain(apps.iter().flat_map(|a| a.backup.profiles.keys()))
        .map(String::as_str)
        .collect();
    names.sort_unstable();
    names.dedup();
    if let Some(profile) = &options.profile {
        if !names.contains(&profile.as_str()) {
            return Err(profile::unknown_profile(profile));
        }
        names.retain(|name| name == profile);
    }

    // (target, owner, profile, keep) of every owner's archives that have a `keep`.
    let mut groups: Vec<(Option<String>, String, &str, u32)> = Vec::new();
    for name in names {
        let flags = Flags {
            profile: Some(name.to_string()),
            ..Flags::default()
        };
        if server_profiles.contains_key(name) {
            if let Some(run) = profile::server_run(settings, &flags) {
                if let Some(keep) = options.keep.or(run.keep) {
                    let target = options.target.clone().or(run.target);
                    groups.push((target, server_name.to_string(), name, keep));
                }
            }
        }
        for app_config in &apps {
            if let Some(run) = profile::app_run(settings, app_config, &flags) {
                if let Some(keep) = options.keep.or(run.keep) {
                    let target = options.target.clone().or(run.target);
                    groups.push((target, app_config.instance(), name, keep));
                }
            }
        }
    }
    if groups.is_empty() {
        out!("No profile sets `keep` (pass --keep to choose how many to keep)");
        return Ok(());
    }

    // List each folder once.
    let mut stores: Vec<(Store, Vec<(String, u64)>)> = Vec::new();
    let mut doomed: Vec<(usize, String, u64)> = Vec::new();
    for (target, owner, name, keep) in &groups {
        let store = backup::resolve_target(&ctx, settings, target.as_deref())?.store(connector);
        let index = match stores
            .iter()
            .position(|(s, _)| s.to_string() == store.to_string())
        {
            Some(index) => index,
            None => {
                let rows = store.list()?;
                stores.push((store, rows));
                stores.len() - 1
            }
        };
        let rows = &stores[index].1;
        let prefix = retention::prefix(owner, name);
        for expired in retention::expired(rows.iter().map(|(n, _)| n.as_str()), &prefix, *keep) {
            let size = rows
                .iter()
                .find(|(n, _)| n == expired)
                .map_or(0, |(_, s)| *s);
            doomed.push((index, expired.to_string(), size));
        }
    }
    doomed.sort();
    doomed.dedup();
    if doomed.is_empty() {
        out!("Nothing to delete: no profile has more archives than it keeps.");
        return Ok(());
    }
    let mut last = None;
    for (index, name, size) in &doomed {
        if last != Some(*index) {
            out!("{}:", stores[*index].0);
            last = Some(*index);
        }
        out!("  delete {name}  {}", backup::human_size(*size));
    }
    if !options.yes && !app::confirm()? {
        out!("Cancelled.");
        return Ok(());
    }
    for (index, name, _) in &doomed {
        stores[*index].0.delete(name)?;
    }
    match doomed.len() {
        1 => out!("Deleted 1 archive."),
        n => out!("Deleted {n} archives."),
    }
    Ok(())
}
