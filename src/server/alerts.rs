//! What the server reports by itself (`notifications` in compost-server.yaml):
//! failed scheduled backups, shared apps and instances that are down, and low
//! disk space. The cron tick checks every few minutes; a notification goes out
//! when something breaks and when it is fine again, never on every check.

use std::time::Duration;

use anyhow::{Context, Result};

use super::lock::Lock;
use super::prune;
use super::state::{self as server_state, Status};
use super::state_file::{AlertRecord, BackupStatus, StateFile};
use crate::app::{instance, state as app_state};
use crate::config::server::Event;
use crate::notify::{self, Notification, Severity};
use crate::server::state_file::DeployStatus;
use crate::shared::ServerCtx;

/// Free disk space must rise this many points above the threshold before
/// `disk` is fine again, so that it doesn't flap around the threshold.
const DISK_MARGIN: f64 = 2.0;
/// Bad health checks in a row before an alert: a container restarting
/// shouldn't wake anyone.
const HEALTH_CHECKS: u32 = 2;

/// One thing to watch, as found by this check.
#[derive(Debug, Clone)]
pub struct Check {
    pub key: String,
    pub event: Event,
    /// Whether it is a problem now.
    pub bad: bool,
    /// Whether it is still a problem when an alert is out (for a margin).
    pub still_bad: bool,
    /// Bad checks in a row before the alert.
    pub needed: u32,
    pub detail: String,
    pub severity: Severity,
    pub alert_title: String,
    pub ok_title: String,
}

/// The record after `check`, and what to send, if anything.
pub fn step(
    record: Option<&AlertRecord>,
    check: &Check,
    now: &str,
) -> (AlertRecord, Option<(Severity, String)>) {
    let alerting = record.is_some_and(|r| r.alerting);
    let bad = if alerting { check.still_bad } else { check.bad };
    let since = |changed: bool| match record {
        Some(r) if !changed => r.since.clone(),
        _ => now.to_string(),
    };
    let detail = (!check.detail.is_empty()).then(|| check.detail.clone());
    match (alerting, bad) {
        (true, true) => (
            AlertRecord {
                key: check.key.clone(),
                alerting: true,
                bad: record.map_or(1, |r| r.bad.saturating_add(1)),
                since: since(false),
                detail,
            },
            None,
        ),
        (true, false) => (
            AlertRecord {
                key: check.key.clone(),
                alerting: false,
                bad: 0,
                since: since(true),
                detail: None,
            },
            Some((Severity::Ok, check.ok_title.clone())),
        ),
        (false, true) => {
            let count = record.map_or(0, |r| r.bad) + 1;
            let fire = count >= check.needed;
            (
                AlertRecord {
                    key: check.key.clone(),
                    alerting: fire,
                    bad: count,
                    since: since(fire || count == 1),
                    detail,
                },
                fire.then(|| (check.severity, check.alert_title.clone())),
            )
        }
        (false, false) => (
            AlertRecord {
                key: check.key.clone(),
                alerting: false,
                bad: 0,
                since: since(record.is_some_and(|r| r.bad > 0) || record.is_none()),
                detail: None,
            },
            None,
        ),
    }
}

/// Run the checks and send what changed. Called by the tick.
pub fn evaluate(ctx: &ServerCtx, server_name: &str, manager_key: &str) -> Result<()> {
    let config = instance::server_config(ctx)?;
    let Some(notifications) = config
        .notifications
        .as_ref()
        .filter(|n| !n.send_to.is_empty())
    else {
        return Ok(());
    };
    let state = StateFile::load(ctx)?;
    let mut checks = backup_checks(&state);
    if let Some(disk) = prune::disk(ctx)? {
        let threshold = f64::from(notifications.disk_free_below);
        let free = disk.free_percent();
        checks.push(Check {
            key: "disk".into(),
            event: Event::Disk,
            bad: free < threshold,
            still_bad: free < threshold + DISK_MARGIN,
            needed: 1,
            detail: format!(
                "{}\n`compost server prune --dry-run` and `compost backup prune` show what can go.",
                disk.describe()
            ),
            severity: Severity::Warning,
            alert_title: format!("Disk space low: {free:.0}% free on {}", disk.mount),
            ok_title: format!(
                "Disk space is fine again: {free:.0}% free on {}",
                disk.mount
            ),
        });
    }
    // A deploy takes containers down for a moment: no health checks meanwhile.
    let deploying = std::path::Path::new(&ctx.path("lock")).exists();
    if !deploying {
        checks.extend(health_checks(ctx, manager_key, &state)?);
    }

    let now = chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false);
    let _lock = lock(ctx)?;
    let mut state = StateFile::load(ctx)?;
    let mut records = Vec::new();
    let mut outgoing = Vec::new();
    for check in &checks {
        let previous = state.alerts.iter().find(|r| r.key == check.key);
        let (record, send) = step(previous, check, &now);
        if let Some((severity, title)) = send {
            outgoing.push((check.event, severity, title, check.detail.clone()));
        }
        records.push(record);
    }
    // Keep the health records while their checks were skipped.
    if deploying {
        records.extend(
            state
                .alerts
                .iter()
                .filter(|r| r.key.starts_with("health:"))
                .cloned(),
        );
    }
    if records != state.alerts {
        state.alerts = records;
        state.save(ctx)?;
    }
    drop(_lock);

    for (event, severity, title, detail) in outgoing {
        let body = if severity == Severity::Ok {
            String::new()
        } else {
            detail
        };
        notify::send_event(
            ctx,
            notifications,
            event,
            &Notification {
                event: event.name().to_string(),
                severity,
                title,
                body,
                server: server_name.to_string(),
            },
        );
    }
    Ok(())
}

