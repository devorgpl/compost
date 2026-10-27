//! `state.yaml`: what compost deployed on the server (spec §8.2). Only compost writes it.

use std::fmt;
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_yaml_ng::Mapping;

use crate::shared::ServerCtx;

pub const STATE_FILE: &str = "state.yaml";
const SCHEMA: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
pub struct StateFile {
    pub schema: u32,
    #[serde(default)]
    pub shared: Vec<SharedRecord>,
    #[serde(default)]
    pub apps: Vec<AppRecord>,
    #[serde(default)]
    pub compost_version: String,
    /// Scheduled backups: when each last ran, and how it went.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub backups: Vec<BackupRecord>,
    /// What the server reported last, so that it reports changes only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alerts: Vec<AlertRecord>,
    /// How far the `bans` notifications got.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bans: Option<BansRecord>,
    /// What the `updates` notifications reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updates: Option<UpdatesRecord>,
    /// When the `status` summary was last sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<StatusRecord>,
    /// Sections this version does not know, kept unchanged.
    #[serde(flatten)]
    pub other: Mapping,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedRecord {
    pub kind: String,
    pub name: String,
    pub status: DeployStatus,
    /// Hash of the rendered files; a different hash means the config changed.
    pub config_hash: String,
    pub updated_at: String,
    /// The image each container ran after the last deploy, and since when.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImageRecord>,
}

/// The image a shared app's service runs, and since which deploy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageRecord {
    pub service: String,
    pub image_id: String,
    /// When a deploy first started it with this image (RFC 3339).
    pub since: String,
}

/// One application instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppRecord {
    pub name: String,
    pub prefix: String,
    pub status: DeployStatus,
    pub updated_at: String,
}

/// The last reported state of something the server watches, e.g.
/// `health:myapp-prod` or `disk`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlertRecord {
    pub key: String,
    /// Whether an alert was sent and not cleared yet.
    pub alerting: bool,
    /// Checks in a row that found a problem.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub bad: u32,
    /// Since when it is in this state (RFC 3339).
    pub since: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// The CrowdSec alerts the `bans` notifications covered.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BansRecord {
    /// The newest alert reported (or seen, before the first report).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_alert: Option<u64>,
    /// When `bans_daily` was last sent (`YYYY-MM-DD`, the server's date).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_daily: Option<String>,
}

/// The newer images and releases the `updates` notifications reported.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdatesRecord {
    /// When the registries were last asked (`YYYY-MM-DD`, the server's date).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_daily: Option<String>,
    /// What was reported, one key per newer image or release.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reported: Vec<String>,
}

/// The last `status` summary.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusRecord {
    /// When it was last sent (`YYYY-MM-DD`, the server's date).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_daily: Option<String>,
    /// The CPU counters then, for the average utilisation until the next one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<CpuTimes>,
}

/// The first line of `/proc/stat`: CPU time since boot, in clock ticks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpuTimes {
    pub total: u64,
    /// Idle and waiting for I/O.
    pub idle: u64,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// A backup profile run on a schedule, for the shared apps or one instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupRecord {
    /// `server` for the shared apps, else the instance, e.g. `myapp-prod`.
    pub owner: String,
    pub profile: String,
    /// When it last ran, or since when it waits for its first run (RFC 3339, with the offset).
    pub last_run: String,
    pub status: BackupStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The size of the archive the last run wrote, in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// The target it was written to, e.g. `server` or `offsite`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

impl BackupRecord {
    /// For the record of an instance that was removed since: kept, since it
    /// tells when its last backup ran.
    pub fn describe_removed(&self) -> String {
        match self.status {
            BackupStatus::Waiting => "instance removed before its first run".to_string(),
            _ => format!("instance removed; last run {}", self.describe()),
        }
    }

    /// e.g. `ok, 2026-10-18 03:00` or `failed, 2026-10-18 03:00: …`.
    pub fn describe(&self) -> String {
        // RFC 3339 → `2026-10-18 03:00`, in the server's time.
        let time = self
            .last_run
            .get(..16)
            .unwrap_or(&self.last_run)
            .replace('T', " ");
        match (self.status, &self.error) {
            (BackupStatus::Waiting, _) => format!("waiting for its first run (since {time})"),
            (status, Some(error)) => {
                let first = error.lines().next().unwrap_or_default();
                format!("{status}, {time}: {first}")
            }
            (status, None) => format!("{status}, {time}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackupStatus {
    /// Scheduled, not run yet.
    Waiting,
    Ok,
    Failed,
}

impl fmt::Display for BackupStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            BackupStatus::Waiting => "waiting",
            BackupStatus::Ok => "ok",
            BackupStatus::Failed => "failed",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeployStatus {
    /// Files are on the server, services not (re)started yet.
    Candidate,
    Deployed,
    Failed,
    /// Deployed, then its containers were stopped (`dev stop`).
    Stopped,
    /// Its links are set up, but its services run outside compost (`dev up --host`).
    Provisioned,
}

impl fmt::Display for DeployStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            DeployStatus::Candidate => "candidate",
            DeployStatus::Deployed => "deployed",
            DeployStatus::Failed => "failed",
            DeployStatus::Stopped => "stopped",
            DeployStatus::Provisioned => "provisioned",
        })
    }
}

