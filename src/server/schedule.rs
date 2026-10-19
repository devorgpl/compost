//! Scheduled backups and notifications. Profiles with a `schedule` run on the
//! server by themselves, and `notifications` are checked there:
//! `server deploy` and `app deploy` install compost there (`bin/compost`) and
//! one crontab line that runs `compost backup tick` every few minutes. The
//! tick runs every job that is due, records how it went in `state.yaml`, then
//! checks what `notifications` watches ([`super::alerts`]).

use std::fs;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local};

use super::MANAGER_KEY_FILE;
use super::backup as server_backup;
use super::lock::Lock;
use super::state_file::{BackupRecord, BackupStatus, StateFile};
use crate::app::backup::{self as app_backup, Which};
use crate::app::instance::{self, Selector};
use crate::app::state::initialized_server;
use crate::backup::profile::{self, Flags};
use crate::backup::schedule::Schedule;
use crate::connector::local::Local as LocalConnector;
use crate::connector::{Connector, shell_quote};
use crate::error::config_err;
use crate::shared::ServerCtx;

/// compost on the server, relative to `COMPOST_SERVER_HOME`.
pub const BINARY: &str = "bin/compost";
/// Output of the ticks.
pub const LOG: &str = "logs/backup.log";
/// The log is cut to its second half above this size.
const LOG_LIMIT: u64 = 1_000_000;
/// How often cron starts a tick.
const TICK: &str = "*/5 * * * *";
/// Owner of the shared apps' jobs in `state.yaml`.
pub const SERVER_OWNER: &str = "server";
/// Releases to download compost from.
const RELEASES: &str = "https://github.com/devorgpl/compost/releases/download";
/// A compost binary to install instead of this one or a download, e.g. a static build.
pub const BINARY_ENV: &str = "COMPOST_SERVER_BINARY";

/// A profile run on a schedule for the shared apps or one instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    /// `server`, or an instance's name and prefix.
    pub instance: Option<(String, String)>,
    pub profile: String,
    pub schedule: String,
}

impl Job {
    /// Its name in `state.yaml`: `server` or e.g. `myapp-prod`.
    pub fn owner(&self) -> String {
        match &self.instance {
            None => SERVER_OWNER.to_string(),
            Some((name, prefix)) => format!("{name}-{prefix}"),
        }
    }

    fn describe(&self) -> String {
        match &self.instance {
            None => format!("profile {} of the shared apps", self.profile),
            Some(_) => format!("profile {} of {}", self.profile, self.owner()),
        }
    }
}

/// Every scheduled job, from the stored server config and the stored configs of the instances.
pub fn jobs(ctx: &ServerCtx) -> Result<Vec<Job>> {
    let config = instance::server_config(ctx)?;
    let settings = config.backup.as_ref();
    let mut jobs = Vec::new();
    if let Some(settings) = settings {
        if config.shared_apps().next().is_some() {
            for name in settings.profiles.keys() {
                let flags = Flags {
                    profile: Some(name.clone()),
                    ..Flags::default()
                };
                if let Some(schedule) =
                    profile::server_run(Some(settings), &flags).and_then(|r| r.schedule)
                {
                    jobs.push(Job {
                        instance: None,
                        profile: name.clone(),
                        schedule,
                    });
                }
            }
        }
    }
    let server_profiles: Vec<&String> = settings
        .map(|s| s.profiles.keys().collect())
        .unwrap_or_default();
    for record in StateFile::load(ctx)?.apps {
        let Some(found) = instance::load(ctx, &record.name, &record.prefix)? else {
            continue;
        };
        let mut names: Vec<&String> = server_profiles
            .iter()
            .copied()
            .chain(found.app.backup.profiles.keys())
            .collect();
        names.sort();
        names.dedup();
        for name in names {
            let flags = Flags {
                profile: Some(name.clone()),
                ..Flags::default()
            };
            if let Some(schedule) =
                profile::app_run(settings, &found.app, &flags).and_then(|r| r.schedule)
            {
                jobs.push(Job {
                    instance: Some((record.name.clone(), record.prefix.clone())),
                    profile: name.clone(),
                    schedule,
                });
            }
        }
    }
    Ok(jobs)
}

