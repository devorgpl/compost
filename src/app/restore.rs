//! `compost app restore` (spec §10.14). An archive with sources is deployed
//! again, possibly as a copy under another prefix; its data is loaded before
//! the services start. Without sources, data is loaded into an instance that
//! is already deployed.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::backup::{
    Component, check_link_filters, images_item, link_selected, links_dir, sources_item, volume_item,
};
use super::instance::{self, Instance};
use super::state::{initialized_server, not_deployed, stored_compose_file};
use super::{deploy, secrets, steps, volumes};
use crate::backup::{self, Archive, InstanceRef, ItemReader, Manifest, Scope};
use crate::config::app::{AppConfig, ComposeFile};
use crate::config::server::{BackupTarget, ServerConfig};
use crate::connector::{Connector, shell_quote};
use crate::error::{CompostError, config_err};
use crate::server::compose::wait_until_ready;
use crate::server::lock::Lock;
use crate::shared::{self, AppCtx, Kind, ServerCtx};

/// Name of the archived config next to the unpacked sources. Files named
/// `.compost-*.yaml` are never uploaded.
const RESTORED_CONFIG: &str = ".compost-restore.yaml";

pub struct Options {
    pub archive: String,
    pub source: Option<BackupTarget>,
    /// `<app>-<prefix>` of the instance in the archive.
    pub instance: Option<String>,
    pub all: bool,
    /// Restore under this prefix instead of the archived one.
    pub prefix: Option<String>,
    /// Overlays merged over the archived config.
    pub files: Vec<PathBuf>,
    pub include: Vec<Component>,
    pub links: Vec<String>,
    pub replace: bool,
    pub yes: bool,
    pub force_unlock: bool,
}

pub fn restore(
    connector: &dyn Connector,
    server_home: &str,
    manager_key: &str,
    options: &Options,
) -> Result<()> {
    let ctx = initialized_server(connector, server_home, manager_key)?;
    let server_config = instance::server_config(&ctx)?;
    let archive = open_archive(&ctx, &server_config, options.source, &options.archive)?;
    if archive.manifest.scope == Scope::Server {
        return Err(config_err(format!(
            "{} is a server backup; restore it with `compost server restore`",
            options.archive
        )));
    }
    let selected = select(&archive.manifest, options.instance.as_deref(), options.all)?;
    if selected.len() > 1 && (options.prefix.is_some() || !options.files.is_empty()) {
        return Err(config_err(
            "--prefix and -f apply to one instance; choose it with --instance",
        ));
    }
    for id in &selected {
        restore_instance(&ctx, server_home, manager_key, &archive, id, options)?;
    }
    Ok(())
}

/// Open an archive from the `--source` (or the server's `backup.target`).
/// With the server source, a relative path is a file in the server's backup folder.
pub fn open_archive<'c>(
    ctx: &ServerCtx<'c>,
    server_config: &ServerConfig,
    source: Option<BackupTarget>,
    path: &str,
) -> Result<Archive<'c>> {
    let settings = server_config.backup.as_ref();
    if backup::resolve_target(source, settings)? == BackupTarget::Server {
        let path = if path.starts_with('/') {
            path.to_string()
        } else {
            format!("{}/{path}", backup::server_dir(ctx, settings))
        };
        out!("Unpacking {path} on the server ...");
        Archive::open_server(ctx.connector, &path)
    } else {
        out!("Reading {path} ...");
        Archive::open_local(ctx.connector, Path::new(path))
    }
}

/// The instances to restore: the one named, all with `--all`, or the only one.
fn select(manifest: &Manifest, instance: Option<&str>, all: bool) -> Result<Vec<InstanceRef>> {
    let names = || {
        manifest
            .instances
            .iter()
            .map(InstanceRef::instance)
            .collect::<Vec<_>>()
            .join(", ")
    };
    match (instance, all, manifest.instances.as_slice()) {
        (Some(name), _, instances) => instances
            .iter()
            .find(|i| i.instance() == name)
            .map(|i| vec![i.clone()])
            .ok_or_else(|| {
                config_err(format!(
                    "the archive has no instance `{name}` (it has {})",
                    names()
                ))
            }),
        (None, true, instances) => Ok(instances.to_vec()),
        (None, false, [one]) => Ok(vec![one.clone()]),
        (None, false, _) => Err(config_err(format!(
            "the archive has several instances ({}); choose with --instance or use --all",
            names()
        ))),
    }
}

