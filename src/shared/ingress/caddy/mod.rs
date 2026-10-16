//! Caddy ingress type (spec §5.3, §9.2).

use std::collections::HashSet;

use anyhow::{Context, Result, bail, ensure};
use minijinja::context;
use serde::Deserialize;

use super::base::{self, IngressLink};
use super::{IngressType, KIND};
use crate::config::check_name;
use crate::connector::shell_quote;
use crate::error::CompostError;
use crate::shared::{
    AppCtx, Contribution, Generated, ItemReader, ItemWriter, Rendered, RenderedFile, Role,
    ServerCtx, SharedAppConfig, is_domain, is_network_name, project_name, render_template,
    shared_dir,
};

const DEFAULT_VERSION: u32 = 2;

pub struct Caddy;

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
}

/// Folder of the site files of `sites`, owned by `server deploy`.
const CUSTOM_DIR: &str = "conf/custom";

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

    fn default_version(&self) -> Option<u32> {
        Some(DEFAULT_VERSION)
    }

    fn generate(&self) -> Generated {
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
        let image = config
            .image
            .clone()
            .unwrap_or_else(|| format!("caddy:{version}-alpine"));

        let mut options = Vec::new();
        if let Some(email) = &caddy.acme_email {
            options.push(format!("email {email}"));
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
        let compose = render_template(
            include_str!("templates/compose.yaml.j2"),
            context! {
                image,
                user,
                http_port => caddy.http_port,
                https_port => caddy.https_port,
                publish_https => caddy.tls != Tls::Off,
                networks,
            },
        )?;
        let caddyfile = render_template(
            include_str!("templates/Caddyfile.j2"),
            context! { options, custom_sites => !caddy.sites.is_empty() },
        )?;

        let dir = shared_dir(KIND, &config.name);
        let mut files = vec![
            RenderedFile::new(format!("{dir}/compose.yaml"), compose),
            RenderedFile::new(format!("{dir}/conf/Caddyfile"), caddyfile),
            RenderedFile::new(
                format!("{dir}/conf/sites/00-default.caddy"),
                include_str!("templates/default-site.caddy").to_string(),
            ),
        ];
        if !caddy.sites.is_empty() {
            check_site_conflicts(ctx, config, &caddy.sites)?;
        }
        for site in &caddy.sites {
            files.push(RenderedFile::new(
                format!("{dir}/{CUSTOM_DIR}/{}.caddy", site.name),
                render_site(config, &caddy, site)?,
            ));
        }
        Ok(Rendered {
            files,
            dirs: vec![format!("{dir}/data"), format!("{dir}/config")],
            existing_networks: caddy.networks,
            owned_dirs: vec![format!("{dir}/{CUSTOM_DIR}")],
            ..Default::default()
        })
    }

    /// Validate the configuration files, then reload them without a restart.
    fn reload_shared(&self, ctx: &ServerCtx, config: &SharedAppConfig) -> Result<bool> {
        let dir = shell_quote(&ctx.path(&shared_dir(KIND, &config.name)));
        let project = shell_quote(&project_name(KIND, &config.name));
        let running = ctx.connector.exec(&format!(
            "cd {dir} && docker compose -p {project} ps --status running -q caddy"
        ))?;
        if running.trim().is_empty() {
            return Ok(false);
        }
        let caddy = |command: &str| {
            format!(
                "cd {dir} && docker compose -p {project} exec -T caddy caddy {command} --config /etc/caddy/Caddyfile --adapter caddyfile"
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

    /// Write the site file, let Caddy validate it, then reload without downtime.
    fn after_apply_application(&self, ctx: &AppCtx, link: &IngressLink) -> Result<()> {
        let caddy: CaddyConfig = ctx.shared.configuration()?;
        let mut routes = base::routes(ctx, link)?;
        // The `/` route becomes the fallback for everything not matched before.
        let fallback = routes
            .iter()
            .position(|r| r.path == "/")
            .map(|i| routes.remove(i).upstream);
        let scheme = if caddy.tls == Tls::Off { "http://" } else { "" };
        let addresses: Vec<String> = link
            .domains
            .iter()
            .map(|d| format!("{scheme}{d}"))
            .collect();
        let site = render_template(
            include_str!("templates/site.caddy.j2"),
            context! {
                instance => ctx.app.instance(),
                entry => link.name.as_str(),
                addresses => addresses.join(", "),
                routes => minijinja::value::Serde(&routes),
                fallback,
            },
        )?;

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
            script.push_str(&format!(
                " && if [ -d .restore/{name} ]; then rm -rf {name} && mv .restore/{name} {name}; fi"
            ));
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

/// The site block of an entry of `sites`, with its config indented.
fn render_site(config: &SharedAppConfig, caddy: &CaddyConfig, site: &Site) -> Result<String> {
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
            lines,
        },
    )
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
        "cd {} && docker compose -p {} exec -T caddy caddy {command} --config /etc/caddy/Caddyfile --adapter caddyfile",
        shell_quote(&ctx.server.path(&shared_dir(KIND, &ctx.shared.name))),
        shell_quote(&project_name(KIND, &ctx.shared.name))
    )
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
        Caddy.render(&ctx, &config).unwrap()
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
        assert!(compose.contains("networks: [compost-ingress]"), "{compose}");
        assert!(
            compose.ends_with("external: true\n"),
            "keeps the final newline: {compose:?}"
        );

        let caddyfile = file(&rendered, "conf/Caddyfile");
        assert!(
            !caddyfile.contains('{'),
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
                "http://shop.example.com, http://www.shop.example.com {\n\treverse_proxy shop-web-1:80\n\n\theader {\n\t  -Server\n\t}\n}\n"
            ),
            "{site:?}"
        );

        let rendered = render("{name: c, type: caddy}");
        assert!(!file(&rendered, "conf/Caddyfile").contains("custom"));
        assert!(rendered.existing_networks.is_empty());
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
}
