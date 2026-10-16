//! Postgres database type (spec §5.3, §9.3).

use anyhow::{Context, Result, bail, ensure};
use minijinja::context;
use serde::{Deserialize, Serialize};

use super::base::{self, DatabaseLink, DatabaseRecord, Endpoint, External};
use super::{DatabaseType, KIND};
use crate::config::app::AppConfig;
use crate::connector::shell_quote;
use crate::error::CompostError;
use crate::shared::{
    AppCtx, Generated, ItemReader, ItemWriter, Monitored, Rendered, RenderedFile, Role, ServerCtx,
    SharedAppConfig, app_service, connectors_dir, generate_password, project_name, render_template,
    service_name, shared_dir,
};

pub struct Postgres;

const PASSWORD_VARIABLE: &str = "POSTGRES_PASSWORD";

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct PostgresConfig {
    /// Secret; generated on first deploy when absent. For an external server:
    /// optional, and with it compost creates databases there.
    root_password: Option<String>,
    /// Superuser of an external server; default `postgres`.
    root_user: Option<String>,
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

impl DatabaseType for Postgres {
    fn type_name(&self) -> &'static str {
        "postgres"
    }

    fn monitored(&self, config: &SharedAppConfig) -> Result<Monitored> {
        Ok(Monitored {
            services: vec![app_service("postgres", service_name(&config.name))],
            metrics: Vec::new(),
        })
    }

    fn generate(&self) -> Generated {
        Generated {
            local: include_str!("templates/generate/local.yaml"),
            prod: None,
            secrets: Some(include_str!("templates/generate/secrets.yaml")),
            notes: &[
                "postgres: set `root_password` in .compost-server.secrets.yaml (copy the example). Optional: without it compost generates one on the first deploy; it cannot be changed later.",
            ],
            requires: &[],
        }
    }

    fn endpoint(&self) -> Endpoint {
        Endpoint {
            url_scheme: "postgres",
            port: 5432,
        }
    }

    /// Extends the generic link validation: names must be plain Postgres identifiers.
    fn validate_link(&self, link: &DatabaseLink, app: &AppConfig) -> Result<()> {
        base::validate_link(link, app)?;
        let (name, user) = base::names(app, link);
        for (key, value) in [("db_name", &name), ("db_user", &user)] {
            ensure!(
                is_identifier(value),
                "{key} `{value}` must match ^[a-z_][a-z0-9_]{{0,62}}$ (set {key} to a shorter name if the default is too long)"
            );
        }
        Ok(())
    }

    /// An external server: only with root credentials.
    fn can_provision(&self, ctx: &ServerCtx, config: &SharedAppConfig) -> Result<bool> {
        Ok(!config.is_external() || Client::root(ctx, config)?.is_some())
    }

    /// Create the user and database if missing, and set the user's password.
    /// On an external server without root credentials they exist already.
    fn before_apply_application(&self, ctx: &AppCtx, link: &DatabaseLink) -> Result<()> {
        let Some(root) = Client::root(ctx.server, ctx.shared)? else {
            return Ok(());
        };
        let credentials = base::credentials(ctx, link, true)?;
        let sql = provisioning_sql(&credentials.name, &credentials.user, &credentials.password);
        run_sql(&root, &sql)
            .map_err(|e| e.context(format!("creating database `{}` failed", credentials.name)))
    }

    /// `WITH (FORCE)` disconnects the app, which is still running (postgres 13+).
    /// On an external server only with root credentials and `allow_purge`;
    /// otherwise the database is kept, with a warning.
    fn drop_database(&self, ctx: &AppCtx, record: &DatabaseRecord) -> Result<()> {
        check_record(record)?;
        let allowed = base::external(ctx.shared)?.is_none_or(|external| external.allow_purge);
        let root = match Client::root(ctx.server, ctx.shared)? {
            Some(root) if allowed => root,
            _ => {
                eprintln!(
                    "warning: database `{}` on {} is kept: it is external (purging needs root credentials and `external.allow_purge: true`)",
                    record.db_name, ctx.shared.name
                );
                return Ok(());
            }
        };
        let sql = format!(
            "DROP DATABASE IF EXISTS \"{}\" WITH (FORCE);\nDROP ROLE IF EXISTS \"{}\";\n",
            record.db_name, record.db_user
        );
        run_sql(&root, &sql)
            .map_err(|e| e.context(format!("dropping database `{}` failed", record.db_name)))
    }

    /// `pg_isready` from a client container on the server's network.
    fn check_external(&self, ctx: &ServerCtx, config: &SharedAppConfig) -> Result<String> {
        let client = Client::external(ctx, config, None)?;
        let output = ctx.connector.run(&client.command("pg_isready", ""), None)?;
        if output.success() {
            Ok("accepting connections".to_string())
        } else {
            bail!(
                "{}",
                format!("{} {}", output.stdout.trim(), output.stderr.trim()).trim()
            )
        }
    }

    /// Plain SQL without owners and privileges, so it loads into a database
    /// with another name and owner (e.g. a copy under another prefix).
    fn dump_database(
        &self,
        ctx: &AppCtx,
        record: &DatabaseRecord,
        name: &str,
        out: &mut ItemWriter,
    ) -> Result<()> {
        check_record(record)?;
        let client = Client::user(ctx.server, ctx.shared, record)?;
        let dump = format!("--no-owner --no-acl -d {}", record.db_name);
        out.stream(name, Role::Data, &client.command("pg_dump", &dump))
    }

    /// The dump runs as the link user (`SET ROLE`), so the objects it creates
    /// belong to that user. The database and user are created first when
    /// missing, e.g. for a link that was deployed without `--apply` only.
    fn load_database(
        &self,
        ctx: &AppCtx,
        record: &DatabaseRecord,
        name: &str,
        from: &ItemReader,
        replace: bool,
    ) -> Result<()> {
        check_record(record)?;
        let root = Client::root(ctx.server, ctx.shared)?;
        let provisioning = provisioning_sql(&record.db_name, &record.db_user, &record.db_password);
        if let Some(root) = &root {
            run_sql(root, &provisioning)
                .with_context(|| format!("creating database `{}` failed", record.db_name))?;
        }
        let client = Client::user(ctx.server, ctx.shared, record)?;
        let count = format!("-d {} -tAc {}", record.db_name, shell_quote(OBJECTS_SQL));
        let objects = ctx
            .server
            .connector
            .exec(&client.command("psql", &count))
            .with_context(|| format!("cannot open database `{}`", record.db_name))?;
        if objects.trim() != "0" {
            if !replace {
                return Err(CompostError::ServerNotReady(format!(
                    "database `{}` is not empty; add --replace to drop and recreate it",
                    record.db_name
                ))
                .into());
            }
            let Some(root) = &root else {
                return Err(CompostError::ServerNotReady(format!(
                    "database `{}` on {} is not empty, and compost has no root credentials to recreate it; empty it first",
                    record.db_name, ctx.shared.name
                ))
                .into());
            };
            let drop = format!(
                "DROP DATABASE IF EXISTS \"{}\" WITH (FORCE);\n",
                record.db_name
            );
            run_sql(root, &format!("{drop}{provisioning}"))
                .with_context(|| format!("recreating database `{}` failed", record.db_name))?;
        }
        out!("  loading database {} ...", record.db_name);
        let load = format!("-d {} -q -v ON_ERROR_STOP=1", record.db_name);
        let prelude = format!("SET ROLE \"{}\";\n", record.db_user);
        from.feed(name, prelude.as_bytes(), &client.command("psql", &load))
            .with_context(|| format!("loading database `{}` failed", record.db_name))
    }

    /// `dumpall.sql` with every database and role, and the root password, which
    /// must match the one in the dump.
    fn backup_shared(
        &self,
        ctx: &ServerCtx,
        config: &SharedAppConfig,
        out: &mut ItemWriter,
    ) -> Result<()> {
        out.stream(
            DUMPALL_ITEM,
            Role::Data,
            &exec_command(ctx, &config.name, "pg_dumpall -U postgres"),
        )?;
        let secrets = ctx
            .read(&secrets_path(&config.name))?
            .context("the root password file is missing; deploy the server again")?;
        out.file(SECRETS_ITEM, Role::Meta, secrets.as_bytes())
    }

    /// Put the root password back before the first start, so the data folder
    /// is created with it.
    fn restore_shared_files(
        &self,
        ctx: &ServerCtx,
        config: &SharedAppConfig,
        from: &ItemReader,
    ) -> Result<()> {
        if from.has(SECRETS_ITEM) {
            let secrets = from.read(SECRETS_ITEM)?;
            ctx.connector.write_file(
                &ctx.path(&secrets_path(&config.name)),
                secrets.as_bytes(),
                0o600,
            )?;
        }
        Ok(())
    }

    /// Without ON_ERROR_STOP: a dump of all databases recreates the `postgres`
    /// role, which always exists. Other errors fail the restore.
    fn restore_shared_data(
        &self,
        ctx: &ServerCtx,
        config: &SharedAppConfig,
        from: &ItemReader,
    ) -> Result<()> {
        if !from.has(DUMPALL_ITEM) {
            return Ok(());
        }
        // While the image initializes a new data folder, a temporary server
        // answers on the socket only; wait for the real one, on TCP.
        let wait = "sh -c 'for i in $(seq 1 60); do pg_isready -q -h 127.0.0.1 -U postgres && exit 0; sleep 1; done; exit 1'";
        ctx.connector
            .exec(&exec_command(ctx, &config.name, wait))
            .context("postgres did not accept connections")?;
        let output = from.feed_output(
            DUMPALL_ITEM,
            &exec_command(ctx, &config.name, "psql -U postgres -d postgres -q"),
        )?;
        let errors = restore_errors(&output.stderr);
        if !output.success() || !errors.is_empty() {
            bail!(
                "loading the databases of {} failed:\n{}",
                config.name,
                if errors.is_empty() {
                    output.stderr.trim().to_string()
                } else {
                    errors.join("\n")
                }
            );
        }
        Ok(())
    }

    /// Extends the generic database validation with Postgres' own settings.
    fn validate_config(&self, config: &SharedAppConfig) -> Result<()> {
        base::validate_config(config)?;

        let postgres: PostgresConfig = config.configuration()?;
        if let Some(password) = &postgres.root_password {
            ensure!(!password.is_empty(), "root_password must not be empty");
            // The password is written single-quoted into an env file and a YAML file.
            ensure!(
                !password.contains(['\n', '\r', '\'']),
                "root_password must be a single line without `'`"
            );
        }
        if let Some(user) = &postgres.root_user {
            ensure!(
                config.is_external(),
                "root_user is only used with `external`; compost's own postgres uses `postgres`"
            );
            ensure!(
                !user.is_empty() && !user.contains(char::is_whitespace),
                "root_user must be a user name"
            );
        }
        Ok(())
    }

    fn secret_keys(&self) -> &'static [&'static str] {
        &["root_password"]
    }

    fn render(&self, ctx: &ServerCtx, config: &SharedAppConfig) -> Result<Rendered> {
        let postgres: PostgresConfig = config.configuration()?;
        if let Some(external) = base::external(config)? {
            return render_external(config, postgres, external, &self.endpoint());
        }
        let Some(version) = config.version else {
            bail!("version is required for databases");
        };
        let image = config
            .image
            .clone()
            .unwrap_or_else(|| format!("postgres:{version}"));
        let dir = shared_dir(KIND, &config.name);
        let secrets_path = secrets_path(&config.name);

        // The root password only takes effect when the data folder is first
        // created, so it is generated once and then reused.
        let existing = ctx
            .read(&secrets_path)?
            .and_then(|text| read_password(&text));
        let password = match (postgres.root_password, existing) {
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
            },
        )?;
        let root_connector = render_template(
            include_str!("templates/root-connector.yaml.j2"),
            context! {
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
                RenderedFile::secret(secrets_path, format!("{PASSWORD_VARIABLE}='{password}'\n")),
                RenderedFile::secret(
                    format!("{}/root-connector.yaml", connectors_dir(KIND, &config.name)),
                    root_connector,
                ),
            ],
            dirs: vec![format!("{dir}/data")],
            ..Default::default()
        })
    }
}

