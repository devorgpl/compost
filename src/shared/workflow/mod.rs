//! Workflow kind: durable workflow servers (e.g. Temporal) that apps link to.
//! Each link gets a namespace of its own on the server.

mod base;
mod temporal;

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_yaml_ng::Value;

use super::{
    AppCtx, Contribution, Generated, ItemReader, ItemWriter, Kind, Monitored, Rendered, Resolution,
    ServerCtx, SharedAppConfig, SharedLinks, parse_link,
};
use crate::config::app::AppConfig;
use base::{Endpoint, WorkflowLink, WorkflowRecord};

pub const KIND: &str = "workflow";

/// Implemented by every workflow type. Methods with a body are the kind's
/// generic behavior: a type inherits them, or overrides them to extend or replace.
pub trait WorkflowType: Sync {
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

    fn validate_link(&self, link: &WorkflowLink, app: &AppConfig) -> Result<()> {
        base::validate_link(link, app)
    }

    fn prepare_application(
        &self,
        ctx: &AppCtx,
        link: &WorkflowLink,
        out: &mut Contribution,
    ) -> Result<()> {
        base::prepare_application(ctx, link, &self.endpoint(ctx.shared), out)
    }

    /// Delete the link record; with `purge`, delete the namespace first when
    /// no other link uses it.
    fn remove_application(&self, ctx: &AppCtx, entry: &str, purge: bool) -> Result<()> {
        if purge {
            if let Some(record) = base::read_record(ctx, entry)? {
                if !base::namespace_used_elsewhere(ctx, entry, &record.namespace)? {
                    self.purge(ctx, &record)?;
                }
            }
        }
        base::remove_application(ctx, entry)
    }

    /// Services and metrics endpoints, for `monitoring` of the shared app.
    fn monitored(&self, _config: &SharedAppConfig) -> Result<Monitored> {
        bail!(
            "workflow shared apps of type `{}` cannot be monitored",
            self.type_name()
        )
    }

    /// Save files of the shared app its data depends on. Its data is in the
    /// databases it links to and is saved with them.
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

    /// Apply changed configuration files without a restart (see [`Kind::reload_shared`]).
    fn reload_shared(&self, _ctx: &ServerCtx, _config: &SharedAppConfig) -> Result<bool> {
        Ok(false)
    }

    fn reloads(&self, _config: &SharedAppConfig) -> bool {
        false
    }

    // Type-specific, no default.

    /// Starter configuration for `server generate`.
    fn generate(&self) -> Generated;

    fn render(&self, ctx: &ServerCtx, config: &SharedAppConfig) -> Result<Rendered>;

    /// The server's own links, e.g. its databases and its UI on the ingress.
    fn shared_links(&self, config: &SharedAppConfig) -> Result<Option<SharedLinks>>;

    /// Where apps reach the server on its network.
    fn endpoint(&self, config: &SharedAppConfig) -> Endpoint;

    /// Create or update the link's namespace, before the app starts.
    fn before_apply_application(&self, ctx: &AppCtx, link: &WorkflowLink) -> Result<()>;

    /// Delete the namespace of a link (`app remove --purge workflow`).
    fn purge(&self, ctx: &AppCtx, record: &WorkflowRecord) -> Result<()>;
}

static TYPES: &[&dyn WorkflowType] = &[&temporal::Temporal];

pub struct WorkflowKind;

impl Kind for WorkflowKind {
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

    fn reload_shared(&self, ctx: &ServerCtx, config: &SharedAppConfig) -> Result<bool> {
        find_type(&config.type_name)?.reload_shared(ctx, config)
    }

    fn reloads(&self, config: &SharedAppConfig) -> bool {
        find_type(&config.type_name).is_ok_and(|t| t.reloads(config))
    }

    /// Workflow servers set up their database schema on the first start.
    fn start_timeout(&self) -> Duration {
        Duration::from_secs(180)
    }

    fn apply_order(&self) -> i32 {
        12 // after databases, before ingress: the namespace must exist when workers start
    }

    fn validate_link(&self, link: &Value, app: &AppConfig) -> Result<()> {
        let link: WorkflowLink = parse_link(link)?;
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

    fn remove_application(&self, ctx: &AppCtx, entry: &str, purge: bool) -> Result<()> {
        find_type(&ctx.shared.type_name)?.remove_application(ctx, entry, purge)
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

    /// The link record, for review. Workflows are in the server's database
    /// and are saved with it.
    fn backup_link(&self, ctx: &AppCtx, entry: &str, out: &mut ItemWriter) -> Result<()> {
        base::backup_link(ctx, entry, out).with_context(|| format!("workflow link `{entry}`"))
    }
}

fn find_type(name: &str) -> Result<&'static dyn WorkflowType> {
    if let Some(found) = TYPES.iter().copied().find(|t| t.type_name() == name) {
        Ok(found)
    } else {
        let known: Vec<_> = TYPES.iter().map(|t| t.type_name()).collect();
        bail!(
            "unknown workflow type `{name}` (known: {})",
            known.join(", ")
        )
    }
}
