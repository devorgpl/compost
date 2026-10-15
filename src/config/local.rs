//! Files in DEPAP_HOME on the operator's machine (spec §4).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::error::config_err;

const MANAGER_FILE: &str = "depap-manager.yaml";
const SERVERS_FILE: &str = "servers.yaml";
const CONTEXT_FILE: &str = "current_context";
const DEFAULT_SSH_USER: &str = "depapuser";
const DEFAULT_SSH_PORT: u16 = 22;
const DEFAULT_SERVER_HOME: &str = "~/.depap-server";

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
    pub connector: Connector,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_home: Option<String>,
}

impl ServerEntry {
    pub fn server_home(&self) -> &str {
        self.server_home.as_deref().unwrap_or(DEFAULT_SERVER_HOME)
    }
}

/// How depap reaches a server. `type: ssh` or `type: local` in servers.yaml.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum Connector {
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

impl Connector {
    /// Short human-readable form, e.g. `ssh depapuser@10.0.0.1:22`.
    pub fn describe(&self) -> String {
        match self {
            Connector::Ssh {
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
            Connector::Local => "local".to_string(),
        }
    }
}

/// The local depap directory, `~/.depap` unless `DEPAP_HOME` is set.
pub struct DepapHome {
    root: PathBuf,
}

impl DepapHome {
    pub fn locate() -> Result<Self> {
        let root = match std::env::var_os("DEPAP_HOME") {
            Some(path) => PathBuf::from(path),
            None => dirs::home_dir()
                .context("cannot determine the home directory; set DEPAP_HOME")?
                .join(".depap"),
        };
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn manager(&self) -> Result<Option<Manager>> {
        read_yaml_if_exists(&self.root.join(MANAGER_FILE))
    }

    /// The manager, or an error telling the user to run `depap init`.
    pub fn require_manager(&self) -> Result<Manager> {
        self.manager()?
            .ok_or_else(|| config_err("depap is not initialized; run `depap init` first"))
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
                    "unknown server `{name}`; see `depap server list` or add it with `depap server define`"
                ))
            })
    }

    pub fn current_context(&self) -> Result<Option<String>> {
        let path = self.root.join(CONTEXT_FILE);
        if !path.exists() {
            return Ok(None);
        }
        let name = fs::read_to_string(&path)
            .with_context(|| format!("cannot read {}", path.display()))?;
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

    /// Target server: `--server`/`DEPAP_SERVER` (handled by clap), then current context (spec §4.4).
    pub fn resolve_server(&self, flag: Option<&str>) -> Result<ServerEntry> {
        let name = match flag {
            Some(name) => name.to_string(),
            None => self.current_context()?.ok_or_else(|| {
                config_err("no server selected; use --server, DEPAP_SERVER or `depap connect`")
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
