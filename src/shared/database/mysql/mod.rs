//! MariaDB and MySQL database types: one implementation, two flavors. They
//! share their SQL for databases, users and grants, and differ in images,
//! tool names and environment variables.

use std::fmt::Write as _;
use std::io::Read;

use anyhow::{Context, Result, bail, ensure};
use minijinja::context;
use serde::{Deserialize, Serialize};

use super::base::{self, DatabaseLink, DatabaseRecord, Endpoint, External, Purge};
use super::{DatabaseType, KIND, peek};
use crate::config::app::AppConfig;
use crate::connector::shell_quote;
use crate::error::CompostError;
use crate::shared::{
    AppCtx, Generated, ItemReader, ItemWriter, Monitored, Rendered, RenderedFile, Role, ServerCtx,
    SharedAppConfig, app_service, connectors_dir, generate_password, project_name, render_template,
    service_name, shared_dir,
};

pub static MARIADB: MySql = MySql {
    flavor: Flavor::MariaDb,
};
pub static MYSQL: MySql = MySql {
    flavor: Flavor::MySql,
};

pub struct MySql {
    flavor: Flavor,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Flavor {
    MariaDb,
    MySql,
}

/// MySQL takes user names up to 32 characters; MariaDB more, but the same
/// limit keeps databases movable between them.
const USER_MAX: usize = 32;
const NAME_MAX: usize = 64;

impl Flavor {
    fn type_name(self) -> &'static str {
        match self {
            Flavor::MariaDb => "mariadb",
            Flavor::MySql => "mysql",
        }
    }

    fn default_version(self) -> u32 {
        match self {
            Flavor::MariaDb => 11,
            Flavor::MySql => 8,
        }
    }

    /// The official image of a major version.
    fn image(self, version: u32) -> String {
        format!("{}:{version}", self.type_name())
    }

