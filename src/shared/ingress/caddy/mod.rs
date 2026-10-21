//! Caddy ingress types (spec §5.3, §9.2): `caddy`, and `caddysecurebuild`,
//! Caddy built on the server with request filtering (see [`security`]).

pub mod crowdsec;
mod security;

use std::collections::HashSet;
use std::fmt::Write as _;

use anyhow::{Context, Result, bail, ensure};
use minijinja::context;
use serde::Deserialize;

use self::security::ServerSecurity;
use super::base::{self, IngressLink, RouteRecord};
use super::security::LinkSecurity;
use super::{IngressType, KIND};
use crate::config::check_name;
use crate::connector::shell_quote;
use crate::error::CompostError;
use crate::shared::{
    AppCtx, Contribution, Generated, ItemReader, ItemWriter, MetricsEndpoint, Monitored, Rendered,
    RenderedFile, Role, ServerCtx, SharedAppConfig, app_service, generate_password, is_domain,
    is_network_name, project_name, render_template, service_name, shared_dir,
};

const DEFAULT_VERSION: u32 = 2;

/// `secure`: the `caddysecurebuild` type, which filters requests.
pub struct Caddy {
    secure: bool,
}

pub static CADDY: Caddy = Caddy { secure: false };
pub static CADDY_SECURE: Caddy = Caddy { secure: true };

const SECURE_TYPE: &str = "caddysecurebuild";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct CaddyConfig {
    http_port: u16,
    https_port: u16,
    tls: Tls,
    acme_email: Option<String>,
    /// Existing docker networks Caddy joins besides compost-ingress, so
    /// `sites` can reach containers compost does not manage.
    networks: Vec<String>,
    /// Sites written by hand, e.g. for apps not moved to compost yet.
    sites: Vec<Site>,
    /// Log every request (to the container output, as JSON).
    access_log: bool,
    /// Request filtering, `caddysecurebuild` only.
    security: Option<ServerSecurity>,
}

impl Default for CaddyConfig {
    fn default() -> Self {
        Self {
            http_port: 80,
            https_port: 443,
            tls: Tls::Auto,
            acme_email: None,
            networks: Vec::new(),
            sites: Vec::new(),
            access_log: false,
            security: None,
        }
    }
}

/// An entry of `configuration.sites`: Caddyfile directives served for its domains.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Site {
    name: String,
    domains: Vec<String>,
    /// The body of the site block, e.g. `reverse_proxy shop-web-1:80`.
    config: String,
    /// Request filtering, `caddysecurebuild` only.
    #[serde(default)]
    security: Option<LinkSecurity>,
}

/// Port of the metrics endpoint inside the container, served with `monitoring`.
const METRICS_PORT: u16 = 9180;

/// Folder of the site files of `sites`, owned by `server deploy`.
const CUSTOM_DIR: &str = "conf/custom";
/// Folder of the CrowdSec agent's configuration, owned by `server deploy`.
const CROWDSEC_DIR: &str = "conf/crowdsec";
/// Generated once: the key the bouncer in Caddy uses for the CrowdSec agent.
const CROWDSEC_SECRETS: &str = "secrets/crowdsec.env";
const API_KEY_VARIABLE: &str = "CROWDSEC_API_KEY";

#[derive(Debug, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Tls {
    Auto,
    Internal,
    Off,
}

impl Caddy {
    /// The site file of an ingress link.
    fn site_text(&self, ctx: &AppCtx, link: &IngressLink) -> Result<String> {
        let caddy: CaddyConfig = ctx.shared.configuration()?;
        let mut routes = base::routes(ctx, link)?;
        // The `/` route becomes the fallback for everything not matched before.
        let fallback = match routes.iter().position(|r| r.path == "/" && !r.exact) {
            Some(i) => handler_lines(&routes.remove(i)),
            None => vec!["respond 404".to_string()],
        };
        let routes: Vec<_> = routes
            .iter()
            .map(|r| {
                let paths = if r.exact {
                    r.path.clone()
                } else {
                    format!("{0} {0}/*", r.path)
                };
                context! { name => r.name.as_str(), paths, lines => handler_lines(r) }
            })
            .collect();
        let mut site_lines =
            self.site_lines(&caddy, link.security.as_ref(), &ctx.link_id(&link.name));
        site_lines.extend(response_lines(link));
        let scheme = if caddy.tls == Tls::Off { "http://" } else { "" };
        let addresses: Vec<String> = link
            .domains
            .iter()
            .map(|d| format!("{scheme}{d}"))
            .collect();
        render_template(
            include_str!("templates/site.caddy.j2"),
            context! {
                instance => ctx.app.instance(),
                entry => link.name.as_str(),
                addresses => addresses.join(", "),
                site_lines,
                routes,
                fallback,
            },
        )
    }

    /// The security of a `caddysecurebuild` ingress, with defaults; `None` for `caddy`.
    fn security(&self, caddy: &CaddyConfig) -> Option<ServerSecurity> {
        self.secure
            .then(|| caddy.security.clone().unwrap_or_default())
    }

    /// The lines that open a site block: its logs and filtering.
    fn site_lines(
        &self,
        caddy: &CaddyConfig,
        security: Option<&LinkSecurity>,
        site: &str,
    ) -> Vec<String> {
        match self.security(caddy) {
            Some(_) => security::site_lines(security, site),
            None => vec!["import compost_site".to_string()],
        }
    }

    /// Security a site or link asks for must be available on this ingress.
    fn check_link_security(
        &self,
        caddy: &CaddyConfig,
        security: Option<&LinkSecurity>,
    ) -> Result<()> {
        let Some(security) = security else {
            return Ok(());
        };
        match self.security(caddy) {
            Some(server) => server.check_link(security),
            None => bail!("`security` needs an ingress of type {SECURE_TYPE}, not caddy"),
        }
    }
}

