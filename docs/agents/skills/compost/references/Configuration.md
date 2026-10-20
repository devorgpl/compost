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

Without `-f`: `server deploy` loads `compost-server.yaml` and `.compost-server.secrets.yaml` (if present); `app deploy` loads `compost.yaml`; `dev` commands load `compost.yaml` and `.compost-local.yaml` (if present). Overlays stored on the server ([Secrets](Secrets.md)) are merged right after the first file. The merge result plus defaults is the **effective configuration**, which is validated, shown by `--dry-run` and stored on the server without secrets.

## compost.yaml

```yaml
name: myapp                            # required
deployment:
  type: docker-compose                 # the only type
  source_dir: .                        # folder uploaded and holding the compose file; relative to compost.yaml
  compose_file: docker-compose.yaml    # default; relative to source_dir
  compose_overrides: []                # more compose files applied after it; relative to compost.yaml
  build: false                         # false: `pull`; true (or server): `docker compose build` on the server; local (experimental): build on this machine and send the images
  attach_sources: false                # upload the whole folder; forced true by build: true
  releases: 5                          # releases kept on the server for `app rollback`; 0 keeps none
  backup: predeploy                    # backup profile run before `app deploy --apply` changes a deployed instance
services:
  ingress: [ … ]                       # see Applications
  database: [ … ]
  oidc: [ … ]
  monitoring: [ … ]
  mail: [ … ]
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
commands:                              # run by name with `app cmd`
  migrate: {run: backend, command: [python, manage.py, migrate], confirm: true}
```

