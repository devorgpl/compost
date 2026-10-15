//! Shared applications: the kind registry and the interface core uses (spec §9.1).
//!
//! Layers: core → kind (`ingress`, `database`) → type (`caddy`, `postgres`).
//! Core only talks to the [`Kind`] trait; each kind looks up its types and calls them.

pub mod database;
pub mod ingress;

use anyhow::Result;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_yaml_ng::Value;

/// One entry under `shared.<kind>` in depap-server.yaml.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SharedAppConfig {
    pub name: String,
    #[serde(rename = "type")]
    pub type_name: String,
    #[serde(default)]
    pub version: Option<u32>,
    #[serde(default)]
    pub image: Option<String>,
    /// Type-specific settings, parsed by the type itself.
    #[serde(default)]
    pub configuration: Value,
}

impl SharedAppConfig {
    /// Parse the `configuration` block into a type's own struct.
    pub fn configuration<T: DeserializeOwned + Default>(&self) -> Result<T> {
        if self.configuration.is_null() {
            return Ok(T::default());
        }
        serde_yaml_ng::from_value(self.configuration.clone())
            .map_err(|e| anyhow::anyhow!("configuration: {e}"))
    }
}

/// What core needs from a kind. Methods grow with the spec (render, hooks, …).
pub trait Kind: Sync {
    /// Key under `shared:` / `services:`; also the value accepted by `--purge`.
    fn name(&self) -> &'static str;

    /// Validate one shared app of this kind.
    fn validate_shared(&self, config: &SharedAppConfig) -> Result<()>;

    /// Validate all shared apps of this kind together. Default: no rules.
    fn validate_instances(&self, _configs: &[SharedAppConfig]) -> Result<()> {
        Ok(())
    }
}

static KINDS: &[&dyn Kind] = &[&database::DatabaseKind, &ingress::IngressKind];

pub fn find_kind(name: &str) -> Option<&'static dyn Kind> {
    KINDS.iter().copied().find(|kind| kind.name() == name)
}

pub fn kind_names() -> Vec<&'static str> {
    KINDS.iter().map(|kind| kind.name()).collect()
}
