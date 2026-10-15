//! Application configuration: `depap.yaml` + deployment overlays (spec §6).

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_yaml_ng::Value;

use super::{check_name, merge};
use crate::error::config_err;
use crate::shared;

const DEFAULT_PREFIX: &str = "main";
const DEFAULT_COMPOSE_FILE: &str = "docker-compose.yaml";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    pub name: String,
    #[serde(default = "default_prefix")]
    pub prefix: String,
    #[serde(default)]
    pub deployment: Deployment,
    /// Links grouped by kind. Each kind parses and validates its own entries.
    #[serde(default)]
    pub services: BTreeMap<String, Vec<Value>>,
    #[serde(default)]
    pub app_services: Vec<AppService>,
    #[serde(default, skip_serializing_if = "BackupConfig::is_empty")]
    pub backup: BackupConfig,
}

/// What `app backup` saves besides link data and sources.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BackupConfig {
    /// Named volumes (keys of the compose file's top-level `volumes`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<String>,
}

impl BackupConfig {
    fn is_empty(&self) -> bool {
        self.volumes.is_empty()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Deployment {
    #[serde(rename = "type")]
    pub kind: DeploymentType,
    pub compose_file: String,
    pub build: bool,
    pub attach_sources: bool,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize)]
pub enum DeploymentType {
    #[default]
    #[serde(rename = "docker-compose")]
    DockerCompose,
}

impl Default for Deployment {
    fn default() -> Self {
        Self {
            kind: DeploymentType::DockerCompose,
            compose_file: DEFAULT_COMPOSE_FILE.to_string(),
            build: false,
            attach_sources: false,
        }
    }
}

impl Deployment {
    /// Building needs the build contexts on the server.
    pub fn uploads_sources(&self) -> bool {
        self.build || self.attach_sources
    }
}

/// A compose service that links can refer to by `name`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AppService {
    pub name: String,
    /// Service name in the compose file.
    pub service: String,
    /// Static variables added to the container.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub environment: BTreeMap<String, Value>,
}

/// What depap needs to know about the user's compose file.
#[derive(Debug, Default)]
pub struct ComposeFile {
    pub services: BTreeMap<String, ComposeService>,
    /// Top-level named volumes → their `name:` when the file sets one.
    pub volumes: BTreeMap<String, Option<String>>,
}

#[derive(Debug, Default)]
pub struct ComposeService {
    /// The service lists its own `networks`.
    pub has_networks: bool,
    pub network_mode: Option<String>,
    pub restart: Option<String>,
}

impl ComposeService {
    /// Docker restarts it after it exits, so an exited container was stopped
    /// on purpose; one-shot jobs (like migrations) do not use these policies.
    pub fn keeps_running(&self) -> bool {
        matches!(self.restart.as_deref(), Some("always" | "unless-stopped"))
    }
}

/// A validated application with everything loaded from disk.
pub struct LoadedApp {
    pub config: AppConfig,
    pub compose: ComposeFile,
    /// Directory containing the first config file (normally depap.yaml).
    pub base_dir: PathBuf,
}

impl AppConfig {
    /// Merge and validate the given files, then read and check the compose file.
    /// `prefix` replaces the one from the files.
    pub fn load(files: &[PathBuf], prefix: Option<&str>) -> Result<LoadedApp> {
        let merged = merge::load_merged(files)?;
        let mut config: AppConfig = serde_yaml_ng::from_value(merged)
            .map_err(|e| config_err(format!("app configuration: {e}")))?;
        if let Some(prefix) = prefix {
            config.prefix = prefix.to_string();
        }

        // `depap.yaml` has the parent "", which means the current directory.
        let base_dir = files
            .first()
            .and_then(|f| f.parent())
            .filter(|dir| !dir.as_os_str().is_empty())
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        let compose_path = base_dir.join(&config.deployment.compose_file);
        let compose = ComposeFile::read(&compose_path)?;
        config.validate(&compose)?;
        Ok(LoadedApp {
            config,
            compose,
            base_dir,
        })
    }

    /// `<app>-<prefix>`: compose project name and instance ID.
    pub fn instance(&self) -> String {
        format!("{}-{}", self.name, self.prefix)
    }

    pub fn app_service(&self, name: &str) -> Option<&AppService> {
        self.app_services.iter().find(|s| s.name == name)
    }

