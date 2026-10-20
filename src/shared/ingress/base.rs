//! Generic ingress behavior, used by every ingress type unless it overrides it.

use std::collections::{BTreeMap, HashSet};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use super::KIND;
use super::security::LinkSecurity;
use crate::config::app::{AppConfig, link_name};
use crate::config::check_name;
use crate::connector::shell_quote;
use crate::shared::{
    self as shared, AppCtx, Contribution, ItemWriter, Resolution, Role, ServerCtx, SharedAppConfig,
    connectors_dir, is_domain,
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
    /// Request filtering; only ingress types that filter requests accept it.
    #[serde(default)]
    pub security: Option<LinkSecurity>,
    /// Largest request body accepted, e.g. `20MB`; larger requests get 413.
    #[serde(default)]
    pub max_body_size: Option<String>,
    /// Response headers of every route.
    #[serde(default)]
    pub headers: Headers,
}

/// Headers to change: `set` replaces a header (or adds it), `remove` deletes it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Headers {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub set: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
}

impl Headers {
    pub fn is_empty(&self) -> bool {
        self.set.is_empty() && self.remove.is_empty()
    }
}

/// A route that answers with a redirect instead of proxying.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Redirect {
    /// A path (`/web/`) or a URL (`https://example.com/`).
    pub to: String,
    #[serde(default = "redirect_status")]
    pub status: u16,
}

fn redirect_status() -> u16 {
    302
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub name: String,
    /// `app_services` name.
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    /// Another link of the app, `<kind>/<entry>`, instead of `target` and
    /// `port`; it decides the paths (e.g. `oidc/login`: its realm).
    #[serde(default)]
    pub link: Option<String>,
    #[serde(default = "root_path")]
    pub path: String,
    #[serde(default)]
    pub strip_prefix: bool,
    /// Match `path` only, not the paths below it.
    #[serde(default)]
    pub exact: bool,
    /// Answer with a redirect, instead of `target`/`port` or `link`.
    #[serde(default)]
    pub redirect: Option<Redirect>,
    /// Request headers set for the service, e.g. `X-Real-IP: "{remote_host}"`.
    #[serde(default)]
    pub header_up: BTreeMap<String, String>,
}

/// A route with its upstream resolved; stored in the link record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteRecord {
    pub name: String,
    pub path: String,
    /// `<alias>:<port>` on the ingress network; empty for a redirect.
    #[serde(default)]
    pub upstream: String,
    pub strip_prefix: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub exact: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect: Option<Redirect>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub header_up: BTreeMap<String, String>,
    /// Removed from the start of the request path before proxying, when that
    /// is only part of the route's path (e.g. `/auth` of `/auth/realms/myapp`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strip_path: Option<String>,
}

/// `connectors/ingress/<ingress>/<instance>-<entry>.yaml` (spec §8.3).
#[derive(Debug, Serialize, Deserialize)]
pub struct IngressRecord {
    pub app: String,
    pub prefix: String,
    pub entry: String,
    pub domains: Vec<String>,
    pub routes: Vec<RouteRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security: Option<LinkSecurity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_body_size: Option<String>,
    #[serde(default, skip_serializing_if = "Headers::is_empty")]
    pub headers: Headers,
}

impl IngressRecord {
    /// e.g. `myapp/staging (ui-front)`.
    pub fn owner(&self) -> String {
        format!("{}/{} ({})", self.app, self.prefix, self.entry)
    }
}

