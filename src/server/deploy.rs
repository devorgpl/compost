//! `compost server deploy` (spec §10.7).

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use super::compose::wait_until_ready_within;
use super::images;
use super::lock::Lock;
use super::plan::{self as server_plan, FileChange};
use super::secrets;
use super::state::{self, Status};
use super::state_file::{DeployStatus, StateFile};
use super::{MANAGER_KEY_FILE, SERVER_CONFIG_FILE};
use crate::app::contribution::Contribution;
use crate::config::app::ComposeFile;
use crate::config::server::{Consumer, ConsumerLink, ServerConfig};
use crate::connector::dry_run::DryRun;
use crate::connector::{Connector, shell_quote};
use crate::error::{CompostError, config_err};
use crate::shared::{self, AppCtx, Kind, Rendered, RenderedFile, ServerCtx, SharedAppConfig};

/// Written next to `compose.yaml` of a shared app with links; docker compose
/// reads it automatically.
pub const SHARED_OVERRIDE_FILE: &str = "compose.override.yaml";

pub struct Options<'a> {
    pub apply: bool,
    pub dry_run: bool,
    /// With `dry_run`: also print the changed lines of files without secrets.
    pub diff: bool,
    pub force_unlock: bool,
    /// Pull images before starting, even when their tags did not change.
    pub pull: bool,
    /// Recreate every container, even unchanged ones.
    pub recreate: bool,
    /// Overlay stored as the shared apps' secrets (`--secret`).
    pub secrets: Option<&'a str>,
    /// Only these shared apps (`--shared`); all when empty. The others are left
    /// as they were deployed, and so is their part of the stored config.
    pub shared: &'a [String],
}

/// One shared app, rendered and ready to be written to the server.
struct Plan<'a> {
    kind: &'static dyn Kind,
    app: &'a SharedAppConfig,
    rendered: Rendered,
    hash: String,
    /// Rendered files differ from the last deploy (or it is new).
    changed: bool,
    /// It was deployed and running after the last deploy.
    was_deployed: bool,
    /// Its compose files changed, so a reload is not enough. Known once the
    /// files are written.
    compose_changed: bool,
    /// Links to other shared apps, if it has any.
    consumer: Option<Consumer>,
    /// Deployed by this run (`--shared`, else every one).
    selected: bool,
}

impl Plan<'_> {
    fn kind_name(&self) -> &'static str {
        self.kind.name()
    }

    /// Set `changed` and `was_deployed` from the record of the last deploy.
    fn compare(&mut self, state_file: &StateFile) {
        let record = state_file.shared(self.kind_name(), &self.app.name);
        self.changed = record.is_none_or(|record| record.config_hash != self.hash);
        self.was_deployed = record.is_some_and(|record| record.status == DeployStatus::Deployed);
    }
}

