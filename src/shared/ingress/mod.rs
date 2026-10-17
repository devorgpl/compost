//! Ingress kind: reverse proxies that route domains to app services.

mod base;
mod caddy;
pub mod security;

use anyhow::{Result, bail};
use serde_yaml_ng::Value;

use super::{
    AppCtx, Contribution, Generated, ItemReader, ItemWriter, Kind, Monitored, Rendered,
    RenderedFile, Resolution, ServerCtx, SharedAppConfig, parse_link,
};
use crate::config::app::AppConfig;
use base::IngressLink;

pub const KIND: &str = "ingress";

/// Implemented by every ingress type. Methods with a body are the kind's generic
/// behavior: a type inherits them, or overrides them to extend or replace.
pub trait IngressType: Sync {
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

    fn prepare_application(
        &self,
        ctx: &AppCtx,
        link: &IngressLink,
        out: &mut Contribution,
    ) -> Result<()> {
        base::prepare_application(ctx, link, out)
    }

    fn before_apply_application(&self, _ctx: &AppCtx, _link: &IngressLink) -> Result<()> {
        Ok(())
    }

    fn remove_application(&self, ctx: &AppCtx, entry: &str) -> Result<()> {
        base::remove_application(ctx, entry)
    }

    /// The link record, for review. Routing is rebuilt by deploy, so there is
    /// nothing to restore.
    fn backup_link(&self, ctx: &AppCtx, entry: &str, out: &mut ItemWriter) -> Result<()> {
        base::backup_link(ctx, entry, out)
    }

    fn backup_shared(
        &self,
        _ctx: &ServerCtx,
        _config: &SharedAppConfig,
        _out: &mut ItemWriter,
    ) -> Result<()> {
        Ok(())
    }

    fn restore_shared_files(
        &self,
        _ctx: &ServerCtx,
        _config: &SharedAppConfig,
        _from: &ItemReader,
    ) -> Result<()> {
        Ok(())
    }

    /// Services and metrics endpoints, for `monitoring` of the shared app.
    fn monitored(&self, _config: &SharedAppConfig) -> Result<Monitored> {
        bail!(
            "ingress shared apps of type `{}` cannot be monitored",
            self.type_name()
        )
    }

    // Type-specific, no default.

    /// Starter configuration for `server generate`.
    fn generate(&self) -> Generated;

    fn render(&self, ctx: &ServerCtx, config: &SharedAppConfig) -> Result<Rendered>;

    /// Apply changed configuration files without a restart (see [`Kind::reload_shared`]).
    fn reload_shared(&self, _ctx: &ServerCtx, _config: &SharedAppConfig) -> Result<bool> {
        Ok(false)
    }

    /// Whether `reload_shared` can apply changed files, for `--dry-run`.
    fn reloads(&self) -> bool {
        false
    }

    /// Files `after_apply_application` would write, for `--dry-run`.
    fn preview_application(&self, _ctx: &AppCtx, _link: &IngressLink) -> Result<Vec<RenderedFile>> {
        Ok(Vec::new())
    }

    /// Make the proxy route the link's domains to the app.
    fn after_apply_application(&self, ctx: &AppCtx, link: &IngressLink) -> Result<()>;
}

static TYPES: &[&dyn IngressType] = &[&caddy::CADDY, &caddy::CADDY_SECURE];

pub struct IngressKind;

impl Kind for IngressKind {
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

    fn validate_instances(&self, configs: &[SharedAppConfig]) -> Result<()> {
        if configs.len() > 1 {
            bail!("only one ingress per server is supported");
        }
        Ok(())
    }

    fn secret_keys(&self, config: &SharedAppConfig) -> Result<&'static [&'static str]> {
        Ok(find_type(&config.type_name)?.secret_keys())
    }

    fn monitored(&self, config: &SharedAppConfig) -> Result<Monitored> {
        find_type(&config.type_name)?.monitored(config)
    }

    fn render(&self, ctx: &ServerCtx, config: &SharedAppConfig) -> Result<Rendered> {
        let mut rendered = find_type(&config.type_name)?.render(ctx, config)?;
        rendered.networks.push(base::NETWORK.to_string());
        Ok(rendered)
    }

    fn reload_shared(&self, ctx: &ServerCtx, config: &SharedAppConfig) -> Result<bool> {
        find_type(&config.type_name)?.reload_shared(ctx, config)
    }

    fn reloads(&self, config: &SharedAppConfig) -> bool {
        find_type(&config.type_name).is_ok_and(|t| t.reloads())
    }

    fn apply_order(&self) -> i32 {
        20 // after databases: routes go live once the app runs
    }

    fn validate_link(&self, link: &Value, app: &AppConfig) -> Result<()> {
        base::validate_link(&parse_link(link)?, app)
    }

    fn resolve<'s>(
        &self,
        link: &Value,
        available: &'s [SharedAppConfig],
    ) -> Result<Resolution<'s>> {
        base::resolve(&parse_link(link)?, available)
    }

    // An ingress link names no type: the type of the resolved ingress handles it.

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

    fn after_apply_application(&self, ctx: &AppCtx, link: &Value) -> Result<()> {
        find_type(&ctx.shared.type_name)?.after_apply_application(ctx, &parse_link(link)?)
    }

    fn preview_application(&self, ctx: &AppCtx, link: &Value) -> Result<Vec<RenderedFile>> {
        find_type(&ctx.shared.type_name)?.preview_application(ctx, &parse_link(link)?)
    }

    fn describe_link(&self, ctx: &AppCtx, entry: &str) -> Result<String> {
        base::describe_link(ctx, entry)
    }

    /// Routing has no data to purge; `--purge ingress` is rejected before this runs.
    fn remove_application(&self, ctx: &AppCtx, entry: &str, _purge: bool) -> Result<()> {
        find_type(&ctx.shared.type_name)?.remove_application(ctx, entry)
    }

    fn backup_shared(
        &self,
        ctx: &ServerCtx,
        config: &SharedAppConfig,
        out: &mut ItemWriter,
    ) -> Result<()> {
        find_type(&config.type_name)?.backup_shared(ctx, config, out)
    }

    fn restore_shared_files(
        &self,
        ctx: &ServerCtx,
        config: &SharedAppConfig,
        from: &ItemReader,
    ) -> Result<()> {
        find_type(&config.type_name)?.restore_shared_files(ctx, config, from)
    }

    fn backup_link(&self, ctx: &AppCtx, entry: &str, out: &mut ItemWriter) -> Result<()> {
        find_type(&ctx.shared.type_name)?.backup_link(ctx, entry, out)
    }
}

fn find_type(name: &str) -> Result<&'static dyn IngressType> {
    if let Some(found) = TYPES.iter().copied().find(|t| t.type_name() == name) {
        Ok(found)
    } else {
        let known: Vec<_> = TYPES.iter().map(|t| t.type_name()).collect();
        bail!(
            "unknown ingress type `{name}` (known: {})",
            known.join(", ")
        )
    }
}
