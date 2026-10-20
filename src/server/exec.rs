//! `compost server exec` and `compost server cmd`: a command in a shared
//! app's container, given on the command line or named in `commands`.

use anyhow::{Context, Result};
use serde_yaml_ng::Value;

use crate::app::exec::{self, Prepared, attach, env_file, env_flags, tty};
use crate::app::instance;
use crate::app::state::initialized_server;
use crate::config::app::ComposeFile;
use crate::config::command;
use crate::config::server::ServerConfig;
use crate::connector::{Connector, shell_quote};
use crate::error::config_err;
use crate::shared::{self, ServerCtx};

/// A container of a shared app.
struct Target {
    kind: String,
    name: String,
    /// `cd <dir> && docker compose -p <project>`
    compose: String,
    /// Compose service, e.g. `compost-shared-caddy-main-crowdsec`.
    service: String,
    /// e.g. `caddy-main/crowdsec`.
    place: String,
}

/// The compose service of a shared app for `service` (one of its other
/// services by its short name, or its main one), checked against its
/// compose file.
pub fn compose_service(
    kind: &str,
    name: &str,
    compose: &str,
    service: Option<&str>,
) -> Result<String> {
    let main = shared::service_name(name);
    let compose_service = match service {
        Some(service) => format!("{main}-{service}"),
        None => main.clone(),
    };
    // A missing or unreadable compose file gives no services; compose then
    // reports the name.
    let file = ComposeFile::parse_all(&[compose]).unwrap_or_default();
    if file.services.is_empty() || file.services.contains_key(&compose_service) {
        return Ok(compose_service);
    }
    let others: Vec<String> = file
        .services
        .keys()
        .filter_map(|s| s.strip_prefix(&format!("{main}-")))
        .map(|s| format!("`{s}`"))
        .collect();
    let what = service.map_or_else(|| "main service".to_string(), |s| format!("service `{s}`"));
    let hint = if others.is_empty() {
        "it has only its main one".to_string()
    } else {
        format!("others: {}", others.join(", "))
    };
    Err(config_err(format!("{kind} {name} has no {what} ({hint})")))
}

impl Target {
    /// `service` is one of the shared app's other services by its short name,
    /// e.g. `crowdsec`; without it, the shared app's main container.
    fn find(
        ctx: &ServerCtx,
        config: &ServerConfig,
        shared_app: &str,
        service: Option<&str>,
    ) -> Result<Self> {
        let (kind, app) = config.find_shared(shared_app)?;
        let name = &app.name;
        if app.is_external() {
            return Err(config_err(format!(
                "{kind} {name} is external: compost runs no containers for it"
            )));
        }
        if shared::find_kind(kind).is_some_and(|k| !k.runs_service(app)) {
            return Err(config_err(format!(
                "{kind} {name} runs only when it sends; it has no container to run a command in"
            )));
        }
        let dir = shared::shared_dir(kind, name);
        let compose = ctx
            .read(&format!("{dir}/compose.yaml"))?
            .unwrap_or_default();
        let compose_service = compose_service(kind, name, &compose, service)?;
        Ok(Self {
            kind: kind.to_string(),
            name: name.clone(),
            compose: format!(
                "cd {} && docker compose -p {}",
                shell_quote(&ctx.path(&dir)),
                shell_quote(&shared::project_name(kind, name))
            ),
            service: compose_service,
            place: match service {
                Some(service) => format!("{name}/{service}"),
                None => name.clone(),
            },
        })
    }

    fn script(&self, argv: &[String], tty: bool, env: &[String]) -> String {
        let mut script = format!("{} exec", self.compose);
        if !tty {
            script.push_str(" -T");
        }
        script.push_str(&env_flags(env));
        script.push(' ');
        script.push_str(&shell_quote(&self.service));
        for arg in argv {
            script.push(' ');
            script.push_str(&shell_quote(arg));
        }
        script
    }

    /// A value of the shared app's `root-connector.yaml`, where compost keeps
    /// its administrator's credentials, e.g. `root_password`.
    fn secret(&self, ctx: &ServerCtx, key: &str) -> Result<String> {
        let path = format!(
            "{}/root-connector.yaml",
            shared::connectors_dir(&self.kind, &self.name)
        );
        let text = ctx.read(&path)?.ok_or_else(|| {
            config_err(format!(
                "{} {} keeps no credentials compost can pass to a command (`{{secret:{key}}}`)",
                self.kind, self.name
            ))
        })?;
        let values: Value = serde_yaml_ng::from_str(&text)
            .with_context(|| format!("invalid {} on the server", ctx.path(&path)))?;
        match values.get(key).and_then(crate::config::app::env_value) {
            Some(value) => Ok(value),
            None => {
                let keys: Vec<&str> = values
                    .as_mapping()
                    .into_iter()
                    .flat_map(|map| map.keys().filter_map(Value::as_str))
                    .collect();
                Err(config_err(format!(
                    "{} {} has no secret `{key}` (keys: {})",
                    self.kind,
                    self.name,
                    keys.join(", ")
                )))
            }
        }
    }
}

