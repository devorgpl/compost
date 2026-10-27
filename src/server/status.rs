//! The daily `status` summary (see [`super::alerts`]): that the server is
//! alive, and how it is doing. A part that can't be read is left out with a
//! warning, so that the summary still goes out.

use std::collections::BTreeMap;

use anyhow::Result;
use chrono::{DateTime, Local};

use super::prune::{self, Disk};
use super::state::{self as server_state, Report, Status};
use super::state_file::{
    AlertRecord, BackupRecord, BackupStatus, CpuTimes, DeployStatus, StateFile,
};
use crate::backup::human_size;
use crate::config::server::ServerConfig;
use crate::notify::Severity;
use crate::shared::ingress::{self, crowdsec};
use crate::shared::{ServerCtx, project_name};

/// Uptime, load, cores, memory and CPU time, in this order.
const HOST_SCRIPT: &str = "cat /proc/uptime /proc/loadavg; nproc; \
    grep -E '^(MemTotal|MemAvailable):' /proc/meminfo; head -n 1 /proc/stat";

/// Each running container's compose project and start time.
const STARTS_SCRIPT: &str = "docker ps -q | xargs -r docker inspect --format \
    '{{index .Config.Labels \"com.docker.compose.project\"}} {{.State.StartedAt}}'";

/// What the host says about itself.
#[derive(Debug, Clone, PartialEq)]
pub struct Host {
    /// Seconds since boot.
    pub uptime: u64,
    /// Load average over 1, 5 and 15 minutes.
    pub load: [f64; 3],
    pub cores: u32,
    /// Bytes.
    pub memory_total: u64,
    pub memory_available: u64,
    pub cpu: CpuTimes,
}

/// Everything the summary tells.
pub struct Summary {
    pub report: Report,
    /// The alert records of `state.yaml`.
    pub alerts: Vec<AlertRecord>,
    /// Deployed instances, e.g. `myapp-prod`.
    pub instances: Vec<String>,
    pub host: Option<Host>,
    /// Average CPU utilisation in percent, and whether since boot (else since
    /// the last summary).
    pub cpu: Option<(f64, bool)>,
    /// The disk with the compost folder, and Docker's when it is another one.
    pub disks: Vec<(Disk, bool)>,
    /// Seconds since each compose project's newest container started.
    pub uptimes: BTreeMap<String, u64>,
    pub backups: Vec<BackupRecord>,
    /// The bans of the last 24 hours and those in force, with caddysecurebuild.
    pub bans: Option<String>,
}

/// Read what the summary tells. `cpu` are the counters of the last summary.
pub fn collect(
    ctx: &ServerCtx,
    config: &ServerConfig,
    manager_key: &str,
    cpu: Option<CpuTimes>,
) -> Result<Summary> {
    let report = server_state::check(ctx.connector, &ctx.home, manager_key)?;
    let state = StateFile::load(ctx)?;
    let host = warn("host figures", host(ctx)).flatten();
    let disks = warn("disk space", disks(ctx)).unwrap_or_default();
    let uptimes = warn(
        "container uptimes",
        ctx.connector
            .exec(STARTS_SCRIPT)
            .map(|text| uptimes(&text, Local::now())),
    )
    .unwrap_or_default();
    Ok(Summary {
        report,
        alerts: state.alerts,
        instances: state
            .apps
            .iter()
            .filter(|r| r.status == DeployStatus::Deployed)
            .map(|r| format!("{}-{}", r.name, r.prefix))
            .collect(),
        cpu: host.as_ref().and_then(|h| cpu_average(cpu, h.cpu)),
        host,
        disks,
        uptimes,
        backups: state.backups,
        bans: warn("bans", bans(ctx, config)).flatten(),
    })
}

fn warn<T>(what: &str, result: Result<T>) -> Option<T> {
    result
        .map_err(|err| eprintln!("warning: status: cannot read the {what}: {err:#}"))
        .ok()
}

fn host(ctx: &ServerCtx) -> Result<Option<Host>> {
    Ok(parse_host(&ctx.connector.exec(HOST_SCRIPT)?))
}

