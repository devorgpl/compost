//! `caddysecurebuild`: Caddy built with the CrowdSec bouncer, CrowdSec AppSec
//! and rate limiting, next to a CrowdSec agent that reads Caddy's access log.

use std::collections::HashSet;

use anyhow::{Result, ensure};
use minijinja::context;
use serde::Deserialize;

use crate::shared::ingress::security::{
    IgnoreRule, LinkSecurity, RateLimit, ignore_lines, is_address_or_network, is_duration,
    validate_ignore, validate_rate_limits,
};
use crate::shared::render_template;

/// Modules built into Caddy, pinned so a deploy builds the same binary.
const MODULES: [&str; 3] = [
    "github.com/hslatman/caddy-crowdsec-bouncer/http@v0.14.1",
    "github.com/hslatman/caddy-crowdsec-bouncer/appsec@v0.14.1",
    "github.com/mholt/caddy-ratelimit@5625512f24f6f59d6f64fb3aafe5eecff0b286db",
];

pub const CROWDSEC_IMAGE: &str = "crowdsecurity/crowdsec:v1.8.1";

/// Port of the CrowdSec AppSec component, on the private network.
const APPSEC_PORT: u16 = 7422;
/// Port of the CrowdSec Local API, on the private network.
const LAPI_PORT: u16 = 8080;

/// Always installed: the Caddy log parser, the HTTP scenarios (including
/// brute force: repeated POSTs answered 401 or 403) and the AppSec rules.
const BASE_COLLECTIONS: [&str; 4] = [
    "crowdsecurity/caddy",
    "crowdsecurity/base-http-scenarios",
    "crowdsecurity/appsec-virtual-patching",
    "crowdsecurity/appsec-generic-rules",
];
const CRS_COLLECTION: &str = "crowdsecurity/appsec-crs";

/// `configuration.security` of a `caddysecurebuild` ingress.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ServerSecurity {
    pub crowdsec: Crowdsec,
    pub appsec: Appsec,
    pub defaults: Defaults,
    pub rate_limits: Vec<RateLimit>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Crowdsec {
    /// Addresses and networks that are never banned.
    pub whitelist: Vec<String>,
    pub ban_duration: String,
    /// Hub collections installed besides the base ones.
    pub collections: Vec<String>,
    /// Hub scenarios installed besides those of the collections.
    pub scenarios: Vec<String>,
    /// Scenarios in CrowdSec's simulation mode: they raise alerts and never
    /// ban, e.g. one that bans an app's normal clients.
    pub simulated_scenarios: Vec<String>,
    /// Requests of every site CrowdSec doesn't count.
    pub ignore: Vec<IgnoreRule>,
}

impl Default for Crowdsec {
    fn default() -> Self {
        Self {
            whitelist: Vec::new(),
            ban_duration: "4h".to_string(),
            collections: Vec::new(),
            scenarios: Vec::new(),
            simulated_scenarios: Vec::new(),
            ignore: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Appsec {
    /// Also check requests against the OWASP Core Rule Set, out of band:
    /// matches don't block the request, repeated matches ban the address.
    pub crs: bool,
    /// How much of a request body AppSec checks; 0: all of it. The bouncer
    /// holds that much of every request in memory.
    pub max_body: ByteSize,
}

impl Default for Appsec {
    fn default() -> Self {
        Self {
            crs: false,
            max_body: ByteSize(1_000_000),
        }
    }
}

/// A number of bytes, written as `1MB`, `512KiB` or plain `1000000`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteSize(pub u64);

impl ByteSize {
    pub fn parse(text: &str) -> Option<ByteSize> {
        let text = text.trim();
        let split = text
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(text.len());
        let (number, unit) = text.split_at(split);
        let number: u64 = number.parse().ok()?;
        let factor: u64 = match unit.trim_start() {
            "" | "B" => 1,
            "kB" | "KB" => 1000,
            "MB" => 1000 * 1000,
            "GB" => 1000 * 1000 * 1000,
            "KiB" => 1 << 10,
            "MiB" => 1 << 20,
            "GiB" => 1 << 30,
            _ => return None,
        };
        number.checked_mul(factor).map(ByteSize)
    }
}

impl<'de> Deserialize<'de> for ByteSize {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let text = match serde_yaml_ng::Value::deserialize(deserializer)? {
            serde_yaml_ng::Value::String(text) => text,
            serde_yaml_ng::Value::Number(n) => n.to_string(),
            _ => String::new(),
        };
        ByteSize::parse(&text).ok_or_else(|| {
            D::Error::custom(format!(
                "`{text}` is not a size like 1MB, 512KiB or 0 (no limit)"
            ))
        })
    }
}

/// What a site gets for the fields its `security` leaves out.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Defaults {
    pub bans: bool,
    pub appsec: bool,
    /// Zones by name. Default: all of `rate_limits`.
    pub rate_limits: Option<Vec<String>>,
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            bans: true,
            appsec: true,
            rate_limits: None,
        }
    }
}

