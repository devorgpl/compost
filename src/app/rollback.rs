//! `compost app releases` and `compost app rollback`: deploy an earlier
//! release again, with the images it ran.

use std::fs;

use anyhow::{Context, Result};

use super::instance::{self, Selector};
use super::release::{self, Release};
use super::restore::unpack_stored;
use super::state::{initialized_server, not_deployed};
use super::{deploy, secrets};
use crate::config::app::{AppConfig, link_name};
use crate::connector::{Connector, shell_quote};
use crate::error::config_err;

pub struct Options {
    /// The release to deploy (default: the one before the newest).
    pub to: Option<u32>,
    pub dry_run: bool,
    pub yes: bool,
    pub force_unlock: bool,
}

/// `compost app releases`
pub fn releases(
    connector: &dyn Connector,
    server_home: &str,
    manager_key: &str,
    selector: &Selector,
) -> Result<()> {
    let ctx = initialized_server(connector, server_home, manager_key)?;
    let (name, prefix) = instance::resolve(&ctx, selector)?;
    let instance = format!("{name}-{prefix}");
    let releases = release::list(&ctx, &name, &prefix)?;
    let Some(current) = releases.last().map(|r| r.number) else {
        out!("{instance} has no releases. Every deploy with --apply records one.");
        return Ok(());
    };
    out!(
        "{:<8} {:<21} {:<24} {:<10} IMAGES",
        "RELEASE",
        "CREATED",
        "BY",
        "COMPOST"
    );
    for release in releases.iter().rev() {
        let mut notes = Vec::new();
        if release.number == current {
            notes.push("current".to_string());
        }
        if let Some(of) = release.rollback_of {
            notes.push(format!("rollback of {of}"));
        }
        let images: Vec<String> = release
            .images
            .iter()
            .map(|image| format!("{}={}", image.service, image.short_id()))
            .collect();
        let notes = if notes.is_empty() {
            String::new()
        } else {
            format!("  ({})", notes.join(", "))
        };
        out!(
            "{:<8} {:<21} {:<24} {:<10} {}{notes}",
            release.number,
            release.created_at,
            release.by,
            release.compost_version,
            images.join(" ")
        );
    }
    Ok(())
}

/// `compost app rollback`
pub fn rollback(
    connector: &dyn Connector,
    server_home: &str,
    manager_key: &str,
    selector: &Selector,
    options: &Options,
) -> Result<()> {
    let ctx = initialized_server(connector, server_home, manager_key)?;
    let (name, prefix) = instance::resolve(&ctx, selector)?;
    let current =
        instance::load(&ctx, &name, &prefix)?.ok_or_else(|| not_deployed(&name, &prefix))?;
    let project = current.app.instance();
    let releases = release::list(&ctx, &name, &prefix)?;
    let newest = releases.last();
    let target = choose(&releases, options.to, &project)?;

    // The release's sources and config, as deploy reads them from a folder.
    let work = tempfile::Builder::new()
        .prefix("compost-rollback-")
        .tempdir()?;
    let release_dir = release::dir(&name, &prefix, target.number);
    let sources_tar = work.path().join("sources.tar");
    connector.stream_out(
        &format!(
            "tar -cf - -C {} .",
            shell_quote(&ctx.path(&format!("{release_dir}/sources")))
        ),
        &mut fs::File::create(&sources_tar)?,
    )?;
    let config_text = ctx
        .read(&format!("{release_dir}/{}", instance::CONFIG_FILE))?
        .with_context(|| format!("release {} has no {}", target.number, instance::CONFIG_FILE))?;
    let files = vec![unpack_stored(work.path(), &sources_tar, &config_text)?];
    let stored = secrets::read_in(&ctx, &name, &prefix)?.map(|(_, value)| value);
    let loaded = AppConfig::load_with(&files, stored, None)?;
    if loaded.config.instance() != project {
        return Err(config_err(format!(
            "release {} is of {}, not {project}",
            target.number,
            loaded.config.instance()
        )));
    }

    out!(
        "Rolling back {project} to release {} ({}, by {})",
        target.number,
        target.created_at,
        target.by
    );
    print_changes(target, newest, &current.app, &loaded.config);
    out!("  databases and volumes are not rolled back");
    if options.dry_run {
        out!("Dry run: nothing is changed on the server.");
        return Ok(());
    }
    if !options.yes && !super::confirm()? {
        out!("Cancelled.");
        return Ok(());
    }

    let deploy_options = deploy::Options {
        apply: true,
        dry_run: false,
        force_unlock: options.force_unlock,
        pull: false,
        recreate: false,
        restore: None,
        purge: Vec::new(),
        secrets: None,
        pinned: Some(target),
    };
    deploy::deploy(
        connector,
        server_home,
        manager_key,
        &loaded,
        &files,
        &deploy_options,
    )
}

/// `--to`, else the release before the newest one.
fn choose<'r>(releases: &'r [Release], to: Option<u32>, project: &str) -> Result<&'r Release> {
    let newest = releases.last().map(|r| r.number);
    match to {
        Some(number) if Some(number) == newest => Err(config_err(format!(
            "release {number} is the current release of {project}"
        ))),
        Some(number) => releases.iter().find(|r| r.number == number).ok_or_else(|| {
            config_err(format!(
                "{project} has no release {number} (see `compost app releases`)"
            ))
        }),
        None => releases.iter().rev().nth(1).ok_or_else(|| {
            config_err(format!(
                "{project} has no earlier release to roll back to (see `compost app releases`)"
            ))
        }),
    }
}

/// Images and links that change compared with the newest release. Links
/// are compared as configured, so an optional link skipped by both is no change.
fn print_changes(
    target: &Release,
    newest: Option<&Release>,
    current: &AppConfig,
    config: &AppConfig,
) {
    for image in &target.images {
        let now = newest
            .and_then(|r| r.images.iter().find(|i| i.service == image.service))
            .map(release::ReleaseImage::short_id);
        let change = match now {
            Some(now) if now == image.short_id() => "unchanged".to_string(),
            Some(now) => format!("now {now}"),
            None => "not running now".to_string(),
        };
        out!(
            "  image {} {} {} ({change})",
            image.service,
            image.image,
            image.short_id()
        );
    }
    let (current, target) = (link_names(current), link_names(config));
    for (kind, entry) in &target {
        if !current.contains(&(kind, entry)) {
            out!("  link {kind} {entry}: added");
        }
    }
    for (kind, entry) in &current {
        if !target.contains(&(kind, entry)) {
            out!("  link {kind} {entry}: removed; its data is kept");
        }
    }
}

/// `(kind, entry)` of every link in the config.
fn link_names(config: &AppConfig) -> Vec<(&str, &str)> {
    config
        .links()
        .filter_map(|(kind, entry)| Some((kind, link_name(entry)?)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(number: u32) -> Release {
        Release {
            number,
            created_at: String::new(),
            compost_version: String::new(),
            by: String::new(),
            rollback_of: None,
            images: Vec::new(),
        }
    }

    #[test]
    fn chooses_the_release_before_the_newest() {
        let releases = [release(1), release(2), release(4)];
        assert_eq!(choose(&releases, None, "a-main").unwrap().number, 2);
        assert_eq!(choose(&releases, Some(1), "a-main").unwrap().number, 1);
        for (to, expected) in [
            (Some(4), "is the current release"),
            (Some(3), "has no release 3"),
        ] {
            let err = choose(&releases, to, "a-main").unwrap_err().to_string();
            assert!(err.contains(expected), "{err}");
        }
        let err = choose(&releases[..1], None, "a-main")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no earlier release"), "{err}");
    }
}
