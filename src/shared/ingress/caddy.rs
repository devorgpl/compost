//! Caddy ingress type (spec §5.3, §9.2).

use anyhow::{Result, ensure};
use serde::Deserialize;

use super::{IngressType, base};
use crate::shared::SharedAppConfig;

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
            ensure!(email.contains('@'), "acme_email `{email}` is not an email address");
            ensure!(caddy.tls == Tls::Auto, "acme_email is only used with `tls: auto`");
        }
        Ok(())
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
}
