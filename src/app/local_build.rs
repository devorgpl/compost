//! `build: local`: build the images on this machine and send them to the
//! server with `docker save | docker load`, without a registry.

use std::io::Read;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use flate2::Compression;
use flate2::read::GzEncoder;

use crate::config::app::LoadedApp;
use crate::connector::local::Local;
use crate::connector::{Connector, shell_quote};
use crate::shared::ServerCtx;

/// What tells two images apart: the image config, which stays the same when
/// a build is fully cached. Image IDs don't: with the containerd image store
/// they change on every build, and they differ between image stores.
const FINGERPRINT: &str = "{{.Created}} {{json .RootFS.Layers}} {{json .Config}}";

/// Images built on this machine, ready to be sent.
pub struct Built {
    /// One per service with `build`, e.g. `myapp-main-web`.
    images: Vec<String>,
}

/// `docker compose build` the services with `build` for the server's platform.
pub fn build(ctx: &ServerCtx, loaded: &LoadedApp, pull: bool) -> Result<Built> {
    let local = Local {
        verbose: ctx.connector.verbose(),
    };
    let platform = ctx
        .connector
        .exec("docker version -f '{{.Server.Os}}/{{.Server.Arch}}'")
        .context("cannot read the platform of Docker on the server")?;
    let platform = platform.trim();
    let compose = compose_command(loaded);
    let images = images(&local, &compose, &loaded.config.instance())?;
    if images.is_empty() {
        return Ok(Built { images });
    }

    out!("  building images locally for {platform} (experimental) ...");
    let pull = if pull { " --pull" } else { "" };
    local
        .exec(&format!(
            "DOCKER_DEFAULT_PLATFORM={} {compose} build -q{pull}",
            shell_quote(platform)
        ))
        .context("cannot build the images on this machine (`build: local`)")?;
    Ok(Built { images })
}

/// Send the images the server doesn't have yet; the caller holds the lock.
pub fn send(ctx: &ServerCtx, built: &Built) -> Result<()> {
    let local = Local {
        verbose: ctx.connector.verbose(),
    };
    let mut changed = Vec::new();
    for image in &built.images {
        let inspect = format!(
            "docker image inspect -f '{FINGERPRINT}' {}",
            shell_quote(image)
        );
        let here = local.exec(&inspect)?;
        let there = ctx.connector.run(&inspect, None)?;
        if !there.success() || there.stdout != here {
            changed.push(image.as_str());
        }
    }
    if changed.is_empty() {
        out!("  images are up to date on the server");
        return Ok(());
    }

    out!("  sending {} ...", changed.join(", "));
    let mut save = Command::new("docker");
    save.arg("save").args(&changed);
    if local.verbose() {
        eprintln!("[local] $ docker save {}", changed.join(" "));
    }
    let mut child = save
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("cannot start `docker save`")?;
    let stdout = child.stdout.take().context("no stdout pipe")?;
    let mut sent = Counter {
        inner: GzEncoder::new(stdout, Compression::fast()),
        bytes: 0,
    };
    let loaded = ctx.connector.stream_in("docker load -q", &mut sent);
    if loaded.is_err() {
        let _ = child.kill();
    }
    let status = child.wait().context("`docker save` failed")?;
    loaded.context("cannot load the images on the server")?;
    if !status.success() {
        bail!("`docker save` failed with {status}");
    }
    out!("  sent {} MB", sent.bytes.div_ceil(1_000_000));
    Ok(())
}

/// `docker compose` on this machine with the app's compose files, as the
/// server runs it on the uploaded sources.
fn compose_command(loaded: &LoadedApp) -> String {
    let deployment = &loaded.config.deployment;
    let mut files = vec![loaded.source_dir.join(&deployment.compose_file)];
    files.extend(loaded.overrides.iter().map(|(from, _)| from.clone()));
    let files: String = files
        .iter()
        .map(|file| format!(" -f {}", shell_quote(&file.to_string_lossy())))
        .collect();
    format!(
        "docker compose -p {} --project-directory {}{files}",
        shell_quote(&loaded.config.instance()),
        shell_quote(&loaded.source_dir.to_string_lossy()),
    )
}

/// The image of every service with `build`: its `image`, or the name compose
/// gives it, `<project>-<service>`.
fn images(local: &Local, compose: &str, project: &str) -> Result<Vec<String>> {
    let output = local
        .exec(&format!("{compose} config --format json"))
        .context(
            "building images locally (`build: local`) needs Docker with compose v2 on this machine",
        )?;
    let config: serde_json::Value =
        serde_json::from_str(&output).context("cannot read `docker compose config`")?;
    let mut images: Vec<String> = config
        .get("services")
        .and_then(|s| s.as_object())
        .into_iter()
        .flatten()
        .filter(|(_, service)| service.get("build").is_some())
        .map(
            |(name, service)| match service.get("image").and_then(|i| i.as_str()) {
                Some(image) => image.to_string(),
                None => format!("{project}-{name}"),
            },
        )
        .collect();
    images.sort();
    images.dedup();
    Ok(images)
}

/// Counts the bytes read through it.
struct Counter<R> {
    inner: R,
    bytes: u64,
}

impl<R: Read> Read for Counter<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.bytes += n as u64;
        Ok(n)
    }
}
