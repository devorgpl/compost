//! Files in `COMPOST_HOME` on the operator's machine (spec §4).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::error::config_err;

const MANAGER_FILE: &str = "compost-manager.yaml";
const SERVERS_FILE: &str = "servers.yaml";
const CONTEXT_FILE: &str = "current_context";
const SETTINGS_FILE: &str = "config.yaml";
const SSH_CONTROL_DIR: &str = "ssh";
const LOGS_DIR: &str = "logs";
const TRACE_FILE: &str = "trace.log";
pub const DEFAULT_SSH_USER: &str = "compostuser";
pub const DEFAULT_SSH_PORT: u16 = 22;
const DEFAULT_SERVER_HOME: &str = "~/.compost-server";

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manager {
    pub manager_key: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Servers {
    #[serde(default)]
    pub servers: Vec<ServerEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerEntry {
    pub name: String,
    pub connector: ConnectorConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_home: Option<String>,
    /// Overrides the global `prefix` for this server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    /// Overrides the global `deploy` settings for this server.
    #[serde(default, skip_serializing_if = "DeploySettings::is_empty")]
    pub deploy: DeploySettings,
    /// Overrides the global `logs` settings for this server.
    #[serde(default, skip_serializing_if = "LogsSettings::is_empty")]
    pub logs: LogsSettings,
    /// What `logs/<name>/trace.log` records of the scripts run on the server.
    #[serde(default, skip_serializing_if = "TraceLevel::is_none")]
    pub trace: TraceLevel,
}

/// `trace` of a server in servers.yaml. Each level records what the one
/// before it does, and more.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TraceLevel {
    #[default]
    None,
    /// Each script, when it starts.
    Command,
    /// Its exit status and duration, and stderr when it fails.
    Basic,
    /// Its stdout and stderr, and how much it got on stdin.
    Verbose,
}

impl TraceLevel {
    pub const NAMES: [&str; 4] = ["none", "command", "basic", "verbose"];

    pub fn is_none(&self) -> bool {
        *self == TraceLevel::None
    }

    pub fn parse(name: &str) -> Option<TraceLevel> {
        serde_yaml_ng::from_str(name).ok()
    }

    pub fn name(self) -> &'static str {
        Self::NAMES[self as usize]
    }
}

/// Preferences of the operator, `config.yaml` in `COMPOST_HOME`.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Instance prefix when neither `--prefix` nor the config files give one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    #[serde(default, skip_serializing_if = "DeploySettings::is_empty")]
    pub deploy: DeploySettings,
    #[serde(default, skip_serializing_if = "LogsSettings::is_empty")]
    pub logs: LogsSettings,
    #[serde(default, skip_serializing_if = "DevSettings::is_empty")]
    pub dev: DevSettings,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DevSettings {
    /// The local server `compost dev` uses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
}

impl DevSettings {
    pub fn is_empty(&self) -> bool {
        self.server.is_none()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploySettings {
    /// `deploy` also starts services, as if `--apply` was given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apply: Option<bool>,
    /// An applied `deploy` pulls images again, as if `--pull` was given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull: Option<bool>,
    /// An applied `deploy` recreates every container, as if `--recreate` was given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recreate: Option<bool>,
    /// The backup `app deploy` takes before it changes an instance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<BackupDefault>,
}

impl DeploySettings {
    pub fn is_empty(&self) -> bool {
        self.apply.is_none()
            && self.pull.is_none()
            && self.recreate.is_none()
            && self.backup.is_none()
    }
}

/// `deploy.backup`: `false` skips the app's `deployment.backup`, a profile
/// name replaces it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackupDefault {
    Off,
    Profile(String),
}

impl BackupDefault {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "false" | "no" | "off" => Ok(BackupDefault::Off),
            "true" | "yes" | "on" => Err(config_err(
                "deploy.backup: `true` is not a profile; give a profile name or `false`",
            )),
            name => {
                crate::config::check_name("backup profile", name)?;
                Ok(BackupDefault::Profile(name.to_string()))
            }
        }
    }

    /// The profile to back up with, `None` for no backup.
    pub fn profile(&self) -> Option<&str> {
        match self {
            BackupDefault::Off => None,
            BackupDefault::Profile(name) => Some(name),
        }
    }
}

