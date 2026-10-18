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
    let (kind_filter, name) = match name.split_once('/') {
        Some((kind, name)) => (Some(kind), name),
        None => (None, name),
    };
    let matches: Vec<(&String, &shared::SharedAppConfig)> = config
        .shared
        .iter()
        .filter(|(kind, _)| kind_filter.is_none_or(|k| k == kind.as_str()))
        .flat_map(|(kind, apps)| apps.iter().map(move |app| (kind, app)))
        .filter(|(_, app)| app.name == name)
        .collect();
    let (kind, app) = match matches.as_slice() {
        [found] => *found,
        [] => {
            let known: Vec<String> = config
                .shared
                .values()
                .flatten()
                .map(|app| app.name.clone())
                .collect();
            return Err(config_err(format!(
                "no shared app `{name}` on this server (shared apps: {})",
                known.join(", ")
            )));
        }
        _ => {
            let names: Vec<String> = matches
                .iter()
                .map(|(kind, app)| format!("{kind}/{}", app.name))
                .collect();
            return Err(config_err(format!(
                "`{name}` is ambiguous; use {}",
                names.join(" or ")
            )));
        }
    };
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
