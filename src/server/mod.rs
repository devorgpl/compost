//! Operations on a managed server.

pub mod deploy;
mod lock;
pub mod state;
mod state_file;

/// Files inside DEPAP_SERVER_HOME (spec §8).
pub const MANAGER_KEY_FILE: &str = "manager_key";
pub const SERVER_CONFIG_FILE: &str = "depap-server.yaml";
