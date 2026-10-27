//! Notifications: messages sent through the communication apps of the server
//! (see [`crate::shared::communication`]), with their image's `send`.

use std::fmt;

use anyhow::{Context, Result, bail};

use crate::app::state::initialized_server;
use crate::config::server::{Event, Notifications};
use crate::connector::{Connector, shell_quote};
use crate::error::config_err;
use crate::shared::ServerCtx;
use crate::shared::communication::{self, KIND};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Info,
    Warning,
    Error,
    /// Something is fine again.
    Ok,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Severity::Info => "info",
            Severity::Warning => "warning",
            Severity::Error => "error",
            Severity::Ok => "ok",
        })
    }
}

pub struct Notification {
    /// e.g. `backup`; empty for a test.
    pub event: String,
    pub severity: Severity,
    pub title: String,
    pub body: String,
    /// The server as messages name it ([`server_label`]).
    pub server: String,
}

/// How messages name the server: `notifications.label`, else `server_name`
/// (as `server define` gave it), and the host's name when it differs, e.g.
/// `prod on vps-1234`, so that channels several servers send to tell them
/// apart.
pub fn server_label(
    ctx: &ServerCtx,
    notifications: Option<&Notifications>,
    server_name: &str,
) -> String {
    let label = notifications
        .and_then(|n| n.label.as_deref())
        .unwrap_or(server_name);
    let host = ctx.connector.exec("uname -n").unwrap_or_default();
    with_host(label, host.trim())
}

fn with_host(label: &str, host: &str) -> String {
    if host.is_empty() || host == label {
        label.to_string()
    } else {
        format!("{label} on {host}")
    }
}

/// Send `notification` through the communication app `name`.
pub fn send_to(ctx: &ServerCtx, name: &str, notification: &Notification) -> Result<()> {
    let image = ctx
        .read(&communication::image_path(name))?
        .with_context(|| {
            format!("communication app {name} is not deployed yet; run `compost server deploy`")
        })?;
    let mut script = format!(
        "docker run --rm -i --env-file {} {} send --severity {} --title {}",
        shell_quote(&ctx.path(&communication::env_path(name))),
        shell_quote(image.trim()),
        notification.severity,
        shell_quote(&notification.title),
    );
    if !notification.server.is_empty() {
        script.push_str(&format!(" --server {}", shell_quote(&notification.server)));
    }
    if !notification.event.is_empty() {
        script.push_str(&format!(" --event {}", shell_quote(&notification.event)));
    }
    let output = ctx
        .connector
        .run(&script, Some(notification.body.as_bytes()))?;
    if !output.success() {
        let reason = output.stderr.trim();
        bail!(
            "{}",
            if reason.is_empty() {
                format!("exit status {}", output.status)
            } else {
                reason.to_string()
            }
        );
    }
    Ok(())
}

/// Send `notification` to every communication app of `send_to` that wants
/// `event`. Failures are warnings: they must not stop what reported them.
pub fn send_event(
    ctx: &ServerCtx,
    notifications: &Notifications,
    event: Event,
    notification: &Notification,
) {
    for recipient in notifications.send_to.iter().filter(|r| r.wants(event)) {
        match send_to(ctx, recipient.name(), notification) {
            Ok(()) => out!("  sent to {}: {}", recipient.name(), notification.title),
            Err(err) => eprintln!("warning: could not notify {}: {err:#}", recipient.name()),
        }
    }
}

/// `compost notify test`: a message to every communication app, or to one.
pub fn test(
    connector: &dyn Connector,
    server_name: &str,
    server_home: &str,
    manager_key: &str,
    to: Option<&str>,
    message: Option<&str>,
) -> Result<()> {
    let ctx = initialized_server(connector, server_home, manager_key)?;
    let config = crate::app::instance::server_config(&ctx)?;
    let mut names: Vec<&str> = config
        .shared
        .get(KIND)
        .map(|apps| apps.iter().map(|a| a.name.as_str()).collect())
        .unwrap_or_default();
    if names.is_empty() {
        return Err(config_err(
            "the server has no communication apps; add one under `shared.communication` of compost-server.yaml and deploy it",
        ));
    }
    if let Some(to) = to {
        if !names.contains(&to) {
            return Err(config_err(format!(
                "--to {to}: no such communication app (known: {})",
                names.join(", ")
            )));
        }
        names.retain(|name| *name == to);
    }
    let notification = Notification {
        event: String::new(),
        severity: Severity::Info,
        title: "Test message from compost".to_string(),
        body: message
            .unwrap_or("compost can send notifications to this channel.")
            .to_string(),
        server: server_label(&ctx, config.notifications.as_ref(), server_name),
    };
    let mut failed = 0;
    for name in &names {
        match send_to(&ctx, name, &notification) {
            Ok(()) => out!("  {name}: sent"),
            Err(err) => {
                out!("  {name}: failed: {err:#}");
                failed += 1;
            }
        }
    }
    if failed > 0 {
        bail!("{failed} of {} channels failed", names.len());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_names_the_host_once() {
        assert_eq!(with_host("prod", "vps-1234"), "prod on vps-1234");
        assert_eq!(with_host("prod", "prod"), "prod");
        assert_eq!(with_host("prod", ""), "prod");
    }
}
