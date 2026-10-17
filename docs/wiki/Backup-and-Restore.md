# Backup and restore

```sh
compost app backup myapp-prod                       # links, sources, volumes → myapp-prod-<time>.compost.tar.gz
compost app backup --all                            # every instance in one archive
compost app backup myapp-prod --link main-db --format raw   # just the SQL dump
compost app restore myapp-prod-….compost.tar.gz --prefix review -f review.yaml   # a copy of the app
compost server backup                               # shared apps: data, config, secrets
compost server restore prod-….compost.tar.gz          # onto a new server, then `app restore`
compost backup list                                 # archives on the server
```

Backups take the server's lock, so they never run during a deploy.

## Targets

Where archives go: `--target` for backups, `--source` for restores. The default is `backup.target` in `compost-server.yaml`, else `local`.

| Target | Meaning |
|---|---|
| `local` | Streamed through the SSH connection to this machine and packed here. Nothing stays on the server. `-o` is a file or folder here. |
| `server` | Packed on the server in its backup folder (`backup.server_path`, default `~/.compost-server/backups`). No data crosses the network. `-o` and restore archives are file names in that folder, or absolute paths. |
| `s3` | Not implemented yet (exit 2). |

```yaml
# compost-server.yaml
backup:
  target: server
  server_path: backups      # absolute, or relative to compost's folder on the server
```

## Formats

| `--format` | Writes | Restorable |
|---|---|---|
| `compost` (default) | `<name>.compost.tar.gz` with a manifest and the configuration needed to restore | yes |
| `raw` | Only the data: one item as a plain file (e.g. a `.sql` dump), several as `<name>.tar.gz` | no |

Archives get mode `0600` and never overwrite an existing file. Default names: `<app>-<prefix>-<UTC yyyymmdd-hhmmss>`, `apps-<time>` for `--all`, `<server>-<time>` or `<server>-<shared app>-<time>`.

## App backup

```sh
compost app backup [<instance> | -f files | --app <name> --prefix <prefix> | --all]
                   [--include links,sources,volumes,images] [--link <entry>]… [--stop]
```

