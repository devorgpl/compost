//! Server configuration: `compost-server.yaml` + `.compost-server.secrets.yaml` (spec §5).

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_yaml_ng::Value;

use super::app::{AppConfig, link_name};
use super::backup::{BUILTIN_TARGETS, Profile, check_target};
use super::command::Template;
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notifications: Option<Notifications>,
    /// Named commands run in shared apps' containers (`server cmd`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub commands: BTreeMap<String, Template>,
}

/// What the server reports by itself, and to which communication apps.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Notifications {
    #[serde(default)]
    pub send_to: Vec<Recipient>,
    /// Percent of free disk space below which `disk` is sent.
    #[serde(default = "default_disk_free_below")]
    pub disk_free_below: u8,
    /// When `bans_daily` and `status` are sent and `updates` checked, `HH:MM`
    /// in the server's time zone.
    #[serde(default = "default_daily_at")]
    pub daily_at: String,
}

fn default_disk_free_below() -> u8 {
    10
}

fn default_daily_at() -> String {
    "08:00".to_string()
}

impl Notifications {
    /// `daily_at` as a time of day; validation checks it.
    pub fn daily_time(&self) -> Option<chrono::NaiveTime> {
        chrono::NaiveTime::parse_from_str(&self.daily_at, "%H:%M").ok()
    }

    /// Whether a recipient wants `event`.
    pub fn wants(&self, event: Event) -> bool {
        self.send_to.iter().any(|r| r.wants(event))
    }
}

/// Events the server reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    /// A scheduled backup failed, or works again.
    Backup,
    /// A shared app or instance is down, or up again.
    Health,
    /// Free disk space is low, or enough again.
    Disk,
    /// CrowdSec banned addresses, or would have (simulated scenarios).
    Bans,
    /// A summary of the bans of the last 24 hours.
    BansDaily,
    /// Shared apps' tags have newer images, or compost a newer release.
    Updates,
    /// A daily summary: state, uptime, disk, CPU, memory, backups and bans.
    Status,
}

impl Event {
    pub fn name(self) -> &'static str {
        match self {
            Event::Backup => "backup",
            Event::Health => "health",
            Event::Disk => "disk",
            Event::Bans => "bans",
            Event::BansDaily => "bans_daily",
            Event::Updates => "updates",
            Event::Status => "status",
        }
    }

    /// Sent only to recipients that list it: a server facing the internet
    /// bans scanners many times a day, moving tags get new images often, and
    /// `status` comes every day, also when all is fine.
    pub fn opt_in(self) -> bool {
        matches!(
            self,
            Event::Bans | Event::BansDaily | Event::Updates | Event::Status
        )
    }
}

/// An entry of `send_to`: a communication app's name (every event), or
/// `{to, events}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum Recipient {
    All(String),
    Some { to: String, events: Vec<Event> },
}

// By hand, so that mistakes are reported as such, not as "no variant matched".
impl<'de> Deserialize<'de> for Recipient {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Filtered {
            to: String,
            events: Vec<Event>,
        }
        match Value::deserialize(deserializer)? {
            Value::String(name) => Ok(Recipient::All(name)),
            value => serde_yaml_ng::from_value::<Filtered>(value)
                .map(|f| Recipient::Some {
                    to: f.to,
                    events: f.events,
                })
                .map_err(D::Error::custom),
        }
    }
}

impl Recipient {
    pub fn name(&self) -> &str {
        match self {
            Recipient::All(name) | Recipient::Some { to: name, .. } => name,
        }
    }