impl IngressType for Caddy {
    fn type_name(&self) -> &'static str {
        if self.secure { SECURE_TYPE } else { "caddy" }
    }

    fn default_version(&self) -> Option<u32> {
        Some(DEFAULT_VERSION)
    }

    /// Its output, its metrics, served on an internal port, and traces of
    /// the requests it serves.
    fn monitored(&self, config: &SharedAppConfig) -> Result<Monitored> {
        Ok(Monitored {
            services: vec![app_service("caddy", service_name(&config.name))],
            metrics: vec![MetricsEndpoint {
                service: "caddy",
                port: METRICS_PORT,
                path: "/metrics",
            }],
            traces: true,
        })
    }

    fn generate(&self) -> Generated {
        if self.secure {
            return Generated {
                local: include_str!("templates/generate/secure-local.yaml"),
                prod: Some(include_str!("templates/generate/secure-prod.yaml")),
                secrets: None,
                notes: &[
                    "caddysecurebuild: point the domains of your apps to the server and set `acme_email` in compost-server.prod.yaml.",
                    "caddysecurebuild: the first deploy builds Caddy on the server, which takes a few minutes.",
                    "caddysecurebuild: add your own address to `whitelist`; list bans with `compost server cmd <name>.bans`, lift one with `compost server cmd <name>.unban <address>`.",
                ],
                requires: &[],
            };
        }
        Generated {
            local: include_str!("templates/generate/local.yaml"),
            prod: Some(include_str!("templates/generate/prod.yaml")),
            secrets: None,
            notes: &[
                "caddy: point the domains of your apps to the server and set `acme_email` in compost-server.prod.yaml.",
                "caddy: locally, apps answer on http://<domain>:18080/; `*.localhost` domains need no DNS.",
            ],
            requires: &[],
        }
    }

    /// Extends the generic ingress validation with Caddy's own settings.
    fn validate_config(&self, config: &SharedAppConfig) -> Result<()> {
        base::validate_config(config)?;

        if let Some(version) = config.version {
            ensure!(
                version == 2,
                "only caddy version 2 is supported, got {version}"
            );
        }
        let caddy: CaddyConfig = config.configuration()?;
        if self.secure {
            ensure!(
                config.image.is_none(),
                "image is not used: {SECURE_TYPE} builds its own image on the server"
            );
        } else {
            ensure!(
                caddy.security.is_none(),
                "security needs type {SECURE_TYPE}"
            );
        }
        if let Some(security) = &caddy.security {
            security.validate()?;
        }
        ensure!(
            caddy.http_port != caddy.https_port,
            "http_port and https_port must differ"
        );
        if let Some(email) = &caddy.acme_email {
            ensure!(
                email.contains('@')
                    && !email.contains(|c: char| c.is_whitespace() || "{}".contains(c)),
                "acme_email `{email}` is not an email address"
            );
            ensure!(
                caddy.tls == Tls::Auto,
                "acme_email is only used with `tls: auto`"
            );
        }

        let mut networks = HashSet::new();
        for network in &caddy.networks {
            ensure!(
                is_network_name(network),
                "networks: `{network}` is not a docker network name"
            );
            ensure!(
                network != base::NETWORK,
                "networks: {} is always joined; list only other networks",
                base::NETWORK
            );
            ensure!(
                networks.insert(network),
                "networks: `{network}` is listed twice"
            );
        }
        let mut names = HashSet::new();
        let mut domains = HashSet::new();
        for site in &caddy.sites {
            check_name("site name", &site.name)?;
            ensure!(
                names.insert(&site.name),
                "sites: `{}` is defined twice",
                site.name
            );
            ensure!(
                !site.domains.is_empty(),
                "sites[{}]: at least one domain is required",
                site.name
            );
            for domain in &site.domains {
                ensure!(
                    is_domain(domain),
                    "sites[{}]: `{domain}` is not a valid domain name",
                    site.name
                );
                ensure!(
                    domains.insert(domain),
                    "sites[{}]: domain `{domain}` is used by another site",
                    site.name
                );
            }
            ensure!(
                !site.config.trim().is_empty(),
                "sites[{}]: config is empty",
                site.name
            );
            if let Some(security) = &site.security {
                security
                    .validate()
                    .and_then(|()| self.check_link_security(&caddy, Some(security)))
                    .with_context(|| format!("sites[{}]", site.name))?;
            }
        }
        Ok(())
    }

    /// Extends the generic preparation: a domain of a site in
    /// compost-server.yaml cannot be used by an app as well.
    fn prepare_application(
        &self,
        ctx: &AppCtx,
        link: &IngressLink,
        out: &mut Contribution,
    ) -> Result<()> {
        let caddy: CaddyConfig = ctx.shared.configuration()?;
        self.check_link_security(&caddy, link.security.as_ref())?;
        for site in &caddy.sites {
            if let Some(domain) = link.domains.iter().find(|d| site.domains.contains(d)) {
                bail!(
                    "domain `{domain}` is served by site `{}` of {} in compost-server.yaml; remove it there and run `compost server deploy` first",
                    site.name,
                    ctx.shared.name
                );
            }
        }
        base::prepare_application(ctx, link, out)
    }

    fn render(&self, ctx: &ServerCtx, config: &SharedAppConfig) -> Result<Rendered> {
        let caddy: CaddyConfig = config.configuration()?;
        let version = config.version.unwrap_or(DEFAULT_VERSION);
        let security = self.security(&caddy);
        let dir = shared_dir(KIND, &config.name);
        let mut files = Vec::new();
        let mut dirs = vec![format!("{dir}/data"), format!("{dir}/config")];
        let mut owned_dirs = vec![format!("{dir}/{CUSTOM_DIR}")];

        let image = match &security {
            // Built on the server; a changed Dockerfile gets a new tag, which
            // `docker compose up` builds.
            Some(security) => {
                let dockerfile = security::dockerfile(version, security)?;
                let tag = format!(
                    "compost-{SECURE_TYPE}-{}:{}",
                    config.name,
                    &sha256_hex(&dockerfile)[..12]
                );
                files.push(RenderedFile::new(
                    format!("{dir}/build/Dockerfile"),
                    dockerfile,
                ));
                tag
            }
            None => config
                .image
                .clone()
                .unwrap_or_else(|| format!("caddy:{version}-alpine")),
        };

        let mut options = Vec::new();
        if let Some(email) = &caddy.acme_email {
            options.push(format!("email {email}"));
        }
        let metrics = config.monitoring.as_ref().is_some_and(|m| m.metrics);
        let tracing = config.monitoring.as_ref().is_some_and(|m| m.traces);
        if metrics {
            options.push("metrics".to_string());
        }
        match caddy.tls {
            Tls::Auto => {}
            Tls::Internal => options.push("local_certs".to_string()),
            Tls::Off => options.push("auto_https off".to_string()),
        }

        // Run as the deploy user, so certificates and state Caddy writes to
        // data/ and config/ belong to that user, not root. The image allows
        // the caddy binary to bind ports 80 and 443 without root.
        let ids = ctx.connector.exec("id -u && id -g")?;
        let user = ids.split_whitespace().collect::<Vec<_>>().join(":");
        ensure!(
            user.split(':').count() == 2,
            "unexpected output of `id`: {ids}"
        );

        let mut networks = vec![base::NETWORK.to_string()];
        networks.extend(caddy.networks.iter().cloned());
        let mut service_networks = networks.clone();
        let mut snippets = None;
        let mut log_lines = Vec::new();
        let mut crowdsec = None;
        if let Some(security) = &security {
            check_link_security_in_use(ctx, config, security)?;
            let host = crowdsec_service(&config.name);
            options.extend(security.options(&host));
            snippets = Some(security.snippets()?.trim_end().to_string());
            log_lines = security.log_lines();
            files.push(RenderedFile::secret(
                format!("{dir}/{CROWDSEC_SECRETS}"),
                crowdsec_secrets(&crowdsec_key(ctx, &config.name)?),
            ));
            let mut hashed = String::new();
            for (name, contents) in security.crowdsec_files()? {
                hashed.push_str(&contents);
                files.push(RenderedFile::new(
                    format!("{dir}/{CROWDSEC_DIR}/{name}"),
                    contents,
                ));
            }
            service_networks.push("crowdsec".to_string());
            dirs.push(format!("{dir}/logs"));
            owned_dirs.push(format!("{dir}/{CROWDSEC_DIR}"));
            crowdsec = Some(context! {
                service => host,
                image => security::CROWDSEC_IMAGE,
                collections => security.collections(),
                scenarios => security.crowdsec.scenarios.clone(),
                whitelist => !security.crowdsec.whitelist.is_empty(),
                config_hash => sha256_hex(&hashed)[..12].to_string(),
            });
        } else {
            check_no_link_security(ctx, config)?;
        }

        let compose = render_template(
            include_str!("templates/compose.yaml.j2"),
            context! {
                image,
                service => service_name(&config.name),
                container => project_name(KIND, &config.name),
                user,
                http_port => caddy.http_port,
                https_port => caddy.https_port,
                publish_https => caddy.tls != Tls::Off,
                networks,
                service_networks,
                crowdsec,
            },
        )?;
        let caddyfile = render_template(
            include_str!("templates/Caddyfile.j2"),
            context! {
                options,
                security => snippets,
                log_lines,
                custom_sites => !caddy.sites.is_empty(),
                access_log => caddy.access_log,
                tracing,
                metrics_port => metrics.then_some(METRICS_PORT),
            },
        )?;

        files.extend([
            RenderedFile::new(format!("{dir}/compose.yaml"), compose),
            RenderedFile::new(format!("{dir}/conf/Caddyfile"), caddyfile),
            RenderedFile::new(
                format!("{dir}/conf/sites/00-default.caddy"),
                include_str!("templates/default-site.caddy").to_string(),
            ),
        ]);
        if !caddy.sites.is_empty() {
            check_site_conflicts(ctx, config, &caddy.sites)?;
        }
        for site in &caddy.sites {
            files.push(RenderedFile::new(
                format!("{dir}/{CUSTOM_DIR}/{}.caddy", site.name),
                self.render_site(config, &caddy, site)?,
            ));
        }
        Ok(Rendered {
            files,
            dirs,
            existing_networks: caddy.networks,
            owned_dirs,
            ..Default::default()
        })
    }

    fn reloads(&self) -> bool {
        true
    }

    fn state_lines(&self, ctx: &ServerCtx, config: &SharedAppConfig) -> Vec<String> {
        if self.secure {
            crowdsec::state_lines(ctx, &config.name)
        } else {
            Vec::new()
        }
    }

    /// Check the new files in a throwaway container of the new image, with
    /// the service's mounts and environment: the running Caddy keeps its
    /// configuration until the check passes. A built image is built first.
    fn check_before_start(&self, ctx: &ServerCtx, config: &SharedAppConfig) -> Result<()> {
        let dir = shell_quote(&ctx.path(&shared_dir(KIND, &config.name)));
        let project = shell_quote(&project_name(KIND, &config.name));
        let service = shell_quote(&service_name(&config.name));
        if self.secure {
            ctx.connector
                .exec(&format!("cd {dir} && docker compose -p {project} build -q"))
                .context("could not build Caddy; the running Caddy is left as it is")?;
        }
        ctx.connector
            .exec(&format!(
                "cd {dir} && docker compose -p {project} run --rm --no-deps -T --entrypoint caddy {service} validate --config /etc/caddy/Caddyfile --adapter caddyfile"
            ))
            .context("Caddy rejected the new configuration; the running Caddy keeps the previous one until it restarts")?;
        Ok(())
    }

    /// Validate the configuration files, then reload them without a restart.
    fn reload_shared(&self, ctx: &ServerCtx, config: &SharedAppConfig) -> Result<bool> {
        let dir = shell_quote(&ctx.path(&shared_dir(KIND, &config.name)));
        let project = shell_quote(&project_name(KIND, &config.name));
        let service = shell_quote(&service_name(&config.name));
        let running = ctx.connector.exec(&format!(
            "cd {dir} && docker compose -p {project} ps --status running -q {service}"
        ))?;
        if running.trim().is_empty() {
            return Ok(false);
        }
        let caddy = |command: &str| {
            format!(
                "cd {dir} && docker compose -p {project} exec -T {service} caddy {command} --config /etc/caddy/Caddyfile --adapter caddyfile"
            )
        };
        ctx.connector.exec(&caddy("validate")).context(
            "Caddy rejected the configuration; it keeps running with the previous one until it restarts",
        )?;
        ctx.connector
            .exec(&caddy("reload"))
            .context("Caddy could not reload its configuration")?;
        Ok(true)
    }

    fn preview_application(&self, ctx: &AppCtx, link: &IngressLink) -> Result<Vec<RenderedFile>> {
        let site = self.site_text(ctx, link)?;
        let path = site_path(ctx, &link.name);
        let relative = path
            .strip_prefix(&format!("{}/", ctx.server.home))
            .unwrap_or(&path)
            .to_string();
        Ok(vec![RenderedFile::new(relative, site)])
    }

    /// Write the site file, let Caddy validate it, then reload without downtime.
    fn after_apply_application(&self, ctx: &AppCtx, link: &IngressLink) -> Result<()> {
        let site = self.site_text(ctx, link)?;
        let connector = ctx.server.connector;
        let site_path = site_path(ctx, &link.name);
        let previous = connector.read_file(&site_path)?;
        connector.write_file(&site_path, site.as_bytes(), 0o644)?;

        if let Err(err) = connector.exec(&caddy_command(ctx, "validate")) {
            match previous {
                Some(previous) => connector.write_file(&site_path, previous.as_bytes(), 0o644)?,
                None => {
                    connector.exec(&format!("rm -f -- {}", shell_quote(&site_path)))?;
                }
            }
            return Err(err.context("Caddy rejected the generated site configuration"));
        }
        connector
            .exec(&caddy_command(ctx, "reload"))
            .context("Caddy could not reload its configuration")?;
        Ok(())
    }

    /// Extends the generic backup with the site file Caddy serves.
    fn backup_link(&self, ctx: &AppCtx, entry: &str, out: &mut ItemWriter) -> Result<()> {
        base::backup_link(ctx, entry, out)?;
        let site = shell_quote(&site_path(ctx, entry));
        out.stream(
            &format!("{entry}.caddy"),
            Role::Data,
            &format!("cat -- {site}"),
        )
    }

    /// Certificates, Caddy state and configuration. Keeping the certificates
    /// avoids hitting ACME rate limits when moving to another server.
    fn backup_shared(
        &self,
        ctx: &ServerCtx,
        config: &SharedAppConfig,
        out: &mut ItemWriter,
    ) -> Result<()> {
        let dir = shell_quote(&ctx.path(&shared_dir(KIND, &config.name)));
        out.stream(
            BACKUP_ITEM,
            Role::Data,
            &format!("cd {dir} && tar -cf - {}", BACKUP_DIRS.join(" ")),
        )
    }

    /// Unpack next to the folders first, then swap them in.
    fn restore_shared_files(
        &self,
        ctx: &ServerCtx,
        config: &SharedAppConfig,
        from: &ItemReader,
    ) -> Result<()> {
        if !from.has(BACKUP_ITEM) {
            return Ok(());
        }
        let dir = shell_quote(&ctx.path(&shared_dir(KIND, &config.name)));
        let mut script = format!(
            "mkdir -p -- {dir} && cd {dir} && rm -rf .restore && mkdir .restore && tar -xf - -C .restore"
        );
        for name in RESTORE_DIRS {
            let _ = write!(
                script,
                " && if [ -d .restore/{name} ]; then rm -rf {name} && mv .restore/{name} {name}; fi"
            );
        }
        script.push_str(" && rm -rf .restore");
        from.feed(BACKUP_ITEM, b"", &script)
    }

    /// Extends the generic removal: also delete the site file and reload.
    fn remove_application(&self, ctx: &AppCtx, entry: &str) -> Result<()> {
        base::remove_application(ctx, entry)?;
        let connector = ctx.server.connector;
        connector.exec(&format!("rm -f -- {}", shell_quote(&site_path(ctx, entry))))?;
        connector
            .exec(&caddy_command(ctx, "reload"))
            .context("Caddy could not reload its configuration")?;
        Ok(())
    }
}