    fn client(self) -> &'static str {
        match self {
            Flavor::MariaDb => "mariadb",
            Flavor::MySql => "mysql",
        }
    }

    fn dump(self) -> &'static str {
        match self {
            Flavor::MariaDb => "mariadb-dump",
            Flavor::MySql => "mysqldump",
        }
    }

    fn admin(self) -> &'static str {
        match self {
            Flavor::MariaDb => "mariadb-admin",
            Flavor::MySql => "mysqladmin",
        }
    }

    /// Dump options besides the database: consistent without locking
    /// InnoDB tables, with routines and triggers, without what needs more
    /// than the database's own rights.
    fn dump_options(self) -> &'static str {
        match self {
            Flavor::MariaDb => "--single-transaction --routines --triggers --no-tablespaces",
            Flavor::MySql => {
                "--single-transaction --routines --triggers --no-tablespaces --set-gtid-purged=OFF"
            }
        }
    }

    /// The image's variable with the root password.
    fn password_variable(self) -> &'static str {
        match self {
            Flavor::MariaDb => "MARIADB_ROOT_PASSWORD",
            Flavor::MySql => "MYSQL_ROOT_PASSWORD",
        }
    }

    fn env_file(self) -> String {
        format!("{}.env", self.type_name())
    }

    /// `healthcheck.test` of the compose file. Over TCP: while the image
    /// initializes a new data folder, a temporary server answers on the socket only.
    fn healthcheck(self) -> &'static str {
        match self {
            Flavor::MariaDb => r#"["CMD", "healthcheck.sh", "--connect", "--innodb_initialized"]"#,
            Flavor::MySql => r#"["CMD-SHELL", "mysqladmin ping -h 127.0.0.1 --silent"]"#,
        }
    }

    /// Client options for an external server's `sslmode` (Postgres' names, as
    /// for every database type).
    fn ssl_options(self, sslmode: Option<&str>) -> &'static str {
        match (self, sslmode) {
            // MariaDB 11.4+ clients verify certificates by default; without a
            // password they can't trust the server's own self-signed one.
            (Flavor::MariaDb, None | Some("allow" | "prefer")) => " --skip-ssl-verify-server-cert",
            (Flavor::MySql, None | Some("allow" | "prefer")) => "",
            (Flavor::MariaDb, Some("disable")) => " --skip-ssl",
            (Flavor::MariaDb, Some("require")) => " --ssl --skip-ssl-verify-server-cert",
            (Flavor::MariaDb, Some(_)) => " --ssl --ssl-verify-server-cert",
            (Flavor::MySql, Some("disable")) => " --ssl-mode=DISABLED",
            (Flavor::MySql, Some("require")) => " --ssl-mode=REQUIRED",
            (Flavor::MySql, Some("verify-ca")) => " --ssl-mode=VERIFY_CA",
            (Flavor::MySql, Some(_)) => " --ssl-mode=VERIFY_IDENTITY",
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct MySqlConfig {
    /// Secret; generated on first deploy when absent. For an external server:
    /// optional, and with it compost creates databases there.
    root_password: Option<String>,
    /// Administrator of an external server; default `root`.
    root_user: Option<String>,
    /// Publish the server's port on 127.0.0.1 of the machine, e.g. for an app
    /// run outside Docker.
    host_port: Option<u16>,
}

/// `root-connector.yaml` of an external server: how to reach it, and the
/// root credentials when the config has them. Secret.
#[derive(Debug, Serialize, Deserialize)]
struct ExternalRecord {
    #[serde(rename = "type")]
    type_name: String,
    instance: String,
    version: Option<u32>,
    host: String,
    port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    network: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sslmode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    root_user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    root_password: Option<String>,
}

const DUMPALL_ITEM: &str = "dumpall.sql";

impl DatabaseType for MySql {
    fn type_name(&self) -> &'static str {
        self.flavor.type_name()
    }

    fn default_version(&self) -> Option<u32> {
        Some(self.flavor.default_version())
    }

    fn monitored(&self, config: &SharedAppConfig) -> Result<Monitored> {
        Ok(Monitored {
            services: vec![app_service(self.type_name(), service_name(&config.name))],
            metrics: Vec::new(),
            traces: false,
        })
    }

    fn generate(&self) -> Generated {
        let (local, secrets, notes): (&'static str, &'static str, &'static [&'static str]) =
            match self.flavor {
                Flavor::MariaDb => (
                    include_str!("templates/generate/mariadb.yaml"),
                    include_str!("templates/generate/mariadb.secrets.yaml"),
                    &[
                        "mariadb: set `root_password` in .compost-server.secrets.yaml (copy the example). Optional: without it compost generates one on the first deploy; it cannot be changed later.",
                    ],
                ),
                Flavor::MySql => (
                    include_str!("templates/generate/mysql.yaml"),
                    include_str!("templates/generate/mysql.secrets.yaml"),
                    &[
                        "mysql: set `root_password` in .compost-server.secrets.yaml (copy the example). Optional: without it compost generates one on the first deploy; it cannot be changed later.",
                    ],
                ),
            };
        Generated {
            local,
            prod: None,
            secrets: Some(secrets),
            notes,
            requires: &[],
        }
    }

    fn endpoint(&self) -> Endpoint {
        Endpoint {
            url_scheme: "mysql",
            port: 3306,
            sslmode_in_url: false,
            user_max: USER_MAX,
        }
    }

    /// Extends the generic link validation: names must be plain identifiers
    /// within MySQL's limits.
    fn validate_link(&self, link: &DatabaseLink, app: &AppConfig) -> Result<()> {
        base::validate_link(link, app)?;
        let (name, user) = base::names(app, link, USER_MAX);
        for (key, value, max) in [("db_name", &name, NAME_MAX), ("db_user", &user, USER_MAX)] {
            ensure!(
                is_identifier(value, max),
                "{key} `{value}` must match ^[a-z_][a-z0-9_]{{0,{}}}$ (set {key} to a shorter name if the default is too long)",
                max - 1
            );
        }
        Ok(())
    }

    /// An external server: only with root credentials.
    fn can_provision(&self, ctx: &ServerCtx, config: &SharedAppConfig) -> Result<bool> {
        Ok(!config.is_external() || Client::root(ctx, config, self.flavor)?.is_some())
    }

    /// Create the database and user if missing, and set the user's password.
    /// On an external server without root credentials they exist already.
    fn before_apply_application(&self, ctx: &AppCtx, link: &DatabaseLink) -> Result<()> {
        let Some(root) = Client::root(ctx.server, ctx.shared, self.flavor)? else {
            return Ok(());
        };
        let credentials = base::credentials(ctx, link, true, &self.endpoint())?;
        let sql = provisioning_sql(&credentials.name, &credentials.user, &credentials.password);
        root.run_sql(&sql)
            .map_err(|e| e.context(format!("creating database `{}` failed", credentials.name)))
    }

    /// On an external server only with root credentials and `allow_purge`;
    /// otherwise the database is kept, with a warning.
    fn drop_database(&self, ctx: &AppCtx, record: &DatabaseRecord, purge: Purge) -> Result<()> {
        check_record(record)?;
        let allowed = base::external(ctx.shared)?.is_none_or(|external| external.allow_purge);
        let root = match Client::root(ctx.server, ctx.shared, self.flavor)? {
            Some(root) if allowed => root,
            _ => {
                eprintln!(
                    "warning: database `{}` on {} is kept: it is external (purging needs root credentials and `external.allow_purge: true`)",
                    record.db_name, ctx.shared.name
                );
                return Ok(());
            }
        };
        if purge.database {
            let sql = format!("DROP DATABASE IF EXISTS `{}`;\n", record.db_name);
            root.run_sql(&sql)
                .map_err(|e| e.context(format!("dropping database `{}` failed", record.db_name)))?;
        }
        if purge.user {
            let sql = format!("DROP USER IF EXISTS '{}'@'%';\n", record.db_user);
            root.run_sql(&sql)
                .map_err(|e| e.context(format!("dropping user `{}` failed", record.db_user)))?;
        }
        Ok(())
    }

    /// `mysqladmin ping` from a client container on the server's network;
    /// it answers without credentials.
    fn check_external(&self, ctx: &ServerCtx, config: &SharedAppConfig) -> Result<String> {
        let client = Client::external(ctx, config, self.flavor, None)?;
        let output = ctx
            .connector
            .run(&client.command(self.flavor.admin(), "ping"), None)?;
        if output.success() {
            Ok("accepting connections".to_string())
        } else {
            bail!(
                "{}",
                format!("{} {}", output.stdout.trim(), output.stderr.trim()).trim()
            )
        }
    }

    /// Plain SQL without `DEFINER` clauses, so it loads into a database with
    /// another name and user (e.g. a copy under another prefix).
    fn dump_database(
        &self,
        ctx: &AppCtx,
        record: &DatabaseRecord,
        name: &str,
        out: &mut ItemWriter,
    ) -> Result<()> {
        check_record(record)?;
        let client = Client::user(ctx.server, ctx.shared, self.flavor, record)?;
        let dump = client.command(
            self.flavor.dump(),
            &format!("{} {}", self.flavor.dump_options(), record.db_name),
        );
        out.stream(name, Role::Data, &without_definers(&dump))
    }

    /// Recreate a database that holds tables only with `replace`. The database
    /// and user are created first when missing.
    fn load_database(
        &self,
        ctx: &AppCtx,
        record: &DatabaseRecord,
        name: &str,
        from: &ItemReader,
        replace: bool,
    ) -> Result<()> {
        if self.prepare_load(ctx, record, replace)? {
            return Err(CompostError::ServerNotReady(format!(
                "database `{}` is not empty; add --replace to drop and recreate it",
                record.db_name
            ))
            .into());
        }
        out!("  loading database {} ...", record.db_name);
        let client = Client::user(ctx.server, ctx.shared, self.flavor, record)?;
        from.feed(
            name,
            b"",
            &client.command(self.flavor.client(), &record.db_name),
        )
        .with_context(|| format!("loading database `{}` failed", record.db_name))
    }

    /// Plain SQL as the link user, without `DEFINER` clauses, which name
    /// users of the server the dump comes from.
    fn import_database(
        &self,
        ctx: &AppCtx,
        record: &DatabaseRecord,
        input: &mut (dyn Read + Send),
        clean: bool,
    ) -> Result<()> {
        self.prepare_load(ctx, record, clean)?;
        let (archive, mut input) = peek(input, b"PGDMP")?;
        ensure!(
            !archive,
            "this is a Postgres archive (pg_dump -Fc); {} takes plain SQL",
            self.type_name()
        );
        let client = Client::user(ctx.server, ctx.shared, self.flavor, record)?;
        let script = format!(
            "sed -e 's/ DEFINER=`[^`]*`@`[^`]*`//g' | ({})",
            client.command(self.flavor.client(), &record.db_name)
        );
        ctx.server
            .connector
            .stream_in(&script, &mut input)
            .with_context(|| format!("importing into database `{}` failed", record.db_name))
    }

    fn export_database(
        &self,
        ctx: &AppCtx,
        record: &DatabaseRecord,
        out: &mut dyn std::io::Write,
    ) -> Result<()> {
        check_record(record)?;
        let client = Client::user(ctx.server, ctx.shared, self.flavor, record)?;
        let dump = client.command(
            self.flavor.dump(),
            &format!("{} {}", self.flavor.dump_options(), record.db_name),
        );
        ctx.server
            .connector
            .stream_out(&without_definers(&dump), out)
    }

    /// `dumpall.sql` with every database, user and grant, and the root
    /// password, which must match the one in the dump.
    fn backup_shared(
        &self,
        ctx: &ServerCtx,
        config: &SharedAppConfig,
        out: &mut ItemWriter,
    ) -> Result<()> {
        let client = Client::container(ctx, config, self.flavor);
        let options = format!("--all-databases --events {}", self.flavor.dump_options());
        out.stream(
            DUMPALL_ITEM,
            Role::Data,
            &client.command(self.flavor.dump(), &options),
        )?;
        let secrets = ctx
            .read(&self.secrets_path(&config.name))?
            .context("the root password file is missing; deploy the server again")?;
        out.file(&self.flavor.env_file(), Role::Meta, secrets.as_bytes())
    }

    /// Put the root password back before the first start, so the data folder
    /// is created with it.
    fn restore_shared_files(
        &self,
        ctx: &ServerCtx,
        config: &SharedAppConfig,
        from: &ItemReader,
    ) -> Result<()> {
        let item = self.flavor.env_file();
        if from.has(&item) {
            let secrets = from.read(&item)?;
            ctx.connector.write_file(
                &ctx.path(&self.secrets_path(&config.name)),
                secrets.as_bytes(),
                0o600,
            )?;
        }
        Ok(())
    }

    fn restore_shared_data(
        &self,
        ctx: &ServerCtx,
        config: &SharedAppConfig,
        from: &ItemReader,
    ) -> Result<()> {
        if !from.has(DUMPALL_ITEM) {
            return Ok(());
        }
        let client = Client::container(ctx, config, self.flavor);
        // While the image initializes a new data folder, a temporary server
        // answers on the socket only; wait for the real one, on TCP.
        let ping = client.command(self.flavor.admin(), "ping -h 127.0.0.1 --silent");
        ctx.connector
            .exec(&format!(
                "for i in $(seq 1 90); do {ping} >/dev/null 2>&1 && exit 0; sleep 1; done; exit 1"
            ))
            .with_context(|| format!("{} did not accept connections", self.type_name()))?;
        from.feed(DUMPALL_ITEM, b"", &client.command(self.flavor.client(), ""))
            .with_context(|| format!("loading the databases of {} failed", config.name))?;
        client.run_sql("FLUSH PRIVILEGES;\n")
    }

    /// Extends the generic database validation with the type's own settings.
    fn validate_config(&self, config: &SharedAppConfig) -> Result<()> {
        base::validate_config(config)?;
        let mysql: MySqlConfig = config.configuration()?;
        if let Some(password) = &mysql.root_password {
            ensure!(!password.is_empty(), "root_password must not be empty");
            // Written single-quoted into an env file, a YAML file and option files.
            ensure!(
                !password.contains(['\n', '\r', '\'', '"', '\\']),
                "root_password must be a single line without quotes or `\\`"
            );
        }
        if let Some(user) = &mysql.root_user {
            ensure!(
                config.is_external(),
                "root_user is only used with `external`; compost's own server uses `root`"
            );
            ensure!(
                !user.is_empty() && !user.contains(|c: char| c.is_whitespace() || c == '\''),
                "root_user must be a user name"
            );
        }
        Ok(())
    }

    fn secret_keys(&self) -> &'static [&'static str] {
        &["root_password"]
    }

    fn render(&self, ctx: &ServerCtx, config: &SharedAppConfig) -> Result<Rendered> {
        let mysql: MySqlConfig = config.configuration()?;
        if let Some(external) = base::external(config)? {
            return render_external(ctx, config, mysql, external, &self.endpoint());
        }
        let Some(version) = config.version else {
            bail!("version is required for databases");
        };
        let image = config
            .image
            .clone()
            .unwrap_or_else(|| self.flavor.image(version));
        let dir = shared_dir(KIND, &config.name);
        let secrets_path = self.secrets_path(&config.name);
        let variable = self.flavor.password_variable();

        // The root password only takes effect when the data folder is first
        // created, so it is generated once and then reused.
        let existing = ctx
            .read(&secrets_path)?
            .and_then(|text| read_variable(&text, variable));
        let password = match (mysql.root_password, existing) {
            (Some(new), Some(old)) if new != old => bail!(
                "root_password differs from the one already on the server; \
                 changing it is not supported yet"
            ),
            (Some(password), _) | (None, Some(password)) => password,
            (None, None) => generate_password(),
        };

        let network = base::network_name(&config.name);
        let compose = render_template(
            include_str!("templates/compose.yaml.j2"),
            context! {
                image,
                service => service_name(&config.name),
                container => project_name(KIND, &config.name),
                network => network.as_str(),
                host => config.name.as_str(),
                env_file => self.flavor.env_file(),
                healthcheck => self.flavor.healthcheck(),
                host_port => mysql.host_port,
            },
        )?;
        let root_connector = render_template(
            include_str!("templates/root-connector.yaml.j2"),
            context! {
                type_name => self.type_name(),
                name => config.name.as_str(),
                version,
                network => network.as_str(),
                host => config.name.as_str(),
                password => password.as_str(),
            },
        )?;

        Ok(Rendered {
            files: vec![
                RenderedFile::new(format!("{dir}/compose.yaml"), compose),
                RenderedFile::secret(secrets_path, format!("{variable}='{password}'\n")),
                RenderedFile::secret(root_record_path(&config.name), root_connector),
            ],
            dirs: vec![format!("{dir}/data")],
            ..Default::default()
        })
    }
}