/// Without `--include`: everything in the archive, or only links with `--link`.
fn components(options: &Options) -> Vec<Component> {
    if options.include.is_empty() && options.links.is_empty() {
        return vec![
            Component::Links,
            Component::Sources,
            Component::Volumes,
            Component::Images,
        ];
    }
    super::backup::components(&options.include, &options.links)
}

fn restore_instance(
    ctx: &ServerCtx,
    server_home: &str,
    manager_key: &str,
    archive: &Archive,
    id: &InstanceRef,
    options: &Options,
) -> Result<()> {
    let components = components(options);
    let dir = id.dir();
    let has_sources = archive.has(&sources_item(&dir));
    if options.include.contains(&Component::Sources) && !has_sources {
        return Err(config_err(format!(
            "the archive has no sources of {}; leave out `sources` to load data into the deployed instance",
            id.instance()
        )));
    }
    let plan = RestorePlan {
        archive,
        dir,
        old_instance: id.instance(),
        components: components.clone(),
        links: options.links.clone(),
        replace: options.replace,
    };
    if components.contains(&Component::Sources) && has_sources {
        restore_deployment(ctx, server_home, manager_key, &plan, options)
    } else {
        restore_data(ctx, &plan, id, options)
    }
}

/// Deploy the archived sources and config (plus overlays), loading the data
/// before the services start.
fn restore_deployment(
    ctx: &ServerCtx,
    server_home: &str,
    manager_key: &str,
    plan: &RestorePlan,
    options: &Options,
) -> Result<()> {
    let work = tempfile::Builder::new()
        .prefix("compost-restore-")
        .tempdir()?;
    let sources = work.path().join("sources");
    let sources_tar = work.path().join("sources.tar");
    plan.archive.fetch(&sources_item(&plan.dir), &sources_tar)?;
    tar::Archive::new(fs::File::open(&sources_tar)?)
        .unpack(&sources)
        .context("cannot unpack the archived sources")?;
    let config = sources.join(RESTORED_CONFIG);
    fs::write(
        &config,
        plan.archive
            .read(&format!("{}/{}", plan.dir, instance::CONFIG_FILE))?,
    )?;
    let mut files = vec![config];
    files.extend(options.files.iter().cloned());
    let loaded = AppConfig::load(&files, options.prefix.as_deref())?;

    // Stored secrets: the target's own, else the archived ones, which are
    // stored for the target once it is deployed.
    let (name, prefix) = (&loaded.config.name, &loaded.config.prefix);
    let mut restored_secrets = None;
    let stored = if let Some((path, value)) = secrets::read_in(ctx, name, prefix)? {
        out!("  using stored secrets {path}");
        Some(value)
    } else {
        let item = format!("{}/{}", plan.dir, secrets::ARCHIVE_ITEM);
        if plan.archive.has(&item) {
            let overlay = secrets::prepare(&plan.archive.read(&item)?, Some(prefix))?;
            if overlay.name != *name {
                bail!("the archived secrets are not for app `{name}`");
            }
            let value = serde_yaml_ng::from_str(&overlay.text)?;
            restored_secrets = Some(overlay);
            Some(value)
        } else {
            None
        }
    };
    let loaded = match stored {
        Some(stored) => AppConfig::load_with(&files, Some(stored), options.prefix.as_deref())?,
        None => loaded,
    };

    let target = loaded.config.instance();
    if target == plan.old_instance {
        out!("Restoring {target}");
    } else {
        out!("Restoring {} as {target}", plan.old_instance);
    }
    plan.check_volumes(ctx, &loaded.config, &loaded.compose)?;
    if options.replace && !options.yes {
        out!("Existing data of {target} is replaced.");
        if !super::confirm()? {
            out!("Cancelled.");
            return Ok(());
        }
    }
    if !loaded.config.backup.before_restore.is_empty() {
        out!(
            "  before_restore: skipped, {target} is deployed again and starts after its data is loaded"
        );
    }
    let deploy_options = deploy::Options {
        apply: true,
        dry_run: false,
        force_unlock: options.force_unlock,
        pull: false,
        recreate: false,
        restore: Some(plan),
        purge: Vec::new(),
        secrets: None,
    };
    deploy::deploy(
        ctx.connector,
        server_home,
        manager_key,
        &loaded,
        &files,
        &deploy_options,
    )?;

    let after = &loaded.config.backup.after_restore;
    if restored_secrets.is_some() || !after.is_empty() {
        let _lock = Lock::acquire(ctx.connector, &ctx.home, options.force_unlock)?;
        if let Some(overlay) = restored_secrets {
            let path = secrets::store(ctx, &overlay)?;
            out!("Restored the stored secrets of {target} to {path}");
        }
        steps::run(ctx, &loaded.config, "after_restore", after)?;
    }
    Ok(())
}