/// After a deploy changed the stored configs: install compost and the crontab
/// line when there are jobs, remove the line when there are none.
pub fn ensure(ctx: &ServerCtx, server_name: &str) -> Result<()> {
    if !needed(ctx)? {
        if update_crontab(ctx, None)? {
            out!(
                "  removed the crontab line of scheduled backups: no profile has a schedule, and nothing is notified"
            );
        }
        return Ok(());
    }
    install_binary(ctx)?;
    // The crontab line creates the log's folder too, in case it is removed later.
    ctx.connector.exec(&format!(
        "mkdir -p -- {}",
        shell_quote(log_dir(&ctx.path(LOG)))
    ))?;
    if update_crontab(ctx, Some(&cron_line(ctx, server_name)?))? {
        out!("  installed the crontab line of scheduled backups and notifications");
    }
    Ok(())
}

/// Whether the tick has work: scheduled backups, or notifications to send.
fn needed(ctx: &ServerCtx) -> Result<bool> {
    let notifies = instance::server_config(ctx)?
        .notifications
        .is_some_and(|n| !n.send_to.is_empty());
    Ok(notifies || !jobs(ctx)?.is_empty())
}

/// [`ensure`], after a command changed the stored configs.
pub fn refresh(
    connector: &dyn Connector,
    server_home: &str,
    manager_key: &str,
    server_name: &str,
) -> Result<()> {
    let ctx = initialized_server(connector, server_home, manager_key)?;
    ensure(&ctx, server_name)
}

/// The folder of the log.
fn log_dir(log: &str) -> &str {
    log.rsplit_once('/').map_or(".", |(dir, _)| dir)
}

/// The crontab entry: `tick` every few minutes, its output in the log.
fn cron_line(ctx: &ServerCtx, server_name: &str) -> Result<String> {
    // Cron's PATH is short; docker may be elsewhere.
    let docker = ctx.connector.exec("command -v docker || true")?;
    let mut path = Vec::new();
    if let Some((dir, _)) = docker.trim().rsplit_once('/') {
        if !dir.is_empty() {
            path.push(dir.to_string());
        }
    }
    for dir in ["/usr/local/bin", "/usr/bin", "/bin"] {
        if !path.iter().any(|p| p == dir) {
            path.push(dir.to_string());
        }
    }
    Ok(format_cron_line(
        &path.join(":"),
        &ctx.path(BINARY),
        &ctx.home,
        server_name,
        &ctx.path(LOG),
    ))
}

/// The line itself. It creates the log's folder first: the shell opens the
/// log before it starts compost, and fails without a word when the folder is
/// gone.
fn format_cron_line(path: &str, binary: &str, home: &str, server_name: &str, log: &str) -> String {
    format!(
        "{TICK} mkdir -p -- {} && PATH={} {} backup tick --server-home {} --server-name {} >> {} 2>&1",
        shell_quote(log_dir(log)),
        shell_quote(path),
        shell_quote(binary),
        shell_quote(home),
        shell_quote(server_name),
        shell_quote(log),
    )
}

fn markers(home: &str) -> (String, String) {
    (
        format!("# compost: scheduled backups of {home}"),
        format!("# compost: end of {home}"),
    )
}

/// `crontab` with compost's block for `home` replaced by `line` (or removed).
pub fn replace_block(crontab: &str, home: &str, line: Option<&str>) -> String {
    let (begin, end) = markers(home);
    let mut out = Vec::new();
    let mut inside = false;
    for current in crontab.lines() {
        if current == begin {
            inside = true;
        } else if current == end {
            inside = false;
        } else if !inside {
            out.push(current.to_string());
        }
    }
    if let Some(line) = line {
        out.push(begin);
        out.push(line.to_string());
        out.push(end);
    }
    let mut text = out.join("\n");
    if !text.is_empty() {
        text.push('\n');
    }
    text
}

