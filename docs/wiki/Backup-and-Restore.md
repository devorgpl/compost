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
| `links` | Database dumps (`pg_dump`, plain SQL); ingress records and site files, for reference |
| `sources` | The uploaded sources (only the compose file when the folder isn't uploaded) |
| `volumes` | The named volumes listed under `backup.volumes` in `compost.yaml` |
| `images` | `docker save` of the services' images |

The default is `links,sources,volumes`. The stored config is always saved, and the overlay stored with `secret deploy`, if any. `--link main-db` or `--link database:main-db` saves only that link. `--stop` stops the services while volumes are copied, for a consistent copy, and starts them again afterwards.

Volumes must be listed to be saved:

```yaml
# compost.yaml
backup:
  volumes: [uploads]       # top-level `volumes:` keys of the compose file
```

Links to an [external database](Servers-and-Shared-Apps.md#external-databases) are dumped too, from a temporary `postgres` client container on the server, with the link's own credentials.

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

A server backup holds the stored `compost-server.yaml`, all link records and each shared app's data: Postgres `pg_dumpall` and its password file; Caddy's certificates, config and `sites`. It holds secrets. External databases are left out; `app backup` saves the apps' data on them.

`server restore` is for a server that doesn't have these shared apps yet (exit 3 if one is deployed). It puts back the records and files, deploys the shared apps, loads their data and then deploys Keycloak, which finds its data in the restored database.

## Moving to a new server

```sh
compost server backup -o old.compost.tar.gz -s old
compost app backup --all --include sources,volumes,images -o apps.compost.tar.gz -s old

compost server define new --address 198.51.100.7
compost server restore old.compost.tar.gz -s new --yes
compost app restore apps.compost.tar.gz --all -s new --yes
```

The server dump already holds the apps' databases and their link records keep the same credentials, so restore apps without `links`. Point DNS to the new server; Caddy's certificates came along, so there's no wait for Let's Encrypt.
