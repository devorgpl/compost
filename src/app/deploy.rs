//! `compost app deploy` (spec §10.8).

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_yaml_ng::Value;

use super::contribution::Contribution;
use super::instance::{self, Instance, PinnedLink, PinnedLinks};
use super::restore::RestorePlan;
use super::sources;
use crate::config::app::{LoadedApp, env_value, link_name};
use crate::config::server::ServerConfig;
use crate::connector::Connector;
use crate::error::CompostError;
use crate::server::SERVER_CONFIG_FILE;
use crate::server::compose::wait_until_ready;
use crate::server::lock::Lock;
use crate::server::state::{self, Report, Status};
use crate::server::state_file::{DeployStatus, StateFile};
use crate::shared::{self, AppCtx, Kind, Resolution, ServerCtx, SharedAppConfig};

pub struct Options<'a> {
    pub apply: bool,
    pub dry_run: bool,
    pub force_unlock: bool,
    /// Load data from a backup before the services start (`app restore`).
    pub restore: Option<&'a RestorePlan<'a, 'a>>,
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
                "the server has no shared applications yet; run `compost server deploy --apply` first",
            ));
        }
        status => {
            let detail = report.detail.clone().unwrap_or_default();
            return Err(not_ready(&format!("server is {status}: {detail}")));
        }
    }
    let ctx = ServerCtx {
        connector,
        home: report
            .home
            .clone()
            .context("server home was not resolved")?,
    };
    let mut state_file = StateFile::load(&ctx)?;
    let links = resolve_links(&ctx, loaded, &report, &state_file)?;
    let files = sources::collect(&loaded.base_dir, &app.deployment, config_files)?;

    let project = app.instance();
    if options.dry_run {
        out!("Dry run: nothing is changed on the server.");
        out!("  instance {project}: {} files to upload", files.len());
        for link in &links {
            out!(
                "  {}: uses {} {}",
                link.path(),
                link.shared.type_name,
                link.shared.name
            );
        }
        return Ok(());
    }

    // 2. Upload and render.
    let _lock = Lock::acquire(connector, &ctx.home, options.force_unlock)?;
    let dir = instance::dir(&app.name, &app.prefix);
    let archive = sources::pack(&loaded.base_dir, &files)?;
    connector.replace_dir(&ctx.path(&format!("{dir}/sources")), &archive)?;
    out!("  uploaded {} files", files.len());

    let mut contribution = Contribution::default();
    for service in &app.app_services {
        for (key, value) in &service.environment {
            let value = env_value(value).context("environment values are checked on load")?;
            contribution.set_env(&service.service, key, value)?;
        }
    }
    for link in &links {
        let link_ctx = AppCtx {
            server: &ctx,
            app,
            shared: &link.shared,
        };
        // Fails on server-side conflicts, e.g. a domain used by another instance.
        link.kind
            .prepare_application(&link_ctx, link.entry, &mut contribution)
            .map_err(|e| not_ready(&format!("{}: {e:#}", link.path())))?;
    }
    let override_yaml = contribution.override_yaml(&loaded.compose)?;
    let write = |file: &str, text: &str, mode| {
        connector.write_file(&ctx.path(&format!("{dir}/{file}")), text.as_bytes(), mode)
    };
    write(instance::OVERRIDE_FILE, &override_yaml, 0o600)?;
    let stored_config = app.without_secrets()?;
    write(
        instance::CONFIG_FILE,
        &serde_yaml_ng::to_string(&stored_config)?,
        0o644,
    )?;
    let pinned = PinnedLinks {
        links: links
            .iter()
            .map(|link| PinnedLink {
                kind: link.kind.name().to_string(),
                entry: link.name.to_string(),
                instance: link.shared.name.clone(),
                type_name: link.shared.type_name.clone(),
                version: link.shared.version,
            })
            .collect(),
    };
    write(
        instance::LINKS_FILE,
        &serde_yaml_ng::to_string(&pinned)?,
        0o644,
    )?;

    state_file.set_app(&app.name, &app.prefix, DeployStatus::Candidate);
    state_file.save(&ctx)?;
    if !options.apply {
        out!("Files are on the server. Run again with --apply to start {project}.");
        return Ok(());
    }

    // 3. Apply. On failure the instance is marked `failed`.
    match apply(&ctx, loaded, &links, options.restore) {
        Ok(()) => {
            state_file.set_app(&app.name, &app.prefix, DeployStatus::Deployed);
            state_file.save(&ctx)?;
            out!();
            super::state::print(
                &ctx,
                &Instance {
                    app: stored_config,
                    links: pinned.links,
                },
            )?;
            Ok(())
        }
        Err(err) => {
            state_file.set_app(&app.name, &app.prefix, DeployStatus::Failed);
            state_file.save(&ctx)?;
            Err(not_ready(&format!("{project} failed: {err:#}")))
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
        .context("the server has no compost-server.yaml; run `compost server deploy` first")?;
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
            Resolution::Use(shared) => links.push(Link {
                kind,
                name,
                entry,
                shared: shared.clone(),
            }),
            Resolution::Skip(reason) => out!("  {path}: skipped ({reason})"),
        }
    }
    links.sort_by_key(|link| link.kind.apply_order());
    Ok(links)
}

/// Run hooks and docker compose. A restore loads its data after the links
/// are ready and before the services start.
fn apply(
    ctx: &ServerCtx,
    loaded: &LoadedApp,
    links: &[Link],
    restore: Option<&RestorePlan>,
) -> Result<()> {
    let app = &loaded.config;
    for link in links {
        let link_ctx = AppCtx {
            server: ctx,
            app,
            shared: &link.shared,
        };
        link.kind
            .before_apply_application(&link_ctx, link.entry)
            .with_context(|| link.path())?;
        if let Some(plan) = restore {
            plan.restore_link(&link_ctx, link.kind, link.name)
                .with_context(|| link.path())?;
        }
    }

    let project = app.instance();
    let compose = instance::compose_command(ctx, app);
    let images_restored = match restore {
        Some(plan) => plan.load_images(ctx, app)?,
        None => false,
    };
    // With restored images, `up` still builds or pulls the ones that are missing.
    if !images_restored {
        if app.deployment.build {
            out!("  building images ...");
            ctx.connector.exec(&format!("{compose} build -q"))?;
        } else {
            ctx.connector
                .exec(&format!("{compose} pull -q --ignore-buildable"))?;
        }
    }
    if let Some(plan) = restore {
        plan.restore_volumes(ctx, app, &loaded.compose, true)?;
    }
    out!("  starting {project} ...");
    ctx.connector
        .exec(&format!("{compose} up -d --remove-orphans"))?;
    wait_until_ready(ctx.connector, &project)?;

    for link in links {
        let link_ctx = AppCtx {
            server: ctx,
            app,
            shared: &link.shared,
        };
        link.kind
            .after_apply_application(&link_ctx, link.entry)
            .with_context(|| link.path())?;
    }
    Ok(())
}

fn not_ready(message: &str) -> anyhow::Error {
    CompostError::ServerNotReady(message.to_string()).into()
}
