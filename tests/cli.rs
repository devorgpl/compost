//! End-to-end tests of the depap binary. Each test uses its own DEPAP_HOME.

use std::fs;

use assert_cmd::Command;
use predicates::str::contains;
use tempfile::TempDir;

struct Env {
    home: TempDir,
    work: TempDir,
}

impl Env {
    fn new() -> Self {
        Self {
            home: TempDir::new().unwrap(),
            work: TempDir::new().unwrap(),
        }
    }

    fn depap(&self, args: &[&str]) -> assert_cmd::assert::Assert {
        Command::cargo_bin("depap")
            .unwrap()
            .env("DEPAP_HOME", self.home.path())
            .env_remove("DEPAP_SERVER")
            .current_dir(self.work.path())
            .args(args)
            .assert()
    }

    fn initialized_with_server() -> Self {
        let env = Self::new();
        env.depap(&["init"]).success();
        env.depap(&["server", "define", "prod1", "--address", "10.0.0.1"])
            .success();
        env
    }
}

#[test]
fn init_is_idempotent() {
    let env = Env::new();
    env.depap(&["init"]).success().stdout(contains("Created"));
    env.depap(&["init"])
        .success()
        .stdout(contains("Already initialized"));
}

#[test]
fn commands_require_init() {
    let env = Env::new();
    env.depap(&["server", "list"])
        .code(2)
        .stderr(contains("run `depap init`"));
}

#[test]
fn define_list_connect_disconnect() {
    let env = Env::initialized_with_server();
    env.depap(&["server", "define", "dev", "--local"]).success();
    env.depap(&["server", "define", "prod1", "--address", "10.0.0.2"])
        .code(2)
        .stderr(contains("--force"));

    env.depap(&["connect", "prod1"]).success();
    env.depap(&["server", "list"])
        .success()
        .stdout(contains("* prod1  ssh depapuser@10.0.0.1:22"))
        .stdout(contains("  dev    local"));

    env.depap(&["disconnect"])
        .success()
        .stdout(contains("Disconnected from prod1"));
    env.depap(&["connect", "nope"])
        .code(2)
        .stderr(contains("unknown server `nope`"));
}

#[test]
fn server_deploy_dry_run_validates_merged_files() {
    let env = Env::initialized_with_server();
    fs::write(
        env.work.path().join("depap-server.yaml"),
        "shared:\n  ingress:\n    - {name: caddy-main, type: caddy}\n  database:\n    - {name: postgres16, type: postgres, version: 16}\n",
    )
    .unwrap();
    fs::write(
        env.work.path().join(".depap-server.secrets.yaml"),
        "shared:\n  database:\n    - name: postgres16\n      configuration: {root_password: ''}\n",
    )
    .unwrap();

    // The secrets file is merged by name, so its invalid password is found.
    env.depap(&["-s", "prod1", "server", "deploy", "--dry-run"])
        .code(2)
        .stderr(contains(
            "shared.database[postgres16]: root_password must not be empty",
        ));

    fs::write(
        env.work.path().join(".depap-server.secrets.yaml"),
        "shared:\n  database:\n    - name: postgres16\n      configuration: {root_password: secret}\n",
    )
    .unwrap();
    env.depap(&["-s", "prod1", "server", "deploy", "--dry-run"])
        .success()
        .stdout(contains("database  postgres16"))
        .stdout(contains("ingress   caddy-main"));
}

/// A `docker` stand-in, so state tests do not need Docker. Set
/// FAKE_DOCKER_RUNNING=1 to report containers as running, FAKE_DOCKER_DOWN=1
/// to fail like a missing daemon.
const FAKE_DOCKER: &str = r#"#!/bin/sh
[ -n "$FAKE_DOCKER_DOWN" ] && { echo "Cannot connect to the Docker daemon" >&2; exit 1; }
case "$1" in
  version) echo 29.0.0 ;;
  compose) echo 2.40.0 ;;
  ps) if [ -n "$FAKE_DOCKER_RUNNING" ]; then echo abc123; fi ;;
  *) exit 1 ;;
