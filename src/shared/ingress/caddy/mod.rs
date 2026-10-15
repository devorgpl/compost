//! Caddy ingress type (spec §5.3, §9.2).

use anyhow::{Result, ensure};
use minijinja::context;
use serde::Deserialize;

use super::{IngressType, KIND, base};
use crate::shared::{
    Rendered, RenderedFile, ServerCtx, SharedAppConfig, render_template, shared_dir,
};

const DEFAULT_VERSION: u32 = 2;

pub struct Caddy;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct CaddyConfig {
    http_port: u16,
    https_port: u16,
    tls: Tls,
    acme_email: Option<String>,
}

impl Default for CaddyConfig {
    fn default() -> Self {
        Self {
            http_port: 80,
            https_port: 443,
            tls: Tls::Auto,
            acme_email: None,
        }
    }
}

#[derive(Debug, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Tls {
    Auto,
    Internal,
    Off,
}

impl IngressType for Caddy {
    fn type_name(&self) -> &'static str {
        "caddy"
    }

    /// Extends the generic ingress validation with Caddy's own settings.
    fn validate_config(&self, config: &SharedAppConfig) -> Result<()> {
        base::validate_config(config)?;

        if let Some(version) = config.version {
            ensure!(version == 2, "only caddy version 2 is supported, got {version}");
        }
        let caddy: CaddyConfig = config.configuration()?;
        ensure!(
            caddy.http_port != caddy.https_port,
            "http_port and https_port must differ"
        );
        if let Some(email) = &caddy.acme_email {
            ensure!(
                email.contains('@') && !email.contains(|c: char| c.is_whitespace() || "{}".contains(c)),
                "acme_email `{email}` is not an email address"
            );
            ensure!(caddy.tls == Tls::Auto, "acme_email is only used with `tls: auto`");
        }
        Ok(())
    }

    fn render(&self, _ctx: &ServerCtx, config: &SharedAppConfig) -> Result<Rendered> {
        let caddy: CaddyConfig = config.configuration()?;
        let version = config.version.unwrap_or(DEFAULT_VERSION);
        let image = config
            .image
            .clone()
            .unwrap_or_else(|| format!("caddy:{version}-alpine"));

        let mut options = Vec::new();
        if let Some(email) = &caddy.acme_email {
            options.push(format!("email {email}"));
        }
        match caddy.tls {
            Tls::Auto => {}
            Tls::Internal => options.push("local_certs".to_string()),
            Tls::Off => options.push("auto_https off".to_string()),
        }

        let compose = render_template(
            include_str!("templates/compose.yaml.j2"),
            context! {
                image,
                http_port => caddy.http_port,
                https_port => caddy.https_port,
                publish_https => caddy.tls != Tls::Off,
                network => base::NETWORK,
            },
        )?;
        let caddyfile = render_template(
            include_str!("templates/Caddyfile.j2"),
            context! { options },
        )?;

        let dir = shared_dir(KIND, &config.name);
        Ok(Rendered {
            files: vec![
                RenderedFile::new(format!("{dir}/compose.yaml"), compose),
                RenderedFile::new(format!("{dir}/conf/Caddyfile"), caddyfile),
                RenderedFile::new(
                    format!("{dir}/conf/sites/00-default.caddy"),
                    include_str!("templates/default-site.caddy").to_string(),
                ),
            ],
            dirs: vec![format!("{dir}/data"), format!("{dir}/config")],
            networks: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn validate(text: &str) -> Result<()> {
        let config: SharedAppConfig = serde_yaml_ng::from_str(text).unwrap();
        Caddy.validate_config(&config)
    }

    #[test]
    fn defaults_are_valid() {
        validate("{name: caddy-main, type: caddy}").unwrap();
    }

    #[test]
    fn runs_generic_ingress_validation_first() {
        let err = validate("{name: c, type: caddy, image: ' '}").unwrap_err();
        assert_eq!(err.to_string(), "image must not be empty");
    }

    #[test]
    fn checks_own_configuration() {
        let err = validate("{name: c, type: caddy, configuration: {http_port: 443}}").unwrap_err();
        assert_eq!(err.to_string(), "http_port and https_port must differ");

        let err = validate("{name: c, type: caddy, configuration: {tls: off, acme_email: a@b.c}}")
            .unwrap_err();
        assert_eq!(err.to_string(), "acme_email is only used with `tls: auto`");
    }

    #[test]
    fn rejects_unknown_configuration_keys() {
        let err = validate("{name: c, type: caddy, configuration: {ports: [80]}}").unwrap_err();
        assert!(err.to_string().contains("unknown field `ports`"), "{err}");
    }

    fn render(text: &str) -> Rendered {
        let config: SharedAppConfig = serde_yaml_ng::from_str(text).unwrap();
        let connector = crate::connector::local::Local { verbose: false };
        let ctx = ServerCtx { connector: &connector, home: "/nonexistent".into() };
        Caddy.render(&ctx, &config).unwrap()
    }

    fn file<'a>(rendered: &'a Rendered, suffix: &str) -> &'a str {
        &rendered.files.iter().find(|f| f.path.ends_with(suffix)).unwrap().contents
    }

    #[test]
    fn renders_defaults() {
        let rendered = render("{name: caddy-main, type: caddy}");
        let compose = file(&rendered, "compose.yaml");
        assert!(compose.contains("image: caddy:2-alpine"), "{compose}");
        assert!(compose.contains(r#""443:443/udp""#), "{compose}");
        assert!(compose.contains("networks: [depap-ingress]"), "{compose}");

        let caddyfile = file(&rendered, "conf/Caddyfile");
        assert!(!caddyfile.contains('{'), "no global options block: {caddyfile}");
        assert!(caddyfile.contains("import /etc/caddy/sites/*.caddy"));
        assert_eq!(
            rendered.dirs,
            ["shared/ingress/caddy-main/data", "shared/ingress/caddy-main/config"]
        );
    }

    #[test]
    fn renders_tls_options_and_ports() {
        let rendered = render(
            "{name: c, type: caddy, configuration: {http_port: 8080, tls: off}}",
        );
        let compose = file(&rendered, "compose.yaml");
        assert!(compose.contains(r#""8080:80""#), "{compose}");
        assert!(!compose.contains("443"), "https is not published: {compose}");
        assert!(file(&rendered, "conf/Caddyfile").contains("{\n\tauto_https off\n}"));

        let rendered = render("{name: c, type: caddy, configuration: {acme_email: a@b.c}}");
        assert!(file(&rendered, "conf/Caddyfile").contains("\temail a@b.c\n"));
    }
}
