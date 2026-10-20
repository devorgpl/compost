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
    /// Requests of this entry CrowdSec doesn't count.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignore: Vec<IgnoreRule>,
}

/// Requests CrowdSec doesn't count towards a ban: those matching all the
/// rule's keys. A key left out matches every request.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IgnoreRule {
    /// Request paths; `*` matches any characters, `/` included.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub methods: Vec<String>,
    /// Response statuses, e.g. 404.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub statuses: Vec<u16>,
}

/// Prefix of the access-log fields that mark a request for the CrowdSec
/// whitelist `compost/ignore`; the value is the statuses, or `any`.
const IGNORE_FIELD: &str = "compost_ignore_";

impl IgnoreRule {
    fn is_empty(&self) -> bool {
        self.paths.is_empty() && self.methods.is_empty() && self.statuses.is_empty()
    }
}

/// Rules with valid paths, methods and statuses. `everything`: a rule
/// without keys is allowed (it leaves out all of a site's requests).
pub fn validate_ignore(what: &str, rules: &[IgnoreRule], everything: bool) -> Result<()> {
    for (i, rule) in rules.iter().enumerate() {
        let at = format!("{what}[{i}]");
        ensure!(
            everything || !rule.is_empty(),
            "{at}: give paths, methods or statuses; a rule without them would leave out every request of every site"
        );
        validate_paths(&at, &rule.paths)?;
        validate_methods(&at, &rule.methods)?;
        for status in &rule.statuses {
            ensure!(
                (100..=599).contains(status),
                "{at}: status {status} is not an HTTP status (100 to 599)"
            );
        }
    }
    Ok(())
}

/// Caddy directives that mark the requests the rules match in the access
/// log, as `compost_ignore_<key><n>` fields. `key` keeps the fields of the
/// server's rules apart from a site's.
pub fn ignore_lines(key: &str, rules: &[IgnoreRule]) -> Vec<String> {
    let mut lines = Vec::new();
    for (i, rule) in rules.iter().enumerate() {
        let field = format!("{IGNORE_FIELD}{key}{i}");
        let value = if rule.statuses.is_empty() {
            "any".to_string()
        } else {
            let statuses: Vec<String> = rule.statuses.iter().map(u16::to_string).collect();
            statuses.join(" ")
        };
        if rule.paths.is_empty() && rule.methods.is_empty() {
            lines.push(format!("log_append {field} \"{value}\""));
            continue;
        }
        lines.push(format!("@{field} {{"));
        if !rule.paths.is_empty() {
            let paths: Vec<String> = rule.paths.iter().map(|p| path_regex(p)).collect();
            lines.push(format!("\tpath_regexp ^(?:{})$", paths.join("|")));
        }
        if !rule.methods.is_empty() {
            lines.push(format!("\tmethod {}", rule.methods.join(" ")));
        }
        lines.push("}".to_string());
        lines.push(format!("log_append @{field} {field} \"{value}\""));
    }
    lines
}

/// A path of a rule as a regular expression: `*` matches any characters.
/// Caddy's `path` matcher would not let a `*` in the middle match a `/`.
fn path_regex(path: &str) -> String {
    path.split('*')
        .map(|part| part.replace('.', "\\."))
        .collect::<Vec<_>>()
        .join(".*")
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
        validate_rate_limits("security.own_rate_limits", &self.own_rate_limits)?;
        validate_ignore("security.ignore", &self.ignore, true)
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
        validate_paths(&at, &zone.paths)?;
        validate_methods(&at, &zone.methods)?;
        ensure!(zone.events > 0, "{at}: events must be at least 1");
        ensure!(
            is_duration(&zone.window),
            "{at}: window `{}` is not a duration like 30s, 1m or 1h",
            zone.window
        );
    }
    Ok(())
}

fn validate_paths(at: &str, paths: &[String]) -> Result<()> {
    for path in paths {
        let valid = path.starts_with('/')
            && path
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "/-_.~*".contains(c));
        ensure!(
            valid,
            "{at}: path `{path}` must start with `/` and use only letters, digits and `/-_.~*`"
        );
    }
    Ok(())
}

fn validate_methods(at: &str, methods: &[String]) -> Result<()> {
    for method in methods {
        ensure!(
            METHODS.contains(&method.as_str()),
            "{at}: unknown method `{method}` (known: {})",
            METHODS.join(", ")
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