pub fn deploy(
    connector: &dyn Connector,
    server_home: &str,
    manager_key: &str,
    config: &ServerConfig,
    options: &Options,
) -> Result<()> {
    // 1. Preflight.
    let report = state::check(connector, server_home, manager_key)?;
    if matches!(
        report.status,
        Status::Unreachable | Status::NoDocker | Status::Foreign
    ) {
        return Err(CompostError::ServerNotReady(format!(
            "server is {}: {}",
            report.status,
            report.detail.unwrap_or_default()
        ))
        .into());
    }
    let ctx = ServerCtx {
        connector,
        home: report.home.context("server home was not resolved")?,
    };

    // 2. Render everything before changing anything on the server.
    let mut plans = render_all(&ctx, config, &StateFile::load(&ctx)?)?;
    check_command_services(config, &plans)?;
    select(config, &mut plans, options.shared)?;

    if options.dry_run {
        return dry_run(&ctx, report.status, &plans, config, options);
    }

    // 3. Initialize and lock.
    let initialize = report.status == Status::Uninitialized;
    if initialize && !options.shared.is_empty() {
        return Err(no_deployed_config());
    }
    if initialize {
        connector.exec(&format!("mkdir -p -- {}", shell_quote(&ctx.home)))?;
    }
    let _lock = Lock::acquire(connector, &ctx.home, options.force_unlock)?;
    if initialize {
        let key = format!("{manager_key}\n");
        connector.write_file(&ctx.path(MANAGER_KEY_FILE), key.as_bytes(), 0o644)?;
        out!("Initialized {}", ctx.home);
    }
    // Read under the lock: a command that finished since the preflight may have
    // changed them, and saving an older copy would undo its changes.
    let mut state_file = StateFile::load(&ctx)?;
    for plan in &mut plans {
        plan.compare(&state_file);
    }
    let previous = match ctx.read(SERVER_CONFIG_FILE)? {
        Some(text) => ServerConfig::parse(&text).ok(),
        None => None,
    };
    let effective = stored_config(config, previous.as_ref(), &plans)?;

    // 4. Write networks and files.
    check_existing_networks(connector, &plans)?;
    create_networks(connector, &plans)?;
    for plan in plans.iter_mut().filter(|p| p.selected) {
        let compose_path = compose_path(plan);
        plan.compose_changed = write_files(&ctx, &plan.rendered, &compose_path)?;
        if let Some(consumer) = &plan.consumer {
            let override_changed = prepare_links(&ctx, plan, consumer).map_err(|e| {
                CompostError::ServerNotReady(format!(
                    "{} {}: {e:#}",
                    plan.kind_name(),
                    plan.app.name
                ))
            })?;
            plan.changed |= override_changed;
            plan.compose_changed |= override_changed;
        } else if remove_override(&ctx, plan)? {
            plan.changed = true;
            plan.compose_changed = true;
        }
        if plan.changed {
            state_file.set_shared(
                plan.kind_name(),
                &plan.app.name,
                DeployStatus::Candidate,
                &plan.hash,
            );
        }
        let change = if plan.changed { "updated" } else { "unchanged" };
        out!("  {:<10} {:<16} {change}", plan.kind_name(), plan.app.name);
    }
    // A deploy of some shared apps leaves the others alone, removed ones too.
    let orphans = if options.shared.is_empty() {
        orphans(&state_file, &plans, previous.as_ref())
    } else {
        Vec::new()
    };
    for (kind, name, was_external) in orphans {
        if was_external && forget_external(&ctx, &kind, &name)? {
            state_file.remove_shared(&kind, &name);
            out!("  {kind:<10} {name:<16} removed: external, compost only forgets it");
        } else if was_external {
            out!(
                "  {kind:<10} {name:<16} orphaned: apps still link to it; deploy them without it first"
            );
        } else {
            out!(
                "  {kind:<10} {name:<16} orphaned: not in the configuration any more, left as it is"
            );
        }
    }
    connector.write_file(&ctx.path(SERVER_CONFIG_FILE), effective.as_bytes(), 0o644)?;
    if let Some(text) = options.secrets {
        let path = secrets::store(&ctx, text)?;
        out!("  stored secrets in {path}");
    }
    state_file.save(&ctx)?;

    if !options.apply {
        out!("Files are on the server. Run again with --apply to start the services.");
        return Ok(());
    }

    // 5. Undo links of shared apps that the config dropped, e.g. a removed
    //    `monitoring`. Then start: `docker compose up -d` leaves unchanged
    //    containers alone; containers whose files changed are recreated, because
    //    compose cannot see those changes, unless the shared app can reload
    //    changed configuration files itself.
    if let Some(previous) = &previous {
        remove_dropped_links(&ctx, previous, &plans)?;
    }
    for plan in plans.iter().filter(|p| p.selected) {
        let kind = plan.kind_name();
        let deployed = state_file
            .shared(kind, &plan.app.name)
            .is_some_and(|r| r.status == DeployStatus::Deployed);
        let recreate = options.recreate || (!deployed && !reloaded(&ctx, plan)?);

        match apply(&ctx, plan, options.pull, recreate) {
            Ok(()) => {
                state_file.set_shared(kind, &plan.app.name, DeployStatus::Deployed, &plan.hash);
                // For `server images`; a deploy does not fail for it.
                if !plan.app.is_external() && plan.kind.runs_service(plan.app) {
                    let project = shared::project_name(kind, &plan.app.name);
                    if let Ok(running) = images::running(connector, &project) {
                        state_file.set_images(kind, &plan.app.name, &running);
                    }
                }
            }
            Err(err) => {
                state_file.set_shared(kind, &plan.app.name, DeployStatus::Failed, &plan.hash);
                state_file.save(&ctx)?;
                return Err(CompostError::ServerNotReady(format!(
                    "{kind} {} did not start: {err:#}",
                    plan.app.name
                ))
                .into());
            }
        }
    }
    state_file.save(&ctx)
}

