//! Key-value kind: Valkey and Redis servers that apps link to, e.g. as a cache,
//! for sessions or queues. One implementation, two flavors.
//!
//! Each link gets an ACL user. On Valkey (9.1+) the user is locked to its own
//! logical database (`db=`); Redis has no such rule, so its users are limited
//! to a key prefix, which the app must use. Users live in an ACL file that
//! compost writes from the link records and loads with `ACL LOAD`.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use anyhow::{Context, Result, bail, ensure};
use minijinja::context;
use serde::{Deserialize, Serialize};
use serde_yaml_ng::Value;

use super::{
    AppCtx, Contribution, Generated, ItemReader, ItemWriter, Kind, Monitored, Rendered,
    RenderedFile, Resolution, Role, ServerCtx, SharedAppConfig, app_service, connectors_dir,
    generate_password, host_address, is_variable_name, parse_link, project_name, render_template,
    resolve_versioned, service_name, shared_dir,
};
use crate::config::app::AppConfig;
use crate::connector::shell_quote;

pub const KIND: &str = "keyvalue";
const PORT: u16 = 6379;
const PASSWORD_VARIABLE: &str = "KV_PASSWORD";
/// Logical databases of a Valkey server; database 0 is left to the `default` user.
const DATABASES: u32 = 256;
const DUMP_ITEM: &str = "dump.rdb";
const SECRETS_ITEM: &str = "password.env";

/// Values a link can map to app variables.
const LINK_VALUES: [&str; 7] = [
    "kv_host",
    "kv_port",
    "kv_user",
    "kv_password",
    "kv_db",
    "kv_prefix",
    "kv_url",
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Flavor {
    Valkey,
    Redis,
}

impl Flavor {
    fn find(type_name: &str) -> Result<Self> {
        match type_name {
            "valkey" => Ok(Flavor::Valkey),
            "redis" => Ok(Flavor::Redis),
            other => bail!("unknown keyvalue type `{other}` (known: valkey, redis)"),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Flavor::Valkey => "valkey",
            Flavor::Redis => "redis",
        }
    }

    fn default_version(self) -> u32 {
        match self {
            Flavor::Valkey => 9,
            Flavor::Redis => 8,
        }
    }

    fn image(self, version: u32) -> String {
        match self {
            Flavor::Valkey => format!("valkey/valkey:{version}"),
            Flavor::Redis => format!("redis:{version}"),
        }
    }

    fn server(self) -> &'static str {
        match self {
            Flavor::Valkey => "valkey-server",
            Flavor::Redis => "redis-server",
        }
    }

    fn cli(self) -> &'static str {
        match self {
            Flavor::Valkey => "valkey-cli",
            Flavor::Redis => "redis-cli",
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct KeyValueConfig {
    /// Password of the `default` user. Secret; generated when absent.
    password: Option<String>,
    /// e.g. `256mb`: the server evicts keys by `eviction` above it.
    max_memory: Option<String>,
    /// `maxmemory-policy`, e.g. `allkeys-lru` for a cache. Default `noeviction`.
    eviction: Option<String>,
    /// Publish the server's port on 127.0.0.1 of the machine, e.g. for an app
    /// run outside Docker.
    host_port: Option<u16>,
}

const EVICTION: [&str; 8] = [
    "noeviction",
    "allkeys-lru",
    "allkeys-lfu",
    "allkeys-random",
    "volatile-lru",
    "volatile-lfu",
    "volatile-random",
    "volatile-ttl",
];

/// An entry under `services.keyvalue` in compost.yaml.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyValueLink {
    name: String,
    #[serde(rename = "type")]
    type_name: String,
    /// Semver requirement on the server's major version.
    #[serde(default = "any_version")]
    version: String,
    #[serde(default)]
    instance: Option<String>,
    #[serde(default)]
    targets: Vec<String>,
    /// App variable name → link value (one of [`LINK_VALUES`]).
    #[serde(default)]
    environment: BTreeMap<String, String>,
}

fn any_version() -> String {
    "*".to_string()
}

/// `connectors/keyvalue/<name>/<instance>-<entry>.yaml`. Secret.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct KeyValueRecord {
    app: String,
    prefix: String,
    entry: String,
    user: String,
    password: String,
    /// Valkey: the user's logical database.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    db: Option<u32>,
    /// Redis: the user's key prefix, e.g. `myapp-prod-cache:`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    key_prefix: Option<String>,
}

