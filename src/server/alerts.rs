//! What the server reports by itself (`notifications` in compost-server.yaml):
//! failed scheduled backups, shared apps and instances that are down, and low
//! disk space. The cron tick checks every few minutes; a notification goes out
//! when something breaks and when it is fine again, never on every check.
//! Besides, CrowdSec's new bans, and a daily summary of them (see
//! [`report_bans`]).

use std::time::Duration;

use anyhow::{Context, Result};

use super::lock::Lock;
use super::prune;
use super::state::{self as server_state, Status};
use super::state_file::{AlertRecord, BackupStatus, StateFile};
use crate::app::{instance, state as app_state};
use crate::config::server::{Event, Notifications, ServerConfig};
use crate::notify::{self, Notification, Severity};
use crate::server::state_file::DeployStatus;
use crate::shared::ingress::{self, crowdsec};
use crate::shared::{ServerCtx, project_name};

/// Free disk space must rise this many points above the threshold before
/// `disk` is fine again, so that it doesn't flap around the threshold.
const DISK_MARGIN: f64 = 2.0;
/// Bad health checks in a row before an alert: a container restarting
/// shouldn't wake anyone.
const HEALTH_CHECKS: u32 = 2;
/// Addresses whose earlier bans a check looks up, at most.
const HISTORIES: usize = 50;

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
        send(
            ctx,
            notifications,
            server_name,
            event,
            severity,
            title,
            body,
        );
    }
    if let Err(err) = report_bans(ctx, server_name, &config, notifications) {
        eprintln!("error: bans notifications: {err:#}");
    }
    Ok(())
}

fn send(
    ctx: &ServerCtx,
    notifications: &Notifications,
    server_name: &str,
    event: Event,
    severity: Severity,
    title: String,
    body: String,
) {
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

/// `bans`: the alerts CrowdSec raised since the last check that banned an
/// address, or would have. `bans_daily`: a summary of the last 24 hours, at
/// `daily_at`. Only recipients that ask for them get them.
fn report_bans(
    ctx: &ServerCtx,
    server_name: &str,
    config: &ServerConfig,
    notifications: &Notifications,
) -> Result<()> {
    let per_check = notifications.wants(Event::Bans);
    let daily = notifications.wants(Event::BansDaily);
    if !per_check && !daily {
        return Ok(());
    }
    let Some(ingress) = config.shared.get(ingress::KIND).and_then(|apps| {
        apps.iter()
            .find(|app| app.type_name == "caddysecurebuild" && !app.is_external())
    }) else {
        return Ok(());
    };
    let name = &ingress.name;
    if !server_state::is_project_running(ctx.connector, &project_name(ingress::KIND, name))? {
        return Ok(());
    }

    let record = StateFile::load(ctx)?.bans.unwrap_or_default();
    let mut updated = record.clone();
    let mut outgoing = Vec::new();
    if per_check {
        // An hour back covers the checks missed while the server was down.
        let mut alerts = crowdsec::alerts(ctx, name, "1h")?;
        alerts.sort_by_key(|alert| alert.id);
        let newest = alerts.last().map(|alert| alert.id);
        updated.last_alert = Some(match (record.last_alert, newest) {
            // The first check: what happened before isn't news.
            (None, newest) => newest.unwrap_or(0),
            (Some(last), newest) => {
                // Lower ids than reported before: CrowdSec's database was reset.
                let last = if newest.is_some_and(|n| n < last) {
                    0
                } else {
                    last
                };
                let new: Vec<_> = alerts
                    .iter()
                    .filter(|alert| alert.id > last && alert.is_ban())
                    .collect();
                // A banned scanner that keeps going sets off more scenarios,
                // with its requests answered 403: that isn't news.
                // One cscli call per address: in a burst, the rest are listed.
                let mut histories = std::collections::BTreeMap::new();
                for alert in &new {
                    let address = alert.source.value.as_str();
                    if !histories.contains_key(address) && histories.len() < HISTORIES {
                        histories.insert(address, crowdsec::alerts_of(ctx, name, address)?);
                    }
                }
                let new: Vec<_> = new
                    .into_iter()
                    .filter(|alert| {
                        histories
                            .get(alert.source.value.as_str())
                            .is_none_or(|history| !alert.banned_before(history, last))
                    })
                    .collect();
                if let Some(message) = crowdsec::bans_message(&new, name) {
                    let severity = if message.banned {
                        Severity::Warning
                    } else {
                        Severity::Info
                    };
                    outgoing.push((Event::Bans, severity, message));
                }
                newest.unwrap_or(last).max(last)
            }
        });
    }
    if daily {
        if let Some(due) = daily_due(notifications, record.last_daily.as_deref()) {
            let alerts = crowdsec::alerts(ctx, name, "24h")?;
            let bans = crowdsec::active_bans(ctx, name)?;
            let message = crowdsec::daily_message(&alerts, &bans, name);
            outgoing.push((Event::BansDaily, Severity::Info, message));
            updated.last_daily = Some(due);
        }
    }

    if updated != record {
        let _lock = lock(ctx)?;
        let mut state = StateFile::load(ctx)?;
        state.bans = Some(updated);
        state.save(ctx)?;
    }
    for (event, severity, message) in outgoing {
        send(
            ctx,
            notifications,
            server_name,
            event,
            severity,
            message.title,
            message.body,
        );
    }
    Ok(())
}

/// The date of the last `daily_at` that passed, if its summary wasn't sent:
/// today's once the time has come, else yesterday's (a check can come just
/// after midnight for `23:58`).
fn daily_due(notifications: &Notifications, last: Option<&str>) -> Option<String> {
    daily_slot(
        notifications.daily_time()?,
        chrono::Local::now().naive_local(),
        last,
    )
}

fn daily_slot(
    at: chrono::NaiveTime,
    now: chrono::NaiveDateTime,
    last: Option<&str>,
) -> Option<String> {
    let date = if now.time() >= at {
        now.date()
    } else {
        now.date().pred_opt()?
    };
    let slot = date.format("%Y-%m-%d").to_string();
    (last != Some(slot.as_str())).then_some(slot)
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

    #[test]
    fn daily_summary_is_due_once_per_slot() {
        let at = chrono::NaiveTime::from_hms_opt(8, 0, 0).unwrap();
        let time =
            |text: &str| chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M").unwrap();
        // Before 08:00, the slot is yesterday's.
        assert_eq!(
            daily_slot(at, time("2026-05-04 07:55"), None).as_deref(),
            Some("2026-05-03")
        );
        assert_eq!(
            daily_slot(at, time("2026-05-04 07:55"), Some("2026-05-03")),
            None
        );
        assert_eq!(
            daily_slot(at, time("2026-05-04 08:00"), Some("2026-05-03")).as_deref(),
            Some("2026-05-04")
        );
        assert_eq!(
            daily_slot(at, time("2026-05-04 23:00"), Some("2026-05-04")),
            None
        );
        // 23:58 with checks every 5 minutes: sent just after midnight.
        let late = chrono::NaiveTime::from_hms_opt(23, 58, 0).unwrap();
        assert_eq!(
            daily_slot(late, time("2026-05-05 00:00"), Some("2026-05-03")).as_deref(),
            Some("2026-05-04")
        );
        assert_eq!(
            daily_slot(late, time("2026-05-05 00:05"), Some("2026-05-04")),
            None
        );
    }
}
