//! Request filtering of an ingress: the `security` of a link or site, and the
//! rate-limit zones it uses. Only ingress types that filter requests accept it.

use std::collections::HashSet;
use std::net::IpAddr;

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

use crate::config::check_name;

/// `security` of an ingress entry in compost.yaml, or of a site in
/// compost-server.yaml. A field left out follows the ingress's `defaults`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LinkSecurity {
    /// Block addresses CrowdSec has banned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bans: Option<bool>,
    /// Inspect requests with CrowdSec AppSec.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub appsec: Option<bool>,
    /// Rate-limit zones of the ingress, by name; replaces the default list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limits: Option<Vec<String>>,
    /// Zones of this entry only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub own_rate_limits: Vec<RateLimit>,
}

/// A rate-limit zone: at most `events` matching requests per client address
/// in `window`; more get 429.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimit {
    pub name: String,
    /// Request paths; `*` matches any characters. Default: every path.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    /// HTTP methods. Default: every method.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub methods: Vec<String>,
    pub events: u32,
    /// A duration, e.g. `1m`.
    pub window: String,
}

const METHODS: [&str; 9] = [
    "GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS", "CONNECT", "TRACE",
];

impl LinkSecurity {
    pub fn validate(&self) -> Result<()> {
        if let Some(names) = &self.rate_limits {
            let mut seen = HashSet::new();
            for name in names {
                check_name("rate limit", name)?;
                ensure!(
                    seen.insert(name),
                    "security.rate_limits: `{name}` is listed twice"
                );
            }
        }
        validate_rate_limits("security.own_rate_limits", &self.own_rate_limits)
    }

    /// Zones of the ingress this uses by name.
    pub fn zone_names(&self) -> &[String] {
        self.rate_limits.as_deref().unwrap_or_default()
    }
}

/// Zones with unique, valid names, paths, methods and limits.
pub fn validate_rate_limits(what: &str, zones: &[RateLimit]) -> Result<()> {
    let mut names = HashSet::new();
    for zone in zones {
        check_name("rate limit name", &zone.name)?;
        ensure!(
            names.insert(&zone.name),
            "{what}: `{}` is defined twice",
            zone.name
        );
        let at = format!("{what}[{}]", zone.name);
        for path in &zone.paths {
            let valid = path.starts_with('/')
                && path
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "/-_.~*".contains(c));
            ensure!(
                valid,
                "{at}: path `{path}` must start with `/` and use only letters, digits and `/-_.~*`"
            );
        }
        for method in &zone.methods {
            ensure!(
                METHODS.contains(&method.as_str()),
                "{at}: unknown method `{method}` (known: {})",
                METHODS.join(", ")
            );
        }
        ensure!(zone.events > 0, "{at}: events must be at least 1");
        ensure!(
            is_duration(&zone.window),
            "{at}: window `{}` is not a duration like 30s, 1m or 1h",
            zone.window
        );
    }
    Ok(())
}

/// Numbers with a unit each, e.g. `90s`, `1h30m`.
pub fn is_duration(text: &str) -> bool {
    let mut digits = 0;
    let mut parts = 0;
    for c in text.chars() {
        if c.is_ascii_digit() {
            digits += 1;
        } else if "smh".contains(c) && digits > 0 {
            digits = 0;
            parts += 1;
        } else {
            return false;
        }
    }
    digits == 0 && parts > 0 && text.chars().any(|c| c.is_ascii_digit() && c != '0')
}

/// An IP address, or a network such as `203.0.113.0/24`.
pub fn is_address_or_network(text: &str) -> bool {
    let (address, prefix) = match text.split_once('/') {
        Some((address, prefix)) => (address, Some(prefix)),
        None => (text, None),
    };
    let Ok(address) = address.parse::<IpAddr>() else {
        return false;
    };
    let max = if address.is_ipv4() { 32 } else { 128 };
    prefix.is_none_or(|p| p.parse::<u8>().is_ok_and(|p| p <= max))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zones(text: &str) -> Result<()> {
        let zones: Vec<RateLimit> = serde_yaml_ng::from_str(text).unwrap();
        validate_rate_limits("rate_limits", &zones)
    }

    #[test]
    fn validates_rate_limits() {
        zones("[{name: login, paths: [/login, /user/*], methods: [POST], events: 10, window: 1m}]")
            .unwrap();
        for (text, expected) in [
            (
                "[{name: Login, events: 1, window: 1m}]",
                "invalid rate limit name",
            ),
            (
                "[{name: a, events: 1, window: 1m}, {name: a, events: 1, window: 1m}]",
                "defined twice",
            ),
            (
                "[{name: a, paths: [login], events: 1, window: 1m}]",
                "must start with `/`",
            ),
            (
                "[{name: a, paths: ['/a b'], events: 1, window: 1m}]",
                "must start with `/`",
            ),
            (
                "[{name: a, methods: [post], events: 1, window: 1m}]",
                "unknown method",
            ),
            ("[{name: a, events: 0, window: 1m}]", "at least 1"),
            ("[{name: a, events: 1, window: 1d}]", "not a duration"),
        ] {
            let err = zones(text).unwrap_err().to_string();
            assert!(err.contains(expected), "{text}: {err}");
        }
    }

    #[test]
    fn link_security_names_zones_once() {
        let security: LinkSecurity =
            serde_yaml_ng::from_str("{rate_limits: [login, login]}").unwrap();
        let err = security.validate().unwrap_err().to_string();
        assert!(err.contains("listed twice"), "{err}");
        let err = serde_yaml_ng::from_str::<LinkSecurity>("{waf: true}").unwrap_err();
        assert!(err.to_string().contains("unknown field `waf`"), "{err}");
    }

    #[test]
    fn checks_durations_and_addresses() {
        for good in ["30s", "1m", "1h30m", "4h"] {
            assert!(is_duration(good), "{good}");
        }
        for bad in ["", "m", "10", "0s", "1d", "1 m", "-1m"] {
            assert!(!is_duration(bad), "{bad}");
        }
        for good in ["203.0.113.7", "203.0.113.0/24", "2001:db8::/32", "::1"] {
            assert!(is_address_or_network(good), "{good}");
        }
        for bad in [
            "203.0.113.0/33",
            "example.com",
            "203.0.113.0/",
            "2001:db8::/129",
        ] {
            assert!(!is_address_or_network(bad), "{bad}");
        }
    }
}
