//! Keycloak OIDC type. Keeps its data in a postgres database of the server and
//! is reached through the ingress.

use std::collections::BTreeMap;

use anyhow::{Result, bail, ensure};
use minijinja::context;
use serde::Deserialize;
use serde_yaml_ng::Value;

use super::base;
use super::{KIND, OidcType};
use crate::config::app::AppService;
use crate::shared::{
    Generated, Rendered, RenderedFile, ServerCtx, SharedAppConfig, SharedLinks, connectors_dir,
    generate_password, is_domain, render_template, shared_dir,
};

pub struct Keycloak;

const DEFAULT_VERSION: u32 = 26;
/// Image tags by major version: the registry has no tags like `26`.
const IMAGE_TAGS: &[(u32, &str)] = &[(26, "26.6")];
/// The database Keycloak needs (postgres features used since Keycloak 26).
const DATABASE_TYPE: &str = "postgres";
const DATABASE_VERSION: &str = ">=18";
const SERVICE: &str = "keycloak";
const HTTP_PORT: u16 = 8080;
const USER_VARIABLE: &str = "KC_BOOTSTRAP_ADMIN_USERNAME";
const PASSWORD_VARIABLE: &str = "KC_BOOTSTRAP_ADMIN_PASSWORD";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct KeycloakConfig {
    /// Public URL, e.g. `https://auth.example.com`. Required.
    url: Option<String>,
    admin_user: String,
    /// Secret; generated on first deploy when absent.
    admin_password: Option<String>,
    database: DatabaseChoice,
}

