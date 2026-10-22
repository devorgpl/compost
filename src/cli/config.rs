//! `compost config …`: preferences in `COMPOST_HOME`, globally or per server.

use anyhow::Result;
use clap::Subcommand;

use crate::config::check_name;
use crate::config::local::{
    BackupDefault, CompostHome, DEFAULT_LOGS_TAIL, DeploySettings, LogsSettings, TraceLevel,
    check_tail,
};
use crate::error::config_err;

const PREFIX: &str = "prefix";
const DEPLOY_APPLY: &str = "deploy.apply";
const DEPLOY_PULL: &str = "deploy.pull";
const DEPLOY_RECREATE: &str = "deploy.recreate";
const DEPLOY_BACKUP: &str = "deploy.backup";
const LOGS_TAIL: &str = "logs.tail";
const LOGS_TIMESTAMPS: &str = "logs.timestamps";
const DEV_SERVER: &str = "dev.server";
const TRACE: &str = "trace";
/// Keys set globally or per server, with the default shown when unset.
const SCOPED: &[(&str, &str)] = &[
    (PREFIX, "main"),
    (DEPLOY_APPLY, "false"),
    (DEPLOY_PULL, "false"),
    (DEPLOY_RECREATE, "false"),
    (DEPLOY_BACKUP, "the app's deployment.backup"),
    (LOGS_TAIL, DEFAULT_LOGS_TAIL),
    (LOGS_TIMESTAMPS, "false"),
];
const KEYS: &[&str] = &[
    PREFIX,
    DEPLOY_APPLY,
    DEPLOY_PULL,
    DEPLOY_RECREATE,
    DEPLOY_BACKUP,
    LOGS_TAIL,
    LOGS_TIMESTAMPS,
    DEV_SERVER,
    TRACE,
];

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Set a preference, e.g. `compost config set deploy.apply true`,
    /// `compost config set prefix staging --for-server staging`,
    /// `compost config set dev.server laptop` or
    /// `compost config set trace basic --for-server prod`.
    ///
    /// Keys: `prefix`, `deploy.apply`, `deploy.pull`, `deploy.recreate`,
    /// `deploy.backup` (a profile or `false`), `logs.tail`, `logs.timestamps`,
    /// `dev.server` (global only) and `trace` (per server only)
    Set {
        key: String,
        value: String,
        /// Only for this server (overrides the global value)
        #[arg(long)]
        for_server: Option<String>,
    },
    /// Remove a preference, so the global value (or the default) applies
    Unset {
        key: String,
        /// Only for this server
        #[arg(long)]
        for_server: Option<String>,
    },
    /// Show the preferences
    Show,
}

pub fn run(home: &CompostHome, command: ConfigCommand) -> Result<()> {
    match command {
        ConfigCommand::Set {
            key,
            value,
            for_server,
        } if key == DEV_SERVER => update_dev_server(home, for_server.as_deref(), Some(value)),
        ConfigCommand::Set {
            key,
            value,
            for_server,
        } if key == TRACE => {
            let level = TraceLevel::parse(&value).ok_or_else(|| {
                config_err(format!(
                    "{TRACE}: `{value}` is not a level ({})",
                    TraceLevel::NAMES.join(", ")
                ))
            })?;
            update_trace(home, for_server.as_deref(), level)
        }
        ConfigCommand::Set {
            key,
            value,
            for_server,
        } => update(home, &key, for_server.as_deref(), Some(&value)),
        ConfigCommand::Unset { key, for_server } if key == DEV_SERVER => {
            update_dev_server(home, for_server.as_deref(), None)
        }
        ConfigCommand::Unset { key, for_server } if key == TRACE => {
            update_trace(home, for_server.as_deref(), TraceLevel::None)
        }
        ConfigCommand::Unset { key, for_server } => update(home, &key, for_server.as_deref(), None),
        ConfigCommand::Show => show(home),
    }
}

/// Set (`Some`) or remove (`None`) `dev.server`: a global setting naming a
/// local server.
fn update_dev_server(
    home: &CompostHome,
    server: Option<&str>,
    value: Option<String>,
) -> Result<()> {
    if server.is_some() {
        return Err(config_err(format!(
            "{DEV_SERVER} is global; leave out --for-server"
        )));
    }
    home.require_manager()?;
    if let Some(name) = &value {
        check_name("server name", name)?;
        let entry = home.find_server(name)?;
        if !matches!(
            entry.connector,
            crate::config::local::ConnectorConfig::Local
        ) {
            return Err(config_err(format!(
                "{DEV_SERVER}: `{name}` is an SSH server; dev commands only manage local servers"
            )));
        }
    }
    let mut settings = home.settings()?;
    settings.dev.server.clone_from(&value);
    home.save_settings(&settings)?;
    match value {
        Some(name) => out!("{DEV_SERVER} = {name}"),
        None => out!("{DEV_SERVER} unset"),
    }
    Ok(())
}

/// Set `trace` of a server in servers.yaml; `none` removes it.
fn update_trace(home: &CompostHome, server: Option<&str>, level: TraceLevel) -> Result<()> {
    let Some(name) = server else {
        return Err(config_err(format!(
            "{TRACE} is set per server; add --for-server <server>"
        )));
    };
    home.require_manager()?;
    let mut servers = home.servers()?;
    let entry = servers
        .servers
        .iter_mut()
        .find(|s| s.name == name)
        .ok_or_else(|| config_err(format!("unknown server `{name}`")))?;
    entry.trace = level;
    home.save_servers(&servers)?;
    if level.is_none() {
        out!("{TRACE} unset for {name}");
    } else {
        out!(
            "{TRACE} = {} for {name}, in {}",
            level.name(),
            home.trace_log(name).display()
        );
    }
    Ok(())
}

