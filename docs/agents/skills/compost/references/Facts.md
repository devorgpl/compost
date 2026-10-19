# Facts and limits

What compost does, what it doesn't do yet, and its limits, in one place. The other pages have the details; this one is for deciding whether compost fits, and for AI agents that should not assume features it lacks.

## What compost is

A single CLI on your machine that deploys docker-compose apps to Linux servers you run. It reaches them over SSH, or manages this machine directly. A server gets shared apps (Caddy, databases, Keycloak, OpenObserve and others) that many apps link to with a few lines of YAML next to their compose file.

compost is not a hosting service: it rents no servers and runs nothing of its own outside your machine and your servers. It installs no agent: a server holds plain files in one folder (`~/.compost-server`), Docker containers and, for scheduled backups and notifications, a copy of compost and one crontab line ([Files and folders](Files-and-Folders.md#on-the-server)).

## Who it's for

People who have, or will rent, Linux servers with Docker, have compose files (or can write them), and want routing, HTTPS, databases, logins, monitoring and backups set up the same way for every app, without Kubernetes.

It isn't for you if you want someone else to run the server, need an app to run on several servers at once with failover, or can't give compost's SSH user access to Docker.

## What works today

- **Shared apps on a server:** Caddy (`caddy`, or `caddysecurebuild` with CrowdSec bans, AppSec and rate limits), Postgres, MariaDB, MySQL, Valkey, Redis, Garage, Keycloak, OpenObserve, Temporal, and channels for notifications. Existing databases (a container, AWS RDS) and S3 services can be registered as external ([Servers and shared apps](Servers-and-Shared-Apps.md#shared-app-types)).
- **Links:** an app gets a domain and routes on the ingress with automatic HTTPS, its own database and user, a key-value database, an S3 bucket and key, a Keycloak realm with a client, roles and users, a Temporal namespace, and log, metric and trace collection. Its services get the connection values as variables ([Applications](Applications.md#ingress-links)).
- **Your compose file stays as it is.** compost adds networks and variables in an override file on the server. `compose_overrides` change apps you don't own without editing them.
- **Instances:** the same app as `staging`, `prod` or a review copy on one server, each with its own domains, data and passwords ([Configuration](Configuration.md#overlays)).
- **Dry runs:** `--dry-run` shows what a deploy would change (files, services to recreate, links) without touching the server; `--diff` adds the changed lines. Without `--apply` (or the `deploy.apply` preference), a deploy only uploads files and starts nothing.
- **Repeatable deploys:** deploying again keeps data and passwords and doesn't recreate unchanged containers.
- **Releases and rollback:** each deploy is a numbered release; `app rollback` starts the images of an earlier one ([Releases and rollback](Applications.md#releases-and-rollback)).
- **Images:** pulled from a registry, built on the server, or built on your machine and sent over SSH without a registry (`build: local`, experimental).
- **Secrets:** in overlays you don't commit, or stored on the server so any machine or CI deploys with `compost.yaml` alone. Passwords you leave out are generated and kept ([Secrets](Secrets.md)).
- **Backup and restore:** apps (database dumps, buckets, volumes, sources, images) and shared apps, to your machine, a folder on the server or S3; profiles with `keep`, scheduled by cron on the server, or run before each deploy. Restores also make copies of an app and move everything to a new server ([Backup and restore](Backup-and-Restore.md)).
- **Notifications** of failed scheduled backups, apps or shared apps going down, and a full disk, to webhook, Slack, Mattermost, Discord, Matrix or email ([Notifications](Notifications.md)).
- **Day to day:** `app state`, `app logs`, `app exec`, `app run`, `server state`, `server logs`, `connector list`, `server prune`.
- **Gradual migration** of servers set up by hand: Caddy serves existing containers, apps keep their databases and volumes, and every phase can be undone. The procedure in [Migrating to compost](Migration.md) was run on a real server.
- **AI agents:** `llms.txt`, `llms-full.txt` and an Agent Skill, built into the binary for its own version ([AI agents](AI-Agents.md)).
- **Several machines and people** can manage the same servers by sharing the manager key; a lock keeps two commands from changing a server at once.

## What doesn't work yet

- **macOS and Windows binaries.** Release binaries are for Linux (x86_64, arm64). On macOS, build from source with Cargo; Windows is not supported.
- **Several servers for one instance.** An instance runs on one server: there is no clustering, load balancing across servers or failover. If the server stops, its apps are offline until it is back.
- **Deploys without downtime.** A deploy recreates changed containers, so a changed service is unavailable while its new container starts. There are no blue-green or rolling deploys.
- **Isolation between apps on one shared app.** Instances linked to the same database, key-value store or monitoring app are on its Docker network and can reach each other's containers. Temporal has no login, so every app on its network reaches every namespace.
- **Rolling back data.** `app rollback` doesn't undo database migrations or volume changes; take an `app backup` first.
- **Deploying on a Git push** or from a web dashboard. compost runs from a terminal, on your machine or in your CI.
- **A web UI.** compost has none; OpenObserve, Keycloak and Temporal have their own.
- **Removing shared apps.** A shared app taken out of `compost-server.yaml` is reported as `orphaned` and keeps running until you stop it.
- **Changing some passwords.** Root passwords of compost's databases, Keycloak's admin password and OpenObserve's root login can't change after the first deploy.
- **More than one ingress** per server, and `caddysecurebuild` behind another proxy or a CDN.
- **MinIO** as a built-in storage type (an existing MinIO works as external `s3`), and Garage features such as versioning, object lock or bucket policies.
- **Podman** and other compose implementations on your machine for `build: local`: only Docker has been tried.

## Limits

| What | Limit |
|---|---|
| Your machine | Linux or macOS, OpenSSH client; Docker with compose v2 only for `build: local` |
| Server | Linux, POSIX `sh` and `tar`, Docker Engine 24 or newer with compose v2; `cron` for scheduled backups and notifications |
| Server user | SSH key login, member of the `docker` group; no root or sudo |
| Server size | Whatever your server has: compost sets no CPU, memory or disk quotas |
| Deploy | Waits up to 60 s for services to run or become healthy; Keycloak gets 180 s |
| Releases | 5 kept per instance by default (`deployment.releases`) |
| Shared app names | Lowercase letters, digits and `-`, up to 31 characters, starting with a letter |
| MariaDB and MySQL users | Up to 32 characters; longer default names are cut and end in a hash |
| Garage buckets | `<app>-<prefix>-<entry>`, 3 to 63 characters |
| Domains | Each served by one instance or one hand-written site per server |
| Scheduled checks | Backups, health and disk are checked every five minutes |

## Security and privacy

- compost talks only to your servers, over SSH, with your SSH config, keys and host key checking. It sends no telemetry.
- Servers reach the internet for what you ask of them: image registries, Let's Encrypt, S3 targets, notification channels, CrowdSec's blocklist with `caddysecurebuild`, and GitHub when scheduled backups download compost there.
- Secrets never appear in output, stored configs or remote command lines. Secret files on the server have mode `0600`, and dry runs only name them.
- Membership in the `docker` group is equivalent to root on the server. So is OpenObserve's collector with `container_metrics` (the default), which reads the Docker socket.
- Backup archives of servers, and of apps with a stored overlay, hold passwords. Keep them as safe as the server.
- Without the manager key you can't manage your servers with compost. Back up `~/.compost/compost-manager.yaml`; a copy is on each server in `~/.compost-server/manager_key`.

## Cost and license

compost is free and open source, under MIT or Apache-2.0, at your choice. It comes without warranty or support commitment. Your servers, registries, S3 storage and domains cost what their providers charge.

## Status

- Developed in the open at [github.com/devorgpl/compost](https://github.com/devorgpl/compost). Releases are tagged `vX.Y.Z`; the [wiki](https://github.com/devorgpl/compost/wiki) describes the latest release and `docs/wiki` in the repository describes master.
- Every push runs formatting, lints and the test suite. Features marked experimental (`build: local`) work but may change without notice.
