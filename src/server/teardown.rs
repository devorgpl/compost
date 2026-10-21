//! `compost dev reset-all`: remove everything compost runs on a server and
//! its folder, so that the next deploy starts from nothing.

use anyhow::{Context, Result, bail};

use super::lock::Lock;
use super::schedule;
use super::state::{self, Status};
use super::state_file::StateFile;
use crate::app::{instance, release};
use crate::connector::{Connector, shell_quote};
use crate::error::CompostError;
use crate::shared::{self, ServerCtx, project_name, shared_dir};

/// Deletes the folder's files that belong to other users (e.g. a database's
/// data) as root, in a container.
pub const CLEANUP_IMAGE: &str = "alpine:3.24";

pub struct Options {
    pub yes: bool,
    pub force_unlock: bool,
}

/// A compose project to take down.
struct Project {
    what: String,
    /// `docker compose` with the project's name and files.
    compose: String,
    /// An instance: the images compose built for it and its rollback images go too.
    instance: Option<String>,
}

/// Remove every instance and shared app with their data, compost's networks,
/// its crontab line and its folder. Returns `false` when the user cancelled
/// or there was nothing to remove.
pub fn teardown(
    connector: &dyn Connector,
    server_name: &str,
    server_home: &str,
    manager_key: &str,
    options: &Options,
) -> Result<bool> {
    let report = state::check(connector, server_home, manager_key)?;
    let home = match (report.status, report.home) {
        (Status::Uninitialized, Some(home)) => {
            out!("Nothing to remove: compost has set up nothing in {home} on {server_name}.");
            return Ok(false);
        }
        (Status::Ok | Status::Degraded, Some(home)) => home,
        (status, _) => {
            return Err(CompostError::ServerNotReady(format!(
                "server is {status}: {}",
                report.detail.unwrap_or_default()
            ))
            .into());
        }
    };
    let user_home = connector.resolve_path("~")?;
    if home == "/" || home.trim_end_matches('/') == user_home.trim_end_matches('/') {
        bail!("compost's folder on {server_name} is {home}; not deleting it");
    }
    let ctx = ServerCtx { connector, home };
    let projects = projects(&ctx)?;
    let archives = connector
        .list_dir(&ctx.path("backups"))?
        .into_iter()
        .filter(|f| !f.starts_with('.'))
        .count();

    out!("Removing everything compost runs on {server_name}, with its data:");
    for project in &projects {
        out!("  {}", project.what);
    }
    out!("  compost's Docker networks and crontab line");
    out!(
        "  the folder {} (every database, bucket, realm and secret)",
        ctx.home
    );
    if archives > 0 {
        out!("  and the {archives} backup archives in it");
    }
    out!("Your servers and manager key in ~/.compost stay.");
    if !options.yes && !crate::app::confirm()? {
        out!("Cancelled.");
        return Ok(false);
    }

    let _lock = Lock::acquire(connector, &ctx.home, options.force_unlock)?;
    let mut failed = Vec::new();
    for project in &projects {
        let images = if project.instance.is_some() {
            " --rmi local"
        } else {
            ""
        };
        let down = format!("{} down --remove-orphans -v{images}", project.compose);
        if let Err(err) = connector.exec(&down) {
            failed.push(format!("{}: {err:#}", project.what));
            continue;
        }
        if let Some(instance) = &project.instance {
            if let Err(err) = release::remove_images(&ctx, instance, None) {
                eprintln!("warning: rollback images of {instance}: {err:#}");
            }
        }
        out!("  removed {}", project.what);
    }
    if !failed.is_empty() {
        bail!(
            "could not remove everything; the folder {} stays, so run this again once these work:\n  {}",
            ctx.home,
            failed.join("\n  ")
        );
    }
    remove_networks(connector)?;
    if schedule::uninstall(&ctx)? {
        out!("  removed the crontab line");
    }
    remove_folder(connector, &ctx.home)?;
    out!("  removed {}", ctx.home);
    out!("Done. `compost dev up` in a project starts again from nothing.");
    Ok(true)
}

/// The instances first (they use the shared apps), then the shared apps,
/// from `state.yaml` and the stored config.
fn projects(ctx: &ServerCtx) -> Result<Vec<Project>> {
    let state_file = StateFile::load(ctx)?;
    let mut projects = Vec::new();
    for record in &state_file.apps {
        let id = format!("{}-{}", record.name, record.prefix);
        let compose = match instance::load(ctx, &record.name, &record.prefix)? {
            Some(deployed) => instance::compose_command(ctx, &deployed.app),
            None => format!("docker compose -p {}", shell_quote(&id)),
        };
        projects.push(Project {
            what: format!("instance {id}"),
            compose,
            instance: Some(id),
        });
    }

    let config = instance::server_config(ctx)?;
    let mut shared_apps: Vec<(String, String)> = config
        .shared_apps()
        .filter(|(kind, app)| {
            !app.is_external() && shared::find_kind(kind).is_some_and(|k| k.runs_service(app))
        })
        .map(|(kind, app)| (kind.to_string(), app.name.clone()))
        .collect();
    // Orphans: no longer configured, still running.
    for record in &state_file.shared {
        if !config
            .shared_apps()
            .any(|(k, a)| k == record.kind && a.name == record.name)
        {
            shared_apps.push((record.kind.clone(), record.name.clone()));
        }
    }
    for (kind, name) in shared_apps {
        let dir = ctx.path(&shared_dir(&kind, &name));
        projects.push(Project {
            what: format!("shared app {kind} {name}"),
            compose: format!(
                "cd {} 2>/dev/null; docker compose -p {}",
                shell_quote(&dir),
                shell_quote(&project_name(&kind, &name))
            ),
            instance: None,
        });
    }
    Ok(projects)
}

/// compost's networks (`compost-…`) that nothing uses any more.
fn remove_networks(connector: &dyn Connector) -> Result<()> {
    let names = connector.exec("docker network ls --format '{{.Name}}'")?;
    for name in names.lines().filter(|n| n.starts_with("compost-")) {
        let removed = connector
            .run(
                &format!("docker network rm {} >/dev/null", shell_quote(name)),
                None,
            )?
            .success();
        if removed {
            out!("  removed network {name}");
        } else {
            eprintln!("warning: network {name} is still in use; left as it is");
        }
    }
    Ok(())
}

/// Delete the folder; files of other users (a database's data) are deleted as
/// root in a container.
fn remove_folder(connector: &dyn Connector, home: &str) -> Result<()> {
    let quoted = shell_quote(home);
    connector.exec(&format!(
        "rm -rf -- {quoted} 2>/dev/null; [ ! -e {quoted} ] || \
         {{ docker run --rm -v {quoted}:/h {CLEANUP_IMAGE} sh -c 'rm -rf /h/* /h/.[!.]* /h/..?*' && rm -rf -- {quoted}; }}"
    ))
    .with_context(|| format!("cannot delete {home}"))?;
    Ok(())
}
