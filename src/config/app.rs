//! Application configuration: `compost.yaml` + deployment overlays (spec §6).

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
const DEFAULT_SOURCE_DIR: &str = ".";
/// Default of `deployment.releases`.
const DEFAULT_RELEASES: u32 = 5;
/// Folder in the uploaded sources that holds copies of `compose_overrides`.
pub const OVERRIDES_DIR: &str = ".compost";

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

/// What `app backup` saves besides link data and sources, and the steps
/// that run around a backup or restore.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BackupConfig {
    /// Named volumes (keys of the compose file's top-level `volumes`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<String>,
    /// Services stopped while volumes are copied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<StopServices>,
    /// Before anything is saved, while the services run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub before_backup: Vec<Step>,
    /// After the data is saved and the stopped services started again.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after_backup: Vec<Step>,
    /// Before data is loaded into a deployed instance, while its services run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub before_restore: Vec<Step>,
    /// After the data is loaded and the services started.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after_restore: Vec<Step>,
}

impl BackupConfig {
    fn is_empty(&self) -> bool {
        self.volumes.is_empty()
            && self.stop.is_none()
            && self.before_backup.is_empty()
            && self.after_backup.is_empty()
            && self.before_restore.is_empty()
            && self.after_restore.is_empty()
    }

    /// The step lists with their keys, for messages.
    fn steps(&self) -> [(&'static str, &[Step]); 4] {
        [
            ("before_backup", &self.before_backup),
            ("after_backup", &self.after_backup),
            ("before_restore", &self.before_restore),
            ("after_restore", &self.after_restore),
        ]
    }
}

/// `backup.stop`: `true` for every service, or a list of compose services.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum StopServices {
    All(bool),
    Some(Vec<String>),
}

/// A command run in a service of the instance: `exec` in its running
/// container, or `run` as a one-off container that must exit with 0.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    /// A string runs with `sh -c`; a list is the command and its arguments.
    /// `run` without it uses the service's own command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<StepCommand>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum StepCommand {
    Shell(String),
    Args(Vec<String>),
}

impl StepCommand {
    /// The command's words.
    pub fn args(&self) -> Vec<String> {
        match self {
            Self::Shell(script) => vec!["sh".into(), "-c".into(), script.clone()],
            Self::Args(args) => args.clone(),
        }
    }
}

impl Step {
    /// The compose service, and whether the step `exec`s into it.
    pub fn target(&self) -> (&str, bool) {
        match (&self.exec, &self.run) {
            (Some(service), _) => (service, true),
            (None, Some(service)) => (service, false),
            (None, None) => ("", false),
        }
    }