    /// Every link entry with the name of its kind.
    pub fn links(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.services
            .iter()
            .flat_map(|(kind, links)| links.iter().map(move |link| (kind.as_str(), link)))
    }

    /// A copy without secret values, safe to store on the server.
    pub fn without_secrets(&self) -> Result<AppConfig> {
        let mut copy = self.clone();
        for (kind_name, links) in &mut copy.services {
            let kind = shared::find_kind(kind_name)
                .with_context(|| format!("unknown kind `{kind_name}`"))?;
            for link in links {
                if let Value::Mapping(map) = link {
                    for key in kind.link_secret_keys() {
                        map.remove(*key);
                    }
                }
            }
        }
        Ok(copy)
    }

    /// Docker volume behind a named volume of the compose file, e.g.
    /// `myapp-staging_uploads`. Volumes in `backup.volumes` are checked on load.
    pub fn docker_volume(&self, compose: &ComposeFile, volume: &str) -> String {
        match compose.volumes.get(volume) {
            Some(Some(name)) => name.clone(),
            _ => format!("{}_{volume}", self.instance()),
        }
    }

    /// Core checks, then delegation to each kind (spec §6.3).
    fn validate(&self, compose: &ComposeFile) -> Result<()> {
        check_name("app name", &self.name)?;
        check_name("prefix", &self.prefix)?;

        let mut names = HashSet::new();
        for service in &self.app_services {
            let path = format!("app_services[{}]", service.name);
            check_name("app service name", &service.name)?;
            if !names.insert(&service.name) {
                return Err(config_err(format!("{path}: name is used more than once")));
            }
            if !compose.services.contains_key(&service.service) {
                return Err(config_err(format!(
                    "{path}: service `{}` is not in {}",
                    service.service, self.deployment.compose_file
                )));
            }
            for (key, value) in &service.environment {
                if env_value(value).is_none() {
                    return Err(config_err(format!(
                        "{path}.environment.{key}: must be a string, number or boolean"
                    )));
                }
            }
        }

        let mut volumes = HashSet::new();
        for volume in &self.backup.volumes {
            if !volumes.insert(volume) {
                return Err(config_err(format!(
                    "backup.volumes: `{volume}` is listed more than once"
                )));
            }
            if !compose.volumes.contains_key(volume) {
                return Err(config_err(format!(
                    "backup.volumes: `{volume}` is not a named volume in {}",
                    self.deployment.compose_file
                )));
            }
        }

        for (kind_name, links) in &self.services {
            let kind = shared::find_kind(kind_name).ok_or_else(|| {
                config_err(format!(
                    "services.{kind_name}: unknown kind (known: {})",
                    shared::kind_names().join(", ")
                ))
            })?;
            let mut names = HashSet::new();
            for link in links {
                let name = link_name(link).ok_or_else(|| {
                    config_err(format!("services.{kind_name}: every entry needs a `name`"))
                })?;
                let path = format!("services.{kind_name}[{name}]");
                check_name("link name", name)?;
                if !names.insert(name) {
                    return Err(config_err(format!("{path}: name is used more than once")));
                }
                kind.validate_link(link, self)
                    .map_err(|e| config_err(format!("{path}: {e:#}")))?;
            }
        }
        Ok(())
    }
}

/// The `name` of a link entry.
pub fn link_name(link: &Value) -> Option<&str> {
    link.get("name").and_then(Value::as_str)
}

