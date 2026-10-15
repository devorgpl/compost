//! Named docker volumes of an instance, copied through a small helper container.

use anyhow::Result;

use super::instance;
use crate::config::app::AppConfig;
use crate::connector::{Connector, shell_quote};
use crate::shared::ServerCtx;

/// Image of the helper container that reads and writes volume contents.
const HELPER_IMAGE: &str = "busybox:stable";

/// `docker run` of the helper with the volume mounted at `/v`.
fn helper(volume: &str, options: &str, command: &str) -> String {
    format!(
        "docker run --rm --pull missing {options} -v {}:/v {HELPER_IMAGE} sh -c {}",
        shell_quote(volume),
        shell_quote(command)
    )
}

pub fn exists(connector: &dyn Connector, volume: &str) -> Result<bool> {
    let output = connector.run(
        &format!(
            "docker volume inspect {} >/dev/null 2>&1",
            shell_quote(volume)
        ),
        None,
    )?;
    Ok(output.success())
}

pub fn is_empty(connector: &dyn Connector, volume: &str) -> Result<bool> {
    let listing = connector.exec(&helper(volume, "", "ls -A /v"))?;
    Ok(listing.trim().is_empty())
}

/// Writes the volume as an uncompressed tar to stdout.
pub fn backup_script(volume: &str) -> String {
    helper(volume, "", "tar -cf - -C /v .")
}

/// Replaces the volume's contents with the tar on stdin.
pub fn restore_script(volume: &str) -> String {
    helper(
        volume,
        "-i",
        "rm -rf /v/* /v/.[!.]* /v/..?* && tar -xf - -C /v",
    )
}

/// Stops an instance's running services until dropped, then starts them
/// again. Services that were not running (e.g. finished migrations) stay as they are.
pub struct Stopped<'a> {
    ctx: &'a ServerCtx<'a>,
    app: &'a AppConfig,
    services: Vec<String>,
}

impl<'a> Stopped<'a> {
    pub fn stop(ctx: &'a ServerCtx<'a>, app: &'a AppConfig) -> Result<Self> {
        let compose = instance::compose_command(ctx, app);
        let running = ctx
            .connector
            .exec(&format!("{compose} ps --services --status running"))?;
        let services: Vec<String> = running.lines().map(str::to_string).collect();
        if !services.is_empty() {
            out!("  stopping {} ...", app.instance());
            ctx.connector.exec(&format!("{compose} stop"))?;
        }
        Ok(Self { ctx, app, services })
    }
}

impl Drop for Stopped<'_> {
    fn drop(&mut self) {
        if self.services.is_empty() {
            return;
        }
        let _cleanup = crate::interrupt::cleanup();
        out!("  starting {} ...", self.app.instance());
        let services: Vec<String> = self.services.iter().map(|s| shell_quote(s)).collect();
        let script = format!(
            "{} start {}",
            instance::compose_command(self.ctx, self.app),
            services.join(" ")
        );
        if let Err(err) = self.ctx.connector.exec(&script) {
            eprintln!(
                "warning: could not start {} again: {err:#}",
                self.app.instance()
            );
        }
    }
}
