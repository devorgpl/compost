//! Generic database behavior, used by every database type unless it overrides it.

use anyhow::{Result, ensure};

use crate::shared::SharedAppConfig;

/// Docker network of one database; apps linked to it join this network.
pub fn network_name(database: &str) -> String {
    format!("depap-db-{database}")
}

pub fn validate_config(config: &SharedAppConfig) -> Result<()> {
    // Apps pick a database by version, so every database must declare one.
    ensure!(config.version.is_some(), "version is required for databases");
    if let Some(image) = &config.image {
        ensure!(!image.trim().is_empty(), "image must not be empty");
    }
    Ok(())
}
