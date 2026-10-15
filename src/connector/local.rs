//! Runs scripts on this machine with `sh -c`.

use std::process::Command;

use super::Connector;

pub struct Local {
    pub verbose: bool,
}

impl Connector for Local {
    fn target(&self) -> String {
        "local".to_string()
    }

    fn command(&self, script: &str) -> Command {
        let mut command = Command::new("sh");
        command.arg("-c").arg(script);
        command
    }

    fn verbose(&self) -> bool {
        self.verbose
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const LOCAL: Local = Local { verbose: false };

    #[test]
    fn run_reports_status_and_output() {
        let output = LOCAL.run("echo out; echo err >&2; exit 7", None).unwrap();
        assert_eq!(output.status, 7);
        assert_eq!(output.stdout, "out\n");
        assert_eq!(output.stderr, "err\n");
    }

    #[test]
    fn exec_fails_on_non_zero_status() {
        let err = LOCAL.exec("echo broken >&2; exit 3").unwrap_err();
        assert!(err.to_string().contains("exit status 3"), "{err}");
        assert!(err.to_string().contains("broken"), "{err}");
    }

    #[test]
    fn run_passes_stdin() {
        let output = LOCAL.run("tr a-z A-Z", Some(b"hello")).unwrap();
        assert_eq!(output.stdout, "HELLO");
    }

    #[test]
    fn write_then_read_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("sub dir/secret.env");
        let path = path.to_str().unwrap();

        assert_eq!(LOCAL.read_file(path).unwrap(), None);
        LOCAL.write_file(path, b"PASSWORD=x\n", 0o600).unwrap();
        assert_eq!(
            LOCAL.read_file(path).unwrap().as_deref(),
            Some("PASSWORD=x\n")
        );

        let mode = LOCAL.exec(&format!("stat -c %a '{path}'")).unwrap();
        assert_eq!(mode.trim(), "600");
    }

    #[test]
    fn resolve_path_expands_tilde() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(
            LOCAL.resolve_path("~/.compost-server").unwrap(),
            format!("{home}/.compost-server")
        );
        assert_eq!(LOCAL.resolve_path("/srv/compost").unwrap(), "/srv/compost");
        assert_eq!(LOCAL.resolve_path("~other/x").unwrap(), "~other/x");
    }
}
