//! OIDC kind: identity providers.

mod base;
mod keycloak;

use std::time::Duration;

use anyhow::{Result, bail};
use serde_yaml_ng::Value;

use super::{
    AppCtx, Contribution, Generated, Kind, Rendered, Resolution, ServerCtx, SharedAppConfig,
    SharedLinks,
};
use crate::config::app::AppConfig;

pub const KIND: &str = "oidc";

/// Implemented by every OIDC type. Methods with a body are the kind's generic
/// behavior: a type inherits them, or overrides them to extend or replace.
pub trait OidcType: Sync {
    fn type_name(&self) -> &'static str;

    fn secret_keys(&self) -> &'static [&'static str] {
        &[]
    }

    /// Version used when the config gives none.
    fn default_version(&self) -> Option<u32> {
        None
    }

    fn validate_config(&self, config: &SharedAppConfig) -> Result<()> {
        base::validate_config(config)
    }

    // Type-specific, no default.

    /// Starter configuration for `server generate`.
    fn generate(&self) -> Generated;

    fn render(&self, ctx: &ServerCtx, config: &SharedAppConfig) -> Result<Rendered>;

    /// The provider's own links, e.g. its database and its domain on the ingress.
    fn shared_links(&self, config: &SharedAppConfig) -> Result<Option<SharedLinks>>;
}

static TYPES: &[&dyn OidcType] = &[&keycloak::Keycloak];

pub struct OidcKind;

impl Kind for OidcKind {
    fn name(&self) -> &'static str {
        KIND
    }

    fn type_names(&self) -> Vec<&'static str> {
        TYPES.iter().map(|t| t.type_name()).collect()
    }

    fn generate(&self, type_name: &str) -> Result<Generated> {
        Ok(find_type(type_name)?.generate())
    }

    fn apply_defaults(&self, config: &mut SharedAppConfig) {
        if let (None, Ok(found)) = (config.version, find_type(&config.type_name)) {
            config.version = found.default_version();
        }
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

    fn shared_links(&self, config: &SharedAppConfig) -> Result<Option<SharedLinks>> {
        find_type(&config.type_name)?.shared_links(config)
    }

    /// Identity providers run database migrations on their first start.
    fn start_timeout(&self) -> Duration {
        Duration::from_secs(180)
    }

    fn apply_order(&self) -> i32 {
        15 // after databases, before ingress
    }

    // Apps cannot link to an OIDC provider yet.

    fn validate_link(&self, _link: &Value, _app: &AppConfig) -> Result<()> {
        bail!("links to oidc providers are not supported yet")
    }

    fn resolve<'s>(
        &self,
        _link: &Value,
        _available: &'s [SharedAppConfig],
    ) -> Result<Resolution<'s>> {
        bail!("links to oidc providers are not supported yet")
    }

    fn prepare_application(
        &self,
        _ctx: &AppCtx,
        _link: &Value,
        _out: &mut Contribution,
    ) -> Result<()> {
        Ok(())
    }

    fn before_apply_application(&self, _ctx: &AppCtx, _link: &Value) -> Result<()> {
        Ok(())
    }

    fn after_apply_application(&self, _ctx: &AppCtx, _link: &Value) -> Result<()> {
        Ok(())
    }

    fn describe_link(&self, _ctx: &AppCtx, _entry: &str) -> Result<String> {
        Ok("no link record".to_string())
    }

    fn remove_application(&self, _ctx: &AppCtx, _entry: &str, _purge: bool) -> Result<()> {
        Ok(())
    }
}

fn find_type(name: &str) -> Result<&'static dyn OidcType> {
    match TYPES.iter().copied().find(|t| t.type_name() == name) {
        Some(found) => Ok(found),
        None => {
            let known: Vec<_> = TYPES.iter().map(|t| t.type_name()).collect();
            bail!("unknown oidc type `{name}` (known: {})", known.join(", "))
        }
    }
}
