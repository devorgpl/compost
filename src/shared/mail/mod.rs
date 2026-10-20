//! Mail kind: a mail catcher for development and test servers. Apps send
//! mail through it instead of a real provider, and people read it in a web
//! inbox. Type `mailpit`: Mailpit (SMTP and the inbox), a mock of the AWS SES
//! API that hands every mail to Mailpit, and optionally moto for other AWS APIs.

use std::collections::{BTreeMap, HashSet};

use anyhow::{Context, Result, bail, ensure};
use minijinja::context;
use serde::{Deserialize, Serialize};
use serde_yaml_ng::Value;

use super::{
    AppCtx, Contribution, Generated, Kind, PublicUrl, Rendered, RenderedFile, Resolution,
    ServerCtx, SharedAppConfig, SharedLinks, app_service, check_link_environment, connectors_dir,
    generate_password, host_address, link_value, parse_link, parse_url, project_name,
    render_template, service_name, shared_dir,
};
use crate::config::app::AppConfig;
use crate::connector::shell_quote;

pub const KIND: &str = "mail";

const TYPE: &str = "mailpit";
const DEFAULT_VERSION: u32 = 1;
/// Image tags by major version of Mailpit.
const IMAGE_TAGS: &[(u32, &str)] = &[(1, "v1.31.3")];
const SES_IMAGE: &str = "ghcr.io/domdomegg/aws-ses-v2-local:2.10.2";
const MOTO_IMAGE: &str = "motoserver/moto:5.2.3";
const SMTP_PORT: u16 = 1025;
const UI_PORT: u16 = 8025;
const SES_PORT: u16 = 8005;
const MOTO_PORT: u16 = 5000;
/// Login of the inbox.
const USER: &str = "admin";
/// Mocks accept any credentials; SDKs want some.
const ACCESS_KEY: &str = "test";

/// Values a link can map to app variables.
const LINK_VALUES: [&str; 9] = [
    "smtp_host",
    "smtp_port",
    "smtp_url",
    "ses_endpoint",
    "aws_endpoint",
    "aws_region",
    "aws_access_key_id",
    "aws_secret_access_key",
    "inbox_url",
];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct MailConfig {
    /// Public URL of the inbox, e.g. `https://mail.test.example.com`; served by the ingress.
    url: Option<String>,
    /// Secret: the inbox's password for `admin`; generated on the first deploy when absent.
    password: Option<String>,
    /// Region in the variables of links.
    region: String,
    /// Also run moto, a mock of other AWS APIs (Cognito, SNS, …).
    moto: bool,
    /// For apps outside Docker (`dev up --host`): the SES API on this port of
    /// 127.0.0.1, SMTP on the next one, moto on the one after.
    host_port: Option<u16>,
    ses_image: Option<String>,
    moto_image: Option<String>,
}

impl Default for MailConfig {
    fn default() -> Self {
        Self {
            url: None,
            password: None,
            region: "us-east-1".to_string(),
            moto: false,
            host_port: None,
            ses_image: None,
            moto_image: None,
        }
    }
}

impl MailConfig {
    fn public_url(&self) -> Result<Option<PublicUrl>> {
        self.url.as_deref().map(parse_url).transpose()
    }
}

/// An entry under `services.mail` in compost.yaml.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MailLink {
    name: String,
    /// Use this shared app; needed when the server has several.
    #[serde(default)]
    instance: Option<String>,
    /// Skip the link when the server runs no mail app, e.g. on production,
    /// where the app sends through the real provider.
    #[serde(default)]
    optional: bool,
    /// `app_services` that receive the variables and join the mail network.
    #[serde(default)]
    targets: Vec<String>,
    /// App variable name → link value (one of [`LINK_VALUES`]).
    #[serde(default)]
    environment: BTreeMap<String, String>,
}

/// `connectors/mail/<name>/<instance>-<entry>.yaml`.
#[derive(Debug, Serialize, Deserialize)]
struct MailRecord {
    app: String,
    prefix: String,
    entry: String,
    targets: Vec<String>,
}

pub struct MailKind;