/// What a route points to.
enum Target<'a> {
    Service { target: &'a str, port: u16 },
    Link { kind: &'a str, entry: &'a str },
    Redirect(&'a Redirect),
}

impl Route {
    fn target(&self) -> Result<Target<'_>> {
        match (&self.target, self.port, &self.link, &self.redirect) {
            (Some(target), Some(port), None, None) => Ok(Target::Service { target, port }),
            (None, None, None, Some(redirect)) => Ok(Target::Redirect(redirect)),
            (None, None, Some(link), None) => {
                let Some((kind, entry)) = link.split_once('/') else {
                    bail!(
                        "route `{}`: link `{link}` must be `<kind>/<entry>`, e.g. oidc/login",
                        self.name
                    );
                };
                Ok(Target::Link { kind, entry })
            }
            _ => bail!(
                "route `{}`: give either `target` and `port`, `link`, or `redirect`",
                self.name
            ),
        }
    }
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
    if let Some(security) = &link.security {
        security.validate()?;
    }
    if let Some(size) = &link.max_body_size {
        ensure!(
            is_size(size),
            "max_body_size: `{size}` is not a size like 20MB, 512KB or 1GiB"
        );
    }
    check_headers("headers.set", &link.headers.set)?;
    for name in &link.headers.remove {
        ensure!(
            is_header_name(name),
            "headers.remove: `{name}` is not a header name"
        );
    }
    let mut names = HashSet::new();
    let mut paths = HashSet::new();
    for route in &link.routes {
        check_name("route name", &route.name)?;
        ensure!(
            names.insert(&route.name),
            "route `{}` is defined twice",
            route.name
        );
        check_headers(
            &format!("route `{}`: header_up", route.name),
            &route.header_up,
        )?;
        let (kind, entry) = match route.target()? {
            Target::Service { target, .. } => {
                ensure!(
                    app.app_service(target).is_some(),
                    "route `{}`: target `{target}` is not in app_services",
                    route.name
                );
                (None, None)
            }
            Target::Redirect(redirect) => {
                check_redirect(redirect).with_context(|| format!("route `{}`", route.name))?;
                ensure!(
                    !route.strip_prefix && route.header_up.is_empty(),
                    "route `{}`: `strip_prefix` and `header_up` are not used with `redirect`",
                    route.name
                );
                (None, None)
            }
            Target::Link { kind, entry } => (Some(kind), Some(entry)),
        };
        if let (Some(kind_name), Some(entry)) = (kind, entry) {
            // The linked entry decides the paths.
            ensure!(
                route.path == "/" && !route.strip_prefix && !route.exact,
                "route `{}`: `path`, `exact` and `strip_prefix` are not used with `link`",
                route.name
            );
            ensure!(
                route.header_up.is_empty(),
                "route `{}`: `header_up` is not used with `link`",
                route.name
            );
            let kind = shared::find_kind(kind_name).filter(|k| k.exposes_routes());
            ensure!(
                kind.is_some(),
                "route `{}`: {kind_name} links cannot be routed",
                route.name
            );
            let exists = app
                .services
                .get(kind_name)
                .is_some_and(|links| links.iter().any(|l| link_name(l) == Some(entry)));
            ensure!(
                exists,
                "route `{}`: there is no services.{kind_name}[{entry}]",
                route.name
            );
            continue;
        }
        let path = &route.path;
        ensure!(
            paths.insert((path, route.exact)),
            "path `{path}` is used by two routes"
        );
        ensure!(
            is_route_path(path),
            "route `{}`: path `{path}` must start with `/`, not end with `/`, and use only letters, digits and `/-_.~`",
            route.name
        );
        ensure!(
            !(route.strip_prefix && path == "/"),
            "route `{}`: strip_prefix has no effect on path `/`",
            route.name
        );
        ensure!(
            !(route.strip_prefix && route.exact),
            "route `{}`: strip_prefix is not used with `exact`",
            route.name
        );
    }
    Ok(())
}

/// A size Caddy reads, e.g. `20MB`, `512KB`, `1GiB` or `1000000`.
fn is_size(size: &str) -> bool {
    let digits = size.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let unit = &size[digits.len()..];
    !digits.is_empty()
        && digits.chars().all(|c| c.is_ascii_digit())
        && ["", "B", "KB", "MB", "GB", "KiB", "MiB", "GiB"].contains(&unit)
}

/// An HTTP header name (RFC 9110 token).
fn is_header_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c))
}

/// Header values go in quotes into the ingress config: no quotes,
/// backslashes or line breaks. Caddy placeholders like `{remote_host}` work.
fn is_header_value(value: &str) -> bool {
    !value
        .chars()
        .any(|c| c == '"' || c == '\\' || c.is_control())
}

