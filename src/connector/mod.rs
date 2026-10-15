//! How depap runs commands on a server: locally or over SSH (spec §10.1, §14.2).
//!
//! A connector only has to implement [`Connector::run`]. Everything else
//! (checked execution, reading and writing files) is built on top of it as
//! default methods, so every connector behaves the same way.

pub mod local;
mod ssh;

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::config::local::{
    ConnectorConfig, DEFAULT_SSH_PORT, DEFAULT_SSH_USER, DepapHome, ServerEntry,
};

/// Exit status used by `read_file` to signal "file does not exist".
const MISSING_FILE_STATUS: i32 = 100;

/// Result of a script that ran to completion (with any exit status).
#[derive(Debug)]
pub struct Output {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn success(&self) -> bool {
        self.status == 0
    }
}

pub trait Connector {
    /// Where commands run, for messages, e.g. `ssh depapuser@10.0.0.1:22`.
    fn target(&self) -> String;

    /// Run a POSIX `sh` script on the server and capture its output.
    ///
    /// Returns an error only when the script could not run at all (e.g. the
    /// server is unreachable). A non-zero exit status is reported in [`Output`].
    fn run(&self, script: &str, stdin: Option<&[u8]>) -> Result<Output>;

    /// Run a script and return its stdout; a non-zero exit status is an error.
    fn exec(&self, script: &str) -> Result<String> {
        let output = self.run(script, None)?;
        if !output.success() {
            return Err(command_failed(script, &output));
        }
        Ok(output.stdout)
    }

    /// Contents of a file, or `None` if it does not exist.
    fn read_file(&self, path: &str) -> Result<Option<String>> {
        let path = shell_quote(path);
        let script = format!("[ -e {path} ] || exit {MISSING_FILE_STATUS}; cat -- {path}");
        let output = self.run(&script, None)?;
        match output.status {
            0 => Ok(Some(output.stdout)),
            MISSING_FILE_STATUS => Ok(None),
            _ => Err(command_failed(&script, &output)),
        }
    }

    /// Create or replace a file atomically with the given permissions (e.g. `0o600`).
    /// Parent directories are created with mode 0700. Contents go through stdin,
    /// so secrets never appear on a command line.
    fn write_file(&self, path: &str, contents: &[u8], mode: u32) -> Result<()> {
        let path = shell_quote(path);
        let script = format!(
            "umask 077 && mkdir -p -- \"$(dirname -- {path})\" \
             && cat > {path}.tmp && chmod {mode:o} {path}.tmp && mv -f -- {path}.tmp {path}"
        );
        let output = self.run(&script, Some(contents))?;
        if !output.success() {
            return Err(command_failed(&script, &output));
        }
        Ok(())
    }

    /// Names of the entries in a directory (empty if it does not exist).
    fn list_dir(&self, path: &str) -> Result<Vec<String>> {
        let path = shell_quote(path);
        let output = self.exec(&format!("[ -d {path} ] || exit 0; ls -1A -- {path}"))?;
        Ok(output.lines().map(str::to_string).collect())
    }

    /// Replace a directory with the contents of an uncompressed tar archive.
    /// The new contents are unpacked next to it first, so a failed upload
    /// leaves the old directory in place.
    fn replace_dir(&self, path: &str, tar: &[u8]) -> Result<()> {
        let new = shell_quote(&format!("{path}.new"));
        let path = shell_quote(path);
        let script = format!(
            "rm -rf -- {new} && mkdir -p -- {new} && tar -x -f - -C {new} \
             && rm -rf -- {path} && mv -- {new} {path}"
        );
        let output = self.run(&script, Some(tar))?;
        if !output.success() {
            return Err(command_failed(&script, &output));
        }
        Ok(())
    }

    /// Expand a leading `~` using the server's `$HOME`.
    fn resolve_path(&self, path: &str) -> Result<String> {
        let rest = match path.strip_prefix('~') {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => rest,
            _ => return Ok(path.to_string()),
        };
        let home = self.exec("printf '%s' \"$HOME\"")?;
        if home.is_empty() {
            bail!("$HOME is not set on {}", self.target());
        }
        Ok(format!("{home}{rest}"))
    }
}

/// The connector configured for a server in servers.yaml.
pub fn open(server: &ServerEntry, home: &DepapHome, verbose: bool) -> Result<Box<dyn Connector>> {
    Ok(match &server.connector {
        ConnectorConfig::Local => Box::new(local::Local { verbose }),
        ConnectorConfig::Ssh {
            address,
            port,
            user,
            identity_file,
        } => Box::new(ssh::Ssh {
            address: address.clone(),
            port: port.unwrap_or(DEFAULT_SSH_PORT),
            user: user.clone().unwrap_or_else(|| DEFAULT_SSH_USER.to_string()),
            identity_file: identity_file.clone(),
            control_path: ssh::control_path(&home.ssh_control_dir()?),
            verbose,
        }),
    })
}

/// Quote a string so `sh` treats it as one literal word.
pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Spawn a local process, feed it `stdin` and collect its output.
fn run_process(mut command: Command, stdin: Option<&[u8]>) -> Result<Output> {
    crate::interrupt::check()?;
    command
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let program = command.get_program().to_string_lossy().into_owned();
    let mut child = command
        .spawn()
        .with_context(|| format!("cannot start `{program}`"))?;

    // Write stdin from a second thread while this one reads the output, so a
    // process producing a lot of output cannot block us (and itself).
    let output = std::thread::scope(|scope| {
        if let (Some(data), Some(mut pipe)) = (stdin, child.stdin.take()) {
            // A write error means the process exited early; its status tells why.
            scope.spawn(move || pipe.write_all(data));
        }
        child.wait_with_output()
    })
    .with_context(|| format!("`{program}` failed"))?;
    // Ctrl-C reached the process too; its result is not meaningful.
    crate::interrupt::check()?;

    Ok(Output {
        status: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

fn log_script(verbose: bool, target: &str, script: &str) {
    if verbose {
        eprintln!("[{target}] $ {script}");
    }
}

fn command_failed(script: &str, output: &Output) -> anyhow::Error {
    let first_line = script.lines().next().unwrap_or_default();
    anyhow::anyhow!(
        "command failed with exit status {}: {first_line}\n{}",
        output.status,
        output.stderr.trim_end()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_survives_sh() {
        let tricky = "it's $HOME `x` \"y\"";
        let connector = local::Local { verbose: false };
        let out = connector
            .exec(&format!("printf '%s' {}", shell_quote(tricky)))
            .unwrap();
        assert_eq!(out, tricky);
    }
}
