//! End-to-end tests of the depap binary. Each test uses its own DEPAP_HOME.

use std::fs;

use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
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
fn init_with_existing_key() {
    let key = "0B9A2E5C-7F1D-4C3B-9A8E-6D5F4C3B2A10";
    let normalized = "0b9a2e5c-7f1d-4c3b-9a8e-6d5f4c3b2a10";
    let env = Env::new();
    env.depap(&["init", "--key", "not-a-key"])
        .code(2)
        .stderr(contains("is not a manager key"));
    env.depap(&["init", "--key", key])
        .success()
        .stdout(contains(format!("manager key {normalized}")));
    env.depap(&["init", "--key", normalized])
        .success()
        .stdout(contains("Already initialized"));

    let other = "11111111-2222-4333-8444-555555555555";
    env.depap(&["init", "--key", other])
        .code(2)
        .stderr(contains("add --force"));
    env.depap(&["init", "--key", other, "--force"])
        .success()
        .stdout(contains(format!("manager key {other}")));
    env.depap(&["init"]).success().stdout(contains(format!(
        "Already initialized (manager key {other})"
    )));
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

/// A `docker` stand-in, so tests do not need Docker. Every call is appended to
/// $FAKE_DOCKER_LOG. Set FAKE_DOCKER_RUNNING=1 to report containers as running,
/// FAKE_DOCKER_DOWN=1 to fail like a missing daemon.
const FAKE_DOCKER: &str = r#"#!/bin/sh
echo "docker $*" >> "$FAKE_DOCKER_LOG"
[ -n "$FAKE_DOCKER_DOWN" ] && { echo "Cannot connect to the Docker daemon" >&2; exit 1; }
case "$*" in
  "version "*) echo 29.0.0 ;;
  "compose version"*) echo 2.40.0 ;;
  "compose "*" ps "*) echo "app running 0 healthy" ;;
  "compose "*" exec "*) cat >> "$FAKE_DOCKER_LOG" ;;
  "compose "*" up "*) if [ -n "$FAKE_DOCKER_SLOW_UP" ]; then sleep 10; fi ;;
  "compose "*) ;;
  "network inspect "*) exit 1 ;;
  "network create "*) ;;
  "ps "*) if [ -n "$FAKE_DOCKER_RUNNING" ]; then echo abc123; fi ;;
  *) echo "fake docker: unexpected call" >&2; exit 1 ;;
esac
"#;

const SERVER_CONFIG: &str = "\
shared:
  ingress:
    - {name: caddy-main, type: caddy}
  database:
    - {name: postgres16, type: postgres, version: 16}
