//! Waiting for docker compose projects to come up.

use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::connector::{Connector, shell_quote};

/// How long to wait for a project to be running (and healthy).
const START_TIMEOUT: Duration = Duration::from_secs(60);
const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// One `<service> <state> <exit code> <health>` line per container.
const PS_FORMAT: &str = "'{{.Service}} {{.State}} {{.ExitCode}} {{.Health}}'";

/// Poll until every container is running (and healthy if it has a
/// healthcheck) or has finished with exit code 0, like a migration.
/// Returns the last `docker compose ps` output.
pub fn wait_until_ready(connector: &dyn Connector, project: &str) -> Result<String> {
    let deadline = Instant::now() + START_TIMEOUT;
    let script = format!(
        "docker compose -p {} ps -a --format {PS_FORMAT}",
        shell_quote(project)
    );
    loop {
        let output = connector.exec(&script)?;
        let containers: Vec<Container> = output.lines().map(Container::parse).collect();
        if let Some(failed) = containers.iter().find(|c| c.state == "exited" && c.exit_code != "0") {
            bail!("service `{}` exited with code {}:\n{}", failed.service, failed.exit_code, output.trim_end());
        }
        if !containers.is_empty() && containers.iter().all(Container::is_ready) {
            return Ok(output);
        }
        if Instant::now() >= deadline {
            bail!("not ready after {}s:\n{}", START_TIMEOUT.as_secs(), output.trim_end());
        }
        sleep(POLL_INTERVAL);
    }
}

struct Container<'a> {
    service: &'a str,
    state: &'a str,
    exit_code: &'a str,
    health: &'a str,
}

impl<'a> Container<'a> {
    fn parse(line: &'a str) -> Self {
        let mut fields = line.split_whitespace();
        let mut next = || fields.next().unwrap_or_default();
        Self { service: next(), state: next(), exit_code: next(), health: next() }
    }

    fn is_ready(&self) -> bool {
        match self.state {
            "running" => self.health.is_empty() || self.health == "healthy",
            "exited" => self.exit_code == "0",
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness_rules() {
        let ready = |line| Container::parse(line).is_ready();
        assert!(ready("web running 0"));
        assert!(ready("db running 0 healthy"));
        assert!(!ready("db running 0 starting"));
        assert!(ready("migration exited 0"));
        assert!(!ready("migration exited 1"));
        assert!(!ready("web restarting 0"));
    }
}