impl MySql {
    fn secrets_path(&self, name: &str) -> String {
        format!(
            "{}/secrets/{}",
            shared_dir(KIND, name),
            self.flavor.env_file()
        )
    }

    /// Create the link's database and user when missing. A database that
    /// holds tables is dropped and recreated with `replace`. Returns whether
    /// it holds tables afterwards.
    fn prepare_load(&self, ctx: &AppCtx, record: &DatabaseRecord, replace: bool) -> Result<bool> {
        check_record(record)?;
        let root = Client::root(ctx.server, ctx.shared, self.flavor)?;
        let provisioning = provisioning_sql(&record.db_name, &record.db_user, &record.db_password);
        if let Some(root) = &root {
            root.run_sql(&provisioning)
                .with_context(|| format!("creating database `{}` failed", record.db_name))?;
        }
        let client = Client::user(ctx.server, ctx.shared, self.flavor, record)?;
        let tables = client
            .query(&format!(
                "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = '{}';",
                record.db_name
            ))
            .with_context(|| format!("cannot open database `{}`", record.db_name))?;
        if tables.trim() == "0" {
            return Ok(false);
        }
        if !replace {
            return Ok(true);
        }
        let Some(root) = &root else {
            return Err(CompostError::ServerNotReady(format!(
                "database `{}` on {} is not empty, and compost has no root credentials to recreate it; empty it first",
                record.db_name, ctx.shared.name
            ))
            .into());
        };
        let drop = format!("DROP DATABASE IF EXISTS `{}`;\n", record.db_name);
        root.run_sql(&format!("{drop}{provisioning}"))
            .with_context(|| format!("recreating database `{}` failed", record.db_name))?;
        Ok(false)
    }
}

