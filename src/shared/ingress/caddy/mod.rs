//! Caddy ingress type (spec §5.3, §9.2).

use anyhow::{Context, Result, ensure};
use minijinja::context;
use serde::Deserialize;

use super::base::{self, IngressLink};
use super::{IngressType, KIND};
use crate::connector::shell_quote;
use crate::shared::{
    AppCtx, Generated, ItemReader, ItemWriter, Rendered, RenderedFile, Role, ServerCtx,
    SharedAppConfig, project_name, render_template, shared_dir,
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
}

impl Default for CaddyConfig {
    fn default() -> Self {
        Self {
            http_port: 80,
            https_port: 443,
            tls: Tls::Auto,
            acme_email: None,
        }
    }
}

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
        Ok(())
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

        let compose = render_template(
            include_str!("templates/compose.yaml.j2"),
            context! {
                image,
                user,
                http_port => caddy.http_port,
                https_port => caddy.https_port,
                publish_https => caddy.tls != Tls::Off,
                network => base::NETWORK,
            },
        )?;
        let caddyfile =
            render_template(include_str!("templates/Caddyfile.j2"), context! { options })?;

        let dir = shared_dir(KIND, &config.name);
        Ok(Rendered {
            files: vec![
                RenderedFile::new(format!("{dir}/compose.yaml"), compose),
                RenderedFile::new(format!("{dir}/conf/Caddyfile"), caddyfile),
                RenderedFile::new(
                    format!("{dir}/conf/sites/00-default.caddy"),
                    include_str!("templates/default-site.caddy").to_string(),
                ),
            ],
            dirs: vec![format!("{dir}/data"), format!("{dir}/config")],
            networks: Vec::new(),
        })
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
