# Admin commands

Most admin tasks on a server come down to running a tool in a container: `cscli` in CrowdSec, `psql` in Postgres, `kcadm.sh` in Keycloak, a migration in your app. compost runs them from your machine through `-s <server>`, like everything else. You don't need an SSH login with Docker rights, and you don't need to know the containers' names.

| Command | What |
|---|---|
| `compost server exec <shared app> -- <command>` | Any command in a shared app's container, e.g. a shell |
| `compost app exec` / `app run` | Any command in an app's container ([Commands in containers](Applications.md#commands-in-containers)) |
| `compost server cmd <shared app>.<name> [args]` | A command compost defines for the shared app's type, e.g. `caddy-main.unban` |
| `compost server cmd <name> [args]` | A command from `commands` in compost-server.yaml |
| `compost app cmd <instance> <name> [args]` | A command from `commands` in compost.yaml |
| `compost app cmd <instance> <link>.import` / `.export` | Load a dump into a database link's database, or dump it ([Database links](#database-links)) |

## Commands in shared apps

```sh
compost server exec postgres18 -- psql -U postgres                     # psql on a terminal
compost server exec postgres18 -- pg_dump -U postgres shop > shop.sql
compost server exec caddy-main --service crowdsec -- cscli decisions list
compost server exec temporal --service admin -- temporal operator namespace list
compost server exec database/postgres18 -- sh                         # <kind>/<name> when two kinds use the name
```

The command and its arguments come after `--`. Without `--service`, it runs in the shared app's main container. `--service` picks one of its other containers by the end of its name:

| Type | Other services |
|---|---|
| `caddysecurebuild` | `crowdsec`: the CrowdSec agent, with `cscli` |
| `openobserve` | `collector` |
| `mailpit` | `ses`: the SES mock; `aws`: moto, with `moto: true` |
| `temporal` | `admin`: the `temporal` CLI, with `TEMPORAL_ADDRESS` set; `ui`, if the UI is on |

An unknown service exits 2 and lists the shared app's other services. External shared apps and communication apps have no containers, so `server exec` refuses them.

As with `app exec`, the command gets a terminal on the server when stdin and stdout are terminals, so a shell or `psql` works and Ctrl-C goes to the command. Otherwise input and output can be piped. compost exits with the command's exit status. `server exec` changes whatever the command changes: there is no dry run and no lock.

## Built-in commands

compost has commands of its own for most shared-app types. They are named `<shared app>.<command>`, so each shared app gets its own, and they need no config:

```sh
compost -s prod server cmd                                  # lists yours and these
compost -s prod server cmd caddy-main.unban 198.51.100.7
echo 'select count(*) from orders;' | compost -s prod server cmd postgres18.psql shop
compost -s prod server cmd postgres18.psql shop             # psql on a terminal
compost -s prod server cmd database/main.activity           # <kind>/<name> when two kinds use the name
```

| Type | Commands |
|---|---|
| `caddysecurebuild` | `bans`, `bans-all` (with the community blocklist), `alerts`, `check <ip>` (its bans), `alert <id>` (the requests behind it), `ban <ip> [duration]` (`4h` by default; `30m`, `168h`, …), `unban <ip>`, `unban-scenario <scenario>` (every ban it made); `ban` and the unbans ask first |
| `postgres` | `psql [database]` (as `postgres`; `postgres` by default), `activity` (`pg_stat_activity`) |
| `mariadb`, `mysql` | `sql [database]` (the client as `root`), `activity` (`show full processlist`) |
| `valkey`, `redis` | `cli`, `info` |
| `garage` | `status`, `buckets`, `keys` |
| `temporal` | `namespaces`, `workflows <namespace>` (the running ones), in its `admin` container |
| `keycloak` | `realms`, `sessions <realm>` (active sessions per client), logged in as its admin |

They run as named commands do, with `--help`, `--dry-run` and `--yes`, and they log in with the credentials the container already has in its environment. External shared apps have none. Your own commands' names have no `.`, so they never clash with these. For anything else, write [your own](#named-commands) or use `server exec`.

## Named commands

Commands you run more than once can go under `commands` in compost-server.yaml. They are reviewed with the config they act on, and anyone with access to the server runs them by name:

```yaml
commands:
  bans:
    description: Addresses banned by this server's CrowdSec
    shared_app: caddy-main
    service: crowdsec
    command: [cscli, decisions, list]

  unban:
    description: Lift a CrowdSec ban
    shared_app: caddy-main
    service: crowdsec
    args:
      ip: {type: ip}
    command: [cscli, decisions, delete, --ip, "{ip}"]
    confirm: true

  sql:
    description: One statement as root on MariaDB
    shared_app: mariadb11
    args:
      statement: {description: e.g. "show processlist"}
      database: {pattern: "[a-z0-9_]+", default: ""}
    env:
      MYSQL_PWD: "{secret:root_password}"
    command: [mariadb, -u, root, -e, "{statement}", "{database}"]

  kc-sessions:
    description: Active sessions per client in a realm
    shared_app: keycloak
    args:
      realm: {pattern: "[a-z0-9-]+"}
    command:
      - bash
      - -c
      - |
        kcadm.sh() { /opt/keycloak/bin/kcadm.sh "$@" --config /tmp/kcadm-cmd.config; }
        kcadm.sh config credentials --server http://localhost:8080 --realm master \
          --user "$KC_BOOTSTRAP_ADMIN_USERNAME" --password "$KC_BOOTSTRAP_ADMIN_PASSWORD" >/dev/null
        kcadm.sh get "realms/$1/client-session-stats"
      - _
      - "{realm}"
```

`server deploy` stores them with the rest of the config on the server. So `server cmd` uses the commands of the last deploy, and works from any machine, CI included, without the local files.

```sh
compost -s prod server cmd                          # lists them, then the built-in ones
#   bans                        Addresses banned by this server's CrowdSec  (caddy-main/crowdsec)
#   kc-sessions <realm>         Active sessions per client in a realm       (keycloak)
#   sql <statement> [database]  One statement as root on MariaDB            (mariadb11)
#   unban <ip>                  Lift a CrowdSec ban                         (caddy-main/crowdsec, asks)
compost -s prod server cmd bans
compost -s prod server cmd unban 198.51.100.7       # asks first; --yes doesn't
compost -s prod server cmd sql "show tables" shop
compost -s prod server cmd sql --database shop "show tables"
compost -s prod server cmd unban --help             # its arguments and what it runs
compost -s prod server cmd unban 198.51.100.7 --dry-run
```

Values go in the order of `args`, or by name as `--<arg> <value>` or `--<arg>=<value>`. `--yes`, `--dry-run` and `--help` come after the command's name too. compost's own options, such as `-s` and `-v`, go before `cmd`. A wrong or missing value exits 2 before anything runs, with the command's usage.

`--dry-run` shows the container, the command with its values and the names of its environment variables, never their values. It runs nothing.

### Keys

| Key | Default | Meaning |
|---|---|---|
| `description` | – | Shown by the list and by `--help` |
| `shared_app` | required | The shared app whose container runs it: a name, or `<kind>/<name>` |
| `service` | its main container | Another of its services, as for `server exec --service` |
| `args` | – | The values it takes, in order (see below) |
| `env` | – | Environment variables of the command; values may use `{<arg>}` and `{secret:<key>}` |
| `command` | required | The program and its arguments, as a list. `{<arg>}` is replaced by the argument's value |
| `confirm` | `false` | Ask before running, unless `--yes`. Set it for commands that change something |

`commands` keys are names like shared apps' names: lowercase letters, digits and `-`.

### Arguments

| Key | Default | Meaning |
|---|---|---|
| `type` | `string` | `string`, `int`, `ip` (IPv4 or IPv6), `cidr` (e.g. `203.0.113.0/24`), `duration` (e.g. `30s`, `4h`), `email` or `enum` |
| `pattern` | – | `string` only: a regular expression the whole value must match |
| `values` | – | `enum` only, required: the allowed values |
| `default` | – | Makes the argument optional. `""` leaves out a word of `command` that is only this placeholder |
| `description` | – | Shown by `--help` |

Arguments with a default come after those without. Every argument must be used in `command` or `env`. Argument names are lowercase letters, digits and `-`. `yes`, `dry-run` and `help` are taken.

A `string` without a `pattern` can't start with `-`, so it can't become an option of the tool it's passed to. No value can contain line breaks or other control characters.

### How values are passed

- **`command` is a list, never a shell string.** Each item is one argument of the program, whatever the values in it hold: `unban "1.2.3.4; rm -rf /data"` passes one (invalid) address to `cscli`. A command that needs a shell writes `[sh, -c, '… "$1" …', _, "{arg}"]`, so values reach the script as positional parameters, as `kc-sessions` does above.
- **Placeholders** are `{<arg>}` for an argument and `{secret:<key>}` for a secret. Other text in braces stays as it is, e.g. `{print $1}` in an `awk` program.
- **Secrets only go through `env`.** `{secret:<key>}` is a value of the shared app's `connectors/<kind>/<name>/root-connector.yaml` on the server, where compost keeps its administrator's credentials, including generated ones: `root_password` for postgres, mariadb and mysql, `admin_password` for keycloak, `root_password` for openobserve. compost writes the variables to a file only the server user can read. The script on the server reads that file and deletes it, then runs `docker compose exec -e <NAME>`. The values never appear in the script, in `-v` output or in the process list. A secret in `command` exits 2 when you deploy.
- Many containers already hold their credentials in their own environment, e.g. Keycloak's `KC_BOOTSTRAP_ADMIN_PASSWORD`, and Postgres lets `postgres` in over its local socket. Commands there don't need a secret.