/// An external server has no compose project: only the record of how to
/// reach it, which also holds the root credentials, if any. A config without
/// `root_password` keeps the credentials of the record already on the server.
fn render_external(
    ctx: &ServerCtx,
    config: &SharedAppConfig,
    mut mysql: MySqlConfig,
    external: External,
    endpoint: &Endpoint,
) -> Result<Rendered> {
    if mysql.root_password.is_none() {
        let existing = ctx
            .read(&root_record_path(&config.name))?
            .and_then(|text| serde_yaml_ng::from_str::<ExternalRecord>(&text).ok());
        if let Some(existing) = existing.filter(|record| record.root_password.is_some()) {
            mysql.root_user = mysql.root_user.or(existing.root_user);
            mysql.root_password = existing.root_password;
        }
    }
    let record = ExternalRecord {
        type_name: config.type_name.clone(),
        instance: config.name.clone(),
        version: config.version,
        host: external.host,
        port: external.port.unwrap_or(endpoint.port),
        network: external.network,
        sslmode: external.sslmode,
        root_user: mysql
            .root_password
            .is_some()
            .then(|| mysql.root_user.unwrap_or_else(|| "root".to_string())),
        root_password: mysql.root_password,
    };
    Ok(Rendered {
        files: vec![RenderedFile::secret(
            root_record_path(&config.name),
            serde_yaml_ng::to_string(&record)?,
        )],
        ..Default::default()
    })
}

