//! Generic ingress behavior, used by every ingress type unless it overrides it.

use std::collections::HashSet;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use super::KIND;
use crate::config::app::AppConfig;
use crate::config::check_name;
use crate::connector::shell_quote;
use crate::shared::{
    AppCtx, Contribution, ItemWriter, Resolution, Role, SharedAppConfig, connectors_dir,
};

/// Docker network shared by the ingress and every app service it routes to.
pub const NETWORK: &str = "compost-ingress";

/// An entry under `services.ingress` in compost.yaml (spec §6.1).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IngressLink {
    pub name: String,
    /// Skip the entry when no domains are given.
    #[serde(default)]
    pub optional: bool,
    #[serde(default)]
    pub domains: Vec<String>,
    #[serde(default)]
    pub routes: Vec<Route>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub name: String,
    /// `app_services` name.
    pub target: String,
    pub port: u16,
    #[serde(default = "root_path")]
    pub path: String,
    #[serde(default)]
    pub strip_prefix: bool,
}

/// A route with its upstream resolved; stored in the link record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteRecord {
    pub name: String,
    pub path: String,
    /// `<alias>:<port>` on the ingress network.
    pub upstream: String,
    pub strip_prefix: bool,
}

/// `connectors/ingress/<ingress>/<instance>-<entry>.yaml` (spec §8.3).
#[derive(Debug, Serialize, Deserialize)]
pub struct IngressRecord {
    pub app: String,
    pub prefix: String,
    pub entry: String,
    pub domains: Vec<String>,
    pub routes: Vec<RouteRecord>,
}

fn root_path() -> String {
    "/".to_string()
}

pub fn validate_config(config: &SharedAppConfig) -> Result<()> {
    if let Some(image) = &config.image {
        ensure!(!image.trim().is_empty(), "image must not be empty");
    }
    Ok(())
}

pub fn validate_link(link: &IngressLink, app: &AppConfig) -> Result<()> {
    ensure!(
        !link.domains.is_empty() || link.optional,
        "domains are required (set them in the deployment overlay), or mark the entry `optional: true`"
    );
    for domain in &link.domains {
        ensure!(is_domain(domain), "`{domain}` is not a valid domain name");
    }

    ensure!(!link.routes.is_empty(), "at least one route is required");
    let mut names = HashSet::new();
    let mut paths = HashSet::new();
    for route in &link.routes {
        check_name("route name", &route.name)?;
        ensure!(
            names.insert(&route.name),
            "route `{}` is defined twice",
            route.name
        );
        ensure!(
            paths.insert(&route.path),
            "path `{}` is used by two routes",
            route.path
        );
        ensure!(
            app.app_service(&route.target).is_some(),
            "route `{}`: target `{}` is not in app_services",
            route.name,
            route.target
        );
        let path = &route.path;
        let valid_path = path.starts_with('/')
            && (path == "/" || !path.ends_with('/'))
            && path
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "/-_.~".contains(c));
        ensure!(
            valid_path,
            "route `{}`: path `{path}` must start with `/`, not end with `/`, and use only letters, digits and `/-_.~`",
            route.name
        );
        ensure!(
            !(route.strip_prefix && path == "/"),
            "route `{}`: strip_prefix has no effect on path `/`",
            route.name
        );
    }
    Ok(())
}

/// Letters, digits, `-` and `.`, optionally starting with `*.`.
fn is_domain(domain: &str) -> bool {
    let name = domain.strip_prefix("*.").unwrap_or(domain);
    !name.is_empty()
        && name.split('.').all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && label
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        })
}

/// The ingress of the server (there is at most one).
pub fn resolve<'s>(link: &IngressLink, available: &'s [SharedAppConfig]) -> Result<Resolution<'s>> {
    if link.domains.is_empty() {
        return Ok(Resolution::Skip(
            "optional and no domains given".to_string(),
        ));
    }
    match available.first() {
        Some(ingress) => Ok(Resolution::Use(ingress)),
        None => bail!(
            "no ingress is running on the server; define one in compost-server.yaml and run `compost server deploy --apply`"
        ),
    }
}

/// Routes with upstreams, longest path first so specific routes win.
pub fn routes(ctx: &AppCtx, link: &IngressLink) -> Result<Vec<RouteRecord>> {
    let mut routes = Vec::new();
    for route in &link.routes {
        let service = ctx.compose_service(&route.target)?;
        routes.push(RouteRecord {
            name: route.name.clone(),
            path: route.path.clone(),
            upstream: format!("{}:{}", ctx.service_alias(service), route.port),
            strip_prefix: route.strip_prefix,
        });
    }
    routes.sort_by(|a, b| b.path.len().cmp(&a.path.len()).then(a.path.cmp(&b.path)));
    Ok(routes)
}

/// Attach route targets to the ingress network and write the link record.
pub fn prepare_application(ctx: &AppCtx, link: &IngressLink, out: &mut Contribution) -> Result<()> {
    check_domain_conflicts(ctx, link)?;
    for route in &link.routes {
        let service = ctx.compose_service(&route.target)?;
        out.join_network(service, NETWORK, Some(ctx.service_alias(service)));
    }

    let record = IngressRecord {
        app: ctx.app.name.clone(),
        prefix: ctx.app.prefix.clone(),
        entry: link.name.clone(),
        domains: link.domains.clone(),
        routes: routes(ctx, link)?,
    };
    let text = serde_yaml_ng::to_string(&record)?;
    ctx.server
        .connector
        .write_file(&record_path(ctx, &link.name), text.as_bytes(), 0o644)
}