fn check_headers(path: &str, headers: &BTreeMap<String, String>) -> Result<()> {
    for (name, value) in headers {
        ensure!(
            is_header_name(name),
            "{path}: `{name}` is not a header name"
        );
        ensure!(
            is_header_value(value),
            "{path}: the value of {name} must not contain quotes, backslashes or line breaks"
        );
    }
    Ok(())
}

fn check_redirect(redirect: &Redirect) -> Result<()> {
    let to = &redirect.to;
    ensure!(
        (to.starts_with('/') || to.starts_with("https://") || to.starts_with("http://"))
            && !to
                .chars()
                .any(|c| c.is_whitespace() || c == '"' || c == '\\'),
        "redirect: `{to}` must be a path starting with `/` or an http(s) URL, without spaces or quotes"
    );
    ensure!(
        [301, 302, 303, 307, 308].contains(&redirect.status),
        "redirect: status {} is not one of 301, 302, 303, 307, 308",
        redirect.status
    );
    Ok(())
}

/// The ingress of the server (there is at most one).
/// Starts with `/`, doesn't end with one (except `/` itself), and uses only
/// letters, digits and `/-_.~`.
pub fn is_route_path(path: &str) -> bool {
    path.starts_with('/')
        && (path == "/" || !path.ends_with('/'))
        && path
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/-_.~".contains(c))
}

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

/// Routes with upstreams, longest path first so specific routes win. Routes
/// to another link get their paths from that link.
pub fn routes(ctx: &AppCtx, link: &IngressLink) -> Result<Vec<RouteRecord>> {
    let mut routes: Vec<RouteRecord> = Vec::new();
    for route in &link.routes {
        match route.target()? {
            Target::Service { target, port } => {
                let service = ctx.compose_service(target)?;
                routes.push(RouteRecord {
                    name: route.name.clone(),
                    path: route.path.clone(),
                    upstream: format!("{}:{port}", ctx.service_alias(service)),
                    strip_prefix: route.strip_prefix,
                    exact: route.exact,
                    redirect: None,
                    header_up: route.header_up.clone(),
                    strip_path: None,
                });
            }
            Target::Redirect(redirect) => routes.push(RouteRecord {
                name: route.name.clone(),
                path: route.path.clone(),
                upstream: String::new(),
                strip_prefix: false,
                exact: route.exact,
                redirect: Some(redirect.clone()),
                header_up: BTreeMap::new(),
                strip_path: None,
            }),
            Target::Link { kind, entry } => {
                let Some((peer, peer_ctx)) = ctx.peer(kind, entry) else {
                    // A route to an optional link that was skipped is skipped too.
                    if is_optional_link(ctx.app, kind, entry) {
                        continue;
                    }
                    bail!(
                        "route `{}`: services.{kind}[{entry}] is not deployed with this app",
                        route.name
                    );
                };
                let exposed = peer
                    .kind
                    .exposed_routes(&peer_ctx, peer.entry)
                    .with_context(|| format!("route `{}`", route.name))?;
                if let Some(domain) = &exposed.domain {
                    ensure!(
                        link.domains.contains(domain),
                        "route `{}`: services.{kind}[{entry}] is served on `{domain}`, which is not in domains",
                        route.name
                    );
                }
                for exposed in exposed.routes {
                    routes.push(RouteRecord {
                        name: format!("{}-{}", route.name, exposed.name),
                        path: exposed.path,
                        upstream: exposed.upstream,
                        strip_prefix: false,
                        exact: false,
                        redirect: None,
                        header_up: BTreeMap::new(),
                        strip_path: exposed.strip_path,
                    });
                }
            }
        }
    }
    ensure!(
        !routes.is_empty(),
        "every route goes to a skipped optional link; mark this entry `optional: true` and leave out its domains where those links are skipped"
    );
    // Of the same path, the exact route goes first.
    routes.sort_by(|a, b| {
        b.path
            .len()
            .cmp(&a.path.len())
            .then(a.path.cmp(&b.path))
            .then(b.exact.cmp(&a.exact))
    });
    // The same path twice: fine when it goes to the same place, e.g. shared
    // static files of two realms.
    routes.dedup_by(|b, a| {
        a.path == b.path && a.exact == b.exact && a.redirect.is_none() && a.upstream == b.upstream
    });
    for pair in routes.windows(2) {
        ensure!(
            (&pair[0].path, pair[0].exact) != (&pair[1].path, pair[1].exact),
            "path `{}` is used by routes `{}` and `{}`",
            pair[0].path,
            pair[0].name,
            pair[1].name
        );
    }
    Ok(routes)
}

