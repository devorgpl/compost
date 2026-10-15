//! Operations on a managed server.

pub mod state;

/// Files inside DEPAP_SERVER_HOME (spec §8).
pub const MANAGER_KEY_FILE: &str = "manager_key";
pub const SERVER_CONFIG_FILE: &str = "depap-server.yaml";
