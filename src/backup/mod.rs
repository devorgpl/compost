//! Backup archives: where they go, what they contain, and how items get in
//! and out of them. Commands (`app backup`, `server restore`, …) and the kinds
//! only describe items as scripts; this module moves the bytes.

mod archive;
mod source;
mod writer;

use std::time::SystemTime;

use anyhow::Result;

pub use archive::{InstanceRef, Manifest, Scope, SharedRef};
pub use source::Archive;
pub use writer::{Backup, Output, human_size};

use crate::config::server::{BackupSettings, BackupTarget};
use crate::error::config_err;
use crate::shared::ServerCtx;

/// Default archive folder of the `server` target, relative to COMPOST_SERVER_HOME.
const DEFAULT_SERVER_PATH: &str = "backups";

/// `compost`: archive with a manifest, restorable by compost. `raw`: the plain
/// files (one file as is, several as a tar.gz), for use outside compost.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    #[default]
    Compost,
    Raw,
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

/// The `--target`/`--source` flag, else the server's `backup.target`, else local.
pub fn resolve_target(
    flag: Option<BackupTarget>,
    settings: Option<&BackupSettings>,
) -> Result<BackupTarget> {
    let target = flag
        .or_else(|| settings.map(|s| s.target))
        .unwrap_or_default();
    if target == BackupTarget::S3 {
        return Err(config_err("the s3 backup target is not implemented yet"));
    }
    Ok(target)
}

/// Absolute archive folder of the `server` target.
pub fn server_dir(ctx: &ServerCtx, settings: Option<&BackupSettings>) -> String {
    let path = settings
        .and_then(|s| s.server_path.as_deref())
        .unwrap_or(DEFAULT_SERVER_PATH);
    if path.starts_with('/') {
        path.to_string()
    } else {
        ctx.path(path)
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
    fn target_defaults_to_local_and_s3_is_not_implemented() {
        assert_eq!(resolve_target(None, None).unwrap(), BackupTarget::Local);
        let server = BackupSettings {
            target: BackupTarget::Server,
            ..BackupSettings::default()
        };
        assert_eq!(
            resolve_target(None, Some(&server)).unwrap(),
            BackupTarget::Server
        );
        assert_eq!(
            resolve_target(Some(BackupTarget::Local), Some(&server)).unwrap(),
            BackupTarget::Local
        );
        let err = resolve_target(Some(BackupTarget::S3), None).unwrap_err();
        assert!(err.to_string().contains("not implemented"), "{err}");
    }

    #[test]
    fn timestamp_is_file_name_friendly() {
        let ts = timestamp();
        assert_eq!(ts.len(), "20261015-120000".len(), "{ts}");
        assert!(ts.chars().all(|c| c.is_ascii_digit() || c == '-'), "{ts}");
    }
}