fn render_all<'a>(
    ctx: &ServerCtx,
    config: &'a ServerConfig,
    state_file: &StateFile,
) -> Result<Vec<Plan<'a>>> {
    let mut plans = Vec::new();
    // Shared apps with links start after the shared apps they use.
    for (kind_name, app) in config.start_order()? {
        let kind =
            shared::find_kind(kind_name).with_context(|| format!("unknown kind `{kind_name}`"))?;
        let rendered = kind
            .render(ctx, app)
            .with_context(|| format!("shared.{kind_name}[{}]", app.name))?;
        let mut plan = Plan {
            kind,
            app,
            hash: hash_rendered(&rendered),
            rendered,
            changed: true,
            was_deployed: false,
            compose_changed: true,
            consumer: config.consumer(kind_name, app)?,
            selected: true,
        };
        plan.compare(state_file);
        plans.push(plan);
    }
    Ok(plans)
}

/// The `service` of each command of `commands` is one the shared app's
/// rendered compose file has.
fn check_command_services(config: &ServerConfig, plans: &[Plan]) -> Result<()> {
    for (name, command) in &config.commands {
        let Some(shared_app) = &command.shared_app else {
            continue;
        };
        let (kind, app) = config.find_shared(shared_app)?;
        let Some(plan) = plans
            .iter()
            .find(|p| p.kind_name() == kind && p.app.name == app.name)
        else {
            continue;
        };
        let path = compose_path(plan);
        let compose = plan
            .rendered
            .files
            .iter()
            .find(|f| f.path == path)
            .map(|f| f.contents.as_str())
            .unwrap_or_default();
        super::exec::compose_service(kind, &app.name, compose, command.service.as_deref())
            .map_err(|e| config_err(format!("commands.{name}: {e}")))?;
    }
    Ok(())
}

/// `sha256:<hex>` over everything a shared app puts on the server.
fn hash_rendered(rendered: &Rendered) -> String {
    let mut hasher = Sha256::new();
    for file in &rendered.files {
        for part in [
            file.path.as_str(),
            file.contents.as_str(),
            &format!("{:o}", file.mode),
        ] {
            hasher.update(part.as_bytes());
            hasher.update([0u8]);
        }
    }
    let lists = [
        &rendered.dirs,
        &rendered.networks,
        &rendered.existing_networks,
        &rendered.owned_dirs,
    ];
    for part in lists.into_iter().flatten() {
        hasher.update(part.as_bytes());
        hasher.update([0u8]);
    }
    let hex: String = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("sha256:{hex}")
}

/// Mark the shared apps named with `--shared` as the ones to deploy; with none
/// named, all are.
fn select(config: &ServerConfig, plans: &mut [Plan], names: &[String]) -> Result<()> {
    if names.is_empty() {
        return Ok(());
    }
    let mut wanted = Vec::new();
    for name in names {
        let (kind, app) = config
            .find_shared(name)
            .map_err(|e| config_err(format!("--shared {name}: {e}")))?;
        wanted.push((kind, app.name.as_str()));
    }
    for plan in plans {
        plan.selected = wanted
            .iter()
            .any(|(kind, name)| *kind == plan.kind_name() && *name == plan.app.name);
    }
    Ok(())
}

fn no_deployed_config() -> anyhow::Error {
    config_err(
        "--shared: the server has no deployed compost-server.yaml yet; deploy all shared apps first",
    )
}

/// The config stored on the server, without secrets. With `--shared`, it is
/// the previously deployed one with the selected shared apps replaced, and
/// every shared app they link to must be in it.
fn stored_config(
    config: &ServerConfig,
    previous: Option<&ServerConfig>,
    plans: &[Plan],
) -> Result<String> {
    let config = config.without_secrets()?;
    if plans.iter().all(|p| p.selected) {
        return Ok(serde_yaml_ng::to_string(&config)?);
    }
    let mut stored = previous.ok_or_else(no_deployed_config)?.clone();
    for plan in plans.iter().filter(|p| p.selected) {
        let kind = plan.kind_name();
        let app = config
            .shared
            .get(kind)
            .and_then(|apps| apps.iter().find(|a| a.name == plan.app.name))
            .context("selected shared apps are in the config")?
            .clone();
        let apps = stored.shared.entry(kind.to_string()).or_default();
        match apps.iter_mut().find(|a| a.name == app.name) {
            Some(old) => *old = app,
            None => apps.push(app),
        }
    }
    for plan in plans.iter().filter(|p| p.selected) {
        for link in plan.consumer.iter().flat_map(|c| &c.links) {
            let deployed = stored
                .shared
                .get(link.kind.name())
                .is_some_and(|apps| apps.iter().any(|a| a.name == link.shared.name));
            if !deployed {
                return Err(config_err(format!(
                    "--shared: {} {} uses {} {}, which is not deployed; add it to --shared",
                    plan.kind_name(),
                    plan.app.name,
                    link.kind.name(),
                    link.shared.name
                )));
            }
        }
    }
    Ok(serde_yaml_ng::to_string(&stored)?)
}

