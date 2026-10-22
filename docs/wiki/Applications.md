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

## With and without compost

A project often runs its own database, cache or mail catcher with `docker compose up`, which compost replaces with shared apps. compost starts every service of the compose files (`docker compose up` without service names): leaving a service out of `app_services` doesn't keep it from starting, it only keeps links from targeting it. Two layouts let the same project run both ways.

### Extra services in an override file

Without `-f`, Docker Compose also reads `docker-compose.override.yaml` (`compose.override.yaml` next to a `compose.yaml`). compost always names its files with `-f` (`compose_file`, then `compose_overrides`), so it never reads that file:

```
myapp/
├── docker-compose.yaml            the app's own services: compost deploys these
├── docker-compose.override.yaml   database, cache, … for plain `docker compose up`
└── compost.yaml
```

```yaml
# docker-compose.yaml
services:
  backend:
    build: .
```

```yaml
# docker-compose.override.yaml
services:
  backend:
    environment:
      DATABASE_URL: postgres://myapp:myapp@db:5432/myapp
    depends_on:
      db: {condition: service_healthy}
  db:
    image: postgres:18
    environment: {POSTGRES_USER: myapp, POSTGRES_PASSWORD: myapp}
    healthcheck: {test: pg_isready -U myapp, interval: 2s}
```

The database link in `compost.yaml` sets the same `DATABASE_URL`, so the app reads it the same way under both. Keep everything that names the extra services, like `depends_on`, in the override file. With `build: true` the file is uploaded with the folder but not used.

### Extra services in a profile

