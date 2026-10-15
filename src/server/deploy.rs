//! `depap server deploy` (spec §10.7).

use std::io::Write;
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use super::lock::Lock;
use super::state::{self, Status};
use super::state_file::{DeployStatus, StateFile};
use super::{MANAGER_KEY_FILE, SERVER_CONFIG_FILE};
use crate::config::server::ServerConfig;
use crate::connector::{Connector, shell_quote};
use crate::error::DepapError;
use crate::shared::{self, Rendered, ServerCtx, SharedAppConfig};

/// How long `--apply` waits for a shared app to be running (and healthy).
const START_TIMEOUT: Duration = Duration::from_secs(60);
const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// `docker compose ps` output: one `<service> <state> <health>` line per container.
const PS_FORMAT: &str = "'{{.Service}} {{.State}} {{.Health}}'";

pub struct Options {
    pub apply: bool,
    pub dry_run: bool,
    pub force_unlock: bool,
}

/// One shared app, rendered and ready to be written to the server.
struct Plan<'a> {
    kind: &'static str,
    app: &'a SharedAppConfig,
    rendered: Rendered,
    hash: String,
    /// Rendered files differ from the last deploy (or it is new).
    changed: bool,
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
    if matches!(report.status, Status::Unreachable | Status::NoDocker | Status::Foreign) {
        return Err(DepapError::ServerNotReady(format!(
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
    let plans = render_all(&ctx, config, &state_file)?;
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
        println!("Initialized {}", ctx.home);
    }

    // 4. Write networks and files.
    create_networks(connector, &plans)?;
    for plan in &plans {
        write_files(&ctx, &plan.rendered)?;
        if plan.changed {
            state_file.set_shared(plan.kind, &plan.app.name, DeployStatus::Candidate, &plan.hash);
        }
        let change = if plan.changed { "updated" } else { "unchanged" };
        println!("  {:<9} {:<16} {change}", plan.kind, plan.app.name);
    }
    for record in &state_file.shared {
        let configured = plans.iter().any(|p| p.kind == record.kind && p.app.name == record.name);
        if !configured {
            println!(
                "  {:<9} {:<16} orphaned: not in the configuration any more, left as it is",
                record.kind, record.name
            );
        }
    }
    connector.write_file(&ctx.path(SERVER_CONFIG_FILE), effective.as_bytes(), 0o644)?;
    state_file.save(&ctx)?;

    if !options.apply {
        println!("Files are on the server. Run again with --apply to start the services.");
        return Ok(());
    }

    // 5. Start. `docker compose up -d` leaves unchanged containers alone; containers
    //    whose files changed are recreated, because compose cannot see those changes.
    for plan in &plans {
        let deployed = state_file
            .shared(plan.kind, &plan.app.name)
            .is_some_and(|r| r.status == DeployStatus::Deployed);
        print!("  starting {} {} ... ", plan.kind, plan.app.name);
        std::io::stdout().flush()?;

        match start(&ctx, plan, !deployed) {
            Ok(()) => {
                println!("running");
                state_file.set_shared(plan.kind, &plan.app.name, DeployStatus::Deployed, &plan.hash);
            }
            Err(err) => {
                println!("failed");
                state_file.set_shared(plan.kind, &plan.app.name, DeployStatus::Failed, &plan.hash);
                state_file.save(&ctx)?;
                return Err(DepapError::ServerNotReady(format!(
                    "{} {} did not start: {err:#}",
                    plan.kind, plan.app.name
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
        let kind = shared::find_kind(kind_name)
            .with_context(|| format!("unknown kind `{kind_name}`"))?;
        let rendered = kind
            .render(ctx, app)
            .with_context(|| format!("shared.{kind_name}[{}]", app.name))?;
        let hash = hash_rendered(&rendered);
        let changed = state_file
            .shared(kind.name(), &app.name)
            .is_none_or(|record| record.config_hash != hash);
        plans.push(Plan { kind: kind.name(), app, rendered, hash, changed });
    }
    Ok(plans)
}

/// `sha256:<hex>` over everything a shared app puts on the server.
fn hash_rendered(rendered: &Rendered) -> String {
    let mut hasher = Sha256::new();
    for file in &rendered.files {
        for part in [file.path.as_str(), file.contents.as_str(), &format!("{:o}", file.mode)] {
            hasher.update(part.as_bytes());
            hasher.update([0u8]);
        }
    }
    for part in rendered.dirs.iter().chain(&rendered.networks) {
        hasher.update(part.as_bytes());
        hasher.update([0u8]);
    }
    let hex: String = hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect();
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

fn write_files(ctx: &ServerCtx, rendered: &Rendered) -> Result<()> {
    if !rendered.dirs.is_empty() {
        let dirs: Vec<String> = rendered.dirs.iter().map(|d| shell_quote(&ctx.path(d))).collect();
        ctx.connector.exec(&format!("mkdir -p -- {}", dirs.join(" ")))?;
    }
    for file in &rendered.files {
        ctx.connector
            .write_file(&ctx.path(&file.path), file.contents.as_bytes(), file.mode)?;
    }
    Ok(())
}

fn start(ctx: &ServerCtx, plan: &Plan, recreate: bool) -> Result<()> {
    let dir = shell_quote(&ctx.path(&shared::shared_dir(plan.kind, &plan.app.name)));
    let project = shared::project_name(plan.kind, &plan.app.name);
    let recreate = if recreate { " --force-recreate" } else { "" };
    ctx.connector.exec(&format!(
        "cd {dir} && docker compose -p {} up -d --quiet-pull{recreate}",
        shell_quote(&project)
    ))?;
    wait_until_running(ctx.connector, &project)
}

/// Poll until every container of the project is running and, if it has a
/// healthcheck, healthy.
fn wait_until_running(connector: &dyn Connector, project: &str) -> Result<()> {
    let deadline = Instant::now() + START_TIMEOUT;
    let script = format!("docker compose -p {} ps -a --format {PS_FORMAT}", shell_quote(project));
    loop {
        let output = connector.exec(&script)?;
        let ready = !output.trim().is_empty()
            && output.lines().all(|line| {
                let mut fields = line.split_whitespace().skip(1);
                let state = fields.next().unwrap_or_default();
                let health = fields.next().unwrap_or_default();
                state == "running" && (health.is_empty() || health == "healthy")
            });
        if ready {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "not running after {}s:\n{}",
                START_TIMEOUT.as_secs(),
                output.trim_end()
            );
        }
        sleep(POLL_INTERVAL);
    }
}

fn print_dry_run(ctx: &ServerCtx, status: Status, plans: &[Plan], apply: bool) {
    println!("Dry run: nothing is changed on the server ({status}, home {}).", ctx.home);
    println!("  networks: {}", all_networks(plans).join(", "));
    for plan in plans {
        let change = if plan.changed { "would update" } else { "unchanged" };
        println!("  {:<9} {:<16} {change}", plan.kind, plan.app.name);
        for dir in &plan.rendered.dirs {
            println!("      {dir}/");
        }
        for file in &plan.rendered.files {
            let secret = if file.is_secret() { "  (secret)" } else { "" };
            println!("      {}  {:o}{secret}", file.path, file.mode);
        }
    }
    if apply {
        println!("  then: docker compose up -d for each shared app, and wait until it runs");
    }
}
