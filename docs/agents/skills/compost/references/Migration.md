# Migrating to compost

How to move a server that runs Docker containers set up by hand (a reverse proxy, apps, their databases) to compost, one step at a time. Every phase ends with everything working, and every step can be undone.

| Phase | What changes | Downtime |
|---|---|---|
| [1. The proxy](#phase-1-caddy-replaces-the-proxy) | compost's Caddy serves your existing sites | seconds |
| [2. Shared services](#phase-2-shared-services) | compost's Postgres (and Keycloak) run next to the old ones; old databases are registered as external | none |
| [3. Apps, one by one](#phase-3-apps-one-by-one) | compost deploys the app; it keeps its database and volumes | while the app starts |
| [4. Data, when convenient](#phase-4-data-into-compost) | the app's data moves into compost's Postgres and volumes | a short stop of that app |
| [5. Clean up](#phase-5-clean-up) | old containers, networks and volumes go | none |

compost only adds its own folder (`~/.compost-server`) and containers whose names start with `compost-` or with your apps' instance names. It never touches containers it did not create.

## Before you start

Take stock of what runs:

```sh
docker ps --format '{{.Names}}\t{{.Image}}\t{{.Ports}}'
docker network ls
docker volume ls
docker compose ls                     # compose projects and their folders
```

Note for each app: its compose project and file, its domains, its database (container, name, user, password), and its named volumes. Find the proxy configuration (nginx `server` blocks, Traefik labels, Caddyfile).

Back up every database and volume the usual way before you change anything.

Then set up compost on your machine and define the server ([Production setup](Production-Setup.md#2-define-it)):

```sh
compost init
compost server define prod --address 203.0.113.10
compost connect prod
compost server state                  # prod (…): UNINITIALIZED
```

The SSH user must be in the `docker` group. compost can then see and join the existing containers' networks.

## Phase 1: Caddy replaces the proxy

compost's Caddy can join the Docker networks of your existing containers (`networks`) and serve sites you write yourself (`sites`), so it takes over the proxy's job as it is.

```sh
compost server generate --with caddy  # in your infrastructure folder
```

Translate each site of the old proxy into an entry of `sites`. `config` is the inside of a Caddyfile site block:

```yaml
# compost-server.yaml
shared:
  ingress:
    - name: caddy-main
      type: caddy
      version: 2
      configuration:
        http_port: 80
        https_port: 443
        tls: auto
        acme_email: admin@example.com
        # Existing networks of the containers the sites proxy to.
        networks: [shop_default, blog_default]
        sites:
          - name: shop
            domains: [shop.example.com, www.shop.example.com]
            config: |
              reverse_proxy backend:5000
          - name: blog
            domains: [blog.example.com]
            config: |
              encode gzip
              reverse_proxy blog-ghost-1:2368
```

Upstreams are reached on the joined networks by compose service name (`backend`) or container name (`blog-ghost-1`).

<details>
<summary>Translating common nginx and Traefik setups</summary>

| nginx | Caddy `config` |
|---|---|
| `location / { proxy_pass http://backend:5000; }` | `reverse_proxy backend:5000` |
| `location /api/ { proxy_pass http://api:8080/; }` | `handle_path /api/* {`<br>`  reverse_proxy api:8080`<br>`}` |
| `client_max_body_size 50m;` | `request_body {`<br>`  max_size 50MB`<br>`}` |
| `return 301 https://example.com$request_uri;` (other domain) | a separate site: `redir https://example.com{uri} permanent` |
| `ssl_certificate …` | nothing: Caddy gets certificates itself |

Traefik labels such as ``traefik.http.routers.shop.rule=Host(`shop.example.com`)`` with `traefik.http.services.shop.loadbalancer.server.port=5000` become a site with `domains: [shop.example.com]` and `reverse_proxy <service>:5000`.

</details>

Check, switch, check:

```sh
compost server deploy --dry-run       # validates; lists the networks it joins
docker stop old-proxy                 # or: docker compose -p proxy stop
compost server deploy --apply
curl -I https://shop.example.com/
```

`server deploy` fails before writing any shared app's files if a listed network does not exist. compost never creates them: they belong to your compose projects.

**Undo:** `docker compose -p compost-ingress-caddy-main stop` and start the old proxy again.

Notes:
- Caddy requests new certificates for every domain. With many domains, mind [Let's Encrypt's rate limits](https://letsencrypt.org/docs/rate-limits/); move them in batches by adding sites over several deploys.
- Later changes to `sites` are reloaded without restarting Caddy. Changes to ports or `networks` restart it for a moment.

## Phase 2: shared services

Add compost's Postgres (and Keycloak, if you want logins) next to the old databases. They publish no ports and use their own networks, so nothing conflicts.

Register every old database that apps will keep using as an **external** database. compost does not run it, but apps can link to it and backups can dump it:

```yaml
  database:
    - name: postgres18
      type: postgres
      version: 18
    - name: shop-db                   # the old database container
      type: postgres
      version: 16                     # its major version
      external:
        host: db                      # service or container name on that network
        network: shop_default
```

```sh
compost server deploy --apply
compost server state
#   database   postgres18       postgres 18  running
#   database   shop-db          postgres 16  external, accepting connections
```

An external database is only used by links that name it with `instance`, so adding one never changes which database other apps get. A hosted database (AWS RDS and similar) is registered the same way, without `network` (see [External databases](Servers-and-Shared-Apps.md#external-databases)).

## Phase 3: apps, one by one

### Where compost.yaml lives

- **In the app's repository**, next to its compose file, if you can change it.
- **In a deployment repository** when the app is not yours, or you don't want compost files in it. `deployment.source_dir` points to a checkout of the app:

```
deploy/                         your repository
├── shop/
│   ├── compost.yaml            source_dir: ../upstream/shop
│   ├── compose.migrate.yaml    changes to the app's compose file
│   └── .compost-prod.yaml      domains, passwords (not committed)
└── upstream/shop/              the app's repository: a submodule or a checkout of a tag
```

`compost app generate --compose-file ../upstream/shop/docker-compose.yaml` in `deploy/shop/` writes a `compost.yaml` with `source_dir` set.

An app whose images are pulled (no `build`) needs only its compose file: `source_dir` can point to a folder that holds just that file.

### Write the configuration

The app links to its old database through the external database, with the credentials it already uses:

```yaml
# deploy/shop/compost.yaml
name: shop
deployment:
  source_dir: ../upstream/shop
  compose_overrides: [compose.migrate.yaml]
  build: true
services:
  database:
    - name: main
      type: postgres
      instance: shop-db              # the external database
      targets: [backend, migrate]
      environment:
        DATABASE_URL: db_url
  ingress:
    - name: site
      routes:
        - {name: api, target: backend, port: 5000}
app_services:
  - {name: backend, service: backend}
  - {name: migrate, service: migrate}
backup:
  volumes: [visits]
```

```yaml
# deploy/shop/.compost-prod.yaml
prefix: prod
services:
  database:
    - {name: main, db_name: shop, db_user: shop, db_password: the-current-password}
  ingress:
    - {name: site, domains: [shop.example.com, www.shop.example.com]}
```

compost passes the variables named in `environment` (here `DATABASE_URL`) to the services, built from the external database's host and these credentials. It adds the database's network to them too. Variables in the app's own compose file with the same name are overridden.

The app's compose file often also runs its database and a proxy, and the new deployment must not start them again. Its volumes must be the existing ones. `compose_overrides` changes that without editing the file:

```yaml
# deploy/shop/compose.migrate.yaml
services:
  db: {profiles: [legacy]}            # not started: the old container keeps running
  proxy: {profiles: [legacy]}
  migrate:
    depends_on: !reset {}             # it waited for `db`
volumes:
  visits:                             # the old deployment's volume, used as it is
    external: true
    name: shop_visits
```

`!reset` needs Docker Compose 2.24 or newer. Check what will happen:

```sh
compost app deploy --dry-run -f compost.yaml,.compost-prod.yaml
```

### Test first (stateless apps only)

An app that keeps no data can run next to the old one under a temporary domain, e.g. `domains: [shop-new.example.com]`, until you are happy with it. **Don't do this with an app that writes to its database or volumes**: two copies would write the same data.

### Cut over

```sh
docker compose -p shop stop backend migrate   # the old app; its database keeps running
# remove the `shop` entry from `sites` in compost-server.yaml, then:
compost server deploy --apply                 # Caddy reloads, no restart
compost app deploy --apply -f compost.yaml,.compost-prod.yaml
curl https://shop.example.com/api/health
```

While the site still serves the domain, `app deploy` refuses it: `domain shop.example.com is served by site shop`. The site goes first.

The downtime is the time the deploy takes, mostly building images when `build: true`. Images from a registry only need a pull.

**Undo:** `compost app remove shop-prod --yes`, put the site back, `compost server deploy --apply`, `docker compose -p shop start backend`. The old database and volumes were used in place, so they have everything.

From now on the app is a compost app: `compost app backup shop-prod` already saves its database (dumped from the external one) and its volumes.

## Phase 4: data into compost

When it suits you, move an app's data into compost's Postgres and compost-named volumes. It takes a short stop of that app, and uses only backup and restore:

```sh
compost app backup shop-prod -o shop-before-move.compost.tar.gz
```

Change the configuration:
- `compost.yaml`: remove `instance: shop-db`, so the link uses compost's Postgres (or pin `instance: postgres18`).
- `.compost-prod.yaml`: remove `db_password`, so compost generates one. Keep `db_name`/`db_user`, or remove them too for the defaults.
- `compose.migrate.yaml`: remove the external `visits` volume, so compost uses `shop-prod_visits`.

Then:

```sh
compost app deploy --no-apply -f compost.yaml,.compost-prod.yaml   # the new link; creates nothing yet
compost app restore shop-before-move.compost.tar.gz --include links,volumes --yes
compost app deploy --apply -f compost.yaml,.compost-prod.yaml
```

`app restore` creates the database and user in compost's Postgres and loads the dump. It also copies the volume. The last deploy switches the app over and removes the link record of the old database. The old database itself is not touched, so you can compare or go back. Requests that reach the app between the restore and the last deploy still go to the old database, so stop the app's traffic for that minute if that matters.

The same procedure moves a database from or to a hosted one: point the link to another `instance`.

**Undo:** put the old configuration back and deploy it.

### Keycloak

compost does not move realms between Keycloak servers. Either:

- export each realm, with its users, from the old Keycloak as Keycloak documents it ([Importing and exporting realms](https://www.keycloak.org/server/importExport): `kc.sh export --realm shop --users realm_file`, run while that Keycloak is stopped). Import the file in compost's admin console (*Create realm* → *Resource file*). Then point the app's `oidc` link at it with `realm: shop`: compost adds its own client and the users it lists next to the imported ones, and leaves the rest of the realm alone.

- or let an `oidc` link create a fresh realm, its client and test users ([Applications](Applications.md#oidc-links)), when old accounts need not survive.

## Phase 5: clean up

When no site proxies to an old network any more, and no app links to an old database:

1. Remove the old networks from `networks` and the external databases from `compost-server.yaml`, then `compost server deploy --apply`. compost forgets an external database once no app links to it. Do this first: Docker won't remove a network while Caddy is still attached to it.
2. Remove the old projects once their data is safely in compost and backed up: `docker compose -p shop down`, then the old volumes (`docker volume rm shop_dbdata`, …).

## Checklist per app

- [ ] Domains, database credentials and volumes noted; backup taken
- [ ] `compost.yaml`, overlay and `compose.migrate.yaml` written; `--dry-run` shows the expected links
- [ ] Old app stopped, site removed, app deployed, domains checked
- [ ] `compost app backup` works
- [ ] Later: data moved into compost, overrides removed, old containers removed
