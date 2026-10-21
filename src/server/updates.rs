//! `compost server images --check`: compare what the shared apps run with
//! their registries, and compost with its latest release.

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use serde_json::Value;

use super::images::{self, Image, Project, Running};
use crate::connector::{Connector, shell_quote};
use crate::shared::{self, ServerCtx};

const RELEASES_URL: &str = "https://api.github.com/repos/devorgpl/compost/releases/latest";

/// What the registry has for one image a container runs or was built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Update {
    /// The registry has the image the server has.
    Current { reference: String },
    /// The registry has another image for the tag.
    Newer {
        reference: String,
        /// Built on the server from it, rather than run.
        base: bool,
        running: Option<String>,
        available: Option<String>,
    },
    /// The registry could not be asked, or did not say.
    Unknown { reference: String, reason: String },
}

/// The newest image of a reference in its registry.
#[derive(Debug, Clone)]
struct Remote {
    digest: String,
    version: Option<String>,
}

/// Asks registries once per reference.
pub struct Checker<'a> {
    connector: &'a dyn Connector,
    platform: String,
    remotes: BTreeMap<String, Result<Remote, String>>,
}

impl<'a> Checker<'a> {
    pub fn new(connector: &'a dyn Connector) -> Result<Self> {
        let platform = connector
            .exec("docker version -f '{{.Server.Os}}/{{.Server.Arch}}'")?
            .trim()
            .to_string();
        Ok(Self {
            connector,
            platform,
            remotes: BTreeMap::new(),
        })
    }

    fn remote(&mut self, reference: &str) -> Result<Remote, String> {
        if let Some(found) = self.remotes.get(reference) {
            return found.clone();
        }
        let found = self.ask(reference).map_err(|e| {
            let text = format!("{e:#}");
            let line = text
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("no answer");
            line.trim().to_string()
        });
        self.remotes.insert(reference.to_string(), found.clone());
        found
    }

    /// `docker buildx imagetools inspect`: the digest of the tag's index (or
    /// manifest), and the version in the configuration for this platform.
    fn ask(&self, reference: &str) -> Result<Remote> {
        let quoted = shell_quote(reference);
        let manifest = self.connector.exec(&format!(
            "docker buildx imagetools inspect {quoted} --format '{{{{json .Manifest}}}}'"
        ))?;
        let manifest: Value = serde_json::from_str(manifest.trim())?;
        let Some(digest) = manifest.get("digest").and_then(Value::as_str) else {
            bail!("the registry sent no digest");
        };
        let config = self.connector.exec(&format!(
            "docker buildx imagetools inspect {quoted} --format '{{{{json .Image}}}}'"
        ))?;
        let config: Value = serde_json::from_str(config.trim())?;
        Ok(Remote {
            digest: digest.to_string(),
            version: platform_config(&config, &self.platform).and_then(version),
        })
    }

    /// What the registry has for each image a container runs or was built from.
    pub fn container(
        &mut self,
        ctx: &ServerCtx,
        project: &Project,
        container: &Running,
        images: &BTreeMap<String, Image>,
    ) -> Result<Vec<Update>> {
        let running = images.get(&container.image_id);
        let running_version = running.and_then(|i| i.version.clone());
        let Some(bases) = build_bases(ctx, project, container)? else {
            let local = running.map(digests).unwrap_or_default();
            return Ok(vec![self.compare(
                &container.reference,
                false,
                &local,
                running_version,
            )]);
        };
        // Built on the server: its bases, by their digests where they are still
        // on the server, else by the version the built image inherited.
        let based: Vec<&str> = bases.iter().map(String::as_str).collect();
        let local = images::inspect(self.connector, &based)?;
        Ok(bases
            .iter()
            .map(|base| match local.get(base) {
                Some(image) => self.compare(base, true, &digests(image), image.version.clone()),
                None => self.compare(base, true, &[], running_version.clone()),
            })
            .collect())
    }

    fn compare(
        &mut self,
        reference: &str,
        base: bool,
        local: &[String],
        running: Option<String>,
    ) -> Update {
        let reference = reference.to_string();
        match self.remote(&reference) {
            Err(reason) => Update::Unknown { reference, reason },
            Ok(remote) => {
                let same = if local.is_empty() {
                    // Only versions to compare.
                    match (&running, &remote.version) {
                        (Some(a), Some(b)) => Some(a == b),
                        _ => None,
                    }
                } else {
                    Some(local.contains(&remote.digest))
                };
                match same {
                    Some(true) => Update::Current { reference },
                    Some(false) => Update::Newer {
                        reference,
                        base,
                        running,
                        available: remote.version,
                    },
                    None => Update::Unknown {
                        reference,
                        reason: "the image is not on the server to compare".to_string(),
                    },
                }
            }
        }
    }
}

/// The digests an image is known by: its ID (an index digest with the
/// containerd image store) and its registry digests.
fn digests(image: &Image) -> Vec<String> {
    let mut digests = vec![image.id.clone()];
    digests.extend(
        image
            .digests
            .iter()
            .filter_map(|d| d.rsplit_once('@').map(|(_, digest)| digest.to_string())),
    );
    digests
}

/// The `FROM` images of a shared app's `build/Dockerfile`, when the container
/// runs an image compost built there.
fn build_bases(ctx: &ServerCtx, project: &Project, c: &Running) -> Result<Option<Vec<String>>> {
    if !c.reference.starts_with("compost-") {
        return Ok(None);
    }
    let path = format!(
        "{}/build/Dockerfile",
        shared::shared_dir(&project.kind, &project.name)
    );
    let Some(dockerfile) = ctx.read(&path)? else {
        return Ok(None);
    };
    Ok(Some(from_images(&dockerfile)))
}