/// Load data into an instance that is deployed, with its services stopped.
fn restore_data(
    ctx: &ServerCtx,
    plan: &RestorePlan,
    id: &InstanceRef,
    options: &Options,
) -> Result<()> {
    let prefix = options.prefix.as_deref().unwrap_or(&id.prefix);
    let instance =
        instance::load(ctx, &id.name, prefix)?.ok_or_else(|| not_deployed(&id.name, prefix))?;
    check_link_filters(std::slice::from_ref(&instance), &options.links)?;
    let app = &instance.app;
    let compose = stored_compose_file(ctx, &instance)?;
    plan.check_volumes(ctx, app, &compose)?;

    let items = plan.summary(&instance);
    if items.is_empty() {
        bail!(
            "the archive has nothing to restore into {} for the selected parts",
            app.instance()
        );
    }
    out!(
        "Restoring into {} (its services are stopped meanwhile):",
        app.instance()
    );
    for item in &items {
        out!("  {item}");
    }
    if !options.yes && !super::confirm()? {
        out!("Cancelled.");
        return Ok(());
    }

    let _lock = Lock::acquire(ctx.connector, &ctx.home, options.force_unlock)?;
    steps::run(ctx, app, "before_restore", &app.backup.before_restore)?;
    let stopped = volumes::Stopped::stop(ctx, app, volumes::Services::All)?;
    let server_config = instance::server_config(ctx)?;
    for link in &instance.links {
        let (Some(kind), Some(shared_app)) = (
            shared::find_kind(&link.kind),
            link.shared_app(&server_config),
        ) else {
            continue;
        };
        let link_ctx = AppCtx {
            server: ctx,
            app,
            shared: &shared_app,
            peers: &[],
        };
        plan.restore_link(&link_ctx, kind, &link.entry)
            .with_context(|| format!("link {} {}", link.kind, link.entry))?;
    }
    let images = plan.load_images(ctx, app)?;
    plan.restore_volumes(ctx, app, &compose, false)?;
    drop(stopped);
    if images {
        // Containers keep their old image until they are recreated.
        ctx.connector
            .exec(&format!("{} up -d", instance::compose_command(ctx, app)))?;
    }
    if !app.backup.after_restore.is_empty() {
        wait_until_ready(ctx.connector, &app.instance())?;
        steps::run(ctx, app, "after_restore", &app.backup.after_restore)?;
    }
    out!("Restored {}.", app.instance());
    Ok(())
}

/// What to load from an archive into one instance. Also used by deploy.
pub struct RestorePlan<'a, 'c> {
    archive: &'a Archive<'c>,
    /// Folder of the archived instance in the archive.
    dir: String,
    old_instance: String,
    components: Vec<Component>,
    links: Vec<String>,
    replace: bool,
}

impl RestorePlan<'_, '_> {
    /// Load a link's data, if it is selected.
    pub fn restore_link(&self, ctx: &AppCtx, kind: &dyn Kind, entry: &str) -> Result<()> {
        if !self.components.contains(&Component::Links)
            || !link_selected(&self.links, kind.name(), entry)
        {
            return Ok(());
        }
        let reader = ItemReader::new(self.archive, links_dir(&self.dir, kind.name()));
        kind.restore_link(ctx, entry, &reader, self.replace)
    }

