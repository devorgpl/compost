//! Commands compost defines for shared apps of some types, run by
//! `compost server cmd <shared app>.<command>`. They use the credentials the
//! containers already have in their environment, so none needs a secret.

use anyhow::{Context, Result};

use super::SharedAppConfig;
use crate::config::command::Template;

const CADDYSECUREBUILD: &str = r#"
bans:
  description: Addresses CrowdSec bans now
  service: crowdsec
  command: [cscli, decisions, list]
alerts:
  description: What CrowdSec detected, newest first
  service: crowdsec
  command: [cscli, alerts, list]
check:
  description: Whether an address is banned, and why
  service: crowdsec
  args:
    ip: {type: ip}
  command: [cscli, decisions, list, --ip, "{ip}"]
alert:
  description: The requests behind an alert (Alert ID in `bans` and `alerts`)
  service: crowdsec
  args:
    id: {type: int}
  command: [cscli, alerts, inspect, "{id}", -d]
ban:
  description: Ban an address
  service: crowdsec
  args:
    ip: {type: ip}
    duration: {pattern: "[0-9]+[smh]", default: 4h, description: "e.g. 30m, 4h, 168h"}
  command: [cscli, decisions, add, --ip, "{ip}", --duration, "{duration}", --reason, "manual ban (compost server cmd)"]
  confirm: true
unban:
  description: Lift the bans of an address
  service: crowdsec
  args:
    ip: {type: ip}
  command: [cscli, decisions, delete, --ip, "{ip}"]
  confirm: true
"#;

const POSTGRES: &str = r#"
psql:
  description: psql as postgres; a terminal, or input piped in
  args:
    database: {pattern: "[A-Za-z0-9_-]+", default: postgres}
  command: [psql, -U, postgres, -d, "{database}"]
activity:
  description: What the server is doing now
  command:
    - psql
    - -U
    - postgres
    - -d
    - postgres
    - -c
    - >-
      select pid, datname, usename, state, now() - query_start as running, left(query, 60) as query
      from pg_stat_activity where state is not null and pid <> pg_backend_pid() order by query_start
"#;

/// `@client@` and `@password@` are replaced for MariaDB and MySQL.
const MYSQL: &str = r#"
sql:
  description: @client@ as root; a terminal, or input piped in
  args:
    database: {pattern: "[A-Za-z0-9_$-]+", default: ""}
  command: [sh, -c, 'MYSQL_PWD="$@password@" exec @client@ -uroot "$@"', _, "{database}"]
activity:
  description: What the server is doing now
  command: [sh, -c, 'MYSQL_PWD="$@password@" exec @client@ -uroot -e "show full processlist"']
"#;

/// `@cli@` is replaced for Valkey and Redis.
const KEYVALUE: &str = r#"
cli:
  description: @cli@ as the default user; a terminal, or input piped in
  command: [sh, -c, 'REDISCLI_AUTH="$KV_PASSWORD" exec @cli@ --no-auth-warning']
info:
  description: Memory, clients and keys
  command: [sh, -c, 'REDISCLI_AUTH="$KV_PASSWORD" exec @cli@ --no-auth-warning info']
"#;

/// Without `RUST_LOG`, garage logs its connection to the node first.
const GARAGE: &str = r#"
status:
  description: The node and its role in the cluster
  env: {RUST_LOG: warn}
  command: [/garage, status]
buckets:
  description: Buckets
  env: {RUST_LOG: warn}
  command: [/garage, bucket, list]
keys:
  description: Access keys
  env: {RUST_LOG: warn}
  command: [/garage, key, list]
"#;

const TEMPORAL: &str = r#"
namespaces:
  description: Namespaces and their retention
  service: admin
  command: [temporal, operator, namespace, list]
workflows:
  description: Running workflows of a namespace
  service: admin
  args:
    namespace: {pattern: "[A-Za-z0-9._-]+"}
  command: [temporal, workflow, list, -n, "{namespace}", --query, "ExecutionStatus='Running'"]
"#;