const DUMPALL_ITEM: &str = "dumpall.sql";
const SECRETS_ITEM: &str = "postgres.env";

/// Counts tables, sequences, views… outside the system schemas.
const OBJECTS_SQL: &str = "SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
     WHERE n.nspname NOT IN ('pg_catalog', 'information_schema') AND n.nspname NOT LIKE 'pg_toast%'";

fn secrets_path(name: &str) -> String {
    format!("{}/secrets/postgres.env", shared_dir(KIND, name))
}

/// An external server has no compose project: only the record of how to
/// reach it, which also holds the root credentials, if any.
fn render_external(
    config: &SharedAppConfig,
    postgres: PostgresConfig,
    external: External,
    endpoint: &Endpoint,
) -> Result<Rendered> {
    let record = ExternalRecord {
        type_name: config.type_name.clone(),
        instance: config.name.clone(),
        version: config.version,
        host: external.host,
        port: external.port.unwrap_or(endpoint.port),
        network: external.network,
        sslmode: external.sslmode,
        root_user: postgres
            .root_password
            .is_some()
            .then(|| postgres.root_user.unwrap_or_else(|| "postgres".to_string())),
        root_password: postgres.root_password,
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

/// Where `psql`, `pg_dump` and `pg_isready` run for a database server.
struct Client<'a> {
    ctx: &'a ServerCtx<'a>,
    place: Place,
}

enum Place {
    /// Inside compost's own postgres container, as the superuser.
    Container { name: String },
    /// A throwaway `postgres` container that connects to an external server.
    /// Connection settings and the password are in an env file on the server
    /// (0600), never on a command line; it is deleted with the client.
    Throwaway {
        image: String,
        network: Option<String>,
        env_file: String,
    },
}

impl<'a> Client<'a> {
    /// The superuser: compost's own server, or an external one with root
    /// credentials. `None` for an external one without.
    fn root(ctx: &'a ServerCtx<'a>, config: &SharedAppConfig) -> Result<Option<Self>> {
        if !config.is_external() {
            return Ok(Some(Self::container(ctx, config)));
        }
        let record = read_external_record(ctx, config)?;
        match (&record.root_user, &record.root_password) {
            (Some(user), Some(password)) => {
                let login = Login {
                    user: user.clone(),
                    password: password.clone(),
                    database: "postgres".to_string(),
                };
                Ok(Some(Self::external(ctx, config, Some(login))?))
            }
            _ => Ok(None),
        }
    }

    /// As the link's user on an external server; the superuser on compost's own.
    fn user(
        ctx: &'a ServerCtx<'a>,
        config: &SharedAppConfig,
        record: &DatabaseRecord,
    ) -> Result<Self> {
        if !config.is_external() {
            return Ok(Self::container(ctx, config));
        }
        let login = Login {
            user: record.db_user.clone(),
            password: record.db_password.clone(),
            database: record.db_name.clone(),
        };
        Self::external(ctx, config, Some(login))
    }

    fn container(ctx: &'a ServerCtx<'a>, config: &SharedAppConfig) -> Self {
        Self {
            ctx,
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
        login: Option<Login>,
    ) -> Result<Self> {
        let record = read_external_record(ctx, config)?;
        let version = config
            .version
            .context("version is required for databases")?;
        let mut env = format!("PGHOST={}\nPGPORT={}\n", record.host, record.port);
        if let Some(mode) = &record.sslmode {
            env.push_str(&format!("PGSSLMODE={mode}\n"));
        }
        if let Some(login) = login {
            env.push_str(&format!(
                "PGUSER={}\nPGPASSWORD={}\nPGDATABASE={}\n",
                login.user, login.password, login.database
            ));
        }
        let env_file = ctx.path(&format!("tmp/pg-{}.env", uuid::Uuid::new_v4().simple()));
        ctx.connector.write_file(&env_file, env.as_bytes(), 0o600)?;
        Ok(Self {
            ctx,
            place: Place::Throwaway {
                image: format!("postgres:{version}-alpine"),
                network: record.network,
                env_file,
            },
        })
    }

    /// The command line for `program` (e.g. `psql`) with `args`.
    fn command(&self, program: &str, args: &str) -> String {
        match &self.place {
            Place::Container { name } => {
                exec_command(self.ctx, name, &format!("{program} -U postgres {args}"))
            }
            Place::Throwaway {
                image,
                network,
                env_file,
            } => {
                let network = network
                    .as_ref()
                    .map(|n| format!(" --network {}", shell_quote(n)))
                    .unwrap_or_default();
                format!(
                    "docker run --rm -i --env-file {}{network} {} {program} {args}",
                    shell_quote(env_file),
                    shell_quote(image)
                )
            }
        }
    }
}

impl Drop for Client<'_> {
    fn drop(&mut self) {
        if let Place::Throwaway { env_file, .. } = &self.place {
            let _ = self
                .ctx
                .connector
                .exec(&format!("rm -f -- {}", shell_quote(env_file)));
        }
    }
}

struct Login {
    user: String,
    password: String,
    database: String,
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

/// Run a command inside the postgres container of a shared app.
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
        is_identifier(&record.db_name) && is_identifier(&record.db_user),
        "unexpected names in the link record"
    );
    Ok(())
}

