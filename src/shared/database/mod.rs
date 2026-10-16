//! Database kind: database servers that apps link to.

mod base;
mod postgres;

use anyhow::{Context, Result, bail};
use serde_yaml_ng::Value;

use super::{
    AppCtx, Contribution, Generated, ItemReader, ItemWriter, Kind, Rendered, Resolution, ServerCtx,
    SharedAppConfig, parse_link,
};
use crate::config::app::AppConfig;
use base::{DatabaseLink, DatabaseRecord, Endpoint};

pub const KIND: &str = "database";

/// Implemented by every database type. Methods with a body are the kind's generic
/// behavior: a type inherits them, or overrides them to extend or replace.
pub trait DatabaseType: Sync {
    fn type_name(&self) -> &'static str;

    /// URL scheme and port, used for `db_port` and `db_url`.
    fn endpoint(&self) -> Endpoint;

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

    fn validate_link(&self, link: &DatabaseLink, app: &AppConfig) -> Result<()> {
        base::validate_link(link, app)
    }

    /// compost can create databases and users on the server: its own, or an
    /// external one it has root credentials for.
    fn can_provision(&self, _ctx: &ServerCtx, config: &SharedAppConfig) -> Result<bool> {
        Ok(!config.is_external())
    }

    fn prepare_application(
        &self,
        ctx: &AppCtx,
        link: &DatabaseLink,
        out: &mut Contribution,
    ) -> Result<()> {
        let provisioned = self.can_provision(ctx.server, ctx.shared)?;
        base::prepare_application(ctx, link, &self.endpoint(), provisioned, out)
    }

    /// Whether an external server answers.
    fn check_external(&self, _ctx: &ServerCtx, config: &SharedAppConfig) -> Result<String> {
        bail!("{} databases cannot be external", config.type_name)
    }

    fn after_apply_application(&self, _ctx: &AppCtx, _link: &DatabaseLink) -> Result<()> {
        Ok(())
    }

    /// Delete the link record; with `purge`, drop the database and user first.
    fn remove_application(&self, ctx: &AppCtx, entry: &str, purge: bool) -> Result<()> {
        if purge {
            if let Some(record) = base::read_record(ctx, entry)? {
                self.drop_database(ctx, &record)?;
            }
        }
        base::remove_application(ctx, entry)
    }

    // Type-specific, no default.

    /// Starter configuration for `server generate`.
    fn generate(&self) -> Generated;

    fn render(&self, ctx: &ServerCtx, config: &SharedAppConfig) -> Result<Rendered>;

    /// Create the database and its user, before the app starts.
    fn before_apply_application(&self, ctx: &AppCtx, link: &DatabaseLink) -> Result<()>;

    /// Delete the database and user of a link (`app remove --purge database`).
    fn drop_database(&self, ctx: &AppCtx, record: &DatabaseRecord) -> Result<()>;

    /// Save the link's database as the item `name`.
    fn dump_database(
        &self,
        ctx: &AppCtx,
        record: &DatabaseRecord,
        name: &str,
        out: &mut ItemWriter,
    ) -> Result<()>;

    /// Load the item `name` into the link's database; a database that is not
    /// empty is only dropped and recreated with `replace`.
    fn load_database(
        &self,
        ctx: &AppCtx,
        record: &DatabaseRecord,
        name: &str,
        from: &ItemReader,
        replace: bool,
    ) -> Result<()>;

    /// Save the whole database server: all databases, roles and the root password.
    fn backup_shared(
        &self,
        ctx: &ServerCtx,
        config: &SharedAppConfig,
        out: &mut ItemWriter,
    ) -> Result<()>;

    fn restore_shared_files(
        &self,
        _ctx: &ServerCtx,
        _config: &SharedAppConfig,
        _from: &ItemReader,
    ) -> Result<()> {
        Ok(())
    }

    fn restore_shared_data(
        &self,
        ctx: &ServerCtx,
        config: &SharedAppConfig,
        from: &ItemReader,
    ) -> Result<()>;
}

static TYPES: &[&dyn DatabaseType] = &[&postgres::Postgres];

pub struct DatabaseKind;

impl Kind for DatabaseKind {
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
        match base::external(config)? {
            None => rendered.networks.push(base::network_name(&config.name)),
            Some(external) => rendered.existing_networks.extend(external.network),
        }
        Ok(rendered)
    }

    fn supports_external(&self) -> bool {
        true
    }

    fn check_external(&self, ctx: &ServerCtx, config: &SharedAppConfig) -> Result<String> {
        find_type(&config.type_name)?.check_external(ctx, config)
    }

    fn apply_order(&self) -> i32 {
        10 // before ingress: the database must exist when the app starts
    }

    fn link_secret_keys(&self) -> &'static [&'static str] {
        &["db_password"]
    }

    fn validate_link(&self, link: &Value, app: &AppConfig) -> Result<()> {
        let link: DatabaseLink = parse_link(link)?;
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

    fn after_apply_application(&self, ctx: &AppCtx, link: &Value) -> Result<()> {
        find_type(&ctx.shared.type_name)?.after_apply_application(ctx, &parse_link(link)?)
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

    fn restore_shared_data(
        &self,
        ctx: &ServerCtx,
        config: &SharedAppConfig,
        from: &ItemReader,
    ) -> Result<()> {
        find_type(&config.type_name)?.restore_shared_data(ctx, config, from)
    }

    /// `<entry>.sql`: a dump of the link's database.
    fn backup_link(&self, ctx: &AppCtx, entry: &str, out: &mut ItemWriter) -> Result<()> {
        let record = base::read_record(ctx, entry)?
            .with_context(|| format!("no link record for `{entry}`; deploy the app again"))?;
        find_type(&ctx.shared.type_name)?.dump_database(ctx, &record, &dump_name(entry), out)
    }

    fn restore_link(
        &self,
        ctx: &AppCtx,
        entry: &str,
        from: &ItemReader,
        replace: bool,
    ) -> Result<()> {
        let name = dump_name(entry);
        if !from.has(&name) {
            return Ok(());
        }
        let record = base::read_record(ctx, entry)?
            .with_context(|| format!("no link record for `{entry}`; deploy the app first"))?;
        find_type(&ctx.shared.type_name)?.load_database(ctx, &record, &name, from, replace)
    }
}

fn dump_name(entry: &str) -> String {
    format!("{entry}.sql")
}

fn find_type(name: &str) -> Result<&'static dyn DatabaseType> {
    match TYPES.iter().copied().find(|t| t.type_name() == name) {
        Some(found) => Ok(found),
        None => {
            let known: Vec<_> = TYPES.iter().map(|t| t.type_name()).collect();
            bail!(
                "unknown database type `{name}` (known: {})",
                known.join(", ")
            )
        }
    }
}
