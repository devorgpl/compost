//! Backup archives: where they go, what they contain, and how items get in
//! and out of them. Commands (`app backup`, `server restore`, …) and the kinds
//! only describe items as scripts; this module moves the bytes.

mod archive;
pub mod profile;
pub mod remote;
pub mod retention;
mod source;
mod writer;

use std::path::PathBuf;
use std::time::SystemTime;

use anyhow::Result;
use serde::{Deserialize, Serialize};

pub use archive::{InstanceRef, Manifest, Scope, SharedRef};
pub use remote::Remote;
pub use retention::Store;
pub use source::Archive;
pub use writer::{Backup, Location, Output, human_size};

use crate::config::backup::unknown_target;
use crate::config::server::{BackupSettings, TargetConfig};
use crate::connector::{Connector, shell_quote};
use crate::shared::ServerCtx;

/// Default archive folder of the `server` target, relative to `COMPOST_SERVER_HOME`.
const DEFAULT_SERVER_PATH: &str = "backups";

/// `compost`: archive with a manifest, restorable by compost. `raw`: the plain
/// files (one file as is, several as a tar.gz), for use outside compost.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    #[default]
    Compost,
    Raw,
}

/// Parts of an instance a backup can hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Component {
    /// Data and configuration of links (database dumps, ingress sites)
    Links,
    /// Uploaded sources (only the compose file when sources are not uploaded)
    Sources,
    /// Named volumes listed under `backup.volumes`
    Volumes,
    /// Docker images of the services, as `docker save` writes them
    Images,
}

/// Without `--include`: everything but images, or only links when `--link` is given.
pub fn components(include: &[Component], links: &[String]) -> Vec<Component> {
    match (include, links) {
        ([], []) => vec![Component::Links, Component::Sources, Component::Volumes],
        ([], _) => vec![Component::Links],
        (include, _) => include.to_vec(),
    }
}

/// Configuration and secrets (`Meta`) are part of compost archives only; `raw`
/// output holds the data alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Data,
    Meta,
}

/// Writes the items of one shared app or link, below a folder of the archive.
pub struct ItemWriter<'a, 'c> {
    backup: &'a mut Backup<'c>,
    base: String,
}

impl<'a, 'c> ItemWriter<'a, 'c> {
    pub fn new(backup: &'a mut Backup<'c>, base: String) -> Self {
        Self { backup, base }
    }

    /// Run `script` on the server; its stdout becomes the item `name`.
    pub fn stream(&mut self, name: &str, role: Role, script: &str) -> Result<()> {
        self.backup
            .stream(&format!("{}/{name}", self.base), role, script)
    }

    pub fn file(&mut self, name: &str, role: Role, contents: &[u8]) -> Result<()> {
        self.backup
            .file(&format!("{}/{name}", self.base), role, contents)
    }
}

/// Reads the items of one shared app or link, below a folder of the archive.
pub struct ItemReader<'a, 'c> {
    archive: &'a Archive<'c>,
    base: String,
}

impl<'a, 'c> ItemReader<'a, 'c> {
    pub fn new(archive: &'a Archive<'c>, base: String) -> Self {
        Self { archive, base }
    }

    fn path(&self, name: &str) -> String {
        format!("{}/{name}", self.base)
    }

    pub fn has(&self, name: &str) -> bool {
        self.archive.has(&self.path(name))
    }

    pub fn read(&self, name: &str) -> Result<String> {
        self.archive.read(&self.path(name))
    }

    /// Run `script` with `prelude` and the item as stdin; a non-zero exit status is an error.
    pub fn feed(&self, name: &str, prelude: &[u8], script: &str) -> Result<()> {
        self.archive.feed(&self.path(name), prelude, script)
    }

    /// Like `feed`, but returns the output whatever the exit status.
    pub fn feed_output(&self, name: &str, script: &str) -> Result<crate::connector::Output> {
        self.archive.feed_output(&self.path(name), script)
    }
}

/// Where the archives of a target are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Destination {
    /// This machine; data is streamed through the connector.
    Local,
    /// A folder on the server (absolute path).
    Server { dir: String },
    /// Outside the server; archives are packed on the server, then copied.
    Remote(Remote),
}

impl Destination {
    /// The folder of archives, for listing and retention. `local` is the current folder.
    pub fn store<'c>(&self, connector: &'c dyn Connector) -> Store<'c> {
        match self {
            Destination::Local => Store::Local(std::path::PathBuf::from(".")),
            Destination::Server { dir } => Store::Server {
                connector,
                dir: dir.clone(),
            },
            Destination::Remote(remote) => Store::Remote {
                connector,
                remote: remote.clone(),
            },
        }
    }
}

/// How a backup gets to its target after [`Backup::finish`].
pub struct Sink<'c> {
    /// The remote and the server folder the archive is packed in first.
    upload: Option<(Remote, String)>,
    connector: &'c dyn Connector,
}