/// `ERROR:` lines of psql, except the expected one about the `postgres` role.
fn restore_errors(stderr: &str) -> Vec<&str> {
    stderr
        .lines()
        .filter(|line| line.contains("ERROR:"))
        .filter(|line| !line.contains("role \"postgres\" already exists"))
        .collect()
}

/// Run SQL as the superuser. SQL can hold passwords, so it goes through
/// stdin, never on the command line.
fn run_sql(root: &Client, sql: &str) -> Result<()> {
    let script = root.command("psql", "-d postgres -q -v ON_ERROR_STOP=1");
    let output = root.ctx.connector.run(&script, Some(sql.as_bytes()))?;
    if !output.success() {
        bail!("{}", output.stderr.trim());
    }
    Ok(())
}

/// `^[a-z_][a-z0-9_]{0,62}$`: safe to use unquoted and inside double quotes.
fn is_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    value.len() <= 63
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Idempotent SQL for psql. Identifiers are validated by `is_identifier`;
/// the password is a string literal with `'` doubled.
fn provisioning_sql(database: &str, user: &str, password: &str) -> String {
    let password = password.replace('\'', "''");
    format!(
        "SELECT 'CREATE ROLE \"{user}\" LOGIN' WHERE NOT EXISTS (SELECT FROM pg_roles WHERE rolname = '{user}')\\gexec\n\
         ALTER ROLE \"{user}\" WITH LOGIN PASSWORD '{password}';\n\
         SELECT 'CREATE DATABASE \"{database}\" OWNER \"{user}\"' WHERE NOT EXISTS (SELECT FROM pg_database WHERE datname = '{database}')\\gexec\n\
         GRANT ALL PRIVILEGES ON DATABASE \"{database}\" TO \"{user}\";\n"
    )
}