";

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
        Self {
            env,
            server_home,
            path,
        }
    }

    fn manager_key(&self) -> String {
        let text = fs::read_to_string(self.env.home.path().join("depap-manager.yaml")).unwrap();
        text.trim()
            .trim_start_matches("manager_key:")
            .trim()
            .to_string()
    }

    fn write_server_file(&self, name: &str, contents: &str) {
        let path = self.server_home.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn read_server_file(&self, name: &str) -> String {
        fs::read_to_string(self.server_home.join(name)).unwrap()
    }

    fn write_work_file(&self, name: &str, contents: &str) {
        let path = self.env.work.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    /// Calls made to the fake docker so far; clears the log.
    fn take_docker_log(&self) -> String {
        let path = self.env.work.path().join("docker.log");
        let log = fs::read_to_string(&path).unwrap_or_default();
        let _ = fs::remove_file(path);
        log
    }

    /// Run `depap -s dev <args>` in the work directory.
    fn run(&self, args: &[&str], extra_env: &[(&str, &str)]) -> assert_cmd::assert::Assert {
        let mut command = Command::cargo_bin("depap").unwrap();
        command
            .env("DEPAP_HOME", self.env.home.path())
            .env("PATH", &self.path)
            .env("FAKE_DOCKER_LOG", self.env.work.path().join("docker.log"))
            .env_remove("DEPAP_SERVER")
            .current_dir(self.env.work.path())
            .args(["-s", "dev"])
            .args(args);
        for (key, value) in extra_env {
            command.env(key, value);
        }
        command.assert()
    }

    fn state(&self, extra_env: &[(&str, &str)]) -> assert_cmd::assert::Assert {
        self.run(&["server", "state"], extra_env)
    }
}

#[test]
fn server_deploy_validates_merged_files() {
    let server = LocalServer::new();
    server.write_work_file("depap-server.yaml", SERVER_CONFIG);
    server.write_work_file(
        ".depap-server.secrets.yaml",
        "shared:\n  database:\n    - name: postgres16\n      configuration: {root_password: ''}\n",
    );

    // The secrets file is merged by name, so its invalid password is found.
    server
        .run(&["server", "deploy", "--dry-run"], &[])
        .code(2)
        .stderr(contains(
            "shared.database[postgres16]: root_password must not be empty",
        ));
}

#[test]
fn server_deploy_dry_run_changes_nothing() {
    let server = LocalServer::new();
    server.write_work_file("depap-server.yaml", SERVER_CONFIG);
    server
        .run(&["server", "deploy", "--dry-run", "--apply"], &[])
        .success()
        .stdout(contains("Dry run"))
        .stdout(contains("networks: depap-db-postgres16, depap-ingress"))
        .stdout(contains(
            "shared/database/postgres16/secrets/postgres.env  600  (secret)",
        ));
    assert!(!server.server_home.exists());
}

#[test]
fn server_deploy_then_apply() {
    use std::os::unix::fs::PermissionsExt;

    let server = LocalServer::new();
    server.write_work_file("depap-server.yaml", SERVER_CONFIG);
    server.write_work_file(
        ".depap-server.secrets.yaml",
        "shared:\n  database:\n    - name: postgres16\n      configuration: {root_password: s3cret}\n",
    );

    // Render and upload only.
    server
        .run(&["server", "deploy"], &[])
        .success()
        .stdout(contains("Initialized"))
        .stdout(contains("database  postgres16       updated"))
        .stdout(contains("Run again with --apply"));
    let log = server.take_docker_log();
    assert!(log.contains("docker network create depap-ingress"), "{log}");
    assert!(
        log.contains("docker network create depap-db-postgres16"),
        "{log}"
    );
    assert!(!log.contains(" up -d"), "{log}");

    assert_eq!(
        server.read_server_file("manager_key").trim(),
        server.manager_key()
    );
    assert!(
        !server.server_home.join("lock").exists(),
        "lock is released"
    );
    let secret = server
        .server_home
        .join("shared/database/postgres16/secrets/postgres.env");
    assert_eq!(
        fs::read_to_string(&secret).unwrap(),
        "POSTGRES_PASSWORD='s3cret'\n"
    );
    assert_eq!(
        fs::metadata(&secret).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let stored_config = server.read_server_file("depap-server.yaml");
    assert!(!stored_config.contains("s3cret"), "{stored_config}");
    assert!(
        server
            .read_server_file("state.yaml")
            .contains("status: candidate")
    );

    // Start: candidates are (re)created.
    let running = [("FAKE_DOCKER_RUNNING", "1")];
    server
        .run(&["server", "deploy", "--apply"], &running)
        .success()
        .stdout(contains("unchanged"))
        .stdout(contains("starting database postgres16 ... running"))
        .stdout(contains("dev (local): OK"));
    let log = server.take_docker_log();
    assert!(
        log.contains(
            "docker compose -p depap-database-postgres16 up -d --quiet-pull --force-recreate"
        ),
        "{log}"
    );
    let state = server.read_server_file("state.yaml");
    assert!(
        !state.contains("candidate") && state.contains("status: deployed"),
        "{state}"
    );

    // Nothing changed: containers are left alone.
    server
        .run(&["server", "deploy", "--apply"], &running)
        .success();
    let log = server.take_docker_log();
    assert!(log.contains("up -d --quiet-pull\n"), "{log}");
    assert!(!log.contains("--force-recreate"), "{log}");
}

#[test]
fn server_deploy_respects_lock() {
    let server = LocalServer::new();
    server.write_work_file("depap-server.yaml", SERVER_CONFIG);
    server.run(&["server", "deploy"], &[]).success();
    server.write_server_file("lock/holder", "host=other pid=1\n");

    server
        .run(&["server", "deploy"], &[])
        .code(3)
        .stderr(contains(
            "locked by another depap command (host=other pid=1)",
        ));
    server
        .run(&["server", "deploy", "--force-unlock"], &[])
        .success();
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
    env.depap(&[
        "server",
        "define",
        "gone",
        "--address",
        "127.0.0.1",
        "--port",
        "1",
    ])
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

const APP_CONFIG: &str = "\
name: myapp
deployment:
  build: true
services:
  database:
    - name: system-data
      type: postgres
      version: '>=16'
      targets: [backend]
      environment:
        DATABASE_URL: db_url
        DATABASE_PASSWORD: db_password
  ingress:
    - name: ui-front
      routes:
        - {name: ui, target: webapp, port: 8080, path: /ui, strip_prefix: true}
        - {name: api, target: backend, port: 5000}
    - name: docs
      optional: true
      routes:
        - {name: docs, target: webapp, port: 8080}
app_services:
  - name: webapp
    service: front-webapp
  - name: backend
    service: backend-webapp
    environment: {APP_MODE: production, PRICE: $5}
";

const APP_OVERLAY: &str = "\
prefix: staging
services:
  database:
    - name: system-data
      db_password: pa$$word
  ingress:
    - name: ui-front
      domains: [staging.example.com]
app_services:
  - name: backend
    environment: {APP_MODE: staging}
";

/// A server with caddy and postgres deployed and running.
fn server_with_shared_apps() -> LocalServer {
    let server = LocalServer::new();
    server.write_work_file("depap-server.yaml", SERVER_CONFIG);
    server
        .run(
            &["server", "deploy", "--apply"],
            &[("FAKE_DOCKER_RUNNING", "1")],
        )
        .success();
    server.take_docker_log();
    server
}

#[test]
fn app_deploy_needs_deployed_shared_apps() {
    let server = LocalServer::new();
    server.write_work_file("app/depap.yaml", APP_CONFIG);
    server.write_work_file(
        "app/docker-compose.yaml",
        "services: {front-webapp: {build: .}, backend-webapp: {build: .}}",
    );
    server.write_work_file("app/.depap-staging.yaml", APP_OVERLAY);
    server
        .run(
            &[
                "app",
                "deploy",
                "-f",
                "app/depap.yaml,app/.depap-staging.yaml",
            ],
            &[],
        )
        .code(3)
        .stderr(contains("run `depap server deploy --apply` first"));
}

#[test]
fn app_deploy_end_to_end() {
    let server = server_with_shared_apps();
    server.write_work_file("app/depap.yaml", APP_CONFIG);
    server.write_work_file(
        "app/docker-compose.yaml",
        "services: {front-webapp: {build: .}, backend-webapp: {build: ., networks: [internal]}}\nnetworks: {internal: {}}\n",
    );
    server.write_work_file("app/.depap-staging.yaml", APP_OVERLAY);
    server.write_work_file("app/Dockerfile", "FROM scratch\n");
    server.write_work_file("app/.git/HEAD", "ref\n");
    let running = [("FAKE_DOCKER_RUNNING", "1")];
    let files = "app/depap.yaml,app/.depap-staging.yaml";

    server
        .run(&["app", "deploy", "--apply", "-f", files], &running)
        .success()
        .stdout(contains("Deploying myapp-staging to dev"))
        .stdout(contains("services.ingress[docs]: skipped (optional and no domains given)"))
        .stdout(contains("uploaded 2 files"))
        .stdout(contains("myapp-staging: deployed ("))
        .stdout(contains("database  system-data      postgres16: database myapp_staging_system_data, user myapp_staging_system_data"))
        .stdout(contains("ingress   ui-front         caddy-main: staging.example.com  /ui → myapp-staging-front-webapp:8080, / → myapp-staging-backend-webapp:5000"));

    // Sources: no .git and no config files (the overlay holds secrets).
    let instance = server.server_home.join("apps/myapp/staging");
    let mut uploaded: Vec<_> = fs::read_dir(instance.join("sources"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    uploaded.sort();
    assert_eq!(uploaded, ["Dockerfile", "docker-compose.yaml"]);

    // Compose override: networks, aliases and escaped variables.
    let override_yaml = fs::read_to_string(instance.join("compose/depap.override.yaml")).unwrap();
    for expected in [
        "aliases:\n        - myapp-staging-front-webapp",
        "default: {}",
        "depap-db-postgres16: {}",
        "APP_MODE: staging",
        "PRICE: $$5",
        "DATABASE_PASSWORD: pa$$$$word",
        "DATABASE_URL: postgres://myapp_staging_system_data:pa%24%24word@postgres16:5432/myapp_staging_system_data",
    ] {
        assert!(
            override_yaml.contains(expected),
            "missing {expected:?} in\n{override_yaml}"
        );
    }
    // backend-webapp has its own networks, so `default` is not added to it.
    let backend = override_yaml.split("backend-webapp:").nth(1).unwrap();
    assert!(
        !backend
            .split("front-webapp:")
            .next()
            .unwrap()
            .contains("default"),
        "{override_yaml}"
    );

    let effective = fs::read_to_string(instance.join("depap.yaml")).unwrap();
    assert!(!effective.contains("pa$$word"), "{effective}");
    assert!(
        fs::read_to_string(instance.join("depap-env.yaml"))
            .unwrap()
            .contains("instance: postgres16")
    );

    // Database created before the app started, Caddy reloaded after.
    let log = server.take_docker_log();
    let position = |needle: &str| {
        log.find(needle)
            .unwrap_or_else(|| panic!("{needle:?} not in\n{log}"))
    };
    let created =
        position("ALTER ROLE \"myapp_staging_system_data\" WITH LOGIN PASSWORD 'pa$$word'");
    let built = position(
        "--project-directory sources -f sources/docker-compose.yaml -f compose/depap.override.yaml build -q",
    );
    let started = position("up -d --remove-orphans");
    let reloaded = position("exec -T caddy caddy reload");
    assert!(
        created < built && built < started && started < reloaded,
        "{log}"
    );

    // Caddy site: longest path first, `/` as fallback.
    let site = fs::read_to_string(
        server
            .server_home
            .join("shared/ingress/caddy-main/conf/sites/myapp-staging-ui-front.caddy"),
    )
    .unwrap();
    assert!(site.contains("staging.example.com {"), "{site}");
    assert!(site.contains("@ui path /ui /ui/*\n\thandle @ui {\n\t\turi strip_prefix /ui\n\t\treverse_proxy myapp-staging-front-webapp:8080"), "{site}");
    assert!(
        site.contains("\thandle {\n\t\treverse_proxy myapp-staging-backend-webapp:5000"),
        "{site}"
    );

    assert!(
        server
            .read_server_file("state.yaml")
            .contains("prefix: staging\n  status: deployed")
    );
}

#[test]
fn app_deploy_rejects_domain_used_by_another_instance() {
    let server = server_with_shared_apps();
    server.write_work_file("app/depap.yaml", APP_CONFIG);
    server.write_work_file(
        "app/docker-compose.yaml",
        "services: {front-webapp: {build: .}, backend-webapp: {build: .}}",
    );
    server.write_work_file("app/.depap-staging.yaml", APP_OVERLAY);
    server.write_work_file(
        "app/.depap-other.yaml",
        &APP_OVERLAY
            .replace("staging", "other")
            .replace("other.example", "staging.example"),
    );
    let running = [("FAKE_DOCKER_RUNNING", "1")];

    server
        .run(
            &[
                "app",
                "deploy",
                "-f",
                "app/depap.yaml,app/.depap-staging.yaml",
            ],
            &running,
        )
        .success();
    server
        .run(
            &[
                "app",
                "deploy",
                "-f",
                "app/depap.yaml,app/.depap-other.yaml",
            ],
            &running,
        )
        .code(3)
        .stderr(contains(
            "domain `staging.example.com` is already used by myapp/staging (ui-front)",
        ));
}

/// A server with myapp-staging deployed; returns the server.
fn server_with_app() -> LocalServer {
    let server = server_with_shared_apps();
    server.write_work_file("app/depap.yaml", APP_CONFIG);
    server.write_work_file(
        "app/docker-compose.yaml",
        "services: {front-webapp: {build: .}, backend-webapp: {build: .}}",
    );
    server.write_work_file("app/.depap-staging.yaml", APP_OVERLAY);
    server
        .run(
            &[
                "app",
                "deploy",
                "--apply",
                "-f",
                "app/depap.yaml,app/.depap-staging.yaml",
            ],
            &[("FAKE_DOCKER_RUNNING", "1")],
        )
        .success();
    server.take_docker_log();
    server
}

#[test]
fn app_state_by_name_or_files() {
    let server = server_with_app();
    let running = [("FAKE_DOCKER_RUNNING", "1")];
    server
        .run(
            &["app", "state", "--app", "myapp", "--prefix", "staging"],
            &running,
        )
        .success()
        .stdout(contains("myapp-staging: deployed"))
        .stdout(contains("app  running healthy"))
        .stdout(contains("database  system-data"));
    server
        .run(
            &[
                "app",
                "state",
                "-f",
                "app/depap.yaml,app/.depap-staging.yaml",
            ],
            &running,
        )
        .success();
    // Without the overlay the prefix is `main`, which is not deployed.
    server
        .run(&["app", "state", "-f", "app/depap.yaml"], &running)
        .code(3)
        .stderr(contains("myapp-main is not deployed"));
    // `--prefix` replaces the prefix from the files.
    server
        .run(
            &[
                "app",
                "state",
                "-f",
                "app/depap.yaml",
                "--prefix",
                "staging",
            ],
            &running,
        )
        .success()
        .stdout(contains("myapp-staging: deployed"));
}

#[test]
fn app_state_and_remove_by_instance_name() {
    let server = server_with_app();
    let running = [("FAKE_DOCKER_RUNNING", "1")];
    server
        .run(&["app", "state", "myapp-staging"], &running)
        .success()
        .stdout(contains("myapp-staging: deployed"));
    server
        .run(&["app", "state", "myapp-prod"], &running)
        .code(3)
        .stderr(contains("myapp-prod is not deployed on this server"));
    server
        .run(
            &["app", "state", "myapp-staging", "--app", "myapp"],
            &running,
        )
        .code(2);

    // Both parts may contain `-`, so `a-b-c` can be a-b/c or a/b-c.
    let state = server.read_server_file("state.yaml");
    server.write_server_file(
        "state.yaml",
        &state.replace(
            "apps:\n",
            "apps:\n\
             - {name: a-b, prefix: c, status: deployed, updated_at: x}\n\
             - {name: a, prefix: b-c, status: deployed, updated_at: x}\n",
        ),
    );
    server
        .run(&["app", "state", "a-b-c"], &running)
        .code(2)
        .stderr(contains(
            "`a-b-c` is ambiguous; use --app and --prefix (--app a-b --prefix c or --app a --prefix b-c)",
        ));

    server
        .run(&["app", "remove", "myapp-staging", "--yes"], &running)
        .success()
        .stdout(contains("Removed myapp-staging."));
    assert!(!server.server_home.join("apps/myapp/staging").exists());
}

#[test]
fn app_remove_keeps_database_unless_purged() {
    let server = server_with_app();
    let running = [("FAKE_DOCKER_RUNNING", "1")];
    let site = server
        .server_home
        .join("shared/ingress/caddy-main/conf/sites/myapp-staging-ui-front.caddy");
    let db_record = server
        .server_home
        .join("connectors/database/postgres16/myapp-staging-system-data.yaml");
    assert!(site.exists() && db_record.exists());

    // Without a terminal and without --yes, nothing happens.
    server
        .run(
            &["app", "remove", "--app", "myapp", "--prefix", "staging"],
            &running,
        )
        .code(2)
        .stderr(contains("add --yes"));

    server
        .run(
            &[
                "app", "remove", "--app", "myapp", "--prefix", "staging", "--purge", "ingress",
                "--yes",
            ],
            &running,
        )
        .code(2)
        .stderr(contains("ingress links have no data to purge"));
    server
        .run(
            &[
                "app", "remove", "--app", "myapp", "--prefix", "staging", "--purge", "queue",
                "--yes",
            ],
            &running,
        )
        .code(2)
        .stderr(contains("unknown kind"));

    server
        .run(
            &[
                "app", "remove", "--app", "myapp", "--prefix", "staging", "--yes",
            ],
            &running,
        )
        .success()
        .stdout(contains("Removed myapp-staging."));
    let log = server.take_docker_log();
    let position = |needle: &str| {
        log.find(needle)
            .unwrap_or_else(|| panic!("{needle:?} not in\n{log}"))
    };
    assert!(
        position("caddy reload") < position("down --remove-orphans"),
        "{log}"
    );
    assert!(!log.contains("DROP"), "database is kept: {log}");
    assert!(!site.exists() && !db_record.exists());
    assert!(!server.server_home.join("apps/myapp").exists());
    assert!(!server.read_server_file("state.yaml").contains("myapp"));

    server
        .run(
            &["app", "state", "--app", "myapp", "--prefix", "staging"],
            &running,
        )
        .code(3);
}

#[test]
fn app_remove_purge_database() {
    let server = server_with_app();
    server
        .run(
            &[
                "app",
                "remove",
                "--app",
                "myapp",
                "--prefix",
                "staging",
                "--purge",
                "database",
                "--yes",
                "--volumes",
            ],
            &[("FAKE_DOCKER_RUNNING", "1")],
        )
        .success()
        .stdout(contains(
            "link database system-data to postgres16, and delete its data",
        ));
    let log = server.take_docker_log();
    assert!(
        log.contains("DROP DATABASE IF EXISTS \"myapp_staging_system_data\" WITH (FORCE);"),
        "{log}"
    );
    assert!(
        log.contains("DROP ROLE IF EXISTS \"myapp_staging_system_data\";"),
        "{log}"
    );
    assert!(log.contains("down --remove-orphans -v"), "{log}");
}

#[test]
fn app_state_reports_stopped_service_as_unhealthy() {
    let server = server_with_shared_apps();
    server.write_work_file("app/depap.yaml", APP_CONFIG);
    server.write_work_file(
        "app/docker-compose.yaml",
        "services: {front-webapp: {build: ., restart: unless-stopped}, backend-webapp: {build: .}}",
    );
    server.write_work_file("app/.depap-staging.yaml", APP_OVERLAY);
    let running = [("FAKE_DOCKER_RUNNING", "1")];
    server
        .run(
            &[
                "app",
                "deploy",
                "--apply",
                "-f",
                "app/depap.yaml,app/.depap-staging.yaml",
            ],
            &running,
        )
        .success();

    // The fake reports one container `app`; make it the long-running service.
    let fake = server.env.work.path().join("bin/docker");
    let script = fs::read_to_string(&fake)
        .unwrap()
        .replace("app running 0 healthy", "front-webapp exited 0");
    fs::write(&fake, script).unwrap();
    server
        .run(
            &["app", "state", "--app", "myapp", "--prefix", "staging"],
            &running,
        )
        .code(4)
        .stdout(contains("front-webapp  exited 0 (stopped)  ← not healthy"));
}

#[test]
fn connector_list_shows_links_per_shared_app() {
    let server = server_with_app();
    let running = [("FAKE_DOCKER_RUNNING", "1")];
    server
        .run(&["connector", "list"], &running)
        .success()
        .stdout(contains(
            "database\n└─ postgres16  postgres 16  running\n   └─ myapp-staging/system-data  database myapp_staging_system_data, user myapp_staging_system_data  app: deployed\n",
        ))
        .stdout(contains(
            "ingress\n└─ caddy-main  caddy 2  running\n   └─ myapp-staging/ui-front  staging.example.com  /ui → myapp-staging-front-webapp:8080, / → myapp-staging-backend-webapp:5000  app: deployed\n",
        ));

    // A record left behind without its instance is shown, not hidden.
    fs::remove_dir_all(server.server_home.join("apps/myapp/staging")).unwrap();
    server
        .run(&["connector", "list"], &running)
        .success()
        .stdout(contains("myapp-staging/ui-front  app not deployed (leftover record myapp-staging-ui-front.yaml)"));

    server
        .run(
            &["app", "remove", "--app", "other", "--prefix", "x", "--yes"],
            &running,
        )
        .code(3);
}

#[test]
fn app_list_shows_deployed_instances() {
    let server = LocalServer::new();
    server
        .run(&["app", "list"], &[])
        .success()
        .stdout(contains("depap has not deployed anything"));

    let server = server_with_app();
    server
        .run(&["app", "list"], &[("FAKE_DOCKER_RUNNING", "1")])
        .success()
        .stdout(contains("myapp-staging  deployed   running"));
    server
        .run(&["app", "list"], &[])
        .success()
        .stdout(contains("myapp-staging  deployed   not running"));

    server
        .run(
            &[
                "app", "remove", "--app", "myapp", "--prefix", "staging", "--yes",
            ],
            &[("FAKE_DOCKER_RUNNING", "1")],
        )
        .success();
    server
        .run(&["app", "list"], &[("FAKE_DOCKER_RUNNING", "1")])
        .success()
        .stdout(contains("No applications deployed."));
}

#[test]
fn connector_list_on_fresh_server() {
    let server = LocalServer::new();
    server
        .run(&["connector", "list"], &[])
        .success()
        .stdout(contains("depap has not deployed anything"));
}

#[test]
fn ctrl_c_stops_and_releases_the_lock() {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let server = LocalServer::new();
    server.write_work_file("depap-server.yaml", SERVER_CONFIG);
    let log = server.env.work.path().join("docker.log");
    let child = std::process::Command::new(env!("CARGO_BIN_EXE_depap"))
        .env("DEPAP_HOME", server.env.home.path())
        .env("PATH", &server.path)
        .env("FAKE_DOCKER_LOG", &log)
        .env("FAKE_DOCKER_SLOW_UP", "1")
        .env_remove("DEPAP_SERVER")
        .current_dir(server.env.work.path())
        .args(["-s", "dev", "server", "deploy", "--apply"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0) // its own group, like a foreground job in a terminal
        .spawn()
        .unwrap();

    // Wait until the first `docker compose up` is running.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !fs::read_to_string(&log)
        .unwrap_or_default()
        .contains(" up -d")
    {
        assert!(Instant::now() < deadline, "deploy never reached `up`");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(server.server_home.join("lock").exists());

    // Ctrl-C in a terminal signals the whole group: depap, sh, docker.
    let group = format!("-{}", child.id());
    std::process::Command::new("kill")
        .args(["-INT", "--", &group])
        .status()
        .unwrap();
    let started = Instant::now();
    let output = child.wait_with_output().unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(130), "{stderr}");
    assert!(
        stderr.contains("interrupted: stopping and cleaning up"),
        "{stderr}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "stopped without waiting for `up`"
    );
    assert!(!server.server_home.join("lock").exists(), "lock released");
    let log = fs::read_to_string(&log).unwrap();
    assert_eq!(
        log.matches(" up -d").count(),
        1,
        "nothing started after Ctrl-C:\n{log}"
    );
}

#[test]
fn closed_stdout_does_not_panic() {
    use std::io::Read;
    use std::process::Stdio;

    let server = server_with_app();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_depap"))
        .env("DEPAP_HOME", server.env.home.path())
        .env("PATH", &server.path)
        .env("FAKE_DOCKER_LOG", server.env.work.path().join("docker.log"))
        .env("FAKE_DOCKER_RUNNING", "1")
        .args(["-s", "dev", "connector", "list"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take()); // like `| head` exiting before reading
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    let status = child.wait().unwrap();
    assert!(status.success(), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
}

#[test]
fn app_deploy_from_inside_the_project_folder() {
    let server = server_with_shared_apps();
    server.write_work_file("app/depap.yaml", APP_CONFIG);
    server.write_work_file(
        "app/docker-compose.yaml",
        "services: {front-webapp: {build: .}, backend-webapp: {build: .}}",
    );
    server.write_work_file("app/.depap-staging.yaml", APP_OVERLAY);
    server.write_work_file("app/Dockerfile", "FROM scratch\n");

    // Like `cd app && depap app deploy -f depap.yaml,.depap-staging.yaml`.
    Command::cargo_bin("depap")
        .unwrap()
        .env("DEPAP_HOME", server.env.home.path())
        .env("PATH", &server.path)
        .env("FAKE_DOCKER_LOG", server.env.work.path().join("docker.log"))
        .env("FAKE_DOCKER_RUNNING", "1")
        .env_remove("DEPAP_SERVER")
        .current_dir(server.env.work.path().join("app"))
        .args(["-s", "dev", "app", "deploy", "--apply"])
        .args(["-f", "depap.yaml,.depap-staging.yaml"])
        .assert()
        .success()
        .stdout(contains("uploaded 2 files"));
}

#[test]
fn app_backup_of_one_link_as_raw_dump() {
    let server = server_with_app();
    server
        .run(
            &[
                "app",
                "backup",
                "myapp-staging",
                "--link",
                "system-data",
                "--format",
                "raw",
                "-o",
                "dump.sql",
            ],
            &[("FAKE_DOCKER_RUNNING", "1")],
        )
        .success()
        .stdout(contains("links/database/system-data.sql"))
        .stdout(contains("Backup written to dump.sql"));
    let log = server.take_docker_log();
    assert!(
        log.contains(
            "exec -T postgres pg_dump -U postgres --no-owner --no-acl -d myapp_staging_system_data"
        ),
        "{log}"
    );
    assert!(!log.contains("docker save"), "only the link: {log}");
    assert!(server.env.work.path().join("dump.sql").exists());

    server
        .run(
            &["app", "backup", "myapp-staging", "--link", "nope"],
            &[("FAKE_DOCKER_RUNNING", "1")],
        )
        .code(2)
        .stderr(contains("--link nope: no such link"));
    server
        .run(
            &["app", "backup", "myapp-staging", "--target", "s3"],
            &[("FAKE_DOCKER_RUNNING", "1")],
        )
        .code(2)
        .stderr(contains("s3 backup target is not implemented"));
}

#[test]
fn app_backup_on_server_then_list_and_restore_data() {
    let server = server_with_app();
    server
        .run(
            &[
                "app",
                "backup",
                "myapp-staging",
                "--target",
                "server",
                "-o",
                "b.tar.gz",
            ],
            &[("FAKE_DOCKER_RUNNING", "1")],
        )
        .success()
        .stdout(contains("apps/myapp/staging/sources.tar"))
        .stdout(contains("apps/myapp/staging/links/ingress/ui-front.caddy"));
    assert!(server.server_home.join("backups/b.tar.gz").exists());
    server
        .run(
            &["backup", "list", "--source", "server"],
            &[("FAKE_DOCKER_RUNNING", "1")],
        )
        .success()
        .stdout(contains("b.tar.gz"));
    server
        .run(&["backup", "list"], &[("FAKE_DOCKER_RUNNING", "1")])
        .code(2)
        .stderr(contains("--source server"));
    server.take_docker_log();

    // The fake database is never empty, so loading needs --replace.
    let restore = [
        "app",
        "restore",
        "b.tar.gz",
        "--source",
        "server",
        "--include",
        "links",
        "--yes",
    ];
    server
        .run(&restore, &[("FAKE_DOCKER_RUNNING", "1")])
        .code(3)
        .stderr(contains("is not empty; add --replace"));
    server
        .run(
            &[&restore[..], &["--replace"]].concat(),
            &[("FAKE_DOCKER_RUNNING", "1")],
        )
        .success()
        .stdout(contains("loading database myapp_staging_system_data"))
        .stdout(contains("Restored myapp-staging."));
    let log = server.take_docker_log();
    assert!(
        log.contains("SET ROLE \"myapp_staging_system_data\";"),
        "{log}"
    );
    assert!(log.contains("DROP DATABASE IF EXISTS"), "{log}");
    assert!(log.contains(" stop"), "the app is stopped meanwhile: {log}");
    assert!(log.contains(" start "), "and started again: {log}");
    // Unpacked files are removed again.
    let left: Vec<_> = fs::read_dir(server.server_home.join("backups"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(left, ["b.tar.gz"]);
}

#[test]
fn server_backup_holds_dump_secrets_and_records() {
    let server = server_with_app();
    server
        .run(
            &["server", "backup", "-o", "server.tar.gz"],
            &[("FAKE_DOCKER_RUNNING", "1")],
        )
        .success()
        .stdout(contains("server/shared/database/postgres16/dumpall.sql"))
        .stdout(contains("server/shared/ingress/caddy-main/caddy.tar"))
        .stdout(contains("holds secrets"));
    assert!(server.take_docker_log().contains("pg_dumpall -U postgres"));

    // An app restore refuses a server backup, and the server refuses to
    // restore shared apps that are deployed.
    server
        .run(
            &["app", "restore", "server.tar.gz"],
            &[("FAKE_DOCKER_RUNNING", "1")],
        )
        .code(2)
        .stderr(contains("restore it with `depap server restore`"));
    server
        .run(
            &["server", "restore", "server.tar.gz", "--yes"],
            &[("FAKE_DOCKER_RUNNING", "1")],
        )
        .code(3)
        .stderr(contains("postgres16 is already deployed"));
}

#[test]
fn restore_rejects_files_that_are_not_archives() {
    let server = server_with_app();
    server.write_work_file("plain.sql", "SELECT 1;");
    server
        .run(
            &["app", "restore", "plain.sql"],
            &[("FAKE_DOCKER_RUNNING", "1")],
        )
        .code(2)
        .stderr(contains("is not a depap archive"));
    server
        .run(&["app", "backup", "--all", "myapp-staging"], &[])
        .code(2)
        .stderr(contains("cannot be used with"));
}

#[test]
fn server_generate_writes_deployable_files() {
    let server = LocalServer::new();
    server
        .run(&["server", "generate", "--list"], &[])
        .success()
        .stdout(contains("ingress/caddy"))
        .stdout(contains("database/postgres"));
    // No terminal to ask on.
    server
        .run(&["server", "generate"], &[])
        .code(2)
        .stderr(contains("pass --with"));
    server
        .run(&["server", "generate", "--with", "mysql"], &[])
        .code(2)
        .stderr(contains("unknown shared app type `mysql`"));

    server
        .run(&["server", "generate", "--with", "caddy,postgres"], &[])
        .success()
        .stdout(contains("Wrote depap-server.yaml"))
        .stdout(contains("Wrote depap-server.prod.yaml"))
        .stdout(contains("Wrote .depap-server.secrets.example.yaml"))
        .stdout(contains("postgres: set `root_password`"));
    server
        .run(&["server", "generate", "--with", "caddy"], &[])
        .code(2)
        .stderr(contains("add --force"));

    server
        .run(&["server", "deploy", "--dry-run"], &[])
        .success()
        .stdout(contains("Dry run"));
    server.write_work_file(
        ".depap-server.secrets.yaml",
        &fs::read_to_string(
            server
                .env
                .work
                .path()
                .join(".depap-server.secrets.example.yaml"),
        )
        .unwrap(),
    );
    server
        .run(
            &[
                "server",
                "deploy",
                "--dry-run",
                "-f",
                "depap-server.yaml,depap-server.prod.yaml,.depap-server.secrets.yaml",
            ],
            &[],
        )
        .success();

    server
        .run(
            &["server", "generate", "--with", "postgres", "--force"],
            &[],
        )
        .success();
    let prod = fs::read_to_string(server.env.work.path().join("depap-server.prod.yaml")).unwrap();
    assert!(prod.contains("shared: {}"), "{prod}");
}

#[test]
fn app_generate_reads_the_compose_file() {
    let env = Env::new();
    fs::write(
        env.work.path().join("compose.yml"),
        "services: {web: {build: .}, worker: {image: x}}\n",
    )
    .unwrap();
    // Works without `depap init`.
    env.depap(&["app", "generate"])
        .code(2)
        .stderr(contains("cannot read compose file docker-compose.yaml"));
    env.depap(&[
        "app",
        "generate",
        "--compose-file",
        "compose.yml",
        "--name",
        "shop",
    ])
    .success()
    .stdout(contains("Wrote depap.yaml"))
    .stdout(contains("Wrote .depap.secrets.example.yaml"));
    let config = fs::read_to_string(env.work.path().join("depap.yaml")).unwrap();
    for expected in [
        "name: shop",
        "compose_file: compose.yml",
        "build: true",
        "- name: worker\n    service: worker",
    ] {
        assert!(
            config.contains(expected),
            "missing {expected:?} in\n{config}"
        );
    }
    let secrets = fs::read_to_string(env.work.path().join(".depap.secrets.example.yaml")).unwrap();
    assert!(secrets.contains("name: shop\nprefix: staging"), "{secrets}");

    env.depap(&["app", "generate", "--compose-file", "compose.yml"])
        .code(2)
        .stderr(contains("add --force"));
}

#[test]
fn config_set_show_unset() {
    let env = Env::initialized_with_server();
    env.depap(&["config", "show"])
        .success()
        .stdout(contains("deploy.apply = false (default)"));
    env.depap(&["config", "set", "deploy.apply", "maybe"])
        .code(2)
        .stderr(contains("not a boolean"));
    env.depap(&["config", "set", "deploy.nothing", "true"])
        .code(2)
        .stderr(contains("unknown setting"));
    env.depap(&["config", "set", "deploy.apply", "true"])
        .success();
    env.depap(&[
        "config",
        "set",
        "deploy.apply",
        "false",
        "--for-server",
        "prod1",
    ])
    .success()
    .stdout(contains("deploy.apply = false for prod1"));
    env.depap(&["config", "show"])
        .success()
        .stdout(contains("deploy.apply = true\n"))
        .stdout(contains("deploy.apply = false  (server prod1)"));

    // Redefining the server keeps its settings.
    env.depap(&[
        "server",
        "define",
        "prod1",
        "--address",
        "10.0.0.2",
        "--force",
    ])
    .success();
    env.depap(&["config", "show"])
        .success()
        .stdout(contains("(server prod1)"));

    env.depap(&["config", "unset", "deploy.apply", "--for-server", "prod1"])
        .success();
    env.depap(&["config", "show"])
        .success()
        .stdout(contains("prod1").not());
    env.depap(&["server", "deploy", "--apply", "--no-apply"])
        .code(2)
        .stderr(contains("cannot be used with"));
}

#[test]
fn deploy_applies_by_default_when_configured() {
    let server = LocalServer::new();
    server.write_work_file("depap-server.yaml", SERVER_CONFIG);
    server
        .run(&["config", "set", "deploy.apply", "true"], &[])
        .success();
    let running = [("FAKE_DOCKER_RUNNING", "1")];
    server
        .run(&["server", "deploy", "--no-apply"], &running)
        .success()
        .stdout(contains("Run again with --apply"));
    server
        .run(&["server", "deploy"], &running)
        .success()
        .stdout(contains("starting database postgres16 ... running"));

    // The server's own setting wins.
    server
        .run(
            &[
                "config",
                "set",
                "deploy.apply",
                "false",
                "--for-server",
                "dev",
            ],
            &[],
        )
        .success();
    server
        .run(&["server", "deploy"], &running)
        .success()
        .stdout(contains("Run again with --apply"));
}

#[test]
fn stored_secrets_are_used_by_app_deploy() {
    use std::os::unix::fs::PermissionsExt;

    let server = server_with_shared_apps();
    server.write_work_file("app/depap.yaml", APP_CONFIG);
    server.write_work_file(
        "app/docker-compose.yaml",
        "services: {front-webapp: {build: .}, backend-webapp: {build: .}}",
    );
    server.write_work_file("app/Dockerfile", "FROM scratch\n");
    server.write_work_file(
        "app/.depap.secrets.yaml",
        &format!(
            "name: myapp\n{}",
            APP_OVERLAY.replace("prefix: staging\n", "")
        ),
    );
    let running = [("FAKE_DOCKER_RUNNING", "1")];

    server
        .run(&["secret", "list"], &[])
        .success()
        .stdout(contains("No stored secrets"));
    server
        .run(
            &[
                "secret",
                "deploy",
                "app/.depap.secrets.yaml",
                "--prefix",
                "staging",
            ],
            &[],
        )
        .success()
        .stdout(contains("Stored secrets of myapp-staging"));
    let stored = server.server_home.join("secrets/apps/myapp/staging.yaml");
    assert_eq!(
        fs::metadata(&stored).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(
        fs::read_to_string(&stored)
            .unwrap()
            .contains("prefix: staging")
    );
    server
        .run(&["secret", "list"], &[])
        .success()
        .stdout(contains("myapp  staging"));

    server
        .run(
            &[
                "app",
                "deploy",
                "--apply",
                "-f",
                "app/depap.yaml",
                "--prefix",
                "staging",
            ],
            &running,
        )
        .success()
        .stdout(contains("using stored secrets"))
        .stdout(contains("caddy-main: staging.example.com"))
        .stdout(contains("uploaded 2 files"));
    let override_yaml = server.read_server_file("apps/myapp/staging/compose/depap.override.yaml");
    assert!(
        override_yaml.contains("APP_MODE: staging"),
        "{override_yaml}"
    );
    assert!(override_yaml.contains("pa$$$$word"), "{override_yaml}");

    // Another prefix has no stored secrets, so its ingress has no domains.
    server
        .run(
            &[
                "app",
                "deploy",
                "--dry-run",
                "-f",
                "app/depap.yaml",
                "--prefix",
                "review",
            ],
            &running,
        )
        .code(2)
        .stdout(contains("using stored secrets").not());

    // Removing the app keeps its secrets for a reinstall.
    server
        .run(&["app", "remove", "myapp-staging", "--yes"], &running)
        .success();
    assert!(stored.exists());
    server
        .run(
            &["secret", "remove", "--app", "myapp", "--prefix", "staging"],
            &[],
        )
        .success();
    assert!(!stored.exists());
    server
        .run(
            &["secret", "remove", "--app", "myapp", "--prefix", "staging"],
            &[],
        )
        .code(2)
        .stderr(contains("no stored secrets"));
}
