//! Server lock: only one mutating depap command per server at a time (spec §10.1).

use std::time::SystemTime;

use anyhow::Result;

use crate::connector::{Connector, shell_quote};
use crate::error::DepapError;

const LOCK_DIR: &str = "lock";
/// Exit status of the lock script when the lock is already taken.
const LOCKED_STATUS: i32 = 100;

/// Holds the lock until dropped, i.e. until the value goes out of scope.
pub struct Lock<'a> {
    connector: &'a dyn Connector,
    path: String,
}

impl<'a> Lock<'a> {
    /// Create `<home>/lock/` (mkdir is atomic) with a note about who holds it.
    pub fn acquire(connector: &'a dyn Connector, home: &str, force: bool) -> Result<Self> {
        let path = format!("{home}/{LOCK_DIR}");
        let quoted = shell_quote(&path);
        if force {
            connector.exec(&format!("rm -rf -- {quoted}"))?;
        }

        let script =
            format!("mkdir -- {quoted} 2>/dev/null || exit {LOCKED_STATUS}; cat > {quoted}/holder");
        let output = connector.run(&script, Some(holder().as_bytes()))?;
        match output.status {
            0 => Ok(Self { connector, path }),
            LOCKED_STATUS => {
                let holder = connector
                    .read_file(&format!("{path}/holder"))?
                    .unwrap_or_else(|| "unknown".to_string());
                Err(DepapError::ServerNotReady(format!(
                    "the server is locked by another depap command ({})\n\
                     If no other command is running, retry with --force-unlock.",
                    holder.trim()
                ))
                .into())
            }
            status => anyhow::bail!(
                "cannot create lock {path} (exit status {status}): {}",
                output.stderr.trim()
            ),
        }
    }
}

impl Drop for Lock<'_> {
    fn drop(&mut self) {
        // Runs after Ctrl-C too: releasing the lock is the point of cleanup.
        let _cleanup = crate::interrupt::cleanup();
        let script = format!("rm -rf -- {}", shell_quote(&self.path));
        if let Err(err) = self.connector.exec(&script) {
            eprintln!(
                "warning: could not remove the server lock {}: {err:#}",
                self.path
            );
        }
    }
}

/// e.g. `host=laptop user=cyprian pid=4242 time=2026-10-15T12:00:00Z`
fn holder() -> String {
    let host = std::fs::read_to_string("/etc/hostname").unwrap_or_else(|_| "unknown".into());
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".into());
    let time = humantime::format_rfc3339_seconds(SystemTime::now());
    format!(
        "host={} user={user} pid={} time={time}\n",
        host.trim(),
        std::process::id()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::local::Local;
    use tempfile::TempDir;

    const LOCAL: Local = Local { verbose: false };

    #[test]
    fn lock_is_exclusive_and_released_on_drop() {
        let dir = TempDir::new().unwrap();
        let home = dir.path().to_str().unwrap();

        let lock = Lock::acquire(&LOCAL, home, false).unwrap();
        let err = Lock::acquire(&LOCAL, home, false).err().unwrap();
        assert!(
            err.to_string().contains("locked by another depap command"),
            "{err}"
        );

        drop(lock);
        assert!(!dir.path().join(LOCK_DIR).exists());
        Lock::acquire(&LOCAL, home, false).unwrap();
    }

    #[test]
    fn force_removes_a_stale_lock() {
        let dir = TempDir::new().unwrap();
        let home = dir.path().to_str().unwrap();
        std::mem::forget(Lock::acquire(&LOCAL, home, false).unwrap()); // never released
        Lock::acquire(&LOCAL, home, true).unwrap();
    }
}