fn network_name(name: &str) -> String {
    format!("compost-kv-{name}")
}

fn secrets_path(name: &str) -> String {
    format!("{}/secrets/password.env", shared_dir(KIND, name))
}

fn acl_path(name: &str) -> String {
    format!("{}/conf/users.acl", shared_dir(KIND, name))
}

fn conf_path(name: &str) -> String {
    format!("{}/conf/server.conf", shared_dir(KIND, name))
}

/// Present while a restored `dump.rdb` waits to be loaded: the server then
/// starts without AOF, which would ignore it (see `restore_shared_data`).
fn restore_marker(name: &str) -> String {
    format!("{}/restore-pending", shared_dir(KIND, name))
}

fn sha256_hex(text: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// User names go into the ACL file: no spaces, nothing ACL rules start with.
fn check_user(user: &str) -> Result<()> {
    ensure!(
        !user.is_empty()
            && user
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "unexpected user name `{user}` in a link record"
    );
    Ok(())
}

/// The ACL file: the `default` user, then one user per link record.
fn acl_file(flavor: Flavor, password: &str, records: &[KeyValueRecord]) -> Result<String> {
    let mut acl = format!("user default on #{} ~* &* +@all\n", sha256_hex(password));
    for record in records {
        check_user(&record.user)?;
        let hash = sha256_hex(&record.password);
        match (flavor, record.db, &record.key_prefix) {
            (Flavor::Valkey, Some(db), _) => {
                let _ = writeln!(
                    acl,
                    "user {} on #{hash} resetchannels &* ~* +@all -@admin -flushall -swapdb db={db}",
                    record.user
                );
            }
            (Flavor::Redis, _, Some(prefix)) => {
                check_user(prefix.trim_end_matches(':'))?;
                let _ = writeln!(
                    acl,
                    "user {} on #{hash} resetchannels &{prefix}* ~{prefix}* +@all -@admin -@dangerous +info",
                    record.user
                );
            }
            _ => bail!(
                "link record of {} does not fit a {} server",
                record.user,
                flavor.name()
            ),
        }
    }
    Ok(acl)
}

fn read_records(ctx: &ServerCtx, name: &str) -> Result<Vec<KeyValueRecord>> {
    let dir = connectors_dir(KIND, name);
    let mut records = Vec::new();
    for file in ctx.connector.list_dir(&ctx.path(&dir))? {
        if !file.ends_with(".yaml") {
            continue;
        }
        let path = format!("{dir}/{file}");
        if let Some(text) = ctx.read(&path)? {
            records.push(
                serde_yaml_ng::from_str(&text)
                    .with_context(|| format!("invalid {}", ctx.path(&path)))?,
            );
        }
    }
    records.sort_by(|a: &KeyValueRecord, b| a.user.cmp(&b.user));
    Ok(records)
}

fn record_path(ctx: &AppCtx, entry: &str) -> String {
    format!(
        "{}/{}.yaml",
        connectors_dir(KIND, &ctx.shared.name),
        ctx.link_id(entry)
    )
}

fn read_record(ctx: &AppCtx, entry: &str) -> Result<Option<KeyValueRecord>> {
    let path = record_path(ctx, entry);
    match ctx.server.read(&path)? {
        None => Ok(None),
        Some(text) => {
            Ok(Some(serde_yaml_ng::from_str(&text).with_context(|| {
                format!("invalid {}", ctx.server.path(&path))
            })?))
        }
    }
}

/// The `default` user's password: the config's, else the one on the server,
/// else a new one.
fn password(ctx: &ServerCtx, config: &SharedAppConfig, kv: &KeyValueConfig) -> Result<String> {
    if let Some(password) = &kv.password {
        return Ok(password.clone());
    }
    let existing = ctx.read(&secrets_path(&config.name))?.and_then(|text| {
        text.lines().find_map(|line| {
            let value = line.strip_prefix(PASSWORD_VARIABLE)?.strip_prefix('=')?;
            Some(value.trim().trim_matches('\'').to_string())
        })
    });
    Ok(existing.unwrap_or_else(generate_password))
}

/// Run `cli args` in the server's container as the `default` user; the
/// password comes from the container's environment, never a command line.
fn cli_command(ctx: &ServerCtx, config: &SharedAppConfig, flavor: Flavor, args: &str) -> String {
    let inner = format!(
        "REDISCLI_AUTH=\"${PASSWORD_VARIABLE}\" exec {} --no-auth-warning {args}",
        flavor.cli()
    );
    exec_command(ctx, &config.name, &format!("sh -c {}", shell_quote(&inner)))
}

fn exec_command(ctx: &ServerCtx, name: &str, command: &str) -> String {
    format!(
        "cd {} && docker compose -p {} exec -T {} {command}",
        shell_quote(&ctx.path(&shared_dir(KIND, name))),
        shell_quote(&project_name(KIND, name)),
        shell_quote(&service_name(name))
    )
}

/// Rewrite the ACL file from the link records and load it.
fn reload_users(ctx: &ServerCtx, config: &SharedAppConfig) -> Result<()> {
    let flavor = Flavor::find(&config.type_name)?;
    let kv: KeyValueConfig = config.configuration()?;
    let acl = acl_file(
        flavor,
        &password(ctx, config, &kv)?,
        &read_records(ctx, &config.name)?,
    )?;
    ctx.connector
        .write_file(&ctx.path(&acl_path(&config.name)), acl.as_bytes(), 0o644)?;
    let output = ctx
        .connector
        .exec(&cli_command(ctx, config, flavor, "ACL LOAD"))
        .with_context(|| format!("loading the users of {} failed", config.name))?;
    ensure!(
        output.trim() == "OK",
        "loading the users of {} failed: {}",
        config.name,
        output.trim()
    );
    Ok(())
}

/// The Valkey database of a new link: the lowest one no record uses.
fn free_database(records: &[KeyValueRecord]) -> Result<u32> {
    (1..DATABASES)
        .find(|n| !records.iter().any(|r| r.db == Some(*n)))
        .context("all databases of this Valkey server are used by links")
}

fn generated(flavor: Flavor) -> Generated {
    let (local, secrets) = match flavor {
        Flavor::Valkey => (
            include_str!("templates/generate/valkey.yaml"),
            include_str!("templates/generate/valkey.secrets.yaml"),
        ),
        Flavor::Redis => (
            include_str!("templates/generate/redis.yaml"),
            include_str!("templates/generate/redis.secrets.yaml"),
        ),
    };
    Generated {
        local,
        prod: None,
        secrets: Some(secrets),
        notes: match flavor {
            Flavor::Valkey => &[
                "valkey: link apps with `services.keyvalue: [{name: cache, type: valkey, targets: […], environment: {REDIS_URL: kv_url}}]`; each gets its own database.",
            ],
            Flavor::Redis => &[
                "redis: each linked app gets a user limited to its key prefix (link value `kv_prefix`); the app must put it in front of its keys. Valkey isolates apps without that.",
            ],
        },
        requires: &[],
    }
}

pub struct KeyValueKind;

impl Kind for KeyValueKind {
    fn name(&self) -> &'static str {
        KIND
    }

    fn type_names(&self) -> Vec<&'static str> {
        vec!["valkey", "redis"]
    }

    fn generate(&self, type_name: &str) -> Result<Generated> {
        Ok(generated(Flavor::find(type_name)?))
    }

    fn apply_defaults(&self, config: &mut SharedAppConfig) {
        if let (None, Ok(flavor)) = (config.version, Flavor::find(&config.type_name)) {
            config.version = Some(flavor.default_version());
        }
    }

    fn validate_shared(&self, config: &SharedAppConfig) -> Result<()> {
        let flavor = Flavor::find(&config.type_name)?;
        let version = config.version.context("version is required")?;
        if flavor == Flavor::Valkey {
            ensure!(
                version >= 9,
                "valkey 9.1 or newer is needed: it locks each app to its own database"
            );
        }
        if let Some(image) = &config.image {
            ensure!(!image.trim().is_empty(), "image must not be empty");
        }
        let kv: KeyValueConfig = config.configuration()?;
        if let Some(password) = &kv.password {
            ensure!(
                !password.is_empty() && !password.contains(['\n', '\r', '\'', ' ']),
                "password must be one word without `'`"
            );
        }
        if let Some(memory) = &kv.max_memory {
            ensure!(
                !memory.is_empty() && memory.chars().all(|c| c.is_ascii_alphanumeric()),
                "max_memory must be a size like `256mb` or `2gb`"
            );
        }
        if let Some(eviction) = &kv.eviction {
            ensure!(
                EVICTION.contains(&eviction.as_str()),
                "eviction: unknown policy `{eviction}` (known: {})",
                EVICTION.join(", ")
            );
        }
        Ok(())
    }

    fn secret_keys(&self, _config: &SharedAppConfig) -> Result<&'static [&'static str]> {
        Ok(&["password"])
    }

    fn monitored(&self, config: &SharedAppConfig) -> Result<Monitored> {
        Ok(Monitored {
            services: vec![app_service(&config.type_name, service_name(&config.name))],
            metrics: Vec::new(),
            traces: false,
        })
    }

    fn render(&self, ctx: &ServerCtx, config: &SharedAppConfig) -> Result<Rendered> {
        let flavor = Flavor::find(&config.type_name)?;
        let kv: KeyValueConfig = config.configuration()?;
        let version = config.version.context("version is required")?;
        let dir = shared_dir(KIND, &config.name);
        let password = password(ctx, config, &kv)?;
        let network = network_name(&config.name);
        let image = config
            .image
            .clone()
            .unwrap_or_else(|| flavor.image(version));

        let restoring = ctx.read(&restore_marker(&config.name))?.is_some();
        let mut conf = format!(
            "# Generated by compost. Do not edit; change compost-server.yaml and run `compost server deploy`.\n\
             bind 0.0.0.0\nport {PORT}\nprotected-mode no\ndir /data\naclfile /conf/users.acl\n\
             appendonly {}\n",
            if restoring { "no" } else { "yes" }
        );
        if flavor == Flavor::Valkey {
            let _ = writeln!(conf, "databases {DATABASES}");
        }
        if let Some(memory) = &kv.max_memory {
            let _ = writeln!(conf, "maxmemory {memory}");
        }
        if let Some(eviction) = &kv.eviction {
            let _ = writeln!(conf, "maxmemory-policy {eviction}");
        }

        let compose = render_template(
            include_str!("templates/compose.yaml.j2"),
            context! {
                image,
                service => service_name(&config.name),
                container => project_name(KIND, &config.name),
                network => network.as_str(),
                host => config.name.as_str(),
                server => flavor.server(),
                cli => flavor.cli(),
                config_hash => &sha256_hex(&format!("{conf}\0{password}"))[..12],
                host_port => kv.host_port,
            },
        )?;
        let acl = acl_file(flavor, &password, &read_records(ctx, &config.name)?)?;
        Ok(Rendered {
            files: vec![
                RenderedFile::new(format!("{dir}/compose.yaml"), compose),
                RenderedFile::new(conf_path(&config.name), conf),
                // Read by the server's own user in the container: passwords
                // are only hashes in it.
                RenderedFile::new(acl_path(&config.name), acl),
                RenderedFile::secret(
                    secrets_path(&config.name),
                    format!("{PASSWORD_VARIABLE}='{password}'\n"),
                ),
            ],
            dirs: vec![format!("{dir}/data"), format!("{dir}/conf")],
            networks: vec![network],
            ..Rendered::default()
        })
    }

    /// Called when files other than the compose file changed: only the ACL
    /// file can, since the config and the password are in a compose label.
    fn reload_shared(&self, ctx: &ServerCtx, config: &SharedAppConfig) -> Result<bool> {
        reload_users(ctx, config).map(|()| true)
    }

    fn reloads(&self, _config: &SharedAppConfig) -> bool {
        true
    }

    fn apply_order(&self) -> i32 {
        10 // with the databases: before the apps that use it
    }

    fn validate_link(&self, link: &Value, app: &AppConfig) -> Result<()> {
        let link: KeyValueLink = parse_link(link)?;
        Flavor::find(&link.type_name)?;
        semver::VersionReq::parse(&link.version)
            .with_context(|| format!("version `{}` is not a valid requirement", link.version))?;
        ensure!(
            !link.targets.is_empty(),
            "targets: at least one app service is required"
        );
        for target in &link.targets {
            ensure!(
                app.app_service(target).is_some(),
                "target `{target}` is not in app_services"
            );
        }
        for (variable, value) in &link.environment {
            ensure!(
                is_variable_name(variable),
                "`{variable}` is not a valid variable name"
            );
            ensure!(
                LINK_VALUES.contains(&value.as_str()),
                "environment.{variable}: unknown value `{value}` (known: {})",
                LINK_VALUES.join(", ")
            );
        }
        Ok(())
    }

    fn resolve<'s>(
        &self,
        link: &Value,
        available: &'s [SharedAppConfig],
    ) -> Result<Resolution<'s>> {
        let link: KeyValueLink = parse_link(link)?;
        let found = resolve_versioned(
            "keyvalue server",
            &link.type_name,
            &link.version,
            link.instance.as_deref(),
            available,
        )?;
        Ok(Resolution::Use(found))
    }

    /// Join the network, set the variables, write the link record (keeping
    /// its user, password and database).
    fn prepare_application(
        &self,
        ctx: &AppCtx,
        link: &Value,
        out: &mut Contribution,
    ) -> Result<()> {
        let link: KeyValueLink = parse_link(link)?;
        let flavor = Flavor::find(&ctx.shared.type_name)?;
        let user = ctx.link_id(&link.name);
        let record = match read_record(ctx, &link.name)? {
            Some(record) => record,
            None => KeyValueRecord {
                app: ctx.app.name.clone(),
                prefix: ctx.app.prefix.clone(),
                entry: link.name.clone(),
                user: user.clone(),
                password: generate_password(),
                db: match flavor {
                    Flavor::Valkey => {
                        Some(free_database(&read_records(ctx.server, &ctx.shared.name)?)?)
                    }
                    Flavor::Redis => None,
                },
                key_prefix: match flavor {
                    Flavor::Valkey => None,
                    Flavor::Redis => Some(format!("{user}:")),
                },
            },
        };
        let (host, port) = if out.is_for_host() {
            host_address(ctx.shared)?
        } else {
            (ctx.shared.name.clone(), PORT)
        };
        let db = record.db.unwrap_or(0);
        let values: BTreeMap<&str, String> = BTreeMap::from([
            ("kv_host", host.clone()),
            ("kv_port", port.to_string()),
            ("kv_user", record.user.clone()),
            ("kv_password", record.password.clone()),
            ("kv_db", db.to_string()),
            ("kv_prefix", record.key_prefix.clone().unwrap_or_default()),
            (
                "kv_url",
                format!(
                    "redis://{}:{}@{host}:{port}/{db}",
                    record.user, record.password
                ),
            ),
        ]);
        let network = network_name(&ctx.shared.name);
        for target in &link.targets {
            let service = ctx.compose_service(target)?;
            out.join_network(service, &network, None);
            for (variable, value) in &link.environment {
                out.set_env(service, variable, values[value.as_str()].clone())?;
            }
        }
        let text = serde_yaml_ng::to_string(&record)?;
        ctx.server.connector.write_file(
            &ctx.server.path(&record_path(ctx, &link.name)),
            text.as_bytes(),
            0o600,
        )
    }

    fn before_apply_application(&self, ctx: &AppCtx, _link: &Value) -> Result<()> {
        reload_users(ctx.server, ctx.shared)
    }

    fn after_apply_application(&self, _ctx: &AppCtx, _link: &Value) -> Result<()> {
        Ok(())
    }

    fn describe_link(&self, ctx: &AppCtx, entry: &str) -> Result<String> {
        Ok(match read_record(ctx, entry)? {
            Some(KeyValueRecord {
                user, db: Some(db), ..
            }) => format!("database {db}, user {user}"),
            Some(KeyValueRecord {
                user,
                key_prefix: Some(prefix),
                ..
            }) => format!("key prefix {prefix}, user {user}"),
            Some(record) => format!("user {}", record.user),
            None => "no link record".to_string(),
        })
    }

    fn supports_purge(&self) -> bool {
        true
    }

    /// Delete the link's user; with `purge`, its keys first.
    fn remove_application(&self, ctx: &AppCtx, entry: &str, purge: bool) -> Result<()> {
        let flavor = Flavor::find(&ctx.shared.type_name)?;
        if let (true, Some(record)) = (purge, read_record(ctx, entry)?) {
            let args = match (record.db, &record.key_prefix) {
                (Some(db), _) => format!("-n {db} FLUSHDB"),
                (None, Some(prefix)) => {
                    check_user(prefix.trim_end_matches(':'))?;
                    format!(
                        "EVAL {} 0 {}",
                        shell_quote(UNLINK_SCRIPT),
                        shell_quote(&format!("{prefix}*"))
                    )
                }
                _ => String::new(),
            };
            if !args.is_empty() {
                ctx.server
                    .connector
                    .exec(&cli_command(ctx.server, ctx.shared, flavor, &args))
                    .with_context(|| format!("deleting the keys of {} failed", record.user))?;
            }
        }
        let path = shell_quote(&ctx.server.path(&record_path(ctx, entry)));
        ctx.server.connector.exec(&format!("rm -f -- {path}"))?;
        reload_users(ctx.server, ctx.shared)
    }

    /// `dump.rdb` (a fresh snapshot) and the password. Users come back from
    /// the link records.
    fn backup_shared(
        &self,
        ctx: &ServerCtx,
        config: &SharedAppConfig,
        out: &mut ItemWriter,
    ) -> Result<()> {
        let flavor = Flavor::find(&config.type_name)?;
        let cli = flavor.cli();
        let snapshot = format!(
            "export REDISCLI_AUTH=\"${PASSWORD_VARIABLE}\"; before=$({cli} --no-auth-warning LASTSAVE); \
             {cli} --no-auth-warning BGSAVE >/dev/null || exit 1; i=0; \
             while [ \"$({cli} --no-auth-warning LASTSAVE)\" = \"$before\" ]; do \
             i=$((i+1)); [ $i -lt 600 ] || {{ echo 'the snapshot did not finish' >&2; exit 1; }}; sleep 1; done; \
             cat /data/dump.rdb"
        );
        out.stream(
            DUMP_ITEM,
            Role::Data,
            &exec_command(
                ctx,
                &config.name,
                &format!("sh -c {}", shell_quote(&snapshot)),
            ),
        )?;
        let secrets = ctx
            .read(&secrets_path(&config.name))?
            .context("the password file is missing; deploy the server again")?;
        out.file(SECRETS_ITEM, Role::Meta, secrets.as_bytes())
    }

    /// Before the first start: the password and the snapshot, without AOF
    /// files, and the marker that makes the server start without AOF:
    /// with AOF on and no AOF files, it would start empty.
    fn restore_shared_files(
        &self,
        ctx: &ServerCtx,
        config: &SharedAppConfig,
        from: &ItemReader,
    ) -> Result<()> {
        if from.has(SECRETS_ITEM) {
            ctx.connector.write_file(
                &ctx.path(&secrets_path(&config.name)),
                from.read(SECRETS_ITEM)?.as_bytes(),
                0o600,
            )?;
        }
        if from.has(DUMP_ITEM) {
            let data = ctx.path(&format!("{}/data", shared_dir(KIND, &config.name)));
            let quoted = shell_quote(&data);
            from.feed(
                DUMP_ITEM,
                b"",
                &format!("mkdir -p -- {quoted} && rm -rf -- {quoted}/appendonlydir && cat > {quoted}/dump.rdb"),
            )?;
            ctx.connector.write_file(
                &ctx.path(&restore_marker(&config.name)),
                b"restored dump.rdb waits to be loaded\n",
                0o644,
            )?;
        }
        Ok(())
    }

    /// The server loaded the snapshot; turning AOF on writes it from memory.
    fn restore_shared_data(
        &self,
        ctx: &ServerCtx,
        config: &SharedAppConfig,
        _from: &ItemReader,
    ) -> Result<()> {
        if ctx.read(&restore_marker(&config.name))?.is_none() {
            return Ok(());
        }
        let flavor = Flavor::find(&config.type_name)?;
        ctx.connector.exec(&cli_command(
            ctx,
            config,
            flavor,
            "CONFIG SET appendonly yes",
        ))?;
        let wait = format!(
            "for i in $(seq 1 600); do {} | grep -q '^aof_rewrite_in_progress:0' && exit 0; sleep 1; done; exit 1",
            cli_command(ctx, config, flavor, "INFO persistence")
        );
        ctx.connector
            .exec(&wait)
            .context("writing the append-only file did not finish")?;
        let conf = ctx.path(&conf_path(&config.name));
        ctx.connector.exec(&format!(
            "sed -i 's/^appendonly no$/appendonly yes/' {} && rm -f -- {}",
            shell_quote(&conf),
            shell_quote(&ctx.path(&restore_marker(&config.name)))
        ))?;
        Ok(())
    }
}