fn parse_host(text: &str) -> Option<Host> {
    let lines: Vec<&str> = text.lines().collect();
    let uptime = lines
        .first()?
        .split_whitespace()
        .next()?
        .parse::<f64>()
        .ok()?;
    let load: Vec<f64> = lines
        .get(1)?
        .split_whitespace()
        .take(3)
        .filter_map(|f| f.parse().ok())
        .collect();
    let memory = |key: &str| {
        lines
            .iter()
            .find_map(|l| l.strip_prefix(key))
            .and_then(|rest| rest.split_whitespace().next()?.parse::<u64>().ok())
            .map(|kb| kb * 1024)
    };
    // user nice system idle iowait irq softirq steal (guest is in user).
    let ticks: Vec<u64> = lines
        .iter()
        .find_map(|l| l.strip_prefix("cpu "))?
        .split_whitespace()
        .take(8)
        .filter_map(|f| f.parse().ok())
        .collect();
    Some(Host {
        uptime: uptime as u64,
        load: load.try_into().ok()?,
        cores: lines.get(2)?.trim().parse().ok()?,
        memory_total: memory("MemTotal:")?,
        memory_available: memory("MemAvailable:")?,
        cpu: CpuTimes {
            total: ticks.iter().sum(),
            idle: ticks.get(3)? + ticks.get(4).unwrap_or(&0),
        },
    })
}

/// Busy time in percent since `last`, or since boot when there is no `last`
/// or the host rebooted since.
fn cpu_average(last: Option<CpuTimes>, now: CpuTimes) -> Option<(f64, bool)> {
    let busy = |total: u64, idle: u64| 100.0 * (1.0 - idle as f64 / total as f64);
    match last {
        Some(last) if now.total > last.total && now.idle >= last.idle => {
            Some((busy(now.total - last.total, now.idle - last.idle), false))
        }
        _ if now.total > 0 => Some((busy(now.total, now.idle), true)),
        _ => None,
    }
}

/// The disk with the compost folder, and Docker's data root when it is on
/// another one (`true`).
fn disks(ctx: &ServerCtx) -> Result<Vec<(Disk, bool)>> {
    let mut disks = Vec::new();
    let Some(home) = prune::disk(ctx)? else {
        return Ok(disks);
    };
    // Without Docker's data root, the compost folder's disk is still worth telling.
    let root = ctx
        .connector
        .exec("docker info --format '{{.DockerRootDir}}'")
        .unwrap_or_default();
    let docker = match root.trim() {
        "" => None,
        root => prune::disk_of(ctx, root)?,
    };
    disks.push((home, false));
    if let Some(docker) = docker.filter(|d| d.mount != disks[0].0.mount) {
        disks.push((docker, true));
    }
    Ok(disks)
}

/// Seconds since each project's newest container started: a restart shows.
fn uptimes(text: &str, now: DateTime<Local>) -> BTreeMap<String, u64> {
    let mut started: BTreeMap<String, DateTime<Local>> = BTreeMap::new();
    for line in text.lines() {
        let Some((project, time)) = line.trim().rsplit_once(' ') else {
            continue;
        };
        let Ok(time) = DateTime::parse_from_rfc3339(time) else {
            continue;
        };
        let time = time.with_timezone(&Local);
        if project.is_empty() {
            continue;
        }
        let entry = started.entry(project.to_string()).or_insert(time);
        *entry = (*entry).max(time);
    }
    started
        .into_iter()
        .map(|(project, time)| {
            let seconds = (now - time).num_seconds().max(0) as u64;
            (project, seconds)
        })
        .collect()
}

fn bans(ctx: &ServerCtx, config: &ServerConfig) -> Result<Option<String>> {
    let Some(ingress) = config.shared.get(ingress::KIND).and_then(|apps| {
        apps.iter()
            .find(|app| app.type_name == "caddysecurebuild" && !app.is_external())
    }) else {
        return Ok(None);
    };
    let name = &ingress.name;
    if !server_state::is_project_running(ctx.connector, &project_name(ingress::KIND, name))? {
        return Ok(None);
    }
    let alerts = crowdsec::alerts(ctx, name, "24h")?;
    let bans = crowdsec::active_bans(ctx, name)?;
    let daily = crowdsec::daily_message(&alerts, &bans, name);
    Ok(Some(format!(
        "{}; in force: {}",
        daily.title,
        bans.describe()
    )))
}

