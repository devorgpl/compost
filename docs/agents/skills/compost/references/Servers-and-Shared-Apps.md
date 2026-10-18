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

With a key (`--identity-file` of `server define`), compost signs in with that key only (`IdentitiesOnly=yes`), not with the other keys in your agent. To use another key for one command, add `--ssh-key-file <path>` to any command, or set `COMPOST_SSH_KEY_FILE`:

```sh
compost --ssh-key-file ~/.ssh/id_rsa_admin server state
```

The file must exist, and the option only works with SSH servers. A connection that is still open from a command in the last 60 s is reused as is, so the key only matters when compost opens a new connection.

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

The output of a shared app's containers, like [`app logs`](Applications.md#logs) for apps:

```sh
compost server logs caddy-main [--follow] [--tail 100] [--since 10m] [--timestamps]
compost server logs database/postgres18     # <kind>/<name> when two kinds use the name
```

## Deploying shared apps

```sh
compost server generate [--with caddy,postgres,keycloak,openobserve,caddysecurebuild] [--force]
compost server deploy [--apply | --no-apply] [--pull] [--recreate] [-f files] [--secret file] [--dry-run [--diff]]
```

`server generate` writes starter files to the current folder (`compost-server.yaml`, `compost-server.prod.yaml`, `.compost-server.secrets.example.yaml`; see [Production setup](Production-Setup.md#3-shared-apps)). It adds types that a chosen type needs: `keycloak` brings `postgres` and `caddy`, `openobserve` brings `caddy`. Any ingress type meets the need for `caddy`, so `--with caddysecurebuild,keycloak` doesn't add `caddy`; choosing two ingress types is an error. `--list` shows the available types.

`server deploy` reads `compost-server.yaml` and `.compost-server.secrets.yaml` from the current folder, or the files given with `-f`, plus the secrets stored on the server with `--secret` ([Secrets](Secrets.md#stored-on-the-server)). It:

1. validates the config (exit 2 on errors, before anything touches the server);
2. creates compost's folder and the Docker networks;
3. renders each shared app's files and uploads them, generating missing passwords;
4. with `--apply`: starts every shared app with `docker compose up -d` and waits until it runs, then prints `server state`. A shared app starts after the shared apps it needs running (Keycloak after its database and Caddy); links to a monitoring app don't count, so Caddy can send its logs to the OpenObserve it serves.

Without `--apply`, only steps 1–3 run. `--dry-run` changes nothing and shows, compared with the server: networks to create, the files of each shared app that would be added, changed or removed (files with secrets only named), dropped links and shared apps left out of the config, and what `--apply` would do with each shared app: `created`, `recreated` (its compose files or override changed, `--recreate`, or its last deploy didn't finish), `reloaded without a restart` (Caddy, when only its other files changed), `pulled` (`--pull`) or `unchanged`. `--diff` adds the changed lines of files without secrets. Running it again changes nothing that didn't change: unchanged containers are not recreated. When only Caddy's sites changed, Caddy reloads them without a restart.

`up -d` pulls only images that are missing, so a new image behind the same tag is not fetched. To update a shared app, change its `version` or `image`, or add `--pull`: it runs `docker compose pull` for each shared app first, and `up` recreates those whose image changed. An image built on the server ([caddysecurebuild](#caddysecurebuild-ingress)) is rebuilt with `--pull` instead, on the newest base images. `--recreate` recreates every shared app's containers (`--force-recreate`) even when nothing changed; their data is kept, but apps using them lose them while they restart. Both need `--apply`. Mind major versions: Postgres does not start on the data of an older major version.

When a shared app loses a link (say its `monitoring` is removed), the deploy with `--apply` removes the link and recreates the shared app without it.

Shared apps removed from the config are reported as `orphaned` and keep running; stop them with `docker compose -p compost-<kind>-<name> down`.

## Shared app types

A server has at most one ingress, and any number of databases (Postgres, MariaDB, MySQL), key-value stores (Valkey, Redis), storage (Garage, external S3), OIDC providers, monitoring apps and communication apps (channels for notifications: webhook, Slack, Mattermost, Discord, Matrix, email; see [Notifications](Notifications.md)). Every shared app has a `name` (lowercase letters, digits and `-`, up to 31 characters, starting with a letter), a `type`, a `version` and optionally an `image`. Type-specific settings go in `configuration`.

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

### caddysecurebuild (`ingress`)

Caddy as above, built on the server with request filtering, next to a [CrowdSec](https://www.crowdsec.net/) agent:

- **Bans.** The agent reads Caddy's access log and bans addresses that attack, e.g. a burst of more than 5 POST requests answered 401 or 403 (brute-force logins; one is forgotten every 10 seconds), path traversal, SQL injection probes or scans for admin pages. It also gets CrowdSec's community blocklist. A banned address gets 403 on every site that enforces bans, for `ban_duration`.
- **AppSec.** Caddy sends every request to CrowdSec's AppSec component first, which blocks known exploits (CrowdSec's virtual patching and generic rules) with 403. Optionally, it also checks the OWASP Core Rule Set, out of band: a match doesn't block the request, but repeated matches ban the address.
- **Rate limits.** Zones you define: at most `events` matching requests per address within `window`; more get 429.

All keys of [caddy](#caddy-ingress) apply, plus `security`. Everything in it is optional:

```yaml
shared:
  ingress:
    - name: caddy-main
      type: caddysecurebuild
      version: 2
      configuration:
        tls: auto
        acme_email: admin@example.com
        security:
          crowdsec:
            whitelist: [203.0.113.0/24]   # never banned: your office, monitoring
            ban_duration: 4h
          appsec:
            crs: false
          defaults:                       # what a site gets unless its own `security` says otherwise
            bans: true
            appsec: true
            rate_limits: [login]          # default: every zone below
          rate_limits:
            - name: login
              paths: [/login, /wp-login.php, /user/login]
              methods: [POST]
              events: 10
              window: 1m
```

| Key | Default | Meaning |
|---|---|---|
| `version` | `2` | Major version of the `caddy:<version>-builder` and `caddy:<version>-alpine` images the build uses |
| `image` | – | Not accepted: the image is built |
| `security.crowdsec.whitelist` | `[]` | IP addresses and networks (`203.0.113.0/24`) that are never banned. CrowdSec never bans private addresses either (`10.0.0.0/8`, `192.168.0.0/16`, …) |
| `security.crowdsec.ban_duration` | `4h` | How long a ban lasts, e.g. `30m`, `1h30m` |
| `security.crowdsec.collections` | `[]` | Hub collections installed besides `crowdsecurity/caddy`, `crowdsecurity/base-http-scenarios`, `crowdsecurity/appsec-virtual-patching` and `crowdsecurity/appsec-generic-rules`, e.g. `crowdsecurity/wordpress` |
| `security.crowdsec.scenarios` | `[]` | Hub scenarios installed besides those of the collections |
| `security.appsec.crs` | `false` | Also check the OWASP Core Rule Set (`crowdsecurity/appsec-crs`), out of band |
| `security.defaults.bans` | `true` | Sites enforce bans |
| `security.defaults.appsec` | `true` | Sites check requests with AppSec |
| `security.defaults.rate_limits` | all zones | Zones sites use, by name |
| `security.rate_limits[].name` | required | Zone name |
| `security.rate_limits[].paths` | every path | Request paths; `*` matches anything, e.g. `/api/*` |
| `security.rate_limits[].methods` | every method | e.g. `[POST]` |
| `security.rate_limits[].events` | required | Requests allowed per address in `window` |
| `security.rate_limits[].window` | required | e.g. `10s`, `1m` |
| `sites[].security` | – | A site's own choice, like an app's (see [Ingress links](Applications.md#ingress-links)) |

Apps choose per ingress entry with [`security`](Applications.md#request-filtering); an entry without it gets `defaults`. A site's zones count each address separately for each site. CrowdSec reads the log of every site, including sites with `bans: false`, so an address banned for attacking one site is blocked on all sites that enforce bans.

Changing `defaults` or a zone applies to every site on the next `server deploy`, with a reload. When you switch an existing `caddy` to this type, apps' sites get the defaults right away, but they share their rate-limit counters until each app's next `app deploy`. `server deploy` refuses to remove a zone that a deployed app's `security` names, and switching back to `type: caddy` while an app has `security`: deploy those apps without it first.

**The build.** The first `server deploy --apply` builds the image on the server (`build/Dockerfile` in the Caddy folder), which takes a few minutes and needs internet access. The image is tagged `compost-caddysecurebuild-<name>:<hash of the Dockerfile>`, so later deploys reuse it until compost pins newer modules. `--pull` rebuilds it on the newest Caddy images.

**The agent** is the container `compost-ingress-<name>-crowdsec` (image `crowdsecurity/crowdsec`). It's only on a network it shares with Caddy, so apps and other shared apps can't reach it. Its state (bans, its registration with CrowdSec) is in the Docker volumes `compost-ingress-<name>_crowdsec-data` and `_crowdsec-config`; it is not part of `server backup`. If the agent is down or still starting, requests pass unchecked rather than failing.

To see and lift bans:

```sh
docker exec compost-ingress-caddy-main-crowdsec cscli decisions list
docker exec compost-ingress-caddy-main-crowdsec cscli alerts list
docker exec compost-ingress-caddy-main-crowdsec cscli decisions delete --ip 198.51.100.7
```

Lock yourself out of a site and it's usually a ban: lift it, then add your address to `whitelist`. Behind another proxy or a CDN, every request comes from the proxy's address; this type is for servers that face the internet directly.

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

### mariadb and mysql (`database`)

A MariaDB or MySQL server. They work alike: each app link gets its own database (`utf8mb4`) and a user with all rights on it, and nothing else.

```yaml
shared:
  database:
    - name: mariadb11
      type: mariadb             # or mysql
      version: 11               # mysql: 8
      configuration:
        root_password: …        # in .compost-server.secrets.yaml
```

| Key | Default | Meaning |
|---|---|---|
| `version` | `11` (mariadb), `8` (mysql) | Major version, matched against apps' `version` requirements |
| `image` | `mariadb:<version>`, `mysql:<version>` | An official image or a compatible one |
| `configuration.root_password` | generated | Password of `root`. Secret, without quotes or `\`. Can't change after the first deploy |

Like Postgres, it is reached only on `compost-db-<name>`, at host name `<name>`, port 3306. User names are at most 32 characters (MySQL's limit, kept for MariaDB too, so databases can move between them): a longer default name is cut and ends in a short hash, e.g. `shop_production_custo_5f6407f3`; `app state` shows it.

Backups use `mariadb-dump` / `mysqldump` with `--single-transaction`, so InnoDB tables are copied consistently without stopping the app. App dumps leave out `DEFINER` clauses, so they load into a copy with another user. A server backup holds every database, user and grant, and restores onto the same major version.

### valkey and redis (`keyvalue`)

A key-value store that apps link to (see [Key-value links](Applications.md#key-value-links)). Valkey is the open-source (BSD) fork of Redis; Redis 8 is licensed AGPLv3 or RSAL/SSPL.

```yaml
shared:
  keyvalue:
    - name: valkey
      type: valkey             # or redis
      version: 9               # redis: 8
      configuration:
        max_memory: 512mb
        eviction: allkeys-lru  # for a cache
        password: …            # in .compost-server.secrets.yaml
```

| Key | Default | Meaning |
|---|---|---|
| `version` | `9` (valkey), `8` (redis) | Major version. Valkey needs 9.1 or newer (the `9` image is): it locks each app's user to its own database |
| `image` | `valkey/valkey:<version>`, `redis:<version>` | |
| `configuration.password` | generated | Password of the `default` user, for compost and you. Secret; a new value replaces the old one with the next deploy |
| `configuration.max_memory` | none | e.g. `512mb`: above it the server evicts keys by `eviction` |
| `configuration.eviction` | `noeviction` | `allkeys-lru`, `allkeys-lfu`, `allkeys-random`, `volatile-lru`, `volatile-lfu`, `volatile-random` or `volatile-ttl` |

It is reached only on `compost-kv-<name>`, at host name `<name>`, port 6379, and keeps its data with AOF (every write is logged) in `data/`. Each link's user is in the server's ACL file, which compost writes from the link records (passwords only as hashes) and loads with `ACL LOAD`, so linking an app never restarts it; a change of `password`, `max_memory` or `eviction` does.

A server backup takes a fresh snapshot (`dump.rdb`) of all databases; `server restore` loads it and then turns AOF on again.

### garage and s3 (`storage`)

S3-compatible object storage that apps link to (see [Storage links](Applications.md#storage-links)).

**garage**: compost runs [Garage](https://garagehq.deuxfleurs.fr), a small S3 server (AGPL), as a single node. Each link gets its own bucket and key.

```yaml
shared:
  storage:
    - name: storage
      type: garage
      version: 2
      configuration:
        url: https://files.example.com   # optional: the S3 API on this domain, through the ingress
```

| Key | Default | Meaning |
|---|---|---|
| `version` | `2` | Major version; compost pins the image (`dxflrs/garage:v2.3.0`) |
| `image` | – | Another image |
| `configuration.url` | – | Public URL of the S3 API, served by the ingress; apps get it as `s3_public_endpoint` |
| `configuration.region` | `garage` | Region name clients sign requests with |
| `configuration.capacity` | `100G` | The node's weight in Garage's layout; not a quota |

It runs as the server user, keeps its metadata and objects in `meta/` and `data/`, and is reached on `compost-storage-<name>` at `http://<name>:3900`. The first link applies a one-node layout. Garage covers what apps usually need (buckets, multipart uploads, presigned URLs) but not everything AWS has: no versioning, object lock or bucket policies. A server backup stops Garage briefly and copies its folders.

A Garage on one server can also be the [S3 backup target](Backup-and-Restore.md#s3-targets) of another: create a bucket and key for it with `docker exec compost-storage-<name> /garage …` and use its `url` as `endpoint`.

**s3**: an S3 service compost doesn't run. It needs only `external`; links pin it with `instance` and give their bucket and key.

```yaml
shared:
  storage:
    - name: aws
      type: s3
      external:
        endpoint: https://s3.eu-central-1.amazonaws.com
        region: eu-central-1
        network: minio_default       # optional: a Docker network to reach it on
```

`server state` shows it as `not checked`: only the links have credentials.

MinIO is not a built-in type: its community edition is distributed as source only since October 2025 and its repository was archived in April 2026. An existing MinIO works as `s3`. RustFS and SeaweedFS may become types later.

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

A database compost doesn't run (an existing Postgres, MariaDB or MySQL container, or a hosted one such as AWS RDS) can be added with `external`. Apps then link to it like to any database, and `app backup` dumps it.

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
| `external.port` | `5432`, `3306` | |
| `external.network` | – | Existing Docker network the database is on; linked services join it. Without it, containers reach the host like any other address |
| `external.sslmode` | – | `disable`, `allow`, `prefer`, `require`, `verify-ca` or `verify-full`; used by compost's own clients, and added to `db_url` for Postgres (MySQL drivers each name it differently: set it in the app) |
| `external.allow_purge` | `false` | Let `--purge database` drop databases there (needs root credentials) |
| `configuration.root_user` | `postgres`, `root` | Superuser, if compost may create databases there |
| `configuration.root_password` | – | Its password; secret. A new value replaces the stored one; a deploy without it keeps the stored credentials |

What compost does with it:
- **Runs nothing.** `server deploy` only checks that `network` exists and stores how to reach it. `server state` checks that it answers (`external, accepting connections`); if it doesn't, the state is `DEGRADED`.
- **Links pin it.** A link uses an external database only with `instance: <name>`, so adding one never changes which database other apps get.
- **Without root credentials**, links name an existing database, user and password (`db_name`, `db_user`, `db_password`), and compost creates nothing. **With them**, compost creates databases and users as on its own Postgres.
- **Backups** run `pg_dump` and `psql` (or the MariaDB/MySQL tools) in a temporary container of the type's image on the server. The credentials go into a file readable only by the server user, deleted after the run, never onto a command line. `server backup` leaves external databases out: `app backup` saves the apps' databases.
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

## Disk space

Pulls and builds leave old images and build cache behind. `server prune` shows how the server's disk is used, then removes what no container needs:

```sh
compost server prune --dry-run     # disk usage and what would be removed
compost server prune [--yes]
```

```
Disk /: 41.2 GB free of 80.0 GB (49% used)
Docker:
  Images            16.0GB  reclaimable 12.5GB (78%)
  Build Cache       10.2GB  reclaimable 5.4GB
compost: backups folder 3.2 GB, releases 410.0 MB (sources; their images are in Docker's)

Unused images to remove:
  myorg/web:1.4                         210MB
  compost-caddysecurebuild-caddy-main:9f2c…  95MB
Also removed: dangling images (<none>) and the build cache.
Kept: 12 release images for `app rollback` (`deployment.releases` sets how many).
```

It removes every tagged image that no container uses, running or stopped, including images of other software on the server; the dangling images; and all of the build cache, so the next `build: true` deploy builds from scratch. The images kept for [rollback](Applications.md#releases-and-rollback) (`compost-release/…`) stay: an image that is also a release image only loses its other name. Volumes and containers are never touched, and neither are backups: [`backup prune`](Backup-and-Restore.md#deleting-old-archives) deletes old archives. It asks before removing anything unless `--yes`, and holds the server's lock, so it never removes an image that a running deploy has pulled but not started yet.

## Locks

Commands that change a server hold a lock (`~/.compost-server/lock/`), so two can't run at once. If a command was killed and left it behind, the next one prints who held it; add `--force-unlock` to remove it.
