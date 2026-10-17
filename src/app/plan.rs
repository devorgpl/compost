//! `app deploy --dry-run`: what the deploy would change on the server and
//! what would happen to each service.

use std::collections::BTreeMap;
use std::fs;

use anyhow::Result;
use sha2::{Digest, Sha256};

use super::instance;
use super::release;
use super::sources::Upload;
use crate::config::app::LoadedApp;
use crate::connector::shell_quote;
use crate::server::plan::{self, Change, FileChange, ServiceChange};
use crate::server::state_file::StateFile;
use crate::shared::{RenderedFile, ServerCtx};

/// What the dry run found before printing.
pub struct AppPlan<'a> {
    pub loaded: &'a LoadedApp,
    pub uploads: &'a [Upload],
    /// Files the deploy writes besides the sources, relative to the server home.
    pub files: Vec<RenderedFile>,
    /// The compose override the deploy writes.
    pub override_yaml: &'a str,
    /// e.g. `services.database[main-db]: uses postgres postgres18`.
    pub links: Vec<String>,
    /// Links of the last deploy that the config no longer has.
    pub dropped: Vec<String>,
    pub apply: bool,
    pub pull: bool,
    pub recreate: bool,
    pub diff: bool,
}

/// Print the plan. `ctx` reads from the server.
pub fn print(ctx: &ServerCtx, plan: &AppPlan) -> Result<()> {
    let app = &plan.loaded.config;
    let project = app.instance();
    let dir = instance::dir(&app.name, &app.prefix);
    let releases = release::list(ctx, &app.name, &app.prefix)?;
    let status = match StateFile::load(ctx)?.app(&app.name, &app.prefix) {
        Some(record) => match releases.last() {
            Some(release) => format!("{}, release {}", record.status, release.number),
            None => record.status.to_string(),
        },
        None => "not deployed yet".to_string(),
    };
    out!("Dry run: nothing is changed on the server.");
    out!("  instance {project} ({status})");

    let sources = source_changes(ctx, &dir, plan.uploads, plan.diff)?;
    if sources.is_empty() {
        out!("  sources: unchanged ({} files)", plan.uploads.len());
    } else {
        out!("  sources ({} files):", plan.uploads.len());
        plan::print_files("    ", &sources, plan.diff);
    }

    for link in &plan.links {
        out!("  {link}");
    }
    for link in &plan.dropped {
        out!("  {link}");
    }

    let files = plan::compare(ctx, &plan.files)?;
    if files.is_empty() {
        out!("  files: unchanged");
    } else {
        out!("  files:");
        plan::print_files("    ", &files, plan.diff);
    }

    if plan.apply {
        out!("  services:");
    } else {
        out!("  services, once deployed with --apply:");
    }
    let prefix = format!("{dir}/sources/");
    let changed_sources: Vec<&str> = sources
        .iter()
        .filter_map(|change| change.path.strip_prefix(&prefix))
        .collect();
    let services = service_lines(ctx, plan, &dir, &changed_sources)?;
    let width = services.keys().map(String::len).max().unwrap_or(0);
    for (service, line) in &services {
        out!("    {service:<width$}  {line}");
    }
    if plan.apply && app.deployment.releases > 0 {
        let next = releases.last().map_or(1, |r| r.number + 1);
        out!("  records release {next}");
    }
    if !plan.apply {
        out!("Without --apply, the files are uploaded and the services are not started.");
    }
    Ok(())
}

/// Sources added, changed or removed, compared by SHA-256 with the server's.
fn source_changes(
    ctx: &ServerCtx,
    dir: &str,
    uploads: &[Upload],
    diff: bool,
) -> Result<Vec<FileChange>> {
    let sources_dir = format!("{dir}/sources");
    let output = ctx.connector.exec(&format!(
        "cd {} 2>/dev/null || exit 0; find . -type f -exec sha256sum {{}} +",
        shell_quote(&ctx.path(&sources_dir))
    ))?;
    let mut on_server: BTreeMap<String, String> = output
        .lines()
        .filter_map(|line| {
            let (hash, path) = line.split_once("  ")?;
            Some((path.strip_prefix("./")?.to_string(), hash.to_string()))
        })
        .collect();

    let mut changes = Vec::new();
    for upload in uploads {
        let name = upload.name.to_string_lossy().into_owned();
        let contents = fs::read(&upload.from)?;
        let hash = hex(&Sha256::digest(&contents));
        let change = match on_server.remove(&name) {
            None => Change::Added,
            Some(old) if old == hash => continue,
            Some(_) => Change::Changed,
        };
        let path = format!("{sources_dir}/{name}");
        let (old, new) = if diff {
            let old = match change {
                Change::Changed => ctx.read(&path)?,
                _ => None,
            };
            (old, Some(String::from_utf8_lossy(&contents).into_owned()))
        } else {
            (None, None)
        };
        changes.push(FileChange {
            path,
            change,
            secret: false,
            old,
            new,
        });
    }
    for name in on_server.into_keys() {
        let path = format!("{sources_dir}/{name}");
        let old = if diff { ctx.read(&path)? } else { None };
        changes.push(FileChange::removed(path, old));
    }
    Ok(changes)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut text, byte| {
        use std::fmt::Write as _;
        let _ = write!(text, "{byte:02x}");
        text
    })
}

