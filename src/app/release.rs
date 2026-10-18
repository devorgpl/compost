//! Releases of an application instance: what every deploy with `--apply`
//! ran, kept on the server so `app rollback` can deploy it again.
//!
//! `apps/<app>/<prefix>/releases/<n>/` holds a copy of the sources, the
//! stored config and `release.yaml`. The images the services ran are tagged
//! `compost-release/<instance>:<n>-<service>`, so pulls and `docker image
//! prune` keep them.

use std::time::SystemTime;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::instance;
use crate::config::app::AppConfig;
use crate::connector::shell_quote;
use crate::server::lock;
use crate::shared::ServerCtx;

/// Relative to the instance folder.
const DIR: &str = "releases";
const RELEASE_FILE: &str = "release.yaml";
/// Repository of the pinned images; the instance follows the `/`.
pub const IMAGE_REPOSITORY: &str = "compost-release";
/// Exit status of the retag script when a pinned image is missing.
const MISSING_STATUS: i32 = 3;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Release {
    pub number: u32,
    pub created_at: String,
    pub compost_version: String,
    /// `user@host` of the deploy.
    pub by: String,
    /// The release this one deployed again, for a rollback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollback_of: Option<u32>,
    #[serde(default)]
    pub images: Vec<ReleaseImage>,
}

/// The image one compose service ran.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseImage {
    pub service: String,
    /// The reference the container was created from, e.g. `nginx:1`.
    pub image: String,
    /// `sha256:…`
    pub id: String,
}

impl ReleaseImage {
    /// The first 12 hexadecimal digits of the ID, as `docker images` shows it.
    pub fn short_id(&self) -> &str {
        let hex = self.id.strip_prefix("sha256:").unwrap_or(&self.id);
        &hex[..hex.len().min(12)]
    }
}

/// `apps/<app>/<prefix>/releases/<n>`
pub fn dir(name: &str, prefix: &str, number: u32) -> String {
    format!("{}/{DIR}/{number}", instance::dir(name, prefix))
}

/// The tag that keeps the image of `service` in release `number`.
pub fn pinned_tag(instance: &str, number: u32, service: &str) -> String {
    format!("{IMAGE_REPOSITORY}/{instance}:{number}-{service}")
}

/// The releases of an instance, oldest first. Folders without `release.yaml`
/// (an interrupted deploy) are left out.
pub fn list(ctx: &ServerCtx, name: &str, prefix: &str) -> Result<Vec<Release>> {
    let mut releases = Vec::new();
    for number in numbers(ctx, name, prefix)? {
        let path = format!("{}/{RELEASE_FILE}", dir(name, prefix, number));
        if let Some(text) = ctx.read(&path)? {
            let release: Release = serde_yaml_ng::from_str(&text)
                .with_context(|| format!("invalid {} on the server", ctx.path(&path)))?;
            releases.push(release);
        }
    }
    Ok(releases)
}

/// Numbers of the release folders, ascending.
fn numbers(ctx: &ServerCtx, name: &str, prefix: &str) -> Result<Vec<u32>> {
    let path = ctx.path(&format!("{}/{DIR}", instance::dir(name, prefix)));
    let mut numbers: Vec<u32> = ctx
        .connector
        .list_dir(&path)?
        .iter()
        .filter_map(|entry| entry.parse().ok())
        .collect();
    numbers.sort_unstable();
    Ok(numbers)
}

/// Record what the instance runs now as its next release, then keep only the
/// newest `deployment.releases`. Returns the new number, or `None` when
/// releases are turned off. The caller holds the lock.
pub fn record(ctx: &ServerCtx, app: &AppConfig, rollback_of: Option<u32>) -> Result<Option<u32>> {
    let keep = app.deployment.releases;
    if keep == 0 {
        prune(ctx, app, 0)?;
        return Ok(None);
    }
    let number = numbers(ctx, &app.name, &app.prefix)?
        .last()
        .map_or(1, |last| last + 1);
    let instance_dir = shell_quote(&ctx.path(&instance::dir(&app.name, &app.prefix)));
    let release_dir = format!("{DIR}/{number}");
    ctx.connector.exec(&format!(
        "cd {instance_dir} && rm -rf -- {release_dir} && mkdir -p -- {release_dir} \
         && cp -a sources {release_dir}/sources && cp {} {release_dir}/{}",
        instance::CONFIG_FILE,
        instance::CONFIG_FILE
    ))?;

    let images = running_images(ctx, &app.instance())?;
    let tags: Vec<String> = images
        .iter()
        .map(|image| {
            format!(
                "docker tag {} {}",
                shell_quote(&image.id),
                shell_quote(&pinned_tag(&app.instance(), number, &image.service))
            )
        })
        .collect();
    if !tags.is_empty() {
        ctx.connector.exec(&tags.join(" && "))?;
    }

    let release = Release {
        number,
        created_at: humantime::format_rfc3339_seconds(SystemTime::now()).to_string(),
        compost_version: env!("CARGO_PKG_VERSION").to_string(),
        by: lock::operator(),
        rollback_of,
        images,
    };
    let path = ctx.path(&format!(
        "{}/{RELEASE_FILE}",
        dir(&app.name, &app.prefix, number)
    ));
    ctx.connector
        .write_file(&path, serde_yaml_ng::to_string(&release)?.as_bytes(), 0o644)?;
    prune(ctx, app, keep)?;
    Ok(Some(number))
}

