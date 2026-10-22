//! `compost secret …`: deployment overlays with secrets, kept on the server.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Subcommand;

use crate::app::secrets;
use crate::config::check_name;
use crate::config::local::CompostHome;
use crate::connector::{self, ConnectOptions};
use crate::server::secrets as server_secrets;

#[derive(Debug, Subcommand)]
pub enum SecretCommand {
    /// Store an instance's overlay (e.g. .compost.secrets.yaml) for `app deploy`
    Deploy {
        /// Overlay with the app's `name`, e.g. domains and database passwords
        file: PathBuf,
        /// Instance prefix (overrides the one in the file; default: the `prefix` setting, else `main`)
        #[arg(long)]
        prefix: Option<String>,
        /// Remove a lock left behind by an interrupted compost command
        #[arg(long)]
        force_unlock: bool,
    },
    /// List the instances with stored secrets, and the shared apps' secrets
    List,
    /// Delete the stored secrets of an instance, or of the shared apps
    Remove {
        /// Application name
        #[arg(long, required_unless_present = "shared", conflicts_with = "shared")]
        app: Option<String>,
        /// Instance prefix
        #[arg(long, default_value = "main", conflicts_with = "shared")]
        prefix: String,
        /// The shared apps' secrets, stored by `server deploy --secret`
        #[arg(long)]
        shared: bool,
        /// Remove a lock left behind by an interrupted compost command
        #[arg(long)]
        force_unlock: bool,
    },
}

pub fn run(
    home: &CompostHome,
    server: Option<&str>,
    command: SecretCommand,
    connect: &ConnectOptions,
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
            let fallback = home.defaults(&server)?.prefix;
            let overlay = secrets::prepare(&text, prefix.as_deref(), &fallback)?;
            let connector = connector::open(&server, home, connect)?;
            secrets::deploy(
                connector.as_ref(),
                server.server_home(),
                &manager.manager_key,
                &overlay,
                force_unlock,
            )
        }
        SecretCommand::List => {
            let connector = connector::open(&server, home, connect)?;
            secrets::list(
                connector.as_ref(),
                server.server_home(),
                &manager.manager_key,
            )
        }
        SecretCommand::Remove {
            app: None,
            force_unlock,
            ..
        } => {
            let connector = connector::open(&server, home, connect)?;
            server_secrets::remove(
                connector.as_ref(),
                server.server_home(),
                &manager.manager_key,
                force_unlock,
            )
        }
        SecretCommand::Remove {
            app: Some(app),
            prefix,
            force_unlock,
            ..
        } => {
            check_name("app name", &app)?;
            check_name("prefix", &prefix)?;
            let connector = connector::open(&server, home, connect)?;
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
