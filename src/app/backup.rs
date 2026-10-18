//! `compost app backup` (spec §10.13): link data, sources, volumes and images
//! of one or every instance.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use super::instance::{self, Instance, Selector};
use super::state::{initialized_server, not_deployed, stored_compose_file};
use super::{secrets, steps, volumes};
use crate::backup::profile::{self, AppRun, Flags};
use crate::backup::{
    self, Backup, Format, InstanceRef, ItemWriter, Manifest, Output, Role, Scope, Sink, retention,
};
use crate::config::app::StopServices;
use crate::config::server::ServerConfig;
use crate::connector::{Connector, shell_quote};
use crate::error::config_err;
use crate::server::lock::Lock;
use crate::server::state_file::StateFile;
use crate::shared::{self, AppCtx, ServerCtx};

pub use crate::backup::{Component, components};

/// `--link` values: `<entry>` or `<kind>:<entry>`.
pub fn link_selected(filters: &[String], kind: &str, entry: &str) -> bool {
    filters.is_empty()
        || filters.iter().any(|filter| match filter.split_once(':') {
            Some((k, e)) => k == kind && e == entry,
            None => filter == entry,
        })
}

pub enum Which {
    One(Selector),
    All,
}

pub struct Options {
    /// `--profile`, `--include`, `--link`, `--format` and `--target`.
    pub flags: Flags,
    pub output: Option<PathBuf>,
    /// Stop every service while volumes are copied, whatever `backup.stop` says.
    pub stop: bool,
    pub force_unlock: bool,
}

/// Archive paths of an instance's items.
pub fn sources_item(dir: &str) -> String {
    format!("{dir}/sources.tar")
}

pub fn images_item(dir: &str) -> String {
    format!("{dir}/images.tar")
}

pub fn volume_item(dir: &str, volume: &str) -> String {
    format!("{dir}/volumes/{volume}.tar")
}

pub fn links_dir(dir: &str, kind: &str) -> String {
    format!("{dir}/links/{kind}")
}

pub fn backup(
    connector: &dyn Connector,
    server_name: &str,
    server_home: &str,
    manager_key: &str,
    which: &Which,
    options: &Options,
) -> Result<()> {
    let ctx = initialized_server(connector, server_home, manager_key)?;
    let server_config = instance::server_config(&ctx)?;
    let settings = server_config.backup.as_ref();
    let flags = &options.flags;

    let (scope, selected) = match which {
        Which::One(selector) => (Scope::App, vec![instance::resolve(&ctx, selector)?]),
        Which::All => {
            let mut apps: Vec<_> = StateFile::load(&ctx)?
                .apps
                .into_iter()
                .map(|r| (r.name, r.prefix))
                .collect();
            if apps.is_empty() {
                bail!("no applications are deployed on this server");
            }
            apps.sort();
            (Scope::Apps, apps)
        }
    };
    let mut instances = Vec::new();
    for (name, prefix) in &selected {
        let found =
            instance::load(&ctx, name, prefix)?.ok_or_else(|| not_deployed(name, prefix))?;
        instances.push(found);
    }
    check_link_filters(&instances, &flags.links)?;
    if let Some(name) = &flags.profile {
        let apps: Vec<_> = instances.iter().map(|i| &i.app).collect();
        if !profile::defined(settings, &apps, name) {
            return Err(profile::unknown_profile(name));
        }
        if scope == Scope::Apps && options.output.is_some() {
            return Err(config_err(
                "with --all and --profile, every instance gets its own archive: leave out -o",
            ));
        }
    }
    let mut runs = Vec::new();
    for instance in &instances {
        let name = flags.profile.as_deref().unwrap_or_default();
        match profile::app_run(settings, &instance.app, flags) {
            Some(run) => runs.push((instance, run)),
            None if scope == Scope::App => {
                return Err(profile::not_for_app(name, &instance.app.instance()));
            }
            None => out!(
                "Skipping {}: profile {name} is not defined for it, or it opted out",
                instance.app.instance()
            ),
        }
    }

    let _lock = Lock::acquire(connector, &ctx.home, options.force_unlock)?;
    match (scope, &flags.profile) {
        // `--all` without a profile: one archive, at the flag's or the server's target.
        (Scope::Apps, None) => {
            let (output, sink) = output(
                &ctx,
                &server_config,
                flags.target.as_deref(),
                options.output.as_ref(),
            )?;
            let format = flags.format.unwrap_or_default();
            let mut backup =
                Backup::new(connector, format, Manifest::new(scope, server_name), output)?;
            for (instance, run) in &runs {
                out!("Backing up {}:", instance.app.instance());
                backup_instance(
                    &ctx,
                    &server_config,
                    instance,
                    run,
                    options.stop,
                    &mut backup,
                )?;
            }
            let location = backup.finish(&format!("apps-{}", backup::timestamp()))?;
            let (location, _) = sink.deliver(location)?;
            out!("Backup written to {location}");
            Ok(())
        }
        _ => {
            let mut failed = Vec::new();
            for (instance, run) in &runs {
                let written = write_one(
                    &ctx,
                    &server_config,
                    server_name,
                    instance,
                    run,
                    flags.profile.as_deref(),
                    options.output.as_ref(),
                    options.stop,
                );
                match written {
                    Err(err) if runs.len() == 1 => return Err(err),
                    Err(err) => {
                        eprintln!("error: {}: {err:#}", instance.app.instance());
                        failed.push(instance.app.instance());
                    }
                    Ok(()) => {}
                }
            }
            if failed.is_empty() {
                Ok(())
            } else {
                bail!("the backup failed for {}", failed.join(", "))
            }
        }
    }
}