/// `FROM caddy:2-builder AS builder` → `caddy:2-builder`; stages built from
/// another stage are left out.
fn from_images(dockerfile: &str) -> Vec<String> {
    let mut stages: Vec<String> = Vec::new();
    let mut images: Vec<String> = Vec::new();
    for line in dockerfile.lines() {
        let mut words = line.split_whitespace();
        if !words.next().is_some_and(|w| w.eq_ignore_ascii_case("FROM")) {
            continue;
        }
        let words: Vec<&str> = words.filter(|w| !w.starts_with("--")).collect();
        let Some(image) = words.first() else {
            continue;
        };
        if !stages.iter().any(|s| s == image) && !images.iter().any(|i| i == image) {
            images.push(image.to_string());
        }
        if let [_, as_word, name, ..] = words.as_slice() {
            if as_word.eq_ignore_ascii_case("AS") {
                stages.push(name.to_string());
            }
        }
    }
    images
}

/// The configuration for `platform` (`linux/amd64`) of what `.Image` printed:
/// one per platform for an index, or the only one.
fn platform_config<'v>(config: &'v Value, platform: &str) -> Option<&'v Value> {
    if config.get("config").is_some() {
        return Some(config);
    }
    let platforms = config.as_object()?;
    platforms
        .iter()
        .find(|(key, _)| *key == platform || key.starts_with(&format!("{platform}/")))
        .map(|(_, value)| value)
}

fn version(config: &Value) -> Option<String> {
    let image_config: images::ImageConfig =
        serde_json::from_value(config.get("config")?.clone()).ok()?;
    images::version_of(&image_config)
}

/// One line about an update, or `None` when the image is current.
pub fn describe(update: &Update) -> Option<String> {
    match update {
        Update::Current { .. } => None,
        Update::Newer {
            reference,
            base,
            running,
            available,
        } => {
            let what = if *base {
                format!("newer {reference} to build on")
            } else {
                format!("newer image for {reference}")
            };
            Some(match (running, available) {
                (Some(running), Some(available)) if running != available => {
                    format!("{what}: version {available} (running {running})")
                }
                (_, Some(available)) => format!("{what}: version {available}, rebuilt"),
                _ => what,
            })
        }
        Update::Unknown { reference, reason } => {
            Some(format!("cannot check {reference}: {reason}"))
        }
    }
}

/// The command that updates a shared app to the newer images of its tags.
pub fn command(name: &str) -> String {
    format!("compost server deploy --apply --pull --shared {name}")
}

/// The latest compost release, if it is newer than this compost.
pub fn newer_compost(connector: &dyn Connector) -> Result<Option<String>> {
    let url = shell_quote(RELEASES_URL);
    let text = connector.exec(&format!(
        "if command -v curl >/dev/null; then curl -fsSL --max-time 20 {url}; \
         else wget -qO- -T 20 {url}; fi"
    ))?;
    let release: Value = serde_json::from_str(&text)?;
    let Some(tag) = release.get("tag_name").and_then(Value::as_str) else {
        bail!("GitHub sent no release");
    };
    let latest = semver::Version::parse(tag.trim_start_matches('v'))?;
    let this = semver::Version::parse(env!("CARGO_PKG_VERSION"))?;
    Ok((latest > this).then(|| tag.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_images_a_dockerfile_builds_from() {
        let dockerfile = "# syntax=docker/dockerfile:1\nFROM caddy:2-builder AS builder\nRUN xcaddy build\n\nFROM caddy:2-alpine\nCOPY --from=builder /usr/bin/caddy /usr/bin/caddy\nFROM builder AS again\n";
        assert_eq!(
            from_images(dockerfile),
            vec!["caddy:2-builder", "caddy:2-alpine"]
        );
    }

    #[test]
    fn picks_the_configuration_of_the_platform() {
        let index: Value = serde_json::from_str(
            r#"{"linux/amd64": {"config": {"Env": ["CADDY_VERSION=v2.11.7"]}},
                "linux/arm64/v8": {"config": {"Env": ["CADDY_VERSION=v2.11.6"]}}}"#,
        )
        .unwrap();
        let version_on = |p| platform_config(&index, p).and_then(version);
        assert_eq!(version_on("linux/amd64"), Some("v2.11.7".into()));
        assert_eq!(version_on("linux/arm64"), Some("v2.11.6".into()));
        let single: Value = serde_json::from_str(
            r#"{"config": {"Labels": {"org.opencontainers.image.version": "1.2.3"}}}"#,
        )
        .unwrap();
        assert_eq!(
            platform_config(&single, "linux/amd64").and_then(version),
            Some("1.2.3".into())
        );
    }

    #[test]
    fn describes_updates() {
        let newer = |running: Option<&str>, available: Option<&str>| {
            describe(&Update::Newer {
                reference: "caddy:2-alpine".into(),
                base: false,
                running: running.map(Into::into),
                available: available.map(Into::into),
            })
            .unwrap()
        };
        assert_eq!(
            newer(Some("v2.11.7"), Some("v2.11.8")),
            "newer image for caddy:2-alpine: version v2.11.8 (running v2.11.7)"
        );
        assert_eq!(
            newer(Some("v2.11.7"), Some("v2.11.7")),
            "newer image for caddy:2-alpine: version v2.11.7, rebuilt"
        );
        assert_eq!(newer(None, None), "newer image for caddy:2-alpine");
        assert_eq!(
            describe(&Update::Current {
                reference: "x".into()
            }),
            None
        );
    }
}
