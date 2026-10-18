//! `compost server prune`: show disk usage, then remove images no container
//! uses (but not those kept for `app rollback`), dangling images and the
//! build cache.

use anyhow::Result;

use super::lock::Lock;
use crate::app::release::IMAGE_REPOSITORY;
use crate::app::state::initialized_server;
use crate::connector::{Connector, shell_quote};
use crate::shared::ServerCtx;

pub struct Options {
    /// Show what would be removed, without removing it.
    pub dry_run: bool,
    pub yes: bool,
    pub force_unlock: bool,
}

/// A tagged image.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Image {
    id: String,
    /// `repository:tag`
    name: String,
    size: String,
}

/// Lines of `docker image ls --no-trunc --format '{{.ID}}\t{{.Repository}}:{{.Tag}}\t{{.Size}}'`.
fn parse_images(listing: &str) -> Vec<Image> {
    listing
        .lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            Some(Image {
                id: parts.next()?.trim().to_string(),
                name: parts.next()?.trim().to_string(),
                size: parts.next().unwrap_or_default().trim().to_string(),
            })
        })
        .collect()
}

/// Tags to remove: images no container uses, except release images and
/// dangling ones (`<none>`, removed by `docker image prune`).
fn unused(images: &[Image], used_ids: &[&str]) -> Vec<Image> {
    let release = format!("{IMAGE_REPOSITORY}/");
    images
        .iter()
        .filter(|image| {
            !used_ids.contains(&image.id.as_str())
                && !image.name.starts_with(&release)
                && !image.name.contains("<none>")
        })
        .cloned()
        .collect()
}

pub fn prune(
    connector: &dyn Connector,
    server_home: &str,
    manager_key: &str,
    options: &Options,
) -> Result<()> {
    let ctx = initialized_server(connector, server_home, manager_key)?;
    print_usage(&ctx)?;

    // Under the lock: a deploy may have pulled images it hasn't started yet.
    let _lock = if options.dry_run {
        None
    } else {
        Some(Lock::acquire(connector, &ctx.home, options.force_unlock)?)
    };
    let images = parse_images(&connector.exec(
        "docker image ls --no-trunc --format '{{.ID}}\t{{.Repository}}:{{.Tag}}\t{{.Size}}'",
    )?);
    let used = connector.exec(
        "docker ps -aq --no-trunc | while read -r id; do docker inspect --format '{{.Image}}' \"$id\"; done",
    )?;
    let used_ids: Vec<&str> = used.lines().map(str::trim).collect();
    let remove = unused(&images, &used_ids);
    let kept = images
        .iter()
        .filter(|i| i.name.starts_with(&format!("{IMAGE_REPOSITORY}/")))
        .count();

    out!();
    if remove.is_empty() {
        out!("No unused images.");
    } else {
        out!("Unused images to remove:");
        let width = remove.iter().map(|i| i.name.len()).max().unwrap_or(0);
        for image in &remove {
            out!("  {:<width$}  {}", image.name, image.size);
        }
        out!("  (an image that is also kept for a release only loses this name)");
    }
    out!("Also removed: dangling images (<none>) and the build cache.");
    if kept > 0 {
        out!(
            "Kept: {kept} release images for `app rollback` (`deployment.releases` sets how many)."
        );
    }
    if options.dry_run {
        out!("Dry run: nothing is removed.");
        return Ok(());
    }
    if !options.yes && !crate::app::confirm()? {
        out!("Cancelled.");
        return Ok(());
    }

    for image in &remove {
        // A container may have started on it meanwhile: then docker refuses, which is fine.
        let output = connector.run(
            &format!("docker image rm {}", shell_quote(&image.name)),
            None,
        )?;
        if !output.success() {
            eprintln!(
                "warning: {} not removed: {}",
                image.name,
                output.stderr.trim()
            );
        }
    }
    let dangling = connector.exec("docker image prune -f")?;
    let cache = connector.exec("docker builder prune -f")?;
    for (what, output) in [("dangling images", &dangling), ("build cache", &cache)] {
        if let Some(space) = output
            .lines()
            .find_map(|l| l.strip_prefix("Total reclaimed space:"))
        {
            out!("  {what}: {} reclaimed", space.trim());
        }
    }
    out!("Removed {} unused images.", remove.len());
    out!();
    print_disk(&ctx)?;
    Ok(())
}

