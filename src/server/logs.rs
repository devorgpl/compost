//! `compost server logs`: the output of a shared app's containers.

use anyhow::Result;

use crate::app::instance;
use crate::app::logs::{self, Options};
use crate::app::state::initialized_server;
use crate::connector::{Connector, shell_quote};
use crate::error::config_err;
use crate::shared;

/// `name` is a shared app's name, or `<kind>/<name>` when two kinds use it.
pub fn logs(
    connector: &dyn Connector,
    server_home: &str,
    manager_key: &str,
    name: &str,
    options: &Options,
) -> Result<()> {
    logs::check(options)?;
    let ctx = initialized_server(connector, server_home, manager_key)?;
    let config = instance::server_config(&ctx)?;
    let (kind, app) = config.find_shared(name)?;
    let name = app.name.as_str();
    if app.is_external() {
        return Err(config_err(format!(
            "{kind} {name} is external: compost runs no containers for it"
        )));
    }
    if shared::find_kind(kind).is_some_and(|k| !k.runs_service(app)) {
        return Err(config_err(format!(
            "{kind} {name} runs only when it sends; it has no output to show"
        )));
    }
    let compose = format!(
        "cd {} && docker compose -p {}",
        shell_quote(&ctx.path(&shared::shared_dir(kind, name))),
        shell_quote(&shared::project_name(kind, name))
    );
    logs::stream(connector, &compose, options)
}