/// Folders of the Caddy folder in `caddy.tar`. `conf/` is saved for review
/// only: deploys write it again.
const BACKUP_DIRS: [&str; 3] = ["data", "config", "conf"];
/// Folders put back on restore: certificates and Caddy's own state.
const RESTORE_DIRS: [&str; 2] = ["data", "config"];
const BACKUP_ITEM: &str = "caddy.tar";

impl Caddy {
    /// The site block of an entry of `sites`, with its config indented.
    fn render_site(
        &self,
        config: &SharedAppConfig,
        caddy: &CaddyConfig,
        site: &Site,
    ) -> Result<String> {
        let scheme = if caddy.tls == Tls::Off { "http://" } else { "" };
        let addresses: Vec<String> = site
            .domains
            .iter()
            .map(|d| format!("{scheme}{d}"))
            .collect();
        let lines: Vec<String> = site.config.trim_end().lines().map(str::to_string).collect();
        render_template(
            include_str!("templates/custom-site.caddy.j2"),
            context! {
                shared => config.name.as_str(),
                addresses => addresses.join(", "),
                site_lines => self.site_lines(caddy, site.security.as_ref(), &format!("site-{}", site.name)),
                lines,
            },
        )
    }
}

/// The inside of a route's `handle` block.
fn handler_lines(route: &RouteRecord) -> Vec<String> {
    if let Some(redirect) = &route.redirect {
        // `*`: a first argument starting with `/` would be a path matcher.
        return vec![format!("redir * {} {}", redirect.to, redirect.status)];
    }
    let mut lines = Vec::new();
    if let Some(strip) = &route.strip_path {
        lines.push(format!("uri strip_prefix {strip}"));
    } else if route.strip_prefix {
        lines.push(format!("uri strip_prefix {}", route.path));
    }
    if route.header_up.is_empty() {
        lines.push(format!("reverse_proxy {}", route.upstream));
    } else {
        lines.push(format!("reverse_proxy {} {{", route.upstream));
        for (name, value) in &route.header_up {
            lines.push(format!("\theader_up {name} \"{value}\""));
        }
        lines.push("}".to_string());
    }
    lines
}