To keep one file, give the extra services a [Compose profile](https://docs.docker.com/compose/how-tos/profiles/). compost turns on no profiles, so they don't start (`--dry-run` shows them as `not started`); `docker compose --profile standalone up` starts them:

```yaml
# docker-compose.yaml
services:
  backend:
    build: .
    environment:
      DATABASE_URL: postgres://myapp:myapp@db:5432/myapp   # compost's override sets its own
    depends_on:
      db: {condition: service_healthy, required: false}
  db:
    image: postgres:18
    profiles: [standalone]
    environment: {POSTGRES_USER: myapp, POSTGRES_PASSWORD: myapp}
    healthcheck: {test: pg_isready -U myapp, interval: 2s}
```

- A `depends_on` on a service in a profile needs `required: false` (Compose 2.20 or newer); without it Compose refuses the project when the profile is off.
- The variables a link sets replace those of the compose file, since compost's override comes last.
- `compost app generate` adds an `app_services` entry for every service, including those in profiles: remove them, and don't make them targets of links.
- Moving a service that already runs into a profile doesn't stop it: Compose leaves its container running.
- `COMPOSE_PROFILES` in a `.env` file in the project folder turns profiles on for compost too, as Compose reads that file on the server. Set it in your shell instead.

Both layouts work with [`compost dev up`](Local-Development.md), which leaves the extra services off and uses shared apps. For an app you can't edit, turn its extra services off with `compose_overrides` instead ([Sources in another folder](#sources-in-another-folder)).

## Building images

Services with a `build` section in the compose file run images that have to be built. `deployment.build` says where:

| `build` | Images of services with `build` | Uploaded to the server |
|---|---|---|
| `false` (default) | not built: every image is pulled | the compose files |
| `true` or `server` | built on the server with `docker compose build` | the whole folder (`source_dir`) |
| `local` (experimental) | built on this machine and sent to the server, without a registry | the compose files |

**`build: local` is experimental:** it works, but has the [known limitations](#known-limitations-of-build-local) below, and its behavior may change in a later version without notice. It suits small servers, which then need neither the CPU and memory for builds nor the sources. With `--apply`, a deploy:

1. builds the images here with `docker compose build`, from the same compose files and folder the server would use, for the server's platform (e.g. `linux/amd64`). This happens before anything on the server changes, so a failed build leaves the deployed instance as it was;
2. compares each image with the one of the same name on the server by its configuration (creation time, layers, settings) and sends those that differ: `docker save`, compressed with gzip, into `docker load` over the server connection. An unchanged image is not sent again;
3. starts the services as with `build: true`.

It needs Docker with compose v2 on this machine. An image is named as on the server: the service's `image`, or `<instance>-<service>`.

### Known limitations of `build: local`

- **Whole images are sent.** A changed image goes over the connection in full, including base layers the server already has. Over a slow connection, large images may deploy faster with `build: true` or from a registry.
- **Variables come from this machine.** Variables in the compose file (`${TAG}`) are filled in from this machine's environment and the folder's `.env` while building, and on the server when the services start; keep them the same on both sides.
- **Other CPUs need emulation.** Building for another CPU (an arm64 Mac for an amd64 server) needs emulation, which is slow: Docker Desktop has it; on Linux install it with `docker run --privileged --rm tonistiigi/binfmt --install all`. The platform is the server's operating system and CPU as its Docker reports them (`linux/amd64`, `linux/arm64`), without a variant such as `arm/v7`.
- **Rollback can't rebuild.** If the images pinned for a release were removed from the server, `app rollback` can't build them again: the release has only the compose files ([Missing images](#releases-and-rollback)).
- **The dry run doesn't build.** `--dry-run` can't tell whether an image changes, so it shows such services as `built locally; sent and recreated if its image changes`.
- **Images pile up.** Every build leaves its image on this machine and, once replaced, an untagged one on the server. `server prune` removes those on the server; `docker image prune` those here.
- **Only Docker** has been tried on this machine, not Podman or other compose implementations.

## Sources in another folder

`compost.yaml` doesn't have to live in the app's repository. In a separate deployment repository, `source_dir` points to the app's folder: that folder is uploaded (with `build: true` or `attach_sources: true`), built from (with `build: local`) and holds the compose file.

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

An entry can also limit request bodies, change response headers, redirect, and set headers for a service:

```yaml
services:
  ingress:
    - name: site
      max_body_size: 20MB        # larger requests get 413; KB, MB, GB, KiB, MiB, GiB or bytes
      headers:                   # response headers of every route
        remove: [Server]
        set: {X-Frame-Options: DENY}
      routes:
        - name: home
          path: /
          exact: true            # only `/` itself, not the paths below it
          redirect: {to: /web/, status: 302}   # status default 302; also 301, 303, 307, 308
        - name: api
          target: api
          port: 5000
          path: /backend
          strip_prefix: true
          header_up: {X-Real-IP: "{remote_host}"}   # request headers for the service
        - {name: web, target: web, port: 80, path: /web}
```

A route has either `target` and `port`, `link`, or `redirect`. `redirect.to` is a path or an `http(s)://` URL; Caddy placeholders work there and in header values, e.g. `https://example.com{uri}`. A redirect on `/` without `exact` catches every path no other route matches. Of two routes with the same path, the `exact` one wins on that path. Header values can't contain quotes, backslashes or line breaks.

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
        ignore:                  # requests CrowdSec never counts towards a ban
          - {paths: [/api/v1/items/*], methods: [GET], statuses: [404]}
          - matrix               # a preset: matrix, registry or s3
```

`app deploy` fails when a named zone isn't defined on the server, when the server's ingress is plain `caddy`, or with `bans: false` when the server drops banned connections ([`ban_action: drop`](Servers-and-Shared-Apps.md#dropping-banned-connections)). A blocked request gets 403, a rate-limited one 429. `ignore` is for apps whose own clients get many 4xx answers, which CrowdSec would take for a scan and ban, e.g. Matrix clients, registries and S3 storage, which have presets; see [False positives](Servers-and-Shared-Apps.md#false-positives).

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

To load a dump into the link's database, or dump it, use `compost app cmd <instance> <entry>.import` and `.export` ([Database links](Admin-Commands.md#database-links)).

In the overlay you can set `db_name`, `db_user` and `db_password`. The defaults come from `<app>_<prefix>_<entry>`, so renaming the entry gives a new, empty database unless you set them. MariaDB and MySQL user names are at most 32 characters: a longer default is cut and ends in a short hash.

**One user for several databases.** Some software logs in to several databases with one user, e.g. Temporal's `auto-setup` image with its main and visibility databases. Give each link its own database and the same `db_user`:

```yaml
    - {name: temporal, type: postgres, targets: [temporal], db_user: temporal, environment: {DB_URL: db_url}}
    - {name: visibility, type: postgres, targets: [temporal], db_user: temporal, environment: {VISIBILITY_DB_URL: db_url}}
```

A user has one password. Leave `db_password` out and the links get the same generated one, or set the same value on all of them; `app deploy` refuses links of the user with different passwords, also in other instances. `--purge database` drops each link's database, and the user with the last link on that database server that uses it. A database two links name with the same `db_name` is kept the same way.

**Existing databases.** To link to a database compost doesn't run, register it as an [external database](Servers-and-Shared-Apps.md#external-databases) and pin it with `instance`. Unless compost has root credentials for it, give the existing `db_name`, `db_user` and `db_password`. compost then only passes them to the services:

```yaml
    - name: main
      type: postgres
      instance: shop-db
      db_name: shop
      db_user: shop
      db_password: …           # in the overlay
```

### Combining values

Every link's `environment` also takes templates, for apps that want the values in another shape. A value with braces is a template: `{name}` is a value of that link, and `{{` and `}}` are literal braces:

```yaml
      environment:
        POSTGRES_URL: "{db_host}:{db_port}"                       # postgres18:5432
        JDBC_URL: "jdbc:postgresql://{db_host}:{db_port}/{db_name}"
        DSN: "{db_url}?sslmode=require&application_name=shop"
        PGDATABASE: db_name                                       # a plain value, as before
```

A template names values of its own link only. An unknown name fails validation (exit 2) with the path of the entry. With [`compost dev env`](Local-Development.md#running-the-app-outside-docker), templates use the addresses on 127.0.0.1 like the plain values.

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

An OIDC link creates a realm, a client and optionally roles and users in a Keycloak, and passes the client settings to services:

```yaml
services:
  oidc:
    - name: login
      type: keycloak
      version: ">=26"
      optional: false                  # true: skip (with a notice) when the server runs no keycloak
      realm: myapp                     # default <app>-<prefix>; created if missing
      client_id: web                   # default <app>-<prefix>-<entry>
      public_client: false             # true: no client secret, e.g. for a single-page app
      redirect_uris: ["https://staging.example.com/*"]   # required (if optional: when deployed), usually in the overlay
      web_origins: []
      client_secret: …                 # secret; generated on the first deploy, then kept
      users:                           # secret; created if missing, passwords set on every deploy (unless temporary)
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
| `oidc_base_url` | `https://auth.example.com`, for libraries that take a base URL and a realm (e.g. keycloak-js) |
| `oidc_internal_base_url` | `http://keycloak:8080` |
| `oidc_origin` | `https://auth.example.com`, e.g. for a CSP `connect-src` or `frame-src` |
| `oidc_jwks_uri` | `http://keycloak:8080/realms/myapp/protocol/openid-connect/certs`, to check tokens without discovery |
| `oidc_realm` | `myapp` |
| `oidc_client_id` | `web` |
| `oidc_client_secret` | empty for public clients |

**Optional links.** When only some servers run Keycloak, e.g. development and test, while staging and production log in through another provider (such as AWS Cognito), mark the link `optional: true`. On a server with no provider of the link's `type`, the deploy skips it with a notice: its `environment` isn't set, so the services take those variables from `app_services` in the overlay, and ingress routes with `link: oidc/<entry>` are skipped too (an ingress entry with no other routes fails; make it `optional` and leave out its domains on those servers). A server that runs Keycloak of another version, or not the pinned `instance`, is still an error. `redirect_uris` are needed only where the link is deployed, so they can stay in the overlays of the servers with Keycloak:

```yaml
# compost.yaml
services:
  oidc:
    - name: login
      type: keycloak
      optional: true
      realm: myapp
      public_client: true
      targets: [api]
      environment:
        OIDC_ISSUER_URL: oidc_issuer
        OIDC_CLIENT_ID: oidc_client_id
```

```yaml
# .compost-prod.yaml: no Keycloak on this server
app_services:
  - name: api
    environment:
      OIDC_ISSUER_URL: https://cognito-idp.eu-central-1.amazonaws.com/eu-central-1_AbCdEf123
      OIDC_CLIENT_ID: 1example23456789
```

`compost dev up` adds Keycloak only for a link that isn't `optional`: to have it locally, set `optional: false` for the link in `.compost-local.yaml`, along with its `redirect_uris`.

**Client settings** are set on every deploy, so changes made in the admin console don't last:

```yaml
      pkce: S256                        # or plain; default: none
      post_logout_redirect_uris: ["https://staging.example.com/"]   # default: the redirect_uris
      direct_access_grants: false       # logins with a password sent by the client; default false
      default_scopes: [basic, web-origins, acr, profile, roles, email]   # default: unchanged
```

Without `post_logout_redirect_uris`, logout may redirect to any of the `redirect_uris` (`+` in Keycloak). `default_scopes` replaces the client's default scopes when set; a new client starts with the realm's defaults, which are the list above in Keycloak 26.

**Realm settings and roles.** `realm_settings` sets the keys it lists on every deploy and leaves the others as they are, so a setting changed in the admin console stays unless the link sets it. `roles` creates realm roles that are missing; compost never deletes them, except with the whole realm. Links that share a realm share these, so they should agree:

```yaml
      realm_settings:
        registration: true              # users register themselves
        reset_password: true            # "forgot password" on the login page
        login_with_email: true
        verify_email: false
        brute_force_protection: true
        ssl_required: external          # none, external or all
        sso_session_idle_timeout: 3h    # durations: 30m, 3h, 1d
        sso_session_max_lifespan: 10h
        access_token_lifespan: 5m
      roles: [platform_admin, tenant_admin, user]
```

**Custom claims.** `client_scopes` creates client scopes in the realm whose mappers put user attributes into tokens, and adds them to the client's default scopes (to the list in `default_scopes` when it is set, else next to the scopes the client has):

```yaml
      client_scopes:
        - name: myapp
          include_in_token_scope: true  # default true: `myapp` appears in the token's `scope`
          mappers:
            - name: company
              type: user_attribute      # the only type so far
              attribute: company        # the user attribute
              claim: "custom:company"   # dots nest the claim: org.company
              multivalued: false        # true: all values, as a list
              id_token: true            # these three default to true
              access_token: true
              userinfo: true
```

A scope's settings and mappers are set on every deploy: mappers it doesn't list are deleted. Names of the scopes Keycloak creates itself (`profile`, `email`, `roles`, …) are refused. Like roles, scopes stay when a link is removed, until the realm is deleted. Links that share a realm should agree on them.

**Users for bootstrap accounts.** A user's `roles` (from the link's `roles`) are added on every deploy, never removed. With `temporary: true`, the password is set only when compost creates the user, and must be changed at the first login, so later deploys keep the one the user chose. `id` gives the user a fixed ID when compost creates it, for an app that recognizes its first admin by ID:

```yaml
      users:
        - username: admin@example.com
          email: admin@example.com
          email_verified: true          # default true when email is set
          id: 11111111-1111-1111-1111-111111111111
          roles: [platform_admin, user]
          password: …                   # secret
          temporary: true
          attributes: {company: ACME, userid: 485, groups: [a, b]}   # set on every deploy
```

`attributes` are set on every deploy; other attributes of the user stay. Keycloak keeps attributes outside its user profile only when the realm allows them, so a link with attributes sets the realm's *unmanaged attributes* policy to *only administrators can view and edit* (`ADMIN_EDIT`), unless it is already that or *enabled*. Users then can't change them on their account page, so a claim taken from them can be trusted. With *enabled*, which compost leaves as it is, users can edit them.

User IDs are unique in the whole Keycloak, not per realm: two instances on one Keycloak (e.g. staging and production) can't create users with the same `id`, and the second deploy fails. If the user already exists with another ID, the deploy keeps it and warns.

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

**A realm under a path of the app's domain.** With a path in `public_url`, the realm is served there, next to the app:

```yaml
services:
  oidc:
    - name: login
      public_url: https://app.example.com/auth
  ingress:
    - name: site
      domains: [app.example.com]
      routes:
        - {name: web, target: web, port: 8080}
        - {name: auth, link: oidc/login}      # /auth/realms/<realm> and /auth/resources
```

The issuer is then `https://app.example.com/auth/realms/<realm>`, on the app's own origin: a single-page app can keep a strict CSP (`connect-src 'self'; frame-src 'self'`), Keycloak's cookies are first-party, and keycloak-js `check-sso` works without third-party cookies. The ingress removes `/auth` before passing requests to Keycloak. Other paths under `/auth` go to the site's other routes, here the app.

## Workflow links

A workflow link gives services a namespace of their own on a [Temporal](Servers-and-Shared-Apps.md#temporal-workflow) server, for workers and the code that starts workflows:

```yaml
services:
  workflow:
    - name: jobs
      type: temporal
      version: "*"             # requirement on the major version
      instance: temporal       # optional: this shared app, when several match
      namespace: billing       # default <app>-<prefix>; created if missing
      retention_days: 7        # how long closed workflows are kept; default 3
      targets: [worker, backend]
      environment:
        TEMPORAL_ADDRESS: wf_address
        TEMPORAL_NAMESPACE: wf_namespace
```

| Link value | Example |
|---|---|
| `wf_host` | `temporal` |
| `wf_port` | `7233` |
| `wf_address` | `temporal:7233` |
| `wf_namespace` | `myapp-staging` |

The namespace is created, or its retention updated, before the app's containers start. Links of several apps can share a namespace when they name it; they must agree on `retention_days`. Namespaces separate workflows but not access: Temporal has no login, so every app on the server's network can reach every namespace.

`--purge workflow` on `app remove` (or for a dropped link) deletes the namespace and its workflows, unless another link uses it; otherwise it stays. App backups save only the link record: workflows are in Temporal's databases, which a server backup holds.

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

With [request tracing](Servers-and-Shared-Apps.md#request-tracing) on the ingress, requests reach the app with a `traceparent` header: an SDK that reads it puts the app's spans in the trace Caddy started.

Services in `targets` and `metrics` join the monitoring network as `<instance>-<service>`. The collector picks up a metrics endpoint once the app runs, without a restart.

compost collects the output with Docker's `fluentd` log driver, set in its override file, so `docker logs` keeps working. A service that sets its own `logging` in the compose file can't be a target with `logs: true`: the deploy fails and says so.

## Mail links

A mail link points services at a [mail catcher](Servers-and-Shared-Apps.md#mailpit-mail) on development and test servers: they send by SMTP or through the AWS SES API, and the mail lands in its inbox. On production, where the app uses a real provider, the server has no mail app and an `optional` link is skipped:

```yaml
services:
  mail:
    - name: outbox
      optional: true                   # skip (with a notice) when the server runs no mail app
      instance: mail                   # optional: this shared app, when there are several
      region: eu-central-1             # aws_region of this link; default: the mail app's region
      targets: [api]
      environment:
        AWS_ENDPOINT_URL_SES: ses_endpoint
        AWS_ENDPOINT_URL_SESV2: ses_endpoint
        AWS_REGION: aws_region
        AWS_ACCESS_KEY_ID: aws_access_key_id
        AWS_SECRET_ACCESS_KEY: aws_secret_access_key
        SMTP_URL: smtp_url
```

| Link value | Example |
|---|---|
| `ses_endpoint` | `http://mail-ses:8005`: the SES API, v1 and v2 |
| `smtp_host`, `smtp_port` | `mail`, `1025` |
| `smtp_url` | `smtp://mail:1025` |
| `aws_endpoint` | `http://mail-aws:5000`: moto, for other AWS APIs. Needs `moto: true` on the mail app |
| `aws_region` | `eu-central-1`: the link's `region`, else the mail app's (default `us-east-1`). The mocks accept any region, so set the one production uses |
| `aws_access_key_id`, `aws_secret_access_key` | `test`: the mocks accept any |
| `inbox_url` | `https://mail.test.example.com`, empty without `url` |

AWS SDKs read `AWS_ENDPOINT_URL_<SERVICE>` for one service and `AWS_ENDPOINT_URL` for all, so the same code sends through real SES where these variables are unset. The services in `targets` join the mail app's network. Removing the link deletes only its record: the mail in the inbox belongs to all apps.

`compost dev up` adds a mail app for a link that isn't `optional`. To keep the link optional on servers and have it locally, set `optional: false` for it in `.compost-local.yaml`. When a link maps `aws_endpoint`, `dev up` sets `moto: true` on the mail app it uses, whether it adds it or it exists (`setting moto: true on mail mail for services.mail[aws]`). On other servers, set `moto: true` in their `compost-server.yaml`.

## Deploying

```sh
compost app deploy [--apply | --no-apply] [--pull | --no-pull] [--recreate | --no-recreate] [-f files] [--secret <file>] [--prefix <prefix>] [--purge <kind>] [--dry-run [--diff]]
                   [--backup <profile> | --no-backup]
```

`-f` defaults to `compost.yaml`. Pass an instance's overlay with `--secret .compost-staging.yaml`: it is stored on the server and merged right after the first file, now and on later deploys without it ([Secrets](Secrets.md)). Other overlays go in `-f compost.yaml,.compost-local.yaml`; they are not stored. `--prefix` overrides the prefix from the files (default: the [`prefix` preference](Configuration.md#preferences), else `main`). `--pull` and `--recreate` can also be on by default, with the `deploy.pull` and `deploy.recreate` preferences. On your own machine, [`compost dev up`](Local-Development.md) does such a deploy and also adds the shared apps the app's links need.

A deploy:

1. validates the config and checks the compose file's service names (exit 2 on errors);
2. checks the server and picks a shared app for each link (exit 3 if none matches, or a domain is taken); with `--apply` and `deployment.backup` (or `--backup <profile>`), backs up the deployed instance with that [profile](Backup-and-Restore.md#before-a-deploy), and stops if the backup fails;
3. uploads the sources: the whole folder with `build: true` or `attach_sources: true`, otherwise only the compose file. Left out: `.git/`, files given with `-f`, `.compost-*.yaml`, `.compost.*.yaml` and patterns in `.compostignore` (gitignore syntax);
4. writes the override file and link records. **Without `--apply` it stops here.**
5. with `--apply`: creates databases, users and realms; runs `docker compose build` (with `build: true`), sends the images built on this machine (with `build: local`, built before step 3; see [Building images](#building-images)) or runs `pull`; then `up -d --remove-orphans`; waits up to 60 s until every service runs (or is healthy) or has exited with code 0, and has run 5 seconds without restarting; removes dropped links; updates Caddy's routes; records a [release](#releases-and-rollback).

The result is shown as by `app state`. Running the same deploy again keeps data and passwords and doesn't recreate unchanged containers.

**Updating images and recreating containers.** With `build: false`, every deploy with `--apply` pulls, so a new image behind the same tag (`:latest`, `:2`) is picked up and its containers are recreated. Two options, both needing `--apply`, do more:

- `--pull`: with `build: true` or `local`, also pulls the images of services without `build` and builds with `--pull`, which fetches newer base images. With `build: false` it changes nothing.
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
- **services**: what `docker compose up` would do, judged from each service's definition in the compose files and the override: `recreated` (naming what changed; for variables only their names), `created`, `removed`, `built again` (`build: true` and changed sources), `built locally` (`build: local`, experimental: the dry run doesn't build, so it can't tell whether the image changes), `pulled` (every deploy with `build: false` pulls; a newer image recreates it), `not started` (in a [Compose profile](#extra-services-in-a-profile), which compost never turns on; a container that already runs keeps running) or `unchanged`. Compose makes the final decision on the server, so treat this as a close estimate.

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
- **Missing images.** If a pinned image was removed by hand, the rollback warns and pulls or builds on the server as a deploy would. Images built with `build: local` can't be built again: the release has only the compose files. Deploy that version from its sources instead.

`deployment.releases` (default `5`) is how many releases an instance keeps; older ones and their image tags are deleted after each deploy, and `0` keeps none. Each release holds a copy of the sources, so lower it for big folders uploaded with `build: true`. Releases are not part of [backups](Backup-and-Restore.md), and `app remove` deletes them with their image tags. [`server prune`](Servers-and-Shared-Apps.md#disk-space) keeps release images when it removes unused ones.

## Inspecting

```sh
compost app list                                 # every instance: status (deployed, candidate, failed; stopped and provisioned from `dev` commands), running, last update
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

`app logs` runs `docker compose logs` on the server with the instance's compose files, so you don't need to know its project or files. `--service` (repeatable or comma-separated) picks compose services; an unknown one exits 2 and lists the instance's services. `--tail` is the number of lines per container (default `100`, or `all`; the `logs.tail` [preference](Configuration.md#preferences) changes it, and `logs.timestamps` turns on `--timestamps`), `--since` a duration (`10m`) or time (`2026-10-17T12:00:00`). On a terminal the output keeps compose's colors, and Ctrl-C ends `--follow` normally (exit 0); piped, it is plain text, e.g. `compost app logs myapp-staging | grep ERROR`.

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

Commands you run more than once can go under `commands` in compost.yaml, with checked arguments and a confirmation, and run by name: `compost app cmd myapp-staging migrate`. See [Admin commands](Admin-Commands.md#commands-in-apps).

## Removing

```sh
compost app remove myapp-staging [--volumes] [--purge database,oidc] [--yes]
```

Removes the routes and link records, then `docker compose down`, the instance's folder (with its releases) and the image tags kept for rollback. Databases and realms stay unless their kind is in `--purge` (databases on an external server stay anyway, see [External databases](Servers-and-Shared-Apps.md#external-databases)); `--volumes` also removes the compose volumes, except external ones. It asks for confirmation unless `--yes`. If a link can't be removed (e.g. its database server is down), it stops there and the instance stays deployed; run it again once that is fixed, and it continues with the links that are left. A stored overlay ([Secrets](Secrets.md)) stays, so deploying the instance again brings it back as it was.

## Validation errors

Errors name the place in the config, e.g. `services.ingress[site].routes[api]: target "backnd" is not in app_services`. Exit code 2 means the files are wrong (nothing was changed), 3 means the server didn't allow it (no matching shared app, a domain used by another instance).