/// The server's disk, Docker's use of it, and compost's backups and releases.
fn print_usage(ctx: &ServerCtx) -> Result<()> {
    print_disk(ctx)?;
    let df = ctx
        .connector
        .exec("docker system df --format '{{.Type}}\t{{.Size}}\t{{.Reclaimable}}'")?;
    out!("Docker:");
    for line in df.lines() {
        let mut parts = line.split('\t');
        let (Some(kind), Some(size)) = (parts.next(), parts.next()) else {
            continue;
        };
        let reclaimable = parts.next().unwrap_or_default();
        out!("  {kind:<14} {size:>9}  reclaimable {reclaimable}");
    }
    let home = shell_quote(&ctx.home);
    let du = ctx.connector.exec(&format!(
        "cd {home} && b=$(du -sk backups 2>/dev/null | cut -f1); \
         r=$(du -skc apps/*/*/releases 2>/dev/null | tail -n 1 | cut -f1); \
         echo \"${{b:-0}} ${{r:-0}}\""
    ))?;
    let mut sizes = du
        .split_whitespace()
        .map(|n| n.parse::<u64>().unwrap_or(0) * 1024);
    let (backups, releases) = (sizes.next().unwrap_or(0), sizes.next().unwrap_or(0));
    out!(
        "compost: backups folder {}, releases {} (sources; their images are in Docker's)",
        crate::backup::human_size(backups),
        crate::backup::human_size(releases)
    );
    Ok(())
}

/// The disk with the server's compost folder.
pub struct Disk {
    pub mount: String,
    /// Bytes.
    pub total: u64,
    pub used: u64,
    pub available: u64,
}

impl Disk {
    /// Free space in percent, as `df` counts it: of what is used or available.
    pub fn free_percent(&self) -> f64 {
        let usable = self.used + self.available;
        if usable == 0 {
            return 100.0;
        }
        self.available as f64 * 100.0 / usable as f64
    }

    /// e.g. `Disk /: 41.2 GB free of 80.0 GB (49% used)`
    pub fn describe(&self) -> String {
        format!(
            "Disk {}: {} free of {} ({:.0}% used)",
            self.mount,
            crate::backup::human_size(self.available),
            crate::backup::human_size(self.total),
            100.0 - self.free_percent()
        )
    }
}

/// `df` of the disk with the server's compost folder.
pub fn disk(ctx: &ServerCtx) -> Result<Option<Disk>> {
    let df = ctx
        .connector
        .exec(&format!("df -Pk {} | tail -n 1", shell_quote(&ctx.home)))?;
    // Filesystem 1024-blocks Used Available Capacity Mounted-on
    let fields: Vec<&str> = df.split_whitespace().collect();
    let [_, total, used, available, _, mount, ..] = fields[..] else {
        return Ok(None);
    };
    let kb = |s: &str| s.parse::<u64>().unwrap_or(0) * 1024;
    Ok(Some(Disk {
        mount: mount.to_string(),
        total: kb(total),
        used: kb(used),
        available: kb(available),
    }))
}

/// Free space on the disk with the server's compost folder.
fn print_disk(ctx: &ServerCtx) -> Result<()> {
    if let Some(disk) = disk(ctx)? {
        out!("{}", disk.describe());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unused_images_keep_release_and_running_ones() {
        let images = parse_images(
            "sha256:aa\tmyorg/web:1\t200MB\n\
             sha256:aa\tcompost-release/myapp-prod:3-web\t200MB\n\
             sha256:bb\tpostgres:18\t400MB\n\
             sha256:cc\t<none>:<none>\t10MB\n\
             sha256:dd\tcompost-caddysecurebuild-caddy-main:old\t80MB\n",
        );
        let names: Vec<String> = unused(&images, &["sha256:bb"])
            .into_iter()
            .map(|i| i.name)
            .collect();
        assert_eq!(
            names,
            ["myorg/web:1", "compost-caddysecurebuild-caddy-main:old"]
        );
    }
}
