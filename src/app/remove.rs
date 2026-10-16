//! `compost app remove` (spec §10.10).

use anyhow::{Context, Result};

use super::instance::{self, PinnedLink, Selector};
use super::state::{initialized_server, not_deployed};
use crate::config::app::AppConfig;
use crate::connector::{Connector, shell_quote};
use crate::error::config_err;
use crate::server::lock::Lock;
use crate::server::state_file::StateFile;
use crate::shared::{self, AppCtx, ServerCtx};

pub struct Options {
    /// Also remove the compose project's volumes.
    pub volumes: bool,
    /// Kinds whose data of this instance is deleted, e.g. `database`.
    pub purge: Vec<String>,
    /// Do not ask for confirmation.
    pub yes: bool,
    pub force_unlock: bool,
}

pub fn remove(
    connector: &dyn Connector,
    server_home: &str,
    manager_key: &str,
    selector: &Selector,
    options: &Options,
) -> Result<()> {
    check_purge(&options.purge)?;
    let ctx = initialized_server(connector, server_home, manager_key)?;
    let (name, prefix) = instance::resolve(&ctx, selector)?;
    let (name, prefix) = (name.as_str(), prefix.as_str());
    let Some(instance) = instance::load(&ctx, name, prefix)? else {
        return Err(not_deployed(name, prefix));
    };
    let project = instance.app.instance();

    out!("Removing {project}:");
    out!(
        "  containers and the folder {}",
        ctx.path(&instance::dir(name, prefix))
    );
    if options.volumes {
        out!("  volumes of the compose project");
    }
    for link in instance.links.iter().chain(&instance.removing) {
        let purged = if options.purge.contains(&link.kind) {
            ", and delete its data"
        } else {
            ""
        };
        out!(
            "  link {} {} to {}{purged}",
            link.kind,
            link.entry,
            link.instance
        );
    }
    if !options.yes && !super::confirm()? {
        out!("Cancelled.");
        return Ok(());
    }

    let _lock = Lock::acquire(connector, &ctx.home, options.force_unlock)?;
    let links: Vec<PinnedLink> = instance
        .links
        .iter()
        .chain(&instance.removing)
        .cloned()
        .collect();
    remove_links(&ctx, &instance.app, &links, &options.purge, false)?;

    let volumes = if options.volumes { " -v" } else { "" };
    connector
        .exec(&format!(
            "{} down --remove-orphans{volumes}",
            instance::compose_command(&ctx, &instance.app)
        ))
        .context("docker compose down failed")?;

    let dir = ctx.path(&instance::dir(name, prefix));
    let app_dir = ctx.path(&format!("apps/{name}"));
    // The app folder goes too, once its last instance is removed.
    connector.exec(&format!(
        "rm -rf -- {} && rmdir -- {} 2>/dev/null || true",
        shell_quote(&dir),
        shell_quote(&app_dir)
    ))?;

    let mut state_file = StateFile::load(&ctx)?;
    state_file.remove_app(name, prefix);
    state_file.save(&ctx)?;
    out!("Removed {project}.");
    Ok(())
}

/// `--purge` values must be kinds that support it (spec §10.10).
pub fn check_purge(kinds: &[String]) -> Result<()> {
    for name in kinds {
        match shared::find_kind(name) {
            Some(kind) if kind.supports_purge() => {}
            Some(_) => {
                return Err(config_err(format!(
                    "--purge {name}: {name} links have no data to purge"
                )));
            }
            None => {
                return Err(config_err(format!(
                    "--purge {name}: unknown kind (known: {})",
                    shared::kind_names().join(", ")
                )));
            }
        }
    }
    Ok(())
}

/// Undo links in reverse deploy order: ingress first, so traffic stops before
/// anything else goes away. Returns the links that could not be removed
/// because their shared app is no longer configured.
pub fn remove_links(
    ctx: &ServerCtx,
    app: &AppConfig,
    links: &[PinnedLink],
    purge: &[String],
    report: bool,
) -> Result<Vec<PinnedLink>> {
    let server_config = instance::server_config(ctx)?;
    let mut sorted: Vec<_> = links
        .iter()
        .filter_map(|link| Some((shared::find_kind(&link.kind)?, link)))
        .collect();
    sorted.sort_by_key(|(kind, _)| std::cmp::Reverse(kind.apply_order()));

    let mut kept = Vec::new();
    for (kind, link) in sorted {
        let Some(shared) = link.shared_app(&server_config) else {
            eprintln!(
                "warning: {} {} is no longer configured; link {} left as it is",
                link.kind, link.instance, link.entry
            );
            kept.push(link.clone());
            continue;
        };
        let purge = purge.contains(&link.kind);
        if report {
            let data = if purge { ", and its data" } else { "" };
            out!(
                "  removing link {} {} ({}){data}",
                link.kind,
                link.entry,
                link.instance
            );
        }
        let link_ctx = AppCtx {
            server: ctx,
            app,
            shared: &shared,
            peers: &[],
        };
        kind.remove_application(&link_ctx, &link.entry, purge)
            .with_context(|| format!("removing link {} {}", link.kind, link.entry))?;
    }
    Ok(kept)
}
