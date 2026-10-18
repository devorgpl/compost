# Applications

An app is a compose file plus `compost.yaml`, which says how compost links its services to the server's shared apps. Each deployment is an **instance** named `<app>-<prefix>`, e.g. `myapp-staging`. The full list of keys is in [Configuration](Configuration.md#compostyaml).

## Start from your compose file

```sh
cd myapp                       # has docker-compose.yaml
compost app generate [--compose-file compose.yaml] [--name myapp]
```

This writes `compost.yaml`, with one `app_services` entry per compose service and commented-out links, and `.compost.secrets.example.yaml` for the per-instance values. Neither needs a server.

A minimal `compost.yaml` for a web service behind the ingress, with a database:

```yaml
name: myapp
deployment:
  build: true                  # build images on the server (uploads the folder)
services:
  ingress:
    - name: site
      routes:
        - {name: api, target: backend, port: 5000, path: /api}
        - {name: web, target: web, port: 8080}           # path "/" by default
  database:
    - name: main-db
      type: postgres
      version: ">=16"
      targets: [backend]
      environment:
        DATABASE_URL: db_url
app_services:
  - {name: web, service: web}
  - {name: backend, service: backend}
```

And the overlay of one instance, `.compost-staging.yaml`:

```yaml
prefix: staging
services:
  ingress:
    - name: site
      domains: [staging.example.com]
```

## Services

`app_services` gives compose services the names that links use, and can add fixed variables:

```yaml
app_services:
  - name: backend
    service: backend-webapp    # the service in the compose file
    environment:
      APP_MODE: production
```

Compose services not listed are deployed as they are, but links can't target them. compost never changes your compose file: it writes an override file on the server that adds networks and variables.

## Sources in another folder

`compost.yaml` doesn't have to live in the app's repository. In a separate deployment repository, `source_dir` points to the app's folder: that folder is uploaded (with `build: true` or `attach_sources: true`) and holds the compose file.

```yaml
name: shop
deployment:
  source_dir: ../upstream/shop          # relative to compost.yaml
  compose_file: docker-compose.yaml     # relative to source_dir
  compose_overrides: [compose.prod.yaml]   # relative to compost.yaml
```

`compose_overrides` are compose files applied after the app's own, as with `docker compose -f … -f …`. With them you can change an app you can't or don't want to edit: turn off services (`profiles: [off]`), reuse existing volumes (`external: true` with `name:`), change images or resources. Relative paths inside them refer to `source_dir`. They are uploaded with the sources as `.compost/compose-override-<n>.yaml`, so backups and restores include them.

