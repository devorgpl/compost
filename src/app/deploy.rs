//! `compost app deploy` (spec §10.8).

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_yaml_ng::Value;

use super::contribution::Contribution;
use super::instance::{self, Instance, PinnedLink, PinnedLinks};
use super::plan::AppPlan;
use super::release::{self, Release};
use super::restore::RestorePlan;
use super::secrets::{self, Overlay};
use super::sources;
use crate::backup::profile::{self, AppRun, Flags};
use crate::config::app::{AppConfig, LoadedApp, env_value, link_name};
use crate::config::server::ServerConfig;
use crate::connector::Connector;
use crate::connector::dry_run::DryRun;
use crate::error::CompostError;
use crate::server::SERVER_CONFIG_FILE;
use crate::server::compose::wait_until_ready;
use crate::server::lock::Lock;
use crate::server::plan as server_plan;
use crate::server::state::{self, Report, Status};
use crate::server::state_file::{DeployStatus, StateFile};
use crate::shared::{
    self, AppCtx, Kind, Peer, RenderedFile, Resolution, ServerCtx, SharedAppConfig,
};

pub struct Options<'a> {
    pub apply: bool,
    pub dry_run: bool,
    /// With `dry_run`: also print the changed lines of files without secrets.
    pub diff: bool,
    pub force_unlock: bool,
    /// Also pull images that `build: true` would not: base images and
    /// services without `build`.
    pub pull: bool,
    /// Recreate every container, even unchanged ones.
    pub recreate: bool,
    /// Load data from a backup before the services start (`app restore`).
    pub restore: Option<&'a RestorePlan<'a, 'a>>,
    /// Kinds whose data is deleted with links dropped from the config.
    pub purge: Vec<String>,
    /// Overlay stored as the instance's secrets with its files (`--secret`).
    pub secrets: Option<&'a Overlay>,
    /// Start the images of this release instead of pulling or building (`app rollback`).
    pub pinned: Option<&'a Release>,
    /// Back up the deployed instance before changing it (`deployment.backup`).
    pub backup: Option<PreDeployBackup<'a>>,
}

/// A backup that `--apply` takes before it changes a deployed instance.
pub struct PreDeployBackup<'a> {
    /// The server's name, for archive names.
    pub server_name: &'a str,
    pub profile: &'a str,
}

/// The deployed instance and the settings of the backup before the deploy;
/// `None` for an instance that isn't deployed yet. The profile is the one the
/// config being deployed defines.
fn pre_deploy_backup(
    ctx: &ServerCtx,
    app: &AppConfig,
    profile: &str,
) -> Result<Option<(Instance, AppRun, ServerConfig)>> {
    let Some(deployed) = instance::load(ctx, &app.name, &app.prefix)? else {
        return Ok(None);
    };
    let server_config = instance::server_config(ctx)?;
    let settings = server_config.backup.as_ref();
    let flags = Flags {
        profile: Some(profile.to_string()),
        ..Flags::default()
    };
    let run = profile::app_run(settings, app, &flags).ok_or_else(|| {
        if profile::defined(settings, &[app], profile) {
            profile::not_for_app(profile, &app.instance())
        } else {
            profile::unknown_profile(profile)
        }
    })?;
    Ok(Some((deployed, run, server_config)))
}

/// A link of the app, resolved to a shared app on the server.
struct Link<'a> {
    kind: &'static dyn Kind,
    name: &'a str,
    entry: &'a Value,
    shared: SharedAppConfig,
}

