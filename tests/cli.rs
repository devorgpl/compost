//! End-to-end tests of the compost binary. Each test uses its own COMPOST_HOME.

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

    fn compost(&self, args: &[&str]) -> assert_cmd::assert::Assert {
        Command::cargo_bin("compost")
            .unwrap()
            .env("COMPOST_HOME", self.home.path())
            .env_remove("COMPOST_SERVER")
            .current_dir(self.work.path())
            .args(args)
            .assert()
    }

    fn initialized_with_server() -> Self {
        let env = Self::new();
        env.compost(&["init"]).success();
        env.compost(&["server", "define", "prod1", "--address", "10.0.0.1"])
            .success();
        env
    }
}

#[test]
fn init_is_idempotent() {
    let env = Env::new();
    env.compost(&["init"]).success().stdout(contains("Created"));
    env.compost(&["init"])
        .success()
        .stdout(contains("Already initialized"));
}

#[test]
fn init_with_existing_key() {
    let key = "0B9A2E5C-7F1D-4C3B-9A8E-6D5F4C3B2A10";
    let normalized = "0b9a2e5c-7f1d-4c3b-9a8e-6d5f4c3b2a10";
    let env = Env::new();
    env.compost(&["init", "--key", "not-a-key"])
        .code(2)
        .stderr(contains("is not a manager key"));
    env.compost(&["init", "--key", key])
        .success()
        .stdout(contains(format!("manager key {normalized}")));
    env.compost(&["init", "--key", normalized])
        .success()
        .stdout(contains("Already initialized"));

    let other = "11111111-2222-4333-8444-555555555555";
    env.compost(&["init", "--key", other])
        .code(2)
        .stderr(contains("add --force"));
    env.compost(&["init", "--key", other, "--force"])
        .success()
        .stdout(contains(format!("manager key {other}")));
    env.compost(&["init"]).success().stdout(contains(format!(
        "Already initialized (manager key {other})"
    )));
}

#[test]
fn commands_require_init() {
    let env = Env::new();
    env.compost(&["server", "list"])
        .code(2)
        .stderr(contains("run `compost init`"));
}

#[test]
fn define_list_connect_disconnect() {
    let env = Env::initialized_with_server();
    env.compost(&["server", "define", "dev", "--local"])
        .success();
    env.compost(&["server", "define", "prod1", "--address", "10.0.0.2"])
        .code(2)
        .stderr(contains("--force"));

    env.compost(&["connect", "prod1"]).success();
    env.compost(&["server", "list"])
        .success()
        .stdout(contains("* prod1  ssh compostuser@10.0.0.1:22"))
        .stdout(contains("  dev    local"));

    env.compost(&["disconnect"])
        .success()
        .stdout(contains("Disconnected from prod1"));
    env.compost(&["connect", "nope"])
        .code(2)
        .stderr(contains("unknown server `nope`"));
}

/// A `docker` stand-in, so tests do not need Docker. Every call is appended to
/// $FAKE_DOCKER_LOG. Set FAKE_DOCKER_RUNNING=1 to report containers as running,
/// FAKE_DOCKER_DOWN=1 to fail like a missing daemon, FAKE_DOCKER_NETWORKS to the
/// networks that exist.
const FAKE_DOCKER: &str = r#"#!/bin/sh
echo "docker $*" >> "$FAKE_DOCKER_LOG"
[ -n "$FAKE_DOCKER_DOWN" ] && { echo "Cannot connect to the Docker daemon" >&2; exit 1; }
case "$*" in
  "version "*) echo 29.0.0 ;;
  "compose version"*) echo 2.40.0 ;;
  "compose "*" ps --services "*)
    for s in ${FAKE_DOCKER_SERVICES:-app}; do
      grep -qx "$s" "$FAKE_DOCKER_LOG.stopped" 2>/dev/null || echo "$s"
    done ;;
  "compose "*" ps "*) echo "app running 0 healthy" ;;
  "compose "*" stop"*)
    names="${*##* stop}"; [ -n "$names" ] || names="${FAKE_DOCKER_SERVICES:-app}"
    for s in $names; do echo "$s" >> "$FAKE_DOCKER_LOG.stopped"; done ;;
  "compose "*" start "*) rm -f "$FAKE_DOCKER_LOG.stopped" ;;
  "compose "*" run "*" failing-job"*) echo "job failed" >&2; exit 1 ;;
  "compose "*" run "*) echo "job output" ;;
  "compose "*" exec "*) cat >> "$FAKE_DOCKER_LOG" ;;
  "compose "*" up "*) if [ -n "$FAKE_DOCKER_SLOW_UP" ]; then sleep 10; fi ;;
  "compose "*) ;;
  "network inspect "*)
    for network in $FAKE_DOCKER_NETWORKS; do [ "$*" = "network inspect $network" ] && exit 0; done
    exit 1 ;;
  "network create "*) ;;
  "ps "*) if [ -n "$FAKE_DOCKER_RUNNING" ]; then echo abc123; fi ;;
  "run "*" pg_dump "*) echo "-- dump" ;;
  "run "*" pg_isready"*) echo "accepting connections" ;;
  "run "*) cat >> "$FAKE_DOCKER_LOG" ;;
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

