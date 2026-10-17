//! Monitoring kind: collects the output, metrics and traces of apps and shared apps.

mod base;
mod openobserve;

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use serde_yaml_ng::Value;

use super::{
    AppCtx, Contribution, Generated, ItemReader, ItemWriter, Kind, Rendered, Resolution, ServerCtx,
    SharedAppConfig, SharedLinks, parse_link,
};
use crate::config::app::AppConfig;
use base::{Endpoint, MonitoringLink};

pub const KIND: &str = "monitoring";

/// Implemented by every monitoring type. Methods with a body are the kind's generic
/// behavior: a type inherits them, or overrides them to extend or replace.
pub trait MonitoringType: Sync {
    fn type_name(&self) -> &'static str;

    fn secret_keys(&self) -> &'static [&'static str] {
        &[]
    }

    /// Version used when the config gives none.
    fn default_version(&self) -> Option<u32> {
        None
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

    // Type-specific, no default.

    /// Starter configuration for `server generate`.
    fn generate(&self) -> Generated;

    fn validate_config(&self, config: &SharedAppConfig) -> Result<()>;

    /// Files of the backend and of its collector, which reads link files
    /// from [`base::targets_dir`].
    fn render(&self, ctx: &ServerCtx, config: &SharedAppConfig) -> Result<Rendered>;

    /// The backend's own links, e.g. its UI on the ingress.
    fn shared_links(&self, config: &SharedAppConfig) -> Result<Option<SharedLinks>>;

    /// Where apps send their data.
    fn endpoint(&self, config: &SharedAppConfig) -> Result<Endpoint>;
}

static TYPES: &[&dyn MonitoringType] = &[&openobserve::OpenObserve];

pub struct MonitoringKind;

impl Kind for MonitoringKind {
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
        if config.monitoring.is_some() {
            bail!("`monitoring` is not supported for monitoring shared apps");
        }
        find_type(&config.type_name)?.validate_config(config)
    }

    /// Each collector listens on its own port of the host for docker output.
    fn validate_instances(&self, configs: &[SharedAppConfig]) -> Result<()> {
        let mut ports: HashMap<u16, &str> = HashMap::new();
        for config in configs {
            let port = find_type(&config.type_name)?.endpoint(config)?.forward_port;
            if let Some(other) = ports.insert(port, &config.name) {
                bail!(
                    "{other} and {} both use forward_port {port}; give each its own",
                    config.name
                );
            }
        }
        Ok(())
    }

    fn secret_keys(&self, config: &SharedAppConfig) -> Result<&'static [&'static str]> {
        Ok(find_type(&config.type_name)?.secret_keys())
    }

    fn render(&self, ctx: &ServerCtx, config: &SharedAppConfig) -> Result<Rendered> {
        let mut rendered = find_type(&config.type_name)?.render(ctx, config)?;
        rendered.networks.push(base::network_name(&config.name));
        rendered.dirs.push(base::targets_dir(&config.name));
        Ok(rendered)
    }

    fn shared_links(&self, config: &SharedAppConfig) -> Result<Option<SharedLinks>> {
        find_type(&config.type_name)?.shared_links(config)
    }

    /// Output goes through the docker log driver, which buffers while the
    /// collector is down, and scrape targets are files: a link works before
    /// the monitoring app runs.
    fn orders_start(&self) -> bool {
        false
    }

    fn apply_order(&self) -> i32 {
        25 // after ingress; nothing waits for it
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

    // A monitoring link names no type: every type takes the same data.

    fn prepare_application(
        &self,
        ctx: &AppCtx,
        link: &Value,
        out: &mut Contribution,
    ) -> Result<()> {
        let endpoint = find_type(&ctx.shared.type_name)?.endpoint(ctx.shared)?;
        let link: MonitoringLink = parse_link(link)?;
        base::prepare_application(ctx, &link, &endpoint, out)
    }

    fn before_apply_application(&self, _ctx: &AppCtx, _link: &Value) -> Result<()> {
        Ok(())
    }

    fn after_apply_application(&self, ctx: &AppCtx, link: &Value) -> Result<()> {
        base::after_apply_application(ctx, &parse_link(link)?)
    }

    fn describe_link(&self, ctx: &AppCtx, entry: &str) -> Result<String> {
        base::describe_link(ctx, entry)
    }

    /// Collected data stays until the backend's retention deletes it.
    fn remove_application(&self, ctx: &AppCtx, entry: &str, _purge: bool) -> Result<()> {
        base::remove_application(ctx, entry)
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

    /// The link record, for review.
    fn backup_link(&self, ctx: &AppCtx, entry: &str, out: &mut ItemWriter) -> Result<()> {
        base::backup_link(ctx, entry, out).with_context(|| format!("monitoring link `{entry}`"))
    }
}

fn find_type(name: &str) -> Result<&'static dyn MonitoringType> {
    if let Some(found) = TYPES.iter().copied().find(|t| t.type_name() == name) {
        Ok(found)
    } else {
        let known: Vec<_> = TYPES.iter().map(|t| t.type_name()).collect();
        bail!(
            "unknown monitoring type `{name}` (known: {})",
            known.join(", ")
        )
    }
}