/// The admin's credentials are in the container's environment.
const KEYCLOAK: &str = r#"
realms:
  description: Realms
  command:
    - bash
    - -c
    - &login |
      set -eu
      config=/tmp/compost-kcadm-$$.config
      trap 'rm -f "$config"' EXIT
      kc() { /opt/keycloak/bin/kcadm.sh "$@" --config "$config"; }
      kc config credentials --server http://localhost:8080 --realm master \
        --user "$KC_BOOTSTRAP_ADMIN_USERNAME" --password "$KC_BOOTSTRAP_ADMIN_PASSWORD" </dev/null >/dev/null
      if [ $# -eq 0 ]; then kc get realms --fields realm,enabled; else kc get "realms/$1/client-session-stats"; fi
sessions:
  description: Active sessions per client of a realm
  args:
    realm: {pattern: "[A-Za-z0-9_-]+"}
  command: [bash, -c, *login, _, "{realm}"]
"#;

/// The YAML of a type's commands, if compost defines some.
fn source(kind: &str, type_name: &str) -> Option<String> {
    Some(match (kind, type_name) {
        ("ingress", "caddysecurebuild") => CADDYSECUREBUILD.to_string(),
        ("database", "postgres") => POSTGRES.to_string(),
        ("database", "mariadb") => MYSQL
            .replace("@client@", "mariadb")
            .replace("@password@", "MARIADB_ROOT_PASSWORD"),
        ("database", "mysql") => MYSQL
            .replace("@client@", "mysql")
            .replace("@password@", "MYSQL_ROOT_PASSWORD"),
        ("keyvalue", "valkey") => KEYVALUE.replace("@cli@", "valkey-cli"),
        ("keyvalue", "redis") => KEYVALUE.replace("@cli@", "redis-cli"),
        ("storage", "garage") => GARAGE.to_string(),
        ("workflow", "temporal") => TEMPORAL.to_string(),
        ("oidc", "keycloak") => KEYCLOAK.to_string(),
        _ => return None,
    })
}

/// compost's commands for a shared app, as `<shared app>.<command>`, in
/// the order they are defined. None for external shared apps.
pub fn builtin(kind: &str, config: &SharedAppConfig) -> Result<Vec<(String, Template)>> {
    let Some(source) = source(kind, &config.type_name) else {
        return Ok(Vec::new());
    };
    if config.is_external() {
        return Ok(Vec::new());
    }
    let commands: serde_yaml_ng::Mapping = serde_yaml_ng::from_str(&source)
        .with_context(|| format!("built-in commands of {kind} {}", config.type_name))?;
    commands
        .into_iter()
        .map(|(name, template)| {
            let name = name.as_str().unwrap_or_default();
            let mut template: Template = serde_yaml_ng::from_value(template)
                .with_context(|| format!("built-in command {name} of {}", config.type_name))?;
            template.shared_app = Some(format!("{kind}/{}", config.name));
            Ok((format!("{}.{name}", config.name), template))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared(type_name: &str) -> SharedAppConfig {
        serde_yaml_ng::from_str(&format!("{{name: main, type: {type_name}}}")).unwrap()
    }

    #[test]
    fn built_in_commands_are_valid() {
        for (kind, type_name) in [
            ("ingress", "caddysecurebuild"),
            ("database", "postgres"),
            ("database", "mariadb"),
            ("database", "mysql"),
            ("keyvalue", "valkey"),
            ("keyvalue", "redis"),
            ("storage", "garage"),
            ("workflow", "temporal"),
            ("oidc", "keycloak"),
        ] {
            let commands = builtin(kind, &shared(type_name)).unwrap();
            assert!(!commands.is_empty(), "{type_name}");
            for (name, template) in &commands {
                assert!(name.starts_with("main."), "{name}");
                template
                    .validate(true)
                    .unwrap_or_else(|e| panic!("{type_name} {name}: {e:#}"));
            }
        }
        assert!(builtin("ingress", &shared("caddy")).unwrap().is_empty());
    }

    #[test]
    fn mariadb_and_mysql_get_their_client_and_password() {
        let (_, sql) = builtin("database", &shared("mysql")).unwrap().remove(0);
        assert_eq!(
            sql.command[2],
            r#"MYSQL_PWD="$MYSQL_ROOT_PASSWORD" exec mysql -uroot "$@""#
        );
        let (_, sql) = builtin("database", &shared("mariadb")).unwrap().remove(0);
        assert_eq!(
            sql.command[2],
            r#"MYSQL_PWD="$MARIADB_ROOT_PASSWORD" exec mariadb -uroot "$@""#
        );
    }
}
