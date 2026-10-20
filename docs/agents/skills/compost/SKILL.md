---
name: compost
description: Deploy docker-compose apps to Linux servers with the compost CLI. Use when working with compost commands or files (compost.yaml, compost-server.yaml, .compost-*.yaml overlays), when deploying, inspecting, backing up or restoring apps and their shared Caddy, Postgres, MariaDB, MySQL, Valkey, Redis, Garage, Keycloak, OpenObserve or Temporal, when managing compost secrets, or when moving existing Docker servers to compost.
---

# compost

compost is a CLI on the user's machine that deploys docker-compose apps to servers over SSH (or to this machine). Servers run **shared apps** (Caddy for domains and HTTPS, Postgres, MariaDB or MySQL, Valkey or Redis, Garage S3 storage, Keycloak, OpenObserve, Temporal); each **app** is a compose file plus `compost.yaml`, which links its services to them. One deployment of an app is an **instance**, `<app>-<prefix>` (e.g. `myapp-staging`), whose domains and passwords come from an **overlay** merged over `compost.yaml`. Concepts: [Home](references/Home.md#concepts).

The `references/` folder holds the full user documentation of the compost version this skill came with. Read the page for the task before writing config: see [Where to look](#where-to-look).

## Before you act

- Run `compost --version`. If it differs from the version in the user's docs or this skill's source, trust `compost <command> --help` over the references.
- `compost server list` and `compost server state` show which servers exist and which one commands go to: `-s <name>`, else `COMPOST_SERVER`, else the context set by `compost connect`. Name the server in every command you suggest or run, so nothing lands on the wrong one.
- Don't make up config keys: every key is in [Configuration](references/Configuration.md).

## Safety rules

- **Dry-run first.** Before any `server deploy` or `app deploy`, run it with `--dry-run` (add `--diff` for the changed lines) and show the user what would change: files, and which services are recreated, built, pulled or reloaded. `--dry-run` never changes the server.
- **Ask before changing a server.** Get the user's OK before `--apply`, `app remove`, `app restore`, `app rollback`, `server restore`, `secret deploy`, `app deploy --secret` and `server deploy --secret` without `--dry-run` and `secret remove`, and before anything with `--purge`, `--volumes`, `--replace` or `--force-unlock`. Never add `--yes` on your own: those commands ask because they delete or overwrite data. `app exec`, `app run` and `server exec` run any command in the user's containers: ask before anything that is not read-only (migrations, shells, writes). `app cmd` and `server cmd` run the named commands of the deployed config and compost's built-in ones: read one with `--help` or `--dry-run` first, and ask before running one that isn't read-only; those with `confirm: true` ask, so don't add `--yes` on your own.
- `compost config set deploy.apply true` makes every deploy apply. Check it with `compost config show`, and pass `--no-apply` when you only want to upload.
- **Secrets.** Overlays (`.compost-*.yaml`, `.compost.*.yaml`, `.compost-server.secrets.yaml`) hold passwords: never commit them, print them or paste them into chat. Commit only `*.example.yaml`. Leave passwords out to have compost generate and keep them. Postgres, MariaDB and MySQL `root_password`, Keycloak `admin_password` and OpenObserve's `root_password` can't change after the first deploy. See [Secrets](references/Secrets.md).
- Your compose file is never changed: compost adds networks and variables in an override file on the server. Don't edit files in `~/.compost-server` on a server by hand.

## Workflows

**First setup** (once per machine, then per server; see [Production setup](references/Production-Setup.md)):

```sh
compost init
compost server define prod --address 203.0.113.10     # --user, --port, --identity-file; --local for this machine
compost connect prod
compost server state                                  # UNINITIALIZED on a new server
```

For one command with another key, add `--ssh-key-file <path>` (it wins over `--identity-file`).

The user must have connected with `ssh` once to accept the host key; compost never skips host key checks.

**Shared apps**, in an infrastructure folder ([Servers and shared apps](references/Servers-and-Shared-Apps.md)):

```sh
compost server generate --with caddy,postgres         # --list shows the types; caddysecurebuild instead of caddy adds CrowdSec bans, AppSec and rate limits
# edit compost-server.prod.yaml (acme_email), copy .compost-server.secrets.example.yaml to .compost-server.secrets.yaml
compost server deploy --dry-run -f compost-server.yaml,compost-server.prod.yaml --secret .compost-server.secrets.yaml
compost server deploy --apply   -f compost-server.yaml,compost-server.prod.yaml --secret .compost-server.secrets.yaml
```

`--secret` stores the file on the server: later `server deploy` runs need only the two committed files, and a new `--secret` replaces the stored copy.

**An app**, next to its compose file ([Applications](references/Applications.md)):

```sh
compost app generate                                  # compost.yaml + .compost.secrets.example.yaml
# uncomment the links you need in compost.yaml; per instance: cp .compost.secrets.example.yaml .compost-staging.yaml
compost app deploy --dry-run --secret .compost-staging.yaml
compost app deploy --apply   --secret .compost-staging.yaml
compost app state myapp-staging
```

compost starts every service of the compose files; `app_services` only names link targets and doesn't select services. When the project should also run with plain `docker compose up`, put the services compost replaces with shared apps (database, cache) in `docker-compose.override.yaml`, which compost never reads, or in a Compose profile, with `required: false` on `depends_on` ([With and without compost](references/Applications.md#with-and-without-compost)).

Deploy an instance's overlay with `--secret`, not as a second `-f` file: it stores the overlay on the server, so later deploys from any machine or CI need only `compost.yaml` and the prefix (`compost app deploy --apply --prefix staging`), and a new `--secret` replaces the stored copy as a whole. `-f compost.yaml,.compost-local.yaml` (nothing stored, every deploy needs the file) is for instances on this machine and throwaway copies.

**Local development** ([Local development](references/Local-Development.md)), in the app's folder:

```sh
compost dev up --dry-run      # shared apps it would add to this machine
compost dev up                # adds missing shared apps, reuses existing ones, deploys the app with --apply
compost dev up --host         # or: links only, shared apps published on 127.0.0.1, for running the app outside Docker
compost dev env -o .env       # its variables with 127.0.0.1 addresses (passwords: keep .env out of git and add it to .compostignore)
compost dev stop              # pause; `dev up` resumes
```

`dev` commands only use a local server (`-s`/`COMPOST_SERVER` must name one, else the `dev.server` preference, else the only local server, else they define `local`), never the SSH context, and load `compost.yaml` plus `.compost-local.yaml` with prefix `local` by default. A link pinning a shared app this machine lacks (`instance: temporal-main`) uses the one that matches it otherwise. `dev up` applies without asking, but only to this machine. `dev reset` (drops the app's databases, buckets, realms and namespaces, then recreates them empty), `dev down --purge` and `dev reset-all` (removes every app and shared app on this machine and `~/.compost-server`, with all data and backup archives in it) delete data: ask first and never add `--yes` on your own.

**Roll back a bad deploy:** `compost app releases <instance>` lists the releases; `compost app rollback <instance> --dry-run` shows what the release before the current one changes (images, links), then `compost app rollback <instance>` (or `--to <n>`) deploys it again with the images it ran. It restarts the instance's services and doesn't roll back databases or volumes, so ask first.

**Update images or recreate containers:** add `--pull` (pull images again, even with unchanged tags; rebuild the images compost builds: on the server with `build: true`, or on this machine with `build: local`) or `--recreate` (recreate every container; volumes are kept) to `server deploy --apply` or `app deploy --apply`. Shared apps are never pulled without `--pull` unless `version` or `image` changes; recreating them restarts what every app uses.

**`build: local` is experimental.** It builds images on this machine and sends them to the server ([Building images](references/Applications.md#building-images)). Never suggest it or switch an app to it; work with it only when the app's config already uses it or the user asks for it, and then point out its [known limitations](references/Applications.md#known-limitations-of-build-local).

**Inspect:** `compost app list`, `compost app state <instance>`, `compost app logs <instance> --service <service> --tail 200`, `compost server logs <shared app>`, `compost connector list`, `compost server state`, `compost backup list`. Never use `--follow` with logs: it doesn't end. Notification channels are `communication` shared apps ([Notifications](references/Notifications.md)); `notifications.send_to` in compost-server.yaml makes the server report failed scheduled backups, unhealthy shared apps or instances, and low disk space (once per change); `compost notify test` sends a test message. `compost server prune --dry-run` shows disk usage; without `--dry-run` it removes every unused image (also of other software on the server), dangling images and the build cache, keeping release images, and asks unless `--yes`.

**Back up and restore** ([Backup and restore](references/Backup-and-Restore.md)): `compost app backup <instance>` or `--all`, `compost server backup`; restores with `app restore` / `server restore`. Profiles under `backup.profiles` (compost-server.yaml, overridden or added per app in compost.yaml) set what is saved, the target (`local`, `server` or one of `backup.targets`: another server folder, or S3, whose keys must be stored with `server deploy --secret`) and `keep`; a profile's `schedule` (cron expression, server's local time) makes the server run it by itself (deploys install compost and a crontab line there; `server state` shows the last runs), `compost backup run <profile>` backs up the shared apps and every instance with one now, `--profile` uses one for a single backup, and `compost backup prune` deletes archives beyond `keep` (asks unless `--yes`). With `deployment.backup`, `app deploy --apply` backs up the deployed instance first and stops if that fails; `--no-backup` skips it. `app restore <archive> --prefix review -f other-domains.yaml` makes a copy of an instance. Steps under `backup` in `compost.yaml` (`before_backup`, `after_backup`, `before_restore`, `after_restore`) run commands in the app's containers during these; `backup.stop` (or `--stop`, all services) stops services while volumes are copied.

**Admin commands** ([Admin commands](references/Admin-Commands.md)): `compost server exec <shared app> [--service crowdsec|admin|…] -- <command>` runs a command in a shared app's container; don't use `ssh` plus `docker exec`. compost has built-in commands per shared-app type, named `<shared app>.<command>`: check `compost server cmd` (the list) before writing your own, e.g. `server cmd caddy-main.bans`, `caddy-main.unban <ip>`, `postgres18.psql [database]` (input can be piped in), `postgres18.activity`, `mariadb11.sql`. Tasks done repeatedly go under `commands` in compost-server.yaml (`shared_app`, `command` as a list, typed `args`, secrets only via `env: {X: "{secret:root_password}"}`) or compost.yaml (`exec:`/`run:` a service), and run with `compost server cmd <name> [args]` or `compost app cmd <instance> <name> [args]`; without a name they list the commands, and `--history` shows the ones run (`logs/commands.log` on the server).

**Existing servers:** follow [Migrating to compost](references/Migration.md) phase by phase; every phase ends with everything working.

## Exit codes

| Code | Meaning | What to do |
|---|---|---|
| 0 | OK | |
| 1 | Unexpected error | Read the message; rerun with `-v` to see the commands run on the server |
| 2 | Invalid local config or usage; nothing was changed | Fix the file at the path the error names (e.g. `services.ingress[site].routes[api]`) |
| 3 | Server not ready, or a server-side check failed | `compost server state`; a missing shared app, a domain used by another instance, SSH or Docker problems |
| 4 | Deployed, but not healthy | `compost app state <instance>`, then `compost app logs <instance> --service <service> --tail 200` |
| 130 | Interrupted | If the next command reports a lock, ask before using `--force-unlock` |

## Where to look

| Task | Reference |
|---|---|
| Whether compost can do something; what it doesn't do yet, limits | [Facts and limits](references/Facts.md) |
| Install or update compost | [Installation](references/Installation.md) |
| Try it locally with the samples | [Quick start](references/Quick-Start.md) |
| A new server, end to end | [Production setup](references/Production-Setup.md) |
| A ready-made setup (static site, API with DB, Keycloak login, Temporal, OpenObserve, RDS, apps you don't own) | [Examples](references/Examples.md) |
| `compost.yaml`, links (ingress, database, oidc, workflow, monitoring), deploy, remove | [Applications](references/Applications.md) |
| Run apps on this machine for development, `dev` commands | [Local development](references/Local-Development.md) |
| `compost-server.yaml`, shared app types, TLS, external databases, locks | [Servers and shared apps](references/Servers-and-Shared-Apps.md) |
| Every config key, merge rules, preferences, environment variables | [Configuration](references/Configuration.md) |
| Passwords and overlays | [Secrets](references/Secrets.md) |
| Backups, restores, moving servers | [Backup and restore](references/Backup-and-Restore.md) |
| Commands in shared apps' containers, named commands (`server cmd`, `app cmd`) | [Admin commands](references/Admin-Commands.md) |
| Files compost writes and Docker names | [Files and folders](references/Files-and-Folders.md) |
| Every command and option | [CLI reference](references/CLI-Reference.md) |
| Moving hand-made Docker servers to compost | [Migrating to compost](references/Migration.md) |
