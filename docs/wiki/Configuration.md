# Configuration

| File | Where | What |
|---|---|---|
| [`compost.yaml`](#compostyaml) | App repository | How the app is deployed. Commit it. |
| [Overlay](#overlays), e.g. `.compost-staging.yaml` | Anywhere, or stored on the server | Values of one instance, incl. secrets |
| [`compost-server.yaml`](#compost-serveryaml) | Infrastructure folder | The server's shared apps. Commit it. |
| `.compost-server.secrets.yaml` | Infrastructure folder | Shared apps' secrets ([Secrets](Secrets.md#shared-apps)) |
| [`servers.yaml`](#serversyaml), [`config.yaml`](#preferences) | `~/.compost` | Your servers and preferences |

Names (apps, prefixes, shared apps, link entries) use lowercase letters, digits and `-`, start with a letter and have at most 31 characters. App names starting with `compost-` are reserved.

## How files are merged

`-f a.yaml -f b.yaml` or `-f a.yaml,b.yaml` deep-merges the files in order:

1. Maps merge key by key.
2. Lists whose items all have a `name` merge **by name**: matching items merge, new ones are appended.
3. Other lists and values from later files replace earlier ones.

So an overlay only repeats the names and the keys it changes:

```yaml
services:
  ingress:
    - name: site                      # merged into the `site` entry of compost.yaml
      domains: [staging.example.com]
```

Without `-f`: `server deploy` loads `compost-server.yaml` and `.compost-server.secrets.yaml` (if present); `app deploy` loads `compost.yaml`. Overlays stored on the server ([Secrets](Secrets.md)) are merged right after the first file. The merge result plus defaults is the **effective configuration**, which is validated, shown by `--dry-run` and stored on the server without secrets.

## compost.yaml

```yaml
name: myapp                            # required
deployment:
  type: docker-compose                 # the only type
  source_dir: .                        # folder uploaded and holding the compose file; relative to compost.yaml
  compose_file: docker-compose.yaml    # default; relative to source_dir
  compose_overrides: []                # more compose files applied after it; relative to compost.yaml
  build: false                         # true: `docker compose build` on the server; false: `pull`
  attach_sources: false                # upload the whole folder; forced true by build: true
  releases: 5                          # releases kept on the server for `app rollback`; 0 keeps none
  backup: predeploy                    # backup profile run before `app deploy --apply` changes a deployed instance
services:
  ingress: [ … ]                       # see Applications
  database: [ … ]
  oidc: [ … ]
  monitoring: [ … ]
app_services:
  - name: backend                      # name used by links
    service: backend-webapp            # service in the compose file
    environment:                       # fixed variables
      APP_MODE: production
backup:
  volumes: [uploads]                   # named volumes saved by `app backup`
  stop: [worker]                       # services stopped while volumes are copied; `true` for all
  before_backup: [ … ]                 # steps; see below
  after_backup: [ … ]
  before_restore: [ … ]
  after_restore: [ … ]
  target: nas                          # where this app's backups go
  profiles:                            # changes to the server's profiles, `false` to opt out, or the app's own
    daily: false
    anonymized: {include: [volumes], volumes: [anon-dumps], before_backup: [ … ]}
```

### `backup`

| Key | Default | Meaning |
|---|---|---|
| `volumes` | `[]` | Top-level `volumes:` keys of the compose file that `app backup` saves |
| `stop` | none | Compose services stopped while the volumes are copied, or `true` for all; `--stop` stops all |
| `before_backup`, `after_backup` | `[]` | Steps run before anything is saved and after it is saved |
| `before_restore`, `after_restore` | `[]` | Steps run before data is loaded into a deployed instance and after it is loaded |
| `target` | the server's `backup.target` | Target of this app's backups: `local`, `server` or a name from the server's `backup.targets`. A profile's target wins over it |
| `profiles` | `{}` | Per profile: settings that override the server's profile (`include`, `links`, `format`, `target`, `keep`, `schedule` or `schedule: off`, and `volumes`, `stop`, `before_backup`, `after_backup` in place of the keys above), or `false` to opt out. A profile the server doesn't have is the app's own. See [Profiles](Backup-and-Restore.md#profiles) |

Each step is `{exec: <service>, command: …}`, run in the service's running container, or `{run: <service>, command: …}`, a one-off container (`command` optional) that must exit with 0. A string `command` runs with `sh -c`; a list is the command and its arguments. See [Backup and restore](Backup-and-Restore.md#steps-around-a-backup-or-restore).

### `services.ingress[]`

| Key | Default | Meaning |
|---|---|---|
| `name` | required | Entry name |
| `domains` | required unless `optional` | Usually in the overlay |
| `optional` | `false` | Skip the entry when no domains are given |
| `routes[].name` | required | |
| `routes[].target` | – | `app_services` name; with `port` |
| `routes[].port` | – | Container port |
| `routes[].path` | `/` | |
| `routes[].strip_prefix` | `false` | Forward `/ui/x` as `/x` |
| `routes[].link` | – | `<kind>/<entry>` of another link instead of `target`/`port`, e.g. `oidc/login` |
| `security.bans` | server default | Block addresses CrowdSec banned. Needs a `caddysecurebuild` ingress, like every `security` key |
| `security.appsec` | server default | Check requests with CrowdSec AppSec |
| `security.rate_limits` | server default | Rate-limit zones of the server, by name; `[]` for none |
| `security.own_rate_limits[]` | `[]` | Zones of this entry: `name`, `paths`, `methods`, `events`, `window` (see [caddysecurebuild](Servers-and-Shared-Apps.md#caddysecurebuild-ingress)) |

### `services.database[]`

| Key | Default | Meaning |
|---|---|---|
| `name` | required | Entry name |
| `type` | required | `postgres` |
| `version` | `"*"` | Requirement on the major version, e.g. `">=16"` |
| `instance` | – | Pin a shared app by name; required for an external database |
| `targets` | `[]` | `app_services` that get the variables and the network |
| `environment` | `{}` | Variable → `db_host`, `db_port`, `db_name`, `db_user`, `db_password`, `db_url` |
| `db_name`, `db_user` | `<app>_<prefix>_<entry>` | Overlay. `^[a-z_][a-z0-9_]{0,62}$` |
| `db_password` | generated | Overlay, secret. Required for an external database without root credentials |

### `services.oidc[]`

| Key | Default | Meaning |
|---|---|---|
| `name`, `type` | required | `type: keycloak` |
| `version`, `instance` | `"*"`, – | As for databases |
| `realm` | `<app>-<prefix>` | Created if missing |
| `client_id` | `<app>-<prefix>-<entry>` | |
| `public_client` | `false` | No client secret |
| `public_url` | – | The realm's own domain; needs an ingress route with `link: oidc/<entry>` |
| `redirect_uris` | required | |
| `web_origins` | `[]` | |
| `client_secret` | generated | Secret |
| `users[]` | `[]` | `username`, `email`, `first_name`, `last_name`, `password`. Secret |
| `targets`, `environment` | | Values: `oidc_issuer`, `oidc_internal_url`, `oidc_realm`, `oidc_client_id`, `oidc_client_secret` |

### `services.monitoring[]`

| Key | Default | Meaning |
|---|---|---|
| `name` | required | Entry name |
| `instance` | – | Pin a monitoring app by name; required when the server has several |
| `optional` | `false` | Skip the entry when the server runs no monitoring app |
| `targets` | `[]` | `app_services` whose output is collected and that get the variables and the network |
| `logs` | `true` | Collect the output of `targets`. Fails for a service with its own `logging` |
| `metrics[]` | `[]` | Prometheus endpoints: `target` (`app_services` name), `port`, `path` (default `/metrics`) |
| `environment` | `{}` | Variable → `otlp_endpoint`, `otlp_grpc_endpoint`, `otlp_protocol`, `service_name`, `resource_attributes` |

At least one of `targets` and `metrics` is required.

Details and examples: [Applications](Applications.md).

## Overlays

Same schema as `compost.yaml`, plus `prefix` (default `main`). Typically:

```yaml
prefix: staging
services:
  ingress:
    - name: site
      domains: [staging.example.com]
  database:
    - name: main-db
      db_password: …
app_services:
  - name: backend
    environment:
      APP_MODE: staging
```

An overlay stored with `secret deploy` also needs `name` (`app deploy --secret` adds it). See [Secrets](Secrets.md#overlays-stored-on-the-server).

## compost-server.yaml

```yaml
shared:
  ingress:
    - {name: caddy-main, type: caddy, version: 2, configuration: {…}}
  database:
    - {name: postgres18, type: postgres, version: 18, image: postgres:18-alpine}
    - {name: shop-db, type: postgres, version: 16, external: {host: db, network: shop_default}}
  oidc:
    - {name: keycloak, type: keycloak, version: 26, configuration: {url: https://auth.example.com}}
  monitoring:
    - {name: monitoring, type: openobserve, version: 1, configuration: {url: https://logs.example.com}}
  communication:
    - {name: ops-chat, type: matrix, configuration: {homeserver: https://matrix.example.org, room: "!abc:example.org"}}
backup:
  target: local                        # local | server | a name from targets
  server_path: backups                 # folder of the server target
  targets:
    nas: {type: server, path: /mnt/nas/compost}   # another folder on the server
    offsite: {type: s3, bucket: acme-backups, path: prod, endpoint: https://s3.example.com}   # credentials in the secrets file
  profiles:
    daily: {include: [links], keep: 7, target: nas, schedule: "0 3 * * *"}
    predeploy: {keep: 3, server: false}
```

`targets` keys: [S3 targets](Backup-and-Restore.md#s3-targets). `profiles` keys: `include`, `links`, `format`, `target`, `keep`, `schedule` (a cron expression: [Scheduled backups](Backup-and-Restore.md#scheduled-backups)) and `server` (whether `backup run` and the schedule also back up the shared apps, default `true`). See [Profiles](Backup-and-Restore.md#profiles).

Each type's keys: [Servers and shared apps](Servers-and-Shared-Apps.md#shared-app-types) (communication types: [Notifications](Notifications.md#channels)), and [External databases](Servers-and-Shared-Apps.md#external-databases) for `external`. At most one ingress per server: `caddy`, or `caddysecurebuild` for Caddy with CrowdSec bans, AppSec and rate limits (its `configuration.security`: [caddysecurebuild](Servers-and-Shared-Apps.md#caddysecurebuild-ingress)).

Keys every shared app has:

| Key | Default | Meaning |
|---|---|---|
| `name`, `type` | required | |
| `version` | per type | Major version |
| `image` | per type | |
| `configuration` | `{}` | The type's keys |
| `external` | – | A server compost doesn't run; databases only |
| `monitoring` | – | Send its output and metrics to a monitoring app: `instance`, `logs` (default `true`), `metrics` (default `true`). See [Monitoring shared apps](Servers-and-Shared-Apps.md#monitoring-shared-apps) |

## servers.yaml

Written by `server define`; you rarely edit it.

```yaml
servers:
  - name: prod
    connector:
      type: ssh                        # ssh | local
      address: 203.0.113.10
      port: 22                         # optional
      user: compostuser                # default
      identity_file: ~/.ssh/id_ed25519 # optional; default: ssh config, agent, ~/.ssh/id_*; --ssh-key-file wins
    server_home: ~/.compost-server     # default
    deploy:
      apply: false                     # optional, see below
  - name: dev
    connector:
      type: local
```

## Preferences

```sh
compost config set deploy.apply true [--for-server prod]
compost config unset deploy.apply [--for-server prod]
compost config show
```

| Key | Default | Meaning |
|---|---|---|
| `deploy.apply` | `false` | `server deploy` and `app deploy` start services without `--apply` |

`--apply`/`--no-apply` win over the server's value, which wins over the global one in `~/.compost/config.yaml`.

## Environment variables

| Variable | Meaning |
|---|---|
| `COMPOST_HOME` | compost's local folder, default `~/.compost` |
| `COMPOST_SERVER` | Target server, when `-s` isn't given (wins over the context) |
| `COMPOST_SSH_KEY_FILE` | SSH private key, when `--ssh-key-file` isn't given (wins over the server's `identity_file`) |
| `COMPOST_SERVER_BINARY` | compost binary that deploys install on the server for [scheduled backups](Backup-and-Restore.md#scheduled-backups), instead of this one or a download |