/// The image of every service of the compose project, one per service.
fn running_images(ctx: &ServerCtx, project: &str) -> Result<Vec<ReleaseImage>> {
    let filter = shell_quote(&format!("label=com.docker.compose.project={project}"));
    let output = ctx.connector.exec(&format!(
        "docker ps -aq --filter {filter} | while read -r id; do \
         docker inspect -f '{{{{index .Config.Labels \"com.docker.compose.service\"}}}} {{{{.Config.Image}}}} {{{{.Image}}}}' \"$id\"; done"
    ))?;
    let mut images: Vec<ReleaseImage> = output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some(ReleaseImage {
                service: fields.next()?.to_string(),
                image: fields.next()?.to_string(),
                id: fields.next()?.to_string(),
            })
        })
        .collect();
    images.sort_by(|a, b| a.service.cmp(&b.service));
    images.dedup_by(|a, b| a.service == b.service);
    Ok(images)
}

/// Delete all but the newest `keep` releases with their pinned images.
fn prune(ctx: &ServerCtx, app: &AppConfig, keep: u32) -> Result<()> {
    let numbers = numbers(ctx, &app.name, &app.prefix)?;
    let old = numbers.len().saturating_sub(keep as usize);
    if old == 0 {
        return Ok(());
    }
    let old = &numbers[..old];
    let dirs: Vec<String> = old
        .iter()
        .map(|n| shell_quote(&ctx.path(&dir(&app.name, &app.prefix, *n))))
        .collect();
    ctx.connector
        .exec(&format!("rm -rf -- {}", dirs.join(" ")))?;
    remove_images(ctx, &app.instance(), Some(old))
}

/// Remove the pinned image tags of these releases (all with `None`). An image
/// that another tag or a container still uses stays.
pub fn remove_images(ctx: &ServerCtx, instance: &str, numbers: Option<&[u32]>) -> Result<()> {
    let pattern = match numbers {
        Some(numbers) => numbers
            .iter()
            .map(|n| format!("{n}-*"))
            .collect::<Vec<_>>()
            .join("|"),
        None => "*".to_string(),
    };
    let repository = shell_quote(&format!("{IMAGE_REPOSITORY}/{instance}"));
    ctx.connector.exec(&format!(
        "docker image ls --format '{{{{.Tag}}}}' {repository} | while read -r tag; do \
         case \"$tag\" in {pattern}) docker image rm {repository}:\"$tag\" >/dev/null 2>&1 || true;; esac; done"
    ))?;
    Ok(())
}

/// Point every image reference of `release` at its pinned image, so the
/// services start exactly what the release ran. `false` (with a warning) when
/// one is missing, e.g. removed by hand; nothing is changed then.
pub fn retag(ctx: &ServerCtx, instance: &str, release: &Release) -> Result<bool> {
    if release.images.is_empty() {
        return Ok(false);
    }
    let mut checks = Vec::new();
    let mut tags = Vec::new();
    for image in &release.images {
        let pinned = shell_quote(&pinned_tag(instance, release.number, &image.service));
        checks.push(format!(
            "docker image inspect {pinned} >/dev/null 2>&1 || exit {MISSING_STATUS}"
        ));
        tags.push(format!("docker tag {pinned} {}", shell_quote(&image.image)));
    }
    let script = format!("{}; {}", checks.join("; "), tags.join(" && "));
    let output = ctx.connector.run(&script, None)?;
    match output.status {
        0 => Ok(true),
        MISSING_STATUS => {
            eprintln!(
                "warning: images of release {} are missing; pulling or building them instead",
                release.number
            );
            Ok(false)
        }
        status => anyhow::bail!(
            "cannot tag the images of release {} (exit status {status}): {}",
            release.number,
            output.stderr.trim()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::Connector;
    use crate::connector::local::Local;
    use tempfile::TempDir;

    const LOCAL: Local = Local { verbose: false };

    #[test]
    fn release_file_round_trip() {
        let release = Release {
            number: 3,
            created_at: "2026-10-17T12:00:00Z".into(),
            compost_version: "1.3.0".into(),
            by: "cyprian@laptop".into(),
            rollback_of: Some(1),
            images: vec![ReleaseImage {
                service: "web".into(),
                image: "nginx:1".into(),
                id: "sha256:0123456789abcdef".into(),
            }],
        };
        let text = serde_yaml_ng::to_string(&release).unwrap();
        let back: Release = serde_yaml_ng::from_str(&text).unwrap();
        assert_eq!(back.rollback_of, Some(1));
        assert_eq!(back.images, release.images);
        assert_eq!(back.images[0].short_id(), "0123456789ab");
        assert_eq!(
            pinned_tag("myapp-main", 3, "web"),
            "compost-release/myapp-main:3-web"
        );
    }

    #[test]
    fn lists_releases_in_order_and_skips_unfinished_ones() {
        let home = TempDir::new().unwrap();
        let ctx = ServerCtx {
            connector: &LOCAL,
            home: home.path().to_str().unwrap().to_string(),
        };
        for number in [2, 10, 1] {
            let path = ctx.path(&format!("{}/{RELEASE_FILE}", dir("app", "main", number)));
            let text = format!("number: {number}\ncreated_at: x\ncompost_version: x\nby: x\n");
            LOCAL.write_file(&path, text.as_bytes(), 0o644).unwrap();
        }
        std::fs::create_dir_all(home.path().join("apps/app/main/releases/11")).unwrap();
        std::fs::create_dir_all(home.path().join("apps/app/main/releases/tmp")).unwrap();

        let numbers: Vec<u32> = list(&ctx, "app", "main")
            .unwrap()
            .iter()
            .map(|r| r.number)
            .collect();
        assert_eq!(numbers, [1, 2, 10]);
        assert_eq!(super::numbers(&ctx, "app", "main").unwrap(), [1, 2, 10, 11]);
        assert!(list(&ctx, "other", "main").unwrap().is_empty());
    }
}
