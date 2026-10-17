//! OIDC kind: identity providers that apps link to for login.

mod base;
mod keycloak;

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_yaml_ng::Value;

use super::{
    AppCtx, Contribution, Exposed, Generated, ItemWriter, Kind, Monitored, Rendered, Resolution,
    ServerCtx, SharedAppConfig, SharedLinks, parse_link,
};
use crate::config::app::AppConfig;
use base::{Endpoint, OidcLink, OidcRecord};

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

    fn validate_link(&self, link: &OidcLink, app: &AppConfig) -> Result<()> {
        base::validate_link(link, app)
    }

    fn prepare_application(
        &self,
        ctx: &AppCtx,
        link: &OidcLink,
        out: &mut Contribution,
    ) -> Result<()> {
        base::prepare_application(ctx, link, &self.endpoint(ctx.shared)?, out)
    }

    /// Delete the link record; with `purge`, delete the client and users first,
    /// and the realm when no other link uses it.
    fn remove_application(&self, ctx: &AppCtx, entry: &str, purge: bool) -> Result<()> {
        if purge {
            if let Some(record) = base::read_record(ctx, entry)? {
                let delete_realm = !base::realm_used_elsewhere(ctx, entry, &record.realm)?;
                self.purge(ctx, &record, delete_realm)?;
            }
        }
        base::remove_application(ctx, entry)
    }

    /// Paths of the link's realm, for an ingress route on its own domain.
    fn exposed_routes(&self, ctx: &AppCtx, link: &OidcLink) -> Result<Exposed> {
        base::exposed_routes(ctx, link, &self.ingress_upstream(ctx.shared))
    }

    /// Services and metrics endpoints, for `monitoring` of the shared app.
    fn monitored(&self, _config: &SharedAppConfig) -> Result<Monitored> {
        bail!(
            "oidc shared apps of type `{}` cannot be monitored",
            self.type_name()
        )
    }

    // Type-specific, no default.

    /// `<host>:<port>` of the provider on the ingress network.
    fn ingress_upstream(&self, config: &SharedAppConfig) -> String;

    /// Starter configuration for `server generate`.
    fn generate(&self) -> Generated;

    fn render(&self, ctx: &ServerCtx, config: &SharedAppConfig) -> Result<Rendered>;

    /// The provider's own links, e.g. its database and its domain on the ingress.
    fn shared_links(&self, config: &SharedAppConfig) -> Result<Option<SharedLinks>>;

    /// Public and internal base URLs of a provider.
    fn endpoint(&self, config: &SharedAppConfig) -> Result<Endpoint>;

    /// Create or update the realm, client and users, before the app starts.
    fn before_apply_application(&self, ctx: &AppCtx, link: &OidcLink) -> Result<()>;

    /// Delete what a link created (`app remove --purge oidc`).
    fn purge(&self, ctx: &AppCtx, record: &OidcRecord, delete_realm: bool) -> Result<()>;
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

    fn monitored(&self, config: &SharedAppConfig) -> Result<Monitored> {
        find_type(&config.type_name)?.monitored(config)
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
        15 // after databases, before ingress: the client must exist when the app starts
    }

    fn link_secret_keys(&self) -> &'static [&'static str] {
        &["client_secret", "users"]
    }

    fn validate_link(&self, link: &Value, app: &AppConfig) -> Result<()> {
        let link: OidcLink = parse_link(link)?;
        find_type(&link.type_name)?.validate_link(&link, app)
    }

    fn resolve<'s>(
        &self,
        link: &Value,
        available: &'s [SharedAppConfig],
    ) -> Result<Resolution<'s>> {
        base::resolve(&parse_link(link)?, available)
    }

    fn prepare_application(
        &self,
        ctx: &AppCtx,
        link: &Value,
        out: &mut Contribution,
    ) -> Result<()> {
        find_type(&ctx.shared.type_name)?.prepare_application(ctx, &parse_link(link)?, out)
    }

    fn before_apply_application(&self, ctx: &AppCtx, link: &Value) -> Result<()> {
        find_type(&ctx.shared.type_name)?.before_apply_application(ctx, &parse_link(link)?)
    }

    fn after_apply_application(&self, _ctx: &AppCtx, _link: &Value) -> Result<()> {
        Ok(())
    }

    fn describe_link(&self, ctx: &AppCtx, entry: &str) -> Result<String> {
        base::describe_link(ctx, entry)
    }

    fn supports_purge(&self) -> bool {
        true
    }

    fn exposes_routes(&self) -> bool {
        true
    }

    fn exposed_routes(&self, ctx: &AppCtx, link: &Value) -> Result<Exposed> {
        find_type(&ctx.shared.type_name)?.exposed_routes(ctx, &parse_link(link)?)
    }

    fn remove_application(&self, ctx: &AppCtx, entry: &str, purge: bool) -> Result<()> {
        find_type(&ctx.shared.type_name)?.remove_application(ctx, entry, purge)
    }

    /// The link record, for review. Realms, clients and users are in the
    /// provider's database and are saved with it.
    fn backup_link(&self, ctx: &AppCtx, entry: &str, out: &mut ItemWriter) -> Result<()> {
        base::backup_link(ctx, entry, out).with_context(|| format!("oidc link `{entry}`"))
    }
}

fn find_type(name: &str) -> Result<&'static dyn OidcType> {
    if let Some(found) = TYPES.iter().copied().find(|t| t.type_name() == name) {
        Ok(found)
    } else {
        let known: Vec<_> = TYPES.iter().map(|t| t.type_name()).collect();
        bail!("unknown oidc type `{name}` (known: {})", known.join(", "))
    }
}