/// Site directives of an ingress link: the request body limit and response headers.
fn response_lines(link: &IngressLink) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(size) = &link.max_body_size {
        lines.extend([
            "request_body {".to_string(),
            format!("\tmax_size {size}"),
            "}".to_string(),
        ]);
    }
    let headers = &link.headers;
    if !headers.is_empty() {
        lines.push("header {".to_string());
        lines.extend(headers.remove.iter().map(|name| format!("\t-{name}")));
        lines.extend(
            headers
                .set
                .iter()
                .map(|(name, value)| format!("\t{name} \"{value}\"")),
        );
        lines.push("}".to_string());
    }
    lines
}

/// Compose service of the CrowdSec agent; unique, since Caddy also sits on
/// the ingress network.
fn crowdsec_service(name: &str) -> String {
    format!("{}-crowdsec", service_name(name))
}

/// The bouncer key: generated on the first deploy, then kept, since the
/// agent registers it only once.
fn crowdsec_key(ctx: &ServerCtx, name: &str) -> Result<String> {
    let path = format!("{}/{CROWDSEC_SECRETS}", shared_dir(KIND, name));
    let existing = ctx.read(&path)?.and_then(|text| {
        text.lines()
            .find_map(|line| line.strip_prefix(&format!("{API_KEY_VARIABLE}=")))
            .map(str::to_string)
    });
    Ok(existing.unwrap_or_else(generate_password))
}

