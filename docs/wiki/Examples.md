# Examples

The repository's [`samples/`](https://github.com/devorgpl/compost/tree/master/samples) folder has a server setup (Caddy, Postgres, Keycloak) and an app (web page, Python API, migration). [Quick start](Quick-Start.md) deploys them on this machine.

## Static site

```yaml
# docker-compose.yaml
services:
  web:
    build: .             # e.g. FROM nginxinc/nginx-unprivileged:alpine, listens on 8080
```

```yaml
# compost.yaml
name: mysite
deployment:
  build: true
services:
  ingress:
    - name: site
      routes:
        - {name: main, target: web, port: 8080}
app_services:
  - {name: web, service: web}
```

```yaml
# .compost-prod.yaml
prefix: prod
services:
  ingress:
    - name: site
      domains: [example.com, www.example.com]
```

```sh
compost app deploy --apply --secret .compost-prod.yaml
```

To build the image on your machine instead of on the server, and upload only the compose file, use `build: local`. It is experimental; see its [known limitations](Applications.md#known-limitations-of-build-local):

```yaml
deployment:
  build: local
```

## API with a database and a migration

From [`samples/sampleapp`](https://github.com/devorgpl/compost/tree/master/samples/sampleapp). The migration runs once before the backend starts; compose's `depends_on` handles the order and compost counts an exit with code 0 as healthy.

```yaml
# docker-compose.yaml
services:
  web:
    build: ./web
    restart: unless-stopped
  backend:
    build: ./backend
    restart: unless-stopped
    volumes: [visits:/data]
    depends_on:
      migrate: {condition: service_completed_successfully}
  migrate:
    build: ./backend
    command: ["python", "migrate.py"]
volumes:
  visits:
```

```yaml
# compost.yaml
name: sampleapp
deployment:
  build: true
services:
  database:
    - name: main-db
      type: postgres
      version: ">=16"
      targets: [backend, migrate]
      environment:
        DATABASE_URL: db_url
  ingress:
    - name: site
      routes:
        - {name: api, target: backend, port: 5000, path: /api}
        - {name: web, target: web, port: 8080}
app_services:
  - {name: web, service: web}
  - {name: backend, service: backend, environment: {APP_MODE: production}}
  - {name: migrate, service: migrate}
backup:
  volumes: [visits]
```

```yaml
# .compost-staging.yaml
prefix: staging
services:
  database:
    - name: main-db
      db_name: sampleapp_staging
      db_user: sampleapp
      db_password: change-me-to-a-long-random-string
  ingress:
    - name: site
      domains: [staging.example.com, www.staging.example.com]
app_services:
  - name: backend
    environment:
      APP_MODE: staging
```

## WordPress on MariaDB

```yaml
# compost-server.yaml
shared:
  ingress:
    - {name: caddy-main, type: caddy}
  database:
    - {name: mariadb11, type: mariadb, version: 11}
```

```yaml
# docker-compose.yaml
services:
  wordpress:
    image: wordpress:6
    volumes: [wp-content:/var/www/html/wp-content]
volumes: {wp-content: {}}
```

```yaml
# compost.yaml
name: blog
app_services:
  - {name: web, service: wordpress}
services:
  database:
    - name: db
      type: mariadb
      targets: [web]
      environment:
        WORDPRESS_DB_HOST: db_host
        WORDPRESS_DB_NAME: db_name
        WORDPRESS_DB_USER: db_user
        WORDPRESS_DB_PASSWORD: db_password
  ingress:
    - name: site
      routes:
        - {name: main, target: web, port: 80}
backup:
  volumes: [wp-content]     # uploads, themes and plugins; the database is dumped through the link
```

```yaml
# .compost-prod.yaml
prefix: prod
services:
  ingress:
    - name: site
      domains: [blog.example.com]
```

`app backup blog-prod` then holds the database dump and `wp-content`; `app restore … --prefix staging -f staging-domains.yaml` makes a copy with its own database.

## Uploads in S3 storage

```yaml
# compost-server.yaml
shared:
  ingress:
    - {name: caddy-main, type: caddy}
  storage:
    - {name: storage, type: garage, configuration: {url: https://files.example.com}}
```

```yaml
# compost.yaml
name: photos
app_services:
  - {name: backend, service: backend}
services:
  storage:
    - name: uploads
      targets: [backend]
      environment:
        S3_ENDPOINT: s3_endpoint                 # for the backend, inside the server
        S3_PUBLIC_ENDPOINT: s3_public_endpoint   # for presigned URLs the browser opens
        S3_REGION: s3_region
        S3_BUCKET: s3_bucket
        AWS_ACCESS_KEY_ID: s3_access_key_id
        AWS_SECRET_ACCESS_KEY: s3_secret_access_key
```

Each instance gets its own bucket (`photos-prod-uploads`, `photos-staging-uploads`) and key. `app backup` includes the bucket's objects. Set the S3 client to path-style addressing.

## A developer's machine

Two projects on one laptop, one with Postgres and S3 storage, the other with Postgres and Valkey. [`compost dev`](Local-Development.md) runs one Postgres for both, with a database each:

```yaml
# shop/compost.yaml
name: shop
app_services: [{name: api, service: api}]
services:
  database:
    - {name: db, type: postgres, version: ">=16", targets: [api], environment: {DATABASE_URL: db_url}}
  storage:
    - {name: files, targets: [api], environment: {S3_ENDPOINT: s3_endpoint, S3_BUCKET: s3_bucket, S3_KEY: s3_access_key_id, S3_SECRET: s3_secret_access_key}}
```

```sh
cd shop && compost dev up --host      # adds postgres18 and garage, publishes them on 127.0.0.1
compost dev env -o .env               # DATABASE_URL=postgres://shop_local_db:…@127.0.0.1:15432/shop_local_db, S3_…
npm run dev                           # your app, with .env

cd ../blog && compost dev up          # reuses postgres18, adds valkey, runs blog in Docker
cd ../shop && compost dev reset --kind database --yes   # an empty shop database; blog's is untouched
```

## Staging and production from one repository

Each overlay is an instance. They share `compost.yaml` and the server's shared apps, with separate domains, databases and containers.

```sh
compost app deploy --apply --secret .compost-staging.yaml   # stores the overlay on the server, then deploys
compost app deploy --apply --secret .compost-prod.yaml
compost app deploy --apply --prefix staging                 # later deploys: the stored overlay is used
compost app list
```

Or on two servers: `-s staging-server` and `-s prod-server`.

## An app you don't own, from a deployment repository

Deploy a third-party app pinned to a release, without adding files to its repository:

```
deploy/
├── wiki/
│   ├── compost.yaml
│   ├── compose.prod.yaml
│   └── .compost-prod.yaml       not committed
└── upstream/wiki/               git submodule, checked out at a release tag
```

```yaml
# deploy/wiki/compost.yaml
name: wiki
deployment:
  source_dir: ../upstream/wiki
  compose_overrides: [compose.prod.yaml]
services:
  database:
    - name: main
      type: postgres
      version: ">=16"
      targets: [app]
      environment:
        DB_URL: db_url
  ingress:
    - name: site
      routes:
        - {name: web, target: app, port: 3000}
app_services:
  - {name: app, service: app}
```

```yaml
# deploy/wiki/compose.prod.yaml: the upstream file brings its own database; compost's is used instead
services:
  db: {profiles: [off]}
  app:
    depends_on: !reset {}
    deploy:
      resources: {limits: {memory: 1g}}
```

```sh
cd deploy/wiki
git -C ../upstream/wiki checkout v2.5.0
compost app deploy --apply --secret .compost-prod.yaml
```

## An app on AWS RDS

The database is hosted; compost creates the app's database and user there with the RDS master user, and backs it up like its own:

```yaml
# compost-server.yaml
shared:
  database:
    - name: rds-main
      type: postgres
      version: 16
      external:
        host: main.abc123.eu-central-1.rds.amazonaws.com
        sslmode: require
      configuration:
        root_user: postgres            # the master user; root_password in .compost-server.secrets.yaml
```

```yaml
# compost.yaml
services:
  database:
    - name: main
      type: postgres
      instance: rds-main               # external databases are always pinned
      targets: [backend]
      environment:
        DATABASE_URL: db_url           # …?sslmode=require
```

The server must reach RDS (security group, VPC). `compost server state` shows `external, accepting connections` when it does.

## Log in with Keycloak

With Keycloak in `compost-server.yaml` (see [Servers and shared apps](Servers-and-Shared-Apps.md#keycloak-oidc)), add an OIDC link to the sample app:

```yaml
# compost.yaml
services:
  oidc:
    - name: login
      type: keycloak
      targets: [backend]
      environment:
        OIDC_ISSUER: oidc_issuer
        OIDC_INTERNAL_URL: oidc_internal_url
        OIDC_CLIENT_ID: oidc_client_id
        OIDC_CLIENT_SECRET: oidc_client_secret
```

Per instance, give the realm its own domain and a test user:

```yaml
# .compost-local.yaml
services:
  oidc:
    - name: login
      realm: sampleapp
      public_url: http://login.localhost:18080
      redirect_uris: ["http://sampleapp.localhost:18080/*"]
      users:
        - {username: alice, email: alice@example.com, first_name: Alice, last_name: Doe, password: alice-password}
  ingress:
    - name: login
      domains: [login.localhost]
      routes:
        - {name: realm, link: oidc/login}
```

```sh
compost app deploy --apply -f compost.yaml,.compost-local.yaml
curl -H 'Host: login.localhost' http://localhost:18080/realms/sampleapp/.well-known/openid-configuration
# {"issuer":"http://login.localhost:18080/realms/sampleapp", ...}
```

`alice` can log in at http://login.localhost:18080/realms/sampleapp/account/. The Keycloak admin console is at the provider's own URL (http://auth.localhost:18080/admin/), as `admin` with the password from `~/.compost-server/shared/oidc/keycloak/secrets/keycloak.env`.

To remove the login, delete both entries and deploy with `--purge oidc`; without it the realm, client and users stay in Keycloak.

## Durable workflows with Temporal

Add Temporal to `compost-server.yaml` (see [Servers and shared apps](Servers-and-Shared-Apps.md#temporal-workflow)). It keeps its data on the Postgres server, and its UI logs in through Keycloak; `compost server generate --with temporal` writes the same:

```yaml
# compost-server.yaml
shared:
  workflow:
    - name: temporal
      type: temporal
      version: 1
      configuration:
        ui:
          url: http://temporal.localhost:18080
          oidc: {realm: temporal}
```

```sh
compost server deploy --apply
```

The UI's users log in from realm `temporal`, which the deploy creates: add them in Keycloak's admin console (http://auth.localhost:18080/admin/, realm `temporal`, Users).

An app with a worker gets its own namespace:

```yaml
# docker-compose.yaml
services:
  worker:
    build: ./worker
    restart: unless-stopped
```

```yaml
# compost.yaml
name: orders
deployment:
  compose_file: docker-compose.yaml
  build: true
services:
  workflow:
    - name: jobs
      type: temporal
      retention_days: 7
      targets: [worker]
      environment:
        TEMPORAL_ADDRESS: wf_address
        TEMPORAL_NAMESPACE: wf_namespace
app_services:
  - {name: worker, service: worker}
```

The worker connects with the two variables, e.g. in Python `await Client.connect(os.environ["TEMPORAL_ADDRESS"], namespace=os.environ["TEMPORAL_NAMESPACE"])`.

```sh
compost app deploy --apply -f compost.yaml,.compost-local.yaml   # prefix: local
compost app state orders-local
#     workflow   jobs             temporal: namespace orders-local, retention 7 days
compost server exec temporal --service admin -- temporal workflow list -n orders-local
```

The workflows show in the UI at http://temporal.localhost:18080/ under namespace `orders-local`. `compost app remove orders-local --purge workflow` deletes the namespace with the app.

## Logs and metrics in OpenObserve

Add a monitoring app to `compost-server.yaml`, and let Caddy and Postgres send their output to it. Caddy serves OpenObserve's UI and also sends its own access log there:

```yaml
# compost-server.yaml
shared:
  ingress:
    - name: caddy-main
      type: caddy
      configuration:
        access_log: true
      monitoring: {}            # output, and Caddy's metrics
  database:
    - name: postgres18
      type: postgres
      version: 18
      monitoring: {}
  monitoring:
    - name: monitoring
      type: openobserve
      version: 1
      configuration:
        url: http://logs.localhost:18080
```

```sh
compost server deploy --apply
grep root_ ~/.compost-server/connectors/monitoring/monitoring/root-connector.yaml   # login for the UI
```

Then link the app. `optional: true` keeps the app deployable on servers without monitoring:

```yaml
# compost.yaml
services:
  monitoring:
    - name: telemetry
      optional: true
      targets: [web, backend, migrate]
      metrics:
        - {target: backend, port: 5000, path: /metrics}   # if the backend serves Prometheus metrics
      environment:                     # for an OpenTelemetry SDK in the backend
        OTEL_EXPORTER_OTLP_ENDPOINT: otlp_endpoint
        OTEL_SERVICE_NAME: service_name
        OTEL_RESOURCE_ATTRIBUTES: resource_attributes
```

```sh
compost app deploy --apply -f compost.yaml,.compost-local.yaml
compost connector list
# monitoring
# └─ monitoring  openobserve 1  running
#    ├─ sampleapp-local/telemetry  logs web, backend, migrate; metrics sampleapp-local-backend:5000/metrics  app: deployed
#    ├─ shared database/postgres18/monitoring  logs postgres
#    └─ shared ingress/caddy-main/monitoring  logs caddy; metrics compost-shared-caddy-main:9180/metrics
```

Open http://logs.localhost:18080/ and log in. The `docker` log stream holds the output of all of them; filter with `fluent_tag = 'sampleapp-local.backend'`. Metrics have a stream each, such as `caddy_http_requests_total` and `container_memory_usage_total`; traces from the SDK show under Traces.

## Block brute-force logins

Switch the ingress to `caddysecurebuild`: Caddy built with CrowdSec. It bans addresses whose login POSTs keep failing with 401 or 403, blocks known exploits with AppSec and throttles login attempts:

```yaml
# compost-server.yaml
shared:
  ingress:
    - name: caddy-main
      type: caddysecurebuild
      configuration:
        security:
          crowdsec:
            whitelist: [203.0.113.7]      # your own address
          rate_limits:
            - name: login
              paths: [/login]
              methods: [POST]
              events: 10
              window: 1m
```

```sh
compost server deploy --apply          # the first one builds Caddy: a few minutes
```

Apps keep their sites and get the defaults on this deploy. One whose admin pages post HTML can turn AppSec off for itself, and an API can get a limit of its own:

```yaml
# compost.yaml
services:
  ingress:
    - name: site
      routes: [ … ]
      security:
        appsec: false
        own_rate_limits:
          - {name: api, paths: [/api/*], events: 100, window: 10s}
```

From an address that isn't whitelisted, a burst of failed logins gets it banned within seconds: every site then answers 403 until `ban_duration` (4 hours by default) passes. Slower attempts are cut off at 10 a minute with 429.

```sh
compost server cmd caddy-main.bans
compost server cmd caddy-main.unban 198.51.100.7
```

If your users get banned for using an app normally (a Matrix client is the common case), add `ignore` rules for the requests behind the ban, or a preset such as `ignore: [matrix]`: see [False positives](Servers-and-Shared-Apps.md#false-positives).

## A review copy of production

```sh
compost app backup myapp-prod -o prod.compost.tar.gz
compost app restore prod.compost.tar.gz --prefix review -f review-domains.yaml
# … test …
compost app remove myapp-review --purge database --volumes --yes
```

`review-domains.yaml` gives the copy its own domains. Its database is a new one named after the `review` prefix, filled from production's dump.

## Nightly backups

```yaml
# compost-server.yaml
backup:
  target: server
  profiles:
    daily:  {include: [links], keep: 7, schedule: "30 2 * * 1-6"}   # databases only
    weekly: {keep: 4, schedule: "30 2 * * sun"}                     # links, sources and volumes
```

```sh
compost -s prod server deploy --apply
```

The server then runs each one by itself: the shared apps and every instance into their own archives, the oldest beyond `keep` deleted. `compost -s prod server state` shows the last runs, `compost -s prod backup list` the archives, and `compost -s prod backup run daily` runs one now. See [Scheduled backups](Backup-and-Restore.md#scheduled-backups).

## An app with its own database

An app whose compose file runs its own MariaDB has no `database` link for compost to dump, and copying its data folder while it runs isn't safe. Dump it into a volume before each backup, and load it after a restore:

```yaml
# compose file
services:
  db:
    image: mariadb:11
    environment: {MARIADB_ROOT_PASSWORD: …}
    volumes: [db-data:/var/lib/mysql, db-dumps:/dumps]
    healthcheck:                  # `after_restore` waits until it accepts connections
      test: [CMD, healthcheck.sh, --connect, --innodb_initialized]
volumes: {db-data: {}, db-dumps: {}}
```

```yaml
# compost.yaml
backup:
  volumes: [db-dumps]
  before_backup:
    - exec: db
      command: mariadb-dump -uroot -p"$MARIADB_ROOT_PASSWORD" --all-databases --single-transaction > /dumps/all.sql
  after_restore:
    - exec: db
      command: mariadb -uroot -p"$MARIADB_ROOT_PASSWORD" < /dumps/all.sql
```

```sh
compost app deploy --apply --secret .compost-prod.yaml    # stores the steps
compost app backup myapp-prod
compost app restore myapp-prod-….compost.tar.gz --include volumes --replace --yes
```

The restore stops the app, puts the dump back into `db-dumps`, starts it and loads the dump. More in [Backup and restore](Backup-and-Restore.md#steps-around-a-backup-or-restore).