impl ServerSecurity {
    pub fn validate(&self) -> Result<()> {
        for entry in &self.crowdsec.whitelist {
            ensure!(
                is_address_or_network(entry),
                "security.crowdsec.whitelist: `{entry}` is not an IP address or network like 203.0.113.0/24"
            );
        }
        ensure!(
            is_duration(&self.crowdsec.ban_duration),
            "security.crowdsec.ban_duration: `{}` is not a duration like 30m or 4h",
            self.crowdsec.ban_duration
        );
        for (what, items) in [
            ("collections", &self.crowdsec.collections),
            ("scenarios", &self.crowdsec.scenarios),
            ("simulated_scenarios", &self.crowdsec.simulated_scenarios),
        ] {
            for item in items {
                ensure!(
                    is_hub_name(item),
                    "security.crowdsec.{what}: `{item}` is not a hub name like crowdsecurity/nginx"
                );
            }
        }
        validate_ignore("security.crowdsec.ignore", &self.crowdsec.ignore, false)?;
        validate_rate_limits("security.rate_limits", &self.rate_limits)?;
        if let Some(names) = &self.defaults.rate_limits {
            let mut seen = HashSet::new();
            for name in names {
                ensure!(
                    self.zone(name).is_some(),
                    "security.defaults.rate_limits: `{name}` is not in security.rate_limits"
                );
                ensure!(
                    seen.insert(name),
                    "security.defaults.rate_limits: `{name}` is listed twice"
                );
            }
        }
        Ok(())
    }

    fn zone(&self, name: &str) -> Option<&RateLimit> {
        self.rate_limits.iter().find(|zone| zone.name == name)
    }

    /// A zone a site's `security` names must be defined here.
    pub fn check_link(&self, security: &LinkSecurity) -> Result<()> {
        for name in security.zone_names() {
            ensure!(
                self.zone(name).is_some(),
                "security.rate_limits: `{name}` is not a rate limit of the ingress; define it in security.rate_limits of compost-server.yaml"
            );
        }
        Ok(())
    }

    fn default_zones(&self) -> Vec<String> {
        match &self.defaults.rate_limits {
            Some(names) => names.clone(),
            None => self.rate_limits.iter().map(|z| z.name.clone()).collect(),
        }
    }

    /// Hub collections for the agent's `COLLECTIONS`.
    pub fn collections(&self) -> Vec<String> {
        let mut all: Vec<String> = BASE_COLLECTIONS.iter().map(ToString::to_string).collect();
        if self.appsec.crs {
            all.push(CRS_COLLECTION.to_string());
        }
        for extra in &self.crowdsec.collections {
            if !all.contains(extra) {
                all.push(extra.clone());
            }
        }
        all
    }

    /// Directives of every site's `compost_log`: marks the requests of
    /// `crowdsec.ignore` in the access log.
    pub fn log_lines(&self) -> Vec<String> {
        ignore_lines("s", &self.crowdsec.ignore)
    }