/// What `--apply` would do with each service, as compose decides it from the
/// compose files, the override and the images.
fn service_lines(
    ctx: &ServerCtx,
    plan: &AppPlan,
    dir: &str,
    changed_sources: &[&str],
) -> Result<BTreeMap<String, String>> {
    let app = &plan.loaded.config;
    let mut old = Vec::new();
    if let Some(deployed) = instance::load(ctx, &app.name, &app.prefix)? {
        for file in deployed.app.deployment.compose_files() {
            old.extend(ctx.read(&format!("{dir}/sources/{file}"))?);
        }
        old.extend(ctx.read(&format!("{dir}/{}", instance::OVERRIDE_FILE))?);
    }
    let mut new = Vec::new();
    for file in app.deployment.compose_files() {
        if let Some(upload) = plan
            .uploads
            .iter()
            .find(|u| u.name.to_string_lossy() == file)
        {
            new.push(fs::read_to_string(&upload.from)?);
        }
    }
    let contexts = build_contexts(&new);
    new.push(plan.override_yaml.to_string());

    let build = app.deployment.build;
    let lines = plan::service_changes(&old, &new)
        .into_iter()
        .map(|(service, change)| {
            let builds = build
                && plan
                    .loaded
                    .compose
                    .services
                    .get(&service)
                    .is_some_and(|s| s.builds);
            let line = match change {
                ServiceChange::Created => "created".to_string(),
                ServiceChange::Removed => "removed (no longer in the compose files)".to_string(),
                _ if plan.recreate => "recreated (--recreate)".to_string(),
                ServiceChange::Changed(details) => format!("recreated: {}", details.join("; ")),
                ServiceChange::Unchanged
                    if builds && context_changed(contexts.get(&service), changed_sources) =>
                {
                    "built again; recreated if its image changes".to_string()
                }
                ServiceChange::Unchanged if builds => "unchanged (built from cache)".to_string(),
                ServiceChange::Unchanged if !build || plan.pull => {
                    "pulled; recreated if a newer image comes".to_string()
                }
                ServiceChange::Unchanged => "unchanged".to_string(),
            };
            (service, line)
        })
        .collect();
    Ok(lines)
}

/// The build context of each service with `build`, relative to the sources;
/// the last compose file that sets it wins.
fn build_contexts(texts: &[String]) -> BTreeMap<String, String> {
    let mut contexts = BTreeMap::new();
    for text in texts {
        let Ok(value) = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(text) else {
            continue;
        };
        let Some(services) = value.get("services").and_then(|s| s.as_mapping()) else {
            continue;
        };
        for (name, service) in services {
            let build = service.get("build");
            let context = build.and_then(|b| b.as_str()).or_else(|| {
                build
                    .and_then(|b| b.get("context"))
                    .and_then(|c| c.as_str())
            });
            if let (Some(name), Some(context)) = (name.as_str(), context) {
                let context = context.trim_start_matches("./").trim_end_matches('/');
                let context = if context == "." { "" } else { context };
                contexts.insert(name.to_string(), context.to_string());
            }
        }
    }
    contexts
}

/// A changed source is inside the build context. Without a known context
/// (e.g. one outside the sources), any change counts.
fn context_changed(context: Option<&String>, changed: &[&str]) -> bool {
    match context {
        Some(context) if !context.is_empty() && !context.starts_with("..") => {
            changed.iter().any(|path| {
                path.strip_prefix(context.as_str())
                    .is_some_and(|rest| rest.starts_with('/'))
            })
        }
        _ => !changed.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebuilds_only_services_whose_context_changed() {
        let contexts = build_contexts(&[
            "services: {web: {build: ./web}, api: {build: {context: backend}}, root: {build: .}}"
                .to_string(),
        ]);
        let changed = ["web/index.html"];
        assert!(context_changed(contexts.get("web"), &changed));
        assert!(!context_changed(contexts.get("api"), &changed));
        assert!(context_changed(contexts.get("root"), &changed));
        assert!(!context_changed(contexts.get("web"), &["webapp/x"]));
        assert!(!context_changed(contexts.get("root"), &[]));
    }
}
