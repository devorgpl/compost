//! Configuration files: local compost home, server config, merging.

pub mod app;
pub mod backup;
pub mod command;
pub mod local;
pub mod merge;
pub mod server;

use anyhow::Result;

use crate::error::config_err;

/// Names of servers, apps, shared apps and links: `^[a-z][a-z0-9-]{0,30}$` (spec §3.1).
pub fn check_name(what: &str, name: &str) -> Result<()> {
    let mut chars = name.chars();
    let valid = name.len() <= 31
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if valid {
        Ok(())
    } else {
        Err(config_err(format!(
            "invalid {what} `{name}`: use lowercase letters, digits and `-`, start with a letter, max 31 characters"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::check_name;

    #[test]
    fn accepts_valid_names() {
        for name in [
            "a",
            "prod1",
            "caddy-main",
            "x234567890123456789012345678901",
        ] {
            assert!(check_name("name", name).is_ok(), "{name}");
        }
    }

    #[test]
    fn rejects_invalid_names() {
        for name in [
            "",
            "1abc",
            "Prod",
            "my_app",
            "-a",
            "x2345678901234567890123456789012",
        ] {
            assert!(check_name("name", name).is_err(), "{name}");
        }
    }
}
