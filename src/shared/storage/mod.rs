//! Storage kind: S3-compatible object storage that apps link to.
//!
//! Type `garage`: compost runs Garage (single node) and gives each link its
//! own bucket and access key. Type `s3`: an S3 service compost doesn't run
//! (`external`); links name their bucket and key, and compost only passes
//! them on. Bucket contents are backed up with rclone.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail, ensure};
use minijinja::context;
use serde::{Deserialize, Serialize};
use serde_yaml_ng::Value;

use super::{
    AppCtx, Contribution, Generated, ItemReader, ItemWriter, Kind, Monitored, Rendered,
    RenderedFile, Resolution, Role, ServerCtx, SharedAppConfig, SharedLinks, app_service,
    check_link_environment, connectors_dir, host_address, link_value, parse_link, parse_url,
    project_name, render_template, service_name, shared_dir,
};
use crate::backup::remote::RCLONE_IMAGE;
use crate::config::app::AppConfig;
use crate::connector::shell_quote;
use crate::error::CompostError;

pub const KIND: &str = "storage";
const S3_PORT: u16 = 3900;
const DEFAULT_REGION: &str = "garage";
/// Image tags by major version.
const GARAGE_TAGS: &[(u32, &str)] = &[(2, "v2.3.0")];
const DEFAULT_VERSION: u32 = 2;
const BACKUP_ITEM: &str = "garage.tar";
const BACKUP_DIRS: [&str; 3] = ["meta", "data", "secrets"];

/// Values a link can map to app variables.
const LINK_VALUES: [&str; 6] = [
    "s3_endpoint",
    "s3_public_endpoint",
    "s3_region",
    "s3_bucket",
    "s3_access_key_id",
    "s3_secret_access_key",
];

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct GarageConfig {
    /// Public URL of the S3 API, served by the ingress.
    url: Option<String>,
    /// Weight of the node in Garage's layout (not a quota), e.g. `100G`.
    capacity: Option<String>,
    region: Option<String>,
    /// Publish the S3 API on 127.0.0.1 of the machine, e.g. for an app
    /// run outside Docker.
    host_port: Option<u16>,
}

/// `external` of an `s3` storage app.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct External {
    endpoint: String,
    #[serde(default)]
    region: Option<String>,
    /// Docker network to reach it on, e.g. of an existing MinIO container.
    #[serde(default)]
    network: Option<String>,
}

/// An entry under `services.storage` in compost.yaml.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StorageLink {
    name: String,
    /// Use this storage app; needed for an external one or when there are several.
    #[serde(default)]
    instance: Option<String>,
    #[serde(default)]
    targets: Vec<String>,
    #[serde(default)]
    environment: BTreeMap<String, String>,
    /// External storage: the existing bucket and key (in the overlay).
    #[serde(default)]
    bucket: Option<String>,
    #[serde(default)]
    access_key_id: Option<String>,
    #[serde(default)]
    secret_access_key: Option<String>,
}

/// `connectors/storage/<name>/<instance>-<entry>.yaml`. Secret.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StorageRecord {
    app: String,
    prefix: String,
    entry: String,
    bucket: String,
    access_key_id: String,
    secret_access_key: String,
}

fn network_name(name: &str) -> String {
    format!("compost-storage-{name}")
}

fn secrets_path(name: &str) -> String {
    format!("{}/secrets/garage.env", shared_dir(KIND, name))
}

fn external(config: &SharedAppConfig) -> Result<Option<External>> {
    match &config.external {
        None => Ok(None),
        Some(value) => serde_yaml_ng::from_value(value.clone())
            .map(Some)
            .map_err(|e| anyhow::anyhow!("external: {e}")),
    }
}

fn garage_image(config: &SharedAppConfig) -> Result<String> {
    if let Some(image) = &config.image {
        return Ok(image.clone());
    }
    let version = config.version.unwrap_or(DEFAULT_VERSION);
    let tag = GARAGE_TAGS
        .iter()
        .find(|(major, _)| *major == version)
        .map(|(_, tag)| *tag)
        .with_context(|| format!("no garage image known for version {version}; set `image`"))?;
    Ok(format!("dxflrs/garage:{tag}"))
}

