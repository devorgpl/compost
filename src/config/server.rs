//! Server configuration: `compost-server.yaml` + `.compost-server.secrets.yaml` (spec §5).

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_yaml_ng::Value;

use super::app::{AppConfig, link_name};
use super::{check_name, merge};
use crate::error::config_err;
use crate::shared::{self, Kind, Resolution, SharedAppConfig};

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

/// A shared app that links to other shared apps, deployed like an application.
pub struct Consumer {
    pub app: AppConfig,
    pub links: Vec<ConsumerLink>,
}

/// One link of a [`Consumer`], resolved to a configured shared app.
pub struct ConsumerLink {
    pub kind: &'static dyn Kind,
    pub name: String,
    pub entry: Value,
    pub shared: SharedAppConfig,
}

impl ConsumerLink {
    /// e.g. `services.database[db]`, for error messages.
    pub fn path(&self) -> String {
        format!("services.{}[{}]", self.kind.name(), self.name)
    }
}

/// Where `backup` writes archives and `restore` reads them by default.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BackupSettings {
    #[serde(default)]
    pub target: BackupTarget,
    /// Archive folder of the `server` target: absolute, or relative to
    /// COMPOST_SERVER_HOME. Default `backups`.
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

    /// The links of a shared app to other shared apps of this config, in
    /// apply order; `None` when it has no links.
    pub fn consumer(&self, kind_name: &str, app: &SharedAppConfig) -> Result<Option<Consumer>> {
        let kind =
            shared::find_kind(kind_name).with_context(|| format!("unknown kind `{kind_name}`"))?;
        let Some(links) = kind.shared_links(app)? else {
            return Ok(None);
        };
        let app = shared::consumer_app(kind_name, app, links);
        let mut resolved = Vec::new();
        for (link_kind_name, entry) in app.links() {
            let link_kind = shared::find_kind(link_kind_name)
                .with_context(|| format!("unknown kind `{link_kind_name}`"))?;
            let name = link_name(entry).context("every link needs a name")?;
            let path = format!("services.{link_kind_name}[{name}]");
            link_kind
                .validate_link(entry, &app)
                .with_context(|| path.clone())?;
            let available = self
                .shared
                .get(link_kind_name)
                .map_or(&[][..], Vec::as_slice);
            let shared = match link_kind
                .resolve(entry, available)
                .with_context(|| path.clone())?
            {
                Resolution::Use(shared) => shared,
                Resolution::Skip(reason) => anyhow::bail!("{path}: {reason}"),
            };
            if link_kind.shared_links(shared)?.is_some() {
                anyhow::bail!(
                    "{path}: {link_kind_name} {} uses other shared apps itself, which is not supported",
                    shared.name
                );
            }
            resolved.push(ConsumerLink {
                kind: link_kind,
                name: name.to_string(),
                entry: entry.clone(),
                shared: shared.clone(),
            });
        }
        resolved.sort_by_key(|link| link.kind.apply_order());
        Ok(Some(Consumer {
            app,
            links: resolved,
        }))
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
        // Links between shared apps, once every shared app is valid on its own.
        for (kind_name, app) in self.shared_apps() {
            self.consumer(kind_name, app)
                .map_err(|e| config_err(format!("shared.{kind_name}[{}]: {e:#}", app.name)))?;
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
        let secrets = dir.join(".compost-server.secrets.example.yaml");
        let real = ServerConfig::load(&[dir.join("compost-server.yaml"), secrets]).unwrap();
        assert_eq!(real.shared_apps().count(), 3);
        let local = ServerConfig::load(&[
            dir.join("compost-server.yaml"),
            dir.join("compost-server.local.yaml"),
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
    fn keycloak_picks_one_postgres_18() {
        let config = |pin: &str| {
            format!(
                "
shared:
  ingress: [{{name: caddy-main, type: caddy}}]
  database:
    - {{name: pg18, type: postgres, version: 18}}
    - {{name: pg19, type: postgres, version: 19}}
  oidc:
    - {{name: kc, type: keycloak, configuration: {{url: 'https://a.example.com'{pin}}}}}
"
            )
        };
        let err = error(&config(""));
        assert!(
            err.contains(
                "shared.oidc[kc]: services.database[db]: several databases match (pg18, pg19)"
            ),
            "{err}"
        );
        let parsed = parse(&config(", database: {instance: pg19}")).unwrap();
        let kc = &parsed.shared["oidc"][0];
        let consumer = parsed.consumer("oidc", kc).unwrap().unwrap();
        let used: Vec<_> = consumer
            .links
            .iter()
            .map(|l| (l.kind.name(), l.shared.name.as_str()))
            .collect();
        assert_eq!(used, [("database", "pg19"), ("ingress", "caddy-main")]);
        assert_eq!(consumer.app.instance(), "compost-oidc-kc");
    }

    #[test]
    fn error_contains_config_path() {
        let err = error("shared: {database: [{name: pg, type: postgres}]}");
        assert!(err.starts_with("shared.database[pg]: "), "{err}");
    }
}
