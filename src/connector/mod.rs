//! How compost runs commands on a server: locally or over SSH (spec §10.1, §14.2).
//!
//! A connector only has to say which local process runs a script
//! ([`Connector::command`]). Everything else (checked execution, streaming,
//! reading and writing files) is built on top of it as default methods, so
//! every connector behaves the same way.

pub mod dry_run;
pub mod local;
mod ssh;
mod trace;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::config::local::{
    CompostHome, ConnectorConfig, DEFAULT_SSH_PORT, DEFAULT_SSH_USER, ServerEntry,
};
use crate::error::config_err;

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
    /// Where commands run, for messages, e.g. `ssh compostuser@10.0.0.1:22`.
    fn target(&self) -> String;

    /// The local process that runs a POSIX `sh` script on the server.
    fn command(&self, script: &str) -> Command;

    /// Like [`Connector::command`], for a script attached to this terminal;
    /// `tty` asks for a terminal on the server.
    fn interactive_command(&self, script: &str, _tty: bool) -> Command {
        self.command(script)
    }

    /// Print every script before it runs.
    fn verbose(&self) -> bool;

    /// An error when the script did not run because the connection failed.
    fn check_connection(&self, _output: &Output) -> Result<()> {
        Ok(())
    }

    /// Run a POSIX `sh` script on the server and capture its output.
    ///
    /// Returns an error only when the script could not run at all (e.g. the
    /// server is unreachable). A non-zero exit status is reported in [`Output`].
    fn run(&self, script: &str, stdin: Option<&[u8]>) -> Result<Output> {
        log_script(self.verbose(), &self.target(), script);
        let output = run_process(self.command(script), stdin)?;
        self.check_connection(&output)?;
        Ok(output)
    }

    /// Like [`Connector::run`], but stdin and stdout are streams, so binary
    /// data of any size (dumps, archives) passes through without being held in
    /// memory. `Output::stdout` stays empty.
    fn run_stream(
        &self,
        script: &str,
        stdin: Option<&mut (dyn Read + Send)>,
        stdout: &mut dyn Write,
    ) -> Result<Output> {
        log_script(self.verbose(), &self.target(), script);
        let output = run_process_stream(self.command(script), stdin, stdout)?;
        self.check_connection(&output)?;
        Ok(output)
    }

    /// Run a script with this terminal's stdin, stdout and stderr, e.g. to
    /// follow logs, and return its exit status.
    fn attach(&self, script: &str, tty: bool) -> Result<i32> {
        log_script(self.verbose(), &self.target(), script);
        crate::interrupt::check()?;
        let mut command = self.interactive_command(script, tty);
        let program = command.get_program().to_string_lossy().into_owned();
        let status = command
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .with_context(|| format!("cannot start `{program}`"))?;
        let status = status.code().unwrap_or(-1);
        self.check_connection(&Output {
            status,
            stdout: String::new(),
            stderr: "see the message above".to_string(),
        })?;
        Ok(status)
    }

    /// Write the script's stdout to `stdout`; a non-zero exit status is an error.
    fn stream_out(&self, script: &str, stdout: &mut dyn Write) -> Result<()> {
        let output = self.run_stream(script, None, stdout)?;
        if !output.success() {
            return Err(command_failed(script, &output));
        }
        Ok(())
    }

    /// Feed `stdin` to the script; a non-zero exit status is an error.
    fn stream_in(&self, script: &str, stdin: &mut (dyn Read + Send)) -> Result<()> {
        let output = self.run_stream(script, Some(stdin), &mut std::io::sink())?;
        if !output.success() {
            return Err(command_failed(script, &output));
        }
        Ok(())
    }

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

/// How this run connects, from the global command-line options.
pub struct ConnectOptions {
    /// Print every script before it runs.
    pub verbose: bool,
    /// Replaces the server's `identity_file` (`--ssh-key-file`).
    pub ssh_key_file: Option<PathBuf>,
}

/// The connector configured for a server in servers.yaml, recording what
/// it runs when the server has `trace`.
pub fn open(
    server: &ServerEntry,
    home: &CompostHome,
    options: &ConnectOptions,
) -> Result<Box<dyn Connector>> {
    let connector = connect(server, home, options)?;
    if server.trace.is_none() {
        return Ok(connector);
    }
    // A log that can't be written doesn't stop the command.
    Ok(match trace::open_log(&home.trace_log(&server.name)) {
        Ok(file) => Box::new(trace::Traced::new(connector, server.trace, file)),
        Err(err) => {
            eprintln!("warning: not tracing {}: {err:#}", server.name);
            connector
        }
    })
}

