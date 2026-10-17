//! Server configuration: `compost-server.yaml` + `.compost-server.secrets.yaml` (spec §5).

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_yaml_ng::Value;

use super::app::{AppConfig, link_name};
use super::{check_name, merge};
use crate::error::config_err;
use crate::shared::{self, Kind, Monitored, Resolution, SharedAppConfig, SharedMonitoring};

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

impl Consumer {
    pub fn peers(&self) -> Vec<shared::Peer<'_>> {
        self.links
            .iter()
            .map(|link| shared::Peer {
                kind: link.kind,
                name: &link.name,
                entry: &link.entry,
                shared: &link.shared,
            })
            .collect()
    }
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

const MONITORING: &str = "monitoring";

/// The `services.monitoring` entry of a shared app with `monitoring`.
fn monitoring_link(monitoring: &SharedMonitoring, monitored: &Monitored) -> Result<Value> {
    let mut link = serde_yaml_ng::Mapping::new();
    link.insert("name".into(), MONITORING.into());
    if let Some(instance) = &monitoring.instance {
        link.insert("instance".into(), instance.as_str().into());
    }
    link.insert("logs".into(), monitoring.logs.into());
    if monitoring.logs {
        let targets: Vec<Value> = monitored
            .services
            .iter()
            .map(|s| s.name.as_str().into())
            .collect();
        link.insert("targets".into(), Value::Sequence(targets));
    }
    if monitoring.metrics && !monitored.metrics.is_empty() {
        let metrics: Vec<Value> = monitored
            .metrics
            .iter()
            .map(|m| {
                serde_yaml_ng::to_value(BTreeMap::from([
                    ("target", Value::from(m.service)),
                    ("port", Value::from(m.port)),
                    ("path", Value::from(m.path)),
                ]))
            })
            .collect::<Result<_, _>>()?;
        link.insert("metrics".into(), Value::Sequence(metrics));
    }
    Ok(Value::Mapping(link))
}

