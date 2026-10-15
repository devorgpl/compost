//! `depap connect` / `depap disconnect` (spec §10.5).

use anyhow::Result;

use crate::config::local::DepapHome;

pub fn connect(home: &DepapHome, name: &str) -> Result<()> {
    let server = home.find_server(name)?;
    home.set_current_context(Some(&server.name))?;
    out!("Connected to {}", server.name);
    Ok(())
}

pub fn disconnect(home: &DepapHome) -> Result<()> {
    home.require_manager()?;
    match home.current_context()? {
        Some(name) => {
            home.set_current_context(None)?;
            out!("Disconnected from {name}");
        }
        None => out!("Not connected"),
    }
    Ok(())
}