fn all_networks(plans: &[Plan]) -> Vec<String> {
    let mut networks: Vec<String> = plans
        .iter()
        .filter(|plan| plan.selected)
        .flat_map(|plan| plan.rendered.networks.iter().cloned())
        .collect();
    networks.sort();
    networks.dedup();
    networks
}

fn create_networks(connector: &dyn Connector, plans: &[Plan]) -> Result<()> {
    for network in all_networks(plans) {
        let network = shell_quote(&network);
        connector.exec(&format!(
            "docker network inspect {network} >/dev/null 2>&1 || docker network create {network} >/dev/null"
        ))?;
    }
    Ok(())
}

/// Networks of containers compost does not manage must exist: creating one
/// would take it away from the compose project that should own it.
fn check_existing_networks(connector: &dyn Connector, plans: &[Plan]) -> Result<()> {
    for plan in plans.iter().filter(|p| p.selected) {
        for network in &plan.rendered.existing_networks {
            let exists = connector
                .exec(&format!(
                    "docker network inspect {} >/dev/null 2>&1 && echo yes || true",
                    shell_quote(network)
                ))?
                .trim()
                == "yes";
            if !exists {
                return Err(CompostError::ServerNotReady(format!(
                    "{} {}: network `{network}` does not exist; compost only joins existing networks, it does not create them",
                    plan.kind_name(),
                    plan.app.name
                ))
                .into());
            }
        }
    }
    Ok(())
}

/// Delete the records of an external shared app that was removed from the
/// config, unless apps still link to it. Returns whether it was forgotten.
fn forget_external(ctx: &ServerCtx, kind: &str, name: &str) -> Result<bool> {
    if has_links(ctx, kind, name)? {
        return Ok(false);
    }
    let dir = shared::connectors_dir(kind, name);
    ctx.connector
        .exec(&format!("rm -rf -- {}", shell_quote(&ctx.path(&dir))))?;
    Ok(true)
}

/// Apps (or shared apps) have link records of this shared app.
fn has_links(ctx: &ServerCtx, kind: &str, name: &str) -> Result<bool> {
    let dir = shared::connectors_dir(kind, name);
    Ok(ctx
        .connector
        .list_dir(&ctx.path(&dir))?
        .into_iter()
        .any(|file| !file.starts_with("root-")))
}

/// Shared apps in `state.yaml` that the config no longer has: kind, name and
/// whether the previous config had them as external.
fn orphans(
    state_file: &StateFile,
    plans: &[Plan],
    previous: Option<&ServerConfig>,
) -> Vec<(String, String, bool)> {
    state_file
        .shared
        .iter()
        .filter(|record| {
            !plans
                .iter()
                .any(|p| p.kind_name() == record.kind && p.app.name == record.name)
        })
        .map(|record| {
            let was_external = previous.is_some_and(|config| {
                config.shared.get(&record.kind).is_some_and(|apps| {
                    apps.iter()
                        .any(|a| a.name == record.name && a.is_external())
                })
            });
            (record.kind.clone(), record.name.clone(), was_external)
        })
        .collect()
}

/// `shared/<kind>/<name>/compose.yaml`.
fn compose_path(plan: &Plan) -> String {
    format!(
        "{}/compose.yaml",
        shared::shared_dir(plan.kind_name(), &plan.app.name)
    )
}

