//! Server configuration: `depap-server.yaml` + `.depap-server.secrets.yaml` (spec §5).

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_yaml_ng::Value;

use super::{check_name, merge};
use crate::error::config_err;
use crate::shared::{self, SharedAppConfig};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Shared apps grouped by kind. Core does not know kind names; each key is
    /// looked up in the kind registry during validation.
    #[serde(default)]
    pub shared: BTreeMap<String, Vec<SharedAppConfig>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<BackupSettings>,
}

/// Where `backup` writes archives and `restore` reads them by default.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BackupSettings {
    #[serde(default)]
    pub target: BackupTarget,
    /// Archive folder of the `server` target: absolute, or relative to
    /// DEPAP_SERVER_HOME. Default `backups`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_path: Option<String>,
    /// Settings of the `s3` target (not implemented yet).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub s3: Option<Value>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum BackupTarget {
    /// This machine; data is streamed through the connector.
    #[default]
    Local,
    /// A folder on the server.
    Server,
    /// An S3 bucket (not implemented yet).
    S3,
}

impl ServerConfig {
    /// Merge, parse and validate the given files.
    pub fn load(files: &[impl AsRef<Path>]) -> Result<Self> {
        let merged = merge::load_merged(files)?;
        let config: ServerConfig = serde_yaml_ng::from_value(merged)
            .map_err(|e| config_err(format!("server configuration: {e}")))?;
        config.complete()
    }

    /// Parse and validate one document, e.g. the effective config stored on a server.
    pub fn parse(text: &str) -> Result<Self> {
        let config: ServerConfig = serde_yaml_ng::from_str(text)
            .map_err(|e| config_err(format!("server configuration: {e}")))?;
        config.complete()
    }

    /// Fill in defaults, then validate: the effective configuration (spec §7).
    fn complete(mut self) -> Result<Self> {
        for (kind_name, apps) in &mut self.shared {
            // Unknown kinds are reported by `validate`.
            if let Some(kind) = shared::find_kind(kind_name) {
                for app in apps {
                    kind.apply_defaults(app);
                }
            }
        }
        self.validate()?;
        Ok(self)
    }

    /// A copy without secret values, safe to store on the server.
    pub fn without_secrets(&self) -> Result<ServerConfig> {
        let mut copy = self.clone();
        for (kind_name, apps) in &mut copy.shared {
            let kind = shared::find_kind(kind_name)
                .with_context(|| format!("unknown kind `{kind_name}`"))?;
            for app in apps {
                let keys = kind.secret_keys(app)?;
                if let Value::Mapping(map) = &mut app.configuration {
                    for key in keys {
                        map.remove(*key);
                    }
                    if map.is_empty() {
                        app.configuration = Value::Null;
                    }
                }
            }
        }
        Ok(copy)
    }

    /// Every shared app with the name of its kind.
    pub fn shared_apps(&self) -> impl Iterator<Item = (&str, &SharedAppConfig)> {
        self.shared
            .iter()
            .flat_map(|(kind, apps)| apps.iter().map(move |app| (kind.as_str(), app)))
    }

    /// Core checks, then delegation to each kind (spec §6.3, §9.1).
    fn validate(&self) -> Result<()> {
        let mut names = HashSet::new();
        for (kind_name, apps) in &self.shared {
            let kind = shared::find_kind(kind_name).ok_or_else(|| {
                config_err(format!(
                    "shared.{kind_name}: unknown kind (known: {})",
                    shared::kind_names().join(", ")
                ))
            })?;

            for app in apps {
                let path = format!("shared.{kind_name}[{}]", app.name);
                check_name("shared app name", &app.name)?;
                if !names.insert(app.name.as_str()) {
                    return Err(config_err(format!("{path}: name is used more than once")));
                }
                kind.validate_shared(app)
                    .map_err(|e| config_err(format!("{path}: {e:#}")))?;
            }
            kind.validate_instances(apps)
                .map_err(|e| config_err(format!("shared.{kind_name}: {e:#}")))?;
        }
        if let Some(path) = self.backup.as_ref().and_then(|b| b.server_path.as_ref()) {
            if path.trim().is_empty() {
                return Err(config_err("backup.server_path must not be empty"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<ServerConfig> {
        ServerConfig::parse(text)
    }

    fn error(text: &str) -> String {
        format!("{:#}", parse(text).unwrap_err())
    }

    #[test]
    fn accepts_spec_example() {
        let config = parse(
            "
shared:
  ingress:
    - name: caddy-main
      type: caddy
      configuration: {http_port: 80, https_port: 443, tls: auto, acme_email: admin@domain.tld}
  database:
    - name: postgres16
      type: postgres
      version: 16
      configuration: {root_password: secret}
",
        )
        .unwrap();
        assert_eq!(config.shared_apps().count(), 2);
    }

    #[test]
    fn samples_are_valid() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("samples/server");
        let secrets = dir.join(".depap-server.secrets.example.yaml");
        let real = ServerConfig::load(&[dir.join("depap-server.yaml"), secrets]).unwrap();
        assert_eq!(real.shared_apps().count(), 2);
        let local = ServerConfig::load(&[
            dir.join("depap-server.yaml"),
            dir.join("depap-server.local.yaml"),
        ])
        .unwrap();
        let caddy = &local.shared["ingress"][0].configuration;
        assert_eq!(caddy["http_port"], 18080);
    }

    #[test]
    fn fills_in_default_versions() {
        let config = parse("shared: {ingress: [{name: caddy-main, type: caddy}]}").unwrap();
        assert_eq!(config.shared["ingress"][0].version, Some(2));
    }

    #[test]
    fn without_secrets_removes_secret_keys() {
        let config = parse(
            "shared: {database: [{name: pg, type: postgres, version: 16, configuration: {root_password: x}}]}",
        )
        .unwrap();
        let text = serde_yaml_ng::to_string(&config.without_secrets().unwrap()).unwrap();
        assert!(!text.contains("root_password"), "{text}");
        assert!(!text.contains("configuration"), "{text}");
        assert!(text.contains("name: pg"), "{text}");
    }

    #[test]
    fn backup_settings() {
        let config = parse("backup: {target: server, server_path: /srv/backups}").unwrap();
        let backup = config.backup.unwrap();
        assert_eq!(backup.target, BackupTarget::Server);
        assert!(error("backup: {target: ftp}").contains("unknown variant `ftp`"));
        assert!(error("backup: {server_path: ''}").contains("must not be empty"));
    }

    #[test]
    fn rejects_unknown_kind() {
        assert!(error("shared: {queue: []}").contains("shared.queue: unknown kind"));
    }

    #[test]
    fn rejects_duplicate_names_across_kinds() {
        let err = error(
            "
shared:
  ingress: [{name: main, type: caddy}]
  database: [{name: main, type: postgres, version: 16}]
",
        );
        assert!(err.contains("name is used more than once"), "{err}");
    }

    #[test]
    fn error_contains_config_path() {
        let err = error("shared: {database: [{name: pg, type: postgres}]}");
        assert!(err.starts_with("shared.database[pg]: "), "{err}");
    }
}