fn root_record_path(name: &str) -> String {
    format!("{}/root-connector.yaml", connectors_dir(KIND, name))
}

fn read_external_record(ctx: &ServerCtx, config: &SharedAppConfig) -> Result<ExternalRecord> {
    let path = root_record_path(&config.name);
    let text = ctx.read(&path)?.with_context(|| {
        format!(
            "{} has no record on the server; run `compost server deploy`",
            config.name
        )
    })?;
    serde_yaml_ng::from_str(&text).with_context(|| format!("invalid {}", ctx.path(&path)))
}

/// Where the client tools run for a database server.
struct Client<'a> {
    ctx: &'a ServerCtx<'a>,
    flavor: Flavor,
    place: Place,
}

enum Place {
    /// Inside compost's own container, as root; the password comes from the
    /// container's own environment, never a command line.
    Container { name: String },
    /// A throwaway client container that connects to an external server.
    /// Connection settings and the password are in an option file on the
    /// server (0600), mounted into it; it is deleted with the client.
    Throwaway {
        image: String,
        network: Option<String>,
        option_file: String,
        ssl: &'static str,
    },
}

impl<'a> Client<'a> {
    /// Root: compost's own server, or an external one with root credentials.
    /// `None` for an external one without.
    fn root(
        ctx: &'a ServerCtx<'a>,
        config: &SharedAppConfig,
        flavor: Flavor,
    ) -> Result<Option<Self>> {
        if !config.is_external() {
            return Ok(Some(Self::container(ctx, config, flavor)));
        }
        let record = read_external_record(ctx, config)?;
        match (&record.root_user, &record.root_password) {
            (Some(user), Some(password)) => Ok(Some(Self::external(
                ctx,
                config,
                flavor,
                Some((user.clone(), password.clone())),
            )?)),
            _ => Ok(None),
        }
    }

