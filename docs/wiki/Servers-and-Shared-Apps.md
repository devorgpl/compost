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

A command in a shared app's container, like [`app exec`](Applications.md#commands-in-containers), and named commands from `commands` in compost-server.yaml ([Admin commands](Admin-Commands.md)):

```sh
compost server exec postgres18 -- psql -U postgres
compost server exec caddy-main --service crowdsec -- cscli decisions list
compost server cmd                                  # lists the named commands
compost server cmd unban 198.51.100.7
```

## Deploying shared apps

```sh
compost server generate [--with caddy,postgres,keycloak,openobserve,caddysecurebuild] [--force]
compost server deploy [--apply | --no-apply] [--pull] [--recreate] [-f files] [--secret file] [--dry-run [--diff]]
```

`server generate` writes starter files to the current folder (`compost-server.yaml`, `compost-server.prod.yaml`, `.compost-server.secrets.example.yaml`; see [Production setup](Production-Setup.md#3-shared-apps)). It adds types that a chosen type needs: `keycloak` brings `postgres` and `caddy`, `openobserve` brings `caddy`, `temporal` brings `postgres`, `caddy` and `keycloak` (for its UI's login). Any ingress type meets the need for `caddy`, so `--with caddysecurebuild,keycloak` doesn't add `caddy`; choosing two ingress types is an error. `--list` shows the available types.

`server deploy` reads `compost-server.yaml` and `.compost-server.secrets.yaml` from the current folder, or the files given with `-f`, plus the secrets stored on the server with `--secret` ([Secrets](Secrets.md#stored-on-the-server)). It:

1. validates the config (exit 2 on errors, before anything touches the server);
2. creates compost's folder and the Docker networks;
3. renders each shared app's files and uploads them, generating missing passwords;
4. with `--apply`: starts every shared app with `docker compose up -d` and waits until it runs, then prints `server state`. A shared app starts after the shared apps it needs running (Keycloak after its database and Caddy); links to a monitoring app don't count, so Caddy can send its logs to the OpenObserve it serves.

Without `--apply`, only steps 1–3 run. `--dry-run` changes nothing and shows, compared with the server: networks to create, the files of each shared app that would be added, changed or removed (files with secrets only named), dropped links and shared apps left out of the config, and what `--apply` would do with each shared app: `created`, `recreated` (its compose files or override changed, `--recreate`, or its last deploy didn't finish), `reloaded without a restart` (Caddy, when only its other files changed), `pulled` (`--pull`) or `unchanged`. `--diff` adds the changed lines of files without secrets. Running it again changes nothing that didn't change: unchanged containers are not recreated. When only Caddy's sites changed, Caddy reloads them without a restart.

`up -d` pulls only images that are missing, so a new image behind the same tag is not fetched. To update a shared app, change its `version` or `image`, or add `--pull`: it runs `docker compose pull` for each shared app first, and `up` recreates those whose image changed. An image built on the server ([caddysecurebuild](#caddysecurebuild-ingress)) is rebuilt with `--pull` instead, on the newest base images. `--recreate` recreates every shared app's containers (`--force-recreate`) even when nothing changed; their data is kept, but apps using them lose them while they restart. Both need `--apply`. Mind major versions: Postgres does not start on the data of an older major version.

When a shared app loses a link (say its `monitoring` is removed), the deploy with `--apply` removes the link and recreates the shared app without it.

Shared apps removed from the config are reported as `orphaned` and keep running; stop them with `docker compose -p compost-<kind>-<name> down`. On this machine, `compost dev up` adds the shared apps an app needs by itself ([Local development](Local-Development.md#starting-an-app-dev-up)); a later `server deploy` from a file without them reports them as `orphaned`.

Two shared apps can't have the same `host_port`.

## Shared app types

A server has at most one ingress, and any number of databases (Postgres, MariaDB, MySQL), key-value stores (Valkey, Redis), storage (Garage, external S3), OIDC providers, monitoring apps, mail catchers (Mailpit), workflow servers (Temporal) and communication apps (channels for notifications: webhook, Slack, Mattermost, Discord, Matrix, email; see [Notifications](Notifications.md)). Every shared app has a `name` (lowercase letters, digits and `-`, up to 31 characters, starting with a letter), a `type`, a `version` and optionally an `image`. Type-specific settings go in `configuration`.

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

With [`monitoring`](#monitoring-shared-apps), Caddy also serves its Prometheus metrics (requests, durations, sizes per host) on port 9180 inside its container, not published on the host, and the collector scrapes them. This needs Caddy 2.8 or newer; the default image is. With `monitoring.traces: true` it also traces every request, see [Request tracing](#request-tracing).

Caddy runs as the server user, so its files can be backed up and removed without root. Hosts and paths no app uses get a 404. When an app is deployed, Caddy is reloaded, not restarted; a site that fails `caddy validate` is rolled back and fails the deploy.

### caddysecurebuild (`ingress`)

Caddy as above, built on the server with request filtering, next to a [CrowdSec](https://www.crowdsec.net/) agent:

- **Bans.** The agent reads Caddy's access log and bans addresses that attack, e.g. a burst of more than 5 POST requests answered 401 or 403 (brute-force logins; one is forgotten every 10 seconds), path traversal, SQL injection probes or scans for admin pages. It also gets CrowdSec's community blocklist. A banned address gets 403 on every site that enforces bans, for `ban_duration`, or has its connections closed ([`ban_action: drop`](#dropping-banned-connections)).
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
            ban_action: 403               # or drop: close banned addresses' connections
            ignore:                       # requests of every site CrowdSec never counts
              - {paths: [/healthz], methods: [GET]}
            simulated_scenarios: []       # scenarios that only raise alerts, never ban
          appsec:
            crs: false
            max_body: 1MB                 # how much of a request body AppSec checks
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
| `security.crowdsec.ban_action` | `403` | What a banned address gets: `403` on every request to a site that enforces bans, or `drop`: Caddy closes its connections before TLS or HTTP (see [Dropping banned connections](#dropping-banned-connections)) |
| `security.crowdsec.collections` | `[]` | Hub collections installed besides `crowdsecurity/caddy`, `crowdsecurity/base-http-scenarios`, `crowdsecurity/appsec-virtual-patching` and `crowdsecurity/appsec-generic-rules`, e.g. `crowdsecurity/wordpress` |
| `security.crowdsec.scenarios` | `[]` | Hub scenarios installed besides those of the collections |
| `security.crowdsec.ignore[]` | `[]` | Requests of every site CrowdSec never counts towards a ban: a rule, or a [preset](#false-positives) name such as `matrix` |
| `security.crowdsec.ignore[].paths` | every path | Request paths; `*` matches any characters, `/` included |
| `security.crowdsec.ignore[].methods` | every method | e.g. `[GET]` |
| `security.crowdsec.ignore[].statuses` | every status | Response statuses, e.g. `[404]` |
| `security.crowdsec.simulated_scenarios` | `[]` | Scenarios in CrowdSec's simulation mode: their alerts show up in `alerts`, but they never ban |
| `security.appsec.crs` | `false` | Also check the OWASP Core Rule Set (`crowdsecurity/appsec-crs`), out of band |
| `security.appsec.max_body` | `1MB` | How much of a request body AppSec checks: `1MB`, `512KiB`, a number of bytes, or `0` for all of it (see [AppSec and request bodies](#appsec-and-request-bodies)) |
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
compost server cmd caddy-main.bans
compost server cmd caddy-main.alerts
compost server cmd caddy-main.check 198.51.100.7
compost server cmd caddy-main.alert 12                   # the requests behind alert 12
compost server cmd caddy-main.unban 198.51.100.7          # asks first
compost server cmd caddy-main.ban 198.51.100.7 30m       # 4h without a duration
compost server cmd caddy-main.unban-scenario crowdsecurity/http-probing   # every ban it made
```

These are [built into compost](Admin-Commands.md#built-in-commands). Other `cscli` commands run with `compost server exec caddy-main --service crowdsec -- cscli …`.

`compost server state` sums up what the agent did, under the ingress's line:

```
  ingress    caddy-main       caddysecurebuild 2  running
                              bans: 3 by this server's scenarios (crowdsecurity/http-probing 2, crowdsecurity/http-bad-user-agent 1), 14210 from blocklists
                              last 24h: 9 alerts, 4 simulated (crowdsecurity/http-crawl-non_statics 4)
```

To hear about bans without looking, add the [notification events](Notifications.md#what-is-sent) `bans` (each new ban, within 5 minutes) or `bans_daily` (a summary once a day).

Lock yourself out of a site and it's usually a ban: lift it, then add your address to `whitelist`. Behind another proxy or a CDN, every request comes from the proxy's address; this type is for servers that face the internet directly.

#### False positives

Some apps answer their own clients' normal requests with many 4xx responses, and CrowdSec takes that for an attack. The usual one is `crowdsecurity/http-probing`: many different paths answered 4xx within seconds, which is how scanners look for admin pages. Matrix clients such as Element do exactly that: they ask for the presence and profile fields of every member of the user's rooms, and the homeserver answers 404 for each one that has none. Every Element user is then banned within minutes. Registries (`HEAD` on missing blobs), S3 clients (missing keys) and SPAs probing optional endpoints can do the same. The other scenarios of `crowdsecurity/base-http-scenarios` look for scans of admin pages and backups, crawlers, path traversal, SQL injection and brute-force logins.

`bans: false` on a site doesn't help: CrowdSec still reads that site's requests, and the address it bans is blocked on every other site. To find out, look at the requests behind the ban:

```sh
compost server cmd caddy-main.check 198.51.100.7          # the ban, with its alert id
compost server cmd caddy-main.alert 12                   # each request: site, method, path, status
```

With [`ban_action: drop`](#dropping-banned-connections), a user banned by mistake sees no 403, only timeouts or reset connections on every site, so they can't tell you it's a ban: `caddy-main.check <their address>` is how to find out.

Then tell CrowdSec which requests not to count, with `ignore` rules. A rule matches requests that match all of its keys (`paths`, `methods`, `statuses`). In an app's ingress entry or a site's `security`, it only applies to that entry's domains:

```yaml
# compost.yaml of the Matrix homeserver
services:
  ingress:
    - name: matrix
      routes: [ … ]
      security:
        ignore:
          - matrix                        # a preset, the same as:
          - paths: [/_matrix/client/*/presence/*, /_matrix/client/*/profile/*]
            methods: [GET]
            statuses: [404]
```

Presets are rules for apps that are known to do this; mix them with your own rules:

| Preset | Rule | For |
|---|---|---|
| `matrix` | `paths: [/_matrix/client/*/presence/*, /_matrix/client/*/profile/*]`, `methods: [GET]`, `statuses: [404]` | Matrix homeservers (Synapse, Conduit, tuwunel) and their clients |
| `registry` | `paths: [/v2/*]`, `methods: [HEAD, GET]`, `statuses: [404]` | Docker registries: clients look for blobs and manifests before pushing them |
| `s3` | `methods: [HEAD]`, `statuses: [404]` | S3 storage such as [Garage](#garage-and-s3-storage): clients check for keys that don't exist yet |

Other requests to the same site are still checked, and so are the same paths with another method or status. A rule of `security.crowdsec.ignore` in compost-server.yaml applies to every site and needs at least one key. An entry's `ignore: [{}]` leaves all of its requests out. Bans, AppSec and rate limits stay as they are; compost marks the requests in Caddy's access log and a CrowdSec whitelist (`compost/ignore`) drops them before any scenario counts them. Then lift the bans the false positive made: `compost server cmd caddy-main.unban-scenario crowdsecurity/http-probing`.

`simulated_scenarios` is the blunter tool: those scenarios still raise alerts, which `caddy-main.alerts` shows, but never ban anyone. Taking a scenario off the list makes it ban again on the next `server deploy`.

#### Watching before banning

When you switch an existing server to this type, its users' normal traffic may look like an attack for reasons you can't foresee. To find out before anyone is locked out:

1. Put the scenarios that ban by request patterns in `simulated_scenarios`, e.g. `crowdsecurity/http-probing`, `crowdsecurity/http-crawl-non_statics` and `crowdsecurity/http-sensitive-files`, and deploy.
2. Subscribe to `bans` or `bans_daily` (see [Notifications](Notifications.md#what-is-sent)): simulated alerts are in them, marked as such. `server state` counts them too.
3. For each address that shouldn't be there, find the requests (`caddy-main.alert <id>`) and add `ignore` rules or presets.
4. When a few days pass without false positives, empty `simulated_scenarios` and deploy.

`defaults.bans: false` is no substitute: CrowdSec still bans, and the addresses are blocked as soon as you turn bans back on, for what's left of `ban_duration`.

#### Dropping banned connections

A 403 tells a scanner it has been noticed, which is its cue to switch addresses. With `ban_action: drop`, Caddy closes a banned address's connections on the HTTP and HTTPS ports before TLS or HTTP, so the server looks down to it:

```yaml
security:
  crowdsec:
    ban_action: drop
```

- It applies to bans only: this server's and the community blocklist's. AppSec still answers a blocked request with 403, since it judges one request, not the address.
- The connection is closed before Caddy knows which site it's for, so every site enforces bans: `server deploy` refuses `defaults.bans: false` or a site with `bans: false`, and `app deploy` refuses an entry with `bans: false`. An app deployed with it already makes `server deploy` fail: deploy it without it first.
- HTTP/3 (QUIC) connections aren't closed: a banned client that uses HTTP/3 still gets 403.
- Dropped connections never reach the access log, so they aren't in the monitoring app or in what CrowdSec reads. Bans, `server state` and the `bans` notifications come from the agent and work as before.
- Whitelisted and private addresses are never banned, so never dropped. SSH and other ports aren't touched: `caddy-main.unban` works for an admin who banned themselves.
- It needs the bouncer's layer4 module, so switching to `drop` (or back) changes `build/Dockerfile`, and the next `server deploy --apply` builds the image again.

#### AppSec and request bodies

AppSec checks the method, URL and headers of every request on a site with `appsec`, and the first `max_body` of its body (1 MB by default). Caddy holds that part of every request in memory while AppSec checks it, which is why there is a limit. With `0` it would read whole uploads, e.g. images pushed to a registry. A bigger body is checked by its beginning only, and the rest is passed on unchecked.

Bodies sent without a length, which browsers and clients do for streamed uploads over HTTP/2 and HTTP/3, aren't checked at all: AppSec gets the request without its body, and Caddy logs `dropping request body from AppSec check as it can't be buffered`. That warning is expected, not a fault.

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
| `configuration.host_port` | – | Also publish it on 127.0.0.1 of the server at this port, for processes outside Docker (see [Local development](Local-Development.md#running-the-app-outside-docker)) |

Containers on its network `compost-db-<name>` reach it at host name `<name>`, port 5432. Without `host_port` it is not published on a host port.

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
| `configuration.host_port` | – | Also publish it on 127.0.0.1 of the server at this port, for processes outside Docker (see [Local development](Local-Development.md#running-the-app-outside-docker)) |

Like Postgres, it is reached on `compost-db-<name>` (and on `host_port`, if set), at host name `<name>`, port 3306. User names are at most 32 characters (MySQL's limit, kept for MariaDB too, so databases can move between them): a longer default name is cut and ends in a short hash, e.g. `shop_production_custo_5f6407f3`; `app state` shows it.

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
| `configuration.host_port` | – | Also publish it on 127.0.0.1 of the server at this port, for processes outside Docker (see [Local development](Local-Development.md#running-the-app-outside-docker)) |

It is reached on `compost-kv-<name>` (and on `host_port`, if set), at host name `<name>`, port 6379, and keeps its data with AOF (every write is logged) in `data/`. Each link's user is in the server's ACL file, which compost writes from the link records (passwords only as hashes) and loads with `ACL LOAD`, so linking an app never restarts it; a change of `password`, `max_memory` or `eviction` does.

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
| `configuration.host_port` | – | Also publish its S3 API on 127.0.0.1 of the server at this port, for processes outside Docker (see [Local development](Local-Development.md#running-the-app-outside-docker)) |

It runs as the server user, keeps its metadata and objects in `meta/` and `data/`, and is reached on `compost-storage-<name>` at `http://<name>:3900`. The first link applies a one-node layout. Garage covers what apps usually need (buckets, multipart uploads, presigned URLs) but not everything AWS has: no versioning, object lock or bucket policies. A server backup stops Garage briefly and copies its folders.

A Garage on one server can also be the [S3 backup target](Backup-and-Restore.md#s3-targets) of another: create a bucket and key for it with `compost server exec <name> -- /garage …` and use its `url` as `endpoint`.

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
- **Logs** stream `default`, **traces** and **metrics** that apps send with OpenTelemetry (OTLP), and the ingress's traces with [request tracing](#request-tracing).
- **Metrics** scraped from the Prometheus endpoints of links, labeled `compost_app`, `compost_prefix` and `compost_service` (named like in `fluent_tag`), and the `container_*` metrics.

Log in with `root_email` and `root_password`; compost writes both to `connectors/monitoring/<name>/root-connector.yaml` on the server. Apps link to it with [`services.monitoring`](Applications.md#monitoring-links).

The log driver sends asynchronously: linked containers start and run while the collector is down, and `docker logs` keeps working.

### mailpit (`mail`)

A mail catcher for development and test servers: apps send mail to it instead of a real provider, and you read it in a web inbox. Nothing leaves the server. It runs up to three containers:

- **mailpit**, [Mailpit](https://mailpit.axllent.org): SMTP for apps, on port 1025 without TLS (any user and password are accepted), and the inbox.
- **ses**, [aws-ses-v2-local](https://github.com/domdomegg/aws-ses-v2-local): the AWS SES API (v1 and v2) on port 8005, so code that sends through SES runs unchanged. It hands every mail to Mailpit.
- **aws**, with `moto: true`: [moto](https://docs.getmoto.org), a mock of other AWS APIs (Cognito, SNS, SQS, …) on port 5000.

```yaml
shared:
  mail:
    - name: mail
      type: mailpit
      version: 1
      configuration:
        url: https://mail.test.example.com
        moto: true
        password: …    # in .compost-server.secrets.yaml
```

| Key | Default | Meaning |
|---|---|---|
| `version` | `1` | Major version of Mailpit |
| `image` | `axllent/mailpit:vX.Y.Z` for 1 | Required for other versions |
| `configuration.url` | – | Public URL of the inbox, `https://<domain>` or `http://<domain>[:port]`, no path. compost routes its domain through the ingress. Without it, the inbox is not reachable |
| `configuration.password` | generated | Secret. Password of the inbox's user `admin`; at least 8 characters, without `:` |
| `configuration.region` | `us-east-1` | The region links give apps (`aws_region`), unless a link sets its own `region` |
| `configuration.moto` | `false` | Also run moto for other AWS APIs. `dev up` turns it on for a link that maps `aws_endpoint` |
| `configuration.host_port` | – | For apps outside Docker ([`dev up --host`](Local-Development.md#running-the-app-outside-docker)): the SES API on this port of 127.0.0.1, SMTP on the next one, moto on the one after |
| `configuration.ses_image` | `ghcr.io/domdomegg/aws-ses-v2-local:X.Y.Z` | |
| `configuration.moto_image` | `motoserver/moto:X.Y.Z` | |

There is one inbox per mail app, for all apps linked to it. Log in as `admin` with the password; compost writes it to `connectors/mail/<name>/root-connector.yaml` on the server. Mail, and moto's data, stay in memory: a restart of the containers (a deploy that changes them, a reboot) empties them, and backups don't hold them. Apps link to it with [`services.mail`](Applications.md#mail-links).

### temporal (`workflow`)

A [Temporal](https://temporal.io) server for durable workflows, which apps link to (see [Workflow links](Applications.md#workflow-links)). It needs a `postgres` database with version 13 or newer on the same server: compost creates two databases on it, `compost_workflow_<name>_db` and `compost_workflow_<name>_visibility`. With `ui`, it also serves Temporal's web UI on a domain through the ingress, with a login through an OIDC provider.

```yaml
shared:
  workflow:
    - name: temporal
      type: temporal
      version: 1
      configuration:
        ui:
          url: https://temporal.example.com
          oidc: {realm: staff}       # users log in from this keycloak realm
```

| Key | Default | Meaning |
|---|---|---|
| `version` | `1` | Major version; compost pins the images (`temporalio/server:vX.Y.Z`) |
| `image` | – | Another server image |
| `configuration.admin_tools_image` | `temporalio/admin-tools` with the tag of the server image | Sets up the database schema and runs the `temporal` CLI for compost. Needed when `image` has no tag |
| `configuration.history_shards` | `4` | Temporal's shard count. Can't change after the first deploy |
| `configuration.database.instance` | – | The database to use, when several match |
| `configuration.dynamic_config` | – | Temporal's [dynamic config](https://docs.temporal.io/references/dynamic-configuration), e.g. `{limit.maxIDLength: [{value: 255}]}`. A change is applied without a restart |
| `configuration.ui.url` | – | Public URL of the web UI, `https://<domain>` or `http://<domain>[:port]`, no path. Without `ui`, there is no UI |
| `configuration.ui.oidc` | – | Log in to the UI through an OIDC provider: `type` (default `keycloak`), `instance` (when several match), `realm` (default `compost-workflow-<name>`) |
| `configuration.ui.anonymous` | `false` | `true` serves the UI without a login, instead of `oidc`. Anyone who reaches it can start, signal and terminate every workflow |
| `configuration.ui.image` | `temporalio/ui:X.Y.Z` | |

The project runs four containers:
- **schema** creates or updates Temporal's tables, then exits. It runs before the server on every start, and does nothing when the tables are current.
- **server**, reached by apps on network `compost-wf-<name>` at `<name>:7233`. Its Prometheus metrics are on port 9090, not published.
- **admin**, Temporal's admin tools, where compost runs the `temporal` CLI to create and delete namespaces. `compost server exec <name> --service admin -- temporal …` gives you the same CLI.
- **ui**, with `ui`.

With `oidc`, the link creates a client in the realm; the realm's users can log in to the UI and see every namespace. compost creates no users: add them in Keycloak's admin console, or name the realm of your staff with `realm`. Temporal's own API has no login: an app on `compost-wf-<name>` can reach every namespace, not only its own.

Upgrade Temporal one minor version at a time (e.g. `1.31` to `1.32`), as Temporal requires: set `image` (and `admin_tools_image`, if `image` has no tag) to each version in turn and deploy. The schema container brings the tables up to date before the new server starts.

Workflows and namespaces live in the two databases and are saved with them; a server backup adds the shard count. `server restore` starts Temporal after its databases are loaded.

## Monitoring shared apps

`monitoring` on a shared app sends its output, the metrics it serves and, for the ingress, traces of the requests it serves to a monitoring app:

```yaml
shared:
  ingress:
    - name: caddy-main
      type: caddy
      configuration:
        access_log: true
      monitoring: {traces: true}
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
| `monitoring.metrics` | `true` | Scrape its metrics, if it serves any: Caddy and Temporal do |
| `monitoring.traces` | `false` | Ingress only: trace every request it serves, on every site, see [Request tracing](#request-tracing) |

Caddy, Postgres, Keycloak and Temporal can be monitored. External shared apps and monitoring apps can't. It works in both directions: OpenObserve's domain is served by the Caddy that sends its logs to it. On the first deploy, Caddy starts before OpenObserve, and Docker keeps Caddy's output in a small buffer and sends it once the collector runs.

### Request tracing

With `monitoring.traces: true` on the ingress, Caddy starts an OpenTelemetry trace for every request to every site, apps' and `sites` alike:

- It sends a span per request to the collector, over OTLP/HTTP, named after the request's host, with its method, path, status and duration. In OpenObserve they show under Traces as service `compost-shared-<name>` (e.g. `compost-shared-caddy-main`).
- It passes the request on to the app with a W3C `traceparent` header. An app with an OpenTelemetry SDK, given the collector's address by its [monitoring link](Applications.md#monitoring-links), continues that trace, so its spans show below Caddy's. A request that comes with a `traceparent` header continues that trace instead.
- With `access_log`, every log line gets the request's `traceID` and `spanID`, to go from a log line to its trace.

Caddy gets the collector's address in `OTEL_EXPORTER_OTLP_ENDPOINT` and the other `OTEL_*` variables, set in its `compose.override.yaml`, so turning tracing on or off recreates Caddy. Spans of requests served while the collector is down are lost. Other shared apps send no traces: `server deploy` refuses `traces` on them.

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

Links of shared apps (Keycloak's database and domain, Temporal's databases, domain and login) show as `shared <kind>/<name>/<entry>`. A link whose instance no longer exists shows as a leftover record.

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
