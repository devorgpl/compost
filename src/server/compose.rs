//! Container status of docker compose projects.

use std::collections::HashMap;
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::connector::{Connector, shell_quote};

/// How long to wait for a project to be running (and healthy).
const START_TIMEOUT: Duration = Duration::from_secs(60);
const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// One `<service> <state> <exit code> <health>` line per container.
const PS_FORMAT: &str = "'{{.Service}} {{.State}} {{.ExitCode}} {{.Health}}'";
/// One `<service> <restart count> <started at>` line per running container.
const UPTIME_FORMAT: &str = r#"'{{index .Config.Labels "com.docker.compose.service"}} {{.RestartCount}} {{.State.StartedAt}}'"#;
/// A container without a healthcheck looks running until it exits, e.g.
/// Caddy on a configuration it rejects, which Docker then restarts: it is
/// ready once it has run this long.
const STABLE_AFTER: Duration = Duration::from_secs(5);
/// Log lines shown of a service that restarted while compost waited.
const LOG_LINES: u32 = 20;

#[derive(Debug, Clone)]
pub struct ContainerStatus {
    pub service: String,
    /// e.g. `running`, `exited`, `restarting`
    pub state: String,
    pub exit_code: String,
    /// `healthy`, `unhealthy`, `starting`, or empty without a healthcheck.
    pub health: String,
}

impl ContainerStatus {
    fn parse(line: &str) -> Self {
        let mut fields = line.split_whitespace().map(str::to_string);
        let mut next = || fields.next().unwrap_or_default();
        Self {
            service: next(),
            state: next(),
            exit_code: next(),
            health: next(),
        }
    }

    /// Running (and healthy, if it has a healthcheck), or finished successfully
    /// like a migration.
    pub fn is_ready(&self) -> bool {
        match self.state.as_str() {
            "running" => self.health.is_empty() || self.health == "healthy",
            "exited" => self.exit_code == "0",
            _ => false,
        }
    }

    /// Like `is_ready`, but a service that should keep running is not ready
    /// once it exited, whatever the exit code (e.g. after `docker stop`).
    pub fn is_healthy(&self, keeps_running: bool) -> bool {
        self.is_ready() && !(keeps_running && self.state == "exited")
    }

    fn failed(&self) -> bool {
        self.state == "exited" && self.exit_code != "0"
    }

    /// e.g. `running healthy`, `exited 0 (finished)`, `exited 1`
    pub fn describe(&self, keeps_running: bool) -> String {
        match self.state.as_str() {
            "exited" if keeps_running => format!("exited {} (stopped)", self.exit_code),
            "exited" if self.exit_code == "0" => "exited 0 (finished)".to_string(),
            "exited" => format!("exited {}", self.exit_code),
            state if self.health.is_empty() => state.to_string(),
            state => format!("{state} {}", self.health),
        }
    }
}

/// All containers of a compose project, including stopped ones.
pub fn ps(connector: &dyn Connector, project: &str) -> Result<Vec<ContainerStatus>> {
    let output = connector.exec(&format!(
        "docker compose -p {} ps -a --format {PS_FORMAT}",
        shell_quote(project)
    ))?;
    Ok(output.lines().map(ContainerStatus::parse).collect())
}

/// Poll until every container is ready; a container that exits with an error
/// fails at once.
pub fn wait_until_ready(connector: &dyn Connector, project: &str) -> Result<Vec<ContainerStatus>> {
    wait_until_ready_within(connector, project, START_TIMEOUT)
}

/// Like [`wait_until_ready`], with another timeout. A running container
/// counts once it has run [`STABLE_AFTER`] without restarting; one that
/// restarts until something it needs is up gets there in time.
pub fn wait_until_ready_within(
    connector: &dyn Connector,
    project: &str,
    timeout: Duration,
) -> Result<Vec<ContainerStatus>> {
    let deadline = Instant::now() + timeout;
    // Restart counts when compost started waiting.
    let mut first_restarts: HashMap<String, u64> = HashMap::new();
    loop {
        let containers = ps(connector, project)?;
        if let Some(failed) = containers.iter().find(|c| c.failed()) {
            bail!(
                "service `{}` exited with code {}\n{}",
                failed.service,
                failed.exit_code,
                summary(&containers)
            );
        }
        let uptimes = uptimes(connector, project)?;
        for uptime in &uptimes {
            first_restarts
                .entry(uptime.service.clone())
                .or_insert(uptime.restarts);
        }
        let stable = uptimes.iter().all(|u| u.seconds >= STABLE_AFTER.as_secs());
        if !containers.is_empty() && containers.iter().all(ContainerStatus::is_ready) && stable {
            return Ok(containers);
        }
        if Instant::now() >= deadline {
            let mut message = format!(
                "not ready after {}s:\n{}",
                timeout.as_secs(),
                summary(&containers)
            );
            for uptime in &uptimes {
                let restarts =
                    uptime.restarts - first_restarts[&uptime.service].min(uptime.restarts);
                if restarts > 0 {
                    message.push_str(&restarted(connector, project, &uptime.service, restarts));
                }
            }
            bail!(message);
        }
        sleep(POLL_INTERVAL);
    }
}

