//! Database kind: database servers that apps link to.

mod base;
mod postgres;

use anyhow::{Result, bail};

use super::{Kind, Rendered, ServerCtx, SharedAppConfig};

pub const KIND: &str = "database";

/// Implemented by every database type. Methods with a body are the kind's generic
/// behavior: a type inherits them, or overrides them to extend or replace.
pub trait DatabaseType: Sync {
    fn type_name(&self) -> &'static str;

    fn secret_keys(&self) -> &'static [&'static str] {
        &[]
    }

    fn validate_config(&self, config: &SharedAppConfig) -> Result<()> {
        base::validate_config(config)
    }

    /// Type-specific, no default.
    fn render(&self, ctx: &ServerCtx, config: &SharedAppConfig) -> Result<Rendered>;
}

static TYPES: &[&dyn DatabaseType] = &[&postgres::Postgres];

pub struct DatabaseKind;

impl Kind for DatabaseKind {
    fn name(&self) -> &'static str {
        KIND
    }

    fn validate_shared(&self, config: &SharedAppConfig) -> Result<()> {
        find_type(&config.type_name)?.validate_config(config)
    }

    fn secret_keys(&self, config: &SharedAppConfig) -> Result<&'static [&'static str]> {
        Ok(find_type(&config.type_name)?.secret_keys())
    }

    fn render(&self, ctx: &ServerCtx, config: &SharedAppConfig) -> Result<Rendered> {
        let mut rendered = find_type(&config.type_name)?.render(ctx, config)?;
        rendered.networks.push(base::network_name(&config.name));
        Ok(rendered)
    }
}

fn find_type(name: &str) -> Result<&'static dyn DatabaseType> {
    match TYPES.iter().copied().find(|t| t.type_name() == name) {
        Some(found) => Ok(found),
        None => {
            let known: Vec<_> = TYPES.iter().map(|t| t.type_name()).collect();
            bail!("unknown database type `{name}` (known: {})", known.join(", "))
        }
    }
}
