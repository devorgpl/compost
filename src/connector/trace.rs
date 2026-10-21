//! `trace` of a server in servers.yaml: every script run on it, appended to
//! `logs/<server>/trace.log` in `COMPOST_HOME`.

use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use super::{Connector, Output};
use crate::config::local::TraceLevel;

/// A bigger log is renamed to `trace.log.1` (replacing the older one) when
/// a command starts.
const MAX_LOG_BYTES: u64 = 10 << 20;
/// At `verbose`, how much of each stream an entry keeps.
const MAX_OUTPUT_BYTES: usize = 64 << 10;

/// Runs scripts with `inner` and records them. Never records stdin, which
/// carries file contents and secrets.
pub struct Traced {
    inner: Box<dyn Connector>,
    level: TraceLevel,
    log: Mutex<Log>,
}

struct Log {
    file: File,
    /// The line naming this compost command, written with the first entry.
    header: Option<String>,
}

/// The log to append to, in a folder only this user can read: verbose
/// entries can hold what the server prints, secrets included.
pub fn open_log(path: &Path) -> Result<File> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

    let dir = path.parent().context("trace log without a folder")?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("cannot create {}", dir.display()))?;
    if fs::metadata(path).is_ok_and(|m| m.len() > MAX_LOG_BYTES) {
        let _ = fs::rename(path, path.with_extension("log.1"));
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("cannot open {}", path.display()))
}

impl Traced {
    pub fn new(inner: Box<dyn Connector>, level: TraceLevel, file: File) -> Self {
        Self {
            inner,
            level,
            log: Mutex::new(Log {
                file,
                header: Some(now()),
            }),
        }
    }

    /// Append one entry in a single write, so entries of compost commands
    /// running at the same time don't mix. A failed write doesn't stop the
    /// command.
    fn write(&self, entry: &str) {
        let Ok(mut log) = self.log.lock() else {
            return;
        };
        let mut text = String::new();
        if let Some(time) = log.header.take() {
            let args: Vec<String> = std::env::args().skip(1).collect();
            let _ = writeln!(
                text,
                "\n{time} {} compost {} ({})",
                std::process::id(),
                args.join(" "),
                self.inner.target()
            );
        }
        text.push_str(entry);
        let _ = log.file.write_all(text.as_bytes());
    }

    /// Before a script runs: its line, written now at `command`.
    fn start(&self, script: &str) -> (String, Instant) {
        let line = command_line(script);
        if self.level == TraceLevel::Command {
            self.write(&line);
        }
        (line, Instant::now())
    }

    /// The entry of a script that ran: its line from [`Traced::start`], then
    /// what the level adds.
    fn record(
        &self,
        (line, started): (String, Instant),
        stdin: Option<usize>,
        result: &Result<Output>,
        stdout: Stdout,
    ) {
        let mut entry = line;
        let took = format_duration(started.elapsed());
        match result {
            Ok(output) => {
                let _ = write!(entry, "  -> exit {} in {took}", output.status);
                if self.level >= TraceLevel::Verbose
                    && let Some(bytes) = stdin
                {
                    let _ = write!(entry, ", stdin {bytes} bytes");
                }
                entry.push('\n');
                if self.level >= TraceLevel::Verbose {
                    match stdout {
                        Stdout::Captured => section(&mut entry, "stdout", &output.stdout),
                        Stdout::Streamed(bytes) => {
                            let _ = writeln!(entry, "  stdout: {bytes} bytes, streamed");
                        }
                        Stdout::Terminal => {
                            entry.push_str("  stdout: on the terminal\n");
                        }
                    }
                }
                if self.level >= TraceLevel::Verbose || !output.success() {
                    section(&mut entry, "stderr", &output.stderr);
                }
            }
            Err(err) => {
                let _ = writeln!(entry, "  -> failed in {took}: {err:#}");
            }
        }
        self.write(&entry);
    }
}

/// Where a script's stdout went.
enum Stdout {
    Captured,
    Streamed(u64),
    Terminal,
}

impl Connector for Traced {
    fn target(&self) -> String {
        self.inner.target()
    }

    fn command(&self, script: &str) -> Command {
        self.inner.command(script)
    }

    fn interactive_command(&self, script: &str, tty: bool) -> Command {
        self.inner.interactive_command(script, tty)
    }

    fn verbose(&self) -> bool {
        self.inner.verbose()
    }

    fn check_connection(&self, output: &Output) -> Result<()> {
        self.inner.check_connection(output)
    }

    fn run(&self, script: &str, stdin: Option<&[u8]>) -> Result<Output> {
        let start = self.start(script);
        let result = self.inner.run(script, stdin);
        if self.level >= TraceLevel::Basic {
            self.record(start, stdin.map(<[u8]>::len), &result, Stdout::Captured);
        }
        result
    }

