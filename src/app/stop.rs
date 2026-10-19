//! `compost dev stop`: stop an instance's containers, keeping everything else.

use anyhow::{Context, Result};

use super::instance;
use super::state::{initialized_server, not_deployed};
use crate::connector::Connector;
use crate::server::lock::Lock;
use crate::server::state_file::{DeployStatus, StateFile};

pub fn stop(
    connector: &dyn Connector,
    server_home: &str,
    manager_key: &str,
    name: &str,
    prefix: &str,
    force_unlock: bool,
) -> Result<()> {
    let ctx = initialized_server(connector, server_home, manager_key)?;
    let Some(deployed) = instance::load(&ctx, name, prefix)? else {
        return Err(not_deployed(name, prefix));
    };
    let _lock = Lock::acquire(connector, &ctx.home, force_unlock)?;
    connector
        .exec(&format!(
            "{} stop",
            instance::compose_command(&ctx, &deployed.app)
        ))
        .context("docker compose stop failed")?;
    let mut state_file = StateFile::load(&ctx)?;
    state_file.set_app(name, prefix, DeployStatus::Stopped);
    state_file.save(&ctx)?;
    out!("Stopped {name}-{prefix}; its data is kept.");
    Ok(())
}