fn check_key(key: &str) -> Result<()> {
    if KEYS.contains(&key) {
        Ok(())
    } else {
        Err(config_err(format!(
            "unknown setting `{key}` (known: {})",
            KEYS.join(", ")
        )))
    }
}

fn parse_bool(key: &str, value: &str) -> Result<bool> {
    match value {
        "true" | "yes" | "on" => Ok(true),
        "false" | "no" | "off" => Ok(false),
        _ => Err(config_err(format!(
            "{key}: `{value}` is not a boolean (true or false)"
        ))),
    }
}

/// The settings of one scope: `config.yaml`, or a server in servers.yaml.
struct Scope<'a> {
    prefix: &'a mut Option<String>,
    deploy: &'a mut DeploySettings,
    logs: &'a mut LogsSettings,
}

impl Scope<'_> {
    /// Set (`Some`) or remove (`None`) one of the [`SCOPED`] keys.
    fn set(&mut self, key: &str, value: Option<&str>) -> Result<()> {
        let flag = |value: Option<&str>| value.map(|v| parse_bool(key, v)).transpose();
        match key {
            PREFIX => {
                *self.prefix = value
                    .map(|v| check_name("prefix", v).map(|()| v.to_string()))
                    .transpose()?;
            }
            DEPLOY_APPLY => self.deploy.apply = flag(value)?,
            DEPLOY_PULL => self.deploy.pull = flag(value)?,
            DEPLOY_RECREATE => self.deploy.recreate = flag(value)?,
            DEPLOY_BACKUP => self.deploy.backup = value.map(BackupDefault::parse).transpose()?,
            LOGS_TAIL => {
                self.logs.tail = value
                    .map(|v| check_tail(v).map(|()| v.to_string()))
                    .transpose()?;
            }
            LOGS_TIMESTAMPS => self.logs.timestamps = flag(value)?,
            _ => unreachable!("not a scoped key: {key}"),
        }
        Ok(())
    }
}

/// The value of one of the [`SCOPED`] keys in a scope, if set.
fn get(
    key: &str,
    prefix: Option<&String>,
    deploy: &DeploySettings,
    logs: &LogsSettings,
) -> Option<String> {
    match key {
        PREFIX => prefix.cloned(),
        DEPLOY_APPLY => deploy.apply.map(|v| v.to_string()),
        DEPLOY_PULL => deploy.pull.map(|v| v.to_string()),
        DEPLOY_RECREATE => deploy.recreate.map(|v| v.to_string()),
        DEPLOY_BACKUP => deploy.backup.as_ref().map(|backup| match backup {
            BackupDefault::Off => "false".to_string(),
            BackupDefault::Profile(name) => name.clone(),
        }),
        LOGS_TAIL => logs.tail.clone(),
        LOGS_TIMESTAMPS => logs.timestamps.map(|v| v.to_string()),
        _ => unreachable!("not a scoped key: {key}"),
    }
}

/// Set (`Some`) or remove (`None`) a key of [`SCOPED`], globally or for one server.
fn update(home: &CompostHome, key: &str, server: Option<&str>, value: Option<&str>) -> Result<()> {
    check_key(key)?;
    home.require_manager()?;
    let scope = if let Some(name) = server {
        let mut servers = home.servers()?;
        let entry = servers
            .servers
            .iter_mut()
            .find(|s| s.name == name)
            .ok_or_else(|| config_err(format!("unknown server `{name}`")))?;
        Scope {
            prefix: &mut entry.prefix,
            deploy: &mut entry.deploy,
            logs: &mut entry.logs,
        }
        .set(key, value)?;
        home.save_servers(&servers)?;
        format!(" for {name}")
    } else {
        let mut settings = home.settings()?;
        Scope {
            prefix: &mut settings.prefix,
            deploy: &mut settings.deploy,
            logs: &mut settings.logs,
        }
        .set(key, value)?;
        home.save_settings(&settings)?;
        String::new()
    };
    if let Some(value) = value {
        out!("{key} = {value}{scope}");
    } else {
        out!("{key} unset{scope}");
    }
    Ok(())
}

fn show(home: &CompostHome) -> Result<()> {
    let settings = home.settings()?;
    let servers = home.servers()?.servers;
    for (key, default) in SCOPED {
        match get(
            key,
            settings.prefix.as_ref(),
            &settings.deploy,
            &settings.logs,
        ) {
            Some(value) => out!("{key} = {value}"),
            None => out!("{key} = {default} (default)"),
        }
        for server in &servers {
            if let Some(value) = get(key, server.prefix.as_ref(), &server.deploy, &server.logs) {
                out!("{key} = {value}  (server {})", server.name);
            }
        }
    }
    out!(
        "{DEV_SERVER} = {}",
        settings.dev.server.as_deref().unwrap_or("- (default)")
    );
    for server in &servers {
        if !server.trace.is_none() {
            out!(
                "{TRACE} = {}  (server {})",
                server.trace.name(),
                server.name
            );
        }
    }
    Ok(())
}