/// `compost server exec`
pub fn exec(
    connector: &dyn Connector,
    server_home: &str,
    manager_key: &str,
    shared_app: &str,
    service: Option<&str>,
    command: &[String],
) -> Result<()> {
    let ctx = initialized_server(connector, server_home, manager_key)?;
    let config = instance::server_config(&ctx)?;
    let target = Target::find(&ctx, &config, shared_app, service)?;
    let tty = tty();
    attach(connector, &target.script(command, tty, &[]), tty)
}

/// `compost server cmd`: list the server's commands, or run one. The
/// commands are those of the deployed compost-server.yaml.
pub fn cmd(
    connector: &dyn Connector,
    server_name: &str,
    server_home: &str,
    manager_key: &str,
    name: Option<&str>,
    words: &[String],
) -> Result<()> {
    let ctx = initialized_server(connector, server_home, manager_key)?;
    let config = instance::server_config(&ctx)?;
    // `caddy-main/crowdsec`, without the kind of `<kind>/<name>`.
    let place = |template: &command::Template| {
        let app = template.shared_app.as_deref().unwrap_or_default();
        let app = app.split_once('/').map_or(app, |(_, name)| name);
        match &template.service {
            Some(service) => format!("{app}/{service}"),
            None => app.to_string(),
        }
    };
    let builtin = builtin_commands(&config)?;
    if name == Some("--history") {
        if let Some(word) = words.first() {
            return Err(config_err(format!(
                "--history takes no arguments, found `{word}`"
            )));
        }
        return exec::history(&ctx, None, server_name);
    }
    let Some(name) = name else {
        if config.commands.is_empty() {
            out!(
                "{server_name} has no commands of its own; add them under `commands` in compost-server.yaml and deploy"
            );
        } else {
            out!("Commands of {server_name}:");
            out_part!(
                "{}",
                command::list(config.commands.iter().map(|(n, t)| (n, t, place(t))))
            );
        }
        if !builtin.is_empty() {
            out!("Built into compost:");
            out_part!(
                "{}",
                command::list(builtin.iter().map(|(n, t)| (n, t, place(t))))
            );
        }
        return Ok(());
    };
    let template = match name.split_once('.') {
        Some((shared_app, command)) => find_builtin(&config, &builtin, shared_app, command)?,
        None => config.commands.get(name).ok_or_else(|| {
            config_err(format!(
                "{server_name} has no command `{name}` ({})",
                exec::known(&config.commands, "compost-server.yaml")
            ))
        })?,
    };
    let call = template.parse_call(name, words)?;
    if call.help {
        out!("{}", template.help(name, &place(template)));
        return Ok(());
    }
    let target = Target::find(
        &ctx,
        &config,
        template.shared_app.as_deref().unwrap_or_default(),
        template.service.as_deref(),
    )?;
    let rendered = template.render(&call.values, &mut |key| target.secret(&ctx, key))?;
    let prepared = Prepared {
        name,
        template,
        place: target.place.clone(),
        rendered,
        yes: call.yes,
        dry_run: call.dry_run,
    };
    if !prepared.go_ahead()? {
        return Ok(());
    }
    let tty = tty();
    let prelude = env_file(&ctx, &prepared.rendered.env)?;
    let script = format!(
        "{prelude}{}",
        target.script(&prepared.rendered.argv, tty, &prepared.env_names())
    );
    let entry = exec::Entry::new(&prepared, None);
    attach(connector, &exec::logged(&ctx, &script, &entry)?, tty)
}

/// compost's commands for the shared apps that run containers.
fn builtin_commands(config: &ServerConfig) -> Result<Vec<(String, command::Template)>> {
    let mut commands = Vec::new();
    for (kind, app) in config.shared_apps() {
        if shared::find_kind(kind).is_some_and(|k| k.runs_service(app)) {
            commands.extend(shared::commands::builtin(kind, app)?);
        }
    }
    Ok(commands)
}

/// A built-in command, as `<shared app>.<command>`; the shared app may be
/// `<kind>/<name>`.
fn find_builtin<'a>(
    config: &ServerConfig,
    builtin: &'a [(String, command::Template)],
    shared_app: &str,
    command: &str,
) -> Result<&'a command::Template> {
    let (kind, app) = config.find_shared(shared_app)?;
    let id = format!("{kind}/{}", app.name);
    let of_app: Vec<(&str, &command::Template)> = builtin
        .iter()
        .filter(|(_, t)| t.shared_app.as_deref() == Some(id.as_str()))
        .filter_map(|(n, t)| Some((n.split_once('.')?.1, t)))
        .collect();
    if let Some((_, template)) = of_app.iter().find(|(n, _)| *n == command) {
        return Ok(template);
    }
    let names: Vec<&str> = of_app.iter().map(|(n, _)| *n).collect();
    Err(config_err(match names.as_slice() {
        [] => format!("compost has no commands for {kind} {}", app.name),
        _ => format!(
            "compost has no command `{command}` for {kind} {} (commands: {})",
            app.name,
            names.join(", ")
        ),
    }))
}