| Part | Saved |
|---|---|
| `links` | Database dumps (`pg_dump`, plain SQL); ingress and monitoring records and site files, for reference. Collected logs and metrics are not part of an app backup |
| `sources` | The uploaded sources (only the compose file when the folder isn't uploaded) |
| `volumes` | The named volumes listed under `backup.volumes` in `compost.yaml` |
| `images` | `docker save` of the services' images |

The default is `links,sources,volumes`. The stored config is always saved, and the overlay stored with `secret deploy`, if any. `--link main-db` or `--link database:main-db` saves only that link.

Volumes must be listed to be saved:

```yaml
# compost.yaml
backup:
  volumes: [uploads]       # top-level `volumes:` keys of the compose file
```

Links to an [external database](Servers-and-Shared-Apps.md#external-databases) are dumped too, from a temporary `postgres` client container on the server, with the link's own credentials.

For a consistent copy of files that a service keeps writing, stop it while the volumes are copied. `backup.stop` names those services (`true` for all); `--stop` stops all of them for one backup. They start again afterwards, as do the services that Compose stopped with them; services that weren't running stay stopped.

```yaml
backup:
  volumes: [uploads, search-index]
  stop: [worker, indexer]
```

## Steps around a backup or restore

Some data must be prepared before it is copied, e.g. a database that runs inside the app is dumped into a volume, and loaded from it after a restore. Steps run commands in the instance's services:

```yaml
# compose file
services:
  db:
    image: mariadb:11
    environment: {MARIADB_ROOT_PASSWORD: …}
    volumes: [db-dumps:/dumps, db-data:/var/lib/mysql]
  backup-job:                     # started only by the step: the profile keeps `app deploy` from starting it
    image: myorg/backup-tools
    profiles: [backup]
    volumes: [uploads:/uploads]
volumes: {db-dumps: {}, db-data: {}, uploads: {}}
```

```yaml
# compost.yaml
backup:
  volumes: [db-dumps, uploads]
  before_backup:
    - exec: db
      command: mariadb-dump -uroot -p"$MARIADB_ROOT_PASSWORD" --all-databases --single-transaction > /dumps/all.sql
    - run: backup-job             # its own command; or `command: [prepare, --fast]`
  after_backup:
    - {exec: db, command: rm -f /dumps/all.sql}
  after_restore:
    - {exec: db, command: mariadb -uroot -p"$MARIADB_ROOT_PASSWORD" < /dumps/all.sql}
```

| Step | Runs | Fails when |
|---|---|---|
| `exec: <service>` | `docker compose exec` in the running container; needs `command` | the service isn't running, or the command exits with an error |
| `run: <service>` | `docker compose run --rm`: a new container, removed afterwards; `command` replaces the service's own | the command exits with an error |

A string `command` runs with `sh -c` in the container, so `>`, `<`, `&&` and the container's variables work; a list (`[mariadb, --version]`) runs as is. Steps run on the server in order and their output is shown. They come from the config stored by the last `app deploy`, so deploy after changing them.

| List | When |
|---|---|
| `before_backup` | Before anything of the instance is saved, while its services run. A failed step stops the backup. |
| `after_backup` | After the data is saved and the stopped services started again. Also runs when the backup or a `before_backup` step failed, to clean up; its own failures are warnings. |
| `before_restore` | When data is loaded into a deployed instance (restore without sources): before its services are stopped. A failed step stops the restore. Not run by a restore with sources: that instance only starts after its data is loaded. |
| `after_restore` | After the data is loaded and the services are ready (healthy, for services with a healthcheck), in both kinds of restore. A failed step fails the restore. |

The backup steps run when the backup saves links or volumes (not for `--include sources` or `images` alone).

## App restore

```sh
compost app restore <archive> [--instance <instance> | --all] [--prefix <prefix>] [-f overlays]
                    [--include …] [--link …]… [--replace] [--yes]
```

**With sources** (the default when the archive has them), it deploys the instance from the archive as `app deploy --apply` would, then loads databases, images and volumes before the services start. The archived config is the base; `-f` overlays are merged over it and `--prefix` changes the instance. That makes copies:

```sh
compost app restore myapp-prod-20261015-120000.compost.tar.gz --prefix review -f review-domains.yaml
```

The copy needs other domains (one domain, one instance), and its databases follow the new prefix unless the config sets `db_name`.

**Without sources** (`--include links,volumes`), it loads data into the deployed instance: it lists what will be replaced, asks unless `--yes`, stops the services, loads, and starts them again.

Both run the instance's [restore steps](#steps-around-a-backup-or-restore): those of the restored config with sources, those of the deployed instance without.

Databases and volumes that already hold data are only replaced with `--replace` (exit 3 otherwise). A link's database is created first if it doesn't exist yet, e.g. after `app deploy --no-apply` of a link that moved to another database.

## Moving an app's data to another database

Back up, point the link at the other database, render the deploy without starting it, load the data, deploy:

```sh
compost app backup myapp-prod -o before.compost.tar.gz
# compost.yaml: change `instance` of the database link (e.g. from an external database to compost's)
compost app deploy --no-apply -f compost.yaml,.compost-prod.yaml
compost app restore before.compost.tar.gz --include links,volumes --yes
compost app deploy --apply -f compost.yaml,.compost-prod.yaml
```

The old database is left as it was. Volumes move the same way when their name changes, e.g. after removing an `external` volume from a compose override. See [Migration](Migration.md#phase-4-data-into-compost).

## Server backup and restore

```sh
compost server backup [--shared <name>]… [--format …] [-o …] [--target …]
compost server restore <archive> [--shared <name>]… [--source …] [--yes]
```

A server backup holds the stored `compost-server.yaml`, the secrets stored with `server deploy --secret` (restored only when the server has none), all link records and each shared app's data: Postgres `pg_dumpall` and its password file; Caddy's certificates, config and `sites` (for `caddysecurebuild` also its CrowdSec configuration, but not the agent's bans: a restored server starts without bans, with a new bouncer key); OpenObserve's data folder and root login. OpenObserve keeps recent data in memory, so it is stopped while its folder is copied (seconds to minutes, depending on its size); the collector keeps retrying what arrives meanwhile for up to five minutes. It holds secrets. External databases are left out; `app backup` saves the apps' data on them.

`server restore` is for a server that doesn't have these shared apps yet (exit 3 if one is deployed). It puts back the records and files, deploys the shared apps, loads their data and then deploys Keycloak, which finds its data in the restored database, and OpenObserve, which needs Caddy for its domain. The `monitoring` of shared apps is set up in that last step too.

## Moving to a new server

```sh
compost server backup -o old.compost.tar.gz -s old
compost app backup --all --include sources,volumes,images -o apps.compost.tar.gz -s old

compost server define new --address 198.51.100.7
compost server restore old.compost.tar.gz -s new --yes
compost app restore apps.compost.tar.gz --all -s new --yes
```

The server dump already holds the apps' databases and their link records keep the same credentials, so restore apps without `links`. Point DNS to the new server; Caddy's certificates came along, so there's no wait for Let's Encrypt.
