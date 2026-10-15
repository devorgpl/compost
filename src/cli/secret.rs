//! `depap secret …`: deployment overlays with secrets, kept on the server.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Subcommand;

use crate::app::secrets;
use crate::config::check_name;
use crate::config::local::DepapHome;
use crate::connector;

#[derive(Debug, Subcommand)]
pub enum SecretCommand {
    /// Store an instance's overlay (e.g. .depap.secrets.yaml) for `app deploy`
    Deploy {
        /// Overlay with the app's `name`, e.g. domains and database passwords
        file: PathBuf,
        /// Instance prefix (overrides the one in the file; default `main`)
        #[arg(long)]
        prefix: Option<String>,
        /// Remove a lock left behind by an interrupted depap command
        #[arg(long)]
        force_unlock: bool,
    },
    /// List the instances with stored secrets
    List,
    /// Delete the stored secrets of an instance
    Remove {
        /// Application name
        #[arg(long)]
        app: String,
        /// Instance prefix
        #[arg(long, default_value = "main")]
        prefix: String,
        /// Remove a lock left behind by an interrupted depap command
        #[arg(long)]
        force_unlock: bool,
    },
}

pub fn run(
    home: &DepapHome,
    server: Option<&str>,
    command: SecretCommand,
    verbose: bool,
) -> Result<()> {
    let manager = home.require_manager()?;
    let server = home.resolve_server(server)?;
    match command {
        SecretCommand::Deploy {
            file,
            prefix,
            force_unlock,
        } => {
            let text = fs::read_to_string(&file)
                .with_context(|| format!("cannot read {}", file.display()))?;
            let overlay = secrets::prepare(&text, prefix.as_deref())?;
            let connector = connector::open(&server, home, verbose)?;
            secrets::deploy(
                connector.as_ref(),
                server.server_home(),
                &manager.manager_key,
                &overlay,
                force_unlock,
            )
        }
        SecretCommand::List => {
            let connector = connector::open(&server, home, verbose)?;
            secrets::list(
                connector.as_ref(),
                server.server_home(),
                &manager.manager_key,
            )
        }
        SecretCommand::Remove {
            app,
            prefix,
            force_unlock,
        } => {
            check_name("app name", &app)?;
            check_name("prefix", &prefix)?;
            let connector = connector::open(&server, home, verbose)?;
            secrets::remove(
                connector.as_ref(),
                server.server_home(),
                &manager.manager_key,
                &app,
                &prefix,
                force_unlock,
            )
        }
    }
}