    /// As the link's user on an external server; root on compost's own.
    fn user(
        ctx: &'a ServerCtx<'a>,
        config: &SharedAppConfig,
        flavor: Flavor,
        record: &DatabaseRecord,
    ) -> Result<Self> {
        if !config.is_external() {
            return Ok(Self::container(ctx, config, flavor));
        }
        let login = (record.db_user.clone(), record.db_password.clone());
        Self::external(ctx, config, flavor, Some(login))
    }

    fn container(ctx: &'a ServerCtx<'a>, config: &SharedAppConfig, flavor: Flavor) -> Self {
        Self {
            ctx,
            flavor,
            place: Place::Container {
                name: config.name.clone(),
            },
        }
    }

    /// A client for an external server; without a login, only to check that
    /// it answers.
    fn external(
        ctx: &'a ServerCtx<'a>,
        config: &SharedAppConfig,
        flavor: Flavor,
        login: Option<(String, String)>,
    ) -> Result<Self> {
        let record = read_external_record(ctx, config)?;
        let version = config
            .version
            .context("version is required for databases")?;
        let mut options = format!("[client]\nhost={}\nport={}\n", record.host, record.port);
        if let Some((user, password)) = login {
            let _ = write!(options, "user={user}\npassword=\"{password}\"\n");
        }
        let option_file = ctx.path(&format!("tmp/my-{}.cnf", uuid::Uuid::new_v4().simple()));
        ctx.connector
            .exec(&format!("mkdir -p -- {}", shell_quote(&ctx.path("tmp"))))?;
        ctx.connector
            .write_file(&option_file, options.as_bytes(), 0o600)?;
        Ok(Self {
            ctx,
            flavor,
            place: Place::Throwaway {
                image: flavor.image(version),
                network: record.network,
                option_file,
                ssl: flavor.ssl_options(record.sslmode.as_deref()),
            },
        })
    }

    /// The command line for `program` (e.g. `mariadb`) with `args`.
    fn command(&self, program: &str, args: &str) -> String {
        match &self.place {
            Place::Container { name } => {
                let variable = self.flavor.password_variable();
                let inner = format!("MYSQL_PWD=\"${variable}\" exec {program} -uroot {args}");
                exec_command(self.ctx, name, &format!("sh -c {}", shell_quote(&inner)))
            }
            Place::Throwaway {
                image,
                network,
                option_file,
                ssl,
            } => {
                let network = network
                    .as_ref()
                    .map(|n| format!(" --network {}", shell_quote(n)))
                    .unwrap_or_default();
                format!(
                    "docker run --rm -i -v {}:/etc/compost.cnf:ro{network} {} {program} --defaults-extra-file=/etc/compost.cnf{ssl} {args}",
                    shell_quote(option_file),
                    shell_quote(image)
                )
            }
        }
    }

    /// Run SQL. SQL can hold passwords, so it goes through stdin, never on
    /// the command line.
    fn run_sql(&self, sql: &str) -> Result<()> {
        let script = self.command(self.flavor.client(), "");
        let output = self.ctx.connector.run(&script, Some(sql.as_bytes()))?;
        if !output.success() {
            bail!("{}", output.stderr.trim());
        }
        Ok(())
    }

    /// The result of a query, without column names.
    fn query(&self, sql: &str) -> Result<String> {
        let script = self.command(self.flavor.client(), "-N -B");
        let output = self.ctx.connector.run(&script, Some(sql.as_bytes()))?;
        if !output.success() {
            bail!("{}", output.stderr.trim());
        }
        Ok(output.stdout)
    }
}

impl Drop for Client<'_> {
    fn drop(&mut self) {
        if let Place::Throwaway { option_file, .. } = &self.place {
            let _ = self
                .ctx
                .connector
                .exec(&format!("rm -f -- {}", shell_quote(option_file)));
        }
    }
}