esac
"#;

/// A local server whose DEPAP_SERVER_HOME is a temp dir, with fake docker on PATH.
struct LocalServer {
    env: Env,
    server_home: std::path::PathBuf,
    path: String,
}

impl LocalServer {
    fn new() -> Self {
        use std::os::unix::fs::PermissionsExt;

        let env = Env::new();
        let bin = env.work.path().join("bin");
        fs::create_dir(&bin).unwrap();
        fs::write(bin.join("docker"), FAKE_DOCKER).unwrap();
        fs::set_permissions(bin.join("docker"), fs::Permissions::from_mode(0o755)).unwrap();
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());

        let server_home = env.work.path().join("server-home");
        env.depap(&["init"]).success();
        env.depap(&[
            "server",
            "define",
            "dev",
            "--local",
            "--server-home",
            server_home.to_str().unwrap(),
        ])
        .success();
        Self { env, server_home, path }
    }

    fn manager_key(&self) -> String {
        let text = fs::read_to_string(self.env.home.path().join("depap-manager.yaml")).unwrap();
        text.trim().trim_start_matches("manager_key:").trim().to_string()
    }

    fn write_server_file(&self, name: &str, contents: &str) {
        fs::create_dir_all(&self.server_home).unwrap();
        fs::write(self.server_home.join(name), contents).unwrap();
    }

    fn state(&self, extra_env: &[(&str, &str)]) -> assert_cmd::assert::Assert {
        let mut command = Command::cargo_bin("depap").unwrap();
        command
            .env("DEPAP_HOME", self.env.home.path())
            .env("PATH", &self.path)
            .env_remove("DEPAP_SERVER")
            .args(["-s", "dev", "server", "state"]);
        for (key, value) in extra_env {
            command.env(key, value);
        }
        command.assert()
    }
}

#[test]
fn state_of_fresh_local_server_is_uninitialized() {
    let server = LocalServer::new();
    server
        .state(&[])
        .success()
        .stdout(contains("dev (local): UNINITIALIZED"))
        .stdout(contains("docker 29.0.0, compose 2.40.0"));
}

#[test]
fn state_without_docker() {
    let server = LocalServer::new();
    server
        .state(&[("FAKE_DOCKER_DOWN", "1")])
        .code(3)
        .stderr(contains("NO_DOCKER"))
        .stderr(contains("Cannot connect to the Docker daemon"));
}

#[test]
fn state_with_other_manager_key_is_foreign() {
    let server = LocalServer::new();
    server.write_server_file("manager_key", "someone-else\n");
    server.state(&[]).code(3).stderr(contains("FOREIGN"));
}

#[test]
fn state_reports_shared_apps() {
    let server = LocalServer::new();
    server.write_server_file("manager_key", &server.manager_key());
    server.write_server_file(
        "depap-server.yaml",
        "shared:\n  database:\n    - {name: postgres16, type: postgres, version: 16}\n",
    );

    server
        .state(&[("FAKE_DOCKER_RUNNING", "1")])
        .success()
        .stdout(contains("dev (local): OK"))
        .stdout(contains("postgres16       postgres 16  running"));
    server
        .state(&[])
        .code(4)
        .stdout(contains("DEGRADED"))
        .stdout(contains("not running"));
}

#[test]
fn state_of_unreachable_ssh_server() {
    let env = Env::new();
    env.depap(&["init"]).success();
    env.depap(&["server", "define", "gone", "--address", "127.0.0.1", "--port", "1"])
        .success();
    env.depap(&["-s", "gone", "server", "state"])
        .code(3)
        .stderr(contains("UNREACHABLE"))
        .stderr(contains("cannot connect"));
}

#[test]
fn server_must_be_selected() {
    let env = Env::initialized_with_server();
    env.depap(&["server", "state"])
        .code(2)
        .stderr(contains("no server selected"));
}