/// Back up one instance into its own archive, then delete the profile's
/// archives beyond its `keep`. The caller holds the server's lock.
#[allow(clippy::too_many_arguments)]
pub fn write_one(
    ctx: &ServerCtx,
    server_config: &ServerConfig,
    server_name: &str,
    instance: &Instance,
    run: &AppRun,
    profile: Option<&str>,
    path: Option<&PathBuf>,
    stop: bool,
) -> Result<()> {
    let (output, sink) = output(ctx, server_config, run.target.as_deref(), path)?;
    let mut manifest = Manifest::new(Scope::App, server_name);
    manifest.profile = profile.map(String::from);
    let mut backup = Backup::new(ctx.connector, run.format, manifest, output)?;
    let owner = instance.app.instance();
    match profile {
        Some(profile) => out!("Backing up {owner} (profile {profile}):"),
        None => out!("Backing up {owner}:"),
    }
    backup_instance(ctx, server_config, instance, run, stop, &mut backup)?;
    let stem = match profile {
        Some(profile) => format!("{owner}-{profile}-{}", backup::timestamp()),
        None => format!("{owner}-{}", backup::timestamp()),
    };
    let location = backup.finish(&stem)?;
    let (location, store) = sink.deliver(location)?;
    out!("Backup written to {location}");
    if let (Some(profile), Some(keep)) = (profile, run.keep) {
        retention::apply(&store, &owner, profile, keep)?;
    }
    Ok(())
}

/// Where the backup goes: the `target` (a flag's or a profile's), else the
/// server's `backup.target`; `path` is the `-o` flag.
pub fn output<'c>(
    ctx: &ServerCtx<'c>,
    server_config: &ServerConfig,
    target: Option<&str>,
    path: Option<&PathBuf>,
) -> Result<(Output, Sink<'c>)> {
    let destination = backup::resolve_target(ctx, server_config.backup.as_ref(), target)?;
    Ok(Sink::new(ctx, destination, path))
}

/// Every `--link` must name a link of at least one selected instance.
pub fn check_link_filters(instances: &[Instance], filters: &[String]) -> Result<()> {
    for filter in filters {
        let one = std::slice::from_ref(filter);
        let found = instances
            .iter()
            .flat_map(|i| &i.links)
            .any(|link| link_selected(one, &link.kind, &link.entry));
        if !found {
            return Err(config_err(format!(
                "--link {filter}: no such link (use <entry> or <kind>:<entry>, see `compost app state`)"
            )));
        }
    }
    Ok(())
}