impl Default for KeycloakConfig {
    fn default() -> Self {
        Self {
            url: None,
            admin_user: "admin".to_string(),
            admin_password: None,
            database: DatabaseChoice::default(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct DatabaseChoice {
    /// Pin a database when several match.
    #[serde(default)]
    instance: Option<String>,
}

/// `http(s)://<domain>[:port]`, split into its parts.
struct PublicUrl {
    /// The URL without a trailing `/`.
    url: String,
    domain: String,
}

fn parse_url(url: &str) -> Result<PublicUrl> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"));
    let Some(rest) = rest else {
        bail!("url `{url}` must start with https:// or http://");
    };
    let authority = rest.strip_suffix('/').unwrap_or(rest);
    let (domain, port) = match authority.split_once(':') {
        Some((domain, port)) => (domain, Some(port)),
        None => (authority, None),
    };
    ensure!(
        is_domain(domain) && !domain.starts_with("*."),
        "url `{url}`: `{domain}` is not a valid domain name (a path is not supported)"
    );
    if let Some(port) = port {
        ensure!(
            port.parse::<u16>().is_ok_and(|p| p > 0),
            "url `{url}`: invalid port `{port}`"
        );
    }
    Ok(PublicUrl {
        url: url.trim_end_matches('/').to_string(),
        domain: domain.to_string(),
    })
}

fn public_url(config: &KeycloakConfig) -> Result<PublicUrl> {
    match &config.url {
        Some(url) => parse_url(url),
        None => bail!("configuration.url is required, e.g. https://auth.example.com"),
    }
}

impl OidcType for Keycloak {
    fn type_name(&self) -> &'static str {
        "keycloak"
    }

    fn default_version(&self) -> Option<u32> {
        Some(DEFAULT_VERSION)
    }

    fn secret_keys(&self) -> &'static [&'static str] {
        &["admin_password"]
    }

    fn generate(&self) -> Generated {
        Generated {
            local: include_str!("templates/generate/local.yaml"),
            prod: Some(include_str!("templates/generate/prod.yaml")),
            secrets: Some(include_str!("templates/generate/secrets.yaml")),
            notes: &[
                "keycloak: needs a postgres database of version 18 or newer and an ingress on the same server.",
                "keycloak: point the domain of `url` to the server; locally, open http://auth.localhost:18080/ and log in as `admin`.",
            ],
            requires: &["database/postgres", "ingress/caddy"],
        }
    }

    /// Extends the generic validation with Keycloak's own settings.
    fn validate_config(&self, config: &SharedAppConfig) -> Result<()> {
        base::validate_config(config)?;
        let keycloak: KeycloakConfig = config.configuration()?;
        public_url(&keycloak)?;
        image(config)?;
        ensure!(
            base::is_username(&keycloak.admin_user),
            "admin_user `{}` must match ^[a-z0-9][a-z0-9._@-]{{0,254}}$",
            keycloak.admin_user
        );
        if let Some(password) = &keycloak.admin_password {
            ensure!(!password.is_empty(), "admin_password must not be empty");
            // Written single-quoted into an env file and a YAML file.
            ensure!(
                !password.contains(['\n', '\r', '\'']),
                "admin_password must be a single line without `'`"
            );
        }
        Ok(())
    }

    fn render(&self, ctx: &ServerCtx, config: &SharedAppConfig) -> Result<Rendered> {
        let keycloak: KeycloakConfig = config.configuration()?;
        let url = public_url(&keycloak)?;
        let version = config.version.unwrap_or(DEFAULT_VERSION);
        let image = image(config)?;
        let dir = shared_dir(KIND, &config.name);
        let secrets_path = secrets_path(&config.name);

        // The bootstrap admin is only created on the first start, so the
        // password is generated once and then reused.
        let existing = ctx
            .read(&secrets_path)?
            .and_then(|text| read_variable(&text, PASSWORD_VARIABLE));
        let password = match (keycloak.admin_password, existing) {
            (Some(new), Some(old)) if new != old => bail!(
                "admin_password differs from the one already on the server; \
                 changing it is not supported yet (change it in Keycloak)"
            ),
            (Some(password), _) | (None, Some(password)) => password,
            (None, None) => generate_password(),
        };

        let network = base::network_name(&config.name);
        let compose = render_template(
            include_str!("templates/compose.yaml.j2"),
            context! {
                image,
                url => url.url.as_str(),
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
                url => url.url.as_str(),
                internal_url => internal_url(&config.name),
                admin_user => keycloak.admin_user.as_str(),
                password => password.as_str(),
            },
        )?;
        let env = format!(
            "{USER_VARIABLE}='{}'\n{PASSWORD_VARIABLE}='{password}'\n",
            keycloak.admin_user
        );

        Ok(Rendered {
            files: vec![
                RenderedFile::new(format!("{dir}/compose.yaml"), compose),
                RenderedFile::secret(secrets_path, env),
                RenderedFile::secret(
                    format!("{}/root-connector.yaml", connectors_dir(KIND, &config.name)),
                    root_connector,
                ),
            ],
            dirs: Vec::new(),
            networks: Vec::new(),
        })
    }

    /// A postgres database (>= 18) and the domain of `url` on the ingress.
    fn shared_links(&self, config: &SharedAppConfig) -> Result<Option<SharedLinks>> {
        let keycloak: KeycloakConfig = config.configuration()?;
        let url = public_url(&keycloak)?;

        let mut database = serde_yaml_ng::Mapping::new();
        let mut set = |key: &str, value: Value| {
            database.insert(key.into(), value);
        };
        set("name", "db".into());
        set("type", DATABASE_TYPE.into());
        set("version", DATABASE_VERSION.into());
        if let Some(instance) = &keycloak.database.instance {
            set("instance", instance.as_str().into());
        }
        set("targets", vec![Value::from(SERVICE)].into());
        let environment: serde_yaml_ng::Mapping = [
            ("KC_DB_URL_HOST", "db_host"),
            ("KC_DB_URL_PORT", "db_port"),
            ("KC_DB_URL_DATABASE", "db_name"),
            ("KC_DB_USERNAME", "db_user"),
            ("KC_DB_PASSWORD", "db_password"),
        ]
        .into_iter()
        .map(|(k, v)| (Value::from(k), Value::from(v)))
        .collect();
        set("environment", Value::Mapping(environment));

        let ingress: Value = serde_yaml_ng::from_str(&format!(
            "{{name: web, domains: ['{}'], routes: [{{name: keycloak, target: {SERVICE}, port: {HTTP_PORT}}}]}}",
            url.domain
        ))?;

        Ok(Some(SharedLinks {
            app_services: vec![AppService {
                name: SERVICE.to_string(),
                service: SERVICE.to_string(),
                environment: BTreeMap::new(),
            }],
            services: BTreeMap::from([
                ("database".to_string(), vec![Value::Mapping(database)]),
                ("ingress".to_string(), vec![ingress]),
            ]),
        }))
    }
}

/// The configured image, or the official one of the version.
fn image(config: &SharedAppConfig) -> Result<String> {
    if let Some(image) = &config.image {
        return Ok(image.clone());
    }
    let version = config.version.unwrap_or(DEFAULT_VERSION);
    match IMAGE_TAGS.iter().find(|(major, _)| *major == version) {
        Some((_, tag)) => Ok(format!("quay.io/keycloak/keycloak:{tag}")),
        None => bail!(
            "set `image` for keycloak version {version}, e.g. quay.io/keycloak/keycloak:{version}.0"
        ),
    }
}

fn internal_url(name: &str) -> String {
    format!("http://{name}:{HTTP_PORT}")
}

fn secrets_path(name: &str) -> String {
    format!("{}/secrets/keycloak.env", shared_dir(KIND, name))
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

    const KC: &str = "{name: keycloak, type: keycloak, version: 26, configuration: {url: 'https://auth.example.com'}}";

    fn config(text: &str) -> SharedAppConfig {
        serde_yaml_ng::from_str(text).unwrap()
    }

    fn validate(text: &str) -> Result<()> {
        Keycloak.validate_config(&config(text))
    }

    #[test]
    fn accepts_url_and_password() {
        validate(KC).unwrap();
        validate("{name: k, type: keycloak, configuration: {url: 'http://auth.localhost:18080/', admin_password: x}}")
            .unwrap();
    }

    #[test]
    fn checks_own_configuration() {
        for (text, expected) in [
            ("{name: k, type: keycloak}", "configuration.url is required"),
            (
                "{name: k, type: keycloak, configuration: {url: auth.example.com}}",
                "must start with",
            ),
            (
                "{name: k, type: keycloak, configuration: {url: 'https://a.example.com/auth'}}",
                "a path is not supported",
            ),
            (
                "{name: k, type: keycloak, configuration: {url: 'https://a.example.com:99999'}}",
                "invalid port",
            ),
            (
                "{name: k, type: keycloak, configuration: {url: 'https://a.com', admin_password: \"it's\"}}",
                "without `'`",
            ),
            (
                "{name: k, type: keycloak, configuration: {url: 'https://a.com', realm: x}}",
                "unknown field `realm`",
            ),
            (
                "{name: k, type: keycloak, version: 27, configuration: {url: 'https://a.com'}}",
                "set `image` for keycloak version 27",
            ),
        ] {
            let err = format!("{:#}", validate(text).unwrap_err());
            assert!(err.contains(expected), "{text}: {err}");
        }
    }

    #[test]
    fn links_to_postgres_18_and_the_ingress() {
        let links = Keycloak
            .shared_links(&config(
                "{name: keycloak, type: keycloak, configuration: {url: 'http://auth.localhost:18080', database: {instance: pg18}}}",
            ))
            .unwrap()
            .unwrap();
        let database = &links.services["database"][0];
        assert_eq!(database["type"], "postgres");
        assert_eq!(database["version"], ">=18");
        assert_eq!(database["instance"], "pg18");
        assert_eq!(database["environment"]["KC_DB_PASSWORD"], "db_password");
        let ingress = &links.services["ingress"][0];
        assert_eq!(ingress["domains"][0], "auth.localhost");
        assert_eq!(ingress["routes"][0]["port"], 8080);
        assert_eq!(links.app_services[0].service, "keycloak");
    }

    mod render {
        use super::*;
        use crate::connector::Connector;
        use crate::connector::local::Local;
        use tempfile::TempDir;

        const LOCAL: Local = Local { verbose: false };

        fn render(home: &TempDir, text: &str) -> Result<Rendered> {
            let ctx = ServerCtx {
                connector: &LOCAL,
                home: home.path().to_str().unwrap().to_string(),
            };
            Keycloak.render(&ctx, &config(text))
        }

        fn file<'a>(rendered: &'a Rendered, suffix: &str) -> &'a str {
            &rendered
                .files
                .iter()
                .find(|f| f.path.ends_with(suffix))
                .unwrap()
                .contents
        }