    pub fn wants(&self, event: Event) -> bool {
        match self {
            Recipient::All(_) => !event.opt_in(),
            Recipient::Some { events, .. } => events.contains(&event),
        }
    }
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
    ensure!(
        !monitoring.traces || monitored.traces,
        "monitoring.traces: this shared app sends no traces; only an ingress does"
    );
    if monitoring.logs || monitoring.traces {
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
    if monitoring.traces {
        let environment: serde_yaml_ng::Mapping = [
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "otlp_endpoint"),
            ("OTEL_EXPORTER_OTLP_PROTOCOL", "otlp_protocol"),
            ("OTEL_SERVICE_NAME", "service_name"),
            ("OTEL_RESOURCE_ATTRIBUTES", "resource_attributes"),
        ]
        .into_iter()
        .map(|(variable, value)| (variable.into(), value.into()))
        .collect();
        link.insert("environment".into(), Value::Mapping(environment));
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

/// Where `backup` writes archives and `restore` reads them by default, and
/// the backup profiles.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BackupSettings {
    /// `local`, `server` or a name from `targets`. Default `local`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Archive folder of the `server` target: absolute, or relative to
    /// `COMPOST_SERVER_HOME`. Default `backups`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_path: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub targets: BTreeMap<String, TargetConfig>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub profiles: BTreeMap<String, Profile>,
}

/// A named backup target.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum TargetConfig {
    /// Another folder on the server, e.g. a mounted disk.
    Server {
        /// Absolute, or relative to `COMPOST_SERVER_HOME`.
        path: String,
    },
    /// An S3 bucket, reached with rclone in a container on the server.
    S3 {
        bucket: String,
        /// Folder (key prefix) in the bucket.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        /// Default: AWS.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        endpoint: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        region: Option<String>,
        /// rclone's S3 provider, e.g. `AWS`, `Minio`, `Cloudflare`. Default `Other`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
        /// Where the credentials come from. Default `keys`.
        #[serde(default, skip_serializing_if = "S3Credentials::is_keys")]
        credentials: S3Credentials,
        /// Secret.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        access_key_id: Option<String>,
        /// Secret.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        secret_access_key: Option<String>,
        /// The rclone image.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        image: Option<String>,
    },
}

/// Where an S3 target's credentials come from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum S3Credentials {
    /// `access_key_id` and `secret_access_key`, stored on the server.
    #[default]
    Keys,
    /// The server's environment: on EC2, its instance role (rclone's `env_auth`).
    Instance,
}

impl S3Credentials {
    fn is_keys(&self) -> bool {
        *self == Self::Keys
    }
}

impl BackupSettings {
    fn validate(&self) -> Result<()> {
        if let Some(path) = &self.server_path {
            if path.trim().is_empty() {
                return Err(config_err("backup.server_path must not be empty"));
            }
        }
        for (name, target) in &self.targets {
            let path = format!("backup.targets.{name}");
            check_name("backup target name", name)?;
            if BUILTIN_TARGETS.contains(&name.as_str()) {
                return Err(config_err(format!(
                    "{path}: `{name}` is a built-in target; choose another name"
                )));
            }
            match target {
                TargetConfig::Server { path: dir } if dir.trim().is_empty() => {
                    return Err(config_err(format!("{path}.path must not be empty")));
                }
                TargetConfig::S3 { bucket, .. }
                    if bucket.trim().is_empty() || bucket.contains('/') =>
                {
                    return Err(config_err(format!(
                        "{path}.bucket: a bucket name, without `/` (put folders in `path`)"
                    )));
                }
                TargetConfig::Server { .. } | TargetConfig::S3 { .. } => {}
            }
        }
        let names = self.target_names();
        if let Some(target) = &self.target {
            check_target("backup.target", target, &names)?;
        }
        for (name, profile) in &self.profiles {
            profile.validate_server(name, &names)?;
        }
        Ok(())
    }

    /// Names of `targets`.
    pub fn target_names(&self) -> Vec<&str> {
        self.targets.keys().map(String::as_str).collect()
    }
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

