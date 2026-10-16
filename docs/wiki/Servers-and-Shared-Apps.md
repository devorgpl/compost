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
compost server generate [--with caddy,postgres,keycloak] [--force]
compost server deploy [--apply | --no-apply] [-f files] [--dry-run]
```

`server generate` writes starter files to the current folder (`compost-server.yaml`, `compost-server.prod.yaml`, `.compost-server.secrets.example.yaml`; see [Production setup](Production-Setup.md#3-shared-apps)). It adds types that a chosen type needs: `keycloak` brings `postgres` and `caddy`. `--list` shows the available types.

`server deploy` reads `compost-server.yaml` and `.compost-server.secrets.yaml` from the current folder, or the files given with `-f`. It:

1. validates the config (exit 2 on errors, before anything touches the server);
2. creates compost's folder and the Docker networks;
3. renders each shared app's files and uploads them, generating missing passwords;
4. with `--apply`: starts every shared app with `docker compose up -d` and waits until it runs, then prints `server state`.

Without `--apply`, only steps 1–3 run. `--dry-run` shows what would happen and changes nothing. Running it again changes nothing that didn't change: unchanged containers are not recreated.

Shared apps removed from the config are reported as `orphaned` and keep running; stop them with `docker compose -p compost-<kind>-<name> down`.

## Shared app types

A server has at most one ingress, and any number of databases and OIDC providers. Every shared app has a `name` (lowercase letters, digits and `-`, up to 31 characters, starting with a letter), a `type`, a `version` and optionally an `image`. Type-specific settings go in `configuration`.

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
| `configuration.tls` | `auto` | `auto`: certificates from Let's Encrypt · `internal`: Caddy's own CA, for testing · `off`: plain HTTP |
| `configuration.acme_email` | – | Contact for the ACME account |

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
