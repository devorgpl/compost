//! Postgres database type (spec §5.3, §9.3).

use anyhow::{Result, ensure};
use serde::Deserialize;

use super::{DatabaseType, base};
use crate::shared::SharedAppConfig;

pub struct Postgres;

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

    /// Extends the generic database validation with Postgres' own settings.
    fn validate_config(&self, config: &SharedAppConfig) -> Result<()> {
        base::validate_config(config)?;

        let postgres: PostgresConfig = config.configuration()?;
        if let Some(password) = &postgres.root_password {
            ensure!(!password.is_empty(), "root_password must not be empty");
            // The password is written as one line of an env file on the server.
            ensure!(
                !password.contains(['\n', '\r']),
                "root_password must be a single line"
            );
        }
        Ok(())
    }
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
}