/// Where apps reach a storage app: on its network inside, or the public URL.
struct Endpoints {
    internal: String,
    public: String,
    region: String,
    network: Option<String>,
}

fn endpoints(config: &SharedAppConfig) -> Result<Endpoints> {
    if let Some(external) = external(config)? {
        return Ok(Endpoints {
            internal: external.endpoint.clone(),
            public: external.endpoint,
            region: external.region.unwrap_or_else(|| "us-east-1".to_string()),
            network: external.network,
        });
    }
    let garage: GarageConfig = config.configuration()?;
    let internal = format!("http://{}:{S3_PORT}", config.name);
    let public = match garage.url.as_deref() {
        Some(url) => parse_url(url)?.url,
        None => internal.clone(),
    };
    Ok(Endpoints {
        internal,
        public,
        region: garage.region.unwrap_or_else(|| DEFAULT_REGION.to_string()),
        network: Some(network_name(&config.name)),
    })
}

/// Bucket names: `^[a-z0-9][a-z0-9-]{1,61}[a-z0-9]$` (S3's rules, without dots).
fn is_bucket_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (3..=63).contains(&bytes.len())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
        && bytes[0] != b'-'
        && bytes[bytes.len() - 1] != b'-'
}

fn record_path(ctx: &AppCtx, entry: &str) -> String {
    format!(
        "{}/{}.yaml",
        connectors_dir(KIND, &ctx.shared.name),
        ctx.link_id(entry)
    )
}

