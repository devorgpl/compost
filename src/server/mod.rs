//! Operations on a managed server.

pub mod backup;
pub mod compose;
pub mod deploy;
pub mod links;
pub mod lock;
pub mod secrets;
pub mod state;
pub mod state_file;

/// Files inside COMPOST_SERVER_HOME (spec §8).
pub const MANAGER_KEY_FILE: &str = "manager_key";
pub const SERVER_CONFIG_FILE: &str = "compost-server.yaml";