impl Default for StateFile {
    fn default() -> Self {
        Self {
            schema: SCHEMA,
            shared: Vec::new(),
            apps: Vec::new(),
            compost_version: String::new(),
            backups: Vec::new(),
            alerts: Vec::new(),
            bans: None,
            updates: None,
            status: None,
            other: Mapping::new(),
        }
    }
}

impl StateFile {
    pub fn load(ctx: &ServerCtx) -> Result<Self> {
        let Some(text) = ctx.read(STATE_FILE)? else {
            return Ok(Self::default());
        };
        let path = ctx.path(STATE_FILE);
        let state: Self = serde_yaml_ng::from_str(&text)
            .with_context(|| format!("invalid {path} on the server"))?;
        // An older compost would misread a newer layout and save it back.
        anyhow::ensure!(
            state.schema == SCHEMA,
            "{path} has schema {} (written by compost {}), this compost supports {SCHEMA}; upgrade compost",
            state.schema,
            if state.compost_version.is_empty() {
                "unknown"
            } else {
                &state.compost_version
            }
        );
        Ok(state)
    }

    pub fn save(&mut self, ctx: &ServerCtx) -> Result<()> {
        self.compost_version = env!("CARGO_PKG_VERSION").to_string();
        let text = serde_yaml_ng::to_string(self)?;
        ctx.connector
            .write_file(&ctx.path(STATE_FILE), text.as_bytes(), 0o644)
    }

    pub fn shared(&self, kind: &str, name: &str) -> Option<&SharedRecord> {
        self.shared
            .iter()
            .find(|r| r.kind == kind && r.name == name)
    }

    /// Add or update the record of a shared app.
    pub fn set_shared(&mut self, kind: &str, name: &str, status: DeployStatus, config_hash: &str) {
        let record = SharedRecord {
            kind: kind.to_string(),
            name: name.to_string(),
            status,
            config_hash: config_hash.to_string(),
            updated_at: now(),
            images: Vec::new(),
        };
        match self
            .shared
            .iter_mut()
            .find(|r| r.kind == kind && r.name == name)
        {
            Some(existing) => {
                let images = std::mem::take(&mut existing.images);
                *existing = SharedRecord { images, ..record };
            }
            None => self.shared.push(record),
        }
    }

    /// Record the images a shared app's containers run after a deploy; an
    /// image keeps the time it was first deployed.
    pub fn set_images(&mut self, kind: &str, name: &str, running: &[super::images::Running]) {
        let now = now();
        if let Some(record) = self
            .shared
            .iter_mut()
            .find(|r| r.kind == kind && r.name == name)
        {
            record.images = super::images::records(&record.images, running, &now);
        }
    }

    pub fn remove_shared(&mut self, kind: &str, name: &str) {
        self.shared.retain(|r| !(r.kind == kind && r.name == name));
    }

    pub fn app(&self, name: &str, prefix: &str) -> Option<&AppRecord> {
        self.apps
            .iter()
            .find(|r| r.name == name && r.prefix == prefix)
    }

    pub fn remove_app(&mut self, name: &str, prefix: &str) {
        self.apps
            .retain(|r| !(r.name == name && r.prefix == prefix));
    }

    /// Add or update the record of an application instance.
    pub fn set_app(&mut self, name: &str, prefix: &str, status: DeployStatus) {
        let record = AppRecord {
            name: name.to_string(),
            prefix: prefix.to_string(),
            status,
            updated_at: now(),
        };
        match self
            .apps
            .iter_mut()
            .find(|r| r.name == name && r.prefix == prefix)
        {
            Some(existing) => *existing = record,
            None => self.apps.push(record),
        }
    }
}

fn now() -> String {
    humantime::format_rfc3339_seconds(SystemTime::now()).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::local::Local;
    use tempfile::TempDir;

    #[test]
    fn load_refuses_another_schema() {
        let dir = TempDir::new().unwrap();
        let connector = Local { verbose: false };
        let ctx = ServerCtx {
            connector: &connector,
            home: dir.path().to_str().unwrap().to_string(),
        };
        assert_eq!(StateFile::load(&ctx).unwrap().schema, SCHEMA);

        let mut state = StateFile::default();
        state.set_app("app", "main", DeployStatus::Deployed);
        state.save(&ctx).unwrap();
        assert_eq!(StateFile::load(&ctx).unwrap().apps.len(), 1);

        let path = dir.path().join(STATE_FILE);
        std::fs::write(&path, "schema: 2\ncompost_version: 9.0.0\n").unwrap();
        let err = format!("{:#}", StateFile::load(&ctx).unwrap_err());
        assert!(err.contains("schema 2 (written by compost 9.0.0)"), "{err}");
        assert!(err.contains("upgrade compost"), "{err}");
    }
}