impl Kind for MailKind {
    fn name(&self) -> &'static str {
        KIND
    }

    fn type_names(&self) -> Vec<&'static str> {
        vec![TYPE]
    }

    fn generate(&self, type_name: &str) -> Result<Generated> {
        check_type(type_name)?;
        Ok(Generated {
            local: include_str!("templates/generate/local.yaml"),
            prod: Some(include_str!("templates/generate/prod.yaml")),
            secrets: Some(include_str!("templates/generate/secrets.yaml")),
            notes: &[
                "mailpit: for development and test servers; apps link to it with `services.mail` and send mail by SMTP or the AWS SES API.",
                "mailpit: locally, open http://mail.localhost:18080/ and log in as `admin` with the password from connectors/mail/mail/root-connector.yaml on the server.",
            ],
            requires: &["ingress/caddy"],
        })
    }

    fn apply_defaults(&self, config: &mut SharedAppConfig) {
        if config.version.is_none() && config.type_name == TYPE {
            config.version = Some(DEFAULT_VERSION);
        }
    }

    fn validate_shared(&self, config: &SharedAppConfig) -> Result<()> {
        check_type(&config.type_name)?;
        if config.monitoring.is_some() {
            bail!("`monitoring` is not supported for mail shared apps");
        }
        if let Some(image) = &config.image {
            ensure!(!image.trim().is_empty(), "image must not be empty");
        }
        image(config)?;
        let mail: MailConfig = config.configuration()?;
        mail.public_url().context("url")?;
        if let Some(password) = &mail.password {
            ensure!(
                password.len() >= 8 && !password.contains([':', '\'', '\n']),
                "password: at least 8 characters, without `:`, quotes or line breaks"
            );
        }
        ensure!(
            !mail.region.is_empty()
                && mail
                    .region
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "region `{}` is not an AWS region like eu-west-1",
            mail.region
        );
        if let Some(port) = mail.host_port {
            ensure!(
                port > 0 && port <= u16::MAX - 2,
                "host_port {port}: it and the next two ports must be valid"
            );
        }
        for (key, image) in [
            ("ses_image", &mail.ses_image),
            ("moto_image", &mail.moto_image),
        ] {
            if let Some(image) = image {
                ensure!(!image.trim().is_empty(), "{key} must not be empty");
            }
        }
        Ok(())
    }

    fn secret_keys(&self, _config: &SharedAppConfig) -> Result<&'static [&'static str]> {
        Ok(&["password"])
    }

    fn render(&self, ctx: &ServerCtx, config: &SharedAppConfig) -> Result<Rendered> {
        let mail: MailConfig = config.configuration()?;
        let dir = shared_dir(KIND, &config.name);
        let secrets = secrets_path(&config.name);
        // The generated password stays the same on later deploys.
        let password = match mail.password.clone() {
            Some(password) => password,
            None => ctx
                .read(&secrets)?
                .as_deref()
                .and_then(read_password)
                .unwrap_or_else(generate_password),
        };
        let network = network_name(&config.name);
        let service = service_name(&config.name);
        let compose = render_template(
            include_str!("templates/compose.yaml.j2"),
            context! {
                image => image(config)?,
                service => service.as_str(),
                container => project_name(KIND, &config.name),
                ses_image => mail.ses_image.as_deref().unwrap_or(SES_IMAGE),
                moto_image => mail.moto.then(|| mail.moto_image.as_deref().unwrap_or(MOTO_IMAGE)),
                // Mailpit by its compose service name on the project's network.
                smtp_transport => format!(
                    r#"{{"host":"{service}","port":{SMTP_PORT},"secure":false,"ignoreTLS":true}}"#
                ),
                network => network.as_str(),
                host => config.name.as_str(),
                host_port => mail.host_port,
                smtp_port => SMTP_PORT,
                ses_port => SES_PORT,
                moto_port => MOTO_PORT,
            },
        )?;
        let root_connector = render_template(
            include_str!("templates/root-connector.yaml.j2"),
            context! {
                name => config.name.as_str(),
                version => config.version.unwrap_or(DEFAULT_VERSION),
                network => network.as_str(),
                url => mail.public_url()?.map_or("none".to_string(), |u| u.url),
                user => USER,
                password => password.as_str(),
            },
        )?;
        Ok(Rendered {
            files: vec![
                RenderedFile::new(format!("{dir}/compose.yaml"), compose),
                RenderedFile::secret(secrets, format!("MP_UI_AUTH='{USER}:{password}'\n")),
                RenderedFile::secret(
                    format!("{}/root-connector.yaml", connectors_dir(KIND, &config.name)),
                    root_connector,
                ),
            ],
            networks: vec![network],
            ..Default::default()
        })
    }

    /// The domain of `url` on the ingress, for the inbox.
    fn shared_links(&self, config: &SharedAppConfig) -> Result<Option<SharedLinks>> {
        let mail: MailConfig = config.configuration()?;
        let Some(url) = mail.public_url()? else {
            return Ok(None);
        };
        let ingress: Value = serde_yaml_ng::from_str(&format!(
            "{{name: inbox, domains: ['{}'], routes: [{{name: mailpit, target: mailpit, port: {UI_PORT}}}]}}",
            url.domain
        ))?;
        Ok(Some(SharedLinks {
            app_services: vec![app_service("mailpit", service_name(&config.name))],
            services: BTreeMap::from([("ingress".to_string(), vec![ingress])]),
        }))
    }

    fn apply_order(&self) -> i32 {
        30 // nothing waits for it
    }

    fn validate_link(&self, link: &Value, app: &AppConfig) -> Result<()> {
        let link: MailLink = parse_link(link)?;
        ensure!(
            !link.targets.is_empty(),
            "targets: at least one app service is required"
        );
        let mut seen = HashSet::new();
        for target in &link.targets {
            ensure!(
                app.app_service(target).is_some(),
                "target `{target}` is not in app_services"
            );
            ensure!(seen.insert(target), "target `{target}` is listed twice");
        }
        check_link_environment(&link.environment, &LINK_VALUES)
    }

    fn resolve<'s>(
        &self,
        link: &Value,
        available: &'s [SharedAppConfig],
    ) -> Result<Resolution<'s>> {
        let link: MailLink = parse_link(link)?;
        let matching: Vec<&SharedAppConfig> = available
            .iter()
            .filter(|app| link.instance.as_ref().is_none_or(|pin| *pin == app.name))
            .collect();
        match matching.as_slice() {
            [found] => Ok(Resolution::Use(found)),
            [] if link.optional => Ok(Resolution::Skip(
                "optional and no mail app is running".to_string(),
            )),
            [] => match &link.instance {
                Some(pin) => bail!("no running mail app named `{pin}` on the server"),
                None => bail!(
                    "no mail app is running on the server; define one in compost-server.yaml and run `compost server deploy --apply`"
                ),
            },
            several => {
                let names: Vec<&str> = several.iter().map(|app| app.name.as_str()).collect();
                bail!(
                    "several mail apps run ({}); choose one with `instance`",
                    names.join(", ")
                )
            }
        }
    }

    /// Join the mail network, set the mapped variables, write the link record.
    fn prepare_application(
        &self,
        ctx: &AppCtx,
        link: &Value,
        out: &mut Contribution,
    ) -> Result<()> {
        let link: MailLink = parse_link(link)?;
        let mail: MailConfig = ctx.shared.configuration()?;
        let name = &ctx.shared.name;
        // Apps outside Docker reach the mocks on 127.0.0.1.
        let (ses, smtp, aws) = if out.is_for_host() {
            let (host, port) = host_address(ctx.shared)?;
            (
                format!("{host}:{port}"),
                (host.clone(), port + 1),
                format!("{host}:{}", port + 2),
            )
        } else {
            (
                format!("{name}-ses:{SES_PORT}"),
                (name.clone(), SMTP_PORT),
                format!("{name}-aws:{MOTO_PORT}"),
            )
        };
        let mut values: BTreeMap<&str, String> = BTreeMap::from([
            ("smtp_host", smtp.0.clone()),
            ("smtp_port", smtp.1.to_string()),
            ("smtp_url", format!("smtp://{}:{}", smtp.0, smtp.1)),
            ("ses_endpoint", format!("http://{ses}")),
            ("aws_region", mail.region.clone()),
            ("aws_access_key_id", ACCESS_KEY.to_string()),
            ("aws_secret_access_key", ACCESS_KEY.to_string()),
            (
                "inbox_url",
                mail.public_url()?.map(|u| u.url).unwrap_or_default(),
            ),
        ]);
        if mail.moto {
            values.insert("aws_endpoint", format!("http://{aws}"));
        } else if link
            .environment
            .values()
            .any(|v| v.contains("aws_endpoint"))
        {
            bail!("aws_endpoint needs `moto: true` in the configuration of {name}");
        }
        let network = network_name(name);
        for target in &link.targets {
            let service = ctx.compose_service(target)?;
            out.join_network(service, &network, None);
            for (variable, value) in &link.environment {
                out.set_env(service, variable, link_value(value, &values)?)?;
            }
        }
        let record = MailRecord {
            app: ctx.app.name.clone(),
            prefix: ctx.app.prefix.clone(),
            entry: link.name.clone(),
            targets: link.targets.clone(),
        };
        ctx.server.connector.write_file(
            &record_path(ctx, &link.name),
            serde_yaml_ng::to_string(&record)?.as_bytes(),
            0o644,
        )
    }

    fn before_apply_application(&self, _ctx: &AppCtx, _link: &Value) -> Result<()> {
        Ok(())
    }

    fn after_apply_application(&self, _ctx: &AppCtx, _link: &Value) -> Result<()> {
        Ok(())
    }

    fn describe_link(&self, ctx: &AppCtx, entry: &str) -> Result<String> {
        let path = record_path(ctx, entry);
        Ok(match ctx.server.connector.read_file(&path)? {
            Some(text) => {
                let record: MailRecord =
                    serde_yaml_ng::from_str(&text).with_context(|| format!("invalid {path}"))?;
                format!("sends from {}", record.targets.join(", "))
            }
            None => "no link record".to_string(),
        })
    }

    /// Mail lives in Mailpit's memory, for all apps together: nothing to delete.
    fn remove_application(&self, ctx: &AppCtx, entry: &str, _purge: bool) -> Result<()> {
        let path = shell_quote(&record_path(ctx, entry));
        ctx.server.connector.exec(&format!("rm -f -- {path}"))?;
        Ok(())
    }
}