impl<'c> Sink<'c> {
    /// Where [`Backup`] writes, and the sink. `path` is the `-o` flag: a file
    /// or folder here for `local`, else a file name.
    pub fn new(
        ctx: &ServerCtx<'c>,
        destination: Destination,
        path: Option<&PathBuf>,
    ) -> (Output, Self) {
        let name = path.map(|p| p.display().to_string());
        let (output, upload) = match destination {
            Destination::Local => (Output::Local(path.cloned()), None),
            Destination::Server { dir } => (Output::Server { dir, name }, None),
            Destination::Remote(remote) => {
                let dir = ctx.path(&format!("tmp/upload-{}", uuid::Uuid::new_v4().simple()));
                (
                    Output::Server {
                        dir: dir.clone(),
                        name,
                    },
                    Some((remote, dir)),
                )
            }
        };
        let sink = Self {
            upload,
            connector: ctx.connector,
        };
        (output, sink)
    }

    /// After [`Backup::finish`]: copy the archive to a remote target. Returns
    /// where the archive is now, and its folder for retention.
    pub fn deliver(&self, location: Location) -> Result<(String, Store<'c>)> {
        let connector = self.connector;
        match (&self.upload, location) {
            (Some((remote, _)), Location::Server(path)) => {
                let name = path.rsplit('/').next().unwrap_or(&path).to_string();
                out!("  uploading {name} to {remote} ...");
                remote.upload(connector, &path)?;
                Ok((
                    format!("{name} at {remote}"),
                    Store::Remote {
                        connector,
                        remote: remote.clone(),
                    },
                ))
            }
            (_, location) => {
                let store = location.store(connector);
                Ok((location.to_string(), store))
            }
        }
    }
}

impl Drop for Sink<'_> {
    fn drop(&mut self) {
        // The archive was packed here for the upload: done with it either way.
        if let Some((_, dir)) = &self.upload {
            let _cleanup = crate::interrupt::cleanup();
            if let Err(err) = self
                .connector
                .exec(&format!("rm -rf -- {}", shell_quote(dir)))
            {
                eprintln!("warning: could not remove {dir}: {err:#}");
            }
        }
    }
}

/// The target `name` (a `--target`/`--source` flag or a profile's), else the
/// server's `backup.target`, else `local`.
pub fn resolve_target(
    ctx: &ServerCtx,
    settings: Option<&BackupSettings>,
    name: Option<&str>,
) -> Result<Destination> {
    let name = name
        .or_else(|| settings.and_then(|s| s.target.as_deref()))
        .unwrap_or("local");
    let absolute = |path: &str| {
        if path.starts_with('/') {
            path.to_string()
        } else {
            ctx.path(path)
        }
    };
    match name {
        "local" => Ok(Destination::Local),
        "server" => Ok(Destination::Server {
            dir: absolute(
                settings
                    .and_then(|s| s.server_path.as_deref())
                    .unwrap_or(DEFAULT_SERVER_PATH),
            ),
        }),
        name => {
            let targets = settings
                .map(BackupSettings::target_names)
                .unwrap_or_default();
            match settings.and_then(|s| s.targets.get(name)) {
                Some(TargetConfig::Server { path }) => Ok(Destination::Server {
                    dir: absolute(path),
                }),
                Some(target @ TargetConfig::S3 { .. }) => {
                    Ok(Destination::Remote(Remote::new(ctx, name, target)?))
                }
                None => Err(unknown_target("", name, &targets)),
            }
        }
    }
}

/// UTC time for archive names, e.g. `20261015-120000`.
pub fn timestamp() -> String {
    let rfc = humantime::format_rfc3339_seconds(SystemTime::now()).to_string();
    rfc.trim_end_matches('Z')
        .replace(['-', ':'], "")
        .replace('T', "-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets() {
        let connector = crate::connector::local::Local { verbose: false };
        let ctx = ServerCtx {
            connector: &connector,
            home: "/home/c/.compost-server".into(),
        };
        let settings: BackupSettings = serde_yaml_ng::from_str(
            "{target: server, targets: {nas: {type: server, path: /mnt/nas}, rel: {type: server, path: b2}}}",
        )
        .unwrap();
        let resolve = |settings, name| resolve_target(&ctx, settings, name);
        assert_eq!(resolve(None, None).unwrap(), Destination::Local);
        let server = |dir: &str| Destination::Server { dir: dir.into() };
        assert_eq!(
            resolve(Some(&settings), None).unwrap(),
            server("/home/c/.compost-server/backups")
        );
        assert_eq!(
            resolve(Some(&settings), Some("local")).unwrap(),
            Destination::Local
        );
        assert_eq!(
            resolve(Some(&settings), Some("nas")).unwrap(),
            server("/mnt/nas")
        );
        assert_eq!(
            resolve(Some(&settings), Some("rel")).unwrap(),
            server("/home/c/.compost-server/b2")
        );
        let err = resolve(Some(&settings), Some("s3")).unwrap_err();
        assert!(
            err.to_string().contains("unknown backup target `s3`"),
            "{err}"
        );
    }

    #[test]
    fn timestamp_is_file_name_friendly() {
        let ts = timestamp();
        assert_eq!(ts.len(), "20261015-120000".len(), "{ts}");
        assert!(ts.chars().all(|c| c.is_ascii_digit() || c == '-'), "{ts}");
    }
}
