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

#[test]
fn server_must_be_selected() {
    let env = Env::initialized_with_server();
    env.depap(&["server", "state"])
        .code(2)
        .stderr(contains("no server selected"));
}