/// Remove the shared apps of `overlay` that `config` does not have: merged,
/// they would be shared apps without a type. Returns them as `shared.<kind>[<name>]`.
fn without_unknown_apps(overlay: &mut Value, config: &Value) -> Vec<String> {
    let mut removed = Vec::new();
    let Some(Value::Mapping(kinds)) = overlay.get_mut("shared") else {
        return removed;
    };
    for (kind, apps) in kinds.iter_mut() {
        let Value::Sequence(apps) = apps else {
            continue;
        };
        let known = config.get("shared").and_then(|shared| shared.get(kind));
        apps.retain(|app| {
            let name = app.get("name");
            let found = known
                .and_then(Value::as_sequence)
                .is_some_and(|known| known.iter().any(|k| k.get("name") == name));
            if !found {
                removed.push(format!(
                    "shared.{}[{}]",
                    kind.as_str().unwrap_or_default(),
                    name.and_then(Value::as_str).unwrap_or_default()
                ));
            }
            found
        });
    }
    removed
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
    #[cfg(test)]
    pub fn load(files: &[impl AsRef<Path>]) -> Result<Self> {
        Ok(Self::load_with(files, None)?.0)
    }

    /// Like [`ServerConfig::load`], with `stored` (secrets kept on the server)
    /// merged right after the first file. Its shared apps that the files
    /// don't have are left out; returns them as `shared.<kind>[<name>]`.
    pub fn load_with(
        files: &[impl AsRef<Path>],
        stored: Option<Value>,
    ) -> Result<(Self, Vec<String>)> {
        let split = files.len().min(1);
        let mut merged = merge::load_merged(&files[..split])?;
        let mut skipped = Vec::new();
        if let Some(mut stored) = stored {
            skipped = without_unknown_apps(&mut stored, &merge::load_merged(files)?);
            merge::merge(&mut merged, stored);
        }
        merge::merge_files(&mut merged, &files[split..])?;
        let config: ServerConfig = serde_yaml_ng::from_value(merged)
            .map_err(|e| config_err(format!("server configuration: {e}")))?;
        Ok((config.complete()?, skipped))
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
    /// apply order: its type's own and its `monitoring`; `None` when it has
    /// no links.
    pub fn consumer(&self, kind_name: &str, app: &SharedAppConfig) -> Result<Option<Consumer>> {
        let kind =
            shared::find_kind(kind_name).with_context(|| format!("unknown kind `{kind_name}`"))?;
        let mut links = kind.shared_links(app)?;
        if let Some(monitoring) = &app.monitoring {
            let monitored = kind.monitored(app)?;
            let links = links.get_or_insert_with(Default::default);
            for service in &monitored.services {
                if !links.app_services.iter().any(|s| s.name == service.name) {
                    links.app_services.push(service.clone());
                }
            }
            links
                .services
                .entry(MONITORING.to_string())
                .or_default()
                .push(monitoring_link(monitoring, &monitored)?);
        }
        let Some(links) = links else {
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

    /// Shared apps in the order they start: a shared app starts after the
    /// shared apps it links to, unless the link's kind does not need that
    /// (see [`Kind::orders_start`]). Otherwise in config order.
    pub fn start_order(&self) -> Result<Vec<(&str, &SharedAppConfig)>> {
        let apps: Vec<(&str, &SharedAppConfig)> = self.shared_apps().collect();
        // For each shared app, the indexes of the ones it waits for.
        let mut waits_for: Vec<Vec<usize>> = Vec::new();
        for (kind_name, app) in &apps {
            let mut before = Vec::new();
            if let Some(consumer) = self.consumer(kind_name, app)? {
                for link in consumer.links.iter().filter(|l| l.kind.orders_start()) {
                    let index = apps
                        .iter()
                        .position(|(k, a)| *k == link.kind.name() && a.name == link.shared.name)
                        .context("links resolve to configured shared apps")?;
                    before.push(index);
                }
            }
            waits_for.push(before);
        }
        let mut order: Vec<usize> = Vec::new();
        while order.len() < apps.len() {
            let next = (0..apps.len()).find(|i| {
                !order.contains(i) && waits_for[*i].iter().all(|before| order.contains(before))
            });
            match next {
                Some(i) => order.push(i),
                None => {
                    let stuck: Vec<String> = (0..apps.len())
                        .filter(|i| !order.contains(i))
                        .map(|i| format!("{}[{}]", apps[i].0, apps[i].1.name))
                        .collect();
                    anyhow::bail!(
                        "shared apps link to each other in a cycle: {}",
                        stuck.join(", ")
                    );
                }
            }
        }
        Ok(order.into_iter().map(|i| apps[i]).collect())
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
                if app.is_external() && !kind.supports_external() {
                    return Err(config_err(format!(
                        "{path}: `external` is not supported for {kind_name} servers"
                    )));
                }
                kind.validate_shared(app)
                    .map_err(|e| config_err(format!("{path}: {e:#}")))?;
            }
            kind.validate_instances(apps)
                .map_err(|e| config_err(format!("shared.{kind_name}: {e:#}")))?;
        }
        // Links between shared apps, once every shared app is valid on its own.
        for (kind_name, app) in self.shared_apps() {
            let path = format!("shared.{kind_name}[{}]", app.name);
            if app.monitoring.is_some() && app.is_external() {
                return Err(config_err(format!(
                    "{path}: `monitoring` is not supported with `external`: compost does not run it"
                )));
            }
            self.consumer(kind_name, app)
                .map_err(|e| config_err(format!("{path}: {e:#}")))?;
        }
        self.start_order()
            .map_err(|e| config_err(format!("{e:#}")))?;
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
    fn stored_secrets_merge_after_the_first_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let write = |name: &str, text: &str| {
            let path = dir.path().join(name);
            std::fs::write(&path, text).unwrap();
            path
        };
        let base = write(
            "compost-server.yaml",
            "shared: {database: [{name: pg, type: postgres, version: 16}]}",
        );
        let local = write(
            "local.yaml",
            "shared: {database: [{name: pg, configuration: {root_password: local}}]}",
        );
        let stored: Value = serde_yaml_ng::from_str(
            "shared: {database: [{name: pg, configuration: {root_password: stored}}, {name: gone, configuration: {root_password: x}}], oidc: [{name: kc}]}",
        )
        .unwrap();
        let password = |config: &ServerConfig| {
            config.shared["database"][0].configuration["root_password"].clone()
        };

        let (config, skipped) =
            ServerConfig::load_with(std::slice::from_ref(&base), Some(stored.clone())).unwrap();
        assert_eq!(password(&config), Value::from("stored"));
        assert_eq!(config.shared["database"].len(), 1);
        assert_eq!(skipped, ["shared.database[gone]", "shared.oidc[kc]"]);

        let (config, _) = ServerConfig::load_with(&[base, local], Some(stored)).unwrap();
        assert_eq!(password(&config), Value::from("local"));
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
        let local = ServerConfig::load(&[dir.join("compost-server.yaml")]).unwrap();
        assert_eq!(local.shared_apps().count(), 4);
        let caddy = &local.shared["ingress"][0].configuration;
        assert_eq!(caddy["http_port"], 18080);
        let real = ServerConfig::load(&[
            dir.join("compost-server.yaml"),
            dir.join("compost-server.prod.yaml"),
            dir.join(".compost-server.secrets.example.yaml"),
        ])
        .unwrap();
        let caddy = &real.shared["ingress"][0].configuration;
        assert_eq!(caddy["http_port"], 80);
        assert_eq!(caddy["tls"], "auto");
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

    const MONITORED: &str = "
shared:
  ingress: [{name: caddy-main, type: caddy, monitoring: {}}]
  database: [{name: pg18, type: postgres, version: 18, monitoring: {metrics: false}}]
  monitoring:
    - {name: o2, type: openobserve, configuration: {url: 'https://logs.example.com'}}
  oidc:
    - {name: kc, type: keycloak, configuration: {url: 'https://a.example.com'}}
";

    #[test]
    fn monitoring_links_shared_apps_both_ways() {
        let config = parse(MONITORED).unwrap();
        // Caddy sends to o2, o2 is served by Caddy: o2 starts after Caddy only.
        let order: Vec<_> = config
            .start_order()
            .unwrap()
            .iter()
            .map(|(_, app)| app.name.as_str())
            .collect();
        assert_eq!(order, ["pg18", "caddy-main", "o2", "kc"]);

        let caddy = config.consumer("ingress", &config.shared["ingress"][0]);
        let caddy = caddy.unwrap().unwrap();
        assert_eq!(caddy.app.instance(), "compost-ingress-caddy-main");
        assert_eq!(
            caddy.app.app_services[0].service,
            "compost-shared-caddy-main"
        );
        let link = &caddy.links[0];
        assert_eq!(
            (link.kind.name(), link.shared.name.as_str()),
            ("monitoring", "o2")
        );
        assert_eq!(link.entry["targets"][0], "caddy");
        assert_eq!(link.entry["metrics"][0]["port"], 9180);

        let pg = config.consumer("database", &config.shared["database"][0]);
        let pg = pg.unwrap().unwrap();
        assert!(pg.links[0].entry.get("metrics").is_none());
        // Keycloak links to a database that has links itself.
        let kc = config.consumer("oidc", &config.shared["oidc"][0]).unwrap();
        assert_eq!(kc.unwrap().links.len(), 2);
    }

    #[test]
    fn monitoring_needs_a_monitoring_app_and_a_running_shared_app() {
        let err = error("shared: {ingress: [{name: c, type: caddy, monitoring: {}}]}");
        assert!(
            err.contains(
                "shared.ingress[c]: services.monitoring[monitoring]: no monitoring app is running"
            ),
            "{err}"
        );
        let err = error(
            "shared: {database: [{name: pg, type: postgres, version: 16, external: {host: db}, monitoring: {}}]}",
        );
        assert!(err.contains("not supported with `external`"), "{err}");
        let err = error("shared: {monitoring: [{name: o2, type: openobserve, monitoring: {}}]}");
        assert!(
            err.contains("not supported for monitoring shared apps"),
            "{err}"
        );
        let err = error(
            "shared: {monitoring: [{name: a, type: openobserve}, {name: b, type: openobserve}]}",
        );
        assert!(err.contains("both use forward_port 24224"), "{err}");
    }

    #[test]
    fn external_only_where_the_kind_supports_it() {
        let err = error("shared: {ingress: [{name: c, type: caddy, external: {host: x}}]}");
        assert!(
            err.contains("shared.ingress[c]: `external` is not supported for ingress servers"),
            "{err}"
        );
        let err = error(
            "shared: {database: [{name: pg, type: postgres, version: 16, configuration: {root_user: admin}}]}",
        );
        assert!(
            err.contains("root_user is only used with `external`"),
            "{err}"
        );
        parse("shared: {database: [{name: pg, type: postgres, version: 16, external: {host: db}, configuration: {root_user: admin, root_password: x}}]}")
            .unwrap();
    }

    #[test]
    fn error_contains_config_path() {
        let err = error("shared: {database: [{name: pg, type: postgres}]}");
        assert!(err.starts_with("shared.database[pg]: "), "{err}");
    }
}
