//! `--dry-run` plans: how the files a deploy would write differ from the
//! server, and what `docker compose up` would do with each service.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use serde_yaml_ng::Value;

use crate::connector::dry_run::Captured;
use crate::shared::{RenderedFile, ServerCtx};

/// Files larger than this are not diffed.
const MAX_DIFF_BYTES: usize = 256 * 1024;
/// Lines of context around each change in a diff.
const DIFF_CONTEXT: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Added,
    Changed,
    Removed,
}

impl Change {
    fn label(self) -> &'static str {
        match self {
            Change::Added => "added",
            Change::Changed => "changed",
            Change::Removed => "removed",
        }
    }
}

/// A file a deploy would add, change or remove.
#[derive(Debug)]
pub struct FileChange {
    /// Relative to `COMPOST_SERVER_HOME`.
    pub path: String,
    pub change: Change,
    /// Holds secrets: its contents are never shown.
    pub secret: bool,
    pub old: Option<String>,
    pub new: Option<String>,
}

impl FileChange {
    pub fn removed(path: String, old: Option<String>) -> Self {
        let secret = secret_path(&path);
        Self {
            path,
            change: Change::Removed,
            secret,
            old,
            new: None,
        }
    }
}

/// A path under a `secrets` folder holds secrets, whatever its mode.
fn secret_path(path: &str) -> bool {
    path.split('/').any(|part| part == "secrets")
}

/// Files written through a [`crate::connector::dry_run::DryRun`], relative to
/// the server home. Writes outside it are left out.
pub fn captured_files(ctx: &ServerCtx, writes: BTreeMap<String, Captured>) -> Vec<RenderedFile> {
    let home = format!("{}/", ctx.home);
    writes
        .into_iter()
        .filter_map(|(path, captured)| {
            Some(RenderedFile {
                path: path.strip_prefix(&home)?.to_string(),
                contents: captured.contents,
                mode: captured.mode,
            })
        })
        .collect()
}

/// Compare files with the server; unchanged ones are left out. `ctx` must
/// read from the server, not from a dry run.
pub fn compare(ctx: &ServerCtx, files: &[RenderedFile]) -> Result<Vec<FileChange>> {
    let mut changes = Vec::new();
    for file in files {
        let old = ctx.read(&file.path)?;
        let change = match &old {
            None => Change::Added,
            Some(old) if *old == file.contents => continue,
            Some(_) => Change::Changed,
        };
        changes.push(FileChange {
            path: file.path.clone(),
            change,
            secret: file.is_secret() || secret_path(&file.path),
            old,
            new: Some(file.contents.clone()),
        });
    }
    Ok(changes)
}

/// One line per file, e.g. `      changed  apps/myapp/staging/compost.yaml`,
/// and with `diff` the changed lines of files without secrets.
pub fn print_files(indent: &str, changes: &[FileChange], diff: bool) {
    for change in changes {
        let secret = if change.secret {
            "  (secret, not shown)"
        } else {
            ""
        };
        out!(
            "{indent}{:<8} {}{secret}",
            change.change.label(),
            change.path
        );
        if diff && !change.secret {
            print_diff(
                &format!("{indent}  "),
                &change.path,
                change.old.as_deref().unwrap_or_default(),
                change.new.as_deref().unwrap_or_default(),
            );
        }
    }
}

/// A unified diff of a text file, indented.
pub fn print_diff(indent: &str, path: &str, old: &str, new: &str) {
    if old.len() + new.len() > MAX_DIFF_BYTES || old.contains('\0') || new.contains('\0') {
        out!("{indent}(binary or large file, not shown)");
        return;
    }
    let diff = similar::TextDiff::from_lines(old, new);
    let text = diff
        .unified_diff()
        .context_radius(DIFF_CONTEXT)
        .header(&format!("server/{path}"), &format!("new/{path}"))
        .to_string();
    for line in text.lines() {
        out!("{indent}{line}");
    }
}

/// What `docker compose up` does with a service, judged from its definition.
#[derive(Debug, PartialEq, Eq)]
pub enum ServiceChange {
    Created,
    Removed,
    /// What differs, e.g. `image` or `environment: DATABASE_URL`.
    Changed(Vec<String>),
    Unchanged,
    /// In these profiles, which compost never turns on, so `up` doesn't start
    /// it. `running`: it wasn't in one before, so its container keeps running.
    NotStarted {
        profiles: Vec<String>,
        running: bool,
    },
}

