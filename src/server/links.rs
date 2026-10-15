//! `compost connector list` (spec §10.11): shared apps and the app links to them.

use anyhow::{Context, Result};
use serde::Deserialize;

use super::state::{self as server_state, Report, Status};
use super::state_file::StateFile;
use crate::app::instance;
use crate::connector::Connector;
use crate::error::CompostError;
use crate::shared::{self, AppCtx, ServerCtx, SharedAppConfig, connectors_dir};

/// Fields every link record starts with, whatever its kind.
#[derive(Debug, Deserialize)]
struct LinkRecord {
    app: String,
    prefix: String,
    entry: String,
}

pub fn list(connector: &dyn Connector, server_home: &str, manager_key: &str) -> Result<()> {
    let report = server_state::check(connector, server_home, manager_key)?;
    let home = match (report.status, &report.home) {
        (Status::Ok | Status::Degraded, Some(home)) => home.clone(),
        (Status::Uninitialized, _) => {
            out!("No shared applications: compost has not deployed anything on this server.");
            return Ok(());
        }
        (status, _) => {
            return Err(CompostError::ServerNotReady(format!(
                "server is {status}: {}",
                report.detail.clone().unwrap_or_default()
            ))
            .into());
        }
    };
    let ctx = ServerCtx { connector, home };
    let server_config = instance::server_config(&ctx)?;
    let state_file = StateFile::load(&ctx)?;

    for (kind_name, apps) in &server_config.shared {
        out!("{kind_name}");
        for (i, shared_app) in apps.iter().enumerate() {
            let last = i + 1 == apps.len();
            let (branch, indent) = if last {
                ("└─", "   ")
            } else {
                ("├─", "│  ")
            };
            let version = shared_app
                .version
                .map_or_else(|| "-".to_string(), |v| v.to_string());
            out!(
                "{branch} {}  {} {version}  {}",
                shared_app.name,
                shared_app.type_name,
                running(&report, kind_name, &shared_app.name)
            );

            let lines = link_lines(&ctx, &state_file, kind_name, shared_app)?;
            if lines.is_empty() {
                out!("{indent}└─ no links");
            }
            for (j, line) in lines.iter().enumerate() {
                let branch = if j + 1 == lines.len() {
                    "└─"
                } else {
                    "├─"
                };
                out!("{indent}{branch} {line}");
            }
        }
    }
    Ok(())
}

fn running(report: &Report, kind: &str, name: &str) -> &'static str {
    match report
        .shared
        .iter()
        .find(|s| s.kind == kind && s.name == name)
    {
        Some(state) if state.running => "running",
        _ => "not running",
    }
}

/// One line per link record of a shared app, e.g.
/// `myapp-staging/system-data  database mydb, user myuser  app: deployed`.
fn link_lines(
    ctx: &ServerCtx,
    state_file: &StateFile,
    kind_name: &str,
    shared_app: &SharedAppConfig,
) -> Result<Vec<String>> {
    let kind = shared::find_kind(kind_name).context("kinds are checked on load")?;
    let dir = ctx.path(&connectors_dir(kind_name, &shared_app.name));
    let mut lines = Vec::new();
    for file in ctx.connector.list_dir(&dir)? {
        // `root-*` files describe the shared app itself, not a link.
        if file.starts_with("root-") || !file.ends_with(".yaml") {
            continue;
        }
        let path = format!("{dir}/{file}");
        let Some(text) = ctx.connector.read_file(&path)? else {
            continue;
        };
        let record: LinkRecord = serde_yaml_ng::from_str(&text)
            .with_context(|| format!("invalid link record {path}"))?;
        let name = format!("{}-{}/{}", record.app, record.prefix, record.entry);

        let line = match instance::load(ctx, &record.app, &record.prefix)? {
            Some(instance) => {
                let link_ctx = AppCtx {
                    server: ctx,
                    app: &instance.app,
                    shared: shared_app,
                };
                let summary = kind
                    .describe_link(&link_ctx, &record.entry)
                    .unwrap_or_else(|err| format!("cannot read link: {err:#}"));
                let status = state_file
                    .app(&record.app, &record.prefix)
                    .map_or_else(|| "unknown".to_string(), |r| r.status.to_string());
                format!("{name}  {summary}  app: {status}")
            }
            None => format!("{name}  app not deployed (leftover record {file})"),
        };
        lines.push(line);
    }
    lines.sort();
    Ok(lines)
}
