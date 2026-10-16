# Servers and shared apps

## Defining servers

```sh
compost server define prod --address 203.0.113.10 [--user compostuser] [--port 22] [--identity-file ~/.ssh/id_ed25519] [--server-home <path>]
compost server define dev --local          # this machine, no SSH
compost server define prod --address 198.51.100.7 --force   # replace; keeps the server's settings
compost server list
compost connect prod                       # default server for following commands
compost disconnect
```

Defining a server only writes `~/.compost/servers.yaml` (see [Configuration](Configuration.md#serversyaml)). compost's folder on the server, `--server-home`, defaults to `~/.compost-server`.

Every command that talks to a server uses, in order: `-s/--server <name>`, the `COMPOST_SERVER` environment variable, the context from `connect`. With none of them set it fails with exit code 2.

### Connections

compost runs POSIX `sh` scripts on the server through a single `ssh` call each, and streams files as tar. It shares one SSH connection per run and keeps it open 60 s for the next command (`ControlMaster`, sockets in `~/.compost/ssh/`). It uses your SSH config, agent and host key checking as they are.

Add `-v` to any command to print every script it runs on the server.

## Server state

```sh
compost server state
```

| Status | Meaning | Exit code |
|---|---|---|
| `OK` | compost manages the server and every shared app runs (and is healthy, if it has a healthcheck) | 0 |
| `UNINITIALIZED` | Reachable, nothing deployed by compost yet | 0 |
| `DEGRADED` | Some shared app is not running | 4 |
| `UNREACHABLE` | The SSH connection fails | 3 |
| `NO_DOCKER` | `docker` or `docker compose` doesn't work for the server user | 3 |
| `FOREIGN` | Another manager key manages this server (see [Installation](Installation.md#set-up)) | 3 |

Commands that change a server run these checks first, and stop on `UNREACHABLE`, `NO_DOCKER` and `FOREIGN`.

## Deploying shared apps

```sh
compost server generate [--with caddy,postgres,keycloak,openobserve] [--force]
compost server deploy [--apply | --no-apply] [-f files] [--dry-run]
```

`server generate` writes starter files to the current folder (`compost-server.yaml`, `compost-server.prod.yaml`, `.compost-server.secrets.example.yaml`; see [Production setup](Production-Setup.md#3-shared-apps)). It adds types that a chosen type needs: `keycloak` brings `postgres` and `caddy`, `openobserve` brings `caddy`. `--list` shows the available types.

`server deploy` reads `compost-server.yaml` and `.compost-server.secrets.yaml` from the current folder, or the files given with `-f`. It:

1. validates the config (exit 2 on errors, before anything touches the server);
2. creates compost's folder and the Docker networks;
3. renders each shared app's files and uploads them, generating missing passwords;
4. with `--apply`: starts every shared app with `docker compose up -d` and waits until it runs, then prints `server state`. A shared app starts after the shared apps it needs running (Keycloak after its database and Caddy); links to a monitoring app don't count, so Caddy can send its logs to the OpenObserve it serves.

Without `--apply`, only steps 1–3 run. `--dry-run` shows what would happen and changes nothing. Running it again changes nothing that didn't change: unchanged containers are not recreated. When only Caddy's sites changed, Caddy reloads them without a restart.

When a shared app loses a link (say its `monitoring` is removed), the deploy with `--apply` removes the link and recreates the shared app without it.

Shared apps removed from the config are reported as `orphaned` and keep running; stop them with `docker compose -p compost-<kind>-<name> down`.

## Shared app types

A server has at most one ingress, and any number of databases, OIDC providers and monitoring apps. Every shared app has a `name` (lowercase letters, digits and `-`, up to 31 characters, starting with a letter), a `type`, a `version` and optionally an `image`. Type-specific settings go in `configuration`.

### caddy (`ingress`)

Reverse proxy for every app's domains, with automatic HTTPS.

```yaml
shared:
  ingress:
    - name: caddy-main
      type: caddy
      version: 2
      configuration:
        http_port: 80
        https_port: 443
        tls: auto
        acme_email: admin@example.com
```

| Key | Default | Meaning |
|---|---|---|
| `version` | `2` | Only `2` |
| `image` | `caddy:<version>-alpine` | |
| `configuration.http_port` | `80` | Host port |
| `configuration.https_port` | `443` | Host port, TCP and UDP |
| `configuration.tls` | `auto` | How sites get certificates: `auto`, `internal` or `off`, see [TLS](#tls) |
| `configuration.acme_email` | – | Contact for the ACME account. Only with `tls: auto` |
| `configuration.networks` | `[]` | Existing Docker networks Caddy joins too, so `sites` can reach containers compost doesn't manage. Never created: `server deploy` fails if one is missing |
| `configuration.sites[]` | `[]` | Sites written by hand: `name`, `domains`, and `config`, the inside of a Caddyfile site block |
| `configuration.access_log` | `false` | Log every request, as JSON, to Caddy's output. With [`monitoring`](#monitoring-shared-apps), the log goes to OpenObserve. An app's site gets it on the app's next `app deploy` |

```yaml
      configuration:
        networks: [shop_default]
        sites:
          - name: shop
            domains: [shop.example.com]
            config: |
              reverse_proxy backend:5000
```

`sites` are for services compost doesn't deploy, typically while [migrating](Migration.md). A domain belongs to a site or to an app, not both: `app deploy` refuses a domain a site serves, and `server deploy` refuses a site for a domain an app serves.

#### TLS

`configuration.tls` applies to every domain Caddy serves, apps' and `sites` alike:

| Value | Certificates | Ports |
|---|---|---|
| `auto` | From Let's Encrypt, renewed by Caddy. Every domain needs public DNS pointing to the server and port 80 or 443 reachable from the internet before the first deploy. HTTP redirects to HTTPS | `http_port` and `https_port` |
| `internal` | Signed by Caddy's own local CA. Browsers warn unless they trust its root certificate, `data/caddy/pki/authorities/local/root.crt` in the Caddy folder. For test servers and domains without public DNS | `http_port` and `https_port` |
| `off` | None: sites are served over plain HTTP. For a local machine, or behind another proxy that terminates TLS | `http_port` only; `https_port` is not published |

`acme_email` is the contact Let's Encrypt uses for expiry warnings; compost refuses the config when it is set with any `tls` other than `auto`. Certificates are kept in `data/` (see [Files and folders](Files-and-Folders.md)) and are part of `server backup`, so a restored server doesn't request them again.

With [`monitoring`](#monitoring-shared-apps), Caddy also serves its Prometheus metrics (requests, durations, sizes per host) on port 9180 inside its container, not published on the host, and the collector scrapes them. This needs Caddy 2.8 or newer; the default image is.

Caddy runs as the server user, so its files can be backed up and removed without root. Hosts and paths no app uses get a 404. When an app is deployed, Caddy is reloaded, not restarted; a site that fails `caddy validate` is rolled back and fails the deploy.

### postgres (`database`)

A Postgres server. Each app link gets its own database and user.

```yaml
shared:
  database:
    - name: postgres18
      type: postgres
      version: 18
      image: postgres:18-alpine
      configuration:
        root_password: …    # in .compost-server.secrets.yaml
```

| Key | Default | Meaning |
|---|---|---|
| `version` | required | Major version, matched against apps' `version` requirements |
| `image` | `postgres:<version>` | An official `postgres` image or a compatible one |
| `configuration.root_password` | generated | Superuser password. Secret. Can't change after the first deploy |

Postgres is not published on a host port. Only containers on its network `compost-db-<name>` reach it, at host name `<name>`, port 5432.

### keycloak (`oidc`)

An OpenID Connect provider for app logins. It needs a `postgres` database with version 18 or newer and the ingress, both on the same server: compost creates its database `compost_oidc_<name>_db` and routes the domain of `url` to it.

```yaml
shared:
  oidc:
    - name: keycloak
      type: keycloak
      version: 26
      configuration:
        url: https://auth.example.com
        admin_password: …   # in .compost-server.secrets.yaml
```

| Key | Default | Meaning |
|---|---|---|
| `version` | `26` | Major version |
| `image` | `quay.io/keycloak/keycloak:26.6` for 26 | Required for other versions |
| `configuration.url` | required | Public URL, `https://<domain>` or `http://<domain>[:port]`, no path |
| `configuration.admin_user` | `admin` | Administrator in realm `master` |
| `configuration.admin_password` | generated | Secret. Only used on the first start; can't change later |
| `configuration.database.instance` | – | The database to use, when several match |

Its data lives in that database: back them up and restore them together. Apps reach it on network `compost-oidc-<name>` at `http://<name>:8080`. Keycloak gets 180 s to start. See [Applications](Applications.md#oidc-links) for linking apps to it.

### openobserve (`monitoring`)

Collects logs, metrics and traces of apps and shared apps, and shows them in [OpenObserve](https://openobserve.ai)'s UI. It runs two containers:
- **openobserve** stores the data in `shared/monitoring/<name>/data/` and serves the UI. Only the collector and the ingress reach it.
- **collector**, an [OpenTelemetry Collector](https://opentelemetry.io/docs/collector/), receives what apps send and forwards it to OpenObserve. Apps reach it on network `compost-monitoring-<name>` at host name `<name>`, without credentials; only the collector knows OpenObserve's password.

```yaml
shared:
  monitoring:
    - name: monitoring
      type: openobserve
      version: 1
      configuration:
        url: https://logs.example.com
        retention_days: 30
        root_password: …    # in .compost-server.secrets.yaml
```

| Key | Default | Meaning |
|---|---|---|
| `version` | `1` | Major version |
| `image` | `openobserve/openobserve:vX.Y.Z` for 1 | Required for other versions |
| `configuration.url` | – | Public URL of the UI, `https://<domain>` or `http://<domain>[:port]`, no path. compost routes its domain through the ingress. Without it, the UI is not reachable |
| `configuration.root_email` | `admin@compost.local` | Login of the root user. Can't change after the first deploy |
| `configuration.root_password` | generated | Secret. 8 or more characters with a lowercase and an uppercase letter, a digit and a special character, as OpenObserve requires. Can't change after the first deploy |
| `configuration.retention_days` | OpenObserve's (10 years) | Days data is kept |
| `configuration.forward_port` | `24224` | Port on the server's `127.0.0.1` where Docker delivers the output of linked containers. Each monitoring app needs its own |
| `configuration.container_metrics` | `true` | CPU, memory, network and disk I/O of every container on the server. The collector reads them from the Docker socket, mounted read-only; that gives the collector control over Docker, like root on the server. `false` leaves the socket out |
| `configuration.collector_image` | `otel/opentelemetry-collector-contrib:X.Y.Z` | |

What ends up in OpenObserve (organization `default`):
- **Logs** stream `docker`: the output of linked containers, with `fluent_tag` (`<instance>.<service>`: the compose service, e.g. `myapp-prod.backend`; for a shared app its plain name, e.g. `compost-ingress-caddy-main.caddy`), `container_name`, `source` (`stdout`/`stderr`) and the Compose project and service. A line that is a JSON object, like Caddy's log, is stored as fields: `body_level`, `body_msg`, `body_request_method`, `body_status` and so on, nested objects below the third level as JSON text (e.g. `body_request_headers`). Other lines stay in `body`.
- **Logs** stream `default`, **traces** and **metrics** that apps send with OpenTelemetry (OTLP).
- **Metrics** scraped from the Prometheus endpoints of links, labeled `compost_app`, `compost_prefix` and `compost_service` (named like in `fluent_tag`), and the `container_*` metrics.

Log in with `root_email` and `root_password`; compost writes both to `connectors/monitoring/<name>/root-connector.yaml` on the server. Apps link to it with [`services.monitoring`](Applications.md#monitoring-links).

The log driver sends asynchronously: linked containers start and run while the collector is down, and `docker logs` keeps working.

## Monitoring shared apps

`monitoring` on a shared app sends its output, and the metrics it serves, to a monitoring app:

```yaml
shared:
  ingress:
    - name: caddy-main
      type: caddy
      configuration:
        access_log: true
      monitoring: {}
  database:
    - name: postgres18
      type: postgres
      version: 18
      monitoring: {metrics: false}
```

| Key | Default | Meaning |
|---|---|---|
| `monitoring.instance` | – | The monitoring app to use, when there are several |
| `monitoring.logs` | `true` | Collect the output of its containers |
| `monitoring.metrics` | `true` | Scrape its metrics, if it serves any: Caddy does |

Caddy, Postgres and Keycloak can be monitored. External shared apps and monitoring apps can't. It works in both directions: OpenObserve's domain is served by the Caddy that sends its logs to it. On the first deploy, Caddy starts before OpenObserve, and Docker keeps Caddy's output in a small buffer and sends it once the collector runs.

## External databases

A database compost doesn't run (an existing Postgres container, or a hosted one such as AWS RDS) can be added with `external`. Apps then link to it like to any database, and `app backup` dumps it.

```yaml
shared:
  database:
    - name: shop-db                    # an existing container on this server
      type: postgres
      version: 16                      # its major version
      external:
        host: db                       # service or container name
        network: shop_default          # existing network to reach it on
    - name: rds-main                   # AWS RDS
      type: postgres
      version: 16
      external:
        host: main.abc123.eu-central-1.rds.amazonaws.com
        sslmode: require
      configuration:                   # optional; root_password in the secrets file
        root_user: postgres
        root_password: …
```

| Key | Default | Meaning |
|---|---|---|
| `external.host` | required | Host name or address, as the app's containers see it |
| `external.port` | `5432` | |
| `external.network` | – | Existing Docker network the database is on; linked services join it. Without it, containers reach the host like any other address |
| `external.sslmode` | – | `disable`, `allow`, `prefer`, `require`, `verify-ca` or `verify-full`; added to `db_url` and used by compost's own clients |
| `external.allow_purge` | `false` | Let `--purge database` drop databases there (needs root credentials) |
| `configuration.root_user` | `postgres` | Superuser, if compost may create databases there |
| `configuration.root_password` | – | Its password; secret |

What compost does with it:
- **Runs nothing.** `server deploy` only checks that `network` exists and stores how to reach it. `server state` checks that it answers (`external, accepting connections`); if it doesn't, the state is `DEGRADED`.
- **Links pin it.** A link uses an external database only with `instance: <name>`, so adding one never changes which database other apps get.
- **Without root credentials**, links name an existing database, user and password (`db_name`, `db_user`, `db_password`), and compost creates nothing. **With them**, compost creates databases and users as on its own Postgres.
- **Backups** run `pg_dump` and `psql` in a temporary `postgres:<version>-alpine` container on the server. The credentials go into an env file readable only by the server user, deleted after the run, never onto a command line. `server backup` leaves external databases out: `app backup` saves the apps' databases.
- **Never destroys data by accident.** `--purge database` keeps databases there, with a warning, unless root credentials are set and `allow_purge: true`.
- Removed from `compost-server.yaml` while no app links to it, it is forgotten by the next `server deploy`.

Database names and users must be lowercase letters, digits and `_` (as compost's own); rename others before linking.

## Inspecting links

```sh
compost connector list
```

```
database
└─ postgres18  postgres 18  running
   ├─ myapp-prod/main-db  database myapp_prod_main_db, user myapp_prod_main_db  app: deployed
   └─ myapp-staging/main-db  database myapp_staging_main_db, user myapp_staging_main_db  app: deployed
ingress
└─ caddy-main  caddy 2  running
   ├─ myapp-prod/site  example.com  / → myapp-prod-web:8080  app: deployed
   └─ myapp-staging/site  staging.example.com  / → myapp-staging-web:8080  app: deployed
```

Links of shared apps (Keycloak's database and domain) show as `shared <kind>/<name>/<entry>`. A link whose instance no longer exists shows as a leftover record.

## Locks

Commands that change a server hold a lock (`~/.compost-server/lock/`), so two can't run at once. If a command was killed and left it behind, the next one prints who held it; add `--force-unlock` to remove it.
