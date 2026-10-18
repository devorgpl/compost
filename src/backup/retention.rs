//! Archives at a target, and deleting those beyond a profile's `keep`.
//!
//! A profile's archives are named `<owner>-<profile>-<yyyymmdd-hhmmss>…`, where
//! the owner is an instance (`myapp-prod`) or the server's name.

use std::fmt;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::connector::{Connector, shell_quote};

/// A folder with archives.
pub enum Store<'c> {
    Local(PathBuf),
    Server {
        connector: &'c dyn Connector,
        dir: String,
    },
}

impl fmt::Display for Store<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Store::Local(dir) => write!(f, "{}", dir.display()),
            Store::Server { dir, .. } => write!(f, "{dir} (on the server)"),
        }
    }
}

impl Store<'_> {
    /// Files in the folder with their sizes, sorted by name. A missing folder is empty.
    pub fn list(&self) -> Result<Vec<(String, u64)>> {
        let mut rows = match self {
            Store::Local(dir) => {
                let entries = match fs::read_dir(dir) {
                    Ok(entries) => entries,
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
                    Err(err) => return Err(err).context(format!("cannot read {}", dir.display())),
                };
                let mut rows = Vec::new();
                for entry in entries {
                    let entry = entry?;
                    let meta = entry.metadata()?;
                    if meta.is_file() {
                        rows.push((entry.file_name().to_string_lossy().into_owned(), meta.len()));
                    }
                }
                rows
            }
            Store::Server { connector, dir } => {
                let listing = connector.exec(&format!(
                    "cd {} 2>/dev/null || exit 0; for f in *; do [ -f \"$f\" ] && printf '%s\\t%s\\n' \"$(wc -c < \"$f\")\" \"$f\"; done; true",
                    shell_quote(dir)
                ))?;
                listing
                    .lines()
                    .filter_map(|line| {
                        let (size, name) = line.split_once('\t')?;
                        Some((name.to_string(), size.trim().parse().ok()?))
                    })
                    .collect()
            }
        };
        rows.sort();
        Ok(rows)
    }

    pub fn delete(&self, name: &str) -> Result<()> {
        match self {
            Store::Local(dir) => {
                let path = dir.join(name);
                fs::remove_file(&path).with_context(|| format!("cannot delete {}", path.display()))
            }
            Store::Server { connector, dir } => {
                connector.exec(&format!(
                    "rm -f -- {}",
                    shell_quote(&format!("{dir}/{name}"))
                ))?;
                Ok(())
            }
        }
    }
}

/// Start of the names of `owner`'s archives of `profile`.
pub fn prefix(owner: &str, profile: &str) -> String {
    format!("{owner}-{profile}-")
}

/// Whether `name` is `prefix` followed by a timestamp and an extension or
/// `-…`, and not a file still being written (`.tmp`).
fn matches(name: &str, prefix: &str) -> bool {
    let Some(rest) = name.strip_prefix(prefix) else {
        return false;
    };
    if rest.ends_with(".tmp") {
        return false;
    }
    let bytes = rest.as_bytes();
    bytes.len() > 15
        && bytes[..8].iter().all(u8::is_ascii_digit)
        && bytes[8] == b'-'
        && bytes[9..15].iter().all(u8::is_ascii_digit)
        && matches!(bytes[15], b'.' | b'-')
}

/// The archives of `prefix` among `names` beyond the newest `keep`, oldest first.
pub fn expired<'a>(
    names: impl IntoIterator<Item = &'a str>,
    prefix: &str,
    keep: u32,
) -> Vec<&'a str> {
    let mut found: Vec<&str> = names.into_iter().filter(|n| matches(n, prefix)).collect();
    // The timestamp follows the same prefix, so names sort by time.
    found.sort_unstable();
    let excess = found.len().saturating_sub(keep as usize);
    found.truncate(excess);
    found
}

/// Delete `owner`'s archives of `profile` in `store` beyond the newest `keep`.
pub fn apply(store: &Store, owner: &str, profile: &str, keep: u32) -> Result<()> {
    let rows = store.list()?;
    let prefix = prefix(owner, profile);
    for name in expired(rows.iter().map(|(n, _)| n.as_str()), &prefix, keep) {
        store.delete(name)?;
        out!("  deleted {name} (profile {profile} keeps {keep})");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expired_archives() {
        let names = [
            "myapp-prod-daily-20261015-030000.compost.tar.gz",
            "myapp-prod-daily-20261017-030000.compost.tar.gz",
            "myapp-prod-daily-20261016-030000-main-db.sql",
            "myapp-prod-daily-20261014-030000.compost.tar.gz.tmp",
            "myapp-prod-weekly-20261001-030000.compost.tar.gz",
            "myapp-prod-20261001-030000.compost.tar.gz",
            "myapp-prod-daily-latest.compost.tar.gz",
            "myapp-prod-daily-20261013-03000.tar.gz",
        ];
        let daily = prefix("myapp-prod", "daily");
        assert_eq!(
            expired(names, &daily, 2),
            ["myapp-prod-daily-20261015-030000.compost.tar.gz"]
        );
        assert!(expired(names, &daily, 9).is_empty());
        assert_eq!(
            expired(names, &prefix("myapp-prod", "weekly"), 1),
            Vec::<&str>::new()
        );
    }

    #[test]
    fn local_store() {
        let dir = tempfile::TempDir::new().unwrap();
        for name in [
            "s-daily-20261015-030000.tar.gz",
            "s-daily-20261016-030000.tar.gz",
            "other",
        ] {
            fs::write(dir.path().join(name), "x").unwrap();
        }
        let store = Store::Local(dir.path().to_path_buf());
        apply(&store, "s", "daily", 1).unwrap();
        let names: Vec<String> = store.list().unwrap().into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["other", "s-daily-20261016-030000.tar.gz"]);
    }
}