`commands`: [Commands in apps](Admin-Commands.md#commands-in-apps).

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

In the `environment` of every link below, a variable gets one of the link's values, e.g. `db_url`, or a template with values in braces, e.g. `"{db_host}:{db_port}"` ([Combining values](Applications.md#combining-values)).

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
| `routes[].link` | – | `<kind>/<entry>` of another link instead of `target`/`port`, e.g. `oidc/login`. Skipped when that link is optional and skipped |
| `routes[].exact` | `false` | Match only `path`, not the paths below it |
| `routes[].redirect` | – | `{to, status}` instead of `target`/`port`: redirect to a path or URL; `status` 301, 302 (default), 303, 307 or 308 |
| `routes[].header_up` | `{}` | Request headers set for the service, e.g. `{X-Real-IP: "{remote_host}"}` |
| `max_body_size` | – | Largest request body, e.g. `20MB`; larger requests get 413 |
| `headers.set` | `{}` | Response headers set on every route |
| `headers.remove` | `[]` | Response headers removed, e.g. `[Server]` |
| `security.bans` | server default | Block addresses CrowdSec banned. Needs a `caddysecurebuild` ingress, like every `security` key |
| `security.appsec` | server default | Check requests with CrowdSec AppSec |
| `security.rate_limits` | server default | Rate-limit zones of the server, by name; `[]` for none |
| `security.own_rate_limits[]` | `[]` | Zones of this entry: `name`, `paths`, `methods`, `events`, `window` (see [caddysecurebuild](Servers-and-Shared-Apps.md#caddysecurebuild-ingress)) |
| `security.ignore[]` | `[]` | Requests of this entry CrowdSec never counts towards a ban: rules of `paths`, `methods`, `statuses`, or presets `matrix`, `registry`, `s3` (see [False positives](Servers-and-Shared-Apps.md#false-positives)) |

### `services.database[]`

| Key | Default | Meaning |
|---|---|---|
| `name` | required | Entry name |
| `type` | required | `postgres`, `mariadb` or `mysql` |
| `version` | `"*"` | Requirement on the major version, e.g. `">=16"` |
| `instance` | – | Pin a shared app by name; required for an external database |
| `targets` | `[]` | `app_services` that get the variables and the network |
| `environment` | `{}` | Variable → `db_host`, `db_port`, `db_name`, `db_user`, `db_password`, `db_url` |
| `db_name`, `db_user` | `<app>_<prefix>_<entry>` | Overlay. `^[a-z_][a-z0-9_]{0,62}$`. Links may share a user (see [One user for several databases](Applications.md#database-links)) |
| `db_password` | generated | Overlay, secret. Required for an external database without root credentials. Links sharing a user share its password |

### `services.keyvalue[]`

| Key | Default | Meaning |
|---|---|---|
| `name` | required | Entry name |
| `type` | required | `valkey` or `redis` |
| `version` | `"*"` | Requirement on the major version |
| `instance` | – | Pin a shared app by name |
| `targets` | required | `app_services` that get the variables and the network |
| `environment` | `{}` | Variable → `kv_host`, `kv_port`, `kv_user`, `kv_password`, `kv_db`, `kv_prefix`, `kv_url` |

### `services.storage[]`

| Key | Default | Meaning |
|---|---|---|
| `name` | required | Entry name; Garage's bucket is `<app>-<prefix>-<name>` |
| `instance` | – | Pin a storage app by name; required for an external one |
| `targets` | required | `app_services` that get the variables and the network |
| `environment` | `{}` | Variable → `s3_endpoint`, `s3_public_endpoint`, `s3_region`, `s3_bucket`, `s3_access_key_id`, `s3_secret_access_key` |
| `bucket`, `access_key_id`, `secret_access_key` | – | External storage only: the existing bucket and key. Overlay; the keys are secrets |

### `services.oidc[]`

| Key | Default | Meaning |
|---|---|---|
| `name`, `type` | required | `type: keycloak` |
| `version`, `instance` | `"*"`, – | As for databases |
| `optional` | `false` | Skip the entry (and ingress routes to it) when the server runs no provider of its `type` |
| `realm` | `<app>-<prefix>` | Created if missing |
| `client_id` | `<app>-<prefix>-<entry>` | |
| `public_client` | `false` | No client secret |
| `public_url` | – | The realm's own domain, or a path of one (e.g. `https://app.example.com/auth`); needs an ingress route with `link: oidc/<entry>` |
| `redirect_uris` | required | With `optional`, only where the link is deployed |
| `web_origins` | `[]` | |
| `post_logout_redirect_uris` | the `redirect_uris` | |
| `pkce` | – | `S256` or `plain` |
| `direct_access_grants` | `false` | Resource owner password grant |
| `default_scopes` | unchanged | The client's default scopes |
| `realm_settings` | `{}` | `registration`, `reset_password`, `login_with_email`, `verify_email`, `brute_force_protection` (`true`/`false`), `ssl_required` (`none`, `external`, `all`), `sso_session_idle_timeout`, `sso_session_max_lifespan`, `access_token_lifespan` (durations like `30m`, `3h`); unset keys are not changed |
| `roles` | `[]` | Realm roles, created if missing |
| `client_scopes[]` | `[]` | `name`, `include_in_token_scope` (default `true`), `mappers[]`: `name`, `type: user_attribute`, `attribute`, `claim`, `multivalued` (default `false`), `id_token`, `access_token`, `userinfo` (default `true`). Set on every deploy and added to the client's default scopes |
| `client_secret` | generated | Secret |
| `users[]` | `[]` | `username`, `email`, `email_verified` (default `true` with `email`), `first_name`, `last_name`, `id` (when created), `roles` (from `roles`), `password`, `temporary` (password set only when created, changed at first login), `attributes` (set on every deploy). Secret |
| `targets`, `environment` | | Values: `oidc_issuer`, `oidc_internal_url`, `oidc_base_url`, `oidc_internal_base_url`, `oidc_origin`, `oidc_jwks_uri`, `oidc_realm`, `oidc_client_id`, `oidc_client_secret` |

### `services.workflow[]`

| Key | Default | Meaning |
|---|---|---|
| `name`, `type` | required | `type: temporal` |
| `version`, `instance` | `"*"`, – | As for databases |
| `namespace` | `<app>-<prefix>` | Created if missing. `^[a-z0-9][a-z0-9._-]{0,99}$` |
| `retention_days` | `3` | How long closed workflows are kept |
| `targets` | required | `app_services` that get the variables and the network |
| `environment` | `{}` | Variable → `wf_host`, `wf_port`, `wf_address`, `wf_namespace` |

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

### `services.mail[]`

| Key | Default | Meaning |
|---|---|---|
| `name` | required | Entry name |
| `instance` | – | Pin a mail app by name; required when the server has several |
| `optional` | `false` | Skip the entry when the server runs no mail app |
| `targets` | required | `app_services` that get the variables and the network |
| `environment` | `{}` | Variable → `smtp_host`, `smtp_port`, `smtp_url`, `ses_endpoint`, `aws_endpoint` (with `moto: true`), `aws_region`, `aws_access_key_id`, `aws_secret_access_key`, `inbox_url` |

Details and examples: [Applications](Applications.md).

## Overlays

Same schema as `compost.yaml`, plus `prefix` (default `main`; `local` for [`dev` commands](Local-Development.md#starting-an-app-dev-up)). Typically:

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
    - {name: mariadb11, type: mariadb, version: 11}
  keyvalue:
    - {name: valkey, type: valkey, version: 9}
  storage:
    - {name: storage, type: garage, configuration: {url: https://files.example.com}}
  oidc:
    - {name: keycloak, type: keycloak, version: 26, configuration: {url: https://auth.example.com}}
  monitoring:
    - {name: monitoring, type: openobserve, version: 1, configuration: {url: https://logs.example.com}}
  mail:                                # development and test servers
    - {name: mail, type: mailpit, version: 1, configuration: {url: https://mail.test.example.com}}
  workflow:
    - {name: temporal, type: temporal, version: 1, configuration: {ui: {url: https://temporal.example.com, oidc: {}}}}
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

```yaml
notifications:
  send_to: [ops-chat, {to: ops-mail, events: [backup, disk]}]   # communication apps; events: backup, health, disk, bans, bans_daily
  disk_free_below: 10                                           # percent
  daily_at: "08:00"                                             # when bans_daily is sent
```

See [Notifications](Notifications.md#what-is-sent).

```yaml
commands:                              # run by name with `server cmd`
  unban:
    description: Lift a CrowdSec ban
    shared_app: caddy-main
    service: crowdsec
    args: {ip: {type: ip}}
    command: [cscli, decisions, delete, --ip, "{ip}"]
    confirm: true
```

See [Named commands](Admin-Commands.md#keys) for every key, the argument types and how secrets are passed.

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
| `monitoring` | – | Send its output and metrics to a monitoring app: `instance`, `logs` (default `true`), `metrics` (default `true`), `traces` (default `false`, ingress only). See [Monitoring shared apps](Servers-and-Shared-Apps.md#monitoring-shared-apps) |

`configuration.host_port` (postgres, mariadb, mysql, valkey, redis, garage) also publishes the shared app on 127.0.0.1 of the server, e.g. for an app run outside Docker ([Local development](Local-Development.md#running-the-app-outside-docker)). Each port is used by one shared app at most; external shared apps can't have one.

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
compost config set dev.server laptop
compost config show
```

| Key | Default | Meaning |
|---|---|---|
| `deploy.apply` | `false` | `server deploy` and `app deploy` start services without `--apply` |
| `dev.server` | – | The local server `dev` commands use; global only. See [Local development](Local-Development.md#which-machine) |

`--apply`/`--no-apply` win over the server's value, which wins over the global one in `~/.compost/config.yaml`.

## Environment variables

| Variable | Meaning |
|---|---|
| `COMPOST_HOME` | compost's local folder, default `~/.compost` |
| `COMPOST_SERVER` | Target server, when `-s` isn't given (wins over the context); for `dev` commands it must be a local server |
| `COMPOST_SSH_KEY_FILE` | SSH private key, when `--ssh-key-file` isn't given (wins over the server's `identity_file`) |
| `COMPOST_SERVER_BINARY` | compost binary that deploys install on the server for [scheduled backups](Backup-and-Restore.md#scheduled-backups), instead of this one or a download |