/// Write the rendered files and delete files the shared app no longer has in
/// its owned directories. Returns whether the compose file changed.
fn write_files(ctx: &ServerCtx, rendered: &Rendered, compose_path: &str) -> Result<bool> {
    let dirs: Vec<String> = rendered
        .dirs
        .iter()
        .chain(&rendered.owned_dirs)
        .map(|d| shell_quote(&ctx.path(d)))
        .collect();
    if !dirs.is_empty() {
        ctx.connector
            .exec(&format!("mkdir -p -- {}", dirs.join(" ")))?;
    }
    let mut compose_changed = false;
    for file in &rendered.files {
        let path = ctx.path(&file.path);
        if file.path == compose_path {
            compose_changed =
                ctx.connector.read_file(&path)?.as_deref() != Some(file.contents.as_str());
        }
        ctx.connector
            .write_file(&path, file.contents.as_bytes(), file.mode)?;
    }
    for dir in &rendered.owned_dirs {
        let keep: Vec<&str> = rendered
            .files
            .iter()
            .filter_map(|file| file.path.strip_prefix(&format!("{dir}/")))
            .collect();
        for name in ctx.connector.list_dir(&ctx.path(dir))? {
            if !keep.contains(&name.as_str()) {
                let path = shell_quote(&ctx.path(&format!("{dir}/{name}")));
                ctx.connector.exec(&format!("rm -f -- {path}"))?;
            }
        }
    }
    Ok(compose_changed)
}

/// A shared app that ran with the last deploy and whose compose files did not
/// change can take changed configuration files with a reload, without a restart.
fn reloaded(ctx: &ServerCtx, plan: &Plan) -> Result<bool> {
    if !plan.changed || !plan.was_deployed || plan.compose_changed {
        return Ok(false);
    }
    let reloaded = plan.kind.reload_shared(ctx, plan.app).map_err(|e| {
        CompostError::ServerNotReady(format!("{} {}: {e:#}", plan.kind_name(), plan.app.name))
    })?;
    if reloaded {
        out!(
            "  reloaded {} {} without a restart",
            plan.kind_name(),
            plan.app.name
        );
    }
    Ok(reloaded)
}

/// Run the links' `prepare_application` and write the compose override.
/// Returns whether the override changed.
fn prepare_links(ctx: &ServerCtx, plan: &Plan, consumer: &Consumer) -> Result<bool> {
    let peers = consumer.peers();
    let mut contribution = Contribution::default();
    for link in &consumer.links {
        let link_ctx = AppCtx {
            server: ctx,
            app: &consumer.app,
            shared: &link.shared,
            peers: &peers,
        };
        link.kind
            .prepare_application(&link_ctx, &link.entry, &mut contribution)
            .with_context(|| link.path())?;
    }
    let dir = shared::shared_dir(plan.kind_name(), &plan.app.name);
    let compose_path = compose_path(plan);
    let compose = plan
        .rendered
        .files
        .iter()
        .find(|file| file.path == compose_path)
        .with_context(|| format!("{compose_path} was not rendered (this is a bug in compost)"))?;
    let compose = ComposeFile::parse(&compose.contents)?;
    let text = contribution.override_yaml(&compose)?;
    let path = ctx.path(&format!("{dir}/{SHARED_OVERRIDE_FILE}"));
    let changed = ctx.connector.read_file(&path)?.as_deref() != Some(text.as_str());
    ctx.connector.write_file(&path, text.as_bytes(), 0o600)?;
    Ok(changed)
}

/// Delete the compose override of a shared app that has no links any more.
/// Returns whether there was one.
fn remove_override(ctx: &ServerCtx, plan: &Plan) -> Result<bool> {
    let dir = shared::shared_dir(plan.kind_name(), &plan.app.name);
    let path = ctx.path(&format!("{dir}/{SHARED_OVERRIDE_FILE}"));
    if ctx.connector.read_file(&path)?.is_none() {
        return Ok(false);
    }
    ctx.connector
        .exec(&format!("rm -f -- {}", shell_quote(&path)))?;
    Ok(true)
}

/// Remove the links that shared apps had in the previously deployed config
/// and no longer have, or that now use another shared app.
fn remove_dropped_links(ctx: &ServerCtx, previous: &ServerConfig, plans: &[Plan]) -> Result<()> {
    for_each_dropped_link(previous, plans, |kind_name, app, old, link| {
        let peers = old.peers();
        let link_ctx = AppCtx {
            server: ctx,
            app: &old.app,
            shared: &link.shared,
            peers: &peers,
        };
        link.kind
            .remove_application(&link_ctx, &link.name, false)
            .map_err(|e| {
                CompostError::ServerNotReady(format!(
                    "{kind_name} {}: removing {}: {e:#}",
                    app.name,
                    link.path()
                ))
            })?;
        out!(
            "  {kind_name:<10} {:<16} removed link {} ({} {})",
            app.name,
            link.path(),
            link.kind.name(),
            link.shared.name
        );
        Ok(())
    })
}