/// e.g. `3 days`, `1 hour`, `12 minutes`.
fn duration(seconds: u64) -> String {
    let (n, unit) = match seconds {
        s if s >= 86_400 => (s / 86_400, "day"),
        s if s >= 3_600 => (s / 3_600, "hour"),
        s => (s / 60, "minute"),
    };
    format!("{n} {unit}{}", if n == 1 { "" } else { "s" })
}

/// RFC 3339 → `2026-10-18 03:00`, in the server's time.
fn short_time(time: &str) -> String {
    time.get(..16).unwrap_or(time).replace('T', " ")
}

fn first_line(text: Option<&str>) -> &str {
    text.and_then(|t| t.lines().next()).unwrap_or_default()
}

/// What an alert record is about, e.g. `myapp-prod is not healthy`.
fn describe_alert(key: &str) -> String {
    match key.split_once(':') {
        None if key == "disk" => "Disk space low".to_string(),
        Some(("health", "server")) => "Shared apps are down".to_string(),
        Some(("health", owner)) => format!("{owner} is not healthy"),
        Some(("backup", rest)) => match rest.split_once(':') {
            Some(("server", profile)) => {
                format!("Scheduled backup failed: profile {profile} of the shared apps")
            }
            Some((owner, profile)) => {
                format!("Scheduled backup failed: profile {profile} of {owner}")
            }
            None => format!("Scheduled backup failed: {rest}"),
        },
        _ => key.to_string(),
    }
}

fn describe_backup(record: &BackupRecord) -> String {
    let owner = if record.owner == "server" {
        "shared apps"
    } else {
        &record.owner
    };
    let time = short_time(&record.last_run);
    let result = match record.status {
        BackupStatus::Waiting => "waiting for its first run".to_string(),
        BackupStatus::Failed => format!("FAILED {time}: {}", first_line(record.error.as_deref())),
        BackupStatus::Ok => {
            let mut text = format!("ok {time}");
            if let Some(size) = record.size {
                text.push_str(&format!(", {}", human_size(size)));
            }
            if let Some(target) = &record.target {
                text.push_str(&format!(" → {target}"));
            }
            text
        }
    };
    format!("  {owner} {}: {result}", record.profile)
}

