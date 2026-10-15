//! Operations on user applications.

pub mod backup;
pub mod contribution;
pub mod deploy;
pub mod instance;
pub mod list;
pub mod remove;
pub mod restore;
mod sources;
pub mod state;
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
