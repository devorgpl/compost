//! `depap init` (spec §10.2).

use anyhow::Result;
use clap::Args;
use uuid::Uuid;

use crate::config::local::{DepapHome, Manager, Servers};
use crate::error::config_err;

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Use this manager key (a UUID) instead of a new one, e.g. to manage the
    /// servers of another depap installation from this machine
    #[arg(long)]
    pub key: Option<String>,
    /// Replace the manager key of an initialized home
    #[arg(long, requires = "key")]
    pub force: bool,
}

pub fn run(home: &DepapHome, args: InitArgs) -> Result<()> {
    // Normalized, since servers compare the key as text.
    let key = match &args.key {
        Some(key) => Some(
            Uuid::parse_str(key.trim())
                .map_err(|_| config_err(format!("--key `{key}` is not a manager key (a UUID)")))?
                .to_string(),
        ),
        None => None,
    };

    if let Some(manager) = home.manager()? {
        match &key {
            Some(key) if *key != manager.manager_key && !args.force => {
                return Err(config_err(format!(
                    "already initialized with another manager key ({}); add --force to replace it \
                     (servers deployed with the old key will show as FOREIGN)",
                    manager.manager_key
                )));
            }
            Some(key) if *key != manager.manager_key => {}
            _ => {
                out!("Already initialized (manager key {})", manager.manager_key);
                return Ok(());
            }
        }
    }

    let manager = Manager {
        manager_key: key.unwrap_or_else(|| Uuid::new_v4().to_string()),
    };
    home.save_manager(&manager)?;
    if !home.servers_file_exists() {
        home.save_servers(&Servers::default())?;
    }
    out!(
        "Created {} (manager key {})",
        home.root().display(),
        manager.manager_key
    );
    Ok(())
}