/// The severity, title and body of the summary.
pub fn message(summary: &Summary) -> (Severity, String, String) {
    let report = &summary.report;
    let notified: Vec<&AlertRecord> = summary.alerts.iter().filter(|r| r.alerting).collect();
    let fine = report.status == Status::Ok && notified.is_empty();
    let severity = if fine {
        Severity::Info
    } else {
        Severity::Warning
    };
    let title = match notified.len() {
        0 => format!("Daily status: {}", report.status),
        1 => format!("Daily status: {}, 1 alert", report.status),
        n => format!("Daily status: {}, {n} alerts", report.status),
    };

    let mut lines = Vec::new();
    match &report.detail {
        Some(detail) if !detail.is_empty() => {
            lines.push(format!("Server: {}: {detail}", report.status));
        }
        _ => lines.push(format!("Server: {}", report.status)),
    }
    if !notified.is_empty() {
        lines.push("Alerts (notified):".to_string());
        for record in &notified {
            let mut line = format!(
                "  {} (since {})",
                describe_alert(&record.key),
                short_time(&record.since)
            );
            let detail = first_line(record.detail.as_deref());
            if !detail.is_empty() {
                line.push_str(&format!(": {detail}"));
            }
            lines.push(line);
        }
    }

    if let Some(host) = &summary.host {
        lines.push(format!("Uptime: {}", duration(host.uptime)));
        let [one, five, fifteen] = host.load;
        let mut cpu = format!(
            "CPU: load {one:.2} {five:.2} {fifteen:.2} on {} cores",
            host.cores
        );
        if let Some((average, since_boot)) = summary.cpu {
            let since = if since_boot {
                "since boot"
            } else {
                "since the last status"
            };
            cpu.push_str(&format!(", {average:.0}% busy on average {since}"));
        }
        lines.push(cpu);
        lines.push(format!(
            "Memory: {} used of {}, {} available",
            human_size(host.memory_total.saturating_sub(host.memory_available)),
            human_size(host.memory_total),
            human_size(host.memory_available)
        ));
    }
    for (disk, docker) in &summary.disks {
        if *docker {
            lines.push(format!("{} (Docker's data)", disk.describe()));
        } else {
            lines.push(disk.describe());
        }
    }

    let up = |project: &str| {
        summary
            .uptimes
            .get(project)
            .map(|&s| format!(", up {}", duration(s)))
            .unwrap_or_default()
    };
    if !report.shared.is_empty() {
        lines.push("Shared apps:".to_string());
        for app in &report.shared {
            let state = match (app.external, app.on_demand, app.running) {
                (true, _, _) => "external".to_string(),
                (_, true, _) => "on demand".to_string(),
                (_, _, true) => format!("running{}", up(&project_name(&app.kind, &app.name))),
                _ => "NOT RUNNING".to_string(),
            };
            lines.push(format!("  {} {}: {state}", app.kind, app.name));
        }
    }
    if !summary.instances.is_empty() {
        lines.push("Instances:".to_string());
        for name in &summary.instances {
            let record = summary
                .alerts
                .iter()
                .find(|r| r.key == format!("health:{name}"));
            let state = match record {
                Some(r) if r.alerting || r.bad > 0 => {
                    format!("NOT HEALTHY: {}", first_line(r.detail.as_deref()))
                }
                Some(_) => format!("healthy{}", up(name)),
                None => "not checked yet".to_string(),
            };
            lines.push(format!("  {name}: {state}"));
        }
    }
    if !summary.backups.is_empty() {
        lines.push("Scheduled backups (last run):".to_string());
        lines.extend(summary.backups.iter().map(describe_backup));
    }
    if let Some(bans) = &summary.bans {
        lines.push(format!("Bans: {bans}"));
    }
    (severity, title, lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::state::SharedAppState;

    const PROC: &str = "1036800.52 4000000.10
0.42 0.38 0.35 1/234 5678
4
MemTotal:        8000000 kB
MemAvailable:    6000000 kB
cpu  1000 10 500 8000 200 0 50 0 0 0
";

    #[test]
    fn reads_the_host() {
        let host = parse_host(PROC).unwrap();
        assert_eq!(host.uptime, 1_036_800);
        assert_eq!(host.load, [0.42, 0.38, 0.35]);
        assert_eq!(host.cores, 4);
        assert_eq!(host.memory_total, 8_192_000_000);
        assert_eq!(host.memory_available, 6_144_000_000);
        assert_eq!(
            host.cpu,
            CpuTimes {
                total: 9760,
                idle: 8200
            }
        );
        assert_eq!(parse_host("nothing"), None);
    }

    #[test]
    fn cpu_average_since_the_last_summary_or_boot() {
        let last = CpuTimes {
            total: 1000,
            idle: 900,
        };
        let now = CpuTimes {
            total: 2000,
            idle: 1700,
        };
        let (average, since_boot) = cpu_average(Some(last), now).unwrap();
        assert!((average - 20.0).abs() < 1e-9);
        assert!(!since_boot);
        // Rebooted since: the counters started again.
        let rebooted = CpuTimes {
            total: 500,
            idle: 400,
        };
        let (average, since_boot) = cpu_average(Some(now), rebooted).unwrap();
        assert!((average - 20.0).abs() < 1e-9);
        assert!(since_boot);
        assert_eq!(cpu_average(None, CpuTimes::default()), None);
    }

    #[test]
    fn uptime_of_the_newest_container() {
        let now = DateTime::parse_from_rfc3339("2026-10-27T08:00:00Z")
            .unwrap()
            .with_timezone(&Local);
        let text = "myapp-prod 2026-10-24T08:00:00.123456789Z
myapp-prod 2026-10-27T06:00:00Z
compost-ingress-caddy-main 2026-10-27T07:48:00Z
 2026-10-27T07:00:00Z
broken line
";
        let uptimes = uptimes(text, now);
        assert_eq!(uptimes.get("myapp-prod"), Some(&7200));
        assert_eq!(uptimes.get("compost-ingress-caddy-main"), Some(&720));
        assert_eq!(uptimes.len(), 2);
        assert_eq!(duration(7200), "2 hours");
        assert_eq!(duration(720), "12 minutes");
        assert_eq!(duration(86_400), "1 day");
    }

    fn summary() -> Summary {
        Summary {
            report: Report {
                status: Status::Ok,
                detail: None,
                docker: None,
                home: None,
                shared: vec![
                    SharedAppState {
                        kind: "ingress".into(),
                        name: "caddy-main".into(),
                        type_name: "caddy".into(),
                        version: None,
                        running: true,
                        external: false,
                        on_demand: false,
                        details: Vec::new(),
                    },
                    SharedAppState {
                        kind: "communication".into(),
                        name: "chat".into(),
                        type_name: "matrix".into(),
                        version: None,
                        running: true,
                        external: false,
                        on_demand: true,
                        details: Vec::new(),
                    },
                ],
            },
            alerts: vec![AlertRecord {
                key: "health:myapp-prod".into(),
                alerting: false,
                bad: 0,
                since: "2026-10-20T10:00:00+02:00".into(),
                detail: None,
            }],
            instances: vec!["myapp-prod".into()],
            host: parse_host(PROC),
            cpu: Some((7.4, false)),
            disks: vec![(
                Disk {
                    mount: "/".into(),
                    total: 80_000_000_000,
                    used: 40_000_000_000,
                    available: 40_000_000_000,
                },
                false,
            )],
            uptimes: BTreeMap::from([
                (project_name("ingress", "caddy-main"), 3 * 86_400),
                ("myapp-prod".into(), 7200),
            ]),
            backups: vec![BackupRecord {
                owner: "myapp-prod".into(),
                profile: "daily".into(),
                last_run: "2026-10-27T03:30:00+01:00".into(),
                status: BackupStatus::Ok,
                error: None,
                size: Some(15_600_000),
                target: Some("s3".into()),
            }],
            bans: None,
        }
    }

    #[test]
    fn summary_when_all_is_fine() {
        let (severity, title, body) = message(&summary());
        assert_eq!(severity, Severity::Info);
        assert_eq!(title, "Daily status: OK");
        assert_eq!(
            body,
            "Server: OK
Uptime: 12 days
CPU: load 0.42 0.38 0.35 on 4 cores, 7% busy on average since the last status
Memory: 2.0 GB used of 8.2 GB, 6.1 GB available
Disk /: 40.0 GB free of 80.0 GB (50% used)
Shared apps:
  ingress caddy-main: running, up 3 days
  communication chat: on demand
Instances:
  myapp-prod: healthy, up 2 hours
Scheduled backups (last run):
  myapp-prod daily: ok 2026-10-27 03:30, 15.6 MB → s3"
        );
    }

    #[test]
    fn summary_with_alerts() {
        let mut summary = summary();
        summary.alerts = vec![
            AlertRecord {
                key: "health:myapp-prod".into(),
                alerting: true,
                bad: 3,
                since: "2026-10-27T06:10:00+01:00".into(),
                detail: Some("web: exited 1\nworker: exited 1".into()),
            },
            AlertRecord {
                key: "backup:server:daily".into(),
                alerting: true,
                bad: 1,
                since: "2026-10-27T03:00:00+01:00".into(),
                detail: Some("upload failed\nmore".into()),
            },
        ];
        summary.backups[0].status = BackupStatus::Failed;
        summary.backups[0].error = Some("disk full\nmore".into());
        summary.bans = Some("No addresses banned in the last 24h; in force: none".into());
        let (severity, title, body) = message(&summary);
        assert_eq!(severity, Severity::Warning);
        assert_eq!(title, "Daily status: OK, 2 alerts");
        assert!(
            body.contains(
                "Alerts (notified):
  myapp-prod is not healthy (since 2026-10-27 06:10): web: exited 1
  Scheduled backup failed: profile daily of the shared apps (since 2026-10-27 03:00): upload failed"
            ),
            "{body}"
        );
        assert!(
            body.contains("  myapp-prod: NOT HEALTHY: web: exited 1"),
            "{body}"
        );
        assert!(
            body.contains("  myapp-prod daily: FAILED 2026-10-27 03:30: disk full"),
            "{body}"
        );
        assert!(
            body.ends_with("Bans: No addresses banned in the last 24h; in force: none"),
            "{body}"
        );
    }
}