/// How long a running container has run since it last (re)started.
#[derive(Debug, PartialEq)]
struct Uptime {
    service: String,
    restarts: u64,
    seconds: u64,
}

/// The running containers' uptimes, measured by the server's clock. Lines
/// it can't read count as stable, so they never hold up a deploy.
fn uptimes(connector: &dyn Connector, project: &str) -> Result<Vec<Uptime>> {
    let output = connector.exec(&format!(
        "docker compose -p {} ps -q | xargs -r docker inspect --format {UPTIME_FORMAT}; date -u +%s",
        shell_quote(project)
    ))?;
    Ok(parse_uptimes(&output))
}

fn parse_uptimes(output: &str) -> Vec<Uptime> {
    let mut lines: Vec<&str> = output.lines().collect();
    let Some(now) = lines.pop().and_then(|l| l.trim().parse::<i64>().ok()) else {
        return Vec::new();
    };
    lines
        .iter()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let service = fields.next()?.to_string();
            let restarts = fields.next()?.parse().ok()?;
            let started = chrono::DateTime::parse_from_rfc3339(fields.next()?).ok()?;
            let seconds = u64::try_from(now - started.timestamp()).unwrap_or(0);
            Some(Uptime {
                service,
                restarts,
                seconds,
            })
        })
        .collect()
}

/// What to say of a service that restarted while compost waited: how often,
/// and its last log lines.
fn restarted(connector: &dyn Connector, project: &str, service: &str, restarts: u64) -> String {
    let logs = connector
        .exec(&format!(
            "docker compose -p {} logs --no-color --tail {LOG_LINES} {} 2>&1",
            shell_quote(project),
            shell_quote(service)
        ))
        .unwrap_or_default();
    let mut text = format!("\nservice `{service}` restarted {restarts} times; its last log lines:");
    for line in logs.trim_end().lines() {
        text.push_str("\n  ");
        text.push_str(line);
    }
    text
}

fn summary(containers: &[ContainerStatus]) -> String {
    containers
        .iter()
        .map(|c| format!("  {}: {}", c.service, c.describe(false)))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness_rules() {
        let ready = |line| ContainerStatus::parse(line).is_ready();
        assert!(ready("web running 0"));
        assert!(ready("db running 0 healthy"));
        assert!(!ready("db running 0 starting"));
        assert!(ready("migration exited 0"));
        assert!(!ready("migration exited 1"));
        assert!(!ready("web restarting 0"));
    }

    #[test]
    fn stopped_long_running_service_is_not_healthy() {
        let stopped = ContainerStatus::parse("web exited 0");
        assert!(stopped.is_healthy(false));
        assert!(!stopped.is_healthy(true));
        assert!(ContainerStatus::parse("web running 0").is_healthy(true));
    }

    #[test]
    fn uptimes_by_the_servers_clock() {
        let output = "caddy 3 2026-10-21T10:24:00.123456789Z\nweb 0 2026-10-21T09:00:00Z\nodd line\n1792578242\n";
        // 1792578242 is 2026-10-21T10:24:02Z.
        assert_eq!(
            parse_uptimes(output),
            [
                Uptime {
                    service: "caddy".to_string(),
                    restarts: 3,
                    seconds: 2
                },
                Uptime {
                    service: "web".to_string(),
                    restarts: 0,
                    seconds: 5042
                },
            ]
        );
        assert_eq!(parse_uptimes(""), []);
        assert_eq!(
            parse_uptimes("web 0 2026-10-21T09:00:00Z\nnot a time\n"),
            []
        );
    }

    #[test]
    fn descriptions() {
        let describe = |line| ContainerStatus::parse(line).describe(false);
        assert_eq!(describe("db running 0 healthy"), "running healthy");
        assert_eq!(describe("web running 0"), "running");
        assert_eq!(describe("migration exited 0"), "exited 0 (finished)");
        assert_eq!(describe("migration exited 2"), "exited 2");
        assert_eq!(
            ContainerStatus::parse("web exited 0").describe(true),
            "exited 0 (stopped)"
        );
    }
}
