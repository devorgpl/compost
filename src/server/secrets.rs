//! Secrets of the shared apps kept on the server (`server deploy --secret`):
//! an overlay that `server deploy` merges after compost-server.yaml.

use anyhow::{Context, Result};
use serde_yaml_ng::Value;

use super::lock::Lock;
use crate::app::state::initialized_server;
use crate::connector::{Connector, shell_quote};
use crate::error::config_err;
use crate::shared::ServerCtx;

/// Relative to `COMPOST_SERVER_HOME`, next to the apps' `secrets/apps`.
pub const PATH: &str = "secrets/server.yaml";

/// Check an overlay before it is stored: a mapping, as compost-server.yaml.
pub fn parse(text: &str) -> Result<Value> {
    let value: Value =
        serde_yaml_ng::from_str(text).map_err(|e| config_err(format!("secrets file: {e}")))?;
    if !value.is_mapping() {
        return Err(config_err(
            "secrets file: expected a mapping, e.g. with `shared`",
        ));
    }
    Ok(value)
}

/// Write the overlay; returns the absolute path. The caller holds the lock.
pub fn store(ctx: &ServerCtx, text: &str) -> Result<String> {
    let path = ctx.path(PATH);
    ctx.connector.write_file(&path, text.as_bytes(), 0o600)?;
    Ok(path)
}

/// The stored overlay with its absolute path, if any. Reads without checking
/// the server; `server deploy` does that next.
pub fn read(connector: &dyn Connector, server_home: &str) -> Result<Option<(String, Value)>> {
    let ctx = ServerCtx {
        connector,
        home: connector.resolve_path(server_home)?,
    };
    let path = ctx.path(PATH);
    let Some(text) = connector.read_file(&path)? else {
        return Ok(None);
    };
    let value =
        serde_yaml_ng::from_str(&text).with_context(|| format!("invalid {path} on the server"))?;
    Ok(Some((path, value)))
}

/// `compost secret remove --shared`
pub fn remove(
    connector: &dyn Connector,
    server_home: &str,
    manager_key: &str,
    force_unlock: bool,
) -> Result<()> {
    let ctx = initialized_server(connector, server_home, manager_key)?;
    if ctx.read(PATH)?.is_none() {
        return Err(config_err(
            "no stored secrets of the shared apps (see `compost secret list`)",
        ));
    }
    let _lock = Lock::acquire(connector, &ctx.home, force_unlock)?;
    connector.exec(&format!("rm -f -- {}", shell_quote(&ctx.path(PATH))))?;
    out!("Removed the stored secrets of the shared apps");
    Ok(())
}
