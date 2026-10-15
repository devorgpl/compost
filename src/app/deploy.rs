//! `depap app deploy` (spec §10.8).

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Serialize;
use serde_yaml_ng::Value;

use super::contribution::Contribution;
use super::sources;
use crate::config::app::{LoadedApp, env_value, link_name};
use crate::config::server::ServerConfig;
use crate::connector::{Connector, shell_quote};
use crate::error::DepapError;
use crate::server::compose::wait_until_ready;
use crate::server::lock::Lock;
use crate::server::state::{self, Report, Status};
use crate::server::state_file::{DeployStatus, StateFile};
use crate::server::SERVER_CONFIG_FILE;
use crate::shared::{self, AppCtx, Kind, Resolution, ServerCtx, SharedAppConfig};

const OVERRIDE_FILE: &str = "compose/depap.override.yaml";

pub struct Options {
    pub apply: bool,
    pub dry_run: bool,
    pub force_unlock: bool,
}

/// A link of the app, resolved to a shared app on the server.
struct Link<'a> {
    kind: &'static dyn Kind,
    name: &'a str,
    entry: &'a Value,
    shared: SharedAppConfig,
}

impl Link<'_> {
    /// e.g. `services.database[system-data]`, for error messages.
    fn path(&self) -> String {
        format!("services.{}[{}]", self.kind.name(), self.name)
    }
}

/// `depap-env.yaml`: which shared apps this instance uses.
#[derive(Serialize)]
struct PinnedLink<'a> {
    kind: &'a str,
    entry: &'a str,
    instance: &'a str,
    #[serde(rename = "type")]
    type_name: &'a str,
    version: Option<u32>,
}

pub fn deploy(
    connector: &dyn Connector,
    server_home: &str,
    manager_key: &str,
    loaded: &LoadedApp,
    config_files: &[PathBuf],
    options: &Options,
) -> Result<()> {
    let app = &loaded.config;

    // 1. Preflight: the server must already run shared apps.
    let report = state::check(connector, server_home, manager_key)?;
    match report.status {
        Status::Ok | Status::Degraded => {}
        Status::Uninitialized => {
            return Err(not_ready(
                "the server has no shared applications yet; run `depap server deploy --apply` first",
            ));
        }
        status => {
            let detail = report.detail.clone().unwrap_or_default();
            return Err(not_ready(&format!("server is {status}: {detail}")));
        }
    }
    let ctx = ServerCtx {
        connector,
        home: report.home.clone().context("server home was not resolved")?,
    };
    let mut state_file = StateFile::load(&ctx)?;
    let links = resolve_links(&ctx, loaded, &report, &state_file)?;
    let files = sources::collect(&loaded.base_dir, &app.deployment, config_files)?;

    let instance = app.instance();
    if options.dry_run {
        println!("Dry run: nothing is changed on the server.");
        println!("  instance {instance}: {} files to upload", files.len());
        for link in &links {
            println!("  {}: uses {} {}", link.path(), link.shared.type_name, link.shared.name);
        }
        return Ok(());
    }

    // 2. Upload and render.
    let _lock = Lock::acquire(connector, &ctx.home, options.force_unlock)?;
    let dir = format!("apps/{}/{}", app.name, app.prefix);
    let archive = sources::pack(&loaded.base_dir, &files)?;
    connector.replace_dir(&ctx.path(&format!("{dir}/sources")), &archive)?;
    println!("  uploaded {} files", files.len());

    let mut contribution = Contribution::default();
    for service in &app.app_services {
        for (key, value) in &service.environment {
            let value = env_value(value).context("environment values are checked on load")?;
            contribution.set_env(&service.service, key, value)?;
        }
    }
    for link in &links {
        let link_ctx = AppCtx { server: &ctx, app, shared: &link.shared };
        // Fails on server-side conflicts, e.g. a domain used by another instance.
        link.kind
            .prepare_application(&link_ctx, link.entry, &mut contribution)
            .map_err(|e| not_ready(&format!("{}: {e:#}", link.path())))?;
    }
    let override_yaml = contribution.override_yaml(&loaded.compose)?;
    connector.write_file(&ctx.path(&format!("{dir}/{OVERRIDE_FILE}")), override_yaml.as_bytes(), 0o600)?;
    let effective = serde_yaml_ng::to_string(&app.without_secrets()?)?;
    connector.write_file(&ctx.path(&format!("{dir}/depap.yaml")), effective.as_bytes(), 0o644)?;
    let pinned: Vec<PinnedLink> = links
        .iter()
        .map(|link| PinnedLink {
            kind: link.kind.name(),
            entry: link.name,
            instance: &link.shared.name,
            type_name: &link.shared.type_name,
            version: link.shared.version,
        })
        .collect();
    let pinned = serde_yaml_ng::to_string(&BTreeMap::from([("links", pinned)]))?;
    connector.write_file(&ctx.path(&format!("{dir}/depap-env.yaml")), pinned.as_bytes(), 0o644)?;

    state_file.set_app(&app.name, &app.prefix, DeployStatus::Candidate);
    state_file.save(&ctx)?;
    if !options.apply {
        println!("Files are on the server. Run again with --apply to start {instance}.");
        return Ok(());
    }

    // 3. Apply. On failure the instance is marked `failed`.
    match apply(&ctx, loaded, &links, &dir) {
        Ok(services) => {
            state_file.set_app(&app.name, &app.prefix, DeployStatus::Deployed);
            state_file.save(&ctx)?;
            println!("{instance} is deployed:");
            for line in services.lines() {
                println!("  {line}");
            }
            Ok(())
        }
        Err(err) => {
            state_file.set_app(&app.name, &app.prefix, DeployStatus::Failed);
            state_file.save(&ctx)?;
            Err(not_ready(&format!("{instance} failed: {err:#}")))
        }
    }
}