/// Read by both containers: Caddy's bouncer and the agent, which registers it.
fn crowdsec_secrets(key: &str) -> String {
    format!("{API_KEY_VARIABLE}={key}\nBOUNCER_KEY_caddy={key}\n")
}

fn sha256_hex(text: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Apps deployed with `security` must find what they use: removing a
/// rate-limit zone would make Caddy reject their sites on reload.
fn check_link_security_in_use(
    ctx: &ServerCtx,
    config: &SharedAppConfig,
    security: &ServerSecurity,
) -> Result<()> {
    for record in base::records(ctx, &config.name)? {
        if let Some(link) = &record.security {
            security.check_link(link).map_err(|e| {
                CompostError::ServerNotReady(format!(
                    "{} is deployed with this security: {e}; deploy it without that first",
                    record.owner()
                ))
            })?;
        }
    }
    Ok(())
}

/// Plain Caddy has none of the snippets that sites with `security` import.
fn check_no_link_security(ctx: &ServerCtx, config: &SharedAppConfig) -> Result<()> {
    for record in base::records(ctx, &config.name)? {
        if record.security.is_some() {
            return Err(CompostError::ServerNotReady(format!(
                "{} is deployed with `security`, which needs type {SECURE_TYPE}; deploy it without `security` first",
                record.owner()
            ))
            .into());
        }
    }
    Ok(())
}

/// A domain of `sites` must not be served by an app already.
fn check_site_conflicts(ctx: &ServerCtx, config: &SharedAppConfig, sites: &[Site]) -> Result<()> {
    for (domain, owner) in base::domains_in_use(ctx, &config.name)? {
        if let Some(site) = sites.iter().find(|s| s.domains.contains(&domain)) {
            return Err(CompostError::ServerNotReady(format!(
                "sites[{}]: domain `{domain}` is already served by {owner}",
                site.name
            ))
            .into());
        }
    }
    Ok(())
}

/// `conf/sites/<instance>-<entry>.caddy` in the Caddy folder.
fn site_path(ctx: &AppCtx, entry: &str) -> String {
    let dir = shared_dir(KIND, &ctx.shared.name);
    ctx.server
        .path(&format!("{dir}/conf/sites/{}.caddy", ctx.link_id(entry)))
}

/// Run `caddy <command>` on the main Caddyfile inside the running container.
fn caddy_command(ctx: &AppCtx, command: &str) -> String {
    format!(
        "cd {} && docker compose -p {} exec -T {} caddy {command} --config /etc/caddy/Caddyfile --adapter caddyfile",
        shell_quote(&ctx.server.path(&shared_dir(KIND, &ctx.shared.name))),
        shell_quote(&project_name(KIND, &ctx.shared.name)),
        shell_quote(&service_name(&ctx.shared.name))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caddy_type(config: &SharedAppConfig) -> &'static Caddy {
        if config.type_name == SECURE_TYPE {
            &CADDY_SECURE
        } else {
            &CADDY
        }
    }

    fn validate(text: &str) -> Result<()> {
        let config: SharedAppConfig = serde_yaml_ng::from_str(text).unwrap();
        caddy_type(&config).validate_config(&config)
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
    fn checks_networks_and_sites() {
        validate(
            "{name: c, type: caddy, configuration: {networks: [shop_default], sites: [{name: shop, domains: [shop.example.com], config: 'reverse_proxy web:80'}]}}",
        )
        .unwrap();
        for (configuration, expected) in [
            ("{networks: ['-x']}", "not a docker network name"),
            ("{networks: [compost-ingress]}", "always joined"),
            ("{networks: [a, a]}", "listed twice"),
            (
                "{sites: [{name: Shop, domains: [a.com], config: x}]}",
                "invalid site name",
            ),
            (
                "{sites: [{name: a, domains: [], config: x}]}",
                "at least one domain",
            ),
            (
                "{sites: [{name: a, domains: [a.com], config: x}, {name: b, domains: [a.com], config: x}]}",
                "used by another site",
            ),
            (
                "{sites: [{name: a, domains: [a.com], config: x}, {name: a, domains: [b.com], config: x}]}",
                "defined twice",
            ),
            (
                "{sites: [{name: a, domains: [a.com], config: ' '}]}",
                "config is empty",
            ),
        ] {
            let err = validate(&format!(
                "{{name: c, type: caddy, configuration: {configuration}}}"
            ))
            .unwrap_err()
            .to_string();
            assert!(err.contains(expected), "{configuration}: {err}");
        }
    }

    #[test]
    fn rejects_unknown_configuration_keys() {
        let err = validate("{name: c, type: caddy, configuration: {ports: [80]}}").unwrap_err();
        assert!(err.to_string().contains("unknown field `ports`"), "{err}");
    }

    fn render(text: &str) -> Rendered {
        let config: SharedAppConfig = serde_yaml_ng::from_str(text).unwrap();
        let connector = crate::connector::local::Local { verbose: false };
        let ctx = ServerCtx {
            connector: &connector,
            home: "/nonexistent".into(),
        };
        caddy_type(&config).render(&ctx, &config).unwrap()
    }

    fn file<'a>(rendered: &'a Rendered, suffix: &str) -> &'a str {
        &rendered
            .files
            .iter()
            .find(|f| f.path.ends_with(suffix))
            .unwrap()
            .contents
    }

    #[test]
    fn renders_defaults() {
        let rendered = render("{name: caddy-main, type: caddy}");
        let compose = file(&rendered, "compose.yaml");
        assert!(compose.contains("image: caddy:2-alpine"), "{compose}");
        assert!(
            compose.contains("\n  compost-shared-caddy-main:\n"),
            "{compose}"
        );
        let id = |flag| {
            let out = std::process::Command::new("id")
                .arg(flag)
                .output()
                .unwrap()
                .stdout;
            String::from_utf8(out).unwrap().trim().to_string()
        };
        let user = format!("user: \"{}:{}\"", id("-u"), id("-g"));
        assert!(
            compose.contains(&user),
            "runs as the deploy user: {compose}"
        );
        assert!(compose.contains(r#""443:443/udp""#), "{compose}");
        assert!(
            compose.contains("      - ./conf:/etc/caddy:ro\n      - ./data:/data\n"),
            "the folder, so reloads see replaced files: {compose}"
        );
        assert!(compose.contains("networks: [compost-ingress]"), "{compose}");
        assert!(
            compose.ends_with("external: true\n"),
            "keeps the final newline: {compose:?}"
        );

        let caddyfile = file(&rendered, "conf/Caddyfile");
        assert!(
            caddyfile.contains("\n\n# Imported by every site.\n"),
            "no global options block: {caddyfile}"
        );
        assert!(caddyfile.contains("import /etc/caddy/sites/*.caddy"));
        assert_eq!(
            rendered.dirs,
            [
                "shared/ingress/caddy-main/data",
                "shared/ingress/caddy-main/config"
            ]
        );
    }

    #[test]
    fn renders_networks_and_sites() {
        let rendered = render(
            "{name: c, type: caddy, configuration: {tls: off, networks: [shop_default], sites: [{name: shop, domains: [shop.example.com, www.shop.example.com], config: \"reverse_proxy shop-web-1:80\\n\\nheader {\\n  -Server\\n}\\n\"}]}}",
        );
        let compose = file(&rendered, "compose.yaml");
        assert!(
            compose.contains("networks: [compost-ingress, shop_default]"),
            "{compose}"
        );
        assert!(
            compose.contains("  shop_default:\n    external: true\n"),
            "{compose}"
        );
        assert_eq!(rendered.existing_networks, ["shop_default"]);
        assert_eq!(rendered.owned_dirs, ["shared/ingress/c/conf/custom"]);
        assert!(file(&rendered, "conf/Caddyfile").contains("import /etc/caddy/custom/*.caddy"));
        let site = file(&rendered, "conf/custom/shop.caddy");
        assert!(
            site.ends_with(
                "http://shop.example.com, http://www.shop.example.com {\n\timport compost_site\n\treverse_proxy shop-web-1:80\n\n\theader {\n\t  -Server\n\t}\n}\n"
            ),
            "{site:?}"
        );

        let rendered = render("{name: c, type: caddy}");
        assert!(!file(&rendered, "conf/Caddyfile").contains("custom"));
        assert!(rendered.existing_networks.is_empty());
    }

    #[test]
    fn renders_access_log_and_metrics() {
        let caddyfile = |text: &str| file(&render(text), "conf/Caddyfile").to_string();
        let plain = caddyfile("{name: c, type: caddy}");
        assert!(plain.contains("(compost_site) {\n}\n"), "{plain}");
        assert!(!plain.contains("metrics"), "{plain}");

        let logged = caddyfile("{name: c, type: caddy, configuration: {access_log: true}}");
        assert!(logged.contains("(compost_site) {\n\tlog\n}\n"), "{logged}");

        let monitored = caddyfile("{name: c, type: caddy, monitoring: {}}");
        assert!(monitored.starts_with("# Generated"), "{monitored}");
        assert!(monitored.contains("{\n\tmetrics\n}\n"), "{monitored}");
        assert!(
            monitored.contains("http://:9180 {\n\tmetrics\n}\n"),
            "{monitored}"
        );
        let no_metrics = caddyfile("{name: c, type: caddy, monitoring: {metrics: false}}");
        assert!(!no_metrics.contains("metrics"), "{no_metrics}");
        assert!(!monitored.contains("tracing"), "{monitored}");
    }

    #[test]
    fn renders_redirects_headers_and_body_size() {
        let app: crate::config::app::AppConfig = serde_yaml_ng::from_str(
            "{name: esr, prefix: dev, app_services: [{name: api, service: api}, {name: web, service: web}]}",
        )
        .unwrap();
        let connector = crate::connector::local::Local { verbose: false };
        let server = ServerCtx {
            connector: &connector,
            home: "/nonexistent".into(),
        };
        let shared: SharedAppConfig =
            serde_yaml_ng::from_str("{name: caddy-main, type: caddy}").unwrap();
        let ctx = AppCtx {
            server: &server,
            app: &app,
            shared: &shared,
            peers: &[],
        };
        let site = |text: &str| {
            let link: IngressLink = serde_yaml_ng::from_str(text).unwrap();
            CADDY.site_text(&ctx, &link).unwrap()
        };

        let text = site(
            "{name: esr, domains: [dev1.example.eu], max_body_size: 20MB, headers: {remove: [Server], set: {X-Frame-Options: DENY}}, routes: [
              {name: home, path: /, exact: true, redirect: {to: /esr-web/}},
              {name: api, target: api, port: 5000, path: /esr-backend, strip_prefix: true, header_up: {X-Real-IP: '{remote_host}'}},
              {name: web, target: web, port: 80, path: /esr-web}]}",
        );
        assert!(
            text.contains(concat!(
                "\timport compost_site\n",
                "\trequest_body {\n\t\tmax_size 20MB\n\t}\n",
                "\theader {\n\t\t-Server\n\t\tX-Frame-Options \"DENY\"\n\t}\n",
                "\t@api path /esr-backend /esr-backend/*\n",
                "\thandle @api {\n\t\turi strip_prefix /esr-backend\n",
                "\t\treverse_proxy esr-dev-api:5000 {\n\t\t\theader_up X-Real-IP \"{remote_host}\"\n\t\t}\n\t}\n",
                "\t@web path /esr-web /esr-web/*\n",
                "\thandle @web {\n\t\treverse_proxy esr-dev-web:80\n\t}\n",
                "\t@home path /\n",
                "\thandle @home {\n\t\tredir * /esr-web/ 302\n\t}\n",
                "\thandle {\n\t\trespond 404\n\t}\n",
            )),
            "{text}"
        );

        // A redirect on `/` catches every path no other route matches.
        let text = site(
            "{name: esr, domains: [a.example.eu], routes: [{name: web, target: web, port: 80, path: /web}, {name: home, redirect: {to: /web/, status: 301}}]}",
        );
        assert!(
            text.contains("\thandle {\n\t\tredir * /web/ 301\n\t}\n"),
            "{text}"
        );
        assert!(
            !text.contains("request_body") && !text.contains("header"),
            "{text}"
        );
    }

    #[test]
    fn renders_tracing_for_every_site() {
        let caddyfile = |text: &str| file(&render(text), "conf/Caddyfile").to_string();
        let traced = caddyfile("{name: c, type: caddy, monitoring: {traces: true}}");
        assert!(
            traced.contains(
                "(compost_tracing) {\n\ttracing {\n\t\tspan {http.request.host}\n\t}\n}\n"
            ),
            "{traced}"
        );
        assert!(
            traced.contains("(compost_site) {\n\timport compost_tracing\n}\n"),
            "{traced}"
        );
        // Sites with their own security import compost_log, not compost_site.
        let secure = caddyfile("{name: c, type: caddysecurebuild, monitoring: {traces: true}}");
        assert!(
            secure.contains("(compost_log) {\n\timport compost_tracing\n\tlog crowdsec {\n"),
            "{secure}"
        );
    }

    #[test]
    fn renders_tls_options_and_ports() {
        let rendered = render("{name: c, type: caddy, configuration: {http_port: 8080, tls: off}}");
        let compose = file(&rendered, "compose.yaml");
        assert!(compose.contains(r#""8080:80""#), "{compose}");
        assert!(
            !compose.contains("443"),
            "https is not published: {compose}"
        );
        assert!(file(&rendered, "conf/Caddyfile").contains("{\n\tauto_https off\n}"));

        let rendered = render("{name: c, type: caddy, configuration: {acme_email: a@b.c}}");
        assert!(file(&rendered, "conf/Caddyfile").contains("\temail a@b.c\n"));
    }

    const SECURE: &str = "{name: caddy-main, type: caddysecurebuild, version: 2, configuration: {security: {crowdsec: {whitelist: [203.0.113.7]}, rate_limits: [{name: login, paths: [/login], methods: [POST], events: 10, window: 1m}]}, sites: [{name: shop, domains: [shop.example.com], config: 'respond ok', security: {appsec: false}}]}}";

    #[test]
    fn security_needs_the_secure_type() {
        let err = validate("{name: c, type: caddy, configuration: {security: {}}}").unwrap_err();
        assert_eq!(err.to_string(), "security needs type caddysecurebuild");
        let err = validate(
            "{name: c, type: caddy, configuration: {sites: [{name: a, domains: [a.com], config: x, security: {}}]}}",
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("needs an ingress of type caddysecurebuild"),
            "{err:#}"
        );
        let err = validate("{name: c, type: caddysecurebuild, image: caddy:2}").unwrap_err();
        assert!(err.to_string().contains("builds its own image"), "{err}");
        validate("{name: c, type: caddysecurebuild}").unwrap();
        validate(SECURE).unwrap();

        let err = validate(
            "{name: c, type: caddysecurebuild, configuration: {sites: [{name: a, domains: [a.com], config: x, security: {rate_limits: [api]}}]}}",
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("sites[a]: security.rate_limits: `api`"),
            "{err:#}"
        );
        let err = validate(
            "{name: c, type: caddysecurebuild, configuration: {security: {crowdsec: {ban_duration: forever}}}}",
        )
        .unwrap_err();
        assert!(err.to_string().contains("ban_duration"), "{err}");
    }

    #[test]
    fn renders_the_secure_type() {
        let home = tempfile::tempdir().unwrap();
        let config: SharedAppConfig = serde_yaml_ng::from_str(SECURE).unwrap();
        let connector = crate::connector::local::Local { verbose: false };
        let ctx = ServerCtx {
            connector: &connector,
            home: home.path().to_string_lossy().into_owned(),
        };
        let rendered = CADDY_SECURE.render(&ctx, &config).unwrap();

        let dockerfile = file(&rendered, "build/Dockerfile");
        let tag = &sha256_hex(dockerfile)[..12];
        let compose = file(&rendered, "compose.yaml");
        assert!(
            compose.contains(&format!(
                "    build: ./build\n    image: compost-caddysecurebuild-caddy-main:{tag}\n"
            )),
            "{compose}"
        );
        assert!(
            compose.contains("    networks: [compost-ingress, crowdsec]\n"),
            "{compose}"
        );
        assert!(
            compose.contains(
                "\n  compost-shared-caddy-main-crowdsec:\n    image: crowdsecurity/crowdsec:"
            ),
            "{compose}"
        );
        assert!(compose.contains("    networks: [crowdsec]\n"), "{compose}");
        assert!(compose.contains("compost-whitelist.yaml:ro"), "{compose}");
        assert!(
            compose.contains("      - ./conf:/etc/caddy:ro\n"),
            "{compose}"
        );
        assert!(
            compose.contains("      compost.config-hash: \""),
            "{compose}"
        );
        assert!(
            compose.ends_with(
                "  crowdsec: {}\nvolumes:\n  crowdsec-data: {}\n  crowdsec-config: {}\n"
            ),
            "{compose}"
        );
        assert!(
            compose
                .contains("COLLECTIONS: \"crowdsecurity/caddy crowdsecurity/base-http-scenarios"),
            "{compose}"
        );

        let caddyfile = file(&rendered, "conf/Caddyfile");
        for expected in [
            "\torder crowdsec first\n\torder appsec after crowdsec\n\torder rate_limit after appsec\n",
            "\t\tapi_url http://compost-shared-caddy-main-crowdsec:8080\n",
            "\t\tappsec_url http://compost-shared-caddy-main-crowdsec:7422\n\t\tappsec_fail_open\n",
            "(compost_site) {\n\timport compost_log\n\timport compost_security {args[0]}\n}\n",
            "(compost_rate_limit_login) {\n",
            "(compost_security) {\n",
            "}\n\n# One file per application instance",
        ] {
            assert!(caddyfile.contains(expected), "{expected:?}\n{caddyfile}");
        }
        assert!(!caddyfile.contains("\tlog\n"), "no access_log: {caddyfile}");

        let site = file(&rendered, "conf/custom/shop.caddy");
        assert!(
            site.contains("shop.example.com {\n\timport compost_log\n\timport compost_default_bans\n\timport compost_default_rate_limits site-shop\n\trespond ok\n}\n"),
            "{site}"
        );

        let secrets = rendered
            .files
            .iter()
            .find(|f| f.path.ends_with("secrets/crowdsec.env"))
            .unwrap();
        assert!(secrets.is_secret());
        let key = secrets
            .contents
            .strip_prefix("CROWDSEC_API_KEY=")
            .and_then(|rest| rest.split_once('\n'))
            .map(|(key, _)| key.to_string())
            .unwrap();
        assert_eq!(
            secrets.contents,
            format!("CROWDSEC_API_KEY={key}\nBOUNCER_KEY_caddy={key}\n")
        );
        assert!(
            rendered
                .dirs
                .contains(&"shared/ingress/caddy-main/logs".to_string())
        );
        assert_eq!(
            rendered.owned_dirs,
            [
                "shared/ingress/caddy-main/conf/custom",
                "shared/ingress/caddy-main/conf/crowdsec"
            ]
        );
        let crowdsec: Vec<_> = rendered
            .files
            .iter()
            .filter_map(|f| {
                f.path
                    .strip_prefix("shared/ingress/caddy-main/conf/crowdsec/")
            })
            .collect();
        assert_eq!(
            crowdsec,
            [
                "acquis-caddy.yaml",
                "acquis-appsec.yaml",
                "ignore.yaml",
                "simulation.yaml",
                "profiles.yaml",
                "whitelist.yaml"
            ]
        );

        // The key is generated once, then kept.
        let path = home
            .path()
            .join("shared/ingress/caddy-main/secrets/crowdsec.env");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "CROWDSEC_API_KEY=kept\nBOUNCER_KEY_caddy=kept\n").unwrap();
        let again = CADDY_SECURE.render(&ctx, &config).unwrap();
        assert!(file(&again, "secrets/crowdsec.env").starts_with("CROWDSEC_API_KEY=kept\n"));
    }

    #[test]
    fn secure_access_log_goes_to_both_logs() {
        let caddyfile = file(
            &render("{name: c, type: caddysecurebuild, configuration: {access_log: true}}"),
            "conf/Caddyfile",
        )
        .to_string();
        assert!(
            caddyfile.contains("(compost_log) {\n\tlog\n\tlog crowdsec {\n"),
            "{caddyfile}"
        );
    }

    #[test]
    fn checks_the_security_of_deployed_links() {
        let home = tempfile::tempdir().unwrap();
        let records = home.path().join("connectors/ingress/c");
        std::fs::create_dir_all(&records).unwrap();
        std::fs::write(
            records.join("shop-prod-web.yaml"),
            "{app: shop, prefix: prod, entry: web, domains: [shop.example.com], routes: [], security: {rate_limits: [login]}}",
        )
        .unwrap();
        let connector = crate::connector::local::Local { verbose: false };
        let ctx = ServerCtx {
            connector: &connector,
            home: home.path().to_string_lossy().into_owned(),
        };
        let render = |text: &str| {
            let config: SharedAppConfig = serde_yaml_ng::from_str(text).unwrap();
            caddy_type(&config)
                .render(&ctx, &config)
                .map(|_| ())
                .map_err(|e| format!("{e:#}"))
        };
        render("{name: c, type: caddysecurebuild, configuration: {security: {rate_limits: [{name: login, events: 5, window: 1m}]}}}")
            .unwrap();
        let err = render("{name: c, type: caddysecurebuild}").unwrap_err();
        assert!(
            err.contains(
                "shop/prod (web) is deployed with this security: security.rate_limits: `login`"
            ),
            "{err}"
        );
        let err = render("{name: c, type: caddy}").unwrap_err();
        assert!(err.contains("needs type caddysecurebuild"), "{err}");

        // `drop` can't keep an app's `bans: false`.
        std::fs::write(
            records.join("blog-prod-web.yaml"),
            "{app: blog, prefix: prod, entry: web, domains: [blog.example.com], routes: [], security: {bans: false}}",
        )
        .unwrap();
        let zones = "rate_limits: [{name: login, events: 5, window: 1m}]";
        render(&format!(
            "{{name: c, type: caddysecurebuild, configuration: {{security: {{{zones}}}}}}}"
        ))
        .unwrap();
        let err = render(&format!("{{name: c, type: caddysecurebuild, configuration: {{security: {{crowdsec: {{ban_action: drop}}, {zones}}}}}}}"))
            .unwrap_err();
        assert!(
            err.contains("blog/prod (web) is deployed with this security: security.bans: false"),
            "{err}"
        );
    }

    #[test]
    fn drop_refuses_sites_without_bans() {
        let err = validate(
            "{name: c, type: caddysecurebuild, configuration: {security: {crowdsec: {ban_action: drop}}, sites: [{name: a, domains: [a.com], config: x, security: {bans: false}}]}}",
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("sites[a]: security.bans: false"),
            "{err:#}"
        );
        let rendered = render(
            "{name: c, type: caddysecurebuild, configuration: {security: {crowdsec: {ban_action: drop}}}}",
        );
        assert!(file(&rendered, "build/Dockerfile").contains("caddy-crowdsec-bouncer/layer4"));
        assert!(file(&rendered, "conf/Caddyfile").contains("\t\tlayer4 {\n"));
    }

    #[test]
    fn plain_sites_keep_importing_compost_site() {
        let rendered = render(
            "{name: c, type: caddy, configuration: {sites: [{name: shop, domains: [a.com], config: x}]}}",
        );
        assert!(
            file(&rendered, "conf/custom/shop.caddy")
                .ends_with("a.com {\n\timport compost_site\n\tx\n}\n")
        );
        assert!(!file(&rendered, "compose.yaml").contains("crowdsec"));
        assert!(!rendered.files.iter().any(|f| f.path.contains("build/")));
    }
}