/// Write the crontab if compost's block changes; returns whether it did.
fn update_crontab(ctx: &ServerCtx, line: Option<&str>) -> Result<bool> {
    let connector = ctx.connector;
    let has_crontab = connector
        .run("command -v crontab >/dev/null", None)?
        .success();
    if !has_crontab {
        if line.is_some() {
            bail!(
                "scheduled backups need cron on the server, and `crontab` is not there; install cron (e.g. the `cron` package) and deploy again"
            );
        }
        return Ok(false);
    }
    // No crontab yet is an error for `crontab -l`, and the same as an empty one here.
    let current = connector.run("crontab -l 2>/dev/null", None)?.stdout;
    let updated = replace_block(&current, &ctx.home, line);
    if updated == current {
        return Ok(false);
    }
    let output = connector.run("crontab -", Some(updated.as_bytes()))?;
    if !output.success() {
        bail!("cannot write the crontab: {}", output.stderr.trim());
    }
    Ok(true)
}

/// Put this version of compost at `bin/compost`, unless it is there.
fn install_binary(ctx: &ServerCtx) -> Result<()> {
    let connector = ctx.connector;
    let version = env!("CARGO_PKG_VERSION");
    let binary = ctx.path(BINARY);
    let quoted = shell_quote(&binary);
    let installed = connector.run(&format!("{quoted} --version 2>/dev/null"), None)?;
    if installed.success() && installed.stdout.trim() == format!("compost {version}") {
        return Ok(());
    }

    let upload = match std::env::var_os(BINARY_ENV) {
        Some(path) => Some(std::path::PathBuf::from(path)),
        None if can_run_here(connector)? => {
            Some(std::env::current_exe().context("cannot find this compost binary")?)
        }
        None => None,
    };
    let tmp = format!("{binary}.new");
    connector.exec(&format!(
        "mkdir -p -- {}",
        shell_quote(binary.rsplit_once('/').map_or(".", |(dir, _)| dir))
    ))?;
    match upload {
        Some(path) => {
            let bytes =
                fs::read(&path).with_context(|| format!("cannot read {}", path.display()))?;
            connector.write_file(&tmp, &bytes, 0o755)?;
            out!("  installed compost {version} at {binary} for scheduled backups");
        }
        None => {
            if version.contains('-') {
                return Err(config_err(format!(
                    "scheduled backups need compost on the server, and this development build ({version}) \
                     can't install itself there; set {BINARY_ENV} to a static Linux build of it"
                )));
            }
            connector
                .exec(&download_script(version, &tmp))
                .context("cannot download compost onto the server for scheduled backups")?;
            out!("  downloaded compost {version} to {binary} for scheduled backups");
        }
    }
    connector.exec(&format!("mv -f -- {} {quoted}", shell_quote(&tmp)))?;
    Ok(())
}

/// Whether this binary runs on the server: the same machine, or a static
/// Linux build for the server's CPU.
fn can_run_here(connector: &dyn Connector) -> Result<bool> {
    if connector.target() == "local" {
        return Ok(true);
    }
    if !cfg!(all(target_os = "linux", target_env = "musl")) {
        return Ok(false);
    }
    let machine = connector.exec("uname -m")?;
    let machine = match machine.trim() {
        "arm64" => "aarch64",
        other => other,
    };
    Ok(machine == std::env::consts::ARCH)
}

/// Download the release `version` for the server's CPU to `to`, checked against SHA256SUMS.
fn download_script(version: &str, to: &str) -> String {
    format!(
        r#"set -e
case "$(uname -m)" in
  x86_64) target=x86_64-unknown-linux-musl ;;
  aarch64|arm64) target=aarch64-unknown-linux-musl ;;
  *) echo "no compost build for $(uname -m)" >&2; exit 1 ;;
