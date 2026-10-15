//! Server configuration: `depap-server.yaml` + `.depap-server.secrets.yaml` (spec §5).

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use anyhow::Result;
use serde::Deserialize;

use super::{check_name, merge};
use crate::error::config_err;
use crate::shared::{self, SharedAppConfig};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Shared apps grouped by kind. Core does not know kind names; each key is
    /// looked up in the kind registry during validation.
    #[serde(default)]
    pub shared: BTreeMap<String, Vec<SharedAppConfig>>,
}

impl ServerConfig {
    /// Merge, parse and validate the given files.
    pub fn load(files: &[impl AsRef<Path>]) -> Result<Self> {
        let merged = merge::load_merged(files)?;
        let config: ServerConfig = serde_yaml_ng::from_value(merged)
            .map_err(|e| config_err(format!("server configuration: {e}")))?;
        config.validate()?;
        Ok(config)
    }

    /// Parse and validate one document, e.g. the effective config stored on a server.
    pub fn parse(text: &str) -> Result<Self> {
        let config: ServerConfig = serde_yaml_ng::from_str(text)
            .map_err(|e| config_err(format!("server configuration: {e}")))?;
        config.validate()?;
        Ok(config)
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
