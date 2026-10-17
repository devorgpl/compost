//! A connector for `--dry-run`: it reads from the server, keeps every file
//! write in memory and refuses any other command, so code written for a
//! deploy can run unchanged without changing the server.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::process::Command;

use anyhow::{Result, bail};

use super::{Connector, Output};

/// A file a dry run would write.
#[derive(Debug, Clone)]
pub struct Captured {
    pub contents: String,
    pub mode: u32,
}

pub struct DryRun<'a> {
    inner: &'a dyn Connector,
    writes: RefCell<BTreeMap<String, Captured>>,
}

impl<'a> DryRun<'a> {
    pub fn new(inner: &'a dyn Connector) -> Self {
        Self {
            inner,
            writes: RefCell::new(BTreeMap::new()),
        }
    }

    /// The files written so far, by absolute path.
    pub fn writes(&self) -> BTreeMap<String, Captured> {
        self.writes.borrow().clone()
    }
}

impl Connector for DryRun<'_> {
    fn target(&self) -> String {
        self.inner.target()
    }

    fn command(&self, script: &str) -> Command {
        self.inner.command(script)
    }

    fn verbose(&self) -> bool {
        self.inner.verbose()
    }

    fn run(&self, script: &str, _stdin: Option<&[u8]>) -> Result<Output> {
        refuse(script)
    }

    fn run_stream(
        &self,
        script: &str,
        _stdin: Option<&mut (dyn Read + Send)>,
        _stdout: &mut dyn Write,
    ) -> Result<Output> {
        refuse(script)
    }

    fn read_file(&self, path: &str) -> Result<Option<String>> {
        if let Some(captured) = self.writes.borrow().get(path) {
            return Ok(Some(captured.contents.clone()));
        }
        self.inner.read_file(path)
    }

    fn write_file(&self, path: &str, contents: &[u8], mode: u32) -> Result<()> {
        let captured = Captured {
            contents: String::from_utf8_lossy(contents).into_owned(),
            mode,
        };
        self.writes.borrow_mut().insert(path.to_string(), captured);
        Ok(())
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>> {
        let mut names = self.inner.list_dir(path)?;
        let prefix = format!("{}/", path.trim_end_matches('/'));
        for written in self.writes.borrow().keys() {
            if let Some(name) = written.strip_prefix(&prefix) {
                let name = name.split('/').next().unwrap_or(name).to_string();
                if !names.contains(&name) {
                    names.push(name);
                }
            }
        }
        names.sort();
        Ok(names)
    }

    fn resolve_path(&self, path: &str) -> Result<String> {
        self.inner.resolve_path(path)
    }
}

fn refuse(script: &str) -> Result<Output> {
    let first_line = script.lines().next().unwrap_or_default();
    bail!(
        "dry run: would run a command that may change the server (this is a bug in compost): {first_line}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::local::Local;
    use tempfile::TempDir;

    #[test]
    fn keeps_writes_in_memory_and_refuses_commands() {
        let dir = TempDir::new().unwrap();
        let local = Local { verbose: false };
        let existing = dir.path().join("a.yaml");
        std::fs::write(&existing, "old").unwrap();
        let root = dir.path().to_str().unwrap();
        let dry = DryRun::new(&local);

        let new = format!("{root}/sub/b.yaml");
        dry.write_file(&new, b"new", 0o600).unwrap();
        dry.write_file(existing.to_str().unwrap(), b"changed", 0o644)
            .unwrap();
        assert!(!dir.path().join("sub").exists(), "nothing reaches the disk");
        assert_eq!(std::fs::read_to_string(&existing).unwrap(), "old");

        assert_eq!(dry.read_file(&new).unwrap().as_deref(), Some("new"));
        assert_eq!(
            dry.read_file(existing.to_str().unwrap())
                .unwrap()
                .as_deref(),
            Some("changed")
        );
        assert_eq!(dry.list_dir(root).unwrap(), ["a.yaml", "sub"]);
        assert_eq!(dry.writes()[&new].mode, 0o600);

        let err = dry.exec("rm -rf /").unwrap_err().to_string();
        assert!(err.contains("dry run"), "{err}");
    }
}