        #[test]
        fn renders_compose_and_secrets() {
            let home = TempDir::new().unwrap();
            let rendered = render(&home, KC).unwrap();
            let compose = file(&rendered, "compose.yaml");
            assert!(
                compose.contains("image: quay.io/keycloak/keycloak:26.6\n"),
                "{compose}"
            );
            assert!(
                compose.contains("KC_HOSTNAME: \"https://auth.example.com\""),
                "{compose}"
            );
            assert!(compose.contains("compost-oidc-keycloak:\n        aliases: [keycloak]"));
            // Valid YAML that compost can read like an app's compose file.
            let parsed = crate::config::app::ComposeFile::parse(compose).unwrap();
            assert!(parsed.services["keycloak"].has_networks);

            let paths: Vec<_> = rendered
                .files
                .iter()
                .map(|f| (f.path.as_str(), f.is_secret()))
                .collect();
            assert_eq!(
                paths,
                [
                    ("shared/oidc/keycloak/compose.yaml", false),
                    ("shared/oidc/keycloak/secrets/keycloak.env", true),
                    ("connectors/oidc/keycloak/root-connector.yaml", true),
                ]
            );
            assert!(
                file(&rendered, "keycloak.env")
                    .starts_with("KC_BOOTSTRAP_ADMIN_USERNAME='admin'\n")
            );
        }

        #[test]
        fn generated_password_is_reused_and_cannot_change() {
            let home = TempDir::new().unwrap();
            let first = render(&home, KC).unwrap();
            for f in &first.files {
                let path = home.path().join(&f.path);
                LOCAL
                    .write_file(path.to_str().unwrap(), f.contents.as_bytes(), f.mode)
                    .unwrap();
            }
            let second = render(&home, KC).unwrap();
            assert_eq!(file(&first, "keycloak.env"), file(&second, "keycloak.env"));
            let err = render(
                &home,
                "{name: keycloak, type: keycloak, configuration: {url: 'https://auth.example.com', admin_password: other}}",
            )
            .unwrap_err();
            assert!(err.to_string().contains("differs"), "{err}");
        }
    }
}