fn check_type(type_name: &str) -> Result<()> {
    ensure!(
        type_name == TYPE,
        "unknown mail type `{type_name}` (known: {TYPE})"
    );
    Ok(())
}

/// The configured image, or Mailpit's of the version.
fn image(config: &SharedAppConfig) -> Result<String> {
    if let Some(image) = &config.image {
        return Ok(image.clone());
    }
    let version = config.version.unwrap_or(DEFAULT_VERSION);
    match IMAGE_TAGS.iter().find(|(major, _)| *major == version) {
        Some((_, tag)) => Ok(format!("axllent/mailpit:{tag}")),
        None => {
            bail!("set `image` for mailpit version {version}, e.g. axllent/mailpit:v{version}.0.0")
        }
    }
}

/// Docker network of one mail app; apps linked to it join this network.
fn network_name(name: &str) -> String {
    format!("compost-mail-{name}")
}

fn secrets_path(name: &str) -> String {
    format!("{}/secrets/mailpit.env", shared_dir(KIND, name))
}

/// The password in the `MP_UI_AUTH='admin:…'` line written by `render`.
fn read_password(env_file: &str) -> Option<String> {
    env_file.lines().find_map(|line| {
        line.strip_prefix(&format!("MP_UI_AUTH='{USER}:"))
            .and_then(|rest| rest.strip_suffix('\''))
            .map(str::to_string)
    })
}