/// Call `f` with every link of the previous config that `plans` dropped.
fn for_each_dropped_link(
    previous: &ServerConfig,
    plans: &[Plan],
    mut f: impl FnMut(&str, &SharedAppConfig, &Consumer, &ConsumerLink) -> Result<()>,
) -> Result<()> {
    for (kind_name, app) in previous.shared_apps() {
        // Only links that resolved then; a config that does not resolve any
        // more has nothing deployed to remove.
        let Ok(Some(old)) = previous.consumer(kind_name, app) else {
            continue;
        };
        // A shared app removed from the config is left as it is, links included.
        // So is one this deploy leaves out (`--shared`).
        let Some(plan) = plans
            .iter()
            .find(|p| p.selected && p.kind_name() == kind_name && p.app.name == app.name)
        else {
            continue;
        };
        let current = plan.consumer.as_ref();
        for link in &old.links {
            let kept = current.is_some_and(|current| {
                current.links.iter().any(|l| {
                    l.kind.name() == link.kind.name()
                        && l.name == link.name
                        && l.shared.name == link.shared.name
                })
            });
            if !kept {
                f(kind_name, app, &old, link)?;
            }
        }
    }
    Ok(())
}

/// Start a shared app, with the hooks of its links around it.
fn apply(ctx: &ServerCtx, plan: &Plan, pull: bool, recreate: bool) -> Result<()> {
    run_link_hooks(ctx, plan, Hook::BeforeApply)?;
    let on_demand = !plan.kind.runs_service(plan.app);
    if plan.app.is_external() {
        out!(
            "  {} {}: external, not started by compost",
            plan.kind_name(),
            plan.app.name
        );
    } else if on_demand {
        out_part!("  preparing {} {} ... ", plan.kind_name(), plan.app.name);
    } else {
        out_part!("  starting {} {} ... ", plan.kind_name(), plan.app.name);
    }
    match start(ctx, plan, pull, recreate) {
        Ok(()) if plan.app.is_external() => {}
        Ok(()) if on_demand => out!("ready (runs on demand)"),
        Ok(()) => out!("running"),
        Err(err) => {
            out!("failed");
            return Err(err);
        }
    }
    run_link_hooks(ctx, plan, Hook::AfterApply)
}

#[derive(Clone, Copy)]
enum Hook {
    BeforeApply,
    AfterApply,
}

fn run_link_hooks(ctx: &ServerCtx, plan: &Plan, hook: Hook) -> Result<()> {
    let Some(consumer) = &plan.consumer else {
        return Ok(());
    };
    let peers = consumer.peers();
    for link in &consumer.links {
        let link_ctx = AppCtx {
            server: ctx,
            app: &consumer.app,
            shared: &link.shared,
            peers: &peers,
        };
        match hook {
            Hook::BeforeApply => link.kind.before_apply_application(&link_ctx, &link.entry),
            Hook::AfterApply => link.kind.after_apply_application(&link_ctx, &link.entry),
        }
        .with_context(|| link.path())?;
    }
    Ok(())
}

fn start(ctx: &ServerCtx, plan: &Plan, pull: bool, recreate: bool) -> Result<()> {
    if plan.app.is_external() {
        return Ok(());
    }
    if !plan.kind.runs_service(plan.app) {
        return prepare_image(ctx, plan, pull);
    }
    let dir = shell_quote(&ctx.path(&shared::shared_dir(plan.kind_name(), &plan.app.name)));
    let project = shared::project_name(plan.kind_name(), &plan.app.name);
    // `up` pulls only missing images; a new image behind the same tag needs
    // its own pull, after which `up` recreates the containers that use it.
    // Images built on the server (`build:`) are rebuilt on newer base images
    // instead; `build` does nothing for a project without any.
    if pull {
        ctx.connector.exec(&format!(
            "cd {dir} && docker compose -p {project} pull -q --ignore-buildable && docker compose -p {project} build --pull -q",
            project = shell_quote(&project)
        ))?;
    }
    // A pulled image is new too, even when the files are not.
    if plan.changed || recreate || pull {
        plan.kind.check_before_start(ctx, plan.app)?;
    }
    let recreate = if recreate { " --force-recreate" } else { "" };
    // --remove-orphans: a service that was renamed (e.g. `postgres` before
    // services were named `compost-shared-<name>`) must not keep running.
    ctx.connector.exec(&format!(
        "cd {dir} && docker compose -p {} up -d --quiet-pull --remove-orphans{recreate}",
        shell_quote(&project)
    ))?;
    wait_until_ready_within(ctx.connector, &project, plan.kind.start_timeout())?;
    Ok(())
}