fn read_record(ctx: &AppCtx, entry: &str) -> Result<Option<StorageRecord>> {
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

/// Run `/garage args` in the storage app's container.
fn garage(ctx: &ServerCtx, name: &str, args: &str) -> Result<String> {
    ctx.connector.exec(&format!(
        "cd {} && docker compose -p {} exec -T {} /garage {args} 2>/dev/null",
        shell_quote(&ctx.path(&shared_dir(KIND, name))),
        shell_quote(&project_name(KIND, name)),
        shell_quote(&service_name(name))
    ))
}

/// A one-node layout, once: Garage stores nothing without one.
fn ensure_layout(ctx: &ServerCtx, config: &SharedAppConfig) -> Result<()> {
    let layout = garage(ctx, &config.name, "layout show")?;
    if !layout.contains("No nodes currently have a role") {
        return Ok(());
    }
    let node = garage(ctx, &config.name, "node id -q")?;
    let id = node.trim().split('@').next().unwrap_or_default();
    ensure!(
        !id.is_empty() && id.chars().all(|c| c.is_ascii_hexdigit()),
        "unexpected node id from garage: {}",
        node.trim()
    );
    let garage_config: GarageConfig = config.configuration()?;
    let capacity = garage_config.capacity.unwrap_or_else(|| "100G".to_string());
    garage(
        ctx,
        &config.name,
        &format!("layout assign -z dc1 -c {} {id}", shell_quote(&capacity)),
    )?;
    let version = layout
        .lines()
        .find_map(|l| l.strip_prefix("Current cluster layout version:"))
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(0);
    garage(
        ctx,
        &config.name,
        &format!("layout apply --version {}", version + 1),
    )?;
    out!("  {}: applied a one-node layout", config.name);
    Ok(())
}

/// A new access key in Garage's format: `GK` and 24 hexadecimal digits, and
/// a secret of 64.
fn new_key() -> (String, String) {
    let hex = || uuid::Uuid::new_v4().simple().to_string();
    (format!("GK{}", &hex()[..24]), format!("{}{}", hex(), hex()))
}

/// rclone with a link's credentials, on the storage network; `mount` is
/// mounted as `/data`. The credentials are in an env file (0600), never on a
/// command line.
fn rclone(
    ctx: &ServerCtx,
    config: &SharedAppConfig,
    record: &StorageRecord,
    mount: Option<&str>,
    args: &str,
) -> Result<String> {
    let endpoints = endpoints(config)?;
    let env = format!(
        "RCLONE_CONFIG_STORE_TYPE=s3\nRCLONE_CONFIG_STORE_PROVIDER=Other\n\
         RCLONE_CONFIG_STORE_ENDPOINT={}\nRCLONE_CONFIG_STORE_REGION={}\n\
         RCLONE_CONFIG_STORE_ACCESS_KEY_ID={}\nRCLONE_CONFIG_STORE_SECRET_ACCESS_KEY={}\n\
         RCLONE_CONFIG_STORE_NO_CHECK_BUCKET=true\nRCLONE_CONFIG=\n",
        endpoints.internal, endpoints.region, record.access_key_id, record.secret_access_key
    );
    let tmp = ctx.path("tmp");
    let env_file = format!("{tmp}/rclone-{}.env", uuid::Uuid::new_v4().simple());
    ctx.connector
        .exec(&format!("umask 077 && mkdir -p -- {}", shell_quote(&tmp)))?;
    ctx.connector.write_file(&env_file, env.as_bytes(), 0o600)?;
    let network = endpoints
        .network
        .map(|n| format!(" --network {}", shell_quote(&n)))
        .unwrap_or_default();
    let mount = mount
        .map(|dir| format!(" -v {}:/data", shell_quote(dir)))
        .unwrap_or_default();
    let env_quoted = shell_quote(&env_file);
    ctx.connector.exec(&format!(
        "docker run --rm --user \"$(id -u):$(id -g)\" --env-file {env_quoted}{network}{mount} {RCLONE_IMAGE} {args}; \
         status=$?; rm -f -- {env_quoted}; exit $status"
    ))
}

fn generated(type_name: &str) -> Generated {
    match type_name {
        "s3" => Generated {
            local: include_str!("templates/generate/s3.yaml"),
            prod: None,
            secrets: None,
            notes: &[
                "s3: give each link `bucket`, `access_key_id` and `secret_access_key` of an existing bucket (in the app's overlay).",
            ],
            requires: &[],
        },
        _ => Generated {
            local: include_str!("templates/generate/garage.yaml"),
            prod: None,
            secrets: None,
            notes: &[
                "garage: link apps with `services.storage: [{name: files, targets: […], environment: {S3_ENDPOINT: s3_endpoint, S3_BUCKET: s3_bucket, …}}]`; each gets a bucket and key. Set `url` to serve the S3 API on a domain.",
            ],
            requires: &[],
        },
    }
}

pub struct StorageKind;

impl Kind for StorageKind {
    fn name(&self) -> &'static str {
        KIND
    }

    fn type_names(&self) -> Vec<&'static str> {
        vec!["garage", "s3"]
    }

    fn generate(&self, type_name: &str) -> Result<Generated> {
        ensure!(
            ["garage", "s3"].contains(&type_name),
            "unknown storage type `{type_name}`"
        );
        Ok(generated(type_name))
    }

    fn apply_defaults(&self, config: &mut SharedAppConfig) {
        if config.type_name == "garage" && config.version.is_none() {
            config.version = Some(DEFAULT_VERSION);
        }
    }

    fn supports_external(&self) -> bool {
        true
    }

    /// compost has no credentials of its own for an external S3; each link has.
    fn check_external(&self, _ctx: &ServerCtx, _config: &SharedAppConfig) -> Result<String> {
        Ok("not checked: only the links have credentials".to_string())
    }

    fn validate_shared(&self, config: &SharedAppConfig) -> Result<()> {
        match (config.type_name.as_str(), external(config)?) {
            ("garage", None) => {
                garage_image(config)?;
                let garage: GarageConfig = config.configuration()?;
                if let Some(url) = &garage.url {
                    parse_url(url).context("url")?;
                }
                if let Some(capacity) = &garage.capacity {
                    ensure!(
                        capacity.chars().all(|c| c.is_ascii_alphanumeric()) && !capacity.is_empty(),
                        "capacity must be a size like `100G`"
                    );
                }
                if let Some(region) = &garage.region {
                    ensure!(
                        !region.is_empty()
                            && region
                                .chars()
                                .all(|c| c.is_ascii_alphanumeric() || c == '-'),
                        "region must be a word like `garage` or `eu-central-1`"
                    );
                }
            }
            ("garage", Some(_)) => {
                bail!("`external` is for type `s3`: a Garage compost doesn't run is just S3")
            }
            ("s3", Some(external)) => {
                ensure!(
                    external.endpoint.starts_with("https://")
                        || external.endpoint.starts_with("http://"),
                    "external.endpoint must start with https:// or http://"
                );
                ensure!(
                    config.image.is_none() && config.configuration.is_null(),
                    "an external s3 storage has only `external`"
                );
            }
            ("s3", None) => {
                bail!("type s3 is a storage compost doesn't run: give `external: {{endpoint: …}}`")
            }
            (other, _) => bail!("unknown storage type `{other}` (known: garage, s3)"),
        }
        ensure!(
            config.monitoring.is_none() || !config.is_external(),
            "`monitoring` is not supported with `external`"
        );
        Ok(())
    }

    fn secret_keys(&self, _config: &SharedAppConfig) -> Result<&'static [&'static str]> {
        Ok(&[])
    }

    fn monitored(&self, config: &SharedAppConfig) -> Result<Monitored> {
        Ok(Monitored {
            services: vec![app_service("garage", service_name(&config.name))],
            metrics: Vec::new(),
            traces: false,
        })
    }

    fn render(&self, ctx: &ServerCtx, config: &SharedAppConfig) -> Result<Rendered> {
        if let Some(external) = external(config)? {
            return Ok(Rendered {
                existing_networks: external.network.into_iter().collect(),
                ..Rendered::default()
            });
        }
        let garage_config: GarageConfig = config.configuration()?;
        let dir = shared_dir(KIND, &config.name);
        let secrets_path = secrets_path(&config.name);
        // Generated once, then kept: the RPC secret identifies the node.
        let existing = ctx.read(&secrets_path)?.unwrap_or_default();
        let value = |key: &str| {
            existing.lines().find_map(|l| {
                l.strip_prefix(key)
                    .and_then(|r| r.strip_prefix('='))
                    .map(String::from)
            })
        };
        let hex = || {
            format!(
                "{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            )
        };
        let rpc_secret = value("RPC_SECRET").unwrap_or_else(hex);
        let admin_token = value("ADMIN_TOKEN").unwrap_or_else(hex);
        let user = ctx.connector.exec("echo \"$(id -u):$(id -g)\"")?;
        let network = network_name(&config.name);

        let compose = render_template(
            include_str!("templates/compose.yaml.j2"),
            context! {
                image => garage_image(config)?,
                service => service_name(&config.name),
                container => project_name(KIND, &config.name),
                network => network.as_str(),
                host => config.name.as_str(),
                user => user.trim(),
                host_port => garage_config.host_port,
            },
        )?;
        let toml = render_template(
            include_str!("templates/garage.toml.j2"),
            context! {
                rpc_secret => rpc_secret.as_str(),
                admin_token => admin_token.as_str(),
                region => garage_config.region.as_deref().unwrap_or(DEFAULT_REGION),
            },
        )?;
        Ok(Rendered {
            files: vec![
                RenderedFile::new(format!("{dir}/compose.yaml"), compose),
                RenderedFile::secret(format!("{dir}/garage.toml"), toml),
                RenderedFile::secret(
                    secrets_path,
                    format!("RPC_SECRET={rpc_secret}\nADMIN_TOKEN={admin_token}\n"),
                ),
            ],
            dirs: vec![format!("{dir}/meta"), format!("{dir}/data")],
            networks: vec![network],
            ..Rendered::default()
        })
    }

    /// With `url`: the S3 API on the ingress.
    fn shared_links(&self, config: &SharedAppConfig) -> Result<Option<SharedLinks>> {
        if config.is_external() {
            return Ok(None);
        }
        let garage: GarageConfig = config.configuration()?;
        let Some(url) = garage.url.as_deref().map(parse_url).transpose()? else {
            return Ok(None);
        };
        let ingress: Value = serde_yaml_ng::from_str(&format!(
            "{{name: s3, domains: ['{}'], routes: [{{name: s3, target: garage, port: {S3_PORT}}}]}}",
            url.domain
        ))?;
        Ok(Some(SharedLinks {
            app_services: vec![app_service("garage", service_name(&config.name))],
            services: BTreeMap::from([("ingress".to_string(), vec![ingress])]),
        }))
    }

    fn apply_order(&self) -> i32 {
        12 // with the databases: before the apps that use it
    }

    fn link_secret_keys(&self) -> &'static [&'static str] {
        &["access_key_id", "secret_access_key"]
    }

    fn validate_link(&self, link: &Value, app: &AppConfig) -> Result<()> {
        let link: StorageLink = parse_link(link)?;
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
        check_link_environment(&link.environment, &LINK_VALUES)?;
        if let Some(bucket) = &link.bucket {
            ensure!(
                is_bucket_name(bucket),
                "bucket `{bucket}` must be 3–63 lowercase letters, digits and `-`"
            );
        }
        Ok(())
    }

    /// A link uses an external storage only when it names it, as for databases.
    fn resolve<'s>(
        &self,
        link: &Value,
        available: &'s [SharedAppConfig],
    ) -> Result<Resolution<'s>> {
        let link: StorageLink = parse_link(link)?;
        let pin = link.instance.as_deref();
        let matching: Vec<&SharedAppConfig> = available
            .iter()
            .filter(|app| match pin {
                Some(pin) => app.name == pin,
                None => !app.is_external(),
            })
            .collect();
        match matching.as_slice() {
            [found] => Ok(Resolution::Use(found)),
            [] => match pin {
                Some(pin) => bail!("no storage app named `{pin}` on the server"),
                None => bail!(
                    "no storage app runs on the server; add one under shared.storage of compost-server.yaml (or pin an external one with `instance`)"
                ),
            },
            several => {
                let names: Vec<&str> = several.iter().map(|a| a.name.as_str()).collect();
                bail!(
                    "several storage apps run ({}); choose one with `instance`",
                    names.join(", ")
                )
            }
        }
    }

    /// Join the network, set the variables, write the link record. For
    /// Garage, the bucket is `<instance>-<entry>` and the key a new one, which
    /// `before_apply_application` creates in Garage.
    fn prepare_application(
        &self,
        ctx: &AppCtx,
        link: &Value,
        out: &mut Contribution,
    ) -> Result<()> {
        let link: StorageLink = parse_link(link)?;
        let record = if ctx.shared.is_external() {
            let (Some(bucket), Some(key_id), Some(secret)) = (
                link.bucket.clone(),
                link.access_key_id.clone(),
                link.secret_access_key.clone(),
            ) else {
                bail!(
                    "{} is external: give `bucket`, `access_key_id` and `secret_access_key` (usually in the overlay)",
                    ctx.shared.name
                );
            };
            StorageRecord {
                app: ctx.app.name.clone(),
                prefix: ctx.app.prefix.clone(),
                entry: link.name.clone(),
                bucket,
                access_key_id: key_id,
                secret_access_key: secret,
            }
        } else {
            ensure!(
                link.bucket.is_none() && link.access_key_id.is_none(),
                "bucket and keys are only given for an external storage; Garage makes its own"
            );
            match read_record(ctx, &link.name)? {
                Some(record) => record,
                None => {
                    let bucket = ctx.link_id(&link.name);
                    ensure!(
                        is_bucket_name(&bucket),
                        "the bucket name `{bucket}` is not valid S3 (3–63 characters); use a shorter entry name"
                    );
                    let (access_key_id, secret_access_key) = new_key();
                    StorageRecord {
                        app: ctx.app.name.clone(),
                        prefix: ctx.app.prefix.clone(),
                        entry: link.name.clone(),
                        bucket,
                        access_key_id,
                        secret_access_key,
                    }
                }
            }
        };
        set_variables(ctx, &link, &record, out)?;
        let text = serde_yaml_ng::to_string(&record)?;
        ctx.server.connector.write_file(
            &ctx.server.path(&record_path(ctx, &link.name)),
            text.as_bytes(),
            0o600,
        )
    }

    fn before_apply_application(&self, ctx: &AppCtx, link: &Value) -> Result<()> {
        if ctx.shared.is_external() {
            return Ok(());
        }
        let link: StorageLink = parse_link(link)?;
        let record = read_record(ctx, &link.name)?
            .context("the link record is missing; deploy the app again")?;
        provision(ctx.server, ctx.shared, &record)
    }

    fn after_apply_application(&self, _ctx: &AppCtx, _link: &Value) -> Result<()> {
        Ok(())
    }

    fn describe_link(&self, ctx: &AppCtx, entry: &str) -> Result<String> {
        Ok(match read_record(ctx, entry)? {
            Some(record) => format!("bucket {}, key {}", record.bucket, record.access_key_id),
            None => "no link record".to_string(),
        })
    }

    fn supports_purge(&self) -> bool {
        true
    }

    /// Delete the link's key; with `purge` also its bucket and objects.
    /// An external storage is never touched.
    fn remove_application(&self, ctx: &AppCtx, entry: &str, purge: bool) -> Result<()> {
        if let (false, Some(record)) = (ctx.shared.is_external(), read_record(ctx, entry)?) {
            if purge {
                rclone(
                    ctx.server,
                    ctx.shared,
                    &record,
                    None,
                    &format!("purge {}", shell_quote(&format!("store:{}", record.bucket))),
                )
                .with_context(|| format!("deleting bucket {} failed", record.bucket))?;
            }
            // A key that was never made in Garage (a deploy that failed first) has nothing to delete.
            let key = shell_quote(&record.access_key_id);
            if garage(ctx.server, &ctx.shared.name, &format!("key info {key}")).is_ok() {
                garage(
                    ctx.server,
                    &ctx.shared.name,
                    &format!("key delete --yes {key}"),
                )
                .with_context(|| format!("deleting key {} failed", record.access_key_id))?;
            }
        } else if purge && ctx.shared.is_external() {
            eprintln!(
                "warning: the bucket of {entry} on {} is kept: compost doesn't delete data on external storage",
                ctx.shared.name
            );
        }
        let path = shell_quote(&ctx.server.path(&record_path(ctx, entry)));
        ctx.server.connector.exec(&format!("rm -f -- {path}"))?;
        Ok(())
    }

    /// Metadata, data and secrets, copied while Garage is stopped: its
    /// database must not change meanwhile.
    fn backup_shared(
        &self,
        ctx: &ServerCtx,
        config: &SharedAppConfig,
        out: &mut ItemWriter,
    ) -> Result<()> {
        let dir = shell_quote(&ctx.path(&shared_dir(KIND, &config.name)));
        let compose = format!(
            "docker compose -p {}",
            shell_quote(&project_name(KIND, &config.name))
        );
        let service = shell_quote(&service_name(&config.name));
        out.stream(
            BACKUP_ITEM,
            Role::Data,
            &format!(
                "cd {dir} || exit 1; running=$({compose} ps --status running -q {service}); \
                 if [ -n \"$running\" ]; then {compose} stop {service} >/dev/null 2>&1; fi; \
                 tar -cf - {}; status=$?; \
                 if [ -n \"$running\" ]; then {compose} start {service} >/dev/null 2>&1; fi; \
                 exit $status",
                BACKUP_DIRS.join(" ")
            ),
        )
    }

    /// Unpack next to the folders first, then swap them in.
    fn restore_shared_files(
        &self,
        ctx: &ServerCtx,
        config: &SharedAppConfig,
        from: &ItemReader,
    ) -> Result<()> {
        if !from.has(BACKUP_ITEM) {
            return Ok(());
        }
        let dir = shell_quote(&ctx.path(&shared_dir(KIND, &config.name)));
        let mut script = format!(
            "mkdir -p -- {dir} && cd {dir} && rm -rf .restore && mkdir .restore && tar -xf - -C .restore"
        );
        for name in BACKUP_DIRS {
            script.push_str(&format!(
                " && if [ -d .restore/{name} ]; then rm -rf {name} && mv .restore/{name} {name}; fi"
            ));
        }
        script.push_str(" && rm -rf .restore");
        from.feed(BACKUP_ITEM, b"", &script)
    }

    /// `<entry>.tar`: the bucket's objects, fetched with rclone.
    fn backup_link(&self, ctx: &AppCtx, entry: &str, out: &mut ItemWriter) -> Result<()> {
        let Some(record) = read_record(ctx, entry)? else {
            return Ok(());
        };
        let dir = ctx
            .server
            .path(&format!("tmp/storage-{}", uuid::Uuid::new_v4().simple()));
        ctx.server
            .connector
            .exec(&format!("umask 077 && mkdir -p -- {}", shell_quote(&dir)))?;
        let fetched = rclone(
            ctx.server,
            ctx.shared,
            &record,
            Some(&dir),
            &format!(
                "copy {} /data",
                shell_quote(&format!("store:{}", record.bucket))
            ),
        );
        let streamed = fetched.and_then(|_| {
            out.stream(
                &format!("{entry}.tar"),
                Role::Data,
                &format!("tar -cf - -C {} .", shell_quote(&dir)),
            )
        });
        ctx.server
            .connector
            .exec(&format!("rm -rf -- {}", shell_quote(&dir)))?;
        streamed.with_context(|| format!("bucket {}", record.bucket))
    }

    /// Put the objects back; a bucket that holds objects is replaced (made
    /// equal to the backup) only with `replace`.
    fn restore_link(
        &self,
        ctx: &AppCtx,
        entry: &str,
        from: &ItemReader,
        replace: bool,
    ) -> Result<()> {
        let item = format!("{entry}.tar");
        if !from.has(&item) {
            return Ok(());
        }
        let record = read_record(ctx, entry)?
            .with_context(|| format!("no link record for `{entry}`; deploy the app first"))?;
        if !ctx.shared.is_external() {
            provision(ctx.server, ctx.shared, &record)?;
        }
        let remote = shell_quote(&format!("store:{}", record.bucket));
        let listing = rclone(
            ctx.server,
            ctx.shared,
            &record,
            None,
            &format!("lsf --max-depth 1 {remote}"),
        )?;
        if !listing.trim().is_empty() && !replace {
            return Err(CompostError::ServerNotReady(format!(
                "bucket {} is not empty; add --replace to replace its objects",
                record.bucket
            ))
            .into());
        }
        out!("  loading bucket {} ...", record.bucket);
        let dir = ctx
            .server
            .path(&format!("tmp/storage-{}", uuid::Uuid::new_v4().simple()));
        let quoted = shell_quote(&dir);
        from.feed(
            &item,
            b"",
            &format!("umask 077 && mkdir -p -- {quoted} && tar -xf - -C {quoted}"),
        )?;
        let loaded = rclone(
            ctx.server,
            ctx.shared,
            &record,
            Some(&dir),
            &format!("sync /data {remote}"),
        );
        ctx.server.connector.exec(&format!("rm -rf -- {quoted}"))?;
        loaded
            .map(|_| ())
            .with_context(|| format!("loading bucket {} failed", record.bucket))
    }
}

