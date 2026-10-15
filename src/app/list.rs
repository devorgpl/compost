//! `depap app list` (spec §10.12): the application instances in `state.yaml`.

use anyhow::Result;

use crate::connector::Connector;
use crate::error::DepapError;
use crate::server::state::{self as server_state, Status, is_project_running};
use crate::server::state_file::StateFile;
use crate::shared::ServerCtx;

pub fn list(connector: &dyn Connector, server_home: &str, manager_key: &str) -> Result<()> {
    let report = server_state::check(connector, server_home, manager_key)?;
    let home = match (report.status, report.home) {
        (Status::Ok | Status::Degraded, Some(home)) => home,
        (Status::Uninitialized, _) => {
            out!("No applications: depap has not deployed anything on this server.");
            return Ok(());
        }
        (status, _) => {
            return Err(DepapError::ServerNotReady(format!(
                "server is {status}: {}",
                report.detail.unwrap_or_default()
            ))
            .into());
        }
    };
    let ctx = ServerCtx { connector, home };
    let mut apps = StateFile::load(&ctx)?.apps;
    if apps.is_empty() {
        out!("No applications deployed.");
        return Ok(());
    }
    apps.sort_by(|a, b| (&a.name, &a.prefix).cmp(&(&b.name, &b.prefix)));

    let instances: Vec<String> = apps
        .iter()
        .map(|r| format!("{}-{}", r.name, r.prefix))
        .collect();
    let width = instances.iter().map(String::len).max().unwrap_or(0);
    for (record, instance) in apps.iter().zip(&instances) {
        let running = if is_project_running(connector, instance)? {
            "running"
        } else {
            "not running"
        };
        out!(
            "{instance:<width$}  {:<9}  {running:<11}  {}",
            record.status.to_string(),
            record.updated_at
        );
    }
    Ok(())
}
