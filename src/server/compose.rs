//! Container status of docker compose projects.

use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::connector::{Connector, shell_quote};

/// How long to wait for a project to be running (and healthy).
const START_TIMEOUT: Duration = Duration::from_secs(60);
const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// One `<service> <state> <exit code> <health>` line per container.
const PS_FORMAT: &str = "'{{.Service}} {{.State}} {{.ExitCode}} {{.Health}}'";

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
    let deadline = Instant::now() + START_TIMEOUT;
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
        if !containers.is_empty() && containers.iter().all(ContainerStatus::is_ready) {
            return Ok(containers);
        }
        if Instant::now() >= deadline {
            bail!(
                "not ready after {}s:\n{}",
                START_TIMEOUT.as_secs(),
                summary(&containers)
            );
        }
        sleep(POLL_INTERVAL);
    }
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
