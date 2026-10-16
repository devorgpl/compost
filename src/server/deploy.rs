//! `compost server deploy` (spec §10.7).

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use super::compose::wait_until_ready_within;
use super::lock::Lock;
use super::state::{self, Status};
use super::state_file::{DeployStatus, StateFile};
use super::{MANAGER_KEY_FILE, SERVER_CONFIG_FILE};
use crate::app::contribution::Contribution;
use crate::config::app::ComposeFile;
use crate::config::server::{Consumer, ServerConfig};
use crate::connector::{Connector, shell_quote};
use crate::error::CompostError;
use crate::shared::{self, AppCtx, Kind, Rendered, ServerCtx, SharedAppConfig};

/// Written next to `compose.yaml` of a shared app with links; docker compose
/// reads it automatically.
pub const SHARED_OVERRIDE_FILE: &str = "compose.override.yaml";

pub struct Options {
    pub apply: bool,
    pub dry_run: bool,
    pub force_unlock: bool,
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
}

impl Plan<'_> {
    fn kind_name(&self) -> &'static str {
        self.kind.name()
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
    let mut state_file = StateFile::load(&ctx)?;
    let previous = match ctx.read(SERVER_CONFIG_FILE)? {
        Some(text) => ServerConfig::parse(&text).ok(),
        None => None,
    };
    let mut plans = render_all(&ctx, config, &state_file)?;
    let effective = serde_yaml_ng::to_string(&config.without_secrets()?)?;

    if options.dry_run {
        print_dry_run(&ctx, report.status, &plans, options.apply);
        return Ok(());
    }

    // 3. Initialize and lock.
    let initialize = report.status == Status::Uninitialized;
    if initialize {
        connector.exec(&format!("mkdir -p -- {}", shell_quote(&ctx.home)))?;
    }
    let _lock = Lock::acquire(connector, &ctx.home, options.force_unlock)?;
    if initialize {
        let key = format!("{manager_key}\n");
        connector.write_file(&ctx.path(MANAGER_KEY_FILE), key.as_bytes(), 0o644)?;
        out!("Initialized {}", ctx.home);
    }

    // 4. Write networks and files.
    check_existing_networks(connector, &plans)?;
    create_networks(connector, &plans)?;
    for plan in &mut plans {
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
        out!("  {:<9} {:<16} {change}", plan.kind_name(), plan.app.name);
    }
    let orphans: Vec<(String, String)> = state_file
        .shared
        .iter()
        .filter(|record| {
            !plans
                .iter()
                .any(|p| p.kind_name() == record.kind && p.app.name == record.name)
        })
        .map(|record| (record.kind.clone(), record.name.clone()))
        .collect();
    for (kind, name) in orphans {
        let was_external = previous.as_ref().is_some_and(|config| {
            config
                .shared
                .get(&kind)
                .is_some_and(|apps| apps.iter().any(|a| a.name == name && a.is_external()))
        });
        if was_external && forget_external(&ctx, &kind, &name)? {
            state_file.remove_shared(&kind, &name);
            out!("  {kind:<9} {name:<16} removed: external, compost only forgets it");
        } else if was_external {
            out!(
                "  {kind:<9} {name:<16} orphaned: apps still link to it; deploy them without it first"
            );
        } else {
            out!(
                "  {kind:<9} {name:<16} orphaned: not in the configuration any more, left as it is"
            );
        }
    }
    connector.write_file(&ctx.path(SERVER_CONFIG_FILE), effective.as_bytes(), 0o644)?;
    state_file.save(&ctx)?;

    if !options.apply {
        out!("Files are on the server. Run again with --apply to start the services.");
        return Ok(());
    }

    // 5. Start. `docker compose up -d` leaves unchanged containers alone; containers
    //    whose files changed are recreated, because compose cannot see those changes,
    //    unless the shared app can reload changed configuration files itself.
    for plan in &plans {
        let kind = plan.kind_name();
        let deployed = state_file
            .shared(kind, &plan.app.name)
            .is_some_and(|r| r.status == DeployStatus::Deployed);
        let recreate = !deployed && !reloaded(&ctx, plan)?;

        match apply(&ctx, plan, recreate) {
            Ok(()) => {
                state_file.set_shared(kind, &plan.app.name, DeployStatus::Deployed, &plan.hash);
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
    for (kind_name, app) in config.shared_apps() {
        let kind =
            shared::find_kind(kind_name).with_context(|| format!("unknown kind `{kind_name}`"))?;
        let rendered = kind
            .render(ctx, app)
            .with_context(|| format!("shared.{kind_name}[{}]", app.name))?;
        let hash = hash_rendered(&rendered);
        let record = state_file.shared(kind.name(), &app.name);
        let changed = record.is_none_or(|record| record.config_hash != hash);
        let was_deployed = record.is_some_and(|record| record.status == DeployStatus::Deployed);
        let consumer = config.consumer(kind_name, app)?;
        plans.push(Plan {
            kind,
            app,
            rendered,
            hash,
            changed,
            was_deployed,
            compose_changed: true,
            consumer,
        });
    }
    // Shared apps with links start after the shared apps they use.
    plans.sort_by_key(|plan| plan.consumer.is_some());
    Ok(plans)
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

fn all_networks(plans: &[Plan]) -> Vec<String> {
    let mut networks: Vec<String> = plans
        .iter()
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
    for plan in plans {
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
    let dir = shared::connectors_dir(kind, name);
    let links = ctx
        .connector
        .list_dir(&ctx.path(&dir))?
        .into_iter()
        .any(|file| !file.starts_with("root-"));
    if links {
        return Ok(false);
    }
    ctx.connector
        .exec(&format!("rm -rf -- {}", shell_quote(&ctx.path(&dir))))?;
    Ok(true)
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

/// Start a shared app, with the hooks of its links around it.
fn apply(ctx: &ServerCtx, plan: &Plan, recreate: bool) -> Result<()> {
    run_link_hooks(ctx, plan, Hook::BeforeApply)?;
    if plan.app.is_external() {
        out!(
            "  {} {}: external, not started by compost",
            plan.kind_name(),
            plan.app.name
        );
    } else {
        out_part!("  starting {} {} ... ", plan.kind_name(), plan.app.name);
    }
    match start(ctx, plan, recreate) {
        Ok(()) if plan.app.is_external() => {}
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

fn start(ctx: &ServerCtx, plan: &Plan, recreate: bool) -> Result<()> {
    if plan.app.is_external() {
        return Ok(());
    }
    let dir = shell_quote(&ctx.path(&shared::shared_dir(plan.kind_name(), &plan.app.name)));
    let project = shared::project_name(plan.kind_name(), &plan.app.name);
    let recreate = if recreate { " --force-recreate" } else { "" };
    ctx.connector.exec(&format!(
        "cd {dir} && docker compose -p {} up -d --quiet-pull{recreate}",
        shell_quote(&project)
    ))?;
    wait_until_ready_within(ctx.connector, &project, plan.kind.start_timeout())?;
    Ok(())
}

fn print_dry_run(ctx: &ServerCtx, status: Status, plans: &[Plan], apply: bool) {
    out!(
        "Dry run: nothing is changed on the server ({status}, home {}).",
        ctx.home
    );
    out!("  networks: {}", all_networks(plans).join(", "));
    let existing: Vec<&str> = plans
        .iter()
        .flat_map(|plan| plan.rendered.existing_networks.iter().map(String::as_str))
        .collect();
    if !existing.is_empty() {
        out!("  existing networks joined: {}", existing.join(", "));
    }
    for plan in plans {
        let change = if plan.changed {
            "would update"
        } else {
            "unchanged"
        };
        out!("  {:<9} {:<16} {change}", plan.kind_name(), plan.app.name);
        if let Some(consumer) = &plan.consumer {
            let uses: Vec<String> = consumer
                .links
                .iter()
                .map(|link| format!("{} {}", link.kind.name(), link.shared.name))
                .collect();
            out!("      uses {}", uses.join(", "));
        }
        for dir in &plan.rendered.dirs {
            out!("      {dir}/");
        }
        for file in &plan.rendered.files {
            let secret = if file.is_secret() { "  (secret)" } else { "" };
            out!("      {}  {:o}{secret}", file.path, file.mode);
        }
    }
    if apply {
        out!("  then: docker compose up -d for each shared app, and wait until it runs");
    }
}