    fn validate(&self, compose: &ComposeFile) -> Result<()> {
        let service = match (&self.exec, &self.run) {
            (Some(_), Some(_)) | (None, None) => {
                return Err(config_err("set either `exec` or `run`"));
            }
            (Some(service), None) => {
                if self.command.as_ref().is_none_or(|c| c.args().is_empty()) {
                    return Err(config_err("`exec` needs a `command`"));
                }
                service
            }
            (None, Some(service)) => service,
        };
        if matches!(&self.command, Some(StepCommand::Args(args)) if args.is_empty()) {
            return Err(config_err("`command` must not be empty"));
        }
        if !compose.services.contains_key(service) {
            return Err(config_err(format!(
                "service `{service}` is not in the compose file"
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Deployment {
    #[serde(rename = "type")]
    pub kind: DeploymentType,
    /// Folder with the compose file and build contexts, relative to the first
    /// config file. Other than `.` when compost.yaml lives elsewhere, e.g. in a
    /// deployment repository.
    #[serde(skip_serializing_if = "is_default_source_dir")]
    pub source_dir: String,
    /// Relative to `source_dir`.
    pub compose_file: String,
    /// More compose files, applied after `compose_file`; relative to the first
    /// config file. They are uploaded into the sources as
    /// `.compost/compose-override-<n>.yaml`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub compose_overrides: Vec<String>,
    pub build: bool,
    pub attach_sources: bool,
    /// Releases kept on the server for `app rollback`; 0 keeps none.
    #[serde(skip_serializing_if = "is_default_releases")]
    pub releases: u32,
}

fn is_default_source_dir(dir: &str) -> bool {
    dir == DEFAULT_SOURCE_DIR
}

fn is_default_releases(releases: &u32) -> bool {
    *releases == DEFAULT_RELEASES
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
            source_dir: DEFAULT_SOURCE_DIR.to_string(),
            compose_file: DEFAULT_COMPOSE_FILE.to_string(),
            compose_overrides: Vec::new(),
            build: false,
            attach_sources: false,
            releases: DEFAULT_RELEASES,
        }
    }
}

impl Deployment {
    /// Building needs the build contexts on the server.
    pub fn uploads_sources(&self) -> bool {
        self.build || self.attach_sources
    }

    /// The compose files in the uploaded sources, in the order they apply.
    pub fn compose_files(&self) -> Vec<String> {
        let mut files = vec![self.compose_file.clone()];
        files.extend(self.compose_overrides.iter().cloned());
        files
    }
}

/// Name of the `index`th of `compose_overrides` in the uploaded sources.
fn uploaded_override(index: usize) -> String {
    format!("{OVERRIDES_DIR}/compose-override-{}.yaml", index + 1)
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

/// What compost needs to know about the user's compose file.
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
    /// The service has a `build` section.
    pub builds: bool,
    /// The service sets its own `logging`.
    pub has_logging: bool,
}

impl ComposeService {
    /// Docker restarts it after it exits, so an exited container was stopped
    /// on purpose; one-shot jobs (like migrations) do not use these policies.
    pub fn keeps_running(&self) -> bool {
        matches!(self.restart.as_deref(), Some("always" | "unless-stopped"))
    }
}

/// A validated application with everything loaded from disk. Its config
/// describes the uploaded sources: `source_dir` is `.` and
/// `compose_overrides` are their names in the sources.
pub struct LoadedApp {
    pub config: AppConfig,
    /// All compose files merged.
    pub compose: ComposeFile,
    /// The folder uploaded as the sources (`deployment.source_dir`).
    pub source_dir: PathBuf,
    /// Local `compose_overrides`, each with its name in the sources.
    pub overrides: Vec<(PathBuf, String)>,
}

impl AppConfig {
    /// Merge and validate the given files, then read and check the compose file.
    /// `prefix` replaces the one from the files.
    pub fn load(files: &[PathBuf], prefix: Option<&str>) -> Result<LoadedApp> {
        Self::load_with(files, None, prefix)
    }

    /// Like [`AppConfig::load`], with `stored` (secrets kept on the server)
    /// merged after the first file, so later files still override it.
    pub fn load_with(
        files: &[PathBuf],
        stored: Option<Value>,
        prefix: Option<&str>,
    ) -> Result<LoadedApp> {
        let split = files.len().min(1);
        let mut merged = merge::load_merged(&files[..split])?;
        if let Some(stored) = stored {
            merge::merge(&mut merged, stored);
        }
        merge::merge_files(&mut merged, &files[split..])?;
        let mut config: AppConfig = serde_yaml_ng::from_value(merged)
            .map_err(|e| config_err(format!("app configuration: {e}")))?;
        if let Some(prefix) = prefix {
            config.prefix = prefix.to_string();
        }

        // `compost.yaml` has the parent "", which means the current directory.
        let base_dir = files
            .first()
            .and_then(|f| f.parent())
            .filter(|dir| !dir.as_os_str().is_empty())
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        let deployment = &mut config.deployment;
        let source_dir = base_dir.join(&deployment.source_dir);
        if !source_dir.is_dir() {
            return Err(config_err(format!(
                "deployment.source_dir: {} is not a folder",
                source_dir.display()
            )));
        }
        let mut paths = vec![source_dir.join(&deployment.compose_file)];
        let mut overrides = Vec::new();
        for (index, file) in deployment.compose_overrides.iter_mut().enumerate() {
            let path = base_dir.join(&*file);
            paths.push(path.clone());
            *file = uploaded_override(index);
            overrides.push((path, file.clone()));
        }
        deployment.source_dir = DEFAULT_SOURCE_DIR.to_string();
        let compose = ComposeFile::read_all(&paths)?;
        config.validate(&compose)?;
        Ok(LoadedApp {
            config,
            compose,
            source_dir,
            overrides,
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
        if self.name.starts_with(shared::CONSUMER_PREFIX) {
            return Err(config_err(format!(
                "invalid app name `{}`: names starting with `{}` are reserved",
                self.name,
                shared::CONSUMER_PREFIX
            )));
        }
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
        if let Some(StopServices::Some(services)) = &self.backup.stop {
            for service in services {
                if !compose.services.contains_key(service) {
                    return Err(config_err(format!(
                        "backup.stop: service `{service}` is not in {}",
                        self.deployment.compose_file
                    )));
                }
            }
        }
        for (key, steps) in self.backup.steps() {
            for (index, step) in steps.iter().enumerate() {
                step.validate(compose)
                    .map_err(|e| config_err(format!("backup.{key}[{index}]: {e:#}")))?;
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
    /// Read compose files and merge them as `docker compose -f … -f …` does,
    /// as far as compost needs.
    pub fn read_all(paths: &[PathBuf]) -> Result<Self> {
        let texts = paths
            .iter()
            .map(|path| {
                fs::read_to_string(path).map_err(|e| {
                    config_err(format!("cannot read compose file {}: {e}", path.display()))
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let texts: Vec<&str> = texts.iter().map(String::as_str).collect();
        Self::parse_all(&texts).map_err(|e| {
            let names: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
            config_err(format!("{}: {e:#}", names.join(", ")))
        })
    }

    pub fn read(path: &Path) -> Result<Self> {
        Self::read_all(&[path.to_path_buf()])
    }

    pub fn parse(text: &str) -> Result<Self> {
        Self::parse_all(&[text])
    }

    /// The first text is the main compose file; the others override it.
    pub fn parse_all(texts: &[&str]) -> Result<Self> {
        let mut value = Value::Null;
        for text in texts {
            let overlay: Value = serde_yaml_ng::from_str(text)?;
            super::merge::merge(&mut value, overlay);
        }
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
                    builds: service.get("build").is_some(),
                    has_logging: service.get("logging").is_some(),
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
    fn compost_prefix_is_reserved() {
        assert!(error("{name: compost-oidc}").contains("are reserved"));
    }

    #[test]
    fn samples_are_valid() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("samples/sampleapp");
        for (overlay, prefix) in [
            (".compost-staging.example.yaml", "staging"),
            (".compost-local.example.yaml", "local"),
        ] {
            let files = [dir.join("compost.yaml"), dir.join(overlay)];
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
            dir.join("compost.yaml"),
            dir.join(".compost-staging.example.yaml"),
        ];
        let loaded = AppConfig::load(&files, Some("review")).unwrap();
        assert_eq!(loaded.config.instance(), "sampleapp-review");
        let error = AppConfig::load(&files, Some("Bad")).err().unwrap();
        assert!(format!("{error:#}").contains("prefix"), "{error:#}");
    }

    #[test]
    fn stored_secrets_merge_between_files() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("samples/sampleapp");
        let stored: Value = serde_yaml_ng::from_str(
            "{name: sampleapp, prefix: staging, services: {ingress: [{name: site, domains: [stored.example.com]}]},
              app_services: [{name: backend, environment: {APP_MODE: stored}}]}",
        )
        .unwrap();
        let domains = |loaded: &LoadedApp| {
            let (_, site) = loaded
                .config
                .links()
                .find(|(k, _)| *k == "ingress")
                .unwrap();
            site["domains"][0].as_str().unwrap().to_string()
        };

        let loaded =
            AppConfig::load_with(&[dir.join("compost.yaml")], Some(stored.clone()), None).unwrap();
        assert_eq!(loaded.config.instance(), "sampleapp-staging");
        assert_eq!(domains(&loaded), "stored.example.com");

        // A local overlay wins over the stored values.
        let files = [
            dir.join("compost.yaml"),
            dir.join(".compost-local.example.yaml"),
        ];
        let loaded = AppConfig::load_with(&files, Some(stored), None).unwrap();
        assert_eq!(loaded.config.instance(), "sampleapp-local");
        assert_eq!(domains(&loaded), "sampleapp.localhost");
        let backend = loaded.config.app_service("backend").unwrap();
        assert_eq!(backend.environment["APP_MODE"], Value::from("local"));
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
    fn backup_steps_and_stop() {
        let config = validate(
            "{name: a, backup: {stop: [backend-webapp], \
              before_backup: [{exec: backend-webapp, command: 'dump > /x'}], \
              after_backup: [{run: backend-migration}], \
              after_restore: [{run: backend-migration, command: [load, --all]}]}}",
        )
        .unwrap();
        let backup = &config.backup;
        assert_eq!(
            backup.stop,
            Some(StopServices::Some(vec!["backend-webapp".into()]))
        );
        assert_eq!(backup.before_backup[0].target(), ("backend-webapp", true));
        assert_eq!(
            backup.before_backup[0].command.as_ref().unwrap().args(),
            ["sh", "-c", "dump > /x"]
        );
        assert_eq!(
            backup.after_backup[0].target(),
            ("backend-migration", false)
        );
        assert!(backup.after_backup[0].command.is_none());
        assert_eq!(
            validate("{name: a, backup: {stop: true}}")
                .unwrap()
                .backup
                .stop,
            Some(StopServices::All(true))
        );
        // Stored configs keep the steps.
        let stored = serde_yaml_ng::to_string(&config).unwrap();
        let again: AppConfig = serde_yaml_ng::from_str(&stored).unwrap();
        assert_eq!(again.backup.after_restore.len(), 1);

        for (text, message) in [
            ("{stop: [nope]}", "backup.stop: service `nope` is not in"),
            (
                "{before_backup: [{exec: front-webapp}]}",
                "backup.before_backup[0]: `exec` needs a `command`",
            ),
            (
                "{after_backup: [{run: x, exec: x}]}",
                "backup.after_backup[0]: set either `exec` or `run`",
            ),
            (
                "{before_restore: [{command: x}]}",
                "backup.before_restore[0]: set either",
            ),
            (
                "{after_restore: [{run: front-webapp, command: []}]}",
                "must not be empty",
            ),
            (
                "{after_restore: [{run: nope}]}",
                "service `nope` is not in the compose file",
            ),
            ("{after_restore: [{run: x, cmd: y}]}", "unknown field `cmd`"),
        ] {
            let err = error(&format!("{{name: a, backup: {text}}}"));
            assert!(err.contains(message), "{text}: {err}");
        }
    }

    #[test]
    fn compose_overrides_merge_into_the_compose_model() {
        let compose = ComposeFile::parse_all(&[
            "{services: {web: {image: x}, db: {image: y}}, volumes: {data: {}}}",
            "{services: {web: {networks: [old_default]}, extra: {image: z}}, volumes: {data: {external: true, name: shop_data}}}",
        ])
        .unwrap();
        assert!(compose.services["web"].has_networks);
        assert!(!compose.services["db"].has_networks);
        assert!(compose.services.contains_key("extra"));
        let config: AppConfig = serde_yaml_ng::from_str("{name: a, prefix: p}").unwrap();
        assert_eq!(config.docker_volume(&compose, "data"), "shop_data");
    }

    #[test]
    fn sources_in_another_folder() {
        let dir = tempfile::tempdir().unwrap();
        let write = |path: &str, text: &str| {
            let path = dir.path().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, text).unwrap();
            path
        };
        write("upstream/compose.yaml", "services: {web: {image: nginx}}");
        write(
            "deploy/compose.migrate.yaml",
            "{services: {web: {volumes: [data:/data]}}, volumes: {data: {external: true, name: old_data}}}",
        );
        let compost = write(
            "deploy/compost.yaml",
            "{name: shop, deployment: {source_dir: ../upstream, compose_file: compose.yaml, compose_overrides: [compose.migrate.yaml]}, app_services: [{name: web, service: web}], backup: {volumes: [data]}}",
        );
        let loaded = AppConfig::load(&[compost], None).unwrap();
        assert_eq!(loaded.source_dir, dir.path().join("deploy/../upstream"));
        assert_eq!(
            loaded.overrides,
            [(
                dir.path().join("deploy/compose.migrate.yaml"),
                ".compost/compose-override-1.yaml".to_string()
            )]
        );
        // The config describes the uploaded sources, so a restore can load it from there.
        let deployment = &loaded.config.deployment;
        assert_eq!(deployment.source_dir, ".");
        assert_eq!(
            deployment.compose_files(),
            ["compose.yaml", ".compost/compose-override-1.yaml"]
        );
        let stored = serde_yaml_ng::to_string(&loaded.config).unwrap();
        assert!(!stored.contains("source_dir"), "{stored}");
        assert_eq!(
            loaded.config.docker_volume(&loaded.compose, "data"),
            "old_data"
        );

        let missing = write(
            "deploy/missing.yaml",
            "{name: shop, deployment: {source_dir: ../nope}}",
        );
        let err = AppConfig::load(&[missing], None).err().unwrap();
        assert!(
            format!("{err:#}").contains("deployment.source_dir:"),
            "{err:#}"
        );
    }

    #[test]
    fn rejects_non_scalar_environment() {
        let err = error(
            "{name: a, app_services: [{name: web, service: front-webapp, environment: {X: [1]}}]}",
        );
        assert!(err.contains("environment.X"), "{err}");
    }
}