/// `services.<kind>[<entry>]` of the app has `optional: true`.
fn is_optional_link(app: &AppConfig, kind: &str, entry: &str) -> bool {
    app.services
        .get(kind)
        .into_iter()
        .flatten()
        .find(|link| link_name(link) == Some(entry))
        .and_then(|link| link.get("optional"))
        .and_then(serde_yaml_ng::Value::as_bool)
        .unwrap_or(false)
}

/// Attach route targets to the ingress network and write the link record.
pub fn prepare_application(ctx: &AppCtx, link: &IngressLink, out: &mut Contribution) -> Result<()> {
    check_domain_conflicts(ctx, link)?;
    for route in &link.routes {
        // Routes to another link go to a shared app, which joins the ingress network itself.
        if let Target::Service { target, .. } = route.target()? {
            let service = ctx.compose_service(target)?;
            out.join_network(service, NETWORK, Some(ctx.service_alias(service)));
        }
    }
    let routes = routes(ctx, link)?;

    let record = IngressRecord {
        app: ctx.app.name.clone(),
        prefix: ctx.app.prefix.clone(),
        entry: link.name.clone(),
        domains: link.domains.clone(),
        routes,
        security: link.security.clone(),
        max_body_size: link.max_body_size.clone(),
        headers: link.headers.clone(),
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
        .map(|r| match &r.redirect {
            Some(redirect) => format!("{} → redirect {} {}", r.path, redirect.to, redirect.status),
            None => format!("{} → {}", r.path, r.upstream),
        })
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

/// The link records of an ingress: the app links it serves.
pub fn records(ctx: &ServerCtx, ingress: &str) -> Result<Vec<IngressRecord>> {
    let dir = ctx.path(&connectors_dir(KIND, ingress));
    let mut records = Vec::new();
    for file in ctx.connector.list_dir(&dir)? {
        if !file.ends_with(".yaml") {
            continue;
        }
        let path = format!("{dir}/{file}");
        let Some(text) = ctx.connector.read_file(&path)? else {
            continue;
        };
        records.push(serde_yaml_ng::from_str(&text).with_context(|| format!("invalid {path}"))?);
    }
    Ok(records)
}

/// Domains of the link records of an ingress, each with the link serving it,
/// e.g. `myapp/staging (ui-front)`.
pub fn domains_in_use(ctx: &ServerCtx, ingress: &str) -> Result<Vec<(String, String)>> {
    let mut used = Vec::new();
    for record in records(ctx, ingress)? {
        let owner = record.owner();
        for domain in record.domains {
            used.push((domain, owner.clone()));
        }
    }
    Ok(used)
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
        if ctx.is_dropped(KIND, &other.app, &other.prefix, &other.entry) {
            continue;
        }
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
    fn redirects_headers_and_body_size() {
        validate(
            "{name: ui, domains: [a.com], max_body_size: 20MB, headers: {remove: [Server], set: {X-Frame-Options: DENY}}, routes: [
              {name: home, path: /, exact: true, redirect: {to: /web/}},
              {name: web, target: web, port: 80, header_up: {X-Real-IP: '{remote_host}'}},
              {name: old, path: /old, redirect: {to: 'https://b.example.com/', status: 301}}]}",
        )
        .unwrap();
        let bad = [
            ("max_body_size: 20 MB", "not a size"),
            ("max_body_size: 20XB", "not a size"),
            (
                "headers: {remove: ['X A']}",
                "headers.remove: `X A` is not a header name",
            ),
            ("headers: {set: {X-A: 'a\"b'}}", "must not contain quotes"),
            (
                "routes: [{name: r, redirect: {to: web}}]",
                "must be a path starting with `/` or an http(s) URL",
            ),
            (
                "routes: [{name: r, redirect: {to: /web, status: 200}}]",
                "status 200 is not one of",
            ),
            (
                "routes: [{name: r, target: web, port: 80, redirect: {to: /web}}]",
                "either `target` and `port`, `link`, or `redirect`",
            ),
            (
                "routes: [{name: r, path: /a, strip_prefix: true, redirect: {to: /web}}]",
                "not used with `redirect`",
            ),
            (
                "routes: [{name: r, target: web, port: 80, path: /a, exact: true, strip_prefix: true}]",
                "strip_prefix is not used with `exact`",
            ),
            (
                "routes: [{name: r, target: web, port: 80, header_up: {X-A: \"a\\\\b\"}}]",
                "route `r`: header_up: the value of X-A must not contain",
            ),
            (
                "routes: [{name: r, path: /, exact: true, redirect: {to: /web}}, {name: s, target: web, port: 80, exact: true}]",
                "path `/` is used by two routes",
            ),
        ];
        for (fields, expected) in bad {
            let text = if fields.starts_with("routes:") {
                format!("{{name: ui, domains: [a.com], {fields}}}")
            } else {
                format!(
                    "{{name: ui, domains: [a.com], {fields}, routes: [{{name: r, target: web, port: 80}}]}}"
                )
            };
            let err = format!("{:#}", validate(&text).unwrap_err());
            assert!(err.contains(expected), "{text}: {err}");
        }
    }

    #[test]
    fn routes_can_target_links() {
        let app: AppConfig = serde_yaml_ng::from_str(
            "{name: a, services: {oidc: [{name: login}], database: [{name: db}]}, app_services: [{name: web, service: web}]}",
        )
        .unwrap();
        let validate = |routes: &str| {
            let link: IngressLink = serde_yaml_ng::from_str(&format!(
                "{{name: l, domains: [a.com], routes: {routes}}}"
            ))
            .unwrap();
            validate_link(&link, &app).map_err(|e| e.to_string())
        };
        validate("[{name: r, link: oidc/login}, {name: s, target: web, port: 80}]").unwrap();
        for (routes, expected) in [
            (
                "[{name: r, link: oidc/login, port: 80}]",
                "either `target` and `port`, `link`, or `redirect`",
            ),
            (
                "[{name: r, target: web}]",
                "either `target` and `port`, `link`, or `redirect`",
            ),
            ("[{name: r, link: login}]", "must be `<kind>/<entry>`"),
            (
                "[{name: r, link: oidc/login, path: /x}]",
                "not used with `link`",
            ),
            (
                "[{name: r, link: oidc/login, exact: true}]",
                "not used with `link`",
            ),
            (
                "[{name: r, link: oidc/login, header_up: {X-A: b}}]",
                "`header_up` is not used with `link`",
            ),
            (
                "[{name: r, link: oidc/other}]",
                "there is no services.oidc[other]",
            ),
            (
                "[{name: r, link: database/db}]",
                "database links cannot be routed",
            ),
        ] {
            let err = validate(routes).unwrap_err();
            assert!(err.contains(expected), "{routes}: {err}");
        }
    }

    #[test]
    fn link_routes_expand_to_the_realm_of_an_oidc_link() {
        let app: AppConfig =
            serde_yaml_ng::from_str("{name: a, app_services: [{name: web, service: web}]}")
                .unwrap();
        let connector = crate::connector::local::Local { verbose: false };
        let server = crate::shared::ServerCtx {
            connector: &connector,
            home: "/x".into(),
        };
        let caddy: SharedAppConfig =
            serde_yaml_ng::from_str("{name: caddy-main, type: caddy}").unwrap();
        let keycloak: SharedAppConfig = serde_yaml_ng::from_str(
            "{name: kc, type: keycloak, configuration: {url: 'https://auth.example.com'}}",
        )
        .unwrap();
        let oidc = |name: &str, realm: &str| -> serde_yaml_ng::Value {
            serde_yaml_ng::from_str(&format!(
                "{{name: {name}, type: keycloak, realm: {realm}, public_url: 'https://login.example.com'}}"
            ))
            .unwrap()
        };
        let (login, other) = (oidc("login", "main"), oidc("other", "second"));
        let kind = shared::find_kind("oidc").unwrap();
        let peers = [
            shared::Peer {
                kind,
                name: "login",
                entry: &login,
                shared: &keycloak,
            },
            shared::Peer {
                kind,
                name: "other",
                entry: &other,
                shared: &keycloak,
            },
        ];
        let ctx = AppCtx {
            server: &server,
            app: &app,
            shared: &caddy,
            peers: &peers,
        };
        let routes_of = |text: &str| {
            let link: IngressLink = serde_yaml_ng::from_str(text).unwrap();
            routes(&ctx, &link).map_err(|e| format!("{e:#}"))
        };

        let routes = routes_of(
            "{name: l, domains: [login.example.com], routes: [{name: a, link: oidc/login}, {name: b, link: oidc/other}]}",
        )
        .unwrap();
        let paths: Vec<_> = routes
            .iter()
            .map(|r| (r.name.as_str(), r.path.as_str(), r.upstream.as_str()))
            .collect();
        // `/resources` of both links goes to the same place, so it is kept once.
        assert_eq!(
            paths,
            [
                ("b-realm", "/realms/second", "compost-shared-kc:8080"),
                ("a-realm", "/realms/main", "compost-shared-kc:8080"),
                ("a-resources", "/resources", "compost-shared-kc:8080"),
            ]
        );

        let err =
            routes_of("{name: l, domains: [x.example.com], routes: [{name: a, link: oidc/login}]}")
                .unwrap_err();
        assert!(
            err.contains("served on `login.example.com`, which is not in domains"),
            "{err}"
        );
        let err = routes_of(
            "{name: l, domains: [login.example.com], routes: [{name: a, link: oidc/login}, {name: s, target: web, port: 80, path: /resources}]}",
        )
        .unwrap_err();
        assert!(err.contains("path `/resources` is used by routes"), "{err}");
    }

    #[test]
    fn routes_to_a_skipped_optional_link_are_skipped() {
        let app: AppConfig = serde_yaml_ng::from_str(
            "{name: a, services: {oidc: [{name: login, optional: true}, {name: other}]}, app_services: [{name: web, service: web}]}",
        )
        .unwrap();
        let connector = crate::connector::local::Local { verbose: false };
        let server = crate::shared::ServerCtx {
            connector: &connector,
            home: "/x".into(),
        };
        let caddy: SharedAppConfig =
            serde_yaml_ng::from_str("{name: caddy-main, type: caddy}").unwrap();
        // Neither link resolved.
        let ctx = AppCtx {
            server: &server,
            app: &app,
            shared: &caddy,
            peers: &[],
        };
        let routes_of = |list: &str| {
            let link: IngressLink =
                serde_yaml_ng::from_str(&format!("{{name: l, domains: [a.com], routes: {list}}}"))
                    .unwrap();
            routes(&ctx, &link).map_err(|e| format!("{e:#}"))
        };
        let names: Vec<_> =
            routes_of("[{name: r, link: oidc/login}, {name: s, target: web, port: 80}]")
                .unwrap()
                .into_iter()
                .map(|r| r.name)
                .collect();
        assert_eq!(names, ["s"]);
        let err = routes_of("[{name: r, link: oidc/other}]").unwrap_err();
        assert!(
            err.contains("services.oidc[other] is not deployed"),
            "{err}"
        );
        let err = routes_of("[{name: r, link: oidc/login}]").unwrap_err();
        assert!(
            err.contains("every route goes to a skipped optional link"),
            "{err}"
        );
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
            peers: &[],
        };
        let link: IngressLink = serde_yaml_ng::from_str(
            "{name: ui, domains: [a.com], routes: [{name: root, target: web, port: 80}, {name: api, target: web, port: 5000, path: /api/v1}, {name: ui, target: web, port: 80, path: /ui}, {name: home, path: /, exact: true, redirect: {to: /ui/}}]}",
        )
        .unwrap();
        let paths: Vec<_> = routes(&ctx, &link)
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(paths, ["api", "ui", "home", "root"]);
    }
}