/// A scalar YAML value as an environment variable value.
pub fn env_value(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

pub fn default_prefix() -> String {
    DEFAULT_PREFIX.to_string()
}

impl ComposeFile {
    pub fn read(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)
            .map_err(|e| config_err(format!("cannot read compose file {}: {e}", path.display())))?;
        Self::parse(&text).map_err(|e| config_err(format!("{}: {e:#}", path.display())))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let value: Value = serde_yaml_ng::from_str(text)?;
        let Some(Value::Mapping(services)) = value.get("services") else {
            anyhow::bail!("no `services` section");
        };
        let mut compose = ComposeFile::default();
        for (name, service) in services {
            let name = name.as_str().context("service names must be strings")?;
            compose.services.insert(
                name.to_string(),
                ComposeService {
                    has_networks: service.get("networks").is_some(),
                    network_mode: service
                        .get("network_mode")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    restart: service
                        .get("restart")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                },
            );
        }
        if let Some(Value::Mapping(volumes)) = value.get("volumes") {
            for (name, volume) in volumes {
                let name = name.as_str().context("volume names must be strings")?;
                let docker_name = volume.get("name").and_then(Value::as_str);
                compose
                    .volumes
                    .insert(name.to_string(), docker_name.map(str::to_string));
            }
        }
        Ok(compose)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMPOSE: &str = "
services:
  front-webapp: {build: .}
  backend-webapp: {image: x, networks: [internal]}
  backend-migration: {image: x}
";

    fn validate(text: &str) -> Result<AppConfig> {
        let config: AppConfig = serde_yaml_ng::from_str(text)?;
        config.validate(&ComposeFile::parse(COMPOSE).unwrap())?;
        Ok(config)
    }

    fn error(text: &str) -> String {
        format!("{:#}", validate(text).unwrap_err())
    }

    #[test]
    fn samples_are_valid() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("samples/sampleapp");
        for (overlay, prefix) in [
            (".depap-staging.example.yaml", "staging"),
            (".depap-local.example.yaml", "local"),
        ] {
            let files = [dir.join("depap.yaml"), dir.join(overlay)];
            let loaded =
                AppConfig::load(&files, None).unwrap_or_else(|e| panic!("{overlay}: {e:#}"));
            assert_eq!(loaded.config.instance(), format!("sampleapp-{prefix}"));
            assert_eq!(loaded.compose.services.len(), 3);
        }
    }

    #[test]
    fn prefix_override() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("samples/sampleapp");
        let files = [
            dir.join("depap.yaml"),
            dir.join(".depap-staging.example.yaml"),
        ];
        let loaded = AppConfig::load(&files, Some("review")).unwrap();
        assert_eq!(loaded.config.instance(), "sampleapp-review");
        let error = AppConfig::load(&files, Some("Bad")).err().unwrap();
        assert!(format!("{error:#}").contains("prefix"), "{error:#}");
    }

    #[test]
    fn defaults() {
        let config = validate("name: myapp").unwrap();
        assert_eq!(config.instance(), "myapp-main");
        assert_eq!(config.deployment.compose_file, "docker-compose.yaml");
        assert!(!config.deployment.uploads_sources());
    }

    #[test]
    fn reads_compose_services() {
        let compose = ComposeFile::parse(COMPOSE).unwrap();
        assert!(!compose.services["front-webapp"].has_networks);
        assert!(compose.services["backend-webapp"].has_networks);
    }

    #[test]
    fn app_service_must_exist_in_compose() {
        let err = error("{name: a, app_services: [{name: web, service: nope}]}");
        assert!(
            err.contains("app_services[web]: service `nope` is not in docker-compose.yaml"),
            "{err}"
        );
    }

    #[test]
    fn link_errors_carry_their_path() {
        let err = error(
            "{name: a, app_services: [{name: web, service: front-webapp}],
              services: {ingress: [{name: ui, domains: [a.com], routes: [{name: r, target: backend, port: 80}]}]}}",
        );
        assert!(err.starts_with("services.ingress[ui]: "), "{err}");
        assert!(err.contains("`backend`"), "{err}");
    }

    #[test]
    fn rejects_unknown_kind_and_unnamed_entries() {
        assert!(error("{name: a, services: {queue: []}}").contains("services.queue: unknown kind"));
        assert!(
            error("{name: a, services: {database: [{type: postgres}]}}").contains("needs a `name`")
        );
    }

    #[test]
    fn backup_volumes_must_be_named_volumes() {
        let compose = ComposeFile::parse(
            "{services: {web: {image: x}}, volumes: {uploads: {}, media: {name: shared-media}}}",
        )
        .unwrap();
        let config: AppConfig =
            serde_yaml_ng::from_str("{name: a, prefix: p, backup: {volumes: [uploads, media]}}")
                .unwrap();
        config.validate(&compose).unwrap();
        assert_eq!(config.docker_volume(&compose, "uploads"), "a-p_uploads");
        assert_eq!(config.docker_volume(&compose, "media"), "shared-media");

        let err = error("{name: a, backup: {volumes: [data]}}");
        assert!(err.contains("`data` is not a named volume"), "{err}");
    }

    #[test]
    fn rejects_non_scalar_environment() {
        let err = error(
            "{name: a, app_services: [{name: web, service: front-webapp, environment: {X: [1]}}]}",
        );
        assert!(err.contains("environment.X"), "{err}");
    }
}