`compost app generate --compose-file ../upstream/shop/docker-compose.yaml` sets `source_dir` for you. See [Migration](Migration.md#where-compostyaml-lives) for a full layout.

## Ingress links

An ingress entry routes domains to services. Domains usually differ per instance, so they go in the overlay.

```yaml
services:
  ingress:
    - name: site
      optional: false          # true: skip this entry (with a notice) when no domains are given
      domains: [example.com, www.example.com]
      routes:
        - name: ui
          target: web          # an app_services name
          port: 8080           # the container port
          path: /ui            # default "/"
          strip_prefix: false  # true: /ui/x reaches the service as /x
```

Longer paths win, `/` comes last, and paths no route matches get a 404. A domain can be used by only one instance on a server.

A route can also serve another link of the app instead of a service, with `link: <kind>/<entry>`. This is how a Keycloak realm gets its own domain; see [OIDC links](#oidc-links).

### Request filtering

With a [caddysecurebuild](Servers-and-Shared-Apps.md#caddysecurebuild-ingress) ingress, every entry gets the server's `security.defaults`: CrowdSec bans, AppSec checks and rate limits. An entry can choose for itself; a field it leaves out keeps following the default:

```yaml
services:
  ingress:
    - name: site
      routes: [ … ]
      security:
        bans: true               # block addresses CrowdSec banned
        appsec: false            # e.g. a CMS whose editor posts HTML that looks like an attack
        rate_limits: [login]     # zones of the server, by name; replaces the default list, [] for none
        own_rate_limits:         # zones of this entry only, keys as on the server
          - name: api
            paths: [/api/*]
            events: 100
            window: 10s
```

`app deploy` fails when a named zone isn't defined on the server, or when the server's ingress is plain `caddy`. A blocked request gets 403, a rate-limited one 429.

## Database links

A database link creates a database and a user in a matching database server (`postgres`, `mariadb` or `mysql`) and passes the connection values to services:

```yaml
services:
  database:
    - name: main-db
      type: postgres
      version: ">=16"          # requirement on the major version; default "*"
      instance: postgres18     # optional: this shared app, when several match
      targets: [backend, migrate]
      environment:             # variable → link value
        DATABASE_URL: db_url
        PGHOST: db_host
```

| Link value | Example |
|---|---|
| `db_host` | `postgres18` |
| `db_port` | `5432` |
| `db_name` | `myapp_staging_main_db` |
| `db_user` | `myapp_staging_main_db` |
| `db_password` | generated on the first deploy, then kept |
| `db_url` | `postgres://user:password@postgres18:5432/dbname`; `mysql://…:3306/…` for MariaDB and MySQL |

In the overlay you can set `db_name`, `db_user` and `db_password`. The defaults come from `<app>_<prefix>_<entry>`, so renaming the entry gives a new, empty database unless you set them. MariaDB and MySQL user names are at most 32 characters: a longer default is cut and ends in a short hash.

**Existing databases.** To link to a database compost doesn't run, register it as an [external database](Servers-and-Shared-Apps.md#external-databases) and pin it with `instance`. Unless compost has root credentials for it, give the existing `db_name`, `db_user` and `db_password`. compost then only passes them to the services:

```yaml
    - name: main
      type: postgres
      instance: shop-db
      db_name: shop
      db_user: shop
      db_password: …           # in the overlay
```

## Key-value links

A key-value link gives services a Valkey or Redis server of their own to use, e.g. as a cache, for sessions or for job queues:

```yaml
services:
  keyvalue:
    - name: cache
      type: valkey             # or redis
      version: "*"             # requirement on the major version
      instance: valkey         # optional: this shared app, when several match
      targets: [backend, worker]
      environment:
        REDIS_URL: kv_url
```

| Link value | Example |
|---|---|
| `kv_host` | `valkey` |
| `kv_port` | `6379` |
| `kv_user` | `myapp-staging-cache` |
| `kv_password` | generated on the first deploy, then kept |
| `kv_db` | `3` on Valkey; `0` on Redis |
| `kv_prefix` | `myapp-staging-cache:` on Redis; empty on Valkey |
| `kv_url` | `redis://myapp-staging-cache:…@valkey:6379/3` |

**Valkey** gives every link its own logical database and a user locked to it: the app can't read, change or flush another app's keys, and needs no changes. **Redis** has no such lock, so every link gets a user limited to the key prefix `kv_prefix`, and **the app must put that prefix in front of its keys**: most libraries have a setting for it (e.g. `prefix` in ioredis, Laravel's `REDIS_PREFIX`, `key_prefix` in Django's cache). Commands that see every key (`KEYS`, `FLUSHDB`, `FLUSHALL`) are refused there. Use Valkey when an app can't set a prefix.

`--purge keyvalue` on `app remove` (or for a dropped link) deletes the link's keys; otherwise they stay. App backups don't save them: a cache comes back by itself, and a server backup holds everything.

## Storage links

A storage link gives services an S3 bucket and a key for it:

```yaml
services:
  storage:
    - name: files
      targets: [backend]
      environment:
        S3_ENDPOINT: s3_endpoint
        S3_REGION: s3_region
        S3_BUCKET: s3_bucket
        AWS_ACCESS_KEY_ID: s3_access_key_id
        AWS_SECRET_ACCESS_KEY: s3_secret_access_key
```

| Link value | Example |
|---|---|
| `s3_endpoint` | `http://storage:3900`: on the storage network, for the services |
| `s3_public_endpoint` | `https://files.example.com`: the storage app's `url`, for browsers and presigned URLs; else as `s3_endpoint` |
| `s3_region` | `garage` |
| `s3_bucket` | `myapp-staging-files` |
| `s3_access_key_id`, `s3_secret_access_key` | made on the first deploy, then kept |

With [Garage](Servers-and-Shared-Apps.md#garage-and-s3-storage), every link gets its own bucket, `<app>-<prefix>-<entry>` (3 to 63 characters), and a key that can use only that bucket (it can't create others). Use path-style addressing in the app's S3 client (e.g. `forcePathStyle: true`, `use_path_style_endpoint`, `AWS_S3_ADDRESSING_STYLE=path`).

**External S3** (AWS, Backblaze B2, an existing MinIO): pin the storage app with `instance` and give the existing bucket and key, usually in the overlay. compost only passes them on:

```yaml
    - name: archive
      instance: aws
      bucket: shop-archive
      access_key_id: AKIA…          # in the overlay
      secret_access_key: …
```

`app backup` saves the bucket's objects (`links/storage/<entry>.tar`, fetched with rclone); `app restore` puts them back, and replaces a bucket that holds objects only with `--replace` (it then matches the backup). On `app remove`, a Garage link's key is deleted and its bucket kept; `--purge storage` deletes the bucket and its objects too. compost never deletes anything on external storage.

## OIDC links

An OIDC link creates a realm, a client and optionally users in a Keycloak, and passes the client settings to services:

```yaml
services:
  oidc:
    - name: login
      type: keycloak
      version: ">=26"
      realm: myapp                     # default <app>-<prefix>; created if missing
      client_id: web                   # default <app>-<prefix>-<entry>
      public_client: false             # true: no client secret, e.g. for a single-page app
      redirect_uris: ["https://staging.example.com/*"]   # required, usually in the overlay
      web_origins: []
      client_secret: …                 # secret; generated on the first deploy, then kept
      users:                           # secret; created if missing, passwords set on every deploy
        - {username: alice, email: alice@example.com, first_name: Alice, last_name: Doe, password: …}
      targets: [backend]
      environment:
        OIDC_ISSUER: oidc_issuer
        OIDC_CLIENT_ID: oidc_client_id
        OIDC_CLIENT_SECRET: oidc_client_secret
```

| Link value | Example |
|---|---|
| `oidc_issuer` | `https://auth.example.com/realms/myapp` |
| `oidc_internal_url` | `http://keycloak:8080/realms/myapp`, for server-to-server calls |
| `oidc_realm` | `myapp` |
| `oidc_client_id` | `web` |
| `oidc_client_secret` | empty for public clients |

**A realm on its own domain.** Add `public_url` to the link and an ingress route that serves it:

```yaml
services:
  oidc:
    - name: login
      public_url: https://login.example.com
  ingress:
    - name: login-page
      domains: [login.example.com]
      routes:
        - {name: realm, link: oidc/login}     # no target, port or path
```

That domain serves only `/realms/<realm>` and `/resources`; the admin console and other realms get a 404 there. The issuer and login pages use `public_url`. Links that share a realm must agree on it.

## Monitoring links

A monitoring link sends the output of services to a [monitoring app](Servers-and-Shared-Apps.md#openobserve-monitoring), has their Prometheus metrics scraped, and passes the collector's address to services that send OpenTelemetry data themselves:

```yaml
services:
  monitoring:
    - name: telemetry
      optional: true                   # skip (with a notice) when the server runs no monitoring app
      instance: monitoring             # optional: this shared app, when there are several
      targets: [web, backend]          # their output is collected; they get the variables
      logs: true                       # false: only variables and metrics
      metrics:                         # Prometheus endpoints, on any app service
        - {target: backend, port: 9100, path: /metrics}   # path "/metrics" by default
      environment:
        OTEL_EXPORTER_OTLP_ENDPOINT: otlp_endpoint
        OTEL_SERVICE_NAME: service_name
        OTEL_RESOURCE_ATTRIBUTES: resource_attributes
```

| Link value | Example |
|---|---|
| `otlp_endpoint` | `http://monitoring:4318`, OTLP over HTTP |
| `otlp_grpc_endpoint` | `http://monitoring:4317` |
| `otlp_protocol` | `http/protobuf` |
| `service_name` | `backend`: the compose service of each target |
| `resource_attributes` | `service.namespace=myapp-staging,deployment.environment=staging` |

Services in `targets` and `metrics` join the monitoring network as `<instance>-<service>`. The collector picks up a metrics endpoint once the app runs, without a restart.

compost collects the output with Docker's `fluentd` log driver, set in its override file, so `docker logs` keeps working. A service that sets its own `logging` in the compose file can't be a target with `logs: true`: the deploy fails and says so.

## Deploying

```sh
compost app deploy [--apply | --no-apply] [--pull] [--recreate] [-f files] [--prefix <prefix>] [--purge <kind>] [--dry-run [--diff]]
                   [--backup <profile> | --no-backup]
```

`-f` defaults to `compost.yaml`. Add overlays with `-f compost.yaml,.compost-staging.yaml`. If the instance has an overlay stored on the server ([Secrets](Secrets.md)), it is merged right after the first file. `--prefix` overrides the prefix from the files (default `main`).

A deploy:

1. validates the config and checks the compose file's service names (exit 2 on errors);
2. checks the server and picks a shared app for each link (exit 3 if none matches, or a domain is taken); with `--apply` and `deployment.backup` (or `--backup <profile>`), backs up the deployed instance with that [profile](Backup-and-Restore.md#before-a-deploy), and stops if the backup fails;
3. uploads the sources: the whole folder with `build: true` or `attach_sources: true`, otherwise only the compose file. Left out: `.git/`, files given with `-f`, `.compost-*.yaml`, `.compost.*.yaml` and patterns in `.compostignore` (gitignore syntax);
4. writes the override file and link records. **Without `--apply` it stops here.**
5. with `--apply`: creates databases, users and realms; runs `docker compose build` (with `build: true`) or `pull`; then `up -d --remove-orphans`; waits up to 60 s until every service runs (or is healthy) or has exited with code 0; removes dropped links; updates Caddy's routes; records a [release](#releases-and-rollback).

The result is shown as by `app state`. Running the same deploy again keeps data and passwords and doesn't recreate unchanged containers.

**Updating images and recreating containers.** Without `build: true`, every deploy with `--apply` pulls, so a new image behind the same tag (`:latest`, `:2`) is picked up and its containers are recreated. Two options, both needing `--apply`, do more:

- `--pull`: with `build: true`, also pulls the images of services without `build` and builds with `--pull`, which fetches newer base images. Without `build: true` it changes nothing.
- `--recreate`: recreates every container (`up -d --force-recreate`), even unchanged ones. Volumes, databases and passwords are kept.

**What a deploy would change.** `--dry-run` runs the deploy as far as it can without changing the server and compares the result with what is there:

```
Dry run: nothing is changed on the server.
  instance myapp-staging (deployed, release 4)
  sources (14 files):
    changed  apps/myapp/staging/sources/docker-compose.yaml
  services.database[main-db]: uses postgres postgres18
  services.ingress[site]: uses caddy caddy-main
  files:
    changed  connectors/ingress/caddy-main/myapp-staging-site.yaml
    changed  shared/ingress/caddy-main/conf/sites/myapp-staging-site.caddy
    changed  apps/myapp/staging/compose/compost.override.yaml  (secret, not shown)
  services:
    backend  recreated: environment: APP_MODE
    web      built again; recreated if its image changes
    worker   created
  records release 5
```

With `deployment.backup`, a line after the instance's says which profile backs it up first.

- **sources**: files added, changed or removed, compared by checksum with the uploaded ones.
- **files**: what compost writes besides the sources: the override, the stored config, link records and, with `--apply`, Caddy's site files. Files with secrets (link records of databases and OIDC, the override, a `--secret` file) are only named.
- **services**: what `docker compose up` would do, judged from each service's definition in the compose files and the override: `recreated` (naming what changed; for variables only their names), `created`, `removed`, `built again` (`build: true` and changed sources), `pulled` (every deploy without `build: true` pulls; a newer image recreates it) or `unchanged`. Compose makes the final decision on the server, so treat this as a close estimate.

`--diff` adds the changed lines of every file without secrets, as a unified diff. It needs `--dry-run`.

**Dropped links.** When a link of the last deploy is no longer in the config (removed, renamed, an optional ingress without domains, or now matching another shared app), the deploy removes it after the new containers run: its record, routing and scraping go, its data stays (collected logs and metrics stay in the monitoring app). Add `--purge database` (or `oidc`) to delete that data too. `--dry-run` lists them first.

## Releases and rollback

Every deploy with `--apply` whose services start is recorded as a numbered **release** on the server: a copy of the uploaded sources, the effective config (without secrets) and the image each service runs. The image is also tagged `compost-release/<instance>:<n>-<service>`, so a later pull of the same tag or `docker image prune` doesn't remove it. `app restore` and `app rollback` record releases too; a deploy without `--apply` doesn't.

```sh
compost app releases myapp-staging              # or: -f compost.yaml,… / --app myapp --prefix staging
compost app rollback myapp-staging [--to <n>] [--dry-run] [--yes]
```

`app releases` lists them newest first, with the time, who deployed (`user@host`), the compost version and short image IDs. The newest is the `current` one.

`app rollback` deploys the release before the current one, or `--to <n>`, as `app deploy --apply` would: it starts the images that release ran (by ID, without pulling or building) with its compose file and config. First it shows the images and links that change and asks for confirmation unless `--yes`; `--dry-run` stops after that. The rollback becomes a new release (`rollback of <n>`), so you can roll it back too.

- **Data is not rolled back.** Databases and volumes stay as they are; a migration the newer release ran is not undone. Take an `app backup` before deploys that change data.
- **Links follow the release.** A link that the release doesn't have is removed like any [dropped link](#deploying) (its data is kept); a link it has but the current deploy doesn't is created again. Passwords and the stored overlay ([Secrets](Secrets.md)) are the current ones.
- **Missing images.** If a pinned image was removed by hand, the rollback warns and pulls or builds as a deploy would.

`deployment.releases` (default `5`) is how many releases an instance keeps; older ones and their image tags are deleted after each deploy, and `0` keeps none. Each release holds a copy of the sources, so lower it for big folders uploaded with `build: true`. Releases are not part of [backups](Backup-and-Restore.md), and `app remove` deletes them with their image tags. [`server prune`](Servers-and-Shared-Apps.md#disk-space) keeps release images when it removes unused ones.

## Inspecting

```sh
compost app list                                 # every instance: status, running, last update
compost app state myapp-staging                  # or: -f compost.yaml,.compost-staging.yaml / --app myapp --prefix staging
```

`app state` shows the instance's status, its current release, each compose service (running, exited, health), domains and routes, and databases (never passwords). It exits 4 if something is not healthy. A service that exited with code 0, like a migration, counts as healthy, unless it has `restart: always` or `unless-stopped`.

Both read what the deploy stored on the server, so they don't need the app's repository.

### Logs

```sh
compost app logs myapp-staging                          # last 100 lines of every service
compost app logs myapp-staging --service backend --follow   # stream until Ctrl-C
compost app logs myapp-staging --since 1h --timestamps --tail all
```

`app logs` runs `docker compose logs` on the server with the instance's compose files, so you don't need to know its project or files. `--service` (repeatable or comma-separated) picks compose services; an unknown one exits 2 and lists the instance's services. `--tail` is the number of lines per container (default `100`, or `all`), `--since` a duration (`10m`) or time (`2026-10-17T12:00:00`). On a terminal the output keeps compose's colors, and Ctrl-C ends `--follow` normally (exit 0); piped, it is plain text, e.g. `compost app logs myapp-staging | grep ERROR`.

It shows what Docker still keeps for the current containers: a recreated container starts with empty logs. A [monitoring link](#monitoring-links) keeps the history in OpenObserve.

### Commands in containers

```sh
compost app exec myapp-staging --service backend -- sh                 # a shell in the running container
compost app exec myapp-staging --service backend -- ls -la /data
compost app run  myapp-staging --service backend -- python migrate.py  # a one-off container
compost app run  myapp-staging --service backend --no-deps -- python manage.py check
```

The command and its arguments come after `--`. `app exec` runs it in the service's running container (`docker compose exec`); `app run` starts a new container of the service with the same image, variables, networks and volumes, and removes it afterwards (`docker compose run --rm`), so it works when the service isn't running, and starts the services it `depends_on` unless `--no-deps`. Both use the instance's compose files and compost's override, so link variables like `DATABASE_URL` are set.

When stdin and stdout are terminals, the command gets a terminal on the server (a shell works, Ctrl-C goes to the command); otherwise input and output can be piped, e.g. `compost app exec myapp-staging --service backend -- cat /data/export.csv > export.csv`. compost exits with the command's exit status.

They change what the command changes: there is no dry run and no lock.

## Removing

```sh
compost app remove myapp-staging [--volumes] [--purge database,oidc] [--yes]
```

Removes the routes and link records, then `docker compose down`, the instance's folder (with its releases) and the image tags kept for rollback. Databases and realms stay unless their kind is in `--purge` (databases on an external server stay anyway, see [External databases](Servers-and-Shared-Apps.md#external-databases)); `--volumes` also removes the compose volumes, except external ones. It asks for confirmation unless `--yes`. A stored overlay ([Secrets](Secrets.md)) stays, so deploying the instance again brings it back as it was.

## Validation errors

Errors name the place in the config, e.g. `services.ingress[site].routes[api]: target "backnd" is not in app_services`. Exit code 2 means the files are wrong (nothing was changed), 3 means the server didn't allow it (no matching shared app, a domain used by another instance).
