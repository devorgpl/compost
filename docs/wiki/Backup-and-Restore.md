# Backup and restore

```sh
compost app backup myapp-prod                       # links, sources, volumes → myapp-prod-<time>.compost.tar.gz
compost app backup --all                            # every instance in one archive
compost app backup myapp-prod --link main-db --format raw   # just the SQL dump
compost app restore myapp-prod-….compost.tar.gz --prefix review -f review.yaml   # a copy of the app
compost server backup                               # shared apps: data, config, secrets
compost server restore prod-….compost.tar.gz          # onto a new server, then `app restore`
compost backup run daily                            # a profile: shared apps and every instance, old archives deleted
                                                    # (or by itself, on the server: `schedule` in the profile)
compost backup list                                 # archives on the server
```

Backups take the server's lock, so they never run during a deploy.

## Targets

Where archives go: `--target` for backups, `--source` for restores and `backup list`. Without one, a backup goes to its [profile](#profiles)'s target, else the app's `backup.target` (in `compost.yaml`), else `backup.target` in `compost-server.yaml`, else `local`.

| Target | Meaning |
|---|---|
| `local` | Streamed through the SSH connection to this machine and packed here. Nothing stays on the server. `-o` is a file or folder here. |
| `server` | Packed on the server in its backup folder (`backup.server_path`, default `~/.compost-server/backups`). No data crosses the network. `-o` and restore archives are file names in that folder, or absolute paths. |
| a name from `backup.targets` | A target you define. `type: server` is another folder on the server, e.g. a mounted disk; it works like `server`. `type: s3` is an S3 bucket; see below. |

```yaml
# compost-server.yaml
backup:
  target: server
  server_path: backups      # absolute, or relative to compost's folder on the server
  targets:
    nas: {type: server, path: /mnt/nas/compost}
    offsite:
      type: s3
      bucket: acme-backups
      path: prod                                # folder in the bucket (optional)
      endpoint: https://s3.eu-central-1.amazonaws.com
      region: eu-central-1
```

### S3 targets

An archive for an `s3` target is packed on the server as for `server`, copied to the bucket, then deleted from the server. Restores (`--source offsite`) copy it back into the server's `tmp/` first. `backup list --source offsite` lists the bucket's folder, and `keep` and `backup prune` delete there. The copying is done by [rclone](https://rclone.org) in a container on the server (`rclone/rclone`), so the server needs to reach the bucket; nothing is installed.

| Key | Default | Meaning |
|---|---|---|
| `bucket` | required | Bucket name |
| `path` | – | Folder (key prefix) in the bucket |
| `endpoint` | AWS | URL of an S3-compatible service, e.g. Backblaze B2, Cloudflare R2, Hetzner, MinIO, Garage |
| `region` | – | Region, where the service needs one |
| `provider` | `Other` | rclone's S3 provider, e.g. `AWS`, `Minio`, `Cloudflare`, for provider-specific behavior |
| `credentials` | `keys` | `keys`: the two keys below. `instance`: none; rclone takes them from the server's environment, e.g. the EC2 instance role (see below) |
| `access_key_id`, `secret_access_key` | required with `keys` | Credentials; secrets, see below |
| `image` | `rclone/rclone` of a fixed version | Another rclone image, e.g. from a mirror |

The credentials go in the secrets file, and must be stored on the server, since backups and restores read them there:

```yaml
# .compost-server.secrets.yaml
backup:
  targets:
    offsite: {access_key_id: AKIA…, secret_access_key: …}
```

```sh
compost server deploy --apply --secret .compost-server.secrets.yaml
```

They are left out of the stored `compost-server.yaml` and reach rclone in a file readable only by the server user, never on a command line. The key needs to list, read, write and delete objects in that folder; compost doesn't create the bucket.

#### EC2 instance role

A server on AWS EC2 with an instance profile needs no keys: give the role those rights on the bucket and set `credentials: instance`.

```yaml
# compost-server.yaml
backup:
  targets:
    offsite:
      type: s3
      bucket: acme-backups
      provider: AWS
      region: eu-central-1
      credentials: instance
```

rclone then runs with `env_auth` and the host's network (`--network host`), so it reaches the instance metadata service (IMDSv2) despite its default hop limit of 1. Keys in the config or stored on the server are not used.

`app backup --all` without a profile writes one archive, to `--target` or the server's `backup.target`; an app's own `backup.target` applies to backups of that app alone and to profiles.

## Profiles

A profile names a kind of backup: what it saves, where it goes and how many archives of it to keep. Define them in `compost-server.yaml`, where they apply to the shared apps and every app:

```yaml
# compost-server.yaml
backup:
  target: server
  profiles:
    daily:     {include: [links], keep: 7, schedule: "0 3 * * *"}   # databases only, every night
    weekly:    {include: [links, sources, volumes], keep: 4, target: nas, schedule: "0 4 * * sun"}
    predeploy: {keep: 3, server: false}                         # apps only; see below
```

```sh
compost backup run daily                         # shared apps and every instance, one archive each
compost app backup myapp-prod --profile weekly   # one instance
compost app backup --all --profile weekly        # every instance, one archive each
compost server backup --profile daily            # the shared apps
compost app backup myapp-prod --profile weekly --target local   # flags override the profile
```

| Key | Default | Meaning |
|---|---|---|
| `include` | `links,sources,volumes` | Parts of an app backup (as `--include`) |
| `links` | all | Only these links (as `--link`) |
| `format` | `compost` | As `--format` |
| `target` | the app's, then the server's `backup.target` | Where the archives go |
| `keep` | all | Archives kept per instance (and for the shared apps) at the target; older ones are deleted after each backup with the profile |
| `schedule` | – | When the backup runs by itself, as a cron expression; see [Scheduled backups](#scheduled-backups) |
| `server` | `true` | `compost-server.yaml` only: `false` leaves the shared apps out of `backup run` and refuses `server backup --profile` |

An app changes a profile for itself, opts out of it or adds its own in `compost.yaml`. Its profiles also take the app's backup settings, `volumes`, `stop`, `before_backup` and `after_backup`, which replace the app's `backup` keys for that profile:

```yaml
# compost.yaml
backup:
  volumes: [uploads]
  profiles:
    weekly: {keep: 8, schedule: "0 2 * * sat"}   # the other keys come from the server's profile
    daily: false                          # left out of `backup run daily`
    anonymized:                           # this app's own profile
      include: [volumes]
      volumes: [anon-dumps]
      format: raw
      keep: 2
      before_backup:
        - run: anonymizer                 # dumps the database and anonymizes it into anon-dumps; `profiles: [backup]`
                                          # in the compose file keeps `app deploy` from starting it
```

The keys of the app's profile win over the server's, and a key neither sets comes from the app's `backup` settings. Lists replace, they don't add up. The app's profiles take effect with its next `app deploy`, like its other backup settings.

An archive of a profile is named `<instance>-<profile>-<time>`, or `<server>-<profile>-<time>` for the shared apps, and its manifest names the profile. Archives without a profile are never deleted by `keep`.

`backup run` carries on when one instance fails, then exits with an error naming every failure.

### Deleting old archives

`keep` applies after each backup with the profile. To apply it to archives already there, or with another number:

```sh
compost backup prune                       # every profile with `keep`, at each profile's target
compost backup prune --profile daily --keep 3 --target nas --yes
```

It lists what it deletes and asks unless `--yes`. With `--target local` it looks in the current folder.

## Scheduled backups

A profile with a `schedule` runs by itself, on the server, for the shared apps and for every instance it applies to, each on its own:

```yaml
# compost-server.yaml
backup:
  profiles:
    daily: {include: [links], keep: 7, schedule: "0 3 * * *"}
```

```sh
compost server deploy --apply      # installs compost on the server and a crontab line
compost server state               # shows each scheduled backup's last run
```

`schedule` is a cron expression in the server's local time: minute, hour, day of month, month and day of week, each `*`, a number, a range (`1-5`), a list (`1,15`) or a step (`*/6`). Days of the week are `0`–`7` (Sunday is `0` and `7`) or `mon`…`sun`, months `1`–`12` or `jan`…`dec`. `@hourly`, `@daily`, `@weekly` and `@monthly` work too. When both day of month and day of week are set, either one matches, as in cron.

An app gives a profile its own time with `schedule` in its `compost.yaml`, keeps the profile without a schedule with `schedule: off`, or leaves it with `false`; its own profiles can have a schedule as well:

```yaml
# compost.yaml
backup:
  profiles:
    daily: {schedule: "30 3 * * *"}    # half an hour later than the others
    weekly: {schedule: off}            # only `backup run weekly` or `--profile weekly`
```

**How it runs.** When any profile has a schedule, `server deploy`, `app deploy` and `server restore` put the same version of compost at `~/.compost-server/bin/compost` and add a line to the server user's crontab that runs `compost backup tick` every five minutes. The tick runs every backup whose time has come since its last run, one after another, as `app backup --profile` and `server backup --profile` would: the same archive names, targets and `keep`. It needs nothing from your machine; S3 credentials must be stored on the server (`server deploy --secret`), unless the target uses [the instance role](#ec2-instance-role). Its output goes to `~/.compost-server/logs/backup.log`.

- **New schedules** wait for their first time; they don't run at once.
- **Missed times**, e.g. while the server was off, are caught up once at the next tick, if they are less than a month old.
- **Deploys first**: while another command holds the server's lock, the tick waits for it.
- **Results** are kept in `state.yaml`, and a failure (and the next run that works) is [notified](Notifications.md) when `notifications` is set: `server state` lists every scheduled backup with its last run (`ok`, `failed` with the error, or `waiting` for its first run), `app state` those of the instance. After `app remove`, the instance's schedule stops, but its line stays as `instance removed; last run …`, and so do its archives.
- When no profile has a schedule any more and nothing is notified, the next deploy removes the crontab line; `app remove` does too, if it removed the last scheduled instance.

**Requirements.** The server needs `cron` (the `crontab` command). compost copies itself to the server when it is a static Linux build for the server's CPU (the release builds are) or when the server is this machine; otherwise the server downloads the same release from GitHub and checks it against `SHA256SUMS`, which needs `curl` or `wget`. A development build that can't run on the server can't do either: set `COMPOST_SERVER_BINARY` to a static Linux build to install that one. The line names the server as you defined it (`compost server define`), which goes into the archive names; deploy from machines that use the same name.

**When nothing runs.** If `server state` lists no scheduled backups, or one stays `waiting` past its time, check on the server:

- `crontab -l` has the line between `# compost: scheduled backups of …` and `# compost: end of …`. If not, deploy again (`server deploy --apply`).
- `~/.compost-server/logs/backup.log` grows every five minutes. If it doesn't exist, cron isn't running the line: check that the `cron` service runs, and run the line by hand to see its error.
- `compost backup run <profile>` works. It runs the same backups now, without cron.

## Before a deploy

`deployment.backup` names a profile that `app deploy --apply` runs before it changes an instance that is already deployed:

```yaml
# compost.yaml
deployment:
  backup: predeploy
```

The backup uses the deployed instance's data and links, with the profile as the config being deployed defines it. If it fails, the deploy stops before changing anything. `--backup <profile>` picks another profile for one deploy, `--no-backup` skips it, and `--dry-run` says which one would run. The `deploy.backup` [preference](Configuration.md#preferences) does the same for every deploy to a server: `false` skips it (e.g. on a test server), a profile name replaces `deployment.backup`. `app restore` and `app rollback` don't take this backup.

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
| `links` | Database dumps (plain SQL); the objects of storage buckets; ingress, monitoring, key-value and workflow records and site files, for reference. Collected logs and metrics, key-value data and workflows are not part of an app backup |
| `sources` | The uploaded sources (only the compose file when the folder isn't uploaded) |
| `volumes` | The named volumes listed under `backup.volumes` in `compost.yaml` |
| `images` | `docker save` of the services' images |

The default is `links,sources,volumes`. The stored config is always saved, and the overlay stored with `secret deploy`, if any. [Releases](Applications.md#releases-and-rollback) are not saved. `--link main-db` or `--link database:main-db` saves only that link.

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

A server backup holds the stored `compost-server.yaml`, the secrets stored with `server deploy --secret` (restored only when the server has none), all link records and each shared app's data: Postgres `pg_dumpall` and its password file; Caddy's certificates, config and `sites` (for `caddysecurebuild` also its CrowdSec configuration, but not the agent's bans: a restored server starts without bans, with a new bouncer key); OpenObserve's data folder and root login; Temporal's shard count (its workflows are in its Postgres databases). OpenObserve keeps recent data in memory, so it is stopped while its folder is copied (seconds to minutes, depending on its size); the collector keeps retrying what arrives meanwhile for up to five minutes. It holds secrets. External databases are left out; `app backup` saves the apps' data on them.

`server restore` is for a server that doesn't have these shared apps yet (exit 3 if one is deployed). It puts back the records and files, deploys the shared apps, loads their data and then deploys Keycloak and Temporal, which find their data in the restored databases, and OpenObserve, which needs Caddy for its domain. The `monitoring` of shared apps is set up in that last step too.

## Moving to a new server

```sh
compost server backup -o old.compost.tar.gz -s old
compost app backup --all --include sources,volumes,images -o apps.compost.tar.gz -s old

compost server define new --address 198.51.100.7
compost server restore old.compost.tar.gz -s new --yes
compost app restore apps.compost.tar.gz --all -s new --yes
```

The server dump already holds the apps' databases and their link records keep the same credentials, so restore apps without `links`. Point DNS to the new server; Caddy's certificates came along, so there's no wait for Let's Encrypt.