/// Pick a shared app for every link; links skipped by their kind are left out.
fn resolve_links<'a>(
    ctx: &ServerCtx,
    loaded: &'a LoadedApp,
    report: &Report,
    state_file: &StateFile,
) -> Result<Vec<Link<'a>>> {
    let text = ctx
        .read(SERVER_CONFIG_FILE)?
        .context("the server has no depap-server.yaml; run `depap server deploy` first")?;
    let server_config = ServerConfig::parse(&text)?;

    let mut links = Vec::new();
    for (kind_name, entry) in loaded.config.links() {
        let kind = shared::find_kind(kind_name).context("kinds are checked on load")?;
        let name = link_name(entry).context("link names are checked on load")?;
        // Only shared apps that are deployed and running can be linked.
        let available: Vec<SharedAppConfig> = server_config
            .shared
            .get(kind_name)
            .into_iter()
            .flatten()
            .filter(|app| {
                let deployed = state_file
                    .shared(kind_name, &app.name)
                    .is_some_and(|r| r.status == DeployStatus::Deployed);
                let running = report
                    .shared
                    .iter()
                    .any(|s| s.kind == kind_name && s.name == app.name && s.running);
                deployed && running
            })
            .cloned()
            .collect();

        let path = format!("services.{kind_name}[{name}]");
        let resolution = kind
            .resolve(entry, &available)
            .map_err(|e| not_ready(&format!("{path}: {e:#}")))?;
        match resolution {
            Resolution::Use(shared) => links.push(Link { kind, name, entry, shared: shared.clone() }),
            Resolution::Skip(reason) => println!("  {path}: skipped ({reason})"),
        }
    }
    links.sort_by_key(|link| link.kind.apply_order());
    Ok(links)
}

/// Run hooks and docker compose; returns the final `docker compose ps` output.
fn apply(ctx: &ServerCtx, loaded: &LoadedApp, links: &[Link], dir: &str) -> Result<String> {
    let app = &loaded.config;
    for link in links {
        let link_ctx = AppCtx { server: ctx, app, shared: &link.shared };
        link.kind
            .before_apply_application(&link_ctx, link.entry)
            .with_context(|| link.path())?;
    }

    let project = app.instance();
    let compose = format!(
        "cd {} && docker compose -p {} --project-directory sources -f {} -f {OVERRIDE_FILE}",
        shell_quote(&ctx.path(dir)),
        shell_quote(&project),
        shell_quote(&format!("sources/{}", app.deployment.compose_file)),
    );
    if app.deployment.build {
        println!("  building images ...");
        ctx.connector.exec(&format!("{compose} build -q"))?;
    } else {
        ctx.connector.exec(&format!("{compose} pull -q --ignore-buildable"))?;
    }
    println!("  starting {project} ...");
    ctx.connector.exec(&format!("{compose} up -d --remove-orphans"))?;
    let services = wait_until_ready(ctx.connector, &project)?;

    for link in links {
        let link_ctx = AppCtx { server: ctx, app, shared: &link.shared };
        link.kind
            .after_apply_application(&link_ctx, link.entry)
            .with_context(|| link.path())?;
    }
    Ok(services)
}

fn not_ready(message: &str) -> anyhow::Error {
    DepapError::ServerNotReady(message.to_string()).into()
}