/// The link's variables on its targets, and the storage network.
fn set_variables(
    ctx: &AppCtx,
    link: &StorageLink,
    record: &StorageRecord,
    out: &mut Contribution,
) -> Result<()> {
    let endpoints = endpoints(ctx.shared)?;
    let internal = if out.is_for_host() && !ctx.shared.is_external() {
        let (host, port) = host_address(ctx.shared)?;
        format!("http://{host}:{port}")
    } else {
        endpoints.internal
    };
    let values: BTreeMap<&str, String> = BTreeMap::from([
        ("s3_endpoint", internal),
        ("s3_public_endpoint", endpoints.public),
        ("s3_region", endpoints.region),
        ("s3_bucket", record.bucket.clone()),
        ("s3_access_key_id", record.access_key_id.clone()),
        ("s3_secret_access_key", record.secret_access_key.clone()),
    ]);
    for target in &link.targets {
        let service = ctx.compose_service(target)?;
        if let Some(network) = &endpoints.network {
            out.join_network(service, network, None);
        }
        for (variable, value) in &link.environment {
            out.set_env(service, variable, link_value(value, &values)?)?;
        }
    }
    Ok(())
}

/// The bucket and key of a Garage link in Garage, made if missing. The key
/// is imported through the admin API's JSON on stdin, never a command line.
fn provision(ctx: &ServerCtx, config: &SharedAppConfig, record: &StorageRecord) -> Result<()> {
    ensure_layout(ctx, config)?;
    let name = &config.name;
    let bucket = shell_quote(&record.bucket);
    if garage(ctx, name, &format!("bucket info {bucket}")).is_err() {
        garage(ctx, name, &format!("bucket create {bucket}"))
            .with_context(|| format!("creating bucket {} failed", record.bucket))?;
    }
    let key = shell_quote(&record.access_key_id);
    if garage(ctx, name, &format!("key info {key}")).is_err() {
        let payload = serde_json::json!({
            "accessKeyId": record.access_key_id,
            "secretAccessKey": record.secret_access_key,
            "name": record.bucket,
        })
        .to_string();
        let script = format!(
            "cd {} && docker compose -p {} exec -T {} /garage json-api ImportKey - 2>&1 >/dev/null",
            shell_quote(&ctx.path(&shared_dir(KIND, name))),
            shell_quote(&project_name(KIND, name)),
            shell_quote(&service_name(name))
        );
        let output = ctx.connector.run(&script, Some(payload.as_bytes()))?;
        if !output.success() {
            bail!(
                "creating key {} failed: {}",
                record.access_key_id,
                output.stdout.trim()
            );
        }
    }
    garage(
        ctx,
        name,
        &format!("bucket allow --read --write --owner {bucket} --key {key}"),
    )
    .with_context(|| format!("giving key {} its bucket failed", record.access_key_id))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(text: &str) -> SharedAppConfig {
        let mut config: SharedAppConfig = serde_yaml_ng::from_str(text).unwrap();
        StorageKind.apply_defaults(&mut config);
        config
    }

    #[test]
    fn validation() {
        let err = |text: &str| {
            format!(
                "{:#}",
                StorageKind.validate_shared(&config(text)).unwrap_err()
            )
        };
        StorageKind
            .validate_shared(&config("{name: storage, type: garage}"))
            .unwrap();
        StorageKind
            .validate_shared(&config(
                "{name: s3, type: s3, external: {endpoint: 'https://s3.example.com', region: eu}}",
            ))
            .unwrap();
        assert!(err("{name: s3, type: s3}").contains("give `external"));
        assert!(
            err("{name: g, type: garage, external: {endpoint: 'https://x'}}")
                .contains("`external` is for type `s3`")
        );
        assert!(
            err("{name: g, type: garage, version: 1}")
                .contains("no garage image known for version 1")
        );
        assert!(
            err("{name: g, type: garage, configuration: {url: 'files.example.com'}}")
                .contains("url")
        );
        assert!(
            err("{name: s3, type: s3, external: {endpoint: 's3.example.com'}}")
                .contains("https://")
        );
    }

    #[test]
    fn bucket_names() {
        assert!(is_bucket_name("shop-prod-files"));
        assert!(!is_bucket_name("ab"));
        assert!(!is_bucket_name("Shop"));
        assert!(!is_bucket_name("-shop"));
        assert!(!is_bucket_name(&"a".repeat(64)));
    }

    #[test]
    fn keys_in_garage_format() {
        let (id, secret) = new_key();
        assert!(id.starts_with("GK") && id.len() == 26, "{id}");
        assert_eq!(secret.len(), 64);
        assert!(
            id[2..]
                .chars()
                .chain(secret.chars())
                .all(|c| c.is_ascii_hexdigit())
        );
    }

    #[test]
    fn endpoints_inside_and_public() {
        let own = endpoints(&config(
            "{name: storage, type: garage, configuration: {url: 'https://files.example.com/'}}",
        ))
        .unwrap();
        assert_eq!(own.internal, "http://storage:3900");
        assert_eq!(own.public, "https://files.example.com");
        assert_eq!(own.region, "garage");
        assert_eq!(own.network.as_deref(), Some("compost-storage-storage"));
        let aws = endpoints(&config("{name: s3, type: s3, external: {endpoint: 'https://s3.eu-central-1.amazonaws.com', region: eu-central-1}}")).unwrap();
        assert_eq!(aws.internal, aws.public);
        assert_eq!(aws.network, None);
    }
}
