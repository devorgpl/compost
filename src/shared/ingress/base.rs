//! Generic ingress behavior, used by every ingress type unless it overrides it.

use anyhow::{Result, ensure};

use crate::shared::SharedAppConfig;

/// Docker network shared by the ingress and every app service it routes to.
pub const NETWORK: &str = "depap-ingress";

pub fn validate_config(config: &SharedAppConfig) -> Result<()> {
    if let Some(image) = &config.image {
        ensure!(!image.trim().is_empty(), "image must not be empty");
    }
    Ok(())
}