fn backup_instance(
    ctx: &ServerCtx,
    server_config: &ServerConfig,
    instance: &Instance,
    run: &AppRun,
    stop: bool,
    backup: &mut Backup,
) -> Result<()> {
    let components = &run.include;
    let app = &instance.app;
    let server_dir = instance::dir(&app.name, &app.prefix);
    let id = InstanceRef {
        name: app.name.clone(),
        prefix: app.prefix.clone(),
    };
    let dir = id.dir();
    backup.manifest_mut().instances.push(id);

    // What deploy stored: the restore starts from these.
    for file in [instance::CONFIG_FILE, instance::LINKS_FILE] {
        if let Some(text) = ctx.read(&format!("{server_dir}/{file}"))? {
            backup.file(&format!("{dir}/{file}"), Role::Meta, text.as_bytes())?;
        }
    }
    // The overlay from `secret deploy`, which the effective config lacks.
    // Configuration, so not part of raw output.
    let stored = match run.format {
        Format::Compost => ctx.read(&secrets::path(&app.name, &app.prefix))?,
        Format::Raw => None,
    };
    if let Some(text) = stored {
        backup.file(
            &format!("{dir}/{}", secrets::ARCHIVE_ITEM),
            Role::Meta,
            text.as_bytes(),
        )?;
        out!("  stored secrets (the archive holds them)");
    }

    if components.contains(&Component::Sources) {
        let sources = shell_quote(&ctx.path(&format!("{server_dir}/sources")));
        backup.stream(
            &sources_item(&dir),
            Role::Data,
            &format!("tar -cf - -C {sources} ."),
        )?;
    }

    // The steps prepare the data that links and volumes save.
    if components.contains(&Component::Links) || components.contains(&Component::Volumes) {
        let before = steps::run(ctx, app, "before_backup", &run.before_backup);
        let saved =
            before.and_then(|()| save_data(ctx, server_config, instance, run, stop, backup, &dir));
        steps::run_after(ctx, app, "after_backup", &run.after_backup);
        saved?;
    }

    if components.contains(&Component::Images) {
        // Only images that exist: services that never started may have none.
        let compose = instance::compose_command(ctx, app);
        let script = format!(
            "images=$({compose} config --images | sort -u | while read -r image; do \
             docker image inspect \"$image\" >/dev/null 2>&1 && echo \"$image\"; done); \
             [ -n \"$images\" ] || {{ echo 'no images found' >&2; exit 1; }}; \
             docker save $images"
        );
        backup.stream(&images_item(&dir), Role::Data, &script)?;
    }
    Ok(())
}

/// Link data and volumes.
fn save_data(
    ctx: &ServerCtx,
    server_config: &ServerConfig,
    instance: &Instance,
    run: &AppRun,
    stop: bool,
    backup: &mut Backup,
    dir: &str,
) -> Result<()> {
    let app = &instance.app;
    let components = &run.include;
    if components.contains(&Component::Links) {
        for link in instance
            .links
            .iter()
            .filter(|l| link_selected(&run.links, &l.kind, &l.entry))
        {
            let (Some(kind), Some(shared_app)) = (
                shared::find_kind(&link.kind),
                link.shared_app(server_config),
            ) else {
                eprintln!(
                    "warning: {} {} is no longer configured; link {} skipped",
                    link.kind, link.instance, link.entry
                );
                continue;
            };
            let link_ctx = AppCtx {
                server: ctx,
                app,
                shared: &shared_app,
                peers: &[],
            };
            let mut writer = ItemWriter::new(backup, links_dir(dir, &link.kind));
            kind.backup_link(&link_ctx, &link.entry, &mut writer)
                .with_context(|| format!("link {} {}", link.kind, link.entry))?;
        }
    }

    if components.contains(&Component::Volumes) && !run.volumes.is_empty() {
        let compose = stored_compose_file(ctx, instance)?;
        let stop = match (&run.stop, stop) {
            (_, true) | (Some(StopServices::All(true)), _) => Some(volumes::Services::All),
            (Some(StopServices::Some(services)), _) => Some(volumes::Services::Only(services)),
            _ => None,
        };
        let _stopped = stop
            .map(|which| volumes::Stopped::stop(ctx, app, which))
            .transpose()?;
        for volume in &run.volumes {
            let docker_volume = app.docker_volume(&compose, volume);
            if !volumes::exists(ctx.connector, &docker_volume)? {
                eprintln!("warning: volume {docker_volume} does not exist yet; skipped");
                continue;
            }
            backup.stream(
                &volume_item(dir, volume),
                Role::Data,
                &volumes::backup_script(&docker_volume),
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_filters() {
        let selected = |filter: &str| link_selected(&[filter.into()], "database", "main-db");
        assert!(link_selected(&[], "database", "main-db"));
        assert!(selected("main-db"));
        assert!(selected("database:main-db"));
        assert!(!selected("ingress:main-db"));
        assert!(!selected("site"));
    }

    #[test]
    fn default_components() {
        use Component::*;
        assert_eq!(components(&[], &[]), [Links, Sources, Volumes]);
        assert_eq!(components(&[], &["main-db".into()]), [Links]);
        assert_eq!(components(&[Images], &["main-db".into()]), [Images]);
    }
}
