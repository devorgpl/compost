//! `state.yaml`: what depap deployed on the server (spec §8.2). Only depap writes it.

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
    pub depap_version: String,
    /// Sections this version does not know (e.g. `apps`), kept unchanged.
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeployStatus {
    /// Files are on the server, services not (re)started yet.
    Candidate,
    Deployed,
    Failed,
}

impl Default for StateFile {
    fn default() -> Self {
        Self {
            schema: SCHEMA,
            shared: Vec::new(),
            depap_version: String::new(),
            other: Mapping::new(),
        }
    }
}

impl StateFile {
    pub fn load(ctx: &ServerCtx) -> Result<Self> {
        match ctx.read(STATE_FILE)? {
            None => Ok(Self::default()),
            Some(text) => serde_yaml_ng::from_str(&text)
                .with_context(|| format!("invalid {} on the server", ctx.path(STATE_FILE))),
        }
    }

    pub fn save(&mut self, ctx: &ServerCtx) -> Result<()> {
        self.depap_version = env!("CARGO_PKG_VERSION").to_string();
        let text = serde_yaml_ng::to_string(self)?;
        ctx.connector
            .write_file(&ctx.path(STATE_FILE), text.as_bytes(), 0o644)
    }

    pub fn shared(&self, kind: &str, name: &str) -> Option<&SharedRecord> {
        self.shared.iter().find(|r| r.kind == kind && r.name == name)
    }

    /// Add or update the record of a shared app.
    pub fn set_shared(&mut self, kind: &str, name: &str, status: DeployStatus, config_hash: &str) {
        let record = SharedRecord {
            kind: kind.to_string(),
            name: name.to_string(),
            status,
            config_hash: config_hash.to_string(),
            updated_at: humantime::format_rfc3339_seconds(SystemTime::now()).to_string(),
        };
        match self.shared.iter_mut().find(|r| r.kind == kind && r.name == name) {
            Some(existing) => *existing = record,
            None => self.shared.push(record),
        }
    }
}