esac
name="compost-v{version}-$target"
base="{RELEASES}/v{version}"
dir="$(mktemp -d)"
trap 'rm -rf "$dir"' EXIT
get() {{ if command -v curl >/dev/null; then curl -fsSL -o "$2" "$1"; else wget -qO "$2" "$1"; fi; }}
get "$base/$name.tar.gz" "$dir/$name.tar.gz"
get "$base/SHA256SUMS" "$dir/SHA256SUMS"
expected="$(grep " $name.tar.gz\$" "$dir/SHA256SUMS" | cut -d ' ' -f 1)"
actual="$(sha256sum "$dir/$name.tar.gz" | cut -d ' ' -f 1)"
[ -n "$expected" ] && [ "$expected" = "$actual" ] || {{ echo "checksum mismatch for $name.tar.gz" >&2; exit 1; }}
tar -xzf "$dir/$name.tar.gz" -C "$dir"
cp "$dir/$name/compost" {to}
chmod 755 {to}"#,
        to = shell_quote(to),
    )
}

/// `compost backup tick`, run by cron on the server: run every job that is due.
pub fn tick(server_home: &str, server_name: &str) -> Result<()> {
    let connector = LocalConnector { verbose: false };
    let home = connector.resolve_path(server_home)?;
    let manager_key = fs::read_to_string(format!("{home}/{MANAGER_KEY_FILE}"))
        .with_context(|| format!("{home} is not managed by compost"))?;
    let ctx = initialized_server(&connector, &home, manager_key.trim())?;
    trim_log(&ctx.path(LOG));

    let now = Local::now();
    let jobs = jobs(&ctx)?;
    let state = StateFile::load(&ctx)?;
    let mut updates = Vec::new();
    for job in &jobs {
        let schedule: Schedule = job
            .schedule
            .parse()
            .map_err(|e| config_err(format!("{}: schedule: {e}", job.describe())))?;
        let record = state
            .backups
            .iter()
            .find(|r| r.owner == job.owner() && r.profile == job.profile);
        let last = match record.map(|r| DateTime::parse_from_rfc3339(&r.last_run)) {
            Some(Ok(time)) => time.with_timezone(&Local),
            // New jobs wait for their first scheduled time.
            _ => {
                updates.push(record_of(job, now, BackupStatus::Waiting, None));
                continue;
            }
        };
        if schedule
            .due(last.naive_local(), now.naive_local())
            .is_none()
        {
            continue;
        }
        wait_for_lock(&ctx)?;
        let started = Local::now();
        out!(
            "[{}] backup with {}",
            started.format("%Y-%m-%d %H:%M:%S"),
            job.describe()
        );
        let result = run(&connector, &ctx.home, server_name, manager_key.trim(), job);
        let (status, error) = match result {
            Ok(()) => (BackupStatus::Ok, None),
            Err(err) => {
                eprintln!("error: {err:#}");
                (BackupStatus::Failed, Some(format!("{err:#}")))
            }
        };
        out!("[{}] {status}", Local::now().format("%Y-%m-%d %H:%M:%S"));
        updates.push(record_of(job, started, status, error));
    }
    let recorded = record(&ctx, &jobs, updates);
    if let Err(err) = super::alerts::evaluate(&ctx, server_name, manager_key.trim()) {
        eprintln!("error: notifications: {err:#}");
    }
    recorded
}

fn record_of(
    job: &Job,
    time: DateTime<Local>,
    status: BackupStatus,
    error: Option<String>,
) -> BackupRecord {
    BackupRecord {
        owner: job.owner(),
        profile: job.profile.clone(),
        last_run: time.to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
        status,
        error,
    }
}

fn run(
    connector: &dyn Connector,
    home: &str,
    server_name: &str,
    manager_key: &str,
    job: &Job,
) -> Result<()> {
    let flags = Flags {
        profile: Some(job.profile.clone()),
        ..Flags::default()
    };
    match &job.instance {
        None => server_backup::backup(
            connector,
            server_name,
            home,
            manager_key,
            &server_backup::BackupOptions {
                shared: Vec::new(),
                flags,
                output: None,
                force_unlock: false,
            },
        ),
        Some((name, prefix)) => app_backup::backup(
            connector,
            server_name,
            home,
            manager_key,
            &Which::One(Selector::Name {
                name: name.clone(),
                prefix: prefix.clone(),
            }),
            &app_backup::Options {
                flags,
                output: None,
                stop: false,
                force_unlock: false,
            },
        ),
    }
}