impl Serialize for BackupDefault {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            BackupDefault::Off => serializer.serialize_bool(false),
            BackupDefault::Profile(name) => serializer.serialize_str(name),
        }
    }
}

impl<'de> Deserialize<'de> for BackupDefault {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Bool(bool),
            Name(String),
        }
        match Raw::deserialize(deserializer)? {
            Raw::Bool(false) => Ok(BackupDefault::Off),
            Raw::Bool(true) => Err(serde::de::Error::custom(
                "`true` is not a backup profile; give a profile name or `false`",
            )),
            Raw::Name(name) => BackupDefault::parse(&name).map_err(serde::de::Error::custom),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogsSettings {
    /// Default of `--tail`: a number of lines or `all`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tail: Option<String>,
    /// `logs` shows the time of every line, as if `--timestamps` was given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamps: Option<bool>,
}

impl LogsSettings {
    pub fn is_empty(&self) -> bool {
        self.tail.is_none() && self.timestamps.is_none()
    }
}

pub const DEFAULT_LOGS_TAIL: &str = "100";

/// `logs.tail`: digits or `all`.
pub fn check_tail(value: &str) -> Result<()> {
    if value == "all" || (!value.is_empty() && value.bytes().all(|b| b.is_ascii_digit())) {
        Ok(())
    } else {
        Err(config_err(format!(
            "logs.tail: `{value}` is not a number of lines or `all`"
        )))
    }
}

/// The preferences that apply to one server: its own values over the global
/// ones over the built-in defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Defaults {
    pub prefix: String,
    pub apply: bool,
    pub pull: bool,
    pub recreate: bool,
    pub backup: Option<BackupDefault>,
    pub logs_tail: String,
    pub logs_timestamps: bool,
}

impl Defaults {
    pub fn merge(global: &Settings, server: &ServerEntry) -> Self {
        let (g, s) = (&global.deploy, &server.deploy);
        Defaults {
            prefix: server
                .prefix
                .clone()
                .or_else(|| global.prefix.clone())
                .unwrap_or_else(crate::config::app::default_prefix),
            apply: s.apply.or(g.apply).unwrap_or(false),
            pull: s.pull.or(g.pull).unwrap_or(false),
            recreate: s.recreate.or(g.recreate).unwrap_or(false),
            backup: s.backup.clone().or_else(|| g.backup.clone()),
            logs_tail: server
                .logs
                .tail
                .clone()
                .or_else(|| global.logs.tail.clone())
                .unwrap_or_else(|| DEFAULT_LOGS_TAIL.to_string()),
            logs_timestamps: server
                .logs
                .timestamps
                .or(global.logs.timestamps)
                .unwrap_or(false),
        }
    }
}

impl ServerEntry {
    pub fn server_home(&self) -> &str {
        self.server_home.as_deref().unwrap_or(DEFAULT_SERVER_HOME)
    }
}

/// How compost reaches a server. `type: ssh` or `type: local` in servers.yaml.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum ConnectorConfig {
    Ssh {
        address: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        port: Option<u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        identity_file: Option<String>,
    },
    Local,
}

impl ConnectorConfig {
    /// Short human-readable form, e.g. `ssh compostuser@10.0.0.1:22`.
    pub fn describe(&self) -> String {
        match self {
            ConnectorConfig::Ssh {
                address,
                port,
                user,
                identity_file,
            } => {
                let user = user.as_deref().unwrap_or(DEFAULT_SSH_USER);
                let port = port.unwrap_or(DEFAULT_SSH_PORT);
                match identity_file {
                    Some(key) => format!("ssh {user}@{address}:{port} key {key}"),
                    None => format!("ssh {user}@{address}:{port}"),
                }
            }
            ConnectorConfig::Local => "local".to_string(),
        }
    }
}