/// A local server whose COMPOST_SERVER_HOME is a temp dir, with fake docker on PATH.
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
        env.compost(&["init"]).success();
        env.compost(&[
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
        let text = fs::read_to_string(self.env.home.path().join("compost-manager.yaml")).unwrap();
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

    /// Run `compost -s dev <args>` in the work directory.
    fn run(&self, args: &[&str], extra_env: &[(&str, &str)]) -> assert_cmd::assert::Assert {
        let mut command = Command::cargo_bin("compost").unwrap();
        command
            .env("COMPOST_HOME", self.env.home.path())
            .env("PATH", &self.path)
            .env("FAKE_DOCKER_LOG", self.env.work.path().join("docker.log"))
            .env_remove("COMPOST_SERVER")
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
    server.write_work_file("compost-server.yaml", SERVER_CONFIG);
    server.write_work_file(
        ".compost-server.secrets.yaml",
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
    server.write_work_file("compost-server.yaml", SERVER_CONFIG);
    server
        .run(&["server", "deploy", "--dry-run", "--apply"], &[])
        .success()
        .stdout(contains("Dry run"))
        .stdout(contains("networks: compost-db-postgres16, compost-ingress"))
        .stdout(contains(
            "shared/database/postgres16/secrets/postgres.env  600  (secret)",
        ));
    assert!(!server.server_home.exists());
}

#[test]
fn server_deploy_then_apply() {
    use std::os::unix::fs::PermissionsExt;

    let server = LocalServer::new();
    server.write_work_file("compost-server.yaml", SERVER_CONFIG);
    server.write_work_file(
        ".compost-server.secrets.yaml",
        "shared:\n  database:\n    - name: postgres16\n      configuration: {root_password: s3cret}\n",
    );

    // Render and upload only.
    server
        .run(&["server", "deploy"], &[])
        .success()
        .stdout(contains("Initialized"))
        .stdout(contains("database   postgres16       updated"))
        .stdout(contains("Run again with --apply"));
    let log = server.take_docker_log();
    assert!(
        log.contains("docker network create compost-ingress"),
        "{log}"
    );
    assert!(
        log.contains("docker network create compost-db-postgres16"),
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
    let stored_config = server.read_server_file("compost-server.yaml");
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
            "docker compose -p compost-database-postgres16 up -d --quiet-pull --remove-orphans --force-recreate"
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
    assert!(
        log.contains("up -d --quiet-pull --remove-orphans\n"),
        "{log}"
    );
    assert!(!log.contains("--force-recreate"), "{log}");
}

#[test]
fn caddy_joins_existing_networks_and_serves_sites() {
    let server = LocalServer::new();
    let config = |sites: &str| {
        format!(
            "shared:\n  ingress:\n    - name: caddy-main\n      type: caddy\n      configuration:\n        networks: [shop_default]\n        sites: [{sites}]\n"
        )
    };
    let shop = "{name: shop, domains: [shop.example.com], config: 'reverse_proxy shop-web-1:80'}";
    let blog = "{name: blog, domains: [blog.example.com], config: 'reverse_proxy blog:2368'}";
    server.write_work_file("compost-server.yaml", &config(&format!("{shop}, {blog}")));

    // A network compost does not manage is never created.
    server
        .run(&["server", "deploy", "--apply"], &[])
        .code(3)
        .stderr(contains(
            "network `shop_default` does not exist; compost only joins existing networks",
        ));
    let log = server.take_docker_log();
    assert!(!log.contains("network create shop_default"), "{log}");

    let running = [
        ("FAKE_DOCKER_RUNNING", "1"),
        ("FAKE_DOCKER_NETWORKS", "shop_default"),
    ];
    server
        .run(&["server", "deploy", "--apply"], &running)
        .success();
    let compose = server.read_server_file("shared/ingress/caddy-main/compose.yaml");
    assert!(
        compose.contains("networks: [compost-ingress, shop_default]"),
        "{compose}"
    );
    let site = server.read_server_file("shared/ingress/caddy-main/conf/custom/shop.caddy");
    assert!(
        site.contains(
            "shop.example.com {\n\timport compost_site\n\treverse_proxy shop-web-1:80\n}"
        ),
        "{site}"
    );
    server.take_docker_log();

    // Only a site changes: Caddy reloads it and is not recreated, and the
    // file of the removed site is deleted.
    server.write_work_file("compost-server.yaml", &config(shop));
    server
        .run(&["server", "deploy", "--apply"], &running)
        .success()
        .stdout(contains("reloaded ingress caddy-main without a restart"));
    let log = server.take_docker_log();
    assert!(
        log.contains("exec -T compost-shared-caddy-main caddy reload"),
        "{log}"
    );
    assert!(!log.contains("--force-recreate"), "{log}");
    assert!(
        !server
            .server_home
            .join("shared/ingress/caddy-main/conf/custom/blog.caddy")
            .exists()
    );

    // An app cannot take a domain of a site.
    server.write_work_file(
        "app/docker-compose.yaml",
        "services:\n  web:\n    image: nginx\n",
    );
    server.write_work_file(
        "app/compost.yaml",
        "name: shop\nservices:\n  ingress:\n    - name: site\n      domains: [shop.example.com]\n      routes: [{name: web, target: web, port: 80}]\napp_services: [{name: web, service: web}]\n",
    );
    server
        .run(&["app", "deploy", "--apply", "-f", "app/compost.yaml"], &running)
        .code(3)
        .stderr(contains(
            "domain `shop.example.com` is served by site `shop` of caddy-main in compost-server.yaml",
        ));

    // Nor can a site take the domain of an app.
    let app = fs::read_to_string(server.env.work.path().join("app/compost.yaml")).unwrap();
    server.write_work_file(
        "app/compost.yaml",
        &app.replace("shop.example.com", "new.example.com"),
    );
    server
        .run(
            &["app", "deploy", "--apply", "-f", "app/compost.yaml"],
            &running,
        )
        .success();
    server.write_work_file(
        "compost-server.yaml",
        &config(&format!(
            "{shop}, {{name: new, domains: [new.example.com], config: 'respond 200'}}"
        )),
    );
    server
        .run(&["server", "deploy", "--apply"], &running)
        .code(3)
        .stderr(contains(
            "sites[new]: domain `new.example.com` is already served by shop/main (site)",
        ));
}

#[test]
fn app_from_another_folder_with_compose_overrides() {
    let server = LocalServer::new();
    server.write_work_file("compost-server.yaml", SERVER_CONFIG);
    let running = [("FAKE_DOCKER_RUNNING", "1")];
    server
        .run(&["server", "deploy", "--apply"], &running)
        .success();

    server.write_work_file(
        "upstream/docker-compose.yaml",
        "services:\n  web:\n    build: .\n",
    );
    server.write_work_file("upstream/Dockerfile", "FROM nginx\n");
    server.write_work_file(
        "deploy/compose.migrate.yaml",
        "volumes:\n  data:\n    external: true\n    name: old_data\n",
    );
    server.write_work_file(
        "deploy/compost.yaml",
        "name: shop\ndeployment:\n  source_dir: ../upstream\n  compose_overrides: [compose.migrate.yaml]\n  build: true\napp_services: [{name: web, service: web}]\n",
    );
    server
        .run(
            &["app", "deploy", "--apply", "-f", "deploy/compost.yaml"],
            &running,
        )
        .success();

    let sources = server.server_home.join("apps/shop/main/sources");
    assert!(sources.join("Dockerfile").exists());
    assert!(
        !sources.join("compost.yaml").exists(),
        "only the upstream folder is uploaded"
    );
    assert!(
        fs::read_to_string(sources.join(".compost/compose-override-1.yaml"))
            .unwrap()
            .contains("name: old_data")
    );
    let log = server.take_docker_log();
    assert!(
        log.contains("-f sources/docker-compose.yaml -f sources/.compost/compose-override-1.yaml -f compose/compost.override.yaml build"),
        "{log}"
    );
    let stored = server.read_server_file("apps/shop/main/compost.yaml");
    assert!(!stored.contains("source_dir"), "{stored}");
    assert!(
        stored.contains(".compost/compose-override-1.yaml"),
        "{stored}"
    );
}

#[test]
fn external_database_is_linked_and_backed_up_but_never_run() {
    let server = LocalServer::new();
    server.write_work_file(
        "compost-server.yaml",
        &format!(
            "{SERVER_CONFIG}    - name: legacy-db\n      type: postgres\n      version: 15\n      external: {{host: shop-db-1, network: shop_default}}\n"
        ),
    );
    let env = [
        ("FAKE_DOCKER_RUNNING", "1"),
        ("FAKE_DOCKER_NETWORKS", "shop_default"),
    ];
    server
        .run(&["server", "deploy", "--apply"], &env)
        .success()
        .stdout(contains(
            "database legacy-db: external, not started by compost",
        ));
    let log = server.take_docker_log();
    assert!(!log.contains("compost-database-legacy-db"), "{log}");
    assert!(!log.contains("network create shop_default"), "{log}");
    let record = server.read_server_file("connectors/database/legacy-db/root-connector.yaml");
    assert!(record.contains("host: shop-db-1"), "{record}");
    assert!(!record.contains("root_password"), "{record}");
    server.state(&env).success().stdout(contains(
        "legacy-db        postgres 15  external, accepting connections",
    ));

    server.write_work_file(
        "app/docker-compose.yaml",
        "services:\n  web:\n    image: shop\n",
    );
    // `old` uses the external database, `new` gets compost's: external ones
    // are only used when pinned.
    server.write_work_file(
        "app/compost.yaml",
        "name: shop\nservices:\n  database:\n    - {name: old, type: postgres, instance: legacy-db, targets: [web], environment: {OLD_URL: db_url}}\n    - {name: new, type: postgres, targets: [web], environment: {NEW_HOST: db_host}}\napp_services: [{name: web, service: web}]\n",
    );
    server
        .run(&["app", "deploy", "--apply", "-f", "app/compost.yaml"], &env)
        .code(3)
        .stderr(contains(
            "legacy-db is external and compost has no root credentials for it, so it cannot create databases",
        ));
    server.write_work_file(
        "app/.compost-prod.yaml",
        "services:\n  database:\n    - {name: old, db_name: shop, db_user: shop, db_password: 'old secret'}\n",
    );
    server
        .run(
            &[
                "app",
                "deploy",
                "--apply",
                "-f",
                "app/compost.yaml,app/.compost-prod.yaml",
            ],
            &env,
        )
        .success();
    let compose = server.read_server_file("apps/shop/main/compose/compost.override.yaml");
    assert!(
        compose.contains("OLD_URL: postgres://shop:old%20secret@shop-db-1:5432/shop"),
        "{compose}"
    );
    assert!(compose.contains("NEW_HOST: postgres16"), "{compose}");
    assert!(
        compose.contains("shop_default: {}") || compose.contains("shop_default:"),
        "{compose}"
    );
    let log = server.take_docker_log();
    assert!(
        !log.contains("CREATE DATABASE \"shop\""),
        "nothing is created on the external server: {log}"
    );

    // Backups dump it from a throwaway client, with the password in an env file.
    server
        .run(
            &[
                "app",
                "backup",
                "shop-main",
                "--link",
                "old",
                "--format",
                "raw",
                "-o",
                "old.sql",
            ],
            &env,
        )
        .success();
    assert_eq!(
        fs::read_to_string(server.env.work.path().join("old.sql")).unwrap(),
        "-- dump\n"
    );
    let log = server.take_docker_log();
    let run = log
        .lines()
        .find(|l| l.starts_with("docker run"))
        .unwrap_or_else(|| panic!("{log}"));
    assert!(
        run.contains(
            "--network shop_default postgres:15-alpine pg_dump --no-owner --no-acl -d shop"
        ),
        "{run}"
    );
    assert!(!run.contains("old secret"), "{run}");
    let tmp = server.server_home.join("tmp");
    assert_eq!(
        fs::read_dir(&tmp).unwrap().count(),
        0,
        "the env file is deleted"
    );

    // Purging never drops a database on an external server without allow_purge.
    server
        .run(
            &["app", "remove", "shop-main", "--purge", "database", "--yes"],
            &env,
        )
        .success()
        .stderr(contains(
            "warning: database `shop` on legacy-db is kept: it is external",
        ));

    // Removed from the config, an external database is forgotten once no app uses it.
    server.write_work_file("compost-server.yaml", SERVER_CONFIG);
    server
        .run(&["server", "deploy", "--apply"], &env)
        .success()
        .stdout(contains(
            "legacy-db        removed: external, compost only forgets it",
        ));
    assert!(
        !server
            .server_home
            .join("connectors/database/legacy-db")
            .exists()
    );
    assert!(!server.read_server_file("state.yaml").contains("legacy-db"));
}

#[test]
fn external_database_with_root_credentials_is_provisioned() {
    let server = LocalServer::new();
    server.write_work_file(
        "compost-server.yaml",
        &format!(
            "{SERVER_CONFIG}    - name: rds\n      type: postgres\n      version: 16\n      external: {{host: main.rds.example.com, sslmode: require}}\n      configuration: {{root_user: admin, root_password: r00t}}\n"
        ),
    );
    let env = [("FAKE_DOCKER_RUNNING", "1")];
    server.run(&["server", "deploy", "--apply"], &env).success();
    let stored = server.read_server_file("compost-server.yaml");
    assert!(!stored.contains("r00t"), "{stored}");
    server.write_work_file(
        "app/docker-compose.yaml",
        "services:\n  web:\n    image: shop\n",
    );
    server.write_work_file(
        "app/compost.yaml",
        "name: shop\nservices:\n  database:\n    - {name: main, type: postgres, instance: rds, targets: [web], environment: {URL: db_url}}\napp_services: [{name: web, service: web}]\n",
    );
    server.take_docker_log();
    server
        .run(
            &["app", "deploy", "--apply", "-f", "app/compost.yaml"],
            &env,
        )
        .success();
    let log = server.take_docker_log();
    assert!(
        log.contains("docker run --rm -i --env-file")
            && log.contains("postgres:16-alpine psql -d postgres"),
        "{log}"
    );
    assert!(log.contains("CREATE DATABASE \"shop_main_main\""), "{log}");
    assert!(
        !log.lines()
            .any(|l| l.starts_with("docker run") && l.contains("r00t")),
        "{log}"
    );
    let compose = server.read_server_file("apps/shop/main/compose/compost.override.yaml");
    assert!(
        compose.contains("@main.rds.example.com:5432/shop_main_main?sslmode=require"),
        "{compose}"
    );
}

#[test]
fn server_deploy_respects_lock() {
    let server = LocalServer::new();
    server.write_work_file("compost-server.yaml", SERVER_CONFIG);
    server.run(&["server", "deploy"], &[]).success();
    server.write_server_file("lock/holder", "host=other pid=1\n");

    server
        .run(&["server", "deploy"], &[])
        .code(3)
        .stderr(contains(
            "locked by another compost command (host=other pid=1)",
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
        "compost-server.yaml",
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
    env.compost(&["init"]).success();
    env.compost(&[
        "server",
        "define",
        "gone",
        "--address",
        "127.0.0.1",
        "--port",
        "1",
    ])
    .success();
    env.compost(&["-s", "gone", "server", "state"])
        .code(3)
        .stderr(contains("UNREACHABLE"))
        .stderr(contains("cannot connect"));
}

#[test]
fn server_must_be_selected() {
    let env = Env::initialized_with_server();
    env.compost(&["server", "state"])
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
    server.write_work_file("compost-server.yaml", SERVER_CONFIG);
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
    server.write_work_file("app/compost.yaml", APP_CONFIG);
    server.write_work_file(
        "app/docker-compose.yaml",
        "services: {front-webapp: {build: .}, backend-webapp: {build: .}}",
    );
    server.write_work_file("app/.compost-staging.yaml", APP_OVERLAY);
    server
        .run(
            &[
                "app",
                "deploy",
                "-f",
                "app/compost.yaml,app/.compost-staging.yaml",
            ],
            &[],
        )
        .code(3)
        .stderr(contains("run `compost server deploy --apply` first"));
}

#[test]
fn app_deploy_end_to_end() {
    let server = server_with_shared_apps();
    server.write_work_file("app/compost.yaml", APP_CONFIG);
    server.write_work_file(
        "app/docker-compose.yaml",
        "services: {front-webapp: {build: .}, backend-webapp: {build: ., networks: [internal]}}\nnetworks: {internal: {}}\n",
    );
    server.write_work_file("app/.compost-staging.yaml", APP_OVERLAY);
    server.write_work_file("app/Dockerfile", "FROM scratch\n");
    server.write_work_file("app/.git/HEAD", "ref\n");
    let running = [("FAKE_DOCKER_RUNNING", "1")];
    let files = "app/compost.yaml,app/.compost-staging.yaml";

    server
        .run(&["app", "deploy", "--apply", "-f", files], &running)
        .success()
        .stdout(contains("Deploying myapp-staging to dev"))
        .stdout(contains("services.ingress[docs]: skipped (optional and no domains given)"))
        .stdout(contains("uploaded 2 files"))
        .stdout(contains("myapp-staging: deployed ("))
        .stdout(contains("database   system-data      postgres16: database myapp_staging_system_data, user myapp_staging_system_data"))
        .stdout(contains("ingress    ui-front         caddy-main: staging.example.com  /ui → myapp-staging-front-webapp:8080, / → myapp-staging-backend-webapp:5000"));

    // Sources: no .git and no config files (the overlay holds secrets).
    let instance = server.server_home.join("apps/myapp/staging");
    let mut uploaded: Vec<_> = fs::read_dir(instance.join("sources"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    uploaded.sort();
    assert_eq!(uploaded, ["Dockerfile", "docker-compose.yaml"]);

    // Compose override: networks, aliases and escaped variables.
    let override_yaml = fs::read_to_string(instance.join("compose/compost.override.yaml")).unwrap();
    for expected in [
        "aliases:\n        - myapp-staging-front-webapp",
        "default: {}",
        "compost-db-postgres16: {}",
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

    let effective = fs::read_to_string(instance.join("compost.yaml")).unwrap();
    assert!(!effective.contains("pa$$word"), "{effective}");
    assert!(
        fs::read_to_string(instance.join("compost-env.yaml"))
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
        "--project-directory sources -f sources/docker-compose.yaml -f compose/compost.override.yaml build -q",
    );
    let started = position("up -d --remove-orphans");
    let reloaded = position("exec -T compost-shared-caddy-main caddy reload");
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
    server.write_work_file("app/compost.yaml", APP_CONFIG);
    server.write_work_file(
        "app/docker-compose.yaml",
        "services: {front-webapp: {build: .}, backend-webapp: {build: .}}",
    );
    server.write_work_file("app/.compost-staging.yaml", APP_OVERLAY);
    server.write_work_file(
        "app/.compost-other.yaml",
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
                "app/compost.yaml,app/.compost-staging.yaml",
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
                "app/compost.yaml,app/.compost-other.yaml",
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
    server.write_work_file("app/compost.yaml", APP_CONFIG);
    server.write_work_file(
        "app/docker-compose.yaml",
        "services: {front-webapp: {build: .}, backend-webapp: {build: .}}",
    );
    server.write_work_file("app/.compost-staging.yaml", APP_OVERLAY);
    server
        .run(
            &[
                "app",
                "deploy",
                "--apply",
                "-f",
                "app/compost.yaml,app/.compost-staging.yaml",
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
        .stdout(contains("database   system-data"));
    server
        .run(
            &[
                "app",
                "state",
                "-f",
                "app/compost.yaml,app/.compost-staging.yaml",
            ],
            &running,
        )
        .success();
    // Without the overlay the prefix is `main`, which is not deployed.
    server
        .run(&["app", "state", "-f", "app/compost.yaml"], &running)
        .code(3)
        .stderr(contains("myapp-main is not deployed"));
    // `--prefix` replaces the prefix from the files.
    server
        .run(
            &[
                "app",
                "state",
                "-f",
                "app/compost.yaml",
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
    server.write_work_file("app/compost.yaml", APP_CONFIG);
    server.write_work_file(
        "app/docker-compose.yaml",
        "services: {front-webapp: {build: ., restart: unless-stopped}, backend-webapp: {build: .}}",
    );
    server.write_work_file("app/.compost-staging.yaml", APP_OVERLAY);
    let running = [("FAKE_DOCKER_RUNNING", "1")];
    server
        .run(
            &[
                "app",
                "deploy",
                "--apply",
                "-f",
                "app/compost.yaml,app/.compost-staging.yaml",
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
        .stdout(contains("compost has not deployed anything"));

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
        .stdout(contains("compost has not deployed anything"));
}

#[test]
fn ctrl_c_stops_and_releases_the_lock() {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let server = LocalServer::new();
    server.write_work_file("compost-server.yaml", SERVER_CONFIG);
    let log = server.env.work.path().join("docker.log");
    let child = std::process::Command::new(env!("CARGO_BIN_EXE_compost"))
        .env("COMPOST_HOME", server.env.home.path())
        .env("PATH", &server.path)
        .env("FAKE_DOCKER_LOG", &log)
        .env("FAKE_DOCKER_SLOW_UP", "1")
        .env_remove("COMPOST_SERVER")
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

    // Ctrl-C in a terminal signals the whole group: compost, sh, docker.
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
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_compost"))
        .env("COMPOST_HOME", server.env.home.path())
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
    server.write_work_file("app/compost.yaml", APP_CONFIG);
    server.write_work_file(
        "app/docker-compose.yaml",
        "services: {front-webapp: {build: .}, backend-webapp: {build: .}}",
    );
    server.write_work_file("app/.compost-staging.yaml", APP_OVERLAY);
    server.write_work_file("app/Dockerfile", "FROM scratch\n");

    // Like `cd app && compost app deploy -f compost.yaml,.compost-staging.yaml`.
    Command::cargo_bin("compost")
        .unwrap()
        .env("COMPOST_HOME", server.env.home.path())
        .env("PATH", &server.path)
        .env("FAKE_DOCKER_LOG", server.env.work.path().join("docker.log"))
        .env("FAKE_DOCKER_RUNNING", "1")
        .env_remove("COMPOST_SERVER")
        .current_dir(server.env.work.path().join("app"))
        .args(["-s", "dev", "app", "deploy", "--apply"])
        .args(["-f", "compost.yaml,.compost-staging.yaml"])
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
            "exec -T compost-shared-postgres16 pg_dump -U postgres --no-owner --no-acl -d myapp_staging_system_data"
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
        .stderr(contains("restore it with `compost server restore`"));
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
        .stderr(contains("is not a compost archive"));
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
        .stdout(contains("Wrote compost-server.yaml"))
        .stdout(contains("Wrote compost-server.prod.yaml"))
        .stdout(contains("Wrote .compost-server.secrets.example.yaml"))
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
        ".compost-server.secrets.yaml",
        &fs::read_to_string(
            server
                .env
                .work
                .path()
                .join(".compost-server.secrets.example.yaml"),
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
                "compost-server.yaml,compost-server.prod.yaml,.compost-server.secrets.yaml",
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
    let prod = fs::read_to_string(server.env.work.path().join("compost-server.prod.yaml")).unwrap();
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
    // Works without `compost init`.
    env.compost(&["app", "generate"])
        .code(2)
        .stderr(contains("cannot read compose file docker-compose.yaml"));
    env.compost(&[
        "app",
        "generate",
        "--compose-file",
        "compose.yml",
        "--name",
        "shop",
    ])
    .success()
    .stdout(contains("Wrote compost.yaml"))
    .stdout(contains("Wrote .compost.secrets.example.yaml"));
    let config = fs::read_to_string(env.work.path().join("compost.yaml")).unwrap();
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
    let secrets =
        fs::read_to_string(env.work.path().join(".compost.secrets.example.yaml")).unwrap();
    assert!(secrets.contains("name: shop\nprefix: staging"), "{secrets}");

    env.compost(&["app", "generate", "--compose-file", "compose.yml"])
        .code(2)
        .stderr(contains("add --force"));
}

#[test]
fn config_set_show_unset() {
    let env = Env::initialized_with_server();
    env.compost(&["config", "show"])
        .success()
        .stdout(contains("deploy.apply = false (default)"));
    env.compost(&["config", "set", "deploy.apply", "maybe"])
        .code(2)
        .stderr(contains("not a boolean"));
    env.compost(&["config", "set", "deploy.nothing", "true"])
        .code(2)
        .stderr(contains("unknown setting"));
    env.compost(&["config", "set", "deploy.apply", "true"])
        .success();
    env.compost(&[
        "config",
        "set",
        "deploy.apply",
        "false",
        "--for-server",
        "prod1",
    ])
    .success()
    .stdout(contains("deploy.apply = false for prod1"));
    env.compost(&["config", "show"])
        .success()
        .stdout(contains("deploy.apply = true\n"))
        .stdout(contains("deploy.apply = false  (server prod1)"));

    // Redefining the server keeps its settings.
    env.compost(&[
        "server",
        "define",
        "prod1",
        "--address",
        "10.0.0.2",
        "--force",
    ])
    .success();
    env.compost(&["config", "show"])
        .success()
        .stdout(contains("(server prod1)"));

    env.compost(&["config", "unset", "deploy.apply", "--for-server", "prod1"])
        .success();
    env.compost(&["config", "show"])
        .success()
        .stdout(contains("prod1").not());
    env.compost(&["server", "deploy", "--apply", "--no-apply"])
        .code(2)
        .stderr(contains("cannot be used with"));
}

#[test]
fn deploy_applies_by_default_when_configured() {
    let server = LocalServer::new();
    server.write_work_file("compost-server.yaml", SERVER_CONFIG);
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
    server.write_work_file("app/compost.yaml", APP_CONFIG);
    server.write_work_file(
        "app/docker-compose.yaml",
        "services: {front-webapp: {build: .}, backend-webapp: {build: .}}",
    );
    server.write_work_file("app/Dockerfile", "FROM scratch\n");
    server.write_work_file(
        "app/.compost.secrets.yaml",
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
                "app/.compost.secrets.yaml",
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
                "app/compost.yaml",
                "--prefix",
                "staging",
            ],
            &running,
        )
        .success()
        .stdout(contains("using stored secrets"))
        .stdout(contains("caddy-main: staging.example.com"))
        .stdout(contains("uploaded 2 files"));
    let override_yaml = server.read_server_file("apps/myapp/staging/compose/compost.override.yaml");
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
                "app/compost.yaml",
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

#[test]
fn app_backup_and_restore_keep_stored_secrets() {
    let server = server_with_shared_apps();
    server.write_work_file("app/compost.yaml", APP_CONFIG);
    server.write_work_file(
        "app/docker-compose.yaml",
        "services: {front-webapp: {build: .}, backend-webapp: {build: .}}",
    );
    server.write_work_file(
        "app/.compost.secrets.yaml",
        &format!("name: myapp\n{APP_OVERLAY}"),
    );
    let running = [("FAKE_DOCKER_RUNNING", "1")];
    server
        .run(&["secret", "deploy", "app/.compost.secrets.yaml"], &[])
        .success();
    server
        .run(
            &[
                "app",
                "deploy",
                "--apply",
                "-f",
                "app/compost.yaml",
                "--prefix",
                "staging",
            ],
            &running,
        )
        .success();

    server
        .run(
            &["app", "backup", "myapp-staging", "-o", "b.tar.gz"],
            &running,
        )
        .success()
        .stdout(contains("  stored secrets (the archive holds them)"));

    // A new server, as far as this instance is concerned.
    server
        .run(&["app", "remove", "myapp-staging", "--yes"], &running)
        .success();
    server
        .run(
            &["secret", "remove", "--app", "myapp", "--prefix", "staging"],
            &[],
        )
        .success();
    let stored = server.server_home.join("secrets/apps/myapp/staging.yaml");
    assert!(!stored.exists());

    server
        .run(
            &["app", "restore", "b.tar.gz", "--replace", "--yes"],
            &running,
        )
        .success()
        .stdout(contains("Restored the stored secrets of myapp-staging"));
    assert!(fs::read_to_string(&stored).unwrap().contains("pa$$word"));
    // The link record was removed, so the password comes from the secrets.
    let override_yaml = server.read_server_file("apps/myapp/staging/compose/compost.override.yaml");
    assert!(
        override_yaml.contains("DATABASE_PASSWORD: pa$$$$word"),
        "{override_yaml}"
    );

    // Raw output holds data only.
    server
        .run(
            &[
                "app",
                "backup",
                "myapp-staging",
                "--format",
                "raw",
                "-o",
                "raw.tar.gz",
            ],
            &running,
        )
        .success()
        .stdout(contains("stored secrets").not());
}

const BACKUP_STEPS: &str = "
backup:
  volumes: [data]
  stop: [backend-webapp]
  before_backup:
    - {exec: backend-webapp, command: 'dump > /data/dump'}
  after_backup:
    - {run: backup-job, command: [cleanup, --all]}
  before_restore:
    - {exec: front-webapp, command: [maintenance, 'on']}
  after_restore:
    - {exec: backend-webapp, command: 'load < /data/dump'}
";

#[test]
fn app_backup_and_restore_run_steps() {
    let server = server_with_shared_apps();
    server.write_work_file("app/compost.yaml", &format!("{APP_CONFIG}{BACKUP_STEPS}"));
    server.write_work_file(
        "app/docker-compose.yaml",
        "{services: {front-webapp: {build: .}, backend-webapp: {build: .}, \
         backup-job: {build: ., profiles: [backup]}}, volumes: {data: {}}}",
    );
    server.write_work_file("app/.compost-staging.yaml", APP_OVERLAY);
    let env = [
        ("FAKE_DOCKER_RUNNING", "1"),
        ("FAKE_DOCKER_SERVICES", "front-webapp backend-webapp"),
    ];
    let deploy = |files: &str| {
        server
            .run(&["app", "deploy", "--apply", "-f", files], &env)
            .success();
        server.take_docker_log();
    };
    deploy("app/compost.yaml,app/.compost-staging.yaml");

    let output = server
        .run(&["app", "backup", "myapp-staging", "-o", "b.tar.gz"], &env)
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).unwrap();
    let order = [
        "before_backup: exec backend-webapp",
        "links/database/system-data",
        "stopping backend-webapp ...",
        "starting backend-webapp ...",
        "after_backup: run backup-job",
        "    job output",
    ];
    let positions: Vec<_> = order
        .iter()
        .map(|line| {
            stdout
                .find(line)
                .unwrap_or_else(|| panic!("{line}: {stdout}"))
        })
        .collect();
    assert!(positions.is_sorted(), "{stdout}");
    let log = server.take_docker_log();
    assert!(
        log.contains("exec -T backend-webapp sh -c dump > /data/dump"),
        "{log}"
    );
    assert!(log.contains(" stop backend-webapp\n"), "{log}");
    assert!(log.contains(" start backend-webapp\n"), "{log}");
    assert!(
        log.contains("run --rm -T backup-job cleanup --all"),
        "{log}"
    );

    // A failed step stops the backup; the after steps still clean up.
    server.write_work_file(
        "app/.compost-failing.yaml",
        "backup: {before_backup: [{run: failing-job}]}",
    );
    server.write_work_file(
        "app/docker-compose.yaml",
        "{services: {front-webapp: {build: .}, backend-webapp: {build: .}, \
         backup-job: {build: ., profiles: [backup]}, failing-job: {build: .}}, volumes: {data: {}}}",
    );
    deploy("app/compost.yaml,app/.compost-staging.yaml,app/.compost-failing.yaml");
    server
        .run(&["app", "backup", "myapp-staging", "-o", "c.tar.gz"], &env)
        .failure()
        .stdout(contains("after_backup: run backup-job"))
        .stderr(contains("backup.before_backup[0] (run failing-job)"))
        .stderr(contains("job failed"));
    assert!(!server.env.work.path().join("c.tar.gz").exists());
    let log = server.take_docker_log();
    assert!(!log.contains(" stop"), "{log}");
    deploy("app/compost.yaml,app/.compost-staging.yaml");

    // Loading data into the deployed instance.
    let output = server
        .run(
            &[
                "app",
                "restore",
                "b.tar.gz",
                "--include",
                "links",
                "--replace",
                "--yes",
            ],
            &env,
        )
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).unwrap();
    let order = [
        "before_restore: exec front-webapp",
        "stopping myapp-staging ...",
        "starting myapp-staging ...",
        "after_restore: exec backend-webapp",
        "Restored myapp-staging.",
    ];
    let positions: Vec<_> = order
        .iter()
        .map(|line| {
            stdout
                .find(line)
                .unwrap_or_else(|| panic!("{line}: {stdout}"))
        })
        .collect();
    assert!(positions.is_sorted(), "{stdout}");
    let log = server.take_docker_log();
    assert!(log.contains("exec -T front-webapp maintenance on"), "{log}");
    assert!(
        log.contains("exec -T backend-webapp sh -c load < /data/dump"),
        "{log}"
    );

    // Deployed again from the archive: nothing runs before its data is loaded.
    server
        .run(&["app", "remove", "myapp-staging", "--yes"], &env)
        .success();
    server
        .run(&["app", "restore", "b.tar.gz", "--replace", "--yes"], &env)
        .success()
        .stdout(contains("before_restore: skipped"))
        .stdout(contains("after_restore: exec backend-webapp"));
    let log = server.take_docker_log();
    assert!(!log.contains("maintenance"), "{log}");
}

const OIDC_SERVER_CONFIG: &str = "\
shared:
  ingress:
    - {name: caddy-main, type: caddy}
  database:
    - {name: postgres16, type: postgres, version: 16}
    - {name: postgres18, type: postgres, version: 18}
  oidc:
    - {name: keycloak, type: keycloak, configuration: {url: 'https://auth.example.com'}}
";

#[test]
fn keycloak_needs_postgres_18_and_an_ingress() {
    let server = LocalServer::new();
    server.write_work_file(
        "compost-server.yaml",
        "shared:\n  ingress: [{name: caddy-main, type: caddy}]\n  database: [{name: postgres16, type: postgres, version: 16}]\n  oidc: [{name: keycloak, type: keycloak, configuration: {url: 'https://auth.example.com'}}]\n",
    );
    server
        .run(&["server", "deploy", "--dry-run"], &[])
        .code(2)
        .stderr(contains(
            "shared.oidc[keycloak]: services.database[db]: no running postgres database with version `>=18`",
        ));
    server.write_work_file(
        "compost-server.yaml",
        "shared:\n  database: [{name: postgres18, type: postgres, version: 18}]\n  oidc: [{name: keycloak, type: keycloak, configuration: {url: 'https://auth.example.com'}}]\n",
    );
    server
        .run(&["server", "deploy", "--dry-run"], &[])
        .code(2)
        .stderr(contains("services.ingress[web]: no ingress is running"));
}

#[test]
fn keycloak_uses_its_database_and_the_ingress() {
    let server = LocalServer::new();
    let running = [("FAKE_DOCKER_RUNNING", "1")];
    server.write_work_file("compost-server.yaml", OIDC_SERVER_CONFIG);
    server
        .run(&["server", "deploy", "--dry-run"], &[])
        .success()
        .stdout(contains("uses database postgres18, ingress caddy-main"));
    server
        .run(&["server", "deploy", "--apply"], &running)
        .success()
        .stdout(contains("oidc       keycloak         updated"))
        .stdout(contains("starting oidc keycloak ... running"));

    let override_yaml = server.read_server_file("shared/oidc/keycloak/compose.override.yaml");
    for expected in [
        "KC_DB_URL_HOST: postgres18",
        "KC_DB_URL_DATABASE: compost_oidc_keycloak_db",
        "KC_DB_PASSWORD: ",
        "compost-db-postgres18: {}",
        "- compost-shared-keycloak",
    ] {
        assert!(
            override_yaml.contains(expected),
            "missing {expected:?} in\n{override_yaml}"
        );
    }
    let site = server
        .read_server_file("shared/ingress/caddy-main/conf/sites/compost-oidc-keycloak-web.caddy");
    assert!(site.contains("auth.example.com {"), "{site}");
    assert!(
        site.contains("reverse_proxy compost-shared-keycloak:8080"),
        "{site}"
    );

    // Postgres and Caddy run first; the database exists before Keycloak starts,
    // and Caddy routes to it once it runs.
    let log = server.take_docker_log();
    let position = |needle: &str| {
        log.find(needle)
            .unwrap_or_else(|| panic!("{needle:?} not in\n{log}"))
    };
    let postgres = position("-p compost-database-postgres18 up -d");
    let caddy = position("-p compost-ingress-caddy-main up -d");
    let database = position("CREATE DATABASE \"compost_oidc_keycloak_db\"");
    let keycloak = position("-p compost-oidc-keycloak up -d");
    let reload = position("caddy reload");
    assert!(postgres < database && caddy < keycloak, "{log}");
    assert!(database < keycloak && keycloak < reload, "{log}");

    // Nothing changed: no recreate.
    server
        .run(&["server", "deploy", "--apply"], &running)
        .success()
        .stdout(contains("oidc       keycloak         unchanged"));
    assert!(
        !server
            .take_docker_log()
            .contains("compost-oidc-keycloak up -d --quiet-pull --remove-orphans --force-recreate")
    );

    server
        .run(&["connector", "list"], &running)
        .success()
        .stdout(contains("shared oidc/keycloak/db  database compost_oidc_keycloak_db, user compost_oidc_keycloak_db"))
        .stdout(contains("shared oidc/keycloak/web  auth.example.com  / → compost-shared-keycloak:8080"))
        .stdout(contains("keycloak  keycloak 26  running"));
}

const OIDC_APP_CONFIG: &str = "\
name: myapp
services:
  oidc:
    - name: login
      type: keycloak
      targets: [backend]
      redirect_uris: ['https://app.example.com/*']
      users: [{username: alice, password: secret-pw}]
      environment:
        OIDC_ISSUER: oidc_issuer
        OIDC_INTERNAL_URL: oidc_internal_url
        OIDC_CLIENT_ID: oidc_client_id
        OIDC_CLIENT_SECRET: oidc_client_secret
app_services:
  - name: backend
    service: backend
";

#[test]
fn oidc_link_creates_client_and_sets_variables() {
    let server = LocalServer::new();
    let running = [("FAKE_DOCKER_RUNNING", "1")];
    server.write_work_file("compost-server.yaml", OIDC_SERVER_CONFIG);
    server
        .run(&["server", "deploy", "--apply"], &running)
        .success();
    server.take_docker_log();

    server.write_work_file("app/compost.yaml", OIDC_APP_CONFIG);
    server.write_work_file(
        "app/docker-compose.yaml",
        "services: {backend: {image: x}}\n",
    );
    server
        .run(
            &["app", "deploy", "--apply", "-f", "app/compost.yaml"],
            &running,
        )
        .success()
        .stdout(contains(
            "oidc       login            keycloak: realm myapp-main, client myapp-main-login",
        ));

    let override_yaml = server.read_server_file("apps/myapp/main/compose/compost.override.yaml");
    for expected in [
        "OIDC_ISSUER: https://auth.example.com/realms/myapp-main",
        "OIDC_INTERNAL_URL: http://keycloak:8080/realms/myapp-main",
        "OIDC_CLIENT_ID: myapp-main-login",
        "OIDC_CLIENT_SECRET: ",
        "compost-oidc-keycloak: {}",
    ] {
        assert!(
            override_yaml.contains(expected),
            "missing {expected:?} in\n{override_yaml}"
        );
    }
    let effective = server.read_server_file("apps/myapp/main/compost.yaml");
    assert!(!effective.contains("secret-pw"), "{effective}");

    let log = server.take_docker_log();
    let script = log
        .find("-p compost-oidc-keycloak exec -T compost-shared-keycloak bash -s")
        .unwrap();
    let started = log.find("-p myapp-main").unwrap();
    assert!(script < started, "{log}");
    assert!(
        log.contains("kc create realms -s realm=\"$realm\""),
        "{log}"
    );
    assert!(log.contains("\"clientId\":\"myapp-main-login\""), "{log}");
    assert!(log.contains("\"value\":\"secret-pw\""), "{log}");

    server
        .run(
            &["app", "remove", "myapp-main", "--purge", "oidc", "--yes"],
            &running,
        )
        .success()
        .stdout(contains("link oidc login to keycloak, and delete its data"));
    let log = server.take_docker_log();
    assert!(log.contains("kc delete \"clients/$id\""), "{log}");
    assert!(log.contains("kc delete \"realms/$realm\""), "{log}");
    assert!(
        !server
            .server_home
            .join("connectors/oidc/keycloak/myapp-main-login.yaml")
            .exists()
    );
}

#[test]
fn server_restore_starts_keycloak_after_loading_its_database() {
    let running = [("FAKE_DOCKER_RUNNING", "1")];
    let old = LocalServer::new();
    old.write_work_file("compost-server.yaml", OIDC_SERVER_CONFIG);
    old.run(&["server", "deploy", "--apply"], &running)
        .success();
    old.run(&["server", "backup", "-o", "server.tar.gz"], &running)
        .success();

    let new = LocalServer::new();
    fs::copy(
        old.env.work.path().join("server.tar.gz"),
        new.env.work.path().join("server.tar.gz"),
    )
    .unwrap();
    new.run(&["server", "restore", "server.tar.gz", "--yes"], &running)
        .success()
        .stdout(contains("Restored."));

    let log = new.take_docker_log();
    let position = |needle: &str| {
        log.find(needle)
            .unwrap_or_else(|| panic!("{needle:?} not in\n{log}"))
    };
    let loaded = position("psql -U postgres -d postgres -q\n");
    let provisioned = position("CREATE DATABASE \"compost_oidc_keycloak_db\"");
    let keycloak = position("-p compost-oidc-keycloak up -d");
    assert!(loaded < provisioned && provisioned < keycloak, "{log}");
    let config = new.read_server_file("compost-server.yaml");
    assert!(config.contains("name: keycloak"), "{config}");
}

#[test]
fn realm_on_its_own_domain() {
    let server = LocalServer::new();
    let running = [("FAKE_DOCKER_RUNNING", "1")];
    server.write_work_file("compost-server.yaml", OIDC_SERVER_CONFIG);
    server
        .run(&["server", "deploy", "--apply"], &running)
        .success();
    server.take_docker_log();

    let config = |name: &str, public_url: &str| {
        format!(
            "\
name: {name}
services:
  oidc:
    - name: login
      type: keycloak
      realm: shop
      public_url: {public_url}
      targets: [backend]
      redirect_uris: ['https://app.example.com/*']
      environment: {{OIDC_ISSUER: oidc_issuer}}
  ingress:
    - name: login-page
      domains: [login.example.com]
      routes:
        - {{name: realm, link: oidc/login}}
app_services:
  - name: backend
    service: backend
"
        )
    };
    server.write_work_file(
        "app/compost.yaml",
        &config("myapp", "https://login.example.com"),
    );
    server.write_work_file(
        "app/docker-compose.yaml",
        "services: {backend: {image: x}}\n",
    );
    server
        .run(&["app", "deploy", "--apply", "-f", "app/compost.yaml"], &running)
        .success()
        .stdout(contains(
            "login.example.com  /realms/shop → compost-shared-keycloak:8080, /resources → compost-shared-keycloak:8080",
        ));

    let site =
        server.read_server_file("shared/ingress/caddy-main/conf/sites/myapp-main-login-page.caddy");
    assert!(
        site.contains("@realm-realm path /realms/shop /realms/shop/*"),
        "{site}"
    );
    assert!(
        site.contains("respond 404"),
        "everything else is not served: {site}"
    );
    let override_yaml = server.read_server_file("apps/myapp/main/compose/compost.override.yaml");
    assert!(
        override_yaml.contains("OIDC_ISSUER: https://login.example.com/realms/shop"),
        "{override_yaml}"
    );
    let log = server.take_docker_log();
    assert!(
        log.contains("-s 'attributes.frontendUrl=https://login.example.com'"),
        "{log}"
    );

    // Another app using the realm must agree on its URL.
    server.write_work_file(
        "other/compost.yaml",
        &config("other", "https://other.example.com"),
    );
    server.write_work_file(
        "other/docker-compose.yaml",
        "services: {backend: {image: x}}\n",
    );
    server
        .run(&["app", "deploy", "--apply", "-f", "other/compost.yaml"], &running)
        .code(3)
        .stderr(contains(
            "realm `shop` has public_url https://login.example.com in myapp/main (login), here https://other.example.com",
        ));
}

/// APP_CONFIG with the ingress entry `ui-front` renamed to `web`, and with or
/// without the database link.
fn renamed_app(database: bool) -> (String, String) {
    let mut config = APP_CONFIG.replace("- name: ui-front", "- name: web");
    let mut overlay = APP_OVERLAY.replace("- name: ui-front", "- name: web");
    if !database {
        let start = config.find("  database:").unwrap();
        let end = config.find("  ingress:").unwrap();
        config.replace_range(start..end, "");
        let start = overlay.find("  database:").unwrap();
        let end = overlay.find("  ingress:").unwrap();
        overlay.replace_range(start..end, "");
    }
    (config, overlay)
}

#[test]
fn dropped_links_are_removed_once_the_app_runs() {
    let server = server_with_app();
    let running = [("FAKE_DOCKER_RUNNING", "1")];
    let files = "app/compost.yaml,app/.compost-staging.yaml";
    let sites = server
        .server_home
        .join("shared/ingress/caddy-main/conf/sites");
    let db_record = server
        .server_home
        .join("connectors/database/postgres16/myapp-staging-system-data.yaml");
    assert!(sites.join("myapp-staging-ui-front.caddy").exists());

    // The domain moves to the renamed entry; the database link is dropped.
    let (config, overlay) = renamed_app(false);
    server.write_work_file("app/compost.yaml", &config);
    server.write_work_file("app/.compost-staging.yaml", &overlay);

    server
        .run(&["app", "deploy", "--dry-run", "-f", files, "--purge", "database"], &running)
        .success()
        .stdout(contains(
            "link database system-data (postgres16): no longer configured, would be removed with its data (--purge)",
        ))
        .stdout(contains(
            "link ingress ui-front (caddy-main): no longer configured, would be removed; its data is kept",
        ));

    // Without --apply nothing is removed, and the links stay pending.
    server
        .run(&["app", "deploy", "--no-apply", "-f", files], &running)
        .success()
        .stdout(contains(
            "link ingress ui-front (caddy-main) is no longer configured; the next deploy with --apply removes it",
        ));
    assert!(sites.join("myapp-staging-ui-front.caddy").exists());
    server
        .run(&["app", "state", "myapp-staging"], &running)
        .stdout(contains(
            "no longer configured (removed by the next deploy with --apply):",
        ))
        .stdout(contains("database   system-data      postgres16"));
    server.take_docker_log();

    // With --apply: the old site goes before the new one is validated (same
    // domain), and the database keeps its data.
    server
        .run(&["app", "deploy", "--apply", "-f", files], &running)
        .success()
        .stdout(contains("removing link ingress ui-front (caddy-main)\n"))
        .stdout(contains(
            "removing link database system-data (postgres16)\n",
        ))
        .stdout(contains(
            "ingress    web              caddy-main: staging.example.com",
        ))
        .stdout(contains("no longer configured").not());
    let log = server.take_docker_log();
    let reload = log.find("caddy reload").unwrap();
    let validate = log.find("caddy validate").unwrap();
    assert!(reload < validate, "{log}");
    assert!(!log.contains("DROP DATABASE"), "{log}");
    assert!(!sites.join("myapp-staging-ui-front.caddy").exists());
    assert!(sites.join("myapp-staging-web.caddy").exists());
    assert!(!db_record.exists());
    let pinned = server.read_server_file("apps/myapp/staging/compost-env.yaml");
    assert!(!pinned.contains("removing"), "{pinned}");

    // Back again, then dropped with --purge: the data goes too.
    let (config, overlay) = renamed_app(true);
    server.write_work_file("app/compost.yaml", &config);
    server.write_work_file("app/.compost-staging.yaml", &overlay);
    server
        .run(&["app", "deploy", "--apply", "-f", files], &running)
        .success();
    let (config, overlay) = renamed_app(false);
    server.write_work_file("app/compost.yaml", &config);
    server.write_work_file("app/.compost-staging.yaml", &overlay);
    server.take_docker_log();
    server
        .run(
            &[
                "app", "deploy", "--apply", "-f", files, "--purge", "database",
            ],
            &running,
        )
        .success()
        .stdout(contains(
            "removing link database system-data (postgres16), and its data",
        ));
    assert!(
        server
            .take_docker_log()
            .contains("DROP DATABASE IF EXISTS \"myapp_staging_system_data\" WITH (FORCE);")
    );

    server
        .run(
            &["app", "deploy", "-f", files, "--purge", "ingress"],
            &running,
        )
        .code(2)
        .stderr(contains("ingress links have no data to purge"));
}

#[test]
fn stored_secrets_keep_links_when_deploying_compost_yaml_only() {
    let server = server_with_shared_apps();
    let running = [("FAKE_DOCKER_RUNNING", "1")];
    server.write_work_file("app/compost.yaml", APP_CONFIG);
    server.write_work_file(
        "app/docker-compose.yaml",
        "services: {front-webapp: {build: .}, backend-webapp: {build: .}}",
    );
    // Domains and database credentials only in the stored overlay.
    server.write_work_file(
        "app/.compost.secrets.yaml",
        &format!("name: myapp\n{APP_OVERLAY}"),
    );
    server
        .run(&["secret", "deploy", "app/.compost.secrets.yaml"], &[])
        .success();
    let deploy = || {
        server.run(
            &[
                "app",
                "deploy",
                "--apply",
                "-f",
                "app/compost.yaml",
                "--prefix",
                "staging",
            ],
            &running,
        )
    };
    deploy().success();
    deploy()
        .success()
        .stdout(contains("removing").not())
        .stdout(contains(
            "ingress    ui-front         caddy-main: staging.example.com",
        ))
        .stdout(contains("database   system-data      postgres16"));
    assert!(
        server
            .server_home
            .join("shared/ingress/caddy-main/conf/sites/myapp-staging-ui-front.caddy")
            .exists()
    );
}

const MONITORING_SERVER_CONFIG: &str = "\
shared:
  ingress:
    - {name: caddy-main, type: caddy, configuration: {access_log: true}, monitoring: {}}
  monitoring:
    - name: o2
      type: openobserve
      configuration: {url: 'https://logs.example.com', container_metrics: false}
";

#[test]
fn caddy_and_openobserve_monitor_and_serve_each_other() {
    let server = LocalServer::new();
    let running = [("FAKE_DOCKER_RUNNING", "1")];
    server.write_work_file("compost-server.yaml", MONITORING_SERVER_CONFIG);
    server
        .run(&["server", "deploy", "--apply"], &running)
        .success()
        .stdout(contains("starting monitoring o2 ... running"));

    // Caddy sends its output to the collector and is scraped; it starts
    // first, because OpenObserve needs it for its domain.
    let caddy = server.read_server_file("shared/ingress/caddy-main/compose.override.yaml");
    for expected in [
        "fluentd-address: 127.0.0.1:24224",
        "tag: compost-ingress-caddy-main.compost-shared-caddy-main",
        "compost-monitoring-o2:",
        "- compost-shared-caddy-main",
    ] {
        assert!(caddy.contains(expected), "missing {expected:?} in\n{caddy}");
    }
    let caddyfile = server.read_server_file("shared/ingress/caddy-main/conf/Caddyfile");
    assert!(caddyfile.contains("http://:9180 {"), "{caddyfile}");
    let targets = server.read_server_file(
        "shared/monitoring/o2/collector/targets/compost-ingress-caddy-main-monitoring.yaml",
    );
    assert!(
        targets.contains("compost-shared-caddy-main:9180"),
        "{targets}"
    );
    let site = server
        .read_server_file("shared/ingress/caddy-main/conf/sites/compost-monitoring-o2-web.caddy");
    assert!(
        site.contains("reverse_proxy compost-shared-o2:5080"),
        "{site}"
    );
    let log = server.take_docker_log();
    let caddy_up = log.find("-p compost-ingress-caddy-main up -d").unwrap();
    let o2_up = log.find("-p compost-monitoring-o2 up -d").unwrap();
    assert!(caddy_up < o2_up, "{log}");

    server
        .run(&["connector", "list"], &running)
        .success()
        .stdout(contains(
            "shared ingress/caddy-main/monitoring  logs compost-shared-caddy-main; metrics compost-shared-caddy-main:9180/metrics",
        ));

    // Without `monitoring`, the link and Caddy's override go away.
    server.write_work_file(
        "compost-server.yaml",
        &MONITORING_SERVER_CONFIG.replace(", monitoring: {}", ""),
    );
    server
        .run(&["server", "deploy", "--apply"], &running)
        .success()
        .stdout(contains("removed link services.monitoring[monitoring]"));
    let home = &server.server_home;
    assert!(
        !home
            .join("shared/ingress/caddy-main/compose.override.yaml")
            .exists()
    );
    assert!(
        !home
            .join(
                "shared/monitoring/o2/collector/targets/compost-ingress-caddy-main-monitoring.yaml"
            )
            .exists()
    );
    assert!(
        !home
            .join("connectors/monitoring/o2/compost-ingress-caddy-main-monitoring.yaml")
            .exists()
    );
    assert!(server.take_docker_log().contains(
        "-p compost-ingress-caddy-main up -d --quiet-pull --remove-orphans --force-recreate"
    ));
}

#[test]
fn monitoring_link_collects_output_and_sets_variables() {
    let server = LocalServer::new();
    let running = [("FAKE_DOCKER_RUNNING", "1")];
    let app = "\
name: myapp
services:
  monitoring:
    - name: telemetry
      optional: true
      targets: [backend]
      metrics: [{target: backend, port: 9100}]
      environment: {OTEL_EXPORTER_OTLP_ENDPOINT: otlp_endpoint}
app_services:
  - name: backend
    service: backend
";
    server.write_work_file("app/compost.yaml", app);
    server.write_work_file(
        "app/docker-compose.yaml",
        "services: {backend: {image: x}}\n",
    );

    // Optional: skipped while the server has no monitoring app.
    server.write_work_file(
        "compost-server.yaml",
        "shared: {ingress: [{name: caddy-main, type: caddy}]}\n",
    );
    server
        .run(&["server", "deploy", "--apply"], &running)
        .success();
    server
        .run(
            &["app", "deploy", "--apply", "-f", "app/compost.yaml"],
            &running,
        )
        .success()
        .stdout(contains("services.monitoring[telemetry]: skipped"));

    server.write_work_file("compost-server.yaml", MONITORING_SERVER_CONFIG);
    server
        .run(&["server", "deploy", "--apply"], &running)
        .success();
    server
        .run(
            &["app", "deploy", "--apply", "-f", "app/compost.yaml"],
            &running,
        )
        .success()
        .stdout(contains(
            "logs backend; metrics myapp-main-backend:9100/metrics",
        ));
    let override_yaml = server.read_server_file("apps/myapp/main/compose/compost.override.yaml");
    for expected in [
        "OTEL_EXPORTER_OTLP_ENDPOINT: http://o2:4318",
        "driver: fluentd",
        "tag: myapp-main.backend",
        "- myapp-main-backend",
    ] {
        assert!(
            override_yaml.contains(expected),
            "missing {expected:?} in\n{override_yaml}"
        );
    }
    let targets =
        server.read_server_file("shared/monitoring/o2/collector/targets/myapp-main-telemetry.yaml");
    assert!(targets.contains("myapp-main-backend:9100"), "{targets}");
}

#[test]
fn agent_prints_llms_txt_without_a_home() {
    let env = Env::new();
    env.compost(&["agent", "llms"])
        .success()
        .stdout(predicates::str::starts_with("# compost\n"))
        .stdout(contains("/wiki/Configuration"));
    env.compost(&["agent", "llms", "--full"])
        .success()
        .stdout(contains("<!-- Configuration.md -->"))
        .stdout(contains("## `compost agent skill`"));
}

#[test]
fn agent_skill_installs_and_updates_the_skill() {
    let env = Env::new();
    let skills = env.work.path().join("skills");
    let skill = skills.join("compost");
    env.compost(&["agent", "skill", "--dir", skills.to_str().unwrap()])
        .success()
        .stdout(contains("Installed the compost skill"));
    assert!(
        fs::read_to_string(skill.join("SKILL.md"))
            .unwrap()
            .contains("name: compost")
    );
    assert!(skill.join("references/CLI-Reference.md").is_file());

    // A second run replaces references/, dropping files that are no longer there.
    fs::write(skill.join("references/Old-Page.md"), "old").unwrap();
    env.compost(&["agent", "skill", "--dir", skills.to_str().unwrap()])
        .success();
    assert!(!skill.join("references/Old-Page.md").exists());
    assert!(skill.join("references/Home.md").is_file());

    // The default folder is .claude/skills in the current folder.
    env.compost(&["agent", "skill"]).success();
    assert!(
        env.work
            .path()
            .join(".claude/skills/compost/SKILL.md")
            .is_file()
    );
}

#[test]
fn agent_skill_keeps_another_skill_named_compost() {
    let env = Env::new();
    let skill = env.work.path().join("skills/compost");
    fs::create_dir_all(&skill).unwrap();
    fs::write(skill.join("SKILL.md"), "---\nname: other\n---\n").unwrap();
    env.compost(&["agent", "skill", "--dir", "skills"])
        .code(2)
        .stderr(contains("is another skill"));
    assert_eq!(
        fs::read_to_string(skill.join("SKILL.md")).unwrap(),
        "---\nname: other\n---\n"
    );
}
