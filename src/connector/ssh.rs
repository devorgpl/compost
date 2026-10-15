//! Runs scripts on a remote server through the system `ssh` client, so the
//! user's ~/.ssh/config, agent and known_hosts apply unchanged.

use std::path::Path;
use std::process::Command;

use anyhow::{Result, bail};

use super::{Connector, Output, shell_quote};

/// `ssh` exits with this status when it cannot connect or authenticate.
const SSH_ERROR_STATUS: i32 = 255;
/// Keep the shared connection this many seconds after its last use.
const CONTROL_PERSIST_SECONDS: u32 = 60;
/// Unix socket paths are limited to about 104 bytes; `%C` expands to 40.
const MAX_CONTROL_DIR_LEN: usize = 60;

/// `<dir>/%C` for connection sharing, or `None` if the path would be too long.
pub fn control_path(dir: &Path) -> Option<String> {
    let dir = dir.to_str()?;
    (dir.len() <= MAX_CONTROL_DIR_LEN).then(|| format!("{dir}/%C"))
}

pub struct Ssh {
    pub address: String,
    pub port: u16,
    pub user: String,
    pub identity_file: Option<String>,
    /// Socket for sharing one SSH connection between commands (`ControlPath`).
    pub control_path: Option<String>,
    pub verbose: bool,
}

impl Ssh {
    fn args(&self, script: &str) -> Vec<String> {
        let mut args: Vec<String> = [
            "-o",
            "BatchMode=yes", // never prompt for passwords; fail instead
            "-o",
            "ConnectTimeout=10",
            "-p",
        ]
        .map(String::from)
        .into();
        args.push(self.port.to_string());
        if let Some(key) = &self.identity_file {
            args.extend(["-i".to_string(), key.clone()]);
        }
        // The first command opens a connection in the background; later
        // commands reuse it instead of connecting and authenticating again.
        if let Some(path) = &self.control_path {
            for option in [
                "ControlMaster=auto".to_string(),
                format!("ControlPath={path}"),
                format!("ControlPersist={CONTROL_PERSIST_SECONDS}"),
            ] {
                args.extend(["-o".to_string(), option]);
            }
        }
        // `--` stops option parsing, so an address can never be read as an option.
        args.push("--".to_string());
        args.push(format!("{}@{}", self.user, self.address));
        // ssh hands this string to the remote login shell; wrap it so it always runs in sh.
        args.push(format!("sh -c {}", shell_quote(script)));
        args
    }
}

impl Connector for Ssh {
    fn target(&self) -> String {
        format!("ssh {}@{}:{}", self.user, self.address, self.port)
    }

    fn command(&self, script: &str) -> Command {
        let mut command = Command::new("ssh");
        command.args(self.args(script));
        command
    }

    fn verbose(&self) -> bool {
        self.verbose
    }

    fn check_connection(&self, output: &Output) -> Result<()> {
        if output.status == SSH_ERROR_STATUS {
            bail!(
                "cannot connect to {}: {}\n\
                 (check with `ssh -p {} {}@{}`; a new host key must be accepted once)",
                self.target(),
                output.stderr.trim_end(),
                self.port,
                self.user,
                self.address
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_ssh_arguments() {
        let ssh = Ssh {
            address: "10.0.0.1".into(),
            port: 2222,
            user: "compostuser".into(),
            identity_file: Some("~/.ssh/id_ed25519".into()),
            control_path: Some("/home/u/.compost/ssh/%C".into()),
            verbose: false,
        };
        assert_eq!(
            ssh.args("echo 'hi'"),
            [
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "-p",
                "2222",
                "-i",
                "~/.ssh/id_ed25519",
                "-o",
                "ControlMaster=auto",
                "-o",
                "ControlPath=/home/u/.compost/ssh/%C",
                "-o",
                "ControlPersist=60",
                "--",
                "compostuser@10.0.0.1",
                r"sh -c 'echo '\''hi'\'''",
            ]
        );
    }

    #[test]
    fn no_connection_sharing_for_long_paths() {
        assert_eq!(
            control_path(Path::new("/home/u/.compost/ssh")).as_deref(),
            Some("/home/u/.compost/ssh/%C")
        );
        assert_eq!(control_path(&Path::new("/tmp").join("x".repeat(80))), None);
    }
}