/// The local compost directory, `~/.compost` unless `COMPOST_HOME` is set.
pub struct CompostHome {
    root: PathBuf,
}

impl CompostHome {
    pub fn locate() -> Result<Self> {
        let root = match std::env::var_os("COMPOST_HOME") {
            Some(path) => PathBuf::from(path),
            None => dirs::home_dir()
                .context("cannot determine the home directory; set COMPOST_HOME")?
                .join(".compost"),
        };
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn manager(&self) -> Result<Option<Manager>> {
        read_yaml_if_exists(&self.root.join(MANAGER_FILE))
    }

    /// The manager, or an error telling the user to run `compost init`.
    pub fn require_manager(&self) -> Result<Manager> {
        self.manager()?
            .ok_or_else(|| config_err("compost is not initialized; run `compost init` first"))
    }

    pub fn save_manager(&self, manager: &Manager) -> Result<()> {
        write_yaml(&self.root.join(MANAGER_FILE), manager)
    }

    pub fn servers_file_exists(&self) -> bool {
        self.root.join(SERVERS_FILE).exists()
    }

    pub fn servers(&self) -> Result<Servers> {
        self.require_manager()?;
        Ok(read_yaml_if_exists(&self.root.join(SERVERS_FILE))?.unwrap_or_default())
    }

    pub fn save_servers(&self, servers: &Servers) -> Result<()> {
        write_yaml(&self.root.join(SERVERS_FILE), servers)
    }

    pub fn find_server(&self, name: &str) -> Result<ServerEntry> {
        self.servers()?
            .servers
            .into_iter()
            .find(|s| s.name == name)
            .ok_or_else(|| {
                config_err(format!(
                    "unknown server `{name}`; see `compost server list` or add it with `compost server define`"
                ))
            })
    }

    /// Directory for SSH connection sockets (created with mode 0700).
    pub fn ssh_control_dir(&self) -> Result<PathBuf> {
        use std::os::unix::fs::DirBuilderExt;

        let dir = self.root.join(SSH_CONTROL_DIR);
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .with_context(|| format!("cannot create {}", dir.display()))?;
        Ok(dir)
    }

    /// `logs/<server>/trace.log`, see [`TraceLevel`].
    pub fn trace_log(&self, server: &str) -> PathBuf {
        self.root.join(LOGS_DIR).join(server).join(TRACE_FILE)
    }

    pub fn settings(&self) -> Result<Settings> {
        Ok(read_yaml_if_exists(&self.root.join(SETTINGS_FILE))?.unwrap_or_default())
    }

    pub fn save_settings(&self, settings: &Settings) -> Result<()> {
        write_yaml(&self.root.join(SETTINGS_FILE), settings)
    }

    /// The preferences for `server`: its own, then the global ones, else the
    /// built-in defaults.
    pub fn defaults(&self, server: &ServerEntry) -> Result<Defaults> {
        Ok(Defaults::merge(&self.settings()?, server))
    }

    pub fn current_context(&self) -> Result<Option<String>> {
        let path = self.root.join(CONTEXT_FILE);
        if !path.exists() {
            return Ok(None);
        }
        let name =
            fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
        Ok(Some(name.trim().to_string()).filter(|n| !n.is_empty()))
    }

    pub fn set_current_context(&self, name: Option<&str>) -> Result<()> {
        let path = self.root.join(CONTEXT_FILE);
        match name {
            Some(name) => fs::write(&path, format!("{name}\n")),
            None if path.exists() => fs::remove_file(&path),
            None => Ok(()),
        }
        .with_context(|| format!("cannot update {}", path.display()))
    }

    /// Target server: `--server`/`COMPOST_SERVER` (handled by clap), then current context (spec §4.5).
    pub fn resolve_server(&self, flag: Option<&str>) -> Result<ServerEntry> {
        let name = match flag {
            Some(name) => name.to_string(),
            None => self.current_context()?.ok_or_else(|| {
                config_err("no server selected; use --server, COMPOST_SERVER or `compost connect`")
            })?,
        };
        self.find_server(&name)
    }
}

fn read_yaml_if_exists<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    if !path.exists() {
        return Ok(None);
    }
    let text =
        fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let value = serde_yaml_ng::from_str(&text)
        .map_err(|e| config_err(format!("{}: {e}", path.display())))?;
    Ok(Some(value))
}