    /// This config (e.g. the effective one stored on a server, without
    /// secrets) with `stored` merged over it, as `load_with` merges it.
    pub fn with_stored(&self, stored: Option<Value>) -> Result<Self> {
        let mut merged = serde_yaml_ng::to_value(self)?;
        if let Some(mut stored) = stored {
            without_unknown_apps(&mut stored, &merged);
            merge::merge(&mut merged, stored);
        }
        let config: ServerConfig = serde_yaml_ng::from_value(merged)
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
        if let Some(backup) = &mut copy.backup {
            for target in backup.targets.values_mut() {
                if let TargetConfig::S3 {
                    access_key_id,
                    secret_access_key,
                    ..
                } = target
                {
                    *access_key_id = None;
                    *secret_access_key = None;
                }
            }
        }
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
            if let Some(i) = next {
                order.push(i);
            } else {
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
        // `host_port`: the types that have it parse it; core keeps them apart.
        let mut host_ports: BTreeMap<u64, String> = BTreeMap::new();
        for (kind_name, app) in self.shared_apps() {
            let Some(port) = app.configuration.get("host_port") else {
                continue;
            };
            let path = format!("shared.{kind_name}[{}]", app.name);
            if app.is_external() {
                return Err(config_err(format!(
                    "{path}: host_port is not supported with `external`: compost does not run it"
                )));
            }
            let Some(port) = port.as_u64().filter(|p| (1..=65535).contains(p)) else {
                return Err(config_err(format!(
                    "{path}: host_port must be a port number"
                )));
            };
            if let Some(other) = host_ports.insert(port, path.clone()) {
                return Err(config_err(format!(
                    "{path}: host_port {port} is also the host_port of {other}"
                )));
            }
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
        if let Some(backup) = &self.backup {
            backup.validate()?;
        }
        if let Some(notifications) = &self.notifications {
            let channels: Vec<&str> = self
                .shared
                .get(crate::shared::communication::KIND)
                .map(|apps| apps.iter().map(|a| a.name.as_str()).collect())
                .unwrap_or_default();
            for recipient in &notifications.send_to {
                if !channels.contains(&recipient.name()) {
                    return Err(config_err(format!(
                        "notifications.send_to: `{}` is not a communication app (known: {})",
                        recipient.name(),
                        if channels.is_empty() {
                            "none; add one under shared.communication".to_string()
                        } else {
                            channels.join(", ")
                        }
                    )));
                }
                if let Recipient::Some { events, .. } = recipient {
                    if events.is_empty() {
                        return Err(config_err(format!(
                            "notifications.send_to[{}]: events is empty; leave the entry out instead",
                            recipient.name()
                        )));
                    }
                }
            }
            if !(1..=99).contains(&notifications.disk_free_below) {
                return Err(config_err(
                    "notifications.disk_free_below: a percentage from 1 to 99",
                ));
            }
            if notifications.daily_time().is_none() {
                return Err(config_err(format!(
                    "notifications.daily_at: `{}` is not a time like 08:00",
                    notifications.daily_at
                )));
            }
            let bans = [Event::Bans, Event::BansDaily]
                .into_iter()
                .find(|&event| notifications.wants(event));
            let secure = self
                .shared
                .get(crate::shared::ingress::KIND)
                .is_some_and(|apps| {
                    apps.iter()
                        .any(|a| a.type_name == "caddysecurebuild" && !a.is_external())
                });
            if let (Some(event), false) = (bans, secure) {
                return Err(config_err(format!(
                    "notifications.send_to: `{}` needs a caddysecurebuild ingress, whose CrowdSec bans addresses",
                    event.name()
                )));
            }
        }
        for (name, command) in &self.commands {
            check_name("command name", name)?;
            self.validate_command(command)
                .map_err(|e| config_err(format!("commands.{name}: {e:#}")))?;
        }
        Ok(())
    }

    fn validate_command(&self, command: &Template) -> Result<()> {
        if command.exec.is_some() || command.run.is_some() {
            anyhow::bail!("`exec` and `run` are for commands of compost.yaml; use `shared_app`");
        }
        let Some(shared_app) = &command.shared_app else {
            anyhow::bail!("`shared_app`: the shared app whose container runs the command");
        };
        let (kind, app) = self.find_shared(shared_app)?;
        if app.is_external() {
            anyhow::bail!(
                "{kind} {} is external: compost runs no containers for it",
                app.name
            );
        }
        if shared::find_kind(kind).is_some_and(|k| !k.runs_service(app)) {
            anyhow::bail!(
                "{kind} {} runs only when it sends; it has no container to run a command in",
                app.name
            );
        }
        if let Some(service) = &command.service {
            check_name("service", service)?;
        }
        command.validate(true)
    }

    /// A shared app by name, or `<kind>/<name>` when two kinds use the name.
    pub fn find_shared(&self, name: &str) -> Result<(&str, &SharedAppConfig)> {
        let (kind_filter, name) = match name.split_once('/') {
            Some((kind, name)) => (Some(kind), name),
            None => (None, name),
        };
        let matches: Vec<(&str, &SharedAppConfig)> = self
            .shared_apps()
            .filter(|(kind, app)| kind_filter.is_none_or(|k| k == *kind) && app.name == name)
            .collect();
        match matches.as_slice() {
            [found] => Ok(*found),
            [] => {
                let known: Vec<&str> = self
                    .shared_apps()
                    .map(|(_, app)| app.name.as_str())
                    .collect();
                Err(config_err(format!(
                    "no shared app `{name}` on this server (shared apps: {})",
                    if known.is_empty() {
                        "none".to_string()
                    } else {
                        known.join(", ")
                    }
                )))
            }
            _ => {
                let names: Vec<String> = matches
                    .iter()
                    .map(|(kind, app)| format!("{kind}/{}", app.name))
                    .collect();
                Err(config_err(format!(
                    "`{name}` is ambiguous; use {}",
                    names.join(" or ")
                )))
            }
        }
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
    fn notifications() {
        let chat = "shared: {communication: [{name: chat, type: slack}]}\n";
        let config = parse(&format!(
            "{chat}notifications: {{send_to: [chat, {{to: chat, events: [disk]}}], disk_free_below: 15}}"
        ))
        .unwrap();
        let notifications = config.notifications.unwrap();
        assert_eq!(notifications.disk_free_below, 15);
        assert!(notifications.send_to[0].wants(Event::Backup));
        assert!(!notifications.send_to[1].wants(Event::Backup));
        assert!(notifications.send_to[1].wants(Event::Disk));
        // `status` only goes to who lists it, and needs no ingress.
        let config = parse(&format!(
            "{chat}notifications: {{send_to: [chat, {{to: chat, events: [status]}}]}}"
        ))
        .unwrap();
        let notifications = config.notifications.unwrap();
        assert!(!notifications.send_to[0].wants(Event::Status));
        assert!(notifications.send_to[1].wants(Event::Status));
        assert!(
            error("notifications: {send_to: [chat]}")
                .contains("not a communication app (known: none")
        );
        assert!(
            error(&format!(
                "{chat}notifications: {{send_to: [{{to: chat, events: [mail]}}]}}"
            ))
            .contains("unknown variant `mail`")
        );
        assert!(
            error(&format!(
                "{chat}notifications: {{send_to: [{{to: chat, event: [disk]}}]}}"
            ))
            .contains("unknown field `event`")
        );
        assert!(
            error(&format!("{chat}notifications: {{disk_free_below: 0}}")).contains("from 1 to 99")
        );
    }

    #[test]
    fn ban_notifications_are_opt_in() {
        let shared = "shared: {communication: [{name: chat, type: slack}], ingress: [{name: caddy-main, type: caddysecurebuild}]}\n";
        let config = parse(&format!(
            "{shared}notifications: {{send_to: [chat, {{to: chat, events: [bans, bans_daily]}}], daily_at: '07:30'}}"
        ))
        .unwrap();
        let notifications = config.notifications.unwrap();
        assert!(!notifications.send_to[0].wants(Event::Bans));
        assert!(!notifications.send_to[0].wants(Event::BansDaily));
        assert!(notifications.send_to[1].wants(Event::Bans));
        assert!(notifications.send_to[1].wants(Event::BansDaily));
        assert!(!notifications.send_to[0].wants(Event::Status));
        assert_eq!(
            notifications.daily_time(),
            chrono::NaiveTime::from_hms_opt(7, 30, 0)
        );
        let default = parse(&format!("{shared}notifications: {{send_to: [chat]}}")).unwrap();
        assert_eq!(default.notifications.unwrap().daily_at, "08:00");

        assert!(
            error(&format!("{shared}notifications: {{daily_at: '25:00'}}"))
                .contains("`25:00` is not a time like 08:00")
        );
        let plain = "shared: {communication: [{name: chat, type: slack}], ingress: [{name: caddy-main, type: caddy}]}\n";
        assert!(
            error(&format!(
                "{plain}notifications: {{send_to: [{{to: chat, events: [bans_daily]}}]}}"
            ))
            .contains("`bans_daily` needs a caddysecurebuild ingress")
        );
    }

    #[test]
    fn backup_settings() {
        let config = parse("backup: {target: server, server_path: /srv/backups}").unwrap();
        let backup = config.backup.unwrap();
        assert_eq!(backup.target.as_deref(), Some("server"));
        assert!(error("backup: {target: ftp}").contains("unknown backup target `ftp`"));
        assert!(error("backup: {server_path: ''}").contains("must not be empty"));

        let config = parse(
            "backup: {target: nas, targets: {nas: {type: server, path: /mnt/nas}}, \
             profiles: {daily: {include: [links], keep: 7, target: nas}}}",
        )
        .unwrap();
        let backup = config.backup.unwrap();
        assert_eq!(
            backup.targets["nas"],
            TargetConfig::Server {
                path: "/mnt/nas".into()
            }
        );
        assert_eq!(backup.profiles["daily"].keep, Some(7));
        assert!(error("backup: {targets: {server: {type: server, path: x}}}").contains("built-in"));
        assert!(
            error("backup: {targets: {x: {type: server, path: x, port: 1}}}")
                .contains("unknown field `port`")
        );
        assert!(
            error("backup: {profiles: {daily: {target: nas}}}").contains("unknown backup target")
        );
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
        assert!(link.entry.get("environment").is_none());

        let pg = config.consumer("database", &config.shared["database"][0]);
        let pg = pg.unwrap().unwrap();
        assert!(pg.links[0].entry.get("metrics").is_none());
        // Keycloak links to a database that has links itself.
        let kc = config.consumer("oidc", &config.shared["oidc"][0]).unwrap();
        assert_eq!(kc.unwrap().links.len(), 2);
    }

    #[test]
    fn traced_ingress_gets_the_collector_address() {
        let config = parse(&MONITORED.replace(
            "monitoring: {}}",
            "monitoring: {logs: false, traces: true}}",
        ))
        .unwrap();
        let caddy = config.consumer("ingress", &config.shared["ingress"][0]);
        let link = &caddy.unwrap().unwrap().links[0];
        assert_eq!(link.entry["logs"], false);
        assert_eq!(link.entry["targets"][0], "caddy");
        let environment = &link.entry["environment"];
        assert_eq!(environment["OTEL_EXPORTER_OTLP_ENDPOINT"], "otlp_endpoint");
        assert_eq!(environment["OTEL_EXPORTER_OTLP_PROTOCOL"], "otlp_protocol");

        let err =
            error(&MONITORED.replace("monitoring: {metrics: false}", "monitoring: {traces: true}"));
        assert!(
            err.contains("shared.database[pg18]")
                && err.contains("monitoring.traces: this shared app sends no traces"),
            "{err}"
        );
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
    fn commands_run_in_a_shared_app() {
        const SHARED: &str = "shared: {database: [{name: pg, type: postgres, version: 18}, {name: ext, type: postgres, version: 18, external: {host: db}, configuration: {root_user: a, root_password: x}}], communication: [{name: chat, type: matrix, configuration: {homeserver: 'https://m.example.org', room: '!a:example.org'}}]}";
        let config = parse(&format!(
            "{SHARED}\ncommands: {{psql: {{shared_app: pg, env: {{PGPASSWORD: '{{secret:root_password}}'}}, command: [psql, -U, postgres]}}}}"
        ))
        .unwrap();
        // Stored on the server with the config, so `server cmd` works from anywhere.
        let stored = serde_yaml_ng::to_string(&config.without_secrets().unwrap()).unwrap();
        assert!(stored.contains("{secret:root_password}"), "{stored}");
        for (commands, message) in [
            ("{x: {command: [sh]}}", "commands.x: `shared_app`"),
            (
                "{x: {shared_app: nope, command: [sh]}}",
                "no shared app `nope`",
            ),
            ("{x: {shared_app: ext, command: [sh]}}", "is external"),
            (
                "{x: {shared_app: chat, command: [sh]}}",
                "runs only when it sends",
            ),
            (
                "{x: {shared_app: pg, exec: web, command: [sh]}}",
                "use `shared_app`",
            ),
            (
                "{x: {shared_app: pg, service: Bad, command: [sh]}}",
                "invalid service `Bad`",
            ),
        ] {
            let err = error(&format!("{SHARED}\ncommands: {commands}"));
            assert!(err.contains(message), "{commands}: {err}");
        }
    }

    #[test]
    fn error_contains_config_path() {
        let err = error("shared: {database: [{name: pg, type: postgres}]}");
        assert!(err.starts_with("shared.database[pg]: "), "{err}");
    }
}
