//! `compost app state` (spec §10.9).

use anyhow::Result;

use super::instance::{self, Instance, Selector};
use crate::config::app::ComposeFile;
use crate::connector::Connector;
use crate::error::CompostError;
use crate::server::compose;
use crate::server::state::{self as server_state, Status};
use crate::server::state_file::StateFile;
use crate::shared::{self, AppCtx, ServerCtx};

pub fn state(
    connector: &dyn Connector,
    server_home: &str,
    manager_key: &str,
    selector: &Selector,
) -> Result<()> {
    let ctx = initialized_server(connector, server_home, manager_key)?;
    let (name, prefix) = instance::resolve(&ctx, selector)?;
    let Some(instance) = instance::load(&ctx, &name, &prefix)? else {
        return Err(not_deployed(&name, &prefix));
    };
    if print(&ctx, &instance)? {
        Ok(())
    } else {
        Err(CompostError::Unhealthy(format!(
            "{}: some services are not running",
            instance.app.instance()
        ))
        .into())
    }
}

/// A container of an instance.
pub struct ServiceHealth {
    pub name: String,
    pub healthy: bool,
    /// e.g. `running healthy`, `exited 1`.
    pub state: String,
}

/// The instance's containers, with whether each is as it should be.
pub fn services(ctx: &ServerCtx, instance: &Instance) -> Result<Vec<ServiceHealth>> {
    let containers = compose::ps(ctx.connector, &instance.app.instance())?;
    let compose = stored_compose_file(ctx, instance)?;
    Ok(containers
        .iter()
        .map(|container| {
            let keeps_running = compose
                .services
                .get(&container.service)
                .is_some_and(crate::config::app::ComposeService::keeps_running);
            ServiceHealth {
                name: container.service.clone(),
                healthy: container.is_healthy(keeps_running),
                state: container.describe(keeps_running),
            }
        })
        .collect())
}

/// Some containers, all healthy.
pub fn all_healthy(services: &[ServiceHealth]) -> bool {
    !services.is_empty() && services.iter().all(|s| s.healthy)
}

/// Print status, services and links. Returns whether every service is ready.
pub fn print(ctx: &ServerCtx, instance: &Instance) -> Result<bool> {
    let app = &instance.app;
    let state_file = StateFile::load(ctx)?;
    let status = state_file.app(&app.name, &app.prefix).map_or_else(
        || "unknown".to_string(),
        |r| format!("{} ({})", r.status, r.updated_at),
    );
    out!("{}: {status}", app.instance());
    if let Some(release) = super::release::list(ctx, &app.name, &app.prefix)?.last() {
        out!("  release: {} ({})", release.number, release.created_at);
    }
    for record in state_file
        .backups
        .iter()
        .filter(|r| r.owner == app.instance())
    {
        out!(
            "  scheduled backup {}: {}",
            record.profile,
            record.describe()
        );
    }

    let services = services(ctx, instance)?;
    out!("  services:");
    if services.is_empty() {
        out!("    none running");
    }
    let width = services.iter().map(|s| s.name.len()).max().unwrap_or(0);
    let healthy = all_healthy(&services);
    for service in &services {
        let mark = if service.healthy {
            ""
        } else {
            "  ← not healthy"
        };
        out!("    {:<width$}  {}{mark}", service.name, service.state);
    }

    if !instance.links.is_empty() {
        out!("  links:");
        let server_config = instance::server_config(ctx)?;
        for link in &instance.links {
            let summary = match (
                shared::find_kind(&link.kind),
                link.shared_app(&server_config),
            ) {
                (Some(kind), Some(shared)) => {
                    let link_ctx = AppCtx {
                        server: ctx,
                        app,
                        shared: &shared,
                        peers: &[],
                    };
                    kind.describe_link(&link_ctx, &link.entry)
                        .unwrap_or_else(|err| format!("cannot read link: {err:#}"))
                }
                _ => "shared app is no longer configured".to_string(),
            };
            out!(
                "    {:<10} {:<16} {}: {summary}",
                link.kind,
                link.entry,
                link.instance
            );
        }
    }
    if !instance.removing.is_empty() {
        out!("  no longer configured (removed by the next deploy with --apply):");
        for link in &instance.removing {
            out!("    {:<10} {:<16} {}", link.kind, link.entry, link.instance);
        }
    }
    Ok(healthy)
}

/// The compose files uploaded by deploy, merged (empty if they cannot be read).
pub fn stored_compose_file(ctx: &ServerCtx, instance: &Instance) -> Result<ComposeFile> {
    let app = &instance.app;
    let dir = instance::dir(&app.name, &app.prefix);
    let mut texts = Vec::new();
    for file in app.deployment.compose_files() {
        match ctx.read(&format!("{dir}/sources/{file}"))? {
            Some(text) => texts.push(text),
            None => return Ok(ComposeFile::default()),
        }
    }
    let texts: Vec<&str> = texts.iter().map(String::as_str).collect();
    Ok(ComposeFile::parse_all(&texts).unwrap_or_default())
}

/// The server must be reachable, ours, and initialized.
pub fn initialized_server<'a>(
    connector: &'a dyn Connector,
    server_home: &str,
    manager_key: &str,
) -> Result<ServerCtx<'a>> {
    let report = server_state::check(connector, server_home, manager_key)?;
    match (report.status, report.home) {
        (Status::Ok | Status::Degraded, Some(home)) => Ok(ServerCtx { connector, home }),
        (Status::Uninitialized, _) => Err(CompostError::ServerNotReady(
            "compost has not deployed anything on this server".into(),
        )
        .into()),
        (status, _) => Err(CompostError::ServerNotReady(format!(
            "server is {status}: {}",
            report.detail.unwrap_or_default()
        ))
        .into()),
    }
}

pub fn not_deployed(name: &str, prefix: &str) -> anyhow::Error {
    CompostError::ServerNotReady(format!("{name}-{prefix} is not deployed on this server")).into()
}