/// A deploy or backup holds the lock: wait for it, up to half an hour.
fn wait_for_lock(ctx: &ServerCtx) -> Result<()> {
    let lock = ctx.path("lock");
    for _ in 0..360 {
        if !std::path::Path::new(&lock).exists() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(5));
    }
    bail!("the server has been locked for half an hour; trying again with the next tick")
}

/// Save the jobs' records under the lock, dropping those of jobs that are gone.
fn record(ctx: &ServerCtx, jobs: &[Job], updates: Vec<BackupRecord>) -> Result<()> {
    let current = |r: &BackupRecord| {
        jobs.iter()
            .any(|j| j.owner() == r.owner && j.profile == r.profile)
    };
    let mut lock = None;
    for _ in 0..60 {
        match Lock::acquire(ctx.connector, &ctx.home, false) {
            Ok(held) => {
                lock = Some(held);
                break;
            }
            Err(_) => std::thread::sleep(Duration::from_secs(5)),
        }
    }
    let _lock = lock.context("cannot record the scheduled backups: the server stays locked")?;
    let mut state = StateFile::load(ctx)?;
    let before = state.backups.clone();
    state.backups.retain(current);
    for update in updates {
        match state
            .backups
            .iter_mut()
            .find(|r| r.owner == update.owner && r.profile == update.profile)
        {
            Some(existing) => *existing = update,
            None => state.backups.push(update),
        }
    }
    if state.backups != before {
        state.save(ctx)?;
    }
    Ok(())
}

/// Keep the log's newer half once it is too big.
fn trim_log(path: &str) {
    let Ok(meta) = fs::metadata(path) else {
        if let Some((dir, _)) = path.rsplit_once('/') {
            let _ = fs::create_dir_all(dir);
        }
        return;
    };
    if meta.len() > LOG_LIMIT {
        if let Ok(text) = fs::read(path) {
            let half = &text[text.len() / 2..];
            let start = half.iter().position(|b| *b == b'\n').map_or(0, |i| i + 1);
            let _ = fs::write(path, &half[start..]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crontab_block_is_replaced_and_other_lines_kept() {
        let home = "/home/c/.compost-server";
        let mine = "0 1 * * * my-own-job\n";
        let once = replace_block(mine, home, Some("*/5 * * * * tick"));
        assert_eq!(
            once,
            "0 1 * * * my-own-job\n# compost: scheduled backups of /home/c/.compost-server\n*/5 * * * * tick\n# compost: end of /home/c/.compost-server\n"
        );
        assert_eq!(replace_block(&once, home, Some("*/5 * * * * tick")), once);
        let other = replace_block(&once, "/srv/other", Some("x"));
        assert!(other.contains("*/5 * * * * tick") && other.contains("of /srv/other"));
        assert_eq!(replace_block(&once, home, None), mine);
        assert_eq!(replace_block("", home, None), "");
    }

    #[test]
    fn cron_line_creates_the_log_folder_first() {
        let line = format_cron_line(
            "/usr/bin:/bin",
            "/h/bin/compost",
            "/h",
            "prod",
            "/h/logs/backup.log",
        );
        assert_eq!(
            line,
            "*/5 * * * * mkdir -p -- '/h/logs' && PATH='/usr/bin:/bin' '/h/bin/compost' backup tick --server-home '/h' --server-name 'prod' >> '/h/logs/backup.log' 2>&1"
        );
    }

    #[test]
    fn download_checks_the_checksum() {
        let script = download_script("1.4.0", "/h/bin/compost.new");
        assert!(script.contains("releases/download/v1.4.0"), "{script}");
        assert!(script.contains("compost-v1.4.0-$target"), "{script}");
        assert!(script.contains("checksum mismatch"), "{script}");
        assert!(
            script.contains("cp \"$dir/$name/compost\" '/h/bin/compost.new'"),
            "{script}"
        );
    }
}