fn write_yaml<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    let text = serde_yaml_ng::to_string(value)?;
    fs::write(path, text).with_context(|| format!("cannot write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(yaml: &str) -> ServerEntry {
        serde_yaml_ng::from_str(&format!("{{name: s, connector: {{type: local}}, {yaml}}}"))
            .unwrap()
    }

    #[test]
    fn defaults_take_the_server_over_the_global_values() {
        let builtin = Defaults::merge(&Settings::default(), &server("trace: none"));
        assert_eq!(
            builtin,
            Defaults {
                prefix: "main".to_string(),
                apply: false,
                pull: false,
                recreate: false,
                backup: None,
                logs_tail: "100".to_string(),
                logs_timestamps: false,
            }
        );

        let global: Settings = serde_yaml_ng::from_str(
            "{prefix: prod, deploy: {apply: true, pull: true, backup: daily}, logs: {tail: '20'}}",
        )
        .unwrap();
        let own = server(
            "prefix: staging, deploy: {pull: false, recreate: true, backup: false}, logs: {timestamps: true}",
        );
        assert_eq!(
            Defaults::merge(&global, &own),
            Defaults {
                prefix: "staging".to_string(),
                apply: true,
                pull: false,
                recreate: true,
                backup: Some(BackupDefault::Off),
                logs_tail: "20".to_string(),
                logs_timestamps: true,
            }
        );
        assert_eq!(
            Defaults::merge(&global, &server("trace: none")).backup,
            Some(BackupDefault::Profile("daily".to_string()))
        );
    }

    #[test]
    fn unset_settings_are_left_out_of_the_files() {
        let entry = server("trace: none");
        let text = serde_yaml_ng::to_string(&entry).unwrap();
        assert!(!text.contains("deploy"), "{text}");
        assert!(!text.contains("logs"), "{text}");
        assert!(!text.contains("prefix"), "{text}");

        let entry = server("prefix: staging, deploy: {backup: false}, logs: {tail: all}");
        let text = serde_yaml_ng::to_string(&entry).unwrap();
        let back: ServerEntry = serde_yaml_ng::from_str(&text).unwrap();
        assert_eq!(back.prefix.as_deref(), Some("staging"));
        assert_eq!(back.deploy.backup, Some(BackupDefault::Off));
        assert_eq!(back.logs.tail.as_deref(), Some("all"));
    }

    #[test]
    fn backup_is_a_profile_or_false() {
        assert!(serde_yaml_ng::from_str::<DeploySettings>("backup: true").is_err());
        assert!(serde_yaml_ng::from_str::<DeploySettings>("backup: Daily").is_err());
        assert!(BackupDefault::parse("true").is_err());
        assert_eq!(BackupDefault::parse("off").unwrap(), BackupDefault::Off);
        assert_eq!(
            BackupDefault::parse("daily").unwrap().profile(),
            Some("daily")
        );
        assert_eq!(BackupDefault::Off.profile(), None);
    }

    #[test]
    fn tail_is_a_number_or_all() {
        assert!(check_tail("all").is_ok());
        assert!(check_tail("250").is_ok());
        assert!(check_tail("").is_err());
        assert!(check_tail("-5").is_err());
        assert!(check_tail("lots").is_err());
    }
}
