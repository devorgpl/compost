//! Generic OIDC behavior, used by every OIDC type unless it overrides it.

use anyhow::{Result, ensure};

use crate::shared::SharedAppConfig;

/// Docker network of one provider; apps linked to it join this network.
pub fn network_name(provider: &str) -> String {
    format!("compost-oidc-{provider}")
}

pub fn validate_config(config: &SharedAppConfig) -> Result<()> {
    if let Some(image) = &config.image {
        ensure!(!image.trim().is_empty(), "image must not be empty");
    }
    Ok(())
}

/// `^[a-z0-9][a-z0-9._@-]{0,254}$`; providers keep usernames in lower case.
pub fn is_username(value: &str) -> bool {
    let mut chars = value.chars();
    value.len() <= 255
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._@-".contains(c))
}
