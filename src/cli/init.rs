//! `depap init` (spec §10.2).

use anyhow::Result;
use uuid::Uuid;

use crate::config::local::{DepapHome, Manager, Servers};

pub fn run(home: &DepapHome) -> Result<()> {
    if let Some(manager) = home.manager()? {
        println!("Already initialized (manager key {})", manager.manager_key);
        return Ok(());
    }

    let manager = Manager {
        manager_key: Uuid::new_v4().to_string(),
    };
    home.save_manager(&manager)?;
    if !home.servers_file_exists() {
        home.save_servers(&Servers::default())?;
    }
    println!(
        "Created {} (manager key {})",
        home.root().display(),
        manager.manager_key
    );
    Ok(())
}
