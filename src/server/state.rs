//! Server state checks (spec §10.6). Also the preflight of every mutating command.

use std::fmt;

use anyhow::{Context, Result};

use super::{MANAGER_KEY_FILE, SERVER_CONFIG_FILE};
use crate::config::server::ServerConfig;
use crate::connector::{Connector, shell_quote};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Unreachable,
    NoDocker,
    Uninitialized,
    Foreign,
    Degraded,
    Ok,
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Status::Unreachable => "UNREACHABLE",
            Status::NoDocker => "NO_DOCKER",
            Status::Uninitialized => "UNINITIALIZED",
            Status::Foreign => "FOREIGN",
            Status::Degraded => "DEGRADED",
            Status::Ok => "OK",
        })
    }
}

#[derive(Debug)]
pub struct Report {
    pub status: Status,
    /// Why the status is not OK, when there is something to say.
    pub detail: Option<String>,
    /// e.g. `docker 29.7.2, compose 2.40.0`
    pub docker: Option<String>,
    /// DEPAP_SERVER_HOME as an absolute path.
    pub home: Option<String>,
    pub shared: Vec<SharedAppState>,
}

#[derive(Debug)]
pub struct SharedAppState {
    pub kind: String,
    pub name: String,
    pub type_name: String,
    pub version: Option<u32>,
    pub running: bool,
}

impl Report {
    fn stopped(status: Status, detail: String) -> Self {
        Self {
            status,
            detail: Some(detail),
            docker: None,
            home: None,
            shared: Vec::new(),
        }
    }
}

/// Run the checks in order; the first failing check decides the status.
pub fn check(connector: &dyn Connector, server_home: &str, manager_key: &str) -> Result<Report> {
    if let Err(err) = connector.run("true", None) {
        return Ok(Report::stopped(Status::Unreachable, format!("{err:#}")));
    }

    let versions = connector.run(
        "docker version --format '{{.Server.Version}}' && docker compose version --short",
        None,
    )?;
    if !versions.success() {
        return Ok(Report::stopped(Status::NoDocker, versions.stderr.trim().to_string()));
    }
    let mut lines = versions.stdout.lines();
    let docker = format!(
        "docker {}, compose {}",
        lines.next().unwrap_or("?"),
        lines.next().unwrap_or("?")
    );

    let home = connector.resolve_path(server_home)?;
    let mut report = Report {
        status: Status::Ok,
        detail: None,
        docker: Some(docker),
        home: Some(home.clone()),
        shared: Vec::new(),
    };

    match connector.read_file(&format!("{home}/{MANAGER_KEY_FILE}"))? {
        None => {
            report.status = Status::Uninitialized;
            return Ok(report);
        }
        Some(key) if key.trim() != manager_key => {
            report.status = Status::Foreign;
            report.detail = Some(format!(
                "the server is managed by another depap installation (manager key {})",
                key.trim()
            ));
            return Ok(report);
        }
        Some(_) => {}
    }

    let config_path = format!("{home}/{SERVER_CONFIG_FILE}");
    if let Some(text) = connector.read_file(&config_path)? {
        let config = ServerConfig::parse(&text)
            .with_context(|| format!("invalid {config_path} on the server"))?;
        for (kind, app) in config.shared_apps() {
            let project = format!("depap-{kind}-{}", app.name);
            let running = is_project_running(connector, &project)?;
            if !running {
                report.status = Status::Degraded;
            }
            report.shared.push(SharedAppState {
                kind: kind.to_string(),
                name: app.name.clone(),
                type_name: app.type_name.clone(),
                version: app.version,
                running,
            });
        }
    }
    Ok(report)
}

fn is_project_running(connector: &dyn Connector, project: &str) -> Result<bool> {
    let label = shell_quote(&format!("label=com.docker.compose.project={project}"));
    let ids = connector.exec(&format!("docker ps -q --filter {label} --filter status=running"))?;
    Ok(!ids.trim().is_empty())
}
