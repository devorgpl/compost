//! Postgres database type (spec §5.3, §9.3).

use anyhow::{Result, bail, ensure};
use minijinja::context;
use serde::Deserialize;

use super::base::{self, DatabaseLink, DatabaseRecord, Endpoint};
use super::{DatabaseType, KIND};
use crate::config::app::AppConfig;
use crate::connector::shell_quote;
use crate::shared::{
    AppCtx, Rendered, RenderedFile, ServerCtx, SharedAppConfig, connectors_dir, generate_password,
    project_name, render_template, shared_dir,
};

pub struct Postgres;

const PASSWORD_VARIABLE: &str = "POSTGRES_PASSWORD";

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct PostgresConfig {
    /// Secret; generated on first deploy when absent.
    root_password: Option<String>,
}

impl DatabaseType for Postgres {
    fn type_name(&self) -> &'static str {
        "postgres"
    }

    fn endpoint(&self) -> Endpoint {
        Endpoint { url_scheme: "postgres", port: 5432 }
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

    /// Create the user and database if missing, and set the user's password.
    fn before_apply_application(&self, ctx: &AppCtx, link: &DatabaseLink) -> Result<()> {
        let credentials = base::credentials(ctx, link)?;
        let sql = provisioning_sql(&credentials.name, &credentials.user, &credentials.password);
        run_sql(ctx, &sql).map_err(|e| e.context(format!("creating database `{}` failed", credentials.name)))
    }

    /// `WITH (FORCE)` disconnects the app, which is still running (postgres 13+).
    fn drop_database(&self, ctx: &AppCtx, record: &DatabaseRecord) -> Result<()> {
        ensure!(
            is_identifier(&record.db_name) && is_identifier(&record.db_user),
            "unexpected names in the link record"
        );
        let sql = format!(
            "DROP DATABASE IF EXISTS \"{}\" WITH (FORCE);\nDROP ROLE IF EXISTS \"{}\";\n",
            record.db_name, record.db_user
        );
        run_sql(ctx, &sql).map_err(|e| e.context(format!("dropping database `{}` failed", record.db_name)))
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
        Ok(())
    }

    fn secret_keys(&self) -> &'static [&'static str] {
        &["root_password"]
    }

    fn render(&self, ctx: &ServerCtx, config: &SharedAppConfig) -> Result<Rendered> {
        let postgres: PostgresConfig = config.configuration()?;
        let Some(version) = config.version else {
            bail!("version is required for databases");
        };
        let image = config
            .image
            .clone()
            .unwrap_or_else(|| format!("postgres:{version}"));
        let dir = shared_dir(KIND, &config.name);
        let secrets_path = format!("{dir}/secrets/postgres.env");

        // The root password only takes effect when the data folder is first
        // created, so it is generated once and then reused.
        let existing = ctx.read(&secrets_path)?.and_then(|text| read_password(&text));
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
            context! { image, network => network.as_str(), host => config.name.as_str() },
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
            networks: Vec::new(),
        })
    }
}

/// Run SQL as the superuser inside the postgres container. SQL can hold
/// passwords, so it goes through stdin, never on the command line.
fn run_sql(ctx: &AppCtx, sql: &str) -> Result<()> {
    let dir = ctx.server.path(&shared_dir(KIND, &ctx.shared.name));
    let script = format!(
        "cd {} && docker compose -p {} exec -T postgres psql -U postgres -d postgres -q -v ON_ERROR_STOP=1",
        shell_quote(&dir),
        shell_quote(&project_name(KIND, &ctx.shared.name))
    );
    let output = ctx.server.connector.run(&script, Some(sql.as_bytes()))?;
    if !output.success() {
        bail!("{}", output.stderr.trim());
    }
    Ok(())
}

/// `^[a-z_][a-z0-9_]{0,62}$`: safe to use unquoted and inside double quotes.
fn is_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    value.len() <= 63
        && chars.next().is_some_and(|c| c.is_ascii_lowercase() || c == '_')
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
        let err = validate("{name: pg, type: postgres, version: 16, configuration: {root_password: ''}}")
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
            &rendered.files.iter().find(|f| f.path.ends_with("postgres.env")).unwrap().contents
        }

        const PG: &str = "{name: postgres16, type: postgres, version: 16}";

        #[test]
        fn renders_compose_and_secret_files() {
            let home = TempDir::new().unwrap();
            let rendered = render(&home, PG).unwrap();

            let compose = &rendered.files[0].contents;
            assert!(compose.contains("image: postgres:16"), "{compose}");
            assert!(compose.contains("depap-db-postgres16:\n        aliases: [postgres16]"));
            let paths: Vec<_> = rendered.files.iter().map(|f| (f.path.as_str(), f.is_secret())).collect();
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
                format!("{{name: postgres16, type: postgres, version: 16, configuration: {{root_password: {p}}}}}")
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
        assert!(sql.contains("ALTER ROLE \"appuser\" WITH LOGIN PASSWORD 'it''s';"), "{sql}");
        assert!(sql.contains("CREATE DATABASE \"appdb\" OWNER \"appuser\""), "{sql}");
    }

    #[test]
    fn link_names_must_be_identifiers() {
        let app: AppConfig = serde_yaml_ng::from_str(
            "{name: a, app_services: [{name: backend, service: backend}]}",
        )
        .unwrap();
        let link = |text: &str| -> DatabaseLink { serde_yaml_ng::from_str(text).unwrap() };
        Postgres
            .validate_link(&link("{name: data, type: postgres, targets: [backend]}"), &app)
            .unwrap();
        let err = Postgres
            .validate_link(&link("{name: data, type: postgres, targets: [backend], db_name: My-DB}"), &app)
            .unwrap_err();
        assert!(err.to_string().contains("db_name `My-DB`"), "{err}");
    }
}
