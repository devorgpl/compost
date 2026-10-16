---
name: compost
description: Deploy docker-compose apps to Linux servers with the compost CLI. Use when working with compost commands or files (compost.yaml, compost-server.yaml, .compost-*.yaml overlays), when deploying, inspecting, backing up or restoring apps and their shared Caddy, Postgres, Keycloak or OpenObserve, when managing compost secrets, or when moving existing Docker servers to compost.
---

# compost

compost is a CLI on the user's machine that deploys docker-compose apps to servers over SSH (or to this machine). Servers run **shared apps** (Caddy for domains and HTTPS, Postgres, Keycloak, OpenObserve); each **app** is a compose file plus `compost.yaml`, which links its services to them. One deployment of an app is an **instance**, `<app>-<prefix>` (e.g. `myapp-staging`), whose domains and passwords come from an **overlay** merged over `compost.yaml`. Concepts: [Home](references/Home.md#concepts).

The `references/` folder holds the full user documentation of the compost version this skill came with. Read the page for the task before writing config: see [Where to look](#where-to-look).

## Before you act

- Run `compost --version`. If it differs from the version in the user's docs or this skill's source, trust `compost <command> --help` over the references.
- `compost server list` and `compost server state` show which servers exist and which one commands go to: `-s <name>`, else `COMPOST_SERVER`, else the context set by `compost connect`. Name the server in every command you suggest or run, so nothing lands on the wrong one.
- Don't make up config keys: every key is in [Configuration](references/Configuration.md).

## Safety rules

- **Dry-run first.** Before any `server deploy` or `app deploy`, run it with `--dry-run` and show the user what would change. `--dry-run` never changes the server.
- **Ask before changing a server.** Get the user's OK before `--apply`, `app remove`, `app restore`, `server restore`, `secret deploy` and `secret remove`, and before anything with `--purge`, `--volumes`, `--replace` or `--force-unlock`. Never add `--yes` on your own: those commands ask because they delete or overwrite data.
- `compost config set deploy.apply true` makes every deploy apply. Check it with `compost config show`, and pass `--no-apply` when you only want to upload.
- **Secrets.** Overlays (`.compost-*.yaml`, `.compost.*.yaml`, `.compost-server.secrets.yaml`) hold passwords: never commit them, print them or paste them into chat. Commit only `*.example.yaml`. Leave passwords out to have compost generate and keep them. Postgres `root_password`, Keycloak `admin_password` and OpenObserve's `root_password` can't change after the first deploy. See [Secrets](references/Secrets.md).
- Your compose file is never changed: compost adds networks and variables in an override file on the server. Don't edit files in `~/.compost-server` on a server by hand.

## Workflows

**First setup** (once per machine, then per server; see [Production setup](references/Production-Setup.md)):

```sh
compost init
compost server define prod --address 203.0.113.10     # --user, --port, --identity-file; --local for this machine
compost connect prod
compost server state                                  # UNINITIALIZED on a new server
```

The user must have connected with `ssh` once to accept the host key; compost never skips host key checks.

**Shared apps**, in an infrastructure folder ([Servers and shared apps](references/Servers-and-Shared-Apps.md)):

```sh
compost server generate --with caddy,postgres         # --list shows the types
# edit compost-server.prod.yaml (acme_email), copy .compost-server.secrets.example.yaml to .compost-server.secrets.yaml
compost server deploy --dry-run -f compost-server.yaml,compost-server.prod.yaml,.compost-server.secrets.yaml
compost server deploy --apply   -f compost-server.yaml,compost-server.prod.yaml,.compost-server.secrets.yaml
```

**An app**, next to its compose file ([Applications](references/Applications.md)):

```sh
compost app generate                                  # compost.yaml + .compost.secrets.example.yaml
# uncomment the links you need in compost.yaml; per instance: cp .compost.secrets.example.yaml .compost-staging.yaml
compost app deploy --dry-run -f compost.yaml,.compost-staging.yaml
compost app deploy --apply   -f compost.yaml,.compost-staging.yaml
compost app state myapp-staging
```

Or store the overlay on the server, so any machine or CI deploys with `compost.yaml` alone: `compost secret deploy .compost-staging.yaml`, then `compost app deploy --apply --prefix staging`.

**Inspect:** `compost app list`, `compost app state <instance>`, `compost connector list`, `compost server state`, `compost backup list`.

**Back up and restore** ([Backup and restore](references/Backup-and-Restore.md)): `compost app backup <instance>` or `--all`, `compost server backup`; restores with `app restore` / `server restore`. `app restore <archive> --prefix review -f other-domains.yaml` makes a copy of an instance. Steps under `backup` in `compost.yaml` (`before_backup`, `after_backup`, `before_restore`, `after_restore`) run commands in the app's containers during these; `backup.stop` (or `--stop`, all services) stops services while volumes are copied.

**Existing servers:** follow [Migrating to compost](references/Migration.md) phase by phase; every phase ends with everything working.

## Exit codes

| Code | Meaning | What to do |
|---|---|---|
| 0 | OK | |
| 1 | Unexpected error | Read the message; rerun with `-v` to see the commands run on the server |
| 2 | Invalid local config or usage; nothing was changed | Fix the file at the path the error names (e.g. `services.ingress[site].routes[api]`) |
| 3 | Server not ready, or a server-side check failed | `compost server state`; a missing shared app, a domain used by another instance, SSH or Docker problems |
| 4 | Deployed, but not healthy | `compost app state <instance>`, then the service's logs on the server |
| 130 | Interrupted | If the next command reports a lock, ask before using `--force-unlock` |

## Where to look

| Task | Reference |
|---|---|
| Install or update compost | [Installation](references/Installation.md) |
| Try it locally with the samples | [Quick start](references/Quick-Start.md) |
| A new server, end to end | [Production setup](references/Production-Setup.md) |
| A ready-made setup (static site, API with DB, Keycloak login, OpenObserve, RDS, apps you don't own) | [Examples](references/Examples.md) |
| `compost.yaml`, links (ingress, database, oidc, monitoring), deploy, remove | [Applications](references/Applications.md) |
| `compost-server.yaml`, shared app types, TLS, external databases, locks | [Servers and shared apps](references/Servers-and-Shared-Apps.md) |
| Every config key, merge rules, preferences, environment variables | [Configuration](references/Configuration.md) |
| Passwords and overlays | [Secrets](references/Secrets.md) |
| Backups, restores, moving servers | [Backup and restore](references/Backup-and-Restore.md) |
| Files compost writes and Docker names | [Files and folders](references/Files-and-Folders.md) |
| Every command and option | [CLI reference](references/CLI-Reference.md) |
| Moving hand-made Docker servers to compost | [Migrating to compost](references/Migration.md) |
