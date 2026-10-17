//! Errors that decide the process exit code (spec §10).

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CompostError {
    /// Invalid local configuration or command usage.
    #[error("{0}")]
    Config(String),
    /// Server not ready, or a server-side check failed.
    #[error("{0}")]
    ServerNotReady(String),
    /// The command ran, but the final state check is not healthy.
    #[error("{0}")]
    Unhealthy(String),
    /// Stopped by Ctrl-C.
    #[error("interrupted")]
    Interrupted,
    /// A command run for the user (`app exec`, `app run`) exited with this
    /// status, which becomes compost's; it already said why.
    #[error("the command exited with status {0}")]
    CommandExit(u8),
}

impl CompostError {
    pub fn exit_code(&self) -> u8 {
        match self {
            CompostError::Config(_) => 2,
            CompostError::ServerNotReady(_) => 3,
            CompostError::Unhealthy(_) => 4,
            CompostError::Interrupted => 130,
            CompostError::CommandExit(code) => *code,
        }
    }
}

/// Shorthand for returning a configuration error (exit code 2).
pub fn config_err(message: impl Into<String>) -> anyhow::Error {
    CompostError::Config(message.into()).into()
}
