//! Backup archives: where they go, what they contain, and how items get in
//! and out of them. Commands (`app backup`, `server restore`, …) and the kinds
//! only describe items as scripts; this module moves the bytes.

mod archive;
pub mod profile;
pub mod retention;
mod source;
mod writer;

use std::time::SystemTime;

use anyhow::Result;
use serde::{Deserialize, Serialize};

pub use archive::{InstanceRef, Manifest, Scope, SharedRef};
pub use source::Archive;
pub use writer::{Backup, Output, human_size};

use crate::config::backup::unknown_target;
use crate::config::server::{BackupSettings, TargetConfig};
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
