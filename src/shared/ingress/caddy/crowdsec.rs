//! What the CrowdSec agent of a `caddysecurebuild` ingress did: its alerts
//! and active bans, for `server state` and the `bans` notifications.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use serde::Deserialize;

use super::crowdsec_service;
use crate::connector::shell_quote;
use crate::shared::ingress::KIND;
use crate::shared::{ServerCtx, project_name, shared_dir};

/// Alerts listed in a notification; the rest are counted.
const LISTED: usize = 20;
/// Addresses in the daily summary.
const TOP_ADDRESSES: usize = 10;

/// An alert of `cscli alerts list -o json`, with the fields compost uses.
#[derive(Debug, Clone, Deserialize)]
pub struct Alert {
    pub id: u64,
    #[serde(default)]
    pub scenario: String,
    /// `crowdsec` when a scenario raised it, `cscli` for a manual ban, `waf`
    /// for a request AppSec blocked.
    #[serde(default)]
    pub kind: String,
    /// Raised by a scenario in simulation mode: its ban isn't enforced.
    #[serde(default)]
    pub simulated: bool,
    #[serde(default)]
    pub source: Source,
    #[serde(default)]
    pub decisions: Option<Vec<Decision>>,
    #[serde(default)]
    pub events: Option<Vec<AlertEvent>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Source {
    /// The address, or the range for a range ban.
    #[serde(default)]
    pub value: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Decision {
    /// e.g. `ban`.
    #[serde(rename = "type", default)]
    pub action: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AlertEvent {
    #[serde(default)]
    pub meta: Option<Vec<Meta>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Meta {
    #[serde(default)]
    pub key: String,
    /// Missing in some of AppSec's.
    #[serde(default)]
    pub value: String,
}

impl Alert {
    /// A scenario's alert that banned the address, or would have.
    pub fn is_ban(&self) -> bool {
        self.kind == "crowdsec"
            && self
                .decisions
                .iter()
                .flatten()
                .any(|decision| decision.action == "ban")
    }

    /// Domains of the requests behind the alert.
    pub fn sites(&self) -> BTreeSet<&str> {
        self.events
            .iter()
            .flatten()
            .flat_map(|event| event.meta.iter().flatten())
            .filter(|meta| meta.key == "target_fqdn")
            .map(|meta| meta.value.as_str())
            .collect()
    }

    fn line(&self) -> String {
        let sites: Vec<&str> = self.sites().into_iter().collect();
        let mut line = format!("{}  {}", self.source.value, self.scenario);
        if !sites.is_empty() {
            line.push_str(&format!("  {}", sites.join(" ")));
        }
        line.push_str(&format!("  alert {}", self.id));
        if self.simulated {
            line.push_str("  (simulated)");
        }
        line
    }
}

/// Active bans by where they come from.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ActiveBans {
    /// Bans of this server's scenarios, by scenario.
    pub scenarios: BTreeMap<String, usize>,
    /// Of simulated scenarios: not enforced.
    pub simulated: usize,
    /// Added with `cscli` (`<ingress>.ban`).
    pub manual: usize,
    /// From the community blocklist and other lists.
    pub blocklists: usize,
}

impl ActiveBans {
    fn describe(&self) -> String {
        let local: usize = self.scenarios.values().sum();
        let mut parts = Vec::new();
        if local > 0 {
            let mut scenarios: Vec<(&String, &usize)> = self.scenarios.iter().collect();
            scenarios.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
            let scenarios: Vec<String> = scenarios
                .iter()
                .map(|(scenario, n)| format!("{scenario} {n}"))
                .collect();
            parts.push(format!(
                "{local} by this server's scenarios ({})",
                scenarios.join(", ")
            ));
        }
        if self.simulated > 0 {
            parts.push(format!("{} simulated", self.simulated));
        }
        if self.manual > 0 {
            parts.push(format!("{} manual", self.manual));
        }
        if self.blocklists > 0 {
            parts.push(format!("{} from blocklists", self.blocklists));
        }
        if parts.is_empty() {
            "none".to_string()
        } else {
            parts.join(", ")
        }
    }
}

/// Run `cscli` in the agent's container of ingress `name`.
fn cscli(ctx: &ServerCtx, name: &str, args: &str) -> Result<String> {
    ctx.connector
        .exec(&format!(
            "cd {} && docker compose -p {} exec -T {} cscli {args}",
            shell_quote(&ctx.path(&shared_dir(KIND, name))),
            shell_quote(&project_name(KIND, name)),
            shell_quote(&crowdsec_service(name)),
        ))
        .context("cscli in the CrowdSec agent failed")
}

/// Alerts of the last `since` (e.g. `24h`), all of them.
pub fn alerts(ctx: &ServerCtx, name: &str, since: &str) -> Result<Vec<Alert>> {
    parse_alerts(&cscli(
        ctx,
        name,
        &format!("alerts list --since {since} -l 0 -o json"),
    )?)
}

/// `cscli … -o json` prints `null` when there is nothing.
fn parse_alerts(text: &str) -> Result<Vec<Alert>> {
    let text = text.trim();
    if text.is_empty() || text == "null" {
        return Ok(Vec::new());
    }
    serde_json::from_str(text).context("unexpected output of `cscli alerts list`")
}

/// The bans in force, blocklists included. Raw output, since the JSON of a
/// blocklist's thousands of bans is large and only counted.
pub fn active_bans(ctx: &ServerCtx, name: &str) -> Result<ActiveBans> {
    Ok(parse_decisions(&cscli(
        ctx,
        name,
        "decisions list -a -l 0 -t ban -o raw",
    )?))
}

/// Lines of `id,source,ip,reason,action,country,as,events_count,expiration,simulated,alert_id`.
fn parse_decisions(text: &str) -> ActiveBans {
    let mut bans = ActiveBans::default();
    for line in text.lines().filter(|line| !line.starts_with("id,")) {
        let fields: Vec<&str> = line.split(',').collect();
        let n = fields.len();
        if n < 11 {
            continue;
        }
        // The reason is the only field that could hold a comma.
        let reason = fields[3..n - 7].join(",");
        match (fields[1], fields[n - 2] == "true") {
            ("crowdsec", true) => bans.simulated += 1,
            ("crowdsec", false) => *bans.scenarios.entry(reason).or_default() += 1,
            ("cscli", _) => bans.manual += 1,
            _ => bans.blocklists += 1,
        }
    }
    bans
}

/// `server state` lines: the bans in force and the last day's alerts.
pub fn state_lines(ctx: &ServerCtx, name: &str) -> Vec<String> {
    let read = || -> Result<Vec<String>> {
        let bans = active_bans(ctx, name)?;
        let alerts = alerts(ctx, name, "24h")?;
        Ok(describe_state(&bans, &alerts))
    };
    read().unwrap_or_else(|err| vec![format!("crowdsec: {err:#}")])
}

fn describe_state(bans: &ActiveBans, alerts: &[Alert]) -> Vec<String> {
    let scenario_alerts: Vec<&Alert> = alerts.iter().filter(|a| a.kind == "crowdsec").collect();
    let simulated: Vec<&Alert> = scenario_alerts
        .iter()
        .copied()
        .filter(|a| a.simulated)
        .collect();
    let n = scenario_alerts.len();
    let mut day = format!("last 24h: {n} alert{}", if n == 1 { "" } else { "s" });
    if !simulated.is_empty() {
        day.push_str(&format!(
            ", {} simulated ({})",
            simulated.len(),
            count_by_scenario(&simulated)
        ));
    }
    let blocked = alerts.iter().filter(|a| a.kind == "waf").count();
    if blocked > 0 {
        day.push_str(&format!(
            ", {blocked} request{} blocked by AppSec",
            if blocked == 1 { "" } else { "s" }
        ));
    }
    vec![format!("bans: {}", bans.describe()), day]
}

/// `a/x 2, a/y 1`, most first.
fn count_by_scenario(alerts: &[&Alert]) -> String {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for alert in alerts {
        *counts.entry(&alert.scenario).or_default() += 1;
    }
    let mut counts: Vec<(&str, usize)> = counts.into_iter().collect();
    counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let parts: Vec<String> = counts.iter().map(|(s, n)| format!("{s} {n}")).collect();
    parts.join(", ")
}

/// A notification's text.
#[derive(Debug, PartialEq, Eq)]
pub struct Message {
    pub title: String,
    pub body: String,
    /// Something was banned for real, not only simulated.
    pub banned: bool,
}

fn addresses(n: usize) -> String {
    if n == 1 {
        "1 address".to_string()
    } else {
        format!("{n} addresses")
    }
}

/// The `bans` notification for new ban alerts; none when there are none.
pub fn bans_message(alerts: &[&Alert], ingress: &str) -> Option<Message> {
    if alerts.is_empty() {
        return None;
    }
    let simulated = alerts.iter().filter(|a| a.simulated).count();
    let banned = alerts.len() - simulated;
    let title = match (banned, simulated) {
        (0, m) => format!("{} would have been banned (simulated)", addresses(m)),
        (n, 0) => format!("{} banned", addresses(n)),
        (n, m) => format!(
            "{} banned, {m} more would have been (simulated)",
            addresses(n)
        ),
    };
    let mut lines: Vec<String> = alerts.iter().take(LISTED).map(|a| a.line()).collect();
    if alerts.len() > LISTED {
        lines.push(format!("and {} more", alerts.len() - LISTED));
    }
    lines.push(String::new());
    lines.push(format!(
        "The requests behind one: compost server cmd {ingress}.alert <id>"
    ));
    if banned > 0 {
        lines.push(format!(
            "Lift a ban: compost server cmd {ingress}.unban <address>"
        ));
    }
    Some(Message {
        title,
        body: lines.join("\n"),
        banned: banned > 0,
    })
}

/// The `bans_daily` summary of a day's alerts.
pub fn daily_message(alerts: &[Alert], bans: &ActiveBans, ingress: &str) -> Message {
    let ban_alerts: Vec<&Alert> = alerts.iter().filter(|a| a.is_ban()).collect();
    let simulated = ban_alerts.iter().filter(|a| a.simulated).count();
    let total = ban_alerts.len();
    let title = match (total, simulated) {
        (0, _) => "Bans in the last 24h: none".to_string(),
        (n, 0) => format!("Bans in the last 24h: {n}"),
        (n, m) => format!("Bans in the last 24h: {n} ({m} simulated)"),
    };
    let mut lines = Vec::new();
    if total > 0 {
        lines.push("By scenario:".to_string());
        let mut scenarios: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
        for alert in &ban_alerts {
            let entry = scenarios.entry(&alert.scenario).or_default();
            if alert.simulated {
                entry.1 += 1;
            } else {
                entry.0 += 1;
            }
        }
        for (scenario, (banned, simulated)) in scenarios {
            let mut counts = Vec::new();
            if banned > 0 {
                counts.push(format!("{banned} banned"));
            }
            if simulated > 0 {
                counts.push(format!("{simulated} simulated"));
            }
            lines.push(format!("  {scenario}  {}", counts.join(", ")));
        }

        lines.push("Most alerts:".to_string());
        let mut by_address: BTreeMap<&str, (usize, BTreeSet<&str>)> = BTreeMap::new();
        for alert in &ban_alerts {
            let entry = by_address.entry(&alert.source.value).or_default();
            entry.0 += 1;
            entry.1.extend(alert.sites());
        }
        let mut by_address: Vec<_> = by_address.into_iter().collect();
        by_address.sort_by(|a, b| b.1.0.cmp(&a.1.0).then(a.0.cmp(b.0)));
        for (address, (n, sites)) in by_address.iter().take(TOP_ADDRESSES) {
            let sites: Vec<&str> = sites.iter().copied().collect();
            lines.push(
                format!("  {address}  {n}  {}", sites.join(" "))
                    .trim_end()
                    .to_string(),
            );
        }
        if by_address.len() > TOP_ADDRESSES {
            lines.push(format!(
                "  and {} more",
                addresses(by_address.len() - TOP_ADDRESSES)
            ));
        }
    }
    lines.push(format!("Bans in force: {}", bans.describe()));
    lines.push(format!("All alerts: compost server cmd {ingress}.alerts"));
    Message {
        title,
        body: lines.join("\n"),
        banned: total > simulated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from crowdsecurity/crowdsec v1.8.1: a ban of
    /// http-probing, a simulated one of http-crawl-non_statics, and a manual
    /// ban (`cscli decisions add`); events cut to one each. Then a request
    /// AppSec blocked, written after one seen on a server: a `meta` without
    /// `value`, no decisions.
    const ALERTS: &str = include_str!("testdata/alerts.json");

    fn fixture() -> Vec<Alert> {
        parse_alerts(ALERTS).unwrap()
    }

    #[test]
    fn reads_alerts() {
        let alerts = fixture();
        assert_eq!(alerts.len(), 4);
        let probing = &alerts[0];
        assert_eq!(probing.scenario, "crowdsecurity/http-probing");
        assert_eq!(probing.source.value, "198.51.100.7");
        assert!(probing.is_ban() && !probing.simulated);
        assert_eq!(
            probing.sites().into_iter().collect::<Vec<_>>(),
            ["app.example.com"]
        );
        assert!(alerts[1].is_ban() && alerts[1].simulated);
        // A manual ban isn't a scenario's.
        assert_eq!(alerts[2].kind, "cscli");
        assert!(!alerts[2].is_ban());
        assert_eq!(alerts[3].kind, "waf");
        assert!(!alerts[3].is_ban());
        assert!(parse_alerts("null\n").unwrap().is_empty());
        assert!(parse_alerts("[]").unwrap().is_empty());
        assert!(parse_alerts("oops").is_err());
    }

    #[test]
    fn counts_active_bans_by_origin() {
        let raw = "id,source,ip,reason,action,country,as,events_count,expiration,simulated,alert_id
3,cscli,Ip:192.0.2.1,manual 'ban' from 'localhost',ban,,,1,4m32s,false,3
2,crowdsec,Ip:198.51.100.8,crowdsecurity/http-crawl-non_statics,ban,,,41,3h58m57s,true,2
1,crowdsec,Ip:198.51.100.7,crowdsecurity/http-probing,ban,,,11,3h58m57s,false,1
5,crowdsec,Ip:198.51.100.9,crowdsecurity/http-probing,ban,,,11,3h58m57s,false,4
9,CAPI,Ip:203.0.113.1,crowdsecurity/ssh-bf,ban,,,0,160h,false,7
10,lists,Ip:203.0.113.2,firehol_level1,ban,,,0,23h,false,8
";
        let bans = parse_decisions(raw);
        assert_eq!(bans.scenarios["crowdsecurity/http-probing"], 2);
        assert_eq!((bans.simulated, bans.manual, bans.blocklists), (1, 1, 2));
        assert_eq!(
            bans.describe(),
            "2 by this server's scenarios (crowdsecurity/http-probing 2), 1 simulated, 1 manual, 2 from blocklists"
        );
        assert_eq!(
            parse_decisions("No active decisions\n"),
            ActiveBans::default()
        );
        assert_eq!(ActiveBans::default().describe(), "none");
    }

    #[test]
    fn describes_state() {
        let bans = ActiveBans {
            blocklists: 14210,
            ..ActiveBans::default()
        };
        assert_eq!(
            describe_state(&bans, &fixture()),
            [
                "bans: 14210 from blocklists",
                "last 24h: 2 alerts, 1 simulated (crowdsecurity/http-crawl-non_statics 1), 1 request blocked by AppSec",
            ]
        );
        assert_eq!(
            describe_state(&ActiveBans::default(), &[]),
            ["bans: none", "last 24h: 0 alerts"]
        );
    }

    #[test]
    fn bans_message_lists_new_bans() {
        let alerts = fixture();
        let bans: Vec<&Alert> = alerts.iter().filter(|a| a.is_ban()).collect();
        let message = bans_message(&bans, "caddy-main").unwrap();
        assert_eq!(
            message.title,
            "1 address banned, 1 more would have been (simulated)"
        );
        assert!(message.banned);
        assert_eq!(
            message.body,
            "198.51.100.7  crowdsecurity/http-probing  app.example.com  alert 1
198.51.100.8  crowdsecurity/http-crawl-non_statics  app.example.com  alert 2  (simulated)

The requests behind one: compost server cmd caddy-main.alert <id>
Lift a ban: compost server cmd caddy-main.unban <address>"
        );

        let simulated = bans_message(&bans[1..], "caddy-main").unwrap();
        assert_eq!(
            simulated.title,
            "1 address would have been banned (simulated)"
        );
        assert!(!simulated.banned && !simulated.body.contains("unban"));
        assert_eq!(bans_message(&[], "caddy-main"), None);

        let many: Vec<&Alert> = std::iter::repeat_n(bans[0], 25).collect();
        let message = bans_message(&many, "caddy-main").unwrap();
        assert_eq!(message.title, "25 addresses banned");
        assert!(message.body.contains("\nand 5 more\n"), "{}", message.body);
    }

    #[test]
    fn daily_message_sums_up() {
        let bans = ActiveBans {
            blocklists: 3,
            ..ActiveBans::default()
        };
        let message = daily_message(&fixture(), &bans, "caddy-main");
        assert_eq!(message.title, "Bans in the last 24h: 2 (1 simulated)");
        assert_eq!(
            message.body,
            "By scenario:
  crowdsecurity/http-crawl-non_statics  1 simulated
  crowdsecurity/http-probing  1 banned
Most alerts:
  198.51.100.7  1  app.example.com
  198.51.100.8  1  app.example.com
Bans in force: 3 from blocklists
All alerts: compost server cmd caddy-main.alerts"
        );
        let quiet = daily_message(&[], &ActiveBans::default(), "caddy-main");
        assert_eq!(quiet.title, "Bans in the last 24h: none");
        assert_eq!(
            quiet.body,
            "Bans in force: none\nAll alerts: compost server cmd caddy-main.alerts"
        );
        assert!(!quiet.banned);
    }
}
