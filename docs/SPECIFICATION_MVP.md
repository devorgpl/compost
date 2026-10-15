# depap — MVP specification

Status: draft for implementation · Target release: 0.1.0
Vision document: [SPECIFICATION_DRAFT.md](SPECIFICATION_DRAFT.md)

This document describes the **minimum viable product**: the smallest version of depap that runs the four scenarios of the draft end to end (see [§12 Acceptance scenarios](#12-acceptance-scenarios)). Everything not listed here is out of scope for 0.1 and is collected in [§15 Roadmap](#15-roadmap). The MVP is designed so that later features plug into existing extension points instead of requiring rewrites.

Key words MUST, SHOULD and MAY are used as in RFC 2119.

---

## 1. Scope

### 1.1 In scope (0.1)
- A single CLI binary `depap`, run on the operator's machine.
- Managing one or more servers, reached over **SSH** or on **localhost**.
- Shared applications:
  - **ingress**: `caddy` (one instance per server)
  - **database**: `postgres` (any number of instances)
- User applications defined by a **docker compose** file plus `depap.yaml` and a deployment overlay.
- Linking user applications to shared applications:
  - ingress link: domains + path routes → app service port
  - database link: database + role creation, connection variables injected into app services
- Commands: `init`, `server define|list|state|deploy`, `connect`, `disconnect`, `app list|deploy|state|remove`, `connector list`.

### 1.2 Out of scope (0.1)
WebUI, API server, satellites, OIDC, S3, logs, metrics, backup/restore, more than one ingress type, ingress/database tags, removing shared applications, rollback, secret encryption at rest, image registries (images are built on the server or pulled from public registries), Windows as a server.

## 2. Requirements

| Side | Requirement |
|---|---|
| Operator machine | Linux or macOS, `depap` binary, OpenSSH client (`ssh`) |
| Server | Linux, POSIX `sh`, `tar`, Docker Engine ≥ 24, Docker Compose v2 plugin (`docker compose`) |
| Server user | Can log in with a key over SSH, member of the `docker` group (no sudo needed) |

## 3. Concepts

| Term | Meaning |
|---|---|
| **Server** | A host managed by depap. Has a name in the local config. |
| **Connector** | How depap reaches a server: `ssh` or `local`. |
| **Manager key** | UUID that identifies the depap installation managing a server. A server is managed by exactly one manager key. |
| **Shared app** | Server-wide service built from a built-in template: `type` (`caddy`, `postgres`), `name` (unique per server), `version`. Belongs to a **kind** (`ingress`, `database`). |
| **User app** | The operator's application: compose file + `depap.yaml`. Identified by `name`. |
| **Instance** | One deployment of a user app on a server: `<name>` + `<prefix>`. One app may have many instances (e.g. `staging`, `prod`). |
| **Link** | Connection between an instance and a shared app (ingress link, database link). Stored on the server as a connector record. |
| **Hook** | Function a shared app type implements to take part in deploying an instance (see §9). |

### 3.1 Naming rules
- `name`, `prefix`, shared app names, link names: `^[a-z][a-z0-9-]{0,30}$`.
- Instance ID: `<app>-<prefix>`. Used as the docker compose project name.
- Derived database identifiers replace `-` with `_`.

## 4. Local files

`DEPAP_HOME` defaults to `~/.depap` and MAY be overridden with the `DEPAP_HOME` environment variable.

### 4.1 `depap-manager.yaml`
```yaml
manager_key: 0d6c2f5e-7c55-4c1e-9a5b-3f1f1f0e2a11
```
Created by `depap init`. To manage the same servers from a second machine, copy this file (shared manager keys are the MVP answer to teams).

### 4.2 `servers.yaml`
```yaml
servers:
  - name: prod1
    connector:
      type: ssh              # ssh | local
      address: 203.0.113.10  # host name or IP; ssh only
      port: 22               # optional, default 22
      user: depapuser        # optional, default "depapuser"
      identity_file: ~/.ssh/id_ed25519  # optional; default: ssh config / agent / ~/.ssh/id_*
    server_home: ~/.depap-server       # optional, default ~/.depap-server (on the server)
  - name: dev
    connector:
      type: local
```

### 4.3 `current_context`
Plain text file containing a server name. Written by `connect`, removed by `disconnect`.

### 4.4 Choosing the target server
Every command that talks to a server uses, in order: `--server/-s <name>` → `DEPAP_SERVER` environment variable → `current_context`. If none is set, the command fails with exit code 2.

## 5. Server configuration

The operator keeps server configuration in a working directory (e.g. a Git repository with infrastructure files).

- `depap-server.yaml` — structure, safe to commit.
- `.depap-server.secrets.yaml` — secrets, MUST NOT be committed (depap prints a warning if it is tracked by Git).

Both are loaded by default; `-f` replaces the defaults (see §7).

### 5.1 `depap-server.yaml`
```yaml
shared:
  ingress:
    - name: caddy-main
      type: caddy
      version: 2                     # major version, default 2
      configuration:
        http_port: 80                # default 80
        https_port: 443              # default 443
        tls: auto                    # auto | internal | off ; default auto
        acme_email: admin@domain.tld # optional, used for ACME accounts
  database:
    - name: postgres16
      type: postgres
      version: 16                    # major version, required; used for matching
      image: postgres:16-alpine      # optional, default postgres:<version>
backup:                              # optional (§10.13)
  target: local                      # local | server | s3 ; default local (s3: not implemented yet)
  server_path: backups               # archive folder of the server target; absolute or relative to DEPAP_SERVER_HOME
```
`backup` is kept in the stored effective config, so `backup`/`restore` commands read the default target from the server.

### 5.2 `.depap-server.secrets.yaml`
```yaml
shared:
  database:
    - name: postgres16
      configuration:
        root_password: change-me     # optional; generated on first deploy if absent
```

### 5.3 Shared app types (0.1)

**`caddy`** (kind `ingress`)

| Key | Type | Default | Notes |
|---|---|---|---|
| `version` | int | `2` | Only `2` supported |
| `image` | string | `caddy:<version>-alpine` | |
| `configuration.http_port` | int | `80` | Host port |
| `configuration.https_port` | int | `443` | Host port (TCP + UDP) |
| `configuration.tls` | enum | `auto` | `auto`: ACME certificates; `internal`: Caddy's local CA (testing); `off`: HTTP only |
| `configuration.acme_email` | string | – | |

At most one ingress is allowed per server (validation error otherwise).

**`postgres`** (kind `database`)

| Key | Type | Default | Notes |
|---|---|---|---|
| `version` | int | required | Major version |
| `image` | string | `postgres:<version>` | MUST be an official `postgres` image (or compatible) |
| `configuration.root_password` | secret | generated | Generated: 32 hexadecimal characters. Single line, no `'`. Changing it after the first deploy is an error in 0.1 |

Postgres is not published on a host port in 0.1. Only containers on its network can reach it.

## 6. User application configuration

### 6.1 `depap.yaml` (in the application repository)
```yaml
name: myapp
deployment:
  type: docker-compose                 # only value in 0.1
  compose_file: docker-compose.yaml    # default docker-compose.yaml
  build: true                          # run `docker compose build`; default false (images are pulled)
  attach_sources: true                 # upload the whole directory; forced true when build is true
services:
  ingress:
    - name: ui-front
      optional: false                  # true: skipped (with a notice) when the overlay gives no domains
      routes:
        - name: ui
          target: webapp               # app_services name
          port: 8080                   # container port
          path: /ui                    # default "/"
          strip_prefix: false          # true: /ui/x is forwarded as /x
  database:
    - name: system-data
      type: postgres
      version: ">=16"                  # semver requirement on the major version; default "*"
      instance: postgres16             # optional: pin a specific shared app by name
      targets: [backend, migration]    # app_services that receive the variables
      environment:                     # app variable name -> link value
        POSTGRES_HOST: db_host
        POSTGRES_PASSWORD: db_password
app_services:
  - name: webapp
    service: front-webapp              # service name in the compose file
  - name: backend
    service: backend-webapp
    environment:                       # static variables, added to the container
      APP_MODE: production
backup:
  volumes: [uploads]                   # named volumes (top-level `volumes:` keys of the compose file) saved by `app backup`
```

Database link values available in `environment` mappings:

| Value | Example |
|---|---|
| `db_host` | `postgres16` |
| `db_port` | `5432` |
| `db_name` | `myapp_staging_system_data` |
| `db_user` | `myapp_staging_system_data` |
| `db_password` | (generated or from the overlay) |
| `db_url` | `postgres://user:password@postgres16:5432/dbname` (user and password percent-encoded) |

Compose services not listed in `app_services` are deployed unchanged and cannot be linked.

### 6.2 Deployment overlay (e.g. `.depap-staging.yaml`)
Holds per-instance values and secrets. It MAY live outside the repository. It uses the same schema as `depap.yaml` plus the keys below.
```yaml
prefix: staging                        # default "main"
services:
  ingress:
    - name: ui-front
      domains: ["staging.domain.tld", "www.staging.domain.tld"]
  database:
    - name: system-data
      db_name: mystagingappdb          # default <app>_<prefix>_<entry name>
      db_user: myappuser               # default same as default db_name
      db_password: myapppassword       # default: generated on first deploy, then reused
app_services:
  - name: backend
    environment:
      APP_MODE: staging
```

### 6.3 Validation (before anything touches the server)
Validation is layered (§9.1). The core checks only what is common to every app. It then hands each entry under `services.<kind>` to that kind's module, and the kind module passes type-specific parts to the type. Errors carry the config path, e.g. `services.ingress[ui-front].routes[api]: target "backnd" is not in app_services`.

Errors (exit code 2), grouped by the layer that raises them:
- **core**
  - unknown top-level keys, wrong types, invalid names
  - unknown kind under `services`
  - `app_services[].service` not present in the compose file
  - `backup.volumes[]` not a named volume of the compose file, or listed twice
- **ingress** (generic, used by every ingress type)
  - `routes[].target` not in `app_services`
  - entry with no `domains` that is not `optional`
  - duplicate route paths within one entry
- **database** (generic)
  - `targets[]` not in `app_services`
  - unknown link value in `environment`
  - invalid `version` requirement
- **type** (postgres)
  - `db_name`/`db_user` not matching `^[a-z_][a-z0-9_]{0,62}$`

Errors raised against the server during deploy (exit code 3). These are also kind-level checks:
- no shared app of the requested kind/type/version (or the pinned `instance` does not exist or does not match)
- more than one matching database and no `instance` pin
- a domain already used by another instance

## 7. Loading and merging files

`-f <file>` can be repeated or comma-separated (`-f a.yaml,b.yaml`). Files are deep-merged in the order given:
1. Maps merge key by key, recursively.
2. Lists whose items all have a `name` key merge **by `name`**: matching items are merged recursively, new items are appended.
3. All other lists and scalars from later files **replace** earlier values.

Defaults when `-f` is not given:
- `server deploy`: `depap-server.yaml` and `.depap-server.secrets.yaml` if present.
- `app deploy`: `depap.yaml`. An overlay MUST be passed explicitly: `-f depap.yaml -f .depap-staging.yaml`.

The **effective configuration** (merge result plus defaults, e.g. each type's default `version`) is what gets validated, stored on the server (with secrets removed) and shown by `--dry-run`.

## 8. Server layout

`DEPAP_SERVER_HOME` = `server_home` from `servers.yaml`, resolved once to an absolute path.

```
$DEPAP_SERVER_HOME/
├── manager_key                       # manager key UUID
├── depap-server.yaml                 # effective server config, secrets removed
├── state.yaml                        # depap-owned state (§8.2)
├── lock/                             # exists while a mutating command runs (§10.1)
├── backups/                          # archives of the `server` backup target (§10.13, `backup.server_path`)
├── shared/
│   ├── ingress/caddy-main/
│   │   ├── compose.yaml
│   │   ├── conf/Caddyfile
│   │   ├── conf/sites/<instance>-<entry>.caddy
│   │   ├── data/                     # /data  (certificates)
│   │   └── config/                   # /config
│   └── database/postgres16/
│       ├── compose.yaml
│       ├── data/                     # PGDATA (owned by container uid)
│       └── secrets/postgres.env      # POSTGRES_PASSWORD=…  (0600)
├── connectors/
│   ├── ingress/caddy-main/<instance>-<entry>.yaml
│   └── database/postgres16/
│       ├── root-connector.yaml       # (0600)
│       └── <instance>-<entry>.yaml   # (0600)
└── apps/<app>/<prefix>/
    ├── sources/                      # uploaded files (whole dir, or only the compose file)
    ├── compose/
    │   └── depap.override.yaml       # generated compose override, includes variables (0600)
    ├── depap.yaml                    # effective app config, secrets removed
    └── depap-env.yaml                # pinned shared apps used by this instance
```

### 8.1 Docker naming
| Object | Name |
|---|---|
| Ingress network | `depap-ingress` (external, created by depap) |
| Database network | `depap-db-<name>` (external, created by depap) |
| Shared app compose project | `depap-<kind>-<name>`, e.g. `depap-database-postgres16` |
| Instance compose project | `<app>-<prefix>` |
| Ingress alias of an app service | `<app>-<prefix>-<service>` on `depap-ingress` |
| Database host name | `<name>` (alias on `depap-db-<name>`) |

### 8.2 `state.yaml`
```yaml
schema: 1
shared:
  - kind: database
    name: postgres16
    status: deployed          # candidate | deployed | failed
    config_hash: sha256:…
    updated_at: 2026-10-15T12:00:00Z
apps:
  - name: myapp
    prefix: staging
    status: deployed          # candidate | deployed | failed
    config_hash: sha256:…
    updated_at: 2026-10-15T12:05:00Z
depap_version: 0.1.0
```
Only depap writes this file. User config files are never changed by depap.

### 8.3 Connector records
`root-connector.yaml`:
```yaml
type: postgres
instance: postgres16
version: 16
network: depap-db-postgres16
host: postgres16
port: 5432
root_db: postgres
root_user: postgres
root_password: …
```
Database link `<instance>-<entry>.yaml`:
```yaml
app: myapp
prefix: staging
entry: system-data
db_name: mystagingappdb
db_user: myappuser
db_password: …
```
Ingress link `<instance>-<entry>.yaml`:
```yaml
app: myapp
prefix: staging
entry: ui-front
domains: [staging.domain.tld]
routes:
  - path: /ui
    upstream: myapp-staging-front-webapp:8080
    strip_prefix: false
```

## 9. Shared app types: extension interface

Shared app types are built-in Rust modules. Templates are embedded in the binary. Loading plugins at runtime is not part of 0.1.

### 9.1 Three layers: core → kind → type

Every operation (validate, render, status, prepare, apply hooks, remove/purge) goes through three layers. Each layer owns part of the config and part of the behavior:

| Layer | Example | Owns |
|---|---|---|
| **core** | `config`, `server`, `app` | Loading and merging files, top-level schema, names, `app_services` ↔ compose, lock, state, running the pipeline. **Never matches on a kind or type name.** |
| **kind** | `ingress`, `database` | The common schema of its entries: shared app fields (`name`, `type`, `version`, `image`) and link fields (ingress: `domains`, `routes`, `optional`; database: `targets`, `environment`, `db_*`). Also the generic behavior of every type of this kind: picking the shared app, domain conflicts, route ordering, name and credential derivation, env mapping, network attachment, connector records. |
| **type** | `caddy`, `postgres` | The `configuration` block and its secret keys, templates, and the actual actions (Caddy reload, psql). |

**Inheritance rule**: every kind defines a trait for its types. Each operation is a trait method whose **default implementation is the kind's generic behavior**, kept in `<kind>::base`. A type can:
- **inherit**: not implement the method, so the kind's behavior runs;
- **extend**: implement it, call `<kind>::base::<op>` first, then add its own logic (like calling `super`);
- **replace**: implement it without calling the base.

Methods that only a type can provide (e.g. `render`) have no default.

```rust
// shared/ingress/mod.rs — what every ingress type implements
pub trait IngressType: Send + Sync {
    fn type_name(&self) -> &'static str;                       // "caddy"
    fn secret_keys(&self) -> &'static [&'static str] { &[] }

    // Defaults = kind behavior (ingress::base). Override to extend or replace.
    fn validate_config(&self, cfg: &SharedAppConfig) -> Result<()> { base::validate_config(cfg) }
    fn validate_link(&self, link: &IngressLink, app: &AppConfig) -> Result<()> { base::validate_link(link, app) }
    fn prepare_application(&self, ctx: &AppCtx, link: &IngressLink, out: &mut Contribution) -> Result<()> {
        base::prepare_application(ctx, link, out)                // network alias + connector record
    }
    fn before_apply_application(&self, _: &AppCtx, _: &IngressLink) -> Result<()> { Ok(()) }
    fn remove_application(&self, ctx: &AppCtx, link: &IngressLink, purge: bool) -> Result<()> {
        base::remove_application(ctx, link, purge)
    }

    // Type-specific, no default.
    fn render(&self, ctx: &ServerCtx, cfg: &SharedAppConfig) -> Result<FileSet>;
    fn status(&self, ctx: &ServerCtx, cfg: &SharedAppConfig) -> Result<Status>;
    fn after_apply_application(&self, ctx: &AppCtx, link: &IngressLink) -> Result<()>;
}

// shared/ingress/caddy/mod.rs — extends the generic validation
impl IngressType for Caddy {
    fn validate_config(&self, cfg: &SharedAppConfig) -> Result<()> {
        ingress::base::validate_config(cfg)?;                   // parent first
        let c: CaddyConfig = cfg.configuration()?;              // deny_unknown_fields
        ensure!(c.http_port != c.https_port, "http_port and https_port must differ");
        Ok(())
    }
    // validate_link, prepare_application, remove_application: inherited
    …
}
```
`DatabaseType` follows the same pattern. Its `base` handles version matching, credential resolution, env mapping and network attachment. Postgres extends `validate_config` and `validate_link` (identifier rules) and implements `before_apply_application` (psql) and `remove_application` (DROP on purge).

**Core-facing interface.** Each kind module also implements one object-safe trait. Core only knows this trait and the kind registry:
```rust
pub trait Kind: Send + Sync {
    fn name(&self) -> &'static str;                 // key under `shared:` / `services:`; also the --purge value
    fn apply_order(&self) -> i32;                   // database (10) before ingress (20); remove runs in reverse
    fn supports_purge(&self) -> bool;               // database: true, ingress: false (0.1)
    fn validate_shared(&self, cfg: &SharedAppConfig) -> Result<()>;
    fn validate_link(&self, link: &LinkConfig, app: &AppConfig) -> Result<()>;
    fn resolve(&self, ctx: &ServerCtx, link: &LinkConfig) -> Result<SharedAppRef>;   // matching, conflicts
    fn render(&self, ctx: &ServerCtx, cfg: &SharedAppConfig) -> Result<FileSet>;
    fn status(&self, ctx: &ServerCtx, cfg: &SharedAppConfig) -> Result<Status>;
    fn prepare_application(&self, ctx: &AppCtx, link: &LinkConfig, out: &mut Contribution) -> Result<()>;
    fn before_apply_application(&self, ctx: &AppCtx, link: &LinkConfig) -> Result<()>;
    fn after_apply_application(&self, ctx: &AppCtx, link: &LinkConfig) -> Result<()>;
    fn remove_application(&self, ctx: &AppCtx, link: &LinkConfig, purge: bool) -> Result<()>;
}
```
The kind implementation does three things: parse the raw entry into its typed link/config, look up the type in its own registry (`type` → `Box<dyn IngressType>`), and call the type's method. That method is either inherited or overridden. Unknown kinds and types are validation errors. Adding a new kind (e.g. `oidc`, `s3`) means adding one `Kind` implementation and one `<Kind>Type` trait. Core does not change.

Generic operations:
- `validate`: core → `Kind::validate_*` per entry → type.
- `render`, `status`: per shared app → kind → type.
- deploy: core runs the hooks for all links, ordered by `apply_order`.
- remove: core runs the hooks in reverse order, with `purge = kinds_to_purge.contains(kind.name())`.

The draft's `apply_application` is split into `prepare_application`, which only computes the change, and `before_apply_application`, which carries it out. This lets `app deploy` without `--apply` and `--dry-run` work.

Code shared by all kinds (adding external networks and aliases, env files) lives in the `Contribution` builder.

### 9.2 caddy (ingress type)

Inherits `validate_link` and `prepare_application` from `ingress::base`. Extends `validate_config` and `remove_application`. Implements `render`, `status` and `after_apply_application`.


`compose.yaml` (template result):
```yaml
services:
  caddy:
    image: caddy:2-alpine
    user: "1000:1000"   # the deploy user (`id -u`:`id -g` on the server)
    restart: unless-stopped
    ports: ["80:80", "443:443", "443:443/udp"]
    volumes:
      - ./conf/Caddyfile:/etc/caddy/Caddyfile:ro
      - ./conf/sites:/etc/caddy/sites:ro
      - ./data:/data
      - ./config:/config
    networks: [depap-ingress]
networks:
  depap-ingress:
    external: true
```
`conf/Caddyfile`:
```
{
	email admin@domain.tld      # if acme_email
	local_certs                 # if tls: internal
	auto_https off              # if tls: off
}
import /etc/caddy/sites/*.caddy
```
Caddy runs as the deploy user, so everything it writes to `data/` (certificates) and `config/` belongs to that user and can be backed up or removed without root. The official image allows its binary to bind ports 80 and 443 without root.

`conf/sites/00-default.caddy` holds a catch-all `:80` site that answers 404 ("no application is deployed for this host"). The import therefore always matches a file, and an unknown host gets a clear answer.

Site file for an ingress link. Routes are written longest path first, `/` last. Unmatched paths return 404.
```
staging.domain.tld, www.staging.domain.tld {
	@r_api path /api /api/*
	handle @r_api {
		reverse_proxy myapp-staging-backend-webapp:5000
	}
	@r_ui path /ui /ui/*
	handle @r_ui {
		reverse_proxy myapp-staging-front-webapp:8080
	}
	handle {
		respond 404
	}
}
```
With `strip_prefix: true` the route uses `uri strip_prefix /ui` before `reverse_proxy`. With `tls: off` the site address is `http://<domain>`.

- `prepare_application` (inherited from `ingress::base`): attach each target service to `depap-ingress` with its alias; write the ingress connector record.
- `after_apply_application`: write the site file; run `caddy validate`, then `caddy reload` (via `docker compose exec -T caddy`). If validation fails, restore the previous site file and fail the deploy.
- `remove_application` (extends `ingress::base`): the base deletes the connector record; caddy then deletes the site file and reloads. Ingress does not support `--purge` in 0.1.

### 9.3 postgres (database type)

Inherits `prepare_application` from `database::base`. Extends `validate_config` (its `configuration` block), `validate_link` (identifier rules) and `remove_application` (purge). Implements `render`, `status` and `before_apply_application`.


`compose.yaml`:
```yaml
services:
  postgres:
    image: postgres:16
    restart: unless-stopped
    env_file: [./secrets/postgres.env]       # POSTGRES_PASSWORD (read by docker compose, not the container user)
    environment:
      POSTGRES_USER: postgres
    volumes:
      - ./data:/var/lib/postgresql/data   # with PGDATA set to this path explicitly (postgres 18 images moved the default)
    networks:
      depap-db-postgres16:
        aliases: [postgres16]
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U postgres"]
      interval: 5s
      retries: 12
networks:
  depap-db-postgres16:
    external: true
```
- `prepare_application` (inherited from `database::base`): work out the database name, user and password. Values come from the overlay, else the existing connector record, else defaults or a generated password. Attach the target services to `depap-db-<name>`. Add the mapped variables to the target services. Write the database connector record.
- `before_apply_application`: run psql inside the container as `postgres`, with the SQL sent on stdin (never on the command line). The steps are idempotent:
  ```sql
  DO $$ BEGIN
    IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'myappuser')
    THEN CREATE ROLE myappuser LOGIN PASSWORD '…';
    ELSE ALTER ROLE myappuser PASSWORD '…';
    END IF;
  END $$;
  SELECT 'CREATE DATABASE mystagingappdb OWNER myappuser'
  WHERE NOT EXISTS (SELECT FROM pg_database WHERE datname = 'mystagingappdb')\gexec
  ```
  Identifiers are validated (§6.3). Password literals are escaped by doubling `'`.
- `remove_application`: calls `database::base::remove_application` (deletes the connector record). With `--purge database` it then runs `DROP DATABASE` and `DROP ROLE`.

## 10. Commands

Global flags: `-s, --server <name>`, `-v` (verbose: print every remote command), `--version`, `--help`.

Exit codes: `0` OK · `1` unexpected error · `2` invalid local config or usage · `3` server not ready or a server-side check failed · `4` deployed, but the final state check is not healthy · `130` interrupted with Ctrl-C.

### 10.1 Rules shared by mutating commands
- **Preflight**: run the `server state` checks first and stop on `UNREACHABLE`, `NO_DOCKER` or `FOREIGN`.
- **Lock**: create `$DEPAP_SERVER_HOME/lock/` atomically (`mkdir`) and write `holder` (host, user, pid, time) inside it. Remove it on exit, including on errors and Ctrl-C. If the lock exists, fail and print who holds it; `--force-unlock` removes a stale lock.
- **Ctrl-C**: the first Ctrl-C stops the running remote command (the terminal signals the whole foreground job). depap then starts no new command except cleanup, releases the lock and exits with 130. A second Ctrl-C exits at once, possibly leaving the lock. A remote command whose ssh session was cut may keep running on the server.
- **Idempotent**: running the same command again converges to the same result and does not recreate unchanged containers.
- **`--dry-run`**: load, merge and validate the config, run the read-only server checks, and print the files and commands that would run. Nothing is changed on the server.
- **Remote execution**: every remote action is `sh -c` over a single `ssh` invocation (or locally for `local`). Files are uploaded as a tar stream (`ssh … 'tar -x -C <dir>'`). Directories are replaced atomically: extract to `<dir>.new`, then swap. Remote scripts use only POSIX `sh` and tools BusyBox also provides, so minimal servers (e.g. Alpine) work.
- **SSH connection sharing**: depap passes `ControlMaster=auto`, `ControlPath=$DEPAP_HOME/ssh/%C` and `ControlPersist=60`. One connection serves all commands of a run, and stays open 60 s for the next run. If the socket path would be too long for a Unix socket, depap connects separately for each command instead. Measured: 53 remote commands took 7.2 s shared vs 15.7 s unshared, between two containers on one machine; real networks gain more.

### 10.2 `depap init [--key <manager key> [--force]]`
1. If `DEPAP_HOME/depap-manager.yaml` exists, print its manager key and exit 0. With `--key` and a different key, fail with exit 2 unless `--force`, which replaces the key (servers deployed with the old key then show as `FOREIGN`).
2. Otherwise create `DEPAP_HOME` with the `--key` manager key or a new UUIDv4 one, plus an empty `servers.yaml`.

`--key` takes an existing manager key (a UUID, stored in its canonical lowercase form), so another machine or a reinstalled one manages the same servers.

All other commands require `init` (exit 2 with a hint otherwise).

### 10.3 `depap server define <name>`
`--address <host> [--user depapuser] [--port 22] [--identity-file <path>] [--server-home <path>]` or `--local`.
Adds or replaces (with `--force`) the entry in `servers.yaml`. It does not contact the server.

### 10.4 `depap server list`
Table: name, connector, address, current context marker.

### 10.5 `depap connect <name>` / `depap disconnect`
`connect` checks that `<name>` is in `servers.yaml`, then writes `current_context`. `disconnect` deletes `current_context`.

### 10.6 `depap server state`
Checks, in order:
| Check | Failure status |
|---|---|
| Connector works (`ssh … true`) | `UNREACHABLE` |
| `docker version` and `docker compose version` work | `NO_DOCKER` |
| `DEPAP_SERVER_HOME/manager_key` exists | `UNINITIALIZED` (not an error for `server deploy`) |
| `manager_key` equals the local key | `FOREIGN` |
| Every shared app in the remote `depap-server.yaml` is running (healthy if it has a healthcheck) | `DEGRADED` |
| – | `OK` |

Output: overall status, then one row per shared app: kind, name, type, version, state.yaml status, container status. Exit code 0 for `OK`/`UNINITIALIZED`, 3 for `UNREACHABLE`/`NO_DOCKER`/`FOREIGN`, 4 for `DEGRADED`.

### 10.7 `depap server deploy [--apply] [-f …] [--dry-run]`
1. Load, merge and validate the server config (§5, §7).
2. Preflight and lock (§10.1).
3. If `UNINITIALIZED`: create `DEPAP_SERVER_HOME` and write `manager_key`.
4. Create missing docker networks (`depap-ingress` if an ingress is defined, `depap-db-<name>` per database).
5. For each shared app: work out its secrets (config → existing secret files → generated), `render`, upload into `shared/<kind>/<name>/` and the root connector. Mark it `candidate` in `state.yaml` if `config_hash` changed or it is new.
6. Upload the effective `depap-server.yaml` (secrets removed).
7. With `--apply`, for every shared app: `docker compose -p depap-<kind>-<name> up -d`. Compose leaves unchanged containers alone. `--force-recreate` is added when the app is not `deployed` (candidate or failed), because compose cannot see changes in mounted files. Wait up to 60 s for running/healthy. Mark `deployed`, or `failed` and exit 3. Running `up -d` for every app also restarts apps that were stopped.
8. With `--apply`, run `server state`, print it and exit with its code. Without `--apply`, print a hint to run with `--apply`.

Shared apps missing from the new config are reported as `orphaned` and left running. Removing them comes in a later release.

### 10.8 `depap app deploy [--apply] [-f …] [--prefix <prefix>] [--dry-run]`
1. Load, merge and validate the app config (§6, §7). `--prefix` replaces the `prefix` from the files. Parse the compose file to check service names.
2. Preflight and lock. The server MUST be `OK` or `DEGRADED`, and every shared app this instance needs MUST be `deployed` and running.
3. Pick the shared apps for each link (§6.3). Write the pinned names and versions to `depap-env.yaml`.
4. Upload sources to `apps/<app>/<prefix>/sources/`:
   - `attach_sources: true`: the directory containing `depap.yaml`, excluding `.git/`, every file passed with `-f`, files matching `.depap-*.yaml`, and the patterns in `.depapignore` (gitignore syntax).
   - otherwise: only the compose file.
5. **Render phase**: call `prepare_application` for each link and write `compose/depap.override.yaml` (0600), connector records and the effective `depap.yaml` (secrets removed). Mark the instance `candidate`. Without `--apply`, stop here.
6. **Apply phase** (`--apply`):
   1. `before_apply_application` for each link (database before ingress).
   2. Run with `C = docker compose -p <app>-<prefix> --project-directory sources -f sources/<compose_file> -f compose/depap.override.yaml`:
      - `C build` if `build: true`, otherwise `C pull`
      - `C up -d --remove-orphans`
   3. Wait up to 60 s until every container is running (healthy, if it has a healthcheck) or has exited with code 0. An exit with another code fails at once.
   4. `after_apply_application` for each link.
   5. Mark `deployed`, or `failed` and exit 3.
7. Print the same view as `app state`.

Generated `depap.override.yaml` for the API example (§12.4):
```yaml
services:
  front-webapp:
    networks:
      default: {}                        # added because the service had no networks of its own
      depap-ingress:
        aliases: [myapp-staging-front-webapp]
  backend-webapp:
    environment:
      APP_MODE: staging
      POSTGRES_PASSWORD: …
    networks:
      default: {}
      depap-ingress:
        aliases: [myapp-staging-backend-webapp]
      depap-db-postgres16: {}
  backend-migration:
    environment:
      POSTGRES_PASSWORD: …
    networks:
      default: {}
      depap-db-postgres16: {}
networks:
  depap-ingress: { external: true }
  depap-db-postgres16: { external: true }
```
Rules for the override:
- `default` is added only when the service defines no `networks` itself. Otherwise adding a network would silently drop the default one.
- `environment` holds static `app_services[].environment` plus link variables. Every `$` is written as `$$` so compose does not interpolate it. The file therefore contains secrets and is stored as 0600. (Per-service env files were considered, but their quoting rules make arbitrary passwords unreliable.)
- Setting the same variable of a service to two different values is an error.
- A service with `network_mode` cannot join depap networks; linking it is an error.
- The user's compose file is never modified.

### 10.9 `depap app state [<instance> | -f … [--prefix <prefix>] | --app <name> --prefix <prefix>]`
Reads `name` and `prefix` from the given files (defaults as in deploy; `--prefix` replaces the files' prefix), or takes `--app <name> --prefix <prefix>`, or an `<instance>` as `app list` shows it (`<name>-<prefix>`). Both parts may contain `-`, so `<instance>` is looked up in `state.yaml`; if it matches more than one instance, it is a usage error (exit 2) asking for `--app` and `--prefix`. Shows:
- the instance's status from `state.yaml`
- each compose service: running, exited or missing, and health
- ingress links: domains → path → upstream
- database links: instance, database, user (never the password)

Exit 0 if all services are healthy, 4 otherwise. A service is healthy when it runs (and its healthcheck passes), or when it exited with code 0, like a migration. Exception: a service with `restart: always` or `unless-stopped` is meant to keep running. If it has exited, it was stopped and is not healthy, whatever its exit code. Status is read from the files deploy stored on the server, so the app's repository is not needed.

### 10.10 `depap app remove [<instance> | -f … [--prefix <prefix>] | --app <name> --prefix <prefix>] [--volumes] [--purge <kind>]… [--yes]`
The instance is selected as in `app state`. `--purge <kind>` is generic: it can be repeated or comma-separated (`--purge database,s3` in later releases) and names a kind from the registry. Each named kind's `remove_application` gets `purge = true` and deletes the instance's data held by that shared app. Naming a kind whose `supports_purge()` is false (ingress in 0.1), or an unknown kind, is a usage error (exit 2) before anything runs.

1. Validate the `--purge` values. Preflight and lock. Ask for confirmation unless `--yes`. The prompt lists what will be purged.
2. `remove_application` for each link, in reverse `apply_order` (ingress first, so traffic stops before containers go). The hook gets the link's name, not its config: with `--app`/`--prefix` no config files are given, so kinds work from their link records. A link whose shared app is no longer configured is left as it is, with a warning. Postgres purges with `DROP DATABASE … WITH (FORCE)` because the app is still connected at this point (requires postgres 13+).
3. `C down --remove-orphans` (plus `-v` with `--volumes`).
4. Delete `apps/<app>/<prefix>/` and the `state.yaml` entry.

### 10.11 `depap connector list`
Tree built from `connectors/`, the state checks and `state.yaml`. Kinds come from the server config, and links from the link records of each shared app (`root-*` files describe the shared app itself and are skipped). Every link record starts with `app`, `prefix` and `entry`, so core reads records of any kind; the line itself comes from the kind's `describe_link`, the same one `app state` uses.
```
database
└─ postgres16  postgres 16  running
   ├─ myapp-prod/system-data  database myapp_prod_system_data, user myapp_prod_system_data  app: deployed
   └─ myapp-staging/system-data  database myapp_staging_system_data, user myapp_staging_system_data  app: deployed
ingress
└─ caddy-main  caddy -  running
   ├─ myapp-prod/ui-front  prod.example.com  /ui → myapp-prod-front-webapp:8080  app: deployed
   └─ myapp-staging/ui-front  staging.example.com  /ui → myapp-staging-front-webapp:8080  app: deployed
```
A record whose instance no longer exists is shown as `app not deployed (leftover record …)`, not hidden. On a server depap has not deployed to, it prints a notice and exits 0.

### 10.12 `depap app list`
One line per application instance in `state.yaml`, sorted by name and prefix: the instance, its deploy status, whether its compose project has a running container, and when the record was last updated.
```
myapp-prod     deployed   running      2026-10-14T09:12:03Z
myapp-staging  candidate  not running  2026-10-15T07:40:51Z
```
Instances are listed from `state.yaml` only, so the app's repository is not needed. On a server depap has not deployed to, it prints a notice and exits 0.

### 10.13 Backups: targets, formats, archive layout
Backups are taken by `app backup` and `server backup` and restored by `app restore` and `server restore`.

**Target** (`--target` for backups, `--source` for restores; default `backup.target` of the stored `depap-server.yaml`, else `local`):
- `local`: items are streamed through the connector into a temporary folder on this machine, then packed there. Nothing is left on the server.
- `server`: items are written next to the archive folder (`backup.server_path`, default `$DEPAP_SERVER_HOME/backups`) and packed with `tar -cz` on the server; no data crosses the network. `-o` is then a file name in that folder, and restores take a file name in it (or an absolute path).
- `s3`: accepted in config and flags, fails with exit 2 ("not implemented yet").

**Format** (`--format`):
- `depap` (default): `<name>.depap.tar.gz` with `manifest.yaml` and the configuration needed to restore. Only this format can be restored.
- `raw`: the data alone, without manifest, configuration or secrets. One item is written as the plain file (e.g. a `.sql` dump or a `.caddy` site file); several are packed as `<name>.tar.gz`.

Archives are created with mode `0600` and never overwrite an existing file. Default names: `<app>-<prefix>-<UTC yyyymmdd-hhmmss>`, `apps-<time>` (`--all`), `<server>-<time>` or `<server>-<shared app>-<time>`.

```
manifest.yaml                                  # format: 1, scope: server|app|apps, depap_version, created_at,
                                               # server, instances, shared, items [{path, size}]
apps/<app>/<prefix>/depap.yaml                 # stored effective config (no secrets)
apps/<app>/<prefix>/depap-env.yaml
apps/<app>/<prefix>/sources.tar                # the uploaded sources (only the compose file without attach_sources)
apps/<app>/<prefix>/volumes/<volume>.tar
apps/<app>/<prefix>/images.tar                 # docker save
apps/<app>/<prefix>/links/<kind>/<entry>.*     # database: <entry>.sql; ingress: <entry>.yaml (record), <entry>.caddy
server/depap-server.yaml                       # stored effective config
server/connectors.tar                          # link records of the saved shared apps (secrets)
server/shared/<kind>/<name>/…                  # postgres: dumpall.sql, postgres.env (secret); caddy: caddy.tar
```

Kinds take part through `Kind` hooks with no-op defaults: `backup_link`, `restore_link` (app level) and `backup_shared`, `restore_shared_files` (before the shared app is deployed), `restore_shared_data` (after it started). Each hook describes items as scripts run on the server; core streams their output into the archive, or the archived item into their stdin.
- postgres link: `pg_dump --no-owner --no-acl` (plain SQL). Loaded as `SET ROLE "<link user>"` + dump with `ON_ERROR_STOP`, so restored objects belong to the link user, also under another prefix or database name. A database that holds objects is only dropped and recreated with `--replace` (exit 3 otherwise).
- postgres server: `pg_dumpall` plus `secrets/postgres.env`. The password file is put back before the first start, so the data folder gets the same root password as the dump. When loading, the error for the always existing `postgres` role is ignored; any other error fails the restore.
- caddy link: the link record and site file (for review; deploy rebuilds routing). caddy server: `data/`, `config/` and `conf/`; restore puts back `data/` (certificates, which avoids ACME rate limits on a move) and `config/`.

### 10.14 `depap app backup [<instance> | -f … | --app … --prefix … | --all] [--include …] [--link …]… [--format …] [-o …] [--target …] [--stop]`
Parts (`--include`, comma-separated): `links`, `sources`, `volumes`, `images`. Default: `links,sources,volumes`. With `--link <entry>` or `--link <kind>:<entry>` (repeatable) only links are saved unless `--include` says otherwise, e.g. `--link main-db --format raw` writes a plain SQL dump. A `--link` that matches no link is a usage error.
1. Preflight, lock (a backup during a deploy would be inconsistent).
2. Per instance: the stored `depap.yaml` and `depap-env.yaml`; `sources/` as tar; each link's `backup_link`; each `backup.volumes` volume through a `busybox` helper container (`tar -cf - -C /v .`; a volume that does not exist yet is skipped with a warning); with `images`, `docker save` of the images of `C config --images` that exist.
3. `--stop` stops the running services while volumes are copied and starts the same services again afterwards (also on errors and Ctrl-C).

### 10.15 `depap app restore <archive> [--instance <instance> | --all] [--prefix <prefix>] [-f …] [--include …] [--link …]… [--source …] [--replace] [--yes]`
The archive must be an app archive (exit 2 otherwise). With several instances, `--instance` or `--all` is required. Parts default to everything in the archive (only links with `--link`).

**Deployment restore** (sources are restored): the archived `depap.yaml` is the base config, `-f` overlays are merged over it and `--prefix` replaces its prefix. This makes a copy, e.g. `--prefix review -f review-domains.yaml` (domains must differ, see §6.3; database names follow the new prefix unless the config sets `db_name`). The instance is deployed as with `app deploy --apply` (§10.8), and after the `before_apply_application` hooks:
1. `restore_link` for each selected link (the database was just created by the link);
2. `docker load` of the images; images built for the archived instance are tagged for the new one, and the build/pull step is skipped;
3. `C create`, then each volume's contents are replaced;
4. `C up -d` as usual.

**Data restore** (no sources in the parts or in the archive): data is loaded into the deployed instance `<name>-<prefix>` (`--prefix` selects another one). It lists what is replaced and asks for confirmation unless `--yes`. Its running services are stopped, links and volumes are loaded, and the services are started again (`up -d` when images were loaded).

In both modes, volumes and databases that already hold data are replaced only with `--replace` (exit 3 otherwise).

### 10.16 `depap server backup [--shared <name>]… [--format …] [-o …] [--target …]`
Saves the stored `depap-server.yaml`, the `connectors/` records and each shared app's `backup_shared` items (all shared apps, or those named). The archive holds secrets (root passwords, link credentials); depap says so after writing it.

### 10.17 `depap server restore <archive> [--shared <name>]… [--source …] [--yes]`
Restores shared apps onto a server that does not have them yet (a new server, or one without these shared apps; exit 3 if one is deployed).
1. Preflight (an uninitialized server is fine). Merge the selected shared apps from the archived config into the server's config (the archived `backup` settings are used when the server has none). Confirm unless `--yes`.
2. Lock; put back the link records of the selected shared apps and their `restore_shared_files`.
3. `server deploy --apply` with the merged config.
4. Lock; `restore_shared_data` of each selected shared app.

Applications are restored afterwards with `app restore`. Their link records are back, so they keep their database credentials; when the server dump already holds their data, restore them without links: `--include sources,volumes,images`.

### 10.18 `depap backup list [--source server]`
Lists the files in the server's backup folder with their sizes. With the `local` target it is a usage error: local backups are files on this machine.

## 11. Security

- Secret files on the server (`secrets/`, `connectors/database/**`, `compose/depap.override.yaml`) are created with mode `0600`. Their directories use `0700`.
- Secrets never appear in `depap-server.yaml`, the effective `depap.yaml`, `state.yaml`, terminal output, `--dry-run` output (shown as `***`), or remote command lines.
- Overlay files are never uploaded as sources.
- Backup archives are written with mode `0600`. App archives hold no secrets from config, but database dumps hold the app's data. Server archives hold the shared apps' secrets and link credentials.
- Every instance linked to the same shared app shares that app's docker network. Network isolation between instances is a later feature.
- depap only uses the SSH server's existing host key checking. It never disables it.

## 12. Acceptance scenarios

These scenarios define "done" for 0.1. Each one also becomes an automated integration test (§14.3). Use `tls: off` or `internal` and `curl --resolve` for test domains.

### 12.1 Initialize
```
$ depap init
Created ~/.depap (manager key 0d6c2f5e-…)
$ depap init
Already initialized (manager key 0d6c2f5e-…)
```

### 12.2 Define a server and check its state
```
$ depap server define prod1 --address 203.0.113.10 --user depapuser
$ depap connect prod1
$ depap server state
prod1: UNINITIALIZED (no shared applications)
```
Expected: exit 0; `servers.yaml` contains `prod1`; nothing is created on the server.

### 12.3 Caddy and Postgres on the server
Working directory contains `depap-server.yaml` (§5.1) and `.depap-server.secrets.yaml` (§5.2).
```
$ depap server deploy            # render + upload, state: candidate
$ depap server deploy --apply    # start
$ depap server state
prod1: OK
  ingress   caddy-main  caddy 2      deployed  running
  database  postgres16  postgres 16  deployed  healthy
```
Expected: the layout of §8 exists; networks `depap-ingress` and `depap-db-postgres16` exist; `curl -i http://203.0.113.10/` gets a response from Caddy. Running the command again changes nothing (containers are not recreated).

### 12.4 Static web application
`docker-compose.yaml`:
```yaml
services:
  front-webapp:
    build: .        # Dockerfile: FROM nginxinc/nginx-unprivileged:alpine (listens on 8080), COPY ui/ /usr/share/nginx/html/ui/
```
`depap.yaml`:
```yaml
name: mystaticweb
deployment:
  build: true
services:
  ingress:
    - name: ui-front
      routes:
        - name: main
          target: webapp
          port: 8080
          path: /ui
app_services:
  - name: webapp
    service: front-webapp
```
`.depap-staging.yaml`:
```yaml
prefix: staging
services:
  ingress:
    - name: ui-front
      domains: ["staging.domain.tld", "www.staging.domain.tld"]
```
```
$ depap app deploy --apply -f depap.yaml -f .depap-staging.yaml
```
Expected:
- `apps/mystaticweb/staging/sources/` contains the repository, without `.depap-staging.yaml`.
- Compose project `mystaticweb-staging` is running.
- `conf/sites/mystaticweb-staging-ui-front.caddy` exists and Caddy was reloaded, not restarted.
- `curl --resolve staging.domain.tld:443:203.0.113.10 https://staging.domain.tld/ui/` returns the page.
- `/other` returns 404.

### 12.5 Web application with API and database
`docker-compose.yaml` services: `front-webapp`, `backend-webapp` (listens on 5000), `backend-migration` (runs once, exits 0).

`depap.yaml`:
```yaml
name: myapp
deployment:
  build: true
services:
  database:
    - name: system-data
      type: postgres
      version: ">=16"
      targets: [backend, migration]
      environment:
        POSTGRES_USER: db_user
        POSTGRES_PASSWORD: db_password
        POSTGRES_DB: db_name
        POSTGRES_HOST: db_host
        POSTGRES_URL: db_url
  ingress:
    - name: ui-front
      routes:
        - name: ui
          target: webapp
          port: 8080
          path: /ui
        - name: api
          target: backend
          port: 5000
          path: /api
    - name: api-front
      routes:
        - name: api
          target: backend
          port: 5000
          path: /
app_services:
  - name: webapp
    service: front-webapp
  - name: migration
    service: backend-migration
  - name: backend
    service: backend-webapp
    environment:
      APP_MODE: production
      APP_FEATURE_USERS: "true"
```
`.depap-staging.yaml`:
```yaml
prefix: staging
services:
  database:
    - name: system-data
      db_name: mystagingappdb
      db_user: myappuser
      db_password: myapppassword
  ingress:
    - name: ui-front
      domains: ["staging.domain.tld", "www.staging.domain.tld"]
    - name: api-front
      domains: ["api.staging.domain.tld"]
app_services:
  - name: backend
    environment:
      APP_MODE: staging
```
```
$ depap app deploy --apply -f depap.yaml -f .depap-staging.yaml
```
Expected:
- `depap-env.yaml` pins `database: postgres16 (16)` and `ingress: caddy-main (2)`.
- Role `myappuser` and database `mystagingappdb` exist in `postgres16`.
- `docker compose -p myapp-staging exec backend-webapp env` shows `POSTGRES_HOST=postgres16`, `POSTGRES_DB=mystagingappdb`, `APP_MODE=staging`, `APP_FEATURE_USERS=true`.
- `backend-migration` exited 0, and `app state` reports OK.
- `https://staging.domain.tld/api/…` and `https://api.staging.domain.tld/…` both reach the backend.
- `depap connector list` shows both ingress links and the database link.
- Deploying again with no changes keeps the data and the password and does not recreate containers.
- `depap app remove -f depap.yaml -f .depap-staging.yaml --yes` removes the site files and containers but keeps the database. Adding `--purge database` drops it.

## 13. Changes compared to the draft

| Draft | MVP | Why |
|---|---|---|
| `depap-`/`depapp-`, `.depap-server-deployment.yaml`, `.depap-deploy.yaml`, `.depap.yaml` | `depap-server.yaml`, `.depap-server.secrets.yaml`, `depap.yaml`, `.depap-<env>.yaml` | One naming scheme |
| Mixed `kebab-case` keys, `app-services` | `snake_case` everywhere, `app_services` | Matches compose style, one rule |
| `--server_name`, `--server_address` | `-s/--server`, positional name in `server define` | Common CLI style |
| Ingress `links[].link` | `routes[].target` | "link" now only means the connection to a shared app |
| Database `links[]` nested in an entry, `links: [...]` | One database entry = one link with `targets: [...]` | Draft examples always had one link; simpler schema |
| `useprefix` | Defaults include the prefix; an explicit `db_name` is used as given | Removes a flag |
| Prefix defaults to the app name | Prefix defaults to `main` | Project name `<app>-<prefix>` stays readable |
| depap modifies the user's compose file | Generated override file | The user's file stays untouched; re-deploys are clean |
| Variables in `compose/.env` | `env_file` per service | `.env` only feeds interpolation; it does not reach containers |
| `compose/` holds a copy of the compose file | `sources/` holds the compose file, `compose/` holds generated files | Build contexts stay relative to the sources |
| Service name `<prefix>-front-webapp` | Network alias `<app>-<prefix>-<service>` | Unique across apps, works with compose naming |
| Restart ingress | Validate + reload | No downtime, bad config is rejected |
| `version: 1.0` (caddy), `16-slim`, `">16"` | `version` = major used for matching, `image` = tag, app uses a semver requirement | Version matching works on all three |
| State `candidate` written into the user's config | `state.yaml` | Config stays declarative |
| `ssl-enabled`, `optional` without a definition | `ssl-enabled` dropped; `optional` defined (§6.1) | |
| Flat hook list per shared app | core → kind → type layers with default (inherited) behavior | Generic logic written once per kind; types only add what differs |
| `apply_application` | `prepare_application` + `before_apply_application` | Lets `--dry-run` and render-only deploys work |
| `DEPAP_SERVER_HOME` pointer file (`depap_home.env`) | `server_home` in `servers.yaml` | Pointer file postponed |

## 14. Implementation notes

### 14.1 Language: Rust
Rust is a good fit:
- **Distribution**: a single static binary (`x86_64/aarch64-unknown-linux-musl`, macOS). This matters even more once the satellite has to run on servers.
- **Config correctness**: serde types with `deny_unknown_fields` turn most user mistakes into clear errors before the server is touched.
- **Path to server mode**: `axum` + `tokio` are a solid base for the later API and WebUI backend.

Costs to manage: slower iteration than Go or Python, and async complexity. For 0.1:
- **Synchronous code** (no tokio). All work is sequential process calls.
- **No Docker or SSH libraries**: depap runs the system `ssh` binary and `docker compose` on the host. Users' `~/.ssh/config`, agents, jump hosts and known_hosts work with no extra code. A native SSH implementation (`russh`) can come later behind the same trait.

Suggested crates: `clap` (derive), `serde` + a maintained YAML crate (`serde_yaml` is archived; use `serde_yaml_ng` or `serde-saphyr`), `minijinja` (templates), `include_dir` (embedded templates), `anyhow` + `thiserror`, `uuid`, `semver`, `sha2`, `tar`, `ignore` (`.depapignore` matching), `comfy-table` (output); tests: `insta`, `assert_cmd`, `tempfile`.

### 14.2 Module layout
```
src/
  main.rs            # clap entry, exit codes
  cli/               # one file per command group
  config/            # schemas, merge (§7), validation, effective config
  connector/         # trait Connector { exec, exec_with_stdin, upload_tar, read_file, write_file }
    local.rs, ssh.rs
  server/            # state checks, deploy, lock, state.yaml
  app/               # compose parsing, override builder (Contribution), deploy/remove
  shared/
    mod.rs           # trait Kind, kind registry
    ingress/
      mod.rs         # trait IngressType, impl Kind, type registry
      base.rs        # generic ingress behavior (defaults)
      caddy/ (mod.rs, templates/)
    database/
      mod.rs         # trait DatabaseType, impl Kind, type registry
      base.rs        # generic database behavior (defaults)
      postgres/ (mod.rs, templates/)
```

### 14.3 Testing
- **Unit tests**: merge rules, validation, name derivation, rendering (snapshot tests of generated compose, Caddyfile, site files and override).
- **Integration tests** (`cargo test --features it`, CI on a runner with Docker): §12 scenarios against the `local` connector with `tls: off`, plus one SSH run against a container running `sshd` + Docker-in-Docker.

## 15. Roadmap

| Release | Adds | Plugs into |
|---|---|---|
| 0.2 | `app logs`, `app restart`, `server deploy --prune`, `-o json` output, Postgres tags and host port publishing | `state.yaml`, `DatabaseType` |
| 0.3 | Backup/restore (§10.13–§10.18, done), `s3` backup target, S3 link | `Kind` backup hooks, new `s3` kind (`Kind` + `S3Type`) |
| 0.4 | OIDC (Keycloak) link: realms, clients, users | New `oidc` kind (`Kind` + `OidcType`) |
| 0.5 | Logs (OpenObserve) and metrics | Hooks that add to the compose override |
| 0.6 | Rollback (keep N previous `apps/…` revisions), per-instance networks | Atomic directory swap |
| 1.0 | `depap-server` mode: API + WebUI; satellites that register with a parent server | `Connector` trait gets a `satellite` implementation |
