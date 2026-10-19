//! `compost dev reset`: delete the data an instance's links hold, keeping
//! their credentials, so that the next deploy creates them empty.

use anyhow::{Context, Result};

use super::instance::{self, PinnedLink};
use super::remove::check_purge;
use super::state::{initialized_server, not_deployed};
use crate::connector::Connector;
use crate::error::config_err;
use crate::server::lock::Lock;
use crate::shared::{self, AppCtx, connectors_dir};

pub struct Options {
    /// Only links of these kinds (default: every kind that holds data).
    pub kinds: Vec<String>,
    /// Only these links: `<entry>` or `<kind>:<entry>`.
    pub links: Vec<String>,
    /// Let the next deploy generate new passwords and keys.
    pub new_credentials: bool,
    pub yes: bool,
    pub force_unlock: bool,
}

/// Stop the instance and delete the data of the selected links. Returns
/// `false` when the user cancelled.
pub fn reset(
    connector: &dyn Connector,
    server_home: &str,
    manager_key: &str,
    name: &str,
    prefix: &str,
    options: &Options,
) -> Result<bool> {
    check_purge(&options.kinds)?;
    let ctx = initialized_server(connector, server_home, manager_key)?;
    let Some(deployed) = instance::load(&ctx, name, prefix)? else {
        return Err(not_deployed(name, prefix));
    };
    let project = deployed.app.instance();
    let selected = select(&deployed.links, options)?;
    if selected.is_empty() {
        return Err(config_err(format!(
            "{project} has no links whose data can be reset"
        )));
    }

    out!("Resetting {project}: stops it and deletes the data of");
    for link in &selected {
        out!("  {} {} on {}", link.kind, link.entry, link.instance);
    }
    if options.new_credentials {
        out!("with new passwords and keys.");
    } else {
        out!("Passwords and keys stay the same.");
    }
    if !options.yes && !super::confirm()? {
        out!("Cancelled.");
        return Ok(false);
    }

    let _lock = Lock::acquire(connector, &ctx.home, options.force_unlock)?;
    connector
        .exec(&format!(
            "{} stop",
            instance::compose_command(&ctx, &deployed.app)
        ))
        .context("docker compose stop failed")?;
    let server_config = instance::server_config(&ctx)?;
    for link in selected.iter().rev() {
        let kind = shared::find_kind(&link.kind).context("selected links have known kinds")?;
        let shared = link.shared_app(&server_config).with_context(|| {
            format!(
                "{} {} is no longer configured; deploy without link {} first",
                link.kind, link.instance, link.entry
            )
        })?;
        let link_ctx = AppCtx {
            server: &ctx,
            app: &deployed.app,
            shared: &shared,
            peers: &[],
        };
        // Every kind keeps a link's credentials in this record; the next
        // deploy reads them from it.
        let record = ctx.path(&format!(
            "{}/{}.yaml",
            connectors_dir(kind.name(), &shared.name),
            link_ctx.link_id(&link.entry)
        ));
        let kept = connector.read_file(&record)?;
        kind.remove_application(&link_ctx, &link.entry, true)
            .with_context(|| format!("resetting link {} {}", link.kind, link.entry))?;
        if let (Some(text), false) = (kept, options.new_credentials) {
            connector.write_file(&record, text.as_bytes(), 0o600)?;
        }
        out!("  deleted the data of {} {}", link.kind, link.entry);
    }
    Ok(true)
}

/// The links that hold data, narrowed to `--kind` and `--link`, in deploy order.
fn select<'a>(links: &'a [PinnedLink], options: &Options) -> Result<Vec<&'a PinnedLink>> {
    let named = |link: &PinnedLink, wanted: &str| {
        wanted == link.entry || wanted == format!("{}:{}", link.kind, link.entry)
    };
    for wanted in &options.links {
        if !links.iter().any(|link| named(link, wanted)) {
            return Err(config_err(format!(
                "--link {wanted}: the instance has no such link"
            )));
        }
    }
    let mut selected: Vec<&PinnedLink> = links
        .iter()
        .filter(|link| shared::find_kind(&link.kind).is_some_and(|k| k.supports_purge()))
        .filter(|link| options.kinds.is_empty() || options.kinds.contains(&link.kind))
        .filter(|link| {
            options.links.is_empty() || options.links.iter().any(|wanted| named(link, wanted))
        })
        .collect();
    selected.sort_by_key(|link| shared::find_kind(&link.kind).map(|k| k.apply_order()));
    Ok(selected)
}