/// `--dry-run`: compare what the deploy would write with the server and say
/// what `--apply` would do with each shared app. Link hooks run through a
/// connector that keeps their writes in memory.
fn dry_run(
    ctx: &ServerCtx,
    status: Status,
    plans: &[Plan],
    config: &ServerConfig,
    options: &Options,
) -> Result<()> {
    let state_file = StateFile::load(ctx)?;
    let previous = match ctx.read(SERVER_CONFIG_FILE)? {
        Some(text) => ServerConfig::parse(&text).ok(),
        None => None,
    };
    let stored = stored_config(config, previous.as_ref(), plans)?;
    let dry = DryRun::new(ctx.connector);
    let dry_ctx = ServerCtx {
        connector: &dry,
        home: ctx.home.clone(),
    };
    out!(
        "Dry run: nothing is changed on the server ({status}, home {}).",
        ctx.home
    );

    let network_exists = |network: &str| -> Result<bool> {
        Ok(ctx
            .connector
            .exec(&format!(
                "docker network inspect {} >/dev/null 2>&1 && echo yes || true",
                shell_quote(network)
            ))?
            .trim()
            == "yes")
    };
    for network in all_networks(plans) {
        if !network_exists(&network)? {
            out!("  network {network}: created");
        }
    }
    let selected: Vec<&Plan> = plans.iter().filter(|p| p.selected).collect();
    for plan in &selected {
        for network in &plan.rendered.existing_networks {
            if !network_exists(network)? {
                out!(
                    "  network {network}: missing; {} {} joins it, so the deploy fails",
                    plan.kind_name(),
                    plan.app.name
                );
            }
        }
    }

    for plan in selected {
        let (changes, compose_changed) = plan_changes(ctx, &dry, &dry_ctx, plan, options.diff)?;
        let action = shared_action(
            &state_file,
            plan,
            !changes.is_empty(),
            compose_changed,
            options,
        );
        out!("  {:<10} {:<16} {action}", plan.kind_name(), plan.app.name);
        if let Some(consumer) = &plan.consumer {
            // Once each: Temporal has two links to one database.
            let mut uses: Vec<String> = Vec::new();
            for link in &consumer.links {
                let used = format!("{} {}", link.kind.name(), link.shared.name);
                if !uses.contains(&used) {
                    uses.push(used);
                }
            }
            out!("      uses {}", uses.join(", "));
        }
        server_plan::print_files("      ", &changes, options.diff);
    }

    if let Some(previous) = &previous {
        for_each_dropped_link(previous, plans, |kind_name, app, _old, link| {
            out!(
                "  {kind_name:<10} {:<16} link {} ({} {}): removed",
                app.name,
                link.path(),
                link.kind.name(),
                link.shared.name
            );
            Ok(())
        })?;
    }
    let orphans = if options.shared.is_empty() {
        orphans(&state_file, plans, previous.as_ref())
    } else {
        Vec::new()
    };
    for (kind, name, was_external) in orphans {
        let what = if !was_external {
            "orphaned: not in the configuration any more, left as it is"
        } else if has_links(ctx, &kind, &name)? {
            "orphaned: apps still link to it; deploy them without it first"
        } else {
            "removed: external, compost only forgets it"
        };
        out!("  {kind:<10} {name:<16} {what}");
    }

    let mut files = vec![RenderedFile::new(SERVER_CONFIG_FILE.to_string(), stored)];
    if let Some(text) = options.secrets {
        files.push(RenderedFile::secret(
            secrets::PATH.to_string(),
            text.to_string(),
        ));
    }
    let changes = server_plan::compare(ctx, &files)?;
    server_plan::print_files("  ", &changes, options.diff);
    if !options.apply {
        out!("Without --apply, the files are written and the services are not started.");
    }
    Ok(())
}

