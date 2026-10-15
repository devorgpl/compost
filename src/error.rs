//! Errors that decide the process exit code (spec §10).

use thiserror::Error;

#[derive(Debug, Error)]
pub enum DepapError {
    /// Invalid local configuration or command usage.
    #[error("{0}")]
    Config(String),
    /// Command exists in the CLI but has no implementation yet.
    #[error("`{0}` is not implemented yet")]
    NotImplemented(&'static str),
}

impl DepapError {
    pub fn exit_code(&self) -> u8 {
        match self {
            DepapError::Config(_) => 2,
            DepapError::NotImplemented(_) => 1,
        }
    }
}

/// Shorthand for returning a configuration error (exit code 2).
pub fn config_err(message: impl Into<String>) -> anyhow::Error {
    DepapError::Config(message.into()).into()
}
