//! Ctrl-C handling (spec §10.1): stop before the next command and release the
//! server lock, instead of dying with the lock still held.
//!
//! The first Ctrl-C also stops the command that is running (ssh, docker), since
//! the terminal sends it to every process of the foreground job. compost then
//! refuses to start new commands, the error travels up, and cleanup code (like
//! `Lock`'s drop) runs. A second Ctrl-C exits at once.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use anyhow::Result;

use crate::error::CompostError;

static INTERRUPTED: AtomicBool = AtomicBool::new(false);
/// Number of active cleanup guards; commands may run while it is above zero.
static CLEANUPS: AtomicUsize = AtomicUsize::new(0);
/// Number of active [`expect_ctrl_c`] guards: Ctrl-C is how the user ends the
/// running command, so it is not reported.
static EXPECTED: AtomicUsize = AtomicUsize::new(0);

pub fn install() -> Result<()> {
    ctrlc::set_handler(|| {
        if INTERRUPTED.swap(true, Ordering::SeqCst) {
            eprintln!(
                "\ninterrupted again: exiting now (if the server stays locked, use --force-unlock)"
            );
            std::process::exit(130);
        }
        if EXPECTED.load(Ordering::SeqCst) == 0 {
            eprintln!("\ninterrupted: stopping and cleaning up (press Ctrl-C again to exit now)");
        }
    })?;
    Ok(())
}

/// Fails once Ctrl-C was pressed, unless cleanup is running.
pub fn check() -> Result<()> {
    if INTERRUPTED.load(Ordering::SeqCst) && CLEANUPS.load(Ordering::SeqCst) == 0 {
        return Err(CompostError::Interrupted.into());
    }
    Ok(())
}

/// Allows commands to run after Ctrl-C while the returned guard is alive.
pub fn cleanup() -> CleanupGuard {
    CLEANUPS.fetch_add(1, Ordering::SeqCst);
    CleanupGuard
}

pub struct CleanupGuard;

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        CLEANUPS.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Whether Ctrl-C was pressed.
pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

/// Ctrl-C ends the command that runs while the returned guard is alive (e.g.
/// following logs), so the usual message is not printed.
pub fn expect_ctrl_c() -> ExpectGuard {
    EXPECTED.fetch_add(1, Ordering::SeqCst);
    ExpectGuard
}

pub struct ExpectGuard;

impl Drop for ExpectGuard {
    fn drop(&mut self) {
        EXPECTED.fetch_sub(1, Ordering::SeqCst);
    }
}