    fn run_stream(
        &self,
        script: &str,
        stdin: Option<&mut (dyn Read + Send)>,
        stdout: &mut dyn Write,
    ) -> Result<Output> {
        let start = self.start(script);
        let mut counted = Counting {
            inner: stdout,
            bytes: 0,
        };
        let mut stdin_bytes = None;
        let result = match stdin {
            Some(stdin) => {
                let mut reader = CountingReader {
                    inner: stdin,
                    bytes: 0,
                };
                let result = self
                    .inner
                    .run_stream(script, Some(&mut reader), &mut counted);
                stdin_bytes = Some(reader.bytes);
                result
            }
            None => self.inner.run_stream(script, None, &mut counted),
        };
        if self.level >= TraceLevel::Basic {
            let bytes = counted.bytes;
            self.record(start, stdin_bytes, &result, Stdout::Streamed(bytes));
        }
        result
    }

    fn attach(&self, script: &str, tty: bool) -> Result<i32> {
        let start = self.start(script);
        let result = self.inner.attach(script, tty);
        if self.level >= TraceLevel::Basic {
            let output = result.as_ref().map(|status| Output {
                status: *status,
                stdout: String::new(),
                stderr: String::new(),
            });
            let output = output.map_err(|e| anyhow::anyhow!("{e:#}"));
            self.record(start, None, &output, Stdout::Terminal);
        }
        result
    }
}

struct Counting<'a> {
    inner: &'a mut dyn Write,
    bytes: u64,
}

impl Write for Counting<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.bytes += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

struct CountingReader<'a> {
    inner: &'a mut (dyn Read + Send),
    bytes: usize,
}

impl Read for CountingReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buf)?;
        self.bytes += read;
        Ok(read)
    }
}

fn now() -> String {
    chrono::Local::now()
        .format("%Y-%m-%dT%H:%M:%S%.3f%:z")
        .to_string()
}

/// `<time> $ <script>`, its other lines indented.
fn command_line(script: &str) -> String {
    let mut lines = script.trim_end().lines();
    let mut text = format!("{} $ {}\n", now(), lines.next().unwrap_or_default());
    for line in lines {
        let _ = writeln!(text, "    {line}");
    }
    text
}

/// A stream's text, indented, at most [`MAX_OUTPUT_BYTES`] of it.
fn section(entry: &mut String, name: &str, text: &str) {
    let text = text.trim_end();
    if text.is_empty() {
        return;
    }
    let _ = writeln!(entry, "  {name}:");
    let mut end = text.len().min(MAX_OUTPUT_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    for line in text[..end].lines() {
        let _ = writeln!(entry, "    {line}");
    }
    if end < text.len() {
        let _ = writeln!(entry, "    … {} more bytes", text.len() - end);
    }
}

fn format_duration(took: Duration) -> String {
    format!("{:.2}s", took.as_secs_f64())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::local::Local;

    fn traced(level: TraceLevel) -> (tempfile::TempDir, Traced) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs/prod/trace.log");
        let file = open_log(&path).unwrap();
        let traced = Traced::new(Box::new(Local { verbose: false }), level, file);
        (dir, traced)
    }

    fn log(dir: &tempfile::TempDir) -> String {
        fs::read_to_string(dir.path().join("logs/prod/trace.log")).unwrap()
    }

    #[test]
    fn command_records_scripts_only() {
        let (dir, traced) = traced(TraceLevel::Command);
        traced.exec("echo hidden\necho second").unwrap();
        let log = log(&dir);
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines[0], "");
        assert!(lines[1].contains(" compost "), "{log}");
        assert!(lines[2].ends_with(" $ echo hidden"), "{log}");
        assert_eq!(lines[3], "    echo second");
        assert_eq!(lines.len(), 4, "{log}");
    }

    #[test]
    fn basic_adds_status_and_stderr_of_failures() {
        let (dir, traced) = traced(TraceLevel::Basic);
        traced.exec("echo out").unwrap();
        traced.exec("echo oops >&2; exit 3").unwrap_err();
        let log = log(&dir);
        assert!(log.contains("$ echo out\n  -> exit 0 in "), "{log}");
        assert!(!log.contains("    out\n"), "{log}");
        assert!(log.contains("  -> exit 3 in "), "{log}");
        assert!(log.contains("  stderr:\n    oops\n"), "{log}");
        assert_eq!(log.matches(" compost ").count(), 1, "{log}");
    }

    #[test]
    fn verbose_adds_output_but_never_stdin() {
        let (dir, traced) = traced(TraceLevel::Verbose);
        traced
            .write_file(&dir.path().join("f").to_string_lossy(), b"secret", 0o600)
            .unwrap();
        let mut sink = Vec::new();
        traced.stream_out("printf abc", &mut sink).unwrap();
        traced.exec("echo out").unwrap();
        let log = log(&dir);
        assert!(!log.contains("secret"), "{log}");
        assert!(log.contains(", stdin 6 bytes\n"), "{log}");
        assert!(log.contains("  stdout: 3 bytes, streamed\n"), "{log}");
        assert!(log.contains("  stdout:\n    out\n"), "{log}");
        assert_eq!(sink, b"abc");
    }

    #[test]
    fn verbose_keeps_the_start_of_long_output() {
        let mut entry = String::new();
        section(&mut entry, "stdout", &"é".repeat(MAX_OUTPUT_BYTES));
        assert!(
            entry.ends_with(&format!("    … {} more bytes\n", MAX_OUTPUT_BYTES)),
            "{}",
            &entry[entry.len() - 40..]
        );
    }
}