fn connect(
    server: &ServerEntry,
    home: &CompostHome,
    options: &ConnectOptions,
) -> Result<Box<dyn Connector>> {
    let verbose = options.verbose;
    Ok(match &server.connector {
        ConnectorConfig::Local => {
            if options.ssh_key_file.is_some() {
                return Err(config_err(format!(
                    "--ssh-key-file only applies to SSH servers; {} is local",
                    server.name
                )));
            }
            Box::new(local::Local { verbose })
        }
        ConnectorConfig::Ssh {
            address,
            port,
            user,
            identity_file,
        } => Box::new(ssh::Ssh {
            address: address.clone(),
            port: port.unwrap_or(DEFAULT_SSH_PORT),
            user: user.clone().unwrap_or_else(|| DEFAULT_SSH_USER.to_string()),
            identity_file: ssh_key(options.ssh_key_file.as_deref(), identity_file.as_deref())?,
            control_path: ssh::control_path(&home.ssh_control_dir()?),
            verbose,
        }),
    })
}

/// The key to sign in with: `--ssh-key-file`, else the server's `identity_file`.
///
/// ssh only warns about a missing `-i` file and signs in with other keys, so
/// a key given for this run must exist.
fn ssh_key(override_key: Option<&Path>, identity_file: Option<&str>) -> Result<Option<String>> {
    let Some(key) = override_key else {
        return Ok(identity_file.map(String::from));
    };
    // `--ssh-key-file=~/key` reaches compost without the shell expanding `~`.
    let key = match (key.strip_prefix("~"), dirs::home_dir()) {
        (Ok(rest), Some(home)) => home.join(rest),
        _ => key.to_path_buf(),
    };
    if !key.is_file() {
        return Err(config_err(format!(
            "SSH key file {} does not exist",
            key.display()
        )));
    }
    let key = key
        .to_str()
        .ok_or_else(|| config_err(format!("SSH key file {} is not UTF-8", key.display())))?;
    Ok(Some(key.to_string()))
}

/// Quote a string so `sh` treats it as one literal word.
pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Spawn a local process, feed it `stdin` and collect its output.
fn run_process(mut command: Command, stdin: Option<&[u8]>) -> Result<Output> {
    crate::interrupt::check()?;
    command
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
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

/// Like [`run_process`], with streams. Stdin is written and stderr collected
/// on helper threads while this one copies stdout, so no pipe can fill up.
fn run_process_stream(
    mut command: Command,
    stdin: Option<&mut (dyn Read + Send)>,
    stdout: &mut dyn Write,
) -> Result<Output> {
    crate::interrupt::check()?;
    command
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let program = command.get_program().to_string_lossy().into_owned();
    let mut child = command
        .spawn()
        .with_context(|| format!("cannot start `{program}`"))?;
    let mut child_out = child.stdout.take().context("no stdout pipe")?;
    let mut child_err = child.stderr.take().context("no stderr pipe")?;

    let (copied, stderr) = std::thread::scope(|scope| {
        if let (Some(data), Some(mut pipe)) = (stdin, child.stdin.take()) {
            // A write error means the process exited early; its status tells why.
            scope.spawn(move || std::io::copy(data, &mut pipe));
        }
        let stderr = scope.spawn(move || {
            let mut text = Vec::new();
            let _ = child_err.read_to_end(&mut text);
            text
        });
        let copied = std::io::copy(&mut child_out, stdout);
        if copied.is_err() {
            // Nobody reads the output any more; stop the process.
            let _ = child.kill();
        }
        (copied, stderr.join().unwrap_or_default())
    });
    let status = child
        .wait()
        .with_context(|| format!("`{program}` failed"))?;
    crate::interrupt::check()?;
    copied.with_context(|| format!("cannot write the output of `{program}`"))?;
    stdout.flush()?;

    Ok(Output {
        status: status.code().unwrap_or(-1),
        stdout: String::new(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
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

    #[test]
    fn streams_binary_data_both_ways() {
        let connector = local::Local { verbose: false };
        let data: Vec<u8> = (0..3_000_000u32).map(|i| (i * 7 % 256) as u8).collect();
        let mut echoed = Vec::new();
        let output = connector
            .run_stream(
                "cat; echo done >&2",
                Some(&mut data.as_slice()),
                &mut echoed,
            )
            .unwrap();
        assert!(output.success());
        assert_eq!(output.stderr, "done\n");
        assert!(echoed == data, "binary data changed on the way");

        let err = connector
            .stream_out("echo partial; exit 4", &mut Vec::new())
            .unwrap_err();
        assert!(err.to_string().contains("exit status 4"), "{err}");
    }

    #[test]
    fn ssh_key_file_replaces_identity_file() {
        assert_eq!(
            ssh_key(None, Some("~/.ssh/a")).unwrap().as_deref(),
            Some("~/.ssh/a")
        );
        assert_eq!(ssh_key(None, None).unwrap(), None);
        let dir = tempfile::TempDir::new().unwrap();
        let key = dir.path().join("id_rsa");
        std::fs::write(&key, "").unwrap();
        assert_eq!(
            ssh_key(Some(&key), Some("~/.ssh/a")).unwrap().as_deref(),
            key.to_str()
        );
        let err = ssh_key(Some(&dir.path().join("missing")), None).unwrap_err();
        assert!(err.to_string().contains("does not exist"), "{err}");
    }
}