### Checks

`server deploy` (also with `--dry-run`) checks every command: `shared_app` exists, runs containers and isn't external; `service` is one of its services; `command` is a list; placeholders and arguments match; `{secret:…}` appears only in `env`. Which keys `root-connector.yaml` has is checked when the command runs.

## History

`server cmd` and `app cmd` write a line for every command they run to `logs/commands.log` on the server: the time, who ran it (user and host of the machine compost ran on), the command, where it ran, its words with their values, and its exit status. The values of `env` are never written. `--dry-run`, `--help` and a cancelled confirmation write nothing, and neither do `server exec`, `app exec` and `app run`.

```sh
compost -s prod server cmd --history
# Commands of prod:
#   2026-05-04T09:12:40Z  ana@laptop  caddy-main.unban  exit 0    caddy-main/crowdsec: cscli decisions delete --ip 198.51.100.7
#   2026-05-04T09:30:02Z  ci@runner   sql               exit 1    mariadb11: mariadb -u root -e 'show tables' shop
compost app cmd myapp-staging --history
```

`server cmd --history` shows the server's commands and `app cmd <instance> --history` the instance's, the last 50 of each. The file keeps growing; delete it on the server to start over.

## Commands in apps

An app's `compost.yaml` can have `commands` too, run in one of its compose services:

```yaml
commands:
  migrate:
    description: Apply database migrations
    run: backend                       # a new one-off container, like `app run`
    command: [python, manage.py, migrate]
    confirm: true
  create-admin:
    exec: backend                      # the running container, like `app exec`
    args:
      email: {type: email}
    command: [python, manage.py, createsuperuser, --noinput, --email, "{email}"]
```

```sh
compost app cmd myapp-staging                       # lists them
compost app cmd myapp-staging migrate --yes
compost app cmd myapp-staging --history
compost app cmd myapp-staging create-admin admin@example.com
compost app cmd -f compost.yaml,.compost-staging.yaml create-admin admin@example.com   # instance from files
```

The keys are those of [named commands](#keys), with `exec: <service>` or `run: <service>` in place of `shared_app` and `service`. The commands are those of the deployed instance: `app deploy` stores them with its config. Without `-f`, `--app` or `--prefix`, the first word is the instance, as `app list` shows it.

App commands can't use `{secret:…}`: their containers already get the app's secrets and link variables (`DATABASE_URL`, …) from its environment. A `run` command starts the services the service `depends_on`, like `app run` without `--no-deps`.

On your own machine, `compost dev cmd [name] [args]` runs the commands of the app's local instance, from the same files as `dev up` ([Local development](Local-Development.md)).

### Database links

Every database link of an instance has two built-in commands, `<link>.import` and `<link>.export`. They work in the database server's container, as the link's user, so you don't need its name, user or password:

```sh
compost app cmd shop-test main.import --yes < dump.sql            # plain SQL on stdin
compost app cmd shop-test main.import --file dump.pgdump          # asks first
compost app cmd shop-test main.import --clean --file dump.sql     # drop and recreate the database first
compost app cmd shop-test main.export > main.sql                  # or --file main.sql
compost dev cmd main.import --yes < dump.sql                      # the local instance
compost app cmd shop-test database/main.import --file dump.sql    # <kind>/<link> when two kinds use the name
```

`import` loads plain SQL (`pg_dump`, `mariadb-dump`, `mysqldump` output) or, into Postgres, a custom-format archive (`pg_dump -Fc`, recognized by its first bytes). The objects it creates belong to the link's user, so the app can use them, and parts that name roles of the server the dump came from are left out:

| Database | Left out |
|---|---|
| Postgres, plain SQL | Lines that start with `ALTER … OWNER TO`, `GRANT`, `REVOKE`, `ALTER DEFAULT PRIVILEGES` or `SET SESSION AUTHORIZATION`, outside `COPY` data |
| Postgres, archive | Owners and privileges (`pg_restore --no-owner --no-privileges --role <link user>`) |
| MariaDB, MySQL | `DEFINER` clauses |

The import stops at the first error, which can leave part of the dump loaded. Into a database that holds objects, it fails when they clash; `--clean` drops the database and creates it again empty first, which needs root credentials on an external server. `import` asks before it starts; with the dump on stdin it can't, so it needs `--yes`. It takes the server's lock, like a deploy.

`export` writes a plain SQL dump without owners and privileges (`DEFINER` clauses for MariaDB and MySQL), as [`app backup`](Backup-and-Restore.md) saves it, so it loads into another instance with `import`. Both go into `--history`.