/// Compare every service across compose files that apply in order (the
/// user's files, then compost's override). Only names are reported, never
/// values, since overrides hold passwords. Unparsable files count as empty.
pub fn service_changes(old: &[String], new: &[String]) -> BTreeMap<String, ServiceChange> {
    let old = services(old);
    let new = services(new);
    let names: BTreeSet<&String> = old.keys().chain(new.keys()).collect();
    names
        .into_iter()
        .filter_map(|name| {
            let old = old.get(name).filter(|s| profiles(s).is_empty());
            let new = new.get(name);
            let change = match (old, new) {
                (_, Some(new)) if !profiles(new).is_empty() => ServiceChange::NotStarted {
                    profiles: profiles(new),
                    running: old.is_some(),
                },
                // Only in a profile before: there is nothing to remove.
                (None, None) => return None,
                (None, _) => ServiceChange::Created,
                (_, None) => ServiceChange::Removed,
                (Some(old), Some(new)) => {
                    let details = differences(old, new);
                    if details.is_empty() {
                        ServiceChange::Unchanged
                    } else {
                        ServiceChange::Changed(details)
                    }
                }
            };
            Some((name.clone(), change))
        })
        .collect()
}

/// A service's profiles: compose joins those of every file, and `!reset` or
/// `!override` replaces the earlier ones.
fn profiles(sections: &[Value]) -> Vec<String> {
    let names = |value: &Value| -> Vec<String> {
        value
            .as_sequence()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect()
    };
    let mut profiles: Vec<String> = Vec::new();
    for section in sections {
        match section.get("profiles") {
            Some(Value::Tagged(tagged)) if tagged.tag == "reset" || tagged.tag == "override" => {
                profiles = names(&tagged.value);
            }
            Some(value) => {
                for name in names(value) {
                    if !profiles.contains(&name) {
                        profiles.push(name);
                    }
                }
            }
            None => {}
        }
    }
    profiles
}

/// Each service's sections, one per file that has it, in file order.
fn services(texts: &[String]) -> BTreeMap<String, Vec<Value>> {
    let mut services: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for text in texts {
        let Ok(value) = serde_yaml_ng::from_str::<Value>(text) else {
            continue;
        };
        let Some(map) = value.get("services").and_then(Value::as_mapping) else {
            continue;
        };
        for (name, section) in map {
            if let Some(name) = name.as_str() {
                services
                    .entry(name.to_string())
                    .or_default()
                    .push(section.clone());
            }
        }
    }
    services
}

/// Keys whose values differ; for `environment` the variables that differ.
fn differences(old: &[Value], new: &[Value]) -> Vec<String> {
    let keys: BTreeSet<String> = old
        .iter()
        .chain(new)
        .filter_map(Value::as_mapping)
        .flat_map(|map| map.keys().filter_map(|k| k.as_str().map(str::to_string)))
        .collect();
    let mut details = Vec::new();
    for key in keys {
        let values = |sections: &[Value]| -> Vec<Value> {
            sections
                .iter()
                .filter_map(|section| section.get(key.as_str()).cloned())
                .collect()
        };
        let (old_values, new_values) = (values(old), values(new));
        if old_values == new_values {
            continue;
        }
        if key == "environment" {
            let (old_env, new_env) = (environment(&old_values), environment(&new_values));
            let names: BTreeSet<&String> = old_env.keys().chain(new_env.keys()).collect();
            let changed: Vec<&str> = names
                .into_iter()
                .filter(|name| old_env.get(*name) != new_env.get(*name))
                .map(String::as_str)
                .collect();
            if !changed.is_empty() {
                details.push(format!("environment: {}", changed.join(", ")));
            }
        } else {
            details.push(key);
        }
    }
    details
}