impl Link<'_> {
    fn pinned(&self) -> PinnedLink {
        PinnedLink {
            kind: self.kind.name().to_string(),
            entry: self.name.to_string(),
            instance: self.shared.name.clone(),
            type_name: self.shared.type_name.clone(),
            version: self.shared.version,
        }
    }

    fn peer(&self) -> Peer<'_> {
        Peer {
            kind: self.kind,
            name: self.name,
            entry: self.entry,
            shared: &self.shared,
        }
    }

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
    super::remove::check_purge(&options.purge)?;

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
    let links = resolve_links(&ctx, loaded, &report, &StateFile::load(&ctx)?)?;
    let pinned_links: Vec<PinnedLink> = links.iter().map(Link::pinned).collect();
    let dropped = dropped_links(&ctx, app, &pinned_links)?;
    let files = sources::collect(loaded, config_files)?;

    let project = app.instance();
    let backup = match (&options.backup, options.apply) {
        (Some(backup), true) => pre_deploy_backup(&ctx, app, backup.profile)?
            .map(|(deployed, run, server_config)| (backup, deployed, run, server_config)),
        _ => None,
    };
    if options.dry_run {
        let backup = backup.map(|(backup, _, run, server_config)| {
            let target = run
                .target
                .or_else(|| server_config.backup.and_then(|b| b.target))
                .unwrap_or_else(|| "local".into());
            format!(
                "backs up {project} first with profile {} (target {target})",
                backup.profile
            )
        });
        let plan = DryRunInput {
            links: &links,
            pinned_links: &pinned_links,
            dropped: &dropped,
            uploads: &files,
            backup,
        };
        return dry_run(&ctx, loaded, &plan, options);
    }

    // 2. Upload and render.
    let _lock = Lock::acquire(connector, &ctx.home, options.force_unlock)?;
    if let Some((backup, deployed, run, server_config)) = &backup {
        let written = super::backup::write_one(
            &ctx,
            server_config,
            backup.server_name,
            deployed,
            run,
            Some(backup.profile),
            None,
            false,
        );
        written.context(
            "the backup before the deploy failed; nothing was changed (--no-backup skips it)",
        )?;
    }
    // Read under the lock: a command that finished since the preflight may have
    // changed it, and saving an older copy would undo its changes.
    let mut state_file = StateFile::load(&ctx)?;
    let dir = instance::dir(&app.name, &app.prefix);
    let archive = sources::pack(&files)?;
    connector.replace_dir(&ctx.path(&format!("{dir}/sources")), &archive)?;
    out!("  uploaded {} files", files.len());

    let override_yaml = render_override(&ctx, loaded, &links)?;
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
    if let Some(overlay) = options.secrets {
        let path = secrets::store(&ctx, overlay)?;
        out!("  stored secrets in {path}");
    }
    // Dropped links stay listed until a deploy with --apply removes them.
    let write_pinned = |removing: &[PinnedLink]| {
        let pinned = PinnedLinks {
            links: pinned_links.clone(),
            removing: removing.to_vec(),
        };
        write(
            instance::LINKS_FILE,
            &serde_yaml_ng::to_string(&pinned)?,
            0o644,
        )
    };
    write_pinned(&dropped)?;

    state_file.set_app(&app.name, &app.prefix, DeployStatus::Candidate);
    state_file.save(&ctx)?;
    if !options.apply {
        for link in &dropped {
            out!(
                "  link {} {} ({}) is no longer configured; the next deploy with --apply removes it",
                link.kind,
                link.entry,
                link.instance
            );
        }
        out!("Files are on the server. Run again with --apply to start {project}.");
        return Ok(());
    }

    // 3. Apply. On failure the instance is marked `failed`.
    let dropped = Dropped {
        links: &dropped,
        purge: &options.purge,
    };
    match apply(&ctx, loaded, &links, &dropped, options) {
        Ok(kept) => {
            if kept.len() != dropped.links.len() {
                write_pinned(&kept)?;
            }
            // The services run; a release that cannot be recorded does not undo that.
            let rollback_of = options.pinned.map(|release| release.number);
            match release::record(&ctx, app, rollback_of) {
                Ok(Some(number)) => out!("  recorded release {number}"),
                Ok(None) => {}
                Err(err) => eprintln!("warning: could not record a release: {err:#}"),
            }
            state_file.set_app(&app.name, &app.prefix, DeployStatus::Deployed);
            state_file.save(&ctx)?;
            out!();
            super::state::print(
                &ctx,
                &Instance {
                    app: stored_config,
                    links: pinned_links,
                    removing: kept,
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

/// Run the links' render hooks, which write link records, and return the
/// compose override with the variables and networks they add.
fn render_override(ctx: &ServerCtx, loaded: &LoadedApp, links: &[Link]) -> Result<String> {
    let app = &loaded.config;
    let peers: Vec<Peer> = links.iter().map(Link::peer).collect();
    let mut contribution = Contribution::default();
    for service in &app.app_services {
        for (key, value) in &service.environment {
            let value = env_value(value).context("environment values are checked on load")?;
            contribution.set_env(&service.service, key, value)?;
        }
    }
    for link in links {
        let link_ctx = AppCtx {
            server: ctx,
            app,
            shared: &link.shared,
            peers: &peers,
        };
        // Fails on server-side conflicts, e.g. a domain used by another instance.
        link.kind
            .prepare_application(&link_ctx, link.entry, &mut contribution)
            .map_err(|e| not_ready(&format!("{}: {e:#}", link.path())))?;
    }
    contribution.override_yaml(&loaded.compose)
}

/// `--dry-run`: render as a deploy would, through a connector that keeps the
/// writes in memory, and compare the result with the server.
/// What `deploy` worked out before the dry run starts.
struct DryRunInput<'a> {
    links: &'a [Link<'a>],
    pinned_links: &'a [PinnedLink],
    dropped: &'a [PinnedLink],
    uploads: &'a [sources::Upload],
    /// The line about the backup before the deploy, if one would run.
    backup: Option<String>,
}

fn dry_run(
    ctx: &ServerCtx,
    loaded: &LoadedApp,
    input: &DryRunInput,
    options: &Options,
) -> Result<()> {
    let DryRunInput {
        links,
        pinned_links,
        dropped,
        uploads,
        ..
    } = *input;
    let app = &loaded.config;
    let dry = DryRun::new(ctx.connector);
    let dry_ctx = ServerCtx {
        connector: &dry,
        home: ctx.home.clone(),
    };
    let override_yaml = render_override(&dry_ctx, loaded, links)?;
    let mut previews = Vec::new();
    if options.apply {
        let peers: Vec<Peer> = links.iter().map(Link::peer).collect();
        for link in links {
            let link_ctx = AppCtx {
                server: &dry_ctx,
                app,
                shared: &link.shared,
                peers: &peers,
            };
            previews.extend(
                link.kind
                    .preview_application(&link_ctx, link.entry)
                    .with_context(|| link.path())?,
            );
        }
    }

    let dir = instance::dir(&app.name, &app.prefix);
    let mut files = server_plan::captured_files(ctx, dry.writes());
    files.push(RenderedFile::secret(
        format!("{dir}/{}", instance::OVERRIDE_FILE),
        override_yaml.clone(),
    ));
    files.push(RenderedFile::new(
        format!("{dir}/{}", instance::CONFIG_FILE),
        serde_yaml_ng::to_string(&app.without_secrets()?)?,
    ));
    let pinned = PinnedLinks {
        links: pinned_links.to_vec(),
        removing: dropped.to_vec(),
    };
    files.push(RenderedFile::new(
        format!("{dir}/{}", instance::LINKS_FILE),
        serde_yaml_ng::to_string(&pinned)?,
    ));
    if let Some(overlay) = options.secrets {
        files.push(RenderedFile::secret(
            secrets::path(&app.name, &app.prefix),
            overlay.text.clone(),
        ));
    }
    files.extend(previews);

    let plan = AppPlan {
        loaded,
        uploads,
        files,
        override_yaml: &override_yaml,
        links: links
            .iter()
            .map(|link| {
                format!(
                    "{}: uses {} {}",
                    link.path(),
                    link.shared.type_name,
                    link.shared.name
                )
            })
            .collect(),
        dropped: dropped
            .iter()
            .map(|link| {
                format!(
                    "link {} {} ({}): no longer configured, {}",
                    link.kind,
                    link.entry,
                    link.instance,
                    removal(link, &options.purge)
                )
            })
            .collect(),
        backup: input.backup.clone(),
        apply: options.apply,
        pull: options.pull,
        recreate: options.recreate,
        diff: options.diff,
    };
    super::plan::print(ctx, &plan)
}

/// Links of the last deploy (or still waiting for removal) that the config
/// no longer has, or that now use another shared app.
fn dropped_links(
    ctx: &ServerCtx,
    app: &AppConfig,
    current: &[PinnedLink],
) -> Result<Vec<PinnedLink>> {
    let Some(previous) = instance::load(ctx, &app.name, &app.prefix)? else {
        return Ok(Vec::new());
    };
    let mut dropped: Vec<PinnedLink> = Vec::new();
    for link in previous.links.into_iter().chain(previous.removing) {
        let gone = !current.iter().any(|c| c.same(&link));
        if gone && !dropped.iter().any(|d| d.same(&link)) {
            dropped.push(link);
        }
    }
    Ok(dropped)
}

/// What happens to a dropped link, for messages.
fn removal(link: &PinnedLink, purge: &[String]) -> &'static str {
    if purge.contains(&link.kind) {
        "would be removed with its data (--purge)"
    } else {
        "would be removed; its data is kept"
    }
}

/// Links to remove during apply.
struct Dropped<'a> {
    links: &'a [PinnedLink],
    purge: &'a [String],
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
/// Dropped links are removed once the app runs, before the other links'
/// `after_apply_application`: an ingress route moved to another entry must
/// leave its old site first. Returns the dropped links that stay.
fn apply(
    ctx: &ServerCtx,
    loaded: &LoadedApp,
    links: &[Link],
    dropped: &Dropped,
    options: &Options,
) -> Result<Vec<PinnedLink>> {
    let app = &loaded.config;
    let restore = options.restore;
    let peers: Vec<Peer> = links.iter().map(Link::peer).collect();
    for link in links {
        let link_ctx = AppCtx {
            server: ctx,
            app,
            shared: &link.shared,
            peers: &peers,
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
    let images_restored = match (restore, options.pinned) {
        (Some(plan), _) => plan.load_images(ctx, app)?,
        (None, Some(release)) => release::retag(ctx, &project, release)?,
        (None, None) => false,
    };
    // With restored or pinned images, `up` still builds or pulls the ones that are missing.
    if !images_restored {
        if !app.deployment.build || options.pull {
            ctx.connector
                .exec(&format!("{compose} pull -q --ignore-buildable"))?;
        }
        if app.deployment.build {
            out!("  building images ...");
            let pull = if options.pull { " --pull" } else { "" };
            ctx.connector.exec(&format!("{compose} build -q{pull}"))?;
        }
    }
    if let Some(plan) = restore {
        plan.restore_volumes(ctx, app, &loaded.compose, true)?;
    }
    out!("  starting {project} ...");
    let recreate = if options.recreate {
        " --force-recreate"
    } else {
        ""
    };
    ctx.connector
        .exec(&format!("{compose} up -d --remove-orphans{recreate}"))?;
    wait_until_ready(ctx.connector, &project)?;

    let kept = super::remove::remove_links(ctx, app, dropped.links, dropped.purge, true)?;
    for link in links {
        let link_ctx = AppCtx {
            server: ctx,
            app,
            shared: &link.shared,
            peers: &peers,
        };
        link.kind
            .after_apply_application(&link_ctx, link.entry)
            .with_context(|| link.path())?;
    }
    Ok(kept)
}

fn not_ready(message: &str) -> anyhow::Error {
    CompostError::ServerNotReady(message.to_string()).into()
}