/// `dump` with its `DEFINER` clauses removed, failing when the dump fails
/// (POSIX sh has no `pipefail`: the dump's status goes through fd 3).
fn without_definers(dump: &str) -> String {
    format!(
        "exec 4>&1; status=$({{ {{ {dump}; echo $? >&3; }} | sed -e 's/ DEFINER=`[^`]*`@`[^`]*`//g' >&4; }} 3>&1); exit ${{status:-1}}"
    )
}

/// Run a command inside the database container of a shared app.
fn exec_command(ctx: &ServerCtx, name: &str, command: &str) -> String {
    format!(
        "cd {} && docker compose -p {} exec -T {} {command}",
        shell_quote(&ctx.path(&shared_dir(KIND, name))),
        shell_quote(&project_name(KIND, name)),
        shell_quote(&service_name(name))
    )
}

/// Names go into SQL and commands unquoted; they were checked on deploy, but
/// the record is a file on the server.
fn check_record(record: &DatabaseRecord) -> Result<()> {
    ensure!(
        is_identifier(&record.db_name, NAME_MAX) && is_identifier(&record.db_user, USER_MAX),
        "unexpected names in the link record"
    );
    Ok(())
}

/// `^[a-z_][a-z0-9_]*$` of at most `max` characters: safe unquoted, in
/// backticks and in single quotes.
fn is_identifier(value: &str, max: usize) -> bool {
    let mut chars = value.chars();
    value.len() <= max
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Idempotent SQL. Identifiers are validated by `is_identifier`; the password
/// is a string literal with `\` and `'` escaped.
fn provisioning_sql(database: &str, user: &str, password: &str) -> String {
    let password = password.replace('\\', "\\\\").replace('\'', "\\'");
    format!(
        "CREATE DATABASE IF NOT EXISTS `{database}` CHARACTER SET utf8mb4;\n\
         CREATE USER IF NOT EXISTS '{user}'@'%' IDENTIFIED BY '{password}';\n\
         ALTER USER '{user}'@'%' IDENTIFIED BY '{password}';\n\
         GRANT ALL PRIVILEGES ON `{database}`.* TO '{user}'@'%';\n"
    )
}

/// The value of a `NAME='…'` line written by `render`.
fn read_variable(env_file: &str, name: &str) -> Option<String> {
    env_file.lines().find_map(|line| {
        let value = line.strip_prefix(name)?.strip_prefix('=')?;
        Some(value.trim().trim_matches('\'').to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::Connector;
    use crate::connector::local::Local;
    use tempfile::TempDir;

    const LOCAL: Local = Local { verbose: false };

    fn config(text: &str) -> SharedAppConfig {
        serde_yaml_ng::from_str(text).unwrap()
    }

    fn render(kind: &MySql, home: &TempDir, text: &str) -> Result<Rendered> {
        let ctx = ServerCtx {
            connector: &LOCAL,
            home: home.path().to_str().unwrap().to_string(),
        };
        kind.render(&ctx, &config(text))
    }

    fn write(home: &TempDir, rendered: &Rendered) {
        for file in &rendered.files {
            let path = home.path().join(&file.path);
            LOCAL
                .write_file(path.to_str().unwrap(), file.contents.as_bytes(), file.mode)
                .unwrap();
        }
    }

    #[test]
    fn renders_each_flavor() {
        let home = TempDir::new().unwrap();
        let maria = render(
            &MARIADB,
            &home,
            "{name: mariadb11, type: mariadb, version: 11}",
        )
        .unwrap();
        let compose = &maria.files[0].contents;
        assert!(compose.contains("image: mariadb:11\n"), "{compose}");
        assert!(
            compose.contains("env_file: [./secrets/mariadb.env]"),
            "{compose}"
        );
        assert!(compose.contains("healthcheck.sh"), "{compose}");
        assert!(compose.contains("aliases: [mariadb11]"), "{compose}");
        assert!(
            maria.files[1]
                .contents
                .starts_with("MARIADB_ROOT_PASSWORD='")
        );
        assert!(maria.files[1].is_secret() && maria.files[2].is_secret());
        assert!(maria.files[2].contents.contains("port: 3306"));

        let mysql = render(&MYSQL, &home, "{name: mysql8, type: mysql, version: 8}").unwrap();
        assert!(mysql.files[0].contents.contains("image: mysql:8\n"));
        assert!(mysql.files[0].contents.contains("mysqladmin ping"));
        assert!(mysql.files[1].contents.starts_with("MYSQL_ROOT_PASSWORD='"));
    }

    #[test]
    fn password_is_kept_and_cannot_change() {
        let home = TempDir::new().unwrap();
        let text = "{name: m, type: mariadb, version: 11}";
        let first = render(&MARIADB, &home, text).unwrap();
        write(&home, &first);
        assert_eq!(
            render(&MARIADB, &home, text).unwrap().files[1].contents,
            first.files[1].contents
        );
        let err = render(
            &MARIADB,
            &home,
            "{name: m, type: mariadb, version: 11, configuration: {root_password: other}}",
        )
        .unwrap_err();
        assert!(err.to_string().contains("differs"), "{err}");
    }

    #[test]
    fn validation() {
        let err = |text: &str| {
            MARIADB
                .validate_config(&config(text))
                .unwrap_err()
                .to_string()
        };
        assert!(
            err("{name: m, type: mariadb, version: 11, configuration: {root_password: 'a\"b'}}")
                .contains("without quotes")
        );
        assert!(
            err("{name: m, type: mariadb, version: 11, configuration: {root_user: admin}}")
                .contains("only used with `external`")
        );
        MARIADB
            .validate_config(&config(
                "{name: m, type: mariadb, version: 11, external: {host: db.example.com}, configuration: {root_user: admin, root_password: x}}",
            ))
            .unwrap();
    }

    #[test]
    fn link_names_fit_the_limits() {
        let app: AppConfig = serde_yaml_ng::from_str(
            "{name: shop, prefix: production, app_services: [{name: backend, service: backend}]}",
        )
        .unwrap();
        let link = |text: &str| -> DatabaseLink { serde_yaml_ng::from_str(text).unwrap() };
        // The default user is cut to 32 characters with a hash.
        MYSQL
            .validate_link(
                &link("{name: customer-orders, type: mysql, targets: [backend]}"),
                &app,
            )
            .unwrap();
        let err = MYSQL
            .validate_link(
                &link("{name: d, type: mysql, targets: [backend], db_user: a_user_name_that_is_much_too_long}"),
                &app,
            )
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("db_user `a_user_name_that_is_much_too_long`"),
            "{err}"
        );
    }

    #[test]
    fn provisioning_sql_escapes_the_password() {
        let sql = provisioning_sql("shop", "shop_user", "it's\\x");
        assert!(
            sql.contains("CREATE DATABASE IF NOT EXISTS `shop` CHARACTER SET utf8mb4;"),
            "{sql}"
        );
        assert!(sql.contains("IDENTIFIED BY 'it\\'s\\\\x'"), "{sql}");
        assert!(
            sql.contains("GRANT ALL PRIVILEGES ON `shop`.* TO 'shop_user'@'%';"),
            "{sql}"
        );
    }

    #[test]
    fn root_password_stays_out_of_command_lines() {
        let home = TempDir::new().unwrap();
        let ctx = ServerCtx {
            connector: &LOCAL,
            home: home.path().to_str().unwrap().to_string(),
        };
        let client = Client::container(
            &ctx,
            &config("{name: m, type: mariadb, version: 11}"),
            Flavor::MariaDb,
        );
        let command = client.command("mariadb", "-N -B");
        assert!(
            command.ends_with("exec -T 'compost-shared-m' sh -c 'MYSQL_PWD=\"$MARIADB_ROOT_PASSWORD\" exec mariadb -uroot -N -B'"),
            "{command}"
        );
    }

    #[test]
    fn dump_status_survives_the_definer_filter() {
        let run = |dump: &str| LOCAL.run(&without_definers(dump), None).unwrap();
        let ok = run("echo 'CREATE DEFINER=`shop`@`%` VIEW v AS SELECT 1;'");
        assert_eq!(ok.status, 0);
        assert_eq!(ok.stdout, "CREATE VIEW v AS SELECT 1;\n");
        let failed = run("(echo partial; exit 3)");
        assert_eq!(failed.status, 3);
        assert_eq!(failed.stdout, "partial\n");
        // A dump that ends the shell without a status counts as failed.
        assert_eq!(run("exit 0").status, 1);
    }

    #[test]
    fn ssl_options_by_flavor() {
        assert_eq!(
            Flavor::MySql.ssl_options(Some("require")),
            " --ssl-mode=REQUIRED"
        );
        assert_eq!(
            Flavor::MariaDb.ssl_options(Some("verify-full")),
            " --ssl --ssl-verify-server-cert"
        );
        assert_eq!(
            Flavor::MariaDb.ssl_options(None),
            " --skip-ssl-verify-server-cert"
        );
        assert_eq!(Flavor::MySql.ssl_options(None), "");
    }
}
