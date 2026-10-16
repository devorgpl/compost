//! `compost app backup` (spec §10.13): link data, sources, volumes and images
//! of one or every instance.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use super::instance::{self, Instance, Selector};
use super::state::{initialized_server, not_deployed, stored_compose_file};
use super::{secrets, volumes};
use crate::backup::{self, Backup, Format, InstanceRef, ItemWriter, Manifest, Output, Role, Scope};
use crate::config::server::{BackupTarget, ServerConfig};
use crate::connector::{Connector, shell_quote};
use crate::error::config_err;
use crate::server::lock::Lock;
use crate::server::state_file::StateFile;
use crate::shared::{self, AppCtx, ServerCtx};

/// Parts of an instance a backup can hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Component {
    /// Data and configuration of links (database dumps, ingress sites)
    Links,
    /// Uploaded sources (only the compose file when sources are not uploaded)
    Sources,
    /// Named volumes listed under `backup.volumes`
    Volumes,
    /// Docker images of the services, as `docker save` writes them
    Images,
}

/// Without `--include`: everything but images, or only links when `--link` is given.
pub fn components(include: &[Component], links: &[String]) -> Vec<Component> {
    match (include, links) {
        ([], []) => vec![Component::Links, Component::Sources, Component::Volumes],
        ([], _) => vec![Component::Links],
        (include, _) => include.to_vec(),
    }
}

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
    pub include: Vec<Component>,
    pub links: Vec<String>,
    pub format: Format,
    pub target: Option<BackupTarget>,
    pub output: Option<PathBuf>,
    /// Stop the services while volumes are copied.
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
    let output = output(
        &ctx,
        &server_config,
        options.target,
        options.output.as_ref(),
    )?;

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
    check_link_filters(&instances, &options.links)?;
    let components = components(&options.include, &options.links);

    let _lock = Lock::acquire(connector, &ctx.home, options.force_unlock)?;
    let mut backup = Backup::new(
        connector,
        options.format,
        Manifest::new(scope, server_name),
        output,
    )?;
    for instance in &instances {
        out!("Backing up {}:", instance.app.instance());
        backup_instance(
            &ctx,
            &server_config,
            instance,
            &components,
            options,
            &mut backup,
        )?;
    }
    let stem = match scope {
        Scope::App => format!("{}-{}", instances[0].app.instance(), backup::timestamp()),
        _ => format!("apps-{}", backup::timestamp()),
    };
    let location = backup.finish(&stem)?;
    out!("Backup written to {location}");
    Ok(())
}

/// Where the backup goes, from the flags and the server's `backup` settings.
pub fn output(
    ctx: &ServerCtx,
    server_config: &ServerConfig,
    target: Option<BackupTarget>,
    path: Option<&PathBuf>,
) -> Result<Output> {
    let settings = server_config.backup.as_ref();
    Ok(match backup::resolve_target(target, settings)? {
        BackupTarget::Server => Output::Server {
            dir: backup::server_dir(ctx, settings),
            name: path.map(|p| p.display().to_string()),
        },
        _ => Output::Local(path.cloned()),
    })
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
    components: &[Component],
    options: &Options,
    backup: &mut Backup,
) -> Result<()> {
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
    let stored = match options.format {
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

    if components.contains(&Component::Links) {
        for link in instance
            .links
            .iter()
            .filter(|l| link_selected(&options.links, &l.kind, &l.entry))
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
            let mut writer = ItemWriter::new(backup, links_dir(&dir, &link.kind));
            kind.backup_link(&link_ctx, &link.entry, &mut writer)
                .with_context(|| format!("link {} {}", link.kind, link.entry))?;
        }
    }

    if components.contains(&Component::Volumes) && !app.backup.volumes.is_empty() {
        let compose = stored_compose_file(ctx, instance)?;
        let _stopped = if options.stop {
            Some(volumes::Stopped::stop(ctx, app)?)
        } else {
            None
        };
        for volume in &app.backup.volumes {
            let docker_volume = app.docker_volume(&compose, volume);
            if !volumes::exists(ctx.connector, &docker_volume)? {
                eprintln!("warning: volume {docker_volume} does not exist yet; skipped");
                continue;
            }
            backup.stream(
                &volume_item(&dir, volume),
                Role::Data,
                &volumes::backup_script(&docker_volume),
            )?;
        }
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