/// The last result of every scheduled backup that ran.
fn backup_checks(state: &StateFile) -> Vec<Check> {
    state
        .backups
        .iter()
        .filter(|r| r.status != BackupStatus::Waiting)
        .map(|record| {
            let what = if record.owner == "server" {
                format!("profile {} of the shared apps", record.profile)
            } else {
                format!("profile {} of {}", record.profile, record.owner)
            };
            let failed = record.status == BackupStatus::Failed;
            Check {
                key: format!("backup:{}:{}", record.owner, record.profile),
                event: Event::Backup,
                bad: failed,
                still_bad: failed,
                needed: 1,
                detail: format!(
                    "{}\nThe output is in ~/.compost-server/logs/backup.log on the server.",
                    record.error.as_deref().unwrap_or("no details")
                ),
                severity: Severity::Error,
                alert_title: format!("Scheduled backup failed: {what}"),
                ok_title: format!("Scheduled backup works again: {what}"),
            }
        })
        .collect()
}

/// The shared apps together, and every deployed instance.
fn health_checks(ctx: &ServerCtx, manager_key: &str, state: &StateFile) -> Result<Vec<Check>> {
    let mut checks = Vec::new();
    let report = server_state::check(ctx.connector, &ctx.home, manager_key)?;
    let down: Vec<String> = match report.status {
        Status::Ok | Status::Degraded => report
            .shared
            .iter()
            .filter(|app| !app.running)
            .map(|app| format!("{} {}: not running", app.kind, app.name))
            .collect(),
        status => vec![format!(
            "server is {status}: {}",
            report.detail.unwrap_or_default()
        )],
    };
    checks.push(health_check(
        "server",
        &down,
        "Shared apps are down",
        "Shared apps run again",
    ));
    for record in state
        .apps
        .iter()
        .filter(|r| r.status == DeployStatus::Deployed)
    {
        let Some(found) = instance::load(ctx, &record.name, &record.prefix)? else {
            continue;
        };
        let name = found.app.instance();
        let services =
            app_state::services(ctx, &found).with_context(|| format!("checking {name}"))?;
        let problems: Vec<String> = if services.is_empty() {
            vec!["no containers".to_string()]
        } else if app_state::all_healthy(&services) {
            Vec::new()
        } else {
            services
                .iter()
                .filter(|s| !s.healthy)
                .map(|s| format!("{}: {}", s.name, s.state))
                .collect()
        };
        checks.push(health_check(
            &name,
            &problems,
            &format!("{name} is not healthy"),
            &format!("{name} is healthy again"),
        ));
    }
    Ok(checks)
}

fn health_check(owner: &str, problems: &[String], alert: &str, ok: &str) -> Check {
    Check {
        key: format!("health:{owner}"),
        event: Event::Health,
        bad: !problems.is_empty(),
        still_bad: !problems.is_empty(),
        needed: HEALTH_CHECKS,
        detail: problems.join("\n"),
        severity: Severity::Error,
        alert_title: alert.to_string(),
        ok_title: ok.to_string(),
    }
}

/// The server's lock, waiting up to five minutes for it.
fn lock<'c>(ctx: &ServerCtx<'c>) -> Result<Lock<'c>> {
    for _ in 0..60 {
        if let Ok(lock) = Lock::acquire(ctx.connector, &ctx.home, false) {
            return Ok(lock);
        }
        std::thread::sleep(Duration::from_secs(5));
    }
    anyhow::bail!("cannot record the alerts: the server stays locked")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(bad: bool, still_bad: bool, needed: u32) -> Check {
        Check {
            key: "k".into(),
            event: Event::Health,
            bad,
            still_bad,
            needed,
            detail: "d".into(),
            severity: Severity::Error,
            alert_title: "down".into(),
            ok_title: "up".into(),
        }
    }

    /// Run the checks in order; returns what was sent.
    fn run(checks: &[Check]) -> Vec<Option<String>> {
        let mut record: Option<AlertRecord> = None;
        checks
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let (next, send) = step(record.as_ref(), c, &format!("t{i}"));
                record = Some(next);
                send.map(|(severity, title)| format!("{severity} {title}"))
            })
            .collect()
    }

    #[test]
    fn sends_on_changes_only() {
        let bad = check(true, true, 1);
        let good = check(false, false, 1);
        assert_eq!(
            run(&[good.clone(), bad.clone(), bad.clone(), good.clone(), good]),
            [
                None,
                Some("error down".into()),
                None,
                Some("ok up".into()),
                None
            ]
        );
    }

    #[test]
    fn waits_for_checks_in_a_row() {
        let bad = check(true, true, 2);
        let good = check(false, false, 2);
        // One bad check alone is forgotten.
        assert_eq!(
            run(&[
                bad.clone(),
                good.clone(),
                bad.clone(),
                bad.clone(),
                bad,
                good
            ]),
            [
                None,
                None,
                None,
                Some("error down".into()),
                None,
                Some("ok up".into())
            ]
        );
    }

    #[test]
    fn margin_before_it_is_fine_again() {
        // Below the threshold, then just above it (still inside the margin), then well above.
        let below = check(true, true, 1);
        let inside = check(false, true, 1);
        let above = check(false, false, 1);
        assert_eq!(
            run(&[below, inside.clone(), above, inside]),
            [Some("error down".into()), None, Some("ok up".into()), None]
        );
    }

    #[test]
    fn since_marks_the_change() {
        let (first, _) = step(None, &check(true, true, 1), "t1");
        let (same, _) = step(Some(&first), &check(true, true, 1), "t2");
        assert_eq!(same.since, "t1");
        let (fixed, _) = step(Some(&same), &check(false, false, 1), "t3");
        assert_eq!(fixed.since, "t3");
    }
}
