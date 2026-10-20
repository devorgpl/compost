# compost

> compost is a CLI that deploys docker-compose applications to your own Linux servers over SSH (or to this machine). A server gets shared apps that many apps use: Caddy (routing, automatic HTTPS), Postgres databases, Keycloak (OIDC logins) and OpenObserve (logs, metrics, traces). Each app is linked to them by a `compost.yaml` next to its compose file, plus per-instance overlays with domains and passwords.

The pages below are the user documentation (the source is `docs/wiki` in the repository). `compost agent llms --full` prints all of them for the installed version, and `compost agent skill` installs them as an Agent Skill. Prefer `compost <command> --help` over memory: options change between versions.

## Start

- [Home](Home.md): what compost does, concepts (server, context, shared app, app, instance, overlay, link) and requirements
- [Installation](Installation.md): install script, release archives, macOS, first setup, updates
- [Quick start](Quick-Start.md): run compost against this machine with the sample app
- [Production setup](Production-Setup.md): from an empty Linux server to an app on HTTPS, step by step, with a checklist
- [Examples](Examples.md): static site, API with a database, WordPress on MariaDB, uploads in S3 storage, staging and production, apps you don't own, AWS RDS, Keycloak logins, Temporal workflows, OpenObserve, review copies, nightly backups
- [Migrating to compost](Migration.md): move a server with hand-made containers to compost in reversible phases
- [Facts and limits](Facts.md): what works, what doesn't yet (no failover, no zero-downtime deploys, no isolation between apps on a shared app), requirements, limits, security, license and status

## Use

- [Applications](Applications.md): `compost.yaml`, services, ingress, database, key-value, storage, OIDC, workflow and monitoring links, deploying, inspecting, removing
- [Servers and shared apps](Servers-and-Shared-Apps.md): defining servers, `compost-server.yaml`, the caddy, postgres, mariadb, mysql, valkey, redis, garage, s3, keycloak, openobserve and temporal types, external databases, locks
- [Local development](Local-Development.md): `compost dev up` on this machine (adds missing shared apps, reuses existing ones), several projects, `dev up --host` and `dev env` for apps run outside Docker, `dev reset`, `dev stop`, `dev down`
- [Secrets](Secrets.md): overlays with passwords, local or stored on the server, generated passwords
- [Backup and restore](Backup-and-Restore.md): app and server backups, targets, formats, restoring, copies, moving to a new server
- [Admin commands](Admin-Commands.md): `server exec`, named commands in compost-server.yaml and compost.yaml (`server cmd`, `app cmd`): typed arguments, secrets through `env`, confirmation, dry run; built-in commands per shared-app type (`caddy-main.unban`, `postgres18.psql`); history
- [Notifications](Notifications.md): failed scheduled backups, health and disk alerts; communication apps (webhook, Slack, Mattermost, Discord, Matrix, email), test messages, the contract for your own image

## Reference

- [Configuration](Configuration.md): every key of `compost.yaml`, overlays, `compost-server.yaml`, `servers.yaml`, merge rules, preferences, environment variables
- [Files and folders](Files-and-Folders.md): files on your machine, in app and infrastructure folders, on the server, and Docker names
- [CLI reference](CLI-Reference.md): the `--help` of every command, and exit codes
- [AI agents](AI-Agents.md): `llms.txt`, `llms-full.txt` and the compost Agent Skill