fn record_path(ctx: &AppCtx, entry: &str) -> String {
    let dir = connectors_dir(KIND, &ctx.shared.name);
    ctx.server
        .path(&format!("{dir}/{}.yaml", ctx.link_id(entry)))
}

pub fn read_record(ctx: &AppCtx, entry: &str) -> Result<Option<IngressRecord>> {
    let path = record_path(ctx, entry);
    match ctx.server.connector.read_file(&path)? {
        None => Ok(None),
        Some(text) => Ok(Some(
            serde_yaml_ng::from_str(&text).with_context(|| format!("invalid {path}"))?,
        )),
    }
}

/// e.g. `staging.example.com  /ui → myapp-staging-front-webapp:8080`
pub fn describe_link(ctx: &AppCtx, entry: &str) -> Result<String> {
    let Some(record) = read_record(ctx, entry)? else {
        return Ok("no link record".to_string());
    };
    let routes: Vec<String> = record
        .routes
        .iter()
        .map(|r| format!("{} → {}", r.path, r.upstream))
        .collect();
    Ok(format!(
        "{}  {}",
        record.domains.join(", "),
        routes.join(", ")
    ))
}

/// `<entry>.yaml`: the link record (domains and routes).
pub fn backup_link(ctx: &AppCtx, entry: &str, out: &mut ItemWriter) -> Result<()> {
    let record = ctx
        .server
        .connector
        .read_file(&record_path(ctx, entry))?
        .with_context(|| format!("no link record for `{entry}`; deploy the app again"))?;
    out.file(&format!("{entry}.yaml"), Role::Meta, record.as_bytes())
}

/// Delete the link record.
pub fn remove_application(ctx: &AppCtx, entry: &str) -> Result<()> {
    let path = shell_quote(&record_path(ctx, entry));
    ctx.server.connector.exec(&format!("rm -f -- {path}"))?;
    Ok(())
}

/// A domain may be served by only one link.
fn check_domain_conflicts(ctx: &AppCtx, link: &IngressLink) -> Result<()> {
    let dir = ctx.server.path(&connectors_dir(KIND, &ctx.shared.name));
    let own = format!("{}.yaml", ctx.link_id(&link.name));
    for file in ctx.server.connector.list_dir(&dir)? {
        if file == own || !file.ends_with(".yaml") {
            continue;
        }
        let path = format!("{dir}/{file}");
        let Some(text) = ctx.server.connector.read_file(&path)? else {
            continue;
        };
        let other: IngressRecord =
            serde_yaml_ng::from_str(&text).with_context(|| format!("invalid {path}"))?;
        if let Some(domain) = link.domains.iter().find(|d| other.domains.contains(d)) {
            bail!(
                "domain `{domain}` is already used by {}/{} ({})",
                other.app,
                other.prefix,
                other.entry
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> AppConfig {
        serde_yaml_ng::from_str("{name: a, app_services: [{name: web, service: web}]}").unwrap()
    }

    fn validate(text: &str) -> Result<()> {
        let link: IngressLink = serde_yaml_ng::from_str(text).unwrap();
        validate_link(&link, &app())
    }

    #[test]
    fn valid_link() {
        validate("{name: ui, domains: [a.example.com, '*.b.example'], routes: [{name: r, target: web, port: 80, path: /ui}]}")
            .unwrap();
    }

    #[test]
    fn domains_are_required_unless_optional() {
        let err = validate("{name: ui, routes: [{name: r, target: web, port: 80}]}").unwrap_err();
        assert!(err.to_string().contains("optional: true"), "{err}");
        validate("{name: ui, optional: true, routes: [{name: r, target: web, port: 80}]}").unwrap();
    }

    #[test]
    fn rejects_bad_routes_and_domains() {
        let bad = [
            (
                "{name: ui, domains: [A.com], routes: [{name: r, target: web, port: 80}]}",
                "valid domain",
            ),
            (
                "{name: ui, domains: [a.com], routes: []}",
                "at least one route",
            ),
            (
                "{name: ui, domains: [a.com], routes: [{name: r, target: x, port: 80}]}",
                "not in app_services",
            ),
            (
                "{name: ui, domains: [a.com], routes: [{name: r, target: web, port: 80, path: ui}]}",
                "must start",
            ),
            (
                "{name: ui, domains: [a.com], routes: [{name: r, target: web, port: 80, path: /ui/}]}",
                "must start",
            ),
            (
                "{name: ui, domains: [a.com], routes: [{name: r, target: web, port: 80}, {name: s, target: web, port: 81}]}",
                "used by two routes",
            ),
        ];
        for (text, expected) in bad {
            let err = validate(text).unwrap_err().to_string();
            assert!(err.contains(expected), "{text}: {err}");
        }
    }

    #[test]
    fn routes_are_ordered_longest_path_first() {
        let app = app();
        let connector = crate::connector::local::Local { verbose: false };
        let server = crate::shared::ServerCtx {
            connector: &connector,
            home: "/x".into(),
        };
        let shared: SharedAppConfig =
            serde_yaml_ng::from_str("{name: caddy-main, type: caddy}").unwrap();
        let ctx = AppCtx {
            server: &server,
            app: &app,
            shared: &shared,
        };
        let link: IngressLink = serde_yaml_ng::from_str(
            "{name: ui, domains: [a.com], routes: [{name: root, target: web, port: 80}, {name: api, target: web, port: 5000, path: /api/v1}, {name: ui, target: web, port: 80, path: /ui}]}",
        )
        .unwrap();
        let paths: Vec<_> = routes(&ctx, &link)
            .unwrap()
            .into_iter()
            .map(|r| r.path)
            .collect();
        assert_eq!(paths, ["/api/v1", "/ui", "/"]);
    }
}
