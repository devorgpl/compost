# compost

compost deploys docker-compose applications to your own servers. A server gets shared services that many apps use: a **Caddy** reverse proxy with automatic HTTPS, **Postgres** databases, a **Keycloak** login provider and **OpenObserve** for logs, metrics and traces. Each app is linked to them by a few lines of YAML next to its compose file.

compost is a single CLI on your machine. It reaches servers over SSH, or manages this machine directly. It installs no agent: a server only holds plain files in one folder and Docker containers.

```sh
compost server deploy --apply                              # Caddy + Postgres on the server
compost app deploy --apply --secret .compost-staging.yaml   # your app, with a database and a domain
```

## What it does for you

- **Routing and HTTPS:** domains and paths go to your services through Caddy, with certificates from Let's Encrypt.
- **Databases:** each app gets its own Postgres database and user, and its services get the connection variables.
- **Logins:** an app can get a Keycloak realm, client and test users, with the OIDC settings passed to its services.
- **Monitoring:** the output and metrics of apps and shared services go to OpenObserve, and services get the variables to send OpenTelemetry traces.
- **Instances:** the same app runs as `staging`, `prod` or a review copy, each with its own domains and data.
- **Local development:** `compost dev up` runs an app with the shared apps it needs on your machine, next to your other projects, or only its databases and storage for running it from your IDE. See [Local development](Local-Development.md).
- **Secrets:** passwords live in files you don't commit, or are stored on the server; generated ones are kept between deploys.
- **Backup and restore:** apps and shared services, to this machine or to a folder on the server, also to make copies or move to a new server.
- **Gradual migration:** Caddy serves existing containers, apps link to existing or hosted databases (e.g. AWS RDS), and `compost.yaml` can live apart from apps you don't own. See [Migrating to compost](Migration.md).

Your compose file is never changed. compost adds networks and variables in an override file on the server.

## Concepts

| Term | Meaning |
|---|---|
| **Server** | A host compost manages, reached over `ssh` or `local` (this machine). It has a name in your local config. |
| **Context** | The default server for commands, set with `compost connect <server>`. |
| **Shared app** | A server-wide service built from a built-in type: `caddy` (kind `ingress`), `postgres`, `mariadb` and `mysql` (kind `database`), `valkey` and `redis` (kind `keyvalue`), `garage` and external `s3` (kind `storage`), `keycloak` (kind `oidc`), `openobserve` (kind `monitoring`), `temporal` (kind `workflow`); channels for notifications (kind `communication`). Defined in `compost-server.yaml`. |
| **App** | Your application: a compose file plus `compost.yaml`. |
| **Instance** | One deployment of an app on a server, `<app>-<prefix>`, e.g. `myapp-staging`. |
| **Overlay** | A YAML file with the values of one instance (prefix, domains, passwords), merged over `compost.yaml`. |
| **Link** | A connection between an instance and a shared app: a domain on the ingress, a database, a login realm, a workflow namespace, log collection. |
| **Manager key** | A UUID that marks which compost installation manages a server. Copy it to manage the same servers from another machine. |

## Pages

| Start here | Use it | Reference |
|---|---|---|
| [Installation](Installation.md) | [Applications](Applications.md) | [Configuration](Configuration.md) |
| [Quick start](Quick-Start.md) | [Servers and shared apps](Servers-and-Shared-Apps.md) | [Files and folders](Files-and-Folders.md) |
| [Production setup](Production-Setup.md) | [Secrets](Secrets.md) | [CLI reference](CLI-Reference.md) |
| [Examples](Examples.md) | [Local development](Local-Development.md) | |
| | [Backup and restore](Backup-and-Restore.md) | |
| [Migrating to compost](Migration.md) | [Notifications](Notifications.md) | [AI agents](AI-Agents.md) |
| [Facts and limits](Facts.md) | | |

## Requirements

| Side | Needs |
|---|---|
| Your machine | Linux or macOS, the `compost` binary, an OpenSSH client; for apps with [`build: local`](Applications.md#building-images) (experimental) also Docker with compose v2 |
| Server | Linux, POSIX `sh` and `tar` (BusyBox is enough), Docker Engine 24 or newer with the compose v2 plugin; for [scheduled backups](Backup-and-Restore.md#scheduled-backups) and [notifications](Notifications.md) also `cron` |
| Server user | Logs in with an SSH key, member of the `docker` group (no sudo) |
