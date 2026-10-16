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

Without `-f`: `server deploy` loads `compost-server.yaml` and `.compost-server.secrets.yaml` (if present); `app deploy` loads `compost.yaml`. The merge result plus defaults is the **effective configuration**, which is validated, shown by `--dry-run` and stored on the server without secrets.

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
services:
  ingress: [ … ]                       # see Applications
  database: [ … ]
  oidc: [ … ]
app_services:
  - name: backend                      # name used by links
    service: backend-webapp            # service in the compose file
    environment:                       # fixed variables
      APP_MODE: production
backup:
  volumes: [uploads]                   # named volumes saved by `app backup`
```

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

An overlay stored with `secret deploy` also needs `name`. See [Secrets](Secrets.md).

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
backup:
  target: local                        # local | server | s3 (not implemented)
  server_path: backups                 # folder of the server target
```

Each type's keys: [Servers and shared apps](Servers-and-Shared-Apps.md#shared-app-types), and [External databases](Servers-and-Shared-Apps.md#external-databases) for `external`. At most one ingress per server.

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
      identity_file: ~/.ssh/id_ed25519 # optional; default: ssh config, agent, ~/.ssh/id_*
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
