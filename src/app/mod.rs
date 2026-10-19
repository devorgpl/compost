//! Operations on user applications.

pub mod backup;
pub mod contribution;
pub mod deploy;
pub mod exec;
pub mod instance;
pub mod list;
mod local_build;
pub mod logs;
mod plan;
pub mod release;
pub mod remove;
pub mod reset;
pub mod restore;
pub mod rollback;
pub mod secrets;
mod sources;
pub mod state;
mod steps;
pub mod stop;
mod volumes;

use std::io::{BufRead, IsTerminal, Write};

use anyhow::Result;

use crate::error::config_err;

/// Ask `Continue? [y/N]` on the terminal.
pub fn confirm() -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        return Err(config_err(
            "not asking for confirmation without a terminal; add --yes",
        ));
    }
    print!("Continue? [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}