/// Variables of `environment` sections merged in order; a mapping or a list
/// of `NAME=value`.
fn environment(sections: &[Value]) -> BTreeMap<String, Value> {
    let mut env = BTreeMap::new();
    for section in sections {
        match section {
            Value::Mapping(map) => {
                for (name, value) in map {
                    if let Some(name) = name.as_str() {
                        env.insert(name.to_string(), value.clone());
                    }
                }
            }
            Value::Sequence(items) => {
                for item in items.iter().filter_map(Value::as_str) {
                    let (name, value) = item.split_once('=').unwrap_or((item, ""));
                    env.insert(name.to_string(), Value::String(value.to_string()));
                }
            }
            _ => {}
        }
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::local::Local;
    use tempfile::TempDir;

    fn texts(texts: &[&str]) -> Vec<String> {
        texts.iter().map(|t| (*t).to_string()).collect()
    }

    #[test]
    fn service_changes_name_what_differs() {
        let old = texts(&[
            "services: {web: {image: 'nginx:1'}, api: {image: api}, gone: {image: x}}",
            "services: {api: {environment: {DB_URL: old, MODE: a}, networks: [n]}}",
        ]);
        let new = texts(&[
            "services: {web: {image: 'nginx:2'}, api: {image: api}, worker: {image: w}}",
            "services: {api: {environment: [DB_URL=new, MODE=a], networks: [n]}}",
        ]);
        let changes = service_changes(&old, &new);
        assert_eq!(changes["web"], ServiceChange::Changed(vec!["image".into()]));
        assert_eq!(
            changes["api"],
            ServiceChange::Changed(vec!["environment: DB_URL".into()])
        );
        assert_eq!(changes["worker"], ServiceChange::Created);
        assert_eq!(changes["gone"], ServiceChange::Removed);

        let same = service_changes(&new, &new);
        assert!(same.values().all(|c| *c == ServiceChange::Unchanged));
        // A service that only an override changes counts as changed.
        let more = texts(&[&new[0], &new[1], "services: {web: {networks: [x]}}"]);
        assert_eq!(
            service_changes(&new, &more)["web"],
            ServiceChange::Changed(vec!["networks".into()])
        );
    }

    #[test]
    fn services_in_profiles_are_not_started() {
        let old = texts(&[
            "services: {web: {image: w}, db: {image: p}, mail: {image: m, profiles: [dev]}}",
        ]);
        let new = texts(&[
            "services: {web: {image: w}, db: {image: p}, cache: {image: c, profiles: [dev]}, tool: {image: t, profiles: [dev]}}",
            "services: {db: {profiles: ['off']}, tool: {profiles: !reset []}, cache: {profiles: []}}",
        ]);
        let changes = service_changes(&old, &new);
        assert_eq!(changes["web"], ServiceChange::Unchanged);
        assert_eq!(
            changes["db"],
            ServiceChange::NotStarted {
                profiles: vec!["off".into()],
                running: true
            }
        );
        // An empty list doesn't clear the profiles; `!reset` does.
        assert_eq!(
            changes["cache"],
            ServiceChange::NotStarted {
                profiles: vec!["dev".into()],
                running: false
            }
        );
        assert_eq!(changes["tool"], ServiceChange::Created);
        // `mail` never ran, so dropping it changes nothing.
        assert!(!changes.contains_key("mail"));
        // Leaving its profile starts it.
        let back = texts(&["services: {db: {image: p}}"]);
        assert_eq!(service_changes(&new, &back)["db"], ServiceChange::Created);
    }

    #[test]
    fn compare_leaves_out_unchanged_files_and_marks_secrets() {
        let dir = TempDir::new().unwrap();
        let local = Local { verbose: false };
        let ctx = ServerCtx {
            connector: &local,
            home: dir.path().to_str().unwrap().to_string(),
        };
        std::fs::write(dir.path().join("same.yaml"), "a").unwrap();
        std::fs::write(dir.path().join("old.yaml"), "a").unwrap();
        let files = [
            RenderedFile::new("same.yaml".into(), "a".into()),
            RenderedFile::new("old.yaml".into(), "b".into()),
            RenderedFile::secret("new.env".into(), "P=x".into()),
            RenderedFile::new("x/secrets/s.yaml".into(), "s".into()),
        ];
        let changes = compare(&ctx, &files).unwrap();
        let summary: Vec<(&str, Change, bool)> = changes
            .iter()
            .map(|c| (c.path.as_str(), c.change, c.secret))
            .collect();
        assert_eq!(
            summary,
            [
                ("old.yaml", Change::Changed, false),
                ("new.env", Change::Added, true),
                ("x/secrets/s.yaml", Change::Added, true),
            ]
        );
    }
}