/// Deletes the keys matching `ARGV[1]` in batches.
const UNLINK_SCRIPT: &str = "local c = '0' repeat local r = redis.call('SCAN', c, 'MATCH', ARGV[1], 'COUNT', 1000) c = r[1] for _, k in ipairs(r[2]) do redis.call('UNLINK', k) end until c == '0' return 1";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::local::Local;

    const LOCAL: Local = Local { verbose: false };

    fn config(text: &str) -> SharedAppConfig {
        let mut config: SharedAppConfig = serde_yaml_ng::from_str(text).unwrap();
        KeyValueKind.apply_defaults(&mut config);
        config
    }

    fn record(user: &str, db: Option<u32>, prefix: Option<&str>) -> KeyValueRecord {
        KeyValueRecord {
            app: "a".into(),
            prefix: "p".into(),
            entry: "e".into(),
            user: user.into(),
            password: "pw".into(),
            db,
            key_prefix: prefix.map(String::from),
        }
    }

    #[test]
    fn acl_rules_by_flavor() {
        let valkey = acl_file(
            Flavor::Valkey,
            "root",
            &[record("shop-prod-cache", Some(3), None)],
        )
        .unwrap();
        assert!(valkey.starts_with(&format!(
            "user default on #{} ~* &* +@all\n",
            sha256_hex("root")
        )));
        assert!(valkey.contains(&format!(
            "user shop-prod-cache on #{} resetchannels &* ~* +@all -@admin -flushall -swapdb db=3\n",
            sha256_hex("pw")
        )), "{valkey}");
        assert!(
            !valkey.contains("pw\n") && !valkey.contains(" >"),
            "passwords only as hashes"
        );

        let redis = acl_file(
            Flavor::Redis,
            "root",
            &[record("shop-prod-cache", None, Some("shop-prod-cache:"))],
        )
        .unwrap();
        assert!(
            redis.contains("&shop-prod-cache:* ~shop-prod-cache:* +@all -@admin -@dangerous +info"),
            "{redis}"
        );
        assert!(acl_file(Flavor::Redis, "root", &[record("x y", None, Some("x:"))]).is_err());
        assert!(acl_file(Flavor::Redis, "root", &[record("x", Some(1), None)]).is_err());
    }

    #[test]
    fn databases_are_given_out_from_one() {
        assert_eq!(free_database(&[]).unwrap(), 1);
        let used = [record("a", Some(1), None), record("b", Some(3), None)];
        assert_eq!(free_database(&used).unwrap(), 2);
    }

    #[test]
    fn renders_server_files() {
        let home = tempfile::TempDir::new().unwrap();
        let ctx = ServerCtx {
            connector: &LOCAL,
            home: home.path().display().to_string(),
        };
        let rendered = KeyValueKind
            .render(&ctx, &config("{name: cache, type: valkey, configuration: {password: secret, max_memory: 256mb, eviction: allkeys-lru}}"))
            .unwrap();
        let file = |suffix: &str| {
            rendered
                .files
                .iter()
                .find(|f| f.path.ends_with(suffix))
                .unwrap()
        };
        let compose = &file("compose.yaml").contents;
        assert!(compose.contains("image: valkey/valkey:9"), "{compose}");
        assert!(
            compose.contains(r#"command: ["valkey-server", "/conf/server.conf"]"#),
            "{compose}"
        );
        let conf = &file("server.conf").contents;
        assert!(
            conf.contains(
                "appendonly yes\ndatabases 256\nmaxmemory 256mb\nmaxmemory-policy allkeys-lru\n"
            ),
            "{conf}"
        );
        assert!(file("password.env").is_secret());
        assert!(!file("users.acl").contents.contains("secret"));

        // While a restored snapshot waits, the server starts without AOF.
        std::fs::create_dir_all(home.path().join("shared/keyvalue/cache")).unwrap();
        std::fs::write(home.path().join(restore_marker("cache")), "x").unwrap();
        let restoring = KeyValueKind
            .render(&ctx, &config("{name: cache, type: valkey}"))
            .unwrap();
        let conf = &restoring
            .files
            .iter()
            .find(|f| f.path.ends_with("server.conf"))
            .unwrap()
            .contents;
        assert!(conf.contains("appendonly no\n"), "{conf}");
    }

    #[test]
    fn validation() {
        let err = |text: &str| {
            format!(
                "{:#}",
                KeyValueKind.validate_shared(&config(text)).unwrap_err()
            )
        };
        assert!(err("{name: c, type: valkey, version: 8}").contains("9.1 or newer"));
        assert!(
            err("{name: c, type: valkey, configuration: {eviction: lru}}")
                .contains("unknown policy")
        );
        assert!(err("{name: c, type: memcached}").contains("unknown keyvalue type"));
        KeyValueKind
            .validate_shared(&config("{name: c, type: redis}"))
            .unwrap();
    }

    #[test]
    fn password_stays_out_of_command_lines() {
        let home = tempfile::TempDir::new().unwrap();
        let ctx = ServerCtx {
            connector: &LOCAL,
            home: home.path().display().to_string(),
        };
        let command = cli_command(
            &ctx,
            &config("{name: c, type: redis}"),
            Flavor::Redis,
            "ACL LOAD",
        );
        assert!(
            command.ends_with(
                "sh -c 'REDISCLI_AUTH=\"$KV_PASSWORD\" exec redis-cli --no-auth-warning ACL LOAD'"
            ),
            "{command}"
        );
    }
}