fn record_path(ctx: &AppCtx, entry: &str) -> String {
    ctx.server.path(&format!(
        "{}/{}.yaml",
        connectors_dir(KIND, &ctx.shared.name),
        ctx.link_id(entry)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::Connector;

    fn config(text: &str) -> SharedAppConfig {
        let mut config: SharedAppConfig = serde_yaml_ng::from_str(text).unwrap();
        MailKind.apply_defaults(&mut config);
        config
    }

    #[test]
    fn validates_configuration() {
        MailKind
            .validate_shared(&config(
                "{name: mail, type: mailpit, configuration: {url: 'https://mail.example.com', moto: true, host_port: 18005}}",
            ))
            .unwrap();
        for (text, expected) in [
            ("{name: m, type: mailcatcher}", "unknown mail type"),
            (
                "{name: m, type: mailpit, configuration: {password: short}}",
                "at least 8 characters",
            ),
            (
                "{name: m, type: mailpit, configuration: {region: EU}}",
                "not an AWS region",
            ),
            (
                "{name: m, type: mailpit, configuration: {host_port: 65534}}",
                "the next two ports",
            ),
            (
                "{name: m, type: mailpit, configuration: {url: mail}}",
                "url",
            ),
            (
                "{name: m, type: mailpit, configuration: {smtp: true}}",
                "unknown field `smtp`",
            ),
        ] {
            let err = MailKind.validate_shared(&config(text)).unwrap_err();
            assert!(format!("{err:#}").contains(expected), "{text}: {err:#}");
        }
    }

    #[test]
    fn validates_links() {
        let app: AppConfig =
            serde_yaml_ng::from_str("{name: a, app_services: [{name: api, service: api}]}")
                .unwrap();
        let validate = |text: &str| {
            let link: Value = serde_yaml_ng::from_str(text).unwrap();
            MailKind
                .validate_link(&link, &app)
                .map_err(|e| format!("{e:#}"))
        };
        validate("{name: out, targets: [api], environment: {AWS_ENDPOINT_URL_SES: ses_endpoint, SMTP: '{smtp_host}:{smtp_port}'}}")
            .unwrap();
        for (text, expected) in [
            ("{name: out}", "at least one app service"),
            ("{name: out, targets: [web]}", "not in app_services"),
            (
                "{name: out, targets: [api], environment: {X: db_url}}",
                "unknown value `db_url`",
            ),
        ] {
            let err = validate(text).unwrap_err();
            assert!(err.contains(expected), "{text}: {err}");
        }
    }

    #[test]
    fn renders_mailpit_the_ses_mock_and_moto() {
        let connector = crate::connector::local::Local { verbose: false };
        let home = tempfile::tempdir().unwrap();
        let ctx = ServerCtx {
            connector: &connector,
            home: home.path().display().to_string(),
        };
        let render = |text: &str| MailKind.render(&ctx, &config(text)).unwrap();
        let file = |rendered: &Rendered, suffix: &str| {
            rendered
                .files
                .iter()
                .find(|f| f.path.ends_with(suffix))
                .unwrap()
                .contents
                .clone()
        };

        let plain = render("{name: mail, type: mailpit}");
        let compose = file(&plain, "compose.yaml");
        for expected in [
            "image: axllent/mailpit:v1.31.3\n",
            "image: ghcr.io/domdomegg/aws-ses-v2-local:2.10.2\n",
            r#"SMTP_TRANSPORT: '{"host":"compost-shared-mail","port":1025,"secure":false,"ignoreTLS":true}'"#,
            "aliases: [mail-ses]",
        ] {
            assert!(compose.contains(expected), "{expected} in {compose}");
        }
        assert!(
            !compose.contains("moto") && !compose.contains("ports:"),
            "{compose}"
        );
        assert_eq!(plain.networks, ["compost-mail-mail"]);

        let full =
            render("{name: mail, type: mailpit, configuration: {moto: true, host_port: 18005}}");
        let compose = file(&full, "compose.yaml");
        for expected in [
            "image: motoserver/moto:5.2.3\n",
            "aliases: [mail-aws]",
            r#"ports: ["127.0.0.1:18005:8005"]"#,
            r#"ports: ["127.0.0.1:18006:1025"]"#,
            r#"ports: ["127.0.0.1:18007:5000"]"#,
        ] {
            assert!(compose.contains(expected), "{expected} in {compose}");
        }

        // The generated password is written once and then kept.
        let secrets = plain
            .files
            .iter()
            .find(|f| f.path.ends_with("secrets/mailpit.env"))
            .unwrap();
        assert!(secrets.is_secret());
        let password = read_password(&secrets.contents).unwrap();
        assert_eq!(password.len(), 32);
        connector
            .write_file(&ctx.path(&secrets.path), secrets.contents.as_bytes(), 0o600)
            .unwrap();
        let again = render("{name: mail, type: mailpit}");
        assert!(file(&again, "mailpit.env").contains(&password));
        assert!(file(&again, "root-connector.yaml").contains(&format!("password: '{password}'")));
    }
}