/// Files of a shared app that the deploy would add, change or remove, and
/// whether its compose files (or their override) are among them.
fn plan_changes(
    ctx: &ServerCtx,
    dry: &DryRun,
    dry_ctx: &ServerCtx,
    plan: &Plan,
    diff: bool,
) -> Result<(Vec<FileChange>, bool)> {
    let mut changes = server_plan::compare(ctx, &plan.rendered.files)?;
    for dir in &plan.rendered.owned_dirs {
        for name in ctx.connector.list_dir(&ctx.path(dir))? {
            let path = format!("{dir}/{name}");
            if !plan.rendered.files.iter().any(|file| file.path == path) {
                let old = if diff { ctx.read(&path)? } else { None };
                changes.push(FileChange::removed(path, old));
            }
        }
    }

    let dir = shared::shared_dir(plan.kind_name(), &plan.app.name);
    let override_path = format!("{dir}/{SHARED_OVERRIDE_FILE}");
    if let Some(consumer) = &plan.consumer {
        let before = dry.writes();
        prepare_links(dry_ctx, plan, consumer).map_err(|e| {
            CompostError::ServerNotReady(format!("{} {}: {e:#}", plan.kind_name(), plan.app.name))
        })?;
        let written: BTreeMap<_, _> = dry
            .writes()
            .into_iter()
            .filter(|(path, _)| !before.contains_key(path))
            .collect();
        let files = server_plan::captured_files(ctx, written);
        changes.extend(server_plan::compare(ctx, &files)?);
    } else if let Some(old) = ctx.read(&override_path)? {
        changes.push(FileChange::removed(override_path.clone(), Some(old)));
    }
    let compose = compose_path(plan);
    let compose_changed = changes
        .iter()
        .any(|change| change.path == compose || change.path == override_path);
    Ok((changes, compose_changed))
}

/// A shared app that runs on demand: build its image (or pull the one its
/// config names) and check that it speaks the contract.
fn prepare_image(ctx: &ServerCtx, plan: &Plan, pull: bool) -> Result<()> {
    use crate::shared::communication;
    let name = &plan.app.name;
    let image = ctx
        .read(&communication::image_path(name))?
        .context("the image file was not written")?;
    let image = shell_quote(image.trim());
    match &plan.app.image {
        Some(_) => {
            let missing = !ctx
                .connector
                .run(
                    &format!("docker image inspect {image} >/dev/null 2>&1"),
                    None,
                )?
                .success();
            if missing || pull {
                ctx.connector.exec(&format!("docker pull -q {image}"))?;
            }
        }
        None => {
            // The tag changes with the image's files: an existing one is current.
            let build = shell_quote(&ctx.path(&communication::build_dir(name)));
            let script = if pull {
                format!("docker build -q --pull -t {image} {build}")
            } else {
                format!(
                    "docker image inspect {image} >/dev/null 2>&1 || docker build -q -t {image} {build}"
                )
            };
            ctx.connector.exec(&script)?;
        }
    }
    let described = ctx
        .connector
        .exec(&format!("docker run --rm {image} describe"))?;
    let api = serde_json::from_str::<serde_json::Value>(described.trim())
        .ok()
        .and_then(|v| v.get("api").and_then(serde_json::Value::as_u64));
    if api != Some(u64::from(communication::API)) {
        anyhow::bail!(
            "{image} does not implement the communication contract (API {}): `describe` printed {}",
            communication::API,
            described.trim()
        );
    }
    Ok(())
}

/// What `--apply` would do with a shared app, by the rules of [`deploy`].
fn shared_action(
    state_file: &StateFile,
    plan: &Plan,
    changed: bool,
    compose_changed: bool,
    options: &Options,
) -> String {
    let record = state_file.shared(plan.kind_name(), &plan.app.name);
    let deployed = record.is_some_and(|r| r.status == DeployStatus::Deployed);
    let action = if plan.app.is_external() {
        "external: compost does not start it"
    } else if !plan.kind.runs_service(plan.app) {
        if changed || record.is_none() {
            "prepared: image built if needed (runs on demand)"
        } else {
            "unchanged (runs on demand)"
        }
    } else if record.is_none() {
        "created"
    } else if options.recreate {
        "recreated (--recreate)"
    } else if !deployed {
        "recreated: its last deploy did not finish"
    } else if !changed && options.pull {
        "pulled; recreated if a newer image comes"
    } else if !changed {
        "unchanged"
    } else if compose_changed {
        "recreated: compose files changed"
    } else if plan.kind.reloads(plan.app) {
        "reloaded without a restart"
    } else {
        "recreated: configuration files changed"
    };
    action.to_string()
}
