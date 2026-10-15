//! Ingress kind: reverse proxies that route domains to app services.

mod base;
mod caddy;

use anyhow::{Result, bail};

use super::{Kind, SharedAppConfig};

/// Implemented by every ingress type. Methods with a body are the kind's generic
/// behavior: a type inherits them, or overrides them to extend or replace.
pub trait IngressType: Sync {
    fn type_name(&self) -> &'static str;

    fn validate_config(&self, config: &SharedAppConfig) -> Result<()> {
        base::validate_config(config)
    }
}

static TYPES: &[&dyn IngressType] = &[&caddy::Caddy];

pub struct IngressKind;

impl Kind for IngressKind {
    fn name(&self) -> &'static str {
        "ingress"
    }

    fn validate_shared(&self, config: &SharedAppConfig) -> Result<()> {
        find_type(&config.type_name)?.validate_config(config)
    }

    fn validate_instances(&self, configs: &[SharedAppConfig]) -> Result<()> {
        if configs.len() > 1 {
            bail!("only one ingress per server is supported");
        }
        Ok(())
    }
}

fn find_type(name: &str) -> Result<&'static dyn IngressType> {
    match TYPES.iter().copied().find(|t| t.type_name() == name) {
        Some(found) => Ok(found),
        None => {
            let known: Vec<_> = TYPES.iter().map(|t| t.type_name()).collect();
            bail!("unknown ingress type `{name}` (known: {})", known.join(", "))
        }
    }
}