    /// Global options of the Caddyfile: handler order and the bouncer. While
    /// the agent is down or starting, requests pass unchecked (bans fail
    /// open by default, AppSec with `appsec_fail_open`).
    pub fn options(&self, crowdsec_host: &str) -> Vec<String> {
        let mut lines = vec![
            "order crowdsec first".to_string(),
            "order appsec after crowdsec".to_string(),
            "order rate_limit after appsec".to_string(),
            "crowdsec {".to_string(),
            format!("\tapi_url http://{crowdsec_host}:{LAPI_PORT}"),
            "\tapi_key {env.CROWDSEC_API_KEY}".to_string(),
            format!("\tappsec_url http://{crowdsec_host}:{APPSEC_PORT}"),
            "\tappsec_fail_open".to_string(),
        ];
        // Without a limit the bouncer reads whole uploads into memory.
        if self.appsec.max_body.0 > 0 {
            lines.push(format!(
                "\tappsec_max_body_bytes {}",
                self.appsec.max_body.0
            ));
        }
        lines.push("\tticker_interval 15s".to_string());
        lines.push("}".to_string());
        lines
    }

    /// The snippets sites import, in the Caddyfile after `compost_site`.
    pub fn snippets(&self) -> Result<String> {
        let zones: Vec<_> = self
            .rate_limits
            .iter()
            .map(|zone| {
                context! {
                    name => zone.name.clone(),
                    lines => rate_limit_lines("{args[0]}", zone),
                }
            })
            .collect();
        render_template(
            include_str!("templates/security.caddy.j2"),
            context! {
                zones,
                bans => self.defaults.bans,
                appsec => self.defaults.appsec,
                default_zones => self.default_zones(),
            },
        )
    }

    /// Files of the CrowdSec agent, by name in `conf/crowdsec`.
    pub fn crowdsec_files(&self) -> Result<Vec<(&'static str, String)>> {
        let mut appsec_configs = vec!["crowdsecurity/appsec-default"];
        if self.appsec.crs {
            appsec_configs.push("crowdsecurity/crs");
        }
        let mut files = vec![
            (
                "acquis-caddy.yaml",
                include_str!("templates/crowdsec/acquis-caddy.yaml").to_string(),
            ),
            (
                "acquis-appsec.yaml",
                render_template(
                    include_str!("templates/crowdsec/acquis-appsec.yaml.j2"),
                    context! { port => APPSEC_PORT, appsec_configs },
                )?,
            ),
            (
                "ignore.yaml",
                include_str!("templates/crowdsec/ignore.yaml").to_string(),
            ),
            (
                "simulation.yaml",
                render_template(
                    include_str!("templates/crowdsec/simulation.yaml.j2"),
                    context! { scenarios => self.crowdsec.simulated_scenarios.clone() },
                )?,
            ),
            (
                "profiles.yaml",
                render_template(
                    include_str!("templates/crowdsec/profiles.yaml.j2"),
                    context! { duration => self.crowdsec.ban_duration.clone() },
                )?,
            ),
        ];
        if !self.crowdsec.whitelist.is_empty() {
            let (networks, addresses): (Vec<String>, Vec<String>) = self
                .crowdsec
                .whitelist
                .iter()
                .cloned()
                .partition(|e| e.contains('/'));
            files.push((
                "whitelist.yaml",
                render_template(
                    include_str!("templates/crowdsec/whitelist.yaml.j2"),
                    context! { addresses, networks },
                )?,
            ));
        }
        Ok(files)
    }
}

/// `owner/name`, as hub items are named.
fn is_hub_name(name: &str) -> bool {
    let part = |p: &str| {
        !p.is_empty()
            && p.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    };
    name.split_once('/')
        .is_some_and(|(owner, item)| part(owner) && part(item))
}

/// The Dockerfile that builds Caddy with the modules.
pub fn dockerfile(version: u32) -> Result<String> {
    render_template(
        include_str!("templates/Dockerfile.j2"),
        context! { version, modules => MODULES },
    )
}