/// The password from a `POSTGRES_PASSWORD='…'` line written by `render`.
fn read_password(env_file: &str) -> Option<String> {
    env_file.lines().find_map(|line| {
        let value = line.strip_prefix(PASSWORD_VARIABLE)?.strip_prefix('=')?;
        Some(value.trim().trim_matches('\'').to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn validate(text: &str) -> Result<()> {
        let config: SharedAppConfig = serde_yaml_ng::from_str(text).unwrap();
        Postgres.validate_config(&config)
    }

    #[test]
    fn accepts_version_and_password() {
        validate("{name: pg, type: postgres, version: 16, configuration: {root_password: x}}")
            .unwrap();
    }

    #[test]
    fn runs_generic_database_validation_first() {
        let err = validate("{name: pg, type: postgres}").unwrap_err();
        assert_eq!(err.to_string(), "version is required for databases");
    }

    #[test]
    fn checks_own_configuration() {
        let err =
            validate("{name: pg, type: postgres, version: 16, configuration: {root_password: ''}}")
                .unwrap_err();
        assert_eq!(err.to_string(), "root_password must not be empty");
    }

    mod render {
        use super::*;
        use crate::connector::Connector;
        use crate::connector::local::Local;
        use tempfile::TempDir;

        const LOCAL: Local = Local { verbose: false };

        fn render(home: &TempDir, text: &str) -> Result<Rendered> {
            let config: SharedAppConfig = serde_yaml_ng::from_str(text).unwrap();
            let ctx = ServerCtx {
                connector: &LOCAL,
                home: home.path().to_str().unwrap().to_string(),
            };
            Postgres.render(&ctx, &config)
        }

        /// Write rendered files like a deploy would.
        fn write(home: &TempDir, rendered: &Rendered) {
            for file in &rendered.files {
                let path = home.path().join(&file.path);
                LOCAL
                    .write_file(path.to_str().unwrap(), file.contents.as_bytes(), file.mode)
                    .unwrap();
            }
        }

        fn secret(rendered: &Rendered) -> &str {
            &rendered
                .files
                .iter()
                .find(|f| f.path.ends_with("postgres.env"))
                .unwrap()
                .contents
        }

        const PG: &str = "{name: postgres16, type: postgres, version: 16}";

        #[test]
        fn renders_compose_and_secret_files() {
            let home = TempDir::new().unwrap();
            let rendered = render(&home, PG).unwrap();

            let compose = &rendered.files[0].contents;
            assert!(compose.contains("image: postgres:16"), "{compose}");
            // Not `postgres`: compose adds the service name as an alias on its networks.
            assert!(
                compose.contains("\n  compost-shared-postgres16:\n"),
                "{compose}"
            );
            assert!(
                compose.contains("container_name: compost-database-postgres16\n"),
                "{compose}"
            );
            assert!(compose.contains("compost-db-postgres16:\n        aliases: [postgres16]"));
            let paths: Vec<_> = rendered
                .files
                .iter()
                .map(|f| (f.path.as_str(), f.is_secret()))
                .collect();
            assert_eq!(
                paths,
                [
                    ("shared/database/postgres16/compose.yaml", false),
                    ("shared/database/postgres16/secrets/postgres.env", true),
                    ("connectors/database/postgres16/root-connector.yaml", true),
                ]
            );
        }

        #[test]
        fn generated_password_is_reused() {
            let home = TempDir::new().unwrap();
            let first = render(&home, PG).unwrap();
            write(&home, &first);
            let second = render(&home, PG).unwrap();
            assert_eq!(secret(&first), secret(&second));
            assert_eq!(secret(&first).len(), "POSTGRES_PASSWORD=''\n".len() + 32);
        }

        #[test]
        fn configured_password_must_not_change() {
            let home = TempDir::new().unwrap();
            let with_password = |p: &str| {
                format!(
                    "{{name: postgres16, type: postgres, version: 16, configuration: {{root_password: {p}}}}}"
                )
            };
            write(&home, &render(&home, &with_password("first")).unwrap());
            render(&home, &with_password("first")).unwrap();
            let err = render(&home, &with_password("second")).unwrap_err();
            assert!(err.to_string().contains("differs"), "{err}");
        }
    }

    #[test]
    fn provisioning_sql_escapes_password() {
        let sql = provisioning_sql("appdb", "appuser", "it's");
        assert!(sql.contains("WHERE rolname = 'appuser')\\gexec\n"), "{sql}");
        assert!(
            sql.contains("ALTER ROLE \"appuser\" WITH LOGIN PASSWORD 'it''s';"),
            "{sql}"
        );
        assert!(
            sql.contains("CREATE DATABASE \"appdb\" OWNER \"appuser\""),
            "{sql}"
        );
    }

    #[test]
    fn only_the_postgres_role_error_is_expected() {
        let stderr = "psql:<stdin>:14: ERROR:  role \"postgres\" already exists\n\
                      psql:<stdin>:20: ERROR:  syntax error at or near \"x\"\nNOTICE: y\n";
        assert_eq!(
            restore_errors(stderr),
            ["psql:<stdin>:20: ERROR:  syntax error at or near \"x\""]
        );
    }

    #[test]
    fn link_names_must_be_identifiers() {
        let app: AppConfig =
            serde_yaml_ng::from_str("{name: a, app_services: [{name: backend, service: backend}]}")
                .unwrap();
        let link = |text: &str| -> DatabaseLink { serde_yaml_ng::from_str(text).unwrap() };
        Postgres
            .validate_link(
                &link("{name: data, type: postgres, targets: [backend]}"),
                &app,
            )
            .unwrap();
        let err = Postgres
            .validate_link(
                &link("{name: data, type: postgres, targets: [backend], db_name: My-DB}"),
                &app,
            )
            .unwrap_err();
        assert!(err.to_string().contains("db_name `My-DB`"), "{err}");
    }
}
