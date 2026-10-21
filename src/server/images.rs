//! `compost server images`: what each shared app's containers run, and since when.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::Deserialize;

use super::SERVER_CONFIG_FILE;
use super::state_file::{ImageRecord, StateFile};
use crate::config::server::ServerConfig;
use crate::connector::{Connector, shell_quote};
use crate::shared::{self, ServerCtx};

/// Environment variables official images set to their exact version, read when
/// the image has no `org.opencontainers.image.version` label.
const VERSION_VARIABLES: [&str; 7] = [
    "CADDY_VERSION",
    "PG_VERSION",
    "MARIADB_VERSION",
    "MYSQL_VERSION",
    "VALKEY_VERSION",
    "REDIS_VERSION",
    "KEYCLOAK_VERSION",
];
const VERSION_LABEL: &str = "org.opencontainers.image.version";

/// One container of a shared app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Running {
    /// Compose service.
    pub service: String,
    /// The image as the compose file names it, e.g. `caddy:2-alpine`.
    pub reference: String,
    /// The ID of the image the container runs.
    pub image_id: String,
    pub running: bool,
    /// When the container started (RFC 3339).
    pub started: String,
}

/// An image on the server.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Image {
    pub id: String,
    /// Registry digests, `repository@sha256:…`.
    pub digests: Vec<String>,
    /// Its exact version, from its label or environment.
    pub version: Option<String>,
    /// When it was built (RFC 3339).
    pub created: String,
    /// When it was pulled or tagged on the server (RFC 3339), if Docker knows.
    pub pulled: Option<String>,
    /// The environment of its configuration.
    pub env: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ContainerJson {
    image: String,
    config: ContainerConfig,
    state: ContainerState,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ContainerConfig {
    image: String,
    #[serde(default)]
    labels: Option<BTreeMap<String, String>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ContainerState {
    running: bool,
    #[serde(default)]
    started_at: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ImageJson {
    id: String,
    #[serde(default)]
    repo_digests: Option<Vec<String>>,
    #[serde(default)]
    created: String,
    #[serde(default)]
    config: Option<ImageConfig>,
    #[serde(default)]
    metadata: Option<ImageMetadata>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ImageConfig {
    #[serde(default)]
    pub labels: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub env: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ImageMetadata {
    #[serde(default)]
    last_tag_time: Option<String>,
}

/// The exact version an image's configuration names, if any.
pub fn version_of(config: &ImageConfig) -> Option<String> {
    if let Some(version) = config.labels.as_ref().and_then(|l| l.get(VERSION_LABEL)) {
        return Some(version.clone());
    }
    let env = config.env.as_deref().unwrap_or_default();
    VERSION_VARIABLES.iter().find_map(|name| {
        env.iter()
            .find_map(|pair| pair.strip_prefix(&format!("{name}=")))
            .map(str::to_string)
    })
}

/// The containers of a compose project, by service name.
pub fn running(connector: &dyn Connector, project: &str) -> Result<Vec<Running>> {
    let label = shell_quote(&format!("label=com.docker.compose.project={project}"));
    let text = connector.exec(&format!(
        "ids=$(docker ps -aq --filter {label}); [ -z \"$ids\" ] || docker container inspect $ids"
    ))?;
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let containers: Vec<ContainerJson> =
        serde_json::from_str(&text).context("docker container inspect printed no JSON")?;
    let mut running: Vec<Running> = containers
        .into_iter()
        .map(|c| Running {
            service: c
                .config
                .labels
                .as_ref()
                .and_then(|l| l.get("com.docker.compose.service"))
                .cloned()
                .unwrap_or_default(),
            reference: c.config.image,
            image_id: c.image,
            running: c.state.running,
            started: c.state.started_at,
        })
        .collect();
    running.sort_by(|a, b| a.service.cmp(&b.service));
    Ok(running)
}

/// The images with these IDs or references that exist on the server, keyed by
/// what was asked for.
pub fn inspect(connector: &dyn Connector, refs: &[&str]) -> Result<BTreeMap<String, Image>> {
    let mut found = BTreeMap::new();
    if refs.is_empty() {
        return Ok(found);
    }
    // `<ref>\t<json>` per line; the JSON is empty for a missing image.
    let quoted: Vec<String> = refs.iter().map(|r| shell_quote(r)).collect();
    let text = connector.exec(&format!(
        "for ref in {}; do printf '%s\\t' \"$ref\"; docker image inspect --format '{{{{json .}}}}' \"$ref\" 2>/dev/null || echo; done",
        quoted.join(" ")
    ))?;
    for line in text.lines() {
        let Some((asked, json)) = line.split_once('\t') else {
            continue;
        };
        let Ok(json) = serde_json::from_str::<ImageJson>(json) else {
            continue;
        };
        let config = json.config.unwrap_or_default();
        found.insert(
            asked.to_string(),
            Image {
                version: version_of(&config),
                env: config.env.unwrap_or_default(),
                id: json.id,
                digests: json.repo_digests.unwrap_or_default(),
                created: json.created,
                pulled: json
                    .metadata
                    .and_then(|m| m.last_tag_time)
                    .filter(|t| !t.starts_with("0001-")),
            },
        );
    }
    Ok(found)
}

/// `2026-10-02T14:03:51.12Z` → `2026-10-02 14:03`.
pub fn short_time(time: &str) -> String {
    time.get(..16).unwrap_or(time).replacen('T', " ", 1)
}

/// `sha256:d8542f48d34a9cf4…` → `d8542f48d34a`.
pub fn short_id(id: &str) -> &str {
    let hex = id.rsplit(':').next().unwrap_or(id);
    hex.get(..12).unwrap_or(hex)
}

/// The image records of a shared app after a deploy: an image keeps the time
/// it was first seen running.
pub fn records(previous: &[ImageRecord], running: &[Running], now: &str) -> Vec<ImageRecord> {
    running
        .iter()
        .map(|r| {
            let since = previous
                .iter()
                .find(|p| p.service == r.service && p.image_id == r.image_id)
                .map_or_else(|| now.to_string(), |p| p.since.clone());
            ImageRecord {
                service: r.service.clone(),
                image_id: r.image_id.clone(),
                since,
            }
        })
        .collect()
}

/// A shared app compost runs, with its compose project.
pub struct Project {
    pub kind: String,
    pub name: String,
    pub project: String,
}

/// The shared apps of the deployed config that run containers.
pub fn projects(ctx: &ServerCtx) -> Result<Vec<Project>> {
    let text = ctx
        .read(SERVER_CONFIG_FILE)?
        .context("the server has no compost-server.yaml; run `compost server deploy` first")?;
    let config = ServerConfig::parse(&text)?;
    let mut projects = Vec::new();
    for (kind_name, app) in config.shared_apps() {
        let Some(kind) = shared::find_kind(kind_name) else {
            continue;
        };
        if app.is_external() || !kind.runs_service(app) {
            continue;
        }
        projects.push(Project {
            kind: kind_name.to_string(),
            name: app.name.clone(),
            project: shared::project_name(kind_name, &app.name),
        });
    }
    Ok(projects)
}

/// `compost server images`.
pub fn report(connector: &dyn Connector, server_home: &str, manager_key: &str) -> Result<()> {
    let ctx = crate::app::state::initialized_server(connector, server_home, manager_key)?;
    let state_file = StateFile::load(&ctx)?;
    out!(
        "Images of the shared apps on {} (times in UTC):",
        connector.target()
    );
    for project in projects(&ctx)? {
        out!();
        out!("{} {}", project.kind, project.name);
        let containers = running(connector, &project.project)?;
        if containers.is_empty() {
            out!("  no containers: deploy it with `compost server deploy --apply`");
            continue;
        }
        let mut refs: Vec<&str> = Vec::new();
        for c in &containers {
            refs.push(&c.image_id);
            refs.push(&c.reference);
        }
        let images = inspect(connector, &refs)?;
        let recorded = state_file
            .shared(&project.kind, &project.name)
            .map(|r| r.images.as_slice())
            .unwrap_or_default();
        for c in &containers {
            print_container(c, &images, recorded);
        }
    }
    Ok(())
}

fn print_container(c: &Running, images: &BTreeMap<String, Image>, recorded: &[ImageRecord]) {
    let image = images.get(&c.image_id);
    let version = match image.and_then(|i| i.version.as_deref()) {
        Some(version) => format!("version {version}"),
        None => "version unknown".to_string(),
    };
    out!("  {}: {}, {version}", c.service, c.reference);
    let mut about = format!("image {}", short_id(&c.image_id));
    if let Some(image) = image {
        if let Some(day) = image.created.get(..10) {
            about.push_str(&format!(", built {day}"));
        }
        if let Some(pulled) = &image.pulled {
            about.push_str(&format!(", pulled {}", short_time(pulled)));
        }
    }
    out!("    {about}");
    let mut life = if c.running {
        format!("running since {}", short_time(&c.started))
    } else {
        "not running".to_string()
    };
    if let Some(record) = recorded
        .iter()
        .find(|r| r.service == c.service && r.image_id == c.image_id)
    {
        life.push_str(&format!(
            ", deployed with this image {}",
            short_time(&record.since)
        ));
    }
    out!("    {life}");
    // The tag points to another image: pulled, but the container is older.
    if let Some(tagged) = images.get(&c.reference) {
        if tagged.id != c.image_id {
            out!(
                "    newer image {} pulled for {}, not running yet: `server deploy --apply` starts it",
                short_id(&tagged.id),
                c.reference
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_version_from_the_label_then_the_environment() {
        let config = |labels: &[(&str, &str)], env: &[&str]| ImageConfig {
            labels: Some(
                labels
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            ),
            env: Some(env.iter().map(|e| e.to_string()).collect()),
        };
        assert_eq!(
            version_of(&config(
                &[(VERSION_LABEL, "v2.11.7")],
                &["CADDY_VERSION=v2.11.6"]
            )),
            Some("v2.11.7".into())
        );
        assert_eq!(
            version_of(&config(&[], &["GOSU_VERSION=1.17", "PG_VERSION=18.1"])),
            Some("18.1".into())
        );
        assert_eq!(version_of(&config(&[], &["PATH=/bin"])), None);
    }

    #[test]
    fn records_keep_the_time_an_image_was_first_deployed() {
        let running = |id: &str| Running {
            service: "caddy".into(),
            reference: "caddy:2-alpine".into(),
            image_id: id.into(),
            running: true,
            started: String::new(),
        };
        let first = records(&[], &[running("sha256:a")], "2026-10-01T00:00:00Z");
        let same = records(&first, &[running("sha256:a")], "2026-10-02T00:00:00Z");
        assert_eq!(same[0].since, "2026-10-01T00:00:00Z");
        let newer = records(&same, &[running("sha256:b")], "2026-10-03T00:00:00Z");
        assert_eq!(newer[0].since, "2026-10-03T00:00:00Z");
    }

    #[test]
    fn shortens_times_and_ids() {
        assert_eq!(short_time("2026-10-02T14:03:51.12Z"), "2026-10-02 14:03");
        assert_eq!(short_id("sha256:d8542f48d34a9cf4e4c1"), "d8542f48d34a");
    }
}