/// The directives of a site, replacing a plain `import compost_site`: its
/// logs, then the filtering its `security` chooses. `site` names its
/// rate-limit zones, which must be unique across the configuration.
pub fn site_lines(security: Option<&LinkSecurity>, site: &str) -> Vec<String> {
    let Some(security) = security else {
        return vec![format!("import compost_site {site}")];
    };
    let mut lines = vec!["import compost_log".to_string()];
    lines.extend(ignore_lines("", &security.ignore));
    let mut choose = |choice: Option<bool>, snippet: &str| match choice {
        None => lines.push(format!("import compost_default_{snippet}")),
        Some(true) => lines.push(format!("import compost_{snippet}")),
        Some(false) => {}
    };
    choose(security.bans, "bans");
    choose(security.appsec, "appsec");
    match &security.rate_limits {
        None => lines.push(format!("import compost_default_rate_limits {site}")),
        Some(names) => {
            for name in names {
                lines.push(format!("import compost_rate_limit_{name} {site}"));
            }
        }
    }
    for zone in &security.own_rate_limits {
        lines.extend(rate_limit_lines(&format!("{site}_own"), zone));
    }
    lines
}

/// A `rate_limit` block for one zone, named `<prefix>_<name>`.
fn rate_limit_lines(prefix: &str, zone: &RateLimit) -> Vec<String> {
    let mut lines = vec![
        "rate_limit {".to_string(),
        format!("\tzone {prefix}_{} {{", zone.name),
    ];
    if !zone.methods.is_empty() || !zone.paths.is_empty() {
        lines.push("\t\tmatch {".to_string());
        if !zone.methods.is_empty() {
            lines.push(format!("\t\t\tmethod {}", zone.methods.join(" ")));
        }
        if !zone.paths.is_empty() {
            lines.push(format!("\t\t\tpath {}", zone.paths.join(" ")));
        }
        lines.push("\t\t}".to_string());
    }
    lines.extend([
        "\t\tkey {remote_host}".to_string(),
        format!("\t\tevents {}", zone.events),
        format!("\t\twindow {}", zone.window),
        "\t}".to_string(),
        "}".to_string(),
    ]);
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn security(text: &str) -> ServerSecurity {
        serde_yaml_ng::from_str(text).unwrap()
    }

    #[test]
    fn validates_server_security() {
        security(
            "{crowdsec: {whitelist: [203.0.113.7, 198.51.100.0/24], ban_duration: 30m, collections: [crowdsecurity/nginx]}, rate_limits: [{name: login, events: 5, window: 1m}], defaults: {rate_limits: [login]}}",
        )
        .validate()
        .unwrap();
        for (text, expected) in [
            ("{crowdsec: {whitelist: [me]}}", "not an IP address"),
            ("{crowdsec: {ban_duration: 4 hours}}", "not a duration"),
            ("{crowdsec: {collections: [nginx]}}", "not a hub name"),
            ("{crowdsec: {scenarios: ['a/b c']}}", "not a hub name"),
            (
                "{defaults: {rate_limits: [login]}}",
                "not in security.rate_limits",
            ),
            (
                "{rate_limits: [{name: a, events: 1, window: 1m}], defaults: {rate_limits: [a, a]}}",
                "listed twice",
            ),
            (
                "{rate_limits: [{name: a, events: 1, window: 1x}]}",
                "not a duration",
            ),
        ] {
            let err = security(text).validate().unwrap_err().to_string();
            assert!(err.contains(expected), "{text}: {err}");
        }
    }

    #[test]
    fn link_security_must_name_known_zones() {
        let server = security("{rate_limits: [{name: login, events: 5, window: 1m}]}");
        let link = |text: &str| serde_yaml_ng::from_str::<LinkSecurity>(text).unwrap();
        server.check_link(&link("{rate_limits: [login]}")).unwrap();
        let err = server
            .check_link(&link("{rate_limits: [api]}"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("`api` is not a rate limit"), "{err}");
    }

    #[test]
    fn collections_add_crs_and_extras_once() {
        let plain = security("{}").collections();
        assert_eq!(plain, BASE_COLLECTIONS);
        let more = security(
            "{appsec: {crs: true}, crowdsec: {collections: [crowdsecurity/caddy, crowdsecurity/nginx]}}",
        )
        .collections();
        assert_eq!(
            &more[4..],
            ["crowdsecurity/appsec-crs", "crowdsecurity/nginx"]
        );
    }

    #[test]
    fn renders_snippets() {
        let snippets = security(
            "{rate_limits: [{name: login, paths: [/login], methods: [POST], events: 10, window: 1m}, {name: api, events: 100, window: 10s}], defaults: {appsec: false, rate_limits: [login]}}",
        )
        .snippets()
        .unwrap();
        assert!(
            snippets.contains(
                "(compost_rate_limit_login) {\n\trate_limit {\n\t\tzone {args[0]}_login {\n\t\t\tmatch {\n\t\t\t\tmethod POST\n\t\t\t\tpath /login\n\t\t\t}\n\t\t\tkey {remote_host}\n\t\t\tevents 10\n\t\t\twindow 1m\n\t\t}\n\t}\n}\n"
            ),
            "{snippets}"
        );
        assert!(
            snippets.contains(
                "(compost_rate_limit_api) {\n\trate_limit {\n\t\tzone {args[0]}_api {\n\t\t\tkey"
            ),
            "{snippets}"
        );
        assert!(
            snippets.contains("(compost_default_bans) {\n\timport compost_bans\n}\n"),
            "{snippets}"
        );
        assert!(
            snippets.contains("(compost_default_appsec) {\n}\n"),
            "{snippets}"
        );
        assert!(
            snippets.contains(
                "(compost_default_rate_limits) {\n\timport compost_rate_limit_login {args[0]}\n}\n"
            ),
            "{snippets}"
        );
    }

    #[test]
    fn site_lines_follow_link_security() {
        assert_eq!(
            site_lines(None, "shop-prod-web"),
            ["import compost_site shop-prod-web"]
        );

        let link: LinkSecurity = serde_yaml_ng::from_str(
            "{bans: true, appsec: false, rate_limits: [login], own_rate_limits: [{name: api, paths: [/api/*], events: 3, window: 10s}]}",
        )
        .unwrap();
        let lines = site_lines(Some(&link), "shop-prod-web").join("\n");
        assert!(
            lines.starts_with(
                "import compost_log\nimport compost_bans\nimport compost_rate_limit_login shop-prod-web\nrate_limit {\n\tzone shop-prod-web_own_api {\n\t\tmatch {\n\t\t\tpath /api/*\n"
            ),
            "{lines}"
        );
        assert!(!lines.contains("appsec"), "{lines}");

        let lines = site_lines(Some(&LinkSecurity::default()), "s").join("\n");
        assert_eq!(
            lines,
            "import compost_log\nimport compost_default_bans\nimport compost_default_appsec\nimport compost_default_rate_limits s"
        );
    }

    #[test]
    fn renders_crowdsec_files() {
        let files = security("{}").crowdsec_files().unwrap();
        let names: Vec<_> = files.iter().map(|(name, _)| *name).collect();
        assert_eq!(
            names,
            [
                "acquis-caddy.yaml",
                "acquis-appsec.yaml",
                "ignore.yaml",
                "simulation.yaml",
                "profiles.yaml"
            ]
        );
        assert!(files[1].1.contains("  - crowdsecurity/appsec-default\n"));
        assert!(!files[1].1.contains("crs"));
        assert!(files[3].1.ends_with("simulation: false\nexclusions: []\n"));
        assert!(files[4].1.contains("duration: 4h"));

        let files = security(
            "{appsec: {crs: true}, crowdsec: {ban_duration: 30m, whitelist: [203.0.113.7, 198.51.100.0/24], simulated_scenarios: [crowdsecurity/http-probing]}}",
        )
        .crowdsec_files()
        .unwrap();
        assert!(
            files[1].1.contains("  - crowdsecurity/crs\n"),
            "{}",
            files[1].1
        );
        assert!(
            files[3]
                .1
                .ends_with("exclusions:\n  - crowdsecurity/http-probing\n"),
            "{}",
            files[3].1
        );
        assert!(files[4].1.contains("duration: 30m"));
        let (name, whitelist) = &files[5];
        assert_eq!(*name, "whitelist.yaml");
        assert!(
            whitelist.contains("  ip:\n    - 203.0.113.7\n  cidr:\n    - 198.51.100.0/24\n"),
            "{whitelist}"
        );
    }

    #[test]
    fn ignore_rules_mark_requests_in_the_access_log() {
        let server = security(
            "{crowdsec: {ignore: [{statuses: [404]}, {paths: [/healthz], methods: [GET, HEAD]}]}}",
        );
        server.validate().unwrap();
        assert_eq!(
            server.log_lines().join("\n"),
            "log_append compost_ignore_s0 \"404\"\n@compost_ignore_s1 {\n\tpath_regexp ^(?:/healthz)$\n\tmethod GET HEAD\n}\nlog_append @compost_ignore_s1 compost_ignore_s1 \"any\""
        );

        let link: LinkSecurity = serde_yaml_ng::from_str(
            "{ignore: [{paths: [/_matrix/client/*/presence/*, /v1.0/*], statuses: [404, 410]}]}",
        )
        .unwrap();
        link.validate().unwrap();
        let lines = site_lines(Some(&link), "s").join("\n");
        assert!(
            lines.starts_with(
                "import compost_log\n@compost_ignore_0 {\n\tpath_regexp ^(?:/_matrix/client/.*/presence/.*|/v1\\.0/.*)$\n}\nlog_append @compost_ignore_0 compost_ignore_0 \"404 410\"\nimport compost_default_bans"
            ),
            "{lines}"
        );

        // A site may leave all its requests out; the server can't.
        let everything: LinkSecurity = serde_yaml_ng::from_str("{ignore: [{}]}").unwrap();
        everything.validate().unwrap();
        assert!(
            site_lines(Some(&everything), "s")
                .contains(&"log_append compost_ignore_0 \"any\"".to_string())
        );
        for (text, expected) in [
            ("{crowdsec: {ignore: [{}]}}", "every request of every site"),
            (
                "{crowdsec: {ignore: [{statuses: [99]}]}}",
                "not an HTTP status",
            ),
            (
                "{crowdsec: {ignore: [{paths: [a]}]}}",
                "must start with `/`",
            ),
            ("{crowdsec: {ignore: [{methods: [get]}]}}", "unknown method"),
            (
                "{crowdsec: {simulated_scenarios: [http-probing]}}",
                "not a hub name",
            ),
        ] {
            let err = security(text).validate().unwrap_err().to_string();
            assert!(err.contains(expected), "{text}: {err}");
        }
    }

    #[test]
    fn appsec_reads_at_most_max_body() {
        let limit = |text: &str| {
            security(text)
                .options("crowdsec")
                .into_iter()
                .find(|line| line.contains("appsec_max_body_bytes"))
        };
        assert_eq!(limit("{}").unwrap(), "\tappsec_max_body_bytes 1000000");
        assert_eq!(
            limit("{appsec: {max_body: 512KiB}}").unwrap(),
            "\tappsec_max_body_bytes 524288"
        );
        assert_eq!(
            limit("{appsec: {max_body: 2000}}").unwrap(),
            "\tappsec_max_body_bytes 2000"
        );
        assert_eq!(limit("{appsec: {max_body: 0}}"), None);
        for (text, size) in [("10 MB", 10_000_000), ("1GiB", 1 << 30), ("5kB", 5000)] {
            assert_eq!(ByteSize::parse(text), Some(ByteSize(size)), "{text}");
        }
        for bad in ["", "MB", "1 TB", "1.5MB", "-1"] {
            assert_eq!(ByteSize::parse(bad), None, "{bad}");
        }
        let err = serde_yaml_ng::from_str::<ServerSecurity>("{appsec: {max_body: 1mb}}")
            .unwrap_err()
            .to_string();
        assert!(err.contains("`1mb` is not a size like 1MB"), "{err}");
    }

    #[test]
    fn dockerfile_pins_the_modules() {
        let dockerfile = dockerfile(2).unwrap();
        assert!(
            dockerfile.contains("FROM caddy:2-builder AS builder"),
            "{dockerfile}"
        );
        assert!(dockerfile.contains("FROM caddy:2-alpine"), "{dockerfile}");
        for module in MODULES {
            assert!(
                dockerfile.contains(&format!("--with {module}")),
                "{dockerfile}"
            );
        }
    }
}
