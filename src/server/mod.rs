//! Operations on a managed server.

pub mod alerts;
pub mod backup;
pub mod compose;
pub mod deploy;
pub mod exec;
pub mod images;
pub mod links;
pub mod lock;
pub mod logs;
pub mod plan;
pub mod prune;
pub mod schedule;
pub mod secrets;
pub mod state;
pub mod state_file;
pub mod teardown;
pub mod updates;

/// Files inside `COMPOST_SERVER_HOME` (spec §8).
pub const MANAGER_KEY_FILE: &str = "manager_key";
pub const SERVER_CONFIG_FILE: &str = "compost-server.yaml";