    /// `docker load` the archived images; returns whether there were any.
    /// Images built for the archived instance are tagged for this one.
    pub fn load_images(&self, ctx: &ServerCtx, app: &AppConfig) -> Result<bool> {
        let item = images_item(&self.dir);
        if !self.components.contains(&Component::Images) || !self.archive.has(&item) {
            return Ok(false);
        }
        out!("  loading images ...");
        self.archive.feed(&item, b"", "docker load -q")?;
        let new = app.instance();
        if new != self.old_instance {
            let compose = instance::compose_command(ctx, app);
            ctx.connector.exec(&format!(
                "{compose} config --images | sort -u | while read -r image; do \
                 case \"$image\" in {new}-*) old={old}-\"${{image#{new}-}}\"; \
                 if docker image inspect \"$old\" >/dev/null 2>&1; then docker tag \"$old\" \"$image\"; fi;; \
                 esac; done",
                new = shell_quote(&new),
                old = shell_quote(&self.old_instance),
            ))?;
        }
        Ok(true)
    }

    /// Declared volumes the archive has data for.
    fn volumes<'v>(&self, app: &'v AppConfig) -> Vec<&'v String> {
        if !self.components.contains(&Component::Volumes) {
            return Vec::new();
        }
        app.backup
            .volumes
            .iter()
            .filter(|v| self.archive.has(&volume_item(&self.dir, v)))
            .collect()
    }

    /// Volumes holding data are only replaced with `--replace`.
    fn check_volumes(&self, ctx: &ServerCtx, app: &AppConfig, compose: &ComposeFile) -> Result<()> {
        if self.replace {
            return Ok(());
        }
        for volume in self.volumes(app) {
            let name = app.docker_volume(compose, volume);
            if volumes::exists(ctx.connector, &name)? && !volumes::is_empty(ctx.connector, &name)? {
                return Err(CompostError::ServerNotReady(format!(
                    "volume {name} is not empty; add --replace to overwrite it"
                ))
                .into());
            }
        }
        Ok(())
    }

    /// Replace the contents of the volumes. With `create`, compose creates
    /// the containers and volumes first (a new deployment).
    pub fn restore_volumes(
        &self,
        ctx: &ServerCtx,
        app: &AppConfig,
        compose: &ComposeFile,
        create: bool,
    ) -> Result<()> {
        let selected = self.volumes(app);
        if selected.is_empty() {
            return Ok(());
        }
        if create {
            ctx.connector
                .exec(&format!("{} create", instance::compose_command(ctx, app)))?;
        }
        for volume in selected {
            let name = app.docker_volume(compose, volume);
            out!("  restoring volume {name} ...");
            self.archive.feed(
                &volume_item(&self.dir, volume),
                b"",
                &volumes::restore_script(&name),
            )?;
        }
        Ok(())
    }

    /// Archive items that a data restore loads into the instance.
    fn summary(&self, instance: &Instance) -> Vec<String> {
        let mut items = Vec::new();
        if self.components.contains(&Component::Links) {
            for link in &instance.links {
                let dir = links_dir(&self.dir, &link.kind);
                let saved = self
                    .archive
                    .manifest
                    .items_under(&dir)
                    .any(|i| i.path.starts_with(&format!("{dir}/{}.", link.entry)));
                if saved && link_selected(&self.links, &link.kind, &link.entry) {
                    items.push(format!("link {} {}", link.kind, link.entry));
                }
            }
        }
        for volume in self.volumes(&instance.app) {
            items.push(format!("volume {volume} (replaced)"));
        }
        if self.components.contains(&Component::Images) && self.archive.has(&images_item(&self.dir))
        {
            items.push("images".to_string());
        }
        items
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(instances: &[(&str, &str)]) -> Manifest {
        let mut manifest = Manifest::new(Scope::Apps, "s");
        for (name, prefix) in instances {
            manifest.instances.push(InstanceRef {
                name: name.to_string(),
                prefix: prefix.to_string(),
            });
        }
        manifest
    }

    #[test]
    fn selects_instances() {
        let one = manifest(&[("a", "p")]);
        assert_eq!(select(&one, None, false).unwrap()[0].instance(), "a-p");

        let two = manifest(&[("a", "p"), ("b", "q")]);
        let err = select(&two, None, false).unwrap_err().to_string();
        assert!(err.contains("several instances (a-p, b-q)"), "{err}");
        assert_eq!(select(&two, Some("b-q"), false).unwrap()[0].name, "b");
        assert_eq!(select(&two, None, true).unwrap().len(), 2);
        assert!(select(&two, Some("c-r"), false).is_err());
    }
}
