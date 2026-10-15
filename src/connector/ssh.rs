//! Runs scripts on a remote server through the system `ssh` client, so the
//! user's ~/.ssh/config, agent and known_hosts apply unchanged.

use std::process::Command;

use anyhow::{Result, bail};

use super::{Connector, Output, log_script, run_process, shell_quote};

/// `ssh` exits with this status when it cannot connect or authenticate.
const SSH_ERROR_STATUS: i32 = 255;

pub struct Ssh {
    pub address: String,
    pub port: u16,
    pub user: String,
    pub identity_file: Option<String>,
    pub verbose: bool,
}

impl Ssh {
    fn args(&self, script: &str) -> Vec<String> {
        let mut args: Vec<String> = [
            "-o", "BatchMode=yes",      // never prompt for passwords; fail instead
            "-o", "ConnectTimeout=10",
            "-p",
        ]
        .map(String::from)
        .into();
        args.push(self.port.to_string());
        if let Some(key) = &self.identity_file {
            args.extend(["-i".to_string(), key.clone()]);
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

    fn run(&self, script: &str, stdin: Option<&[u8]>) -> Result<Output> {
        log_script(self.verbose, &self.target(), script);
        let mut command = Command::new("ssh");
        command.args(self.args(script));
        let output = run_process(command, stdin)?;
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
        Ok(output)
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
            user: "depapuser".into(),
            identity_file: Some("~/.ssh/id_ed25519".into()),
            verbose: false,
        };
        assert_eq!(
            ssh.args("echo 'hi'"),
            [
                "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "-p", "2222",
                "-i", "~/.ssh/id_ed25519", "--", "depapuser@10.0.0.1",
                r"sh -c 'echo '\''hi'\'''",
            ]
        );
    }
}
