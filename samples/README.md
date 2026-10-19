# compost samples

| Folder | What |
|---|---|
| [`server/`](server) | Shared applications: Caddy (ingress), Postgres (database), Keycloak (OIDC) and OpenObserve (monitoring) |
| [`sampleapp/`](sampleapp) | Web page + Python API + migration, linked to the ingress, the database and [monitoring](#logs-and-metrics); [a login](#log-in-with-keycloak) can be added |

```
sampleapp/
├── compost.yaml                     how to deploy (no secrets)
├── .compost-staging.example.yaml    staging values incl. secrets → copy to .compost-staging.yaml
├── .compost-local.example.yaml      values for trying it on this machine
├── docker-compose.yaml            web, backend, migrate
├── web/                           static page (nginx, port 8080)
└── backend/                       API on port 5000: /api/hits, /api/health
```

Files with secrets are committed only as `*.example.yaml`. compost never loads those by itself. Copy them to the real names, which [`.gitignore`](.gitignore) keeps out of Git.

## Try it on this machine

Needs Docker with compose v2. Uses port 18080 and plain HTTP, and port 24224 on `127.0.0.1` for collecting container output.

```sh
compost init
compost server define dev --local          # compost folder on this machine: ~/.compost-server
compost connect dev

cd samples/server
compost server deploy --apply          # compost-server.yaml: set up for this machine

cd ../sampleapp
cp .compost-local.example.yaml .compost-local.yaml
compost app deploy --apply -f compost.yaml,.compost-local.yaml

curl -H 'Host: sampleapp.localhost' http://localhost:18080/api/hits   # {"hits": 1, "mode": "local"}
# Browsers resolve *.localhost to this machine: open http://sampleapp.localhost:18080/
```

Look around:

```sh
compost app state -f compost.yaml,.compost-local.yaml
compost connector list
```

Clean up (with `--purge oidc` too if you [added the login](#log-in-with-keycloak)):

```sh
compost app remove --app sampleapp --prefix local --purge database --yes
```

The shared apps keep running. Stop them with `docker compose -p compost-ingress-caddy-main down`, `docker compose -p compost-database-postgres18 down` and `docker compose -p compost-oidc-keycloak down` and `docker compose -p compost-monitoring-monitoring down`. Their files are in `~/.compost-server/shared/`.

## With `compost dev`

[`compost dev`](../docs/wiki/Local-Development.md) does the above in one command per app: it adds the shared apps an app needs (here Caddy and Postgres, if this machine has none) and deploys it.

```sh
cd samples/sampleapp
cp .compost-local.example.yaml .compost-local.yaml
compost dev up
curl -H 'Host: sampleapp.localhost' http://localhost:18080/api/hits   # {"hits": 1, "mode": "local"}

compost dev reset --yes        # an empty database with the same password: hits start at 1 again
compost dev down --purge --yes # remove it with its database and volumes
```

To run the backend yourself instead (with Python and `psycopg`), set up only its database and publish Postgres on 127.0.0.1:

```sh
compost dev up --host
compost dev env --service backend --format shell   # export DATABASE_URL='postgres://…@127.0.0.1:15432/…'
eval "$(compost dev env --service backend --format shell)"
python backend/migrate.py
```

## Logs and metrics

The server samples include OpenObserve (`shared.monitoring`). Caddy, Postgres and Keycloak send their output to it (`monitoring: {}`), and Caddy its access log and metrics. The sample app's `telemetry` link sends the output of `web`, `backend` and `migrate`; it is `optional`, so the app deploys on servers without monitoring too.

Open http://logs.localhost:18080/ and log in with the root email and password compost generated:

```sh
grep root_ ~/.compost-server/connectors/monitoring/monitoring/root-connector.yaml
```

Under Logs, pick the stream `docker`, e.g. with the filter `fluent_tag = 'sampleapp-local.backend'`. Under Metrics are Caddy's `caddy_http_*` and the containers' `container_*` metrics. The backend also gets `OTEL_EXPORTER_OTLP_ENDPOINT` and `OTEL_SERVICE_NAME`, for an OpenTelemetry SDK it doesn't use.

## Log in with Keycloak

The server samples include Keycloak, an OpenID Connect provider (`shared.oidc` in [`server/compost-server.yaml`](server/compost-server.yaml)). The deploy above started it:
- its data is in its own database `compost_oidc_keycloak_db` in `postgres18` (Keycloak needs postgres 18 or newer);
- Caddy serves it on the domain of its `url`, here http://auth.localhost:18080/;
- the first start takes a minute or two while Keycloak creates its tables.

Open the admin console at http://auth.localhost:18080/admin/ and log in as `admin`. compost generated the password on the first deploy:

```sh
grep PASSWORD ~/.compost-server/shared/oidc/keycloak/secrets/keycloak.env
```

### Give the sample app a login

An `oidc` link creates a realm, a client and test users, and passes the client's settings to app services. Add it to `sampleapp/compost.yaml`, next to `database` and `ingress`:

```yaml
services:
  oidc:
    - name: login
      type: keycloak
      targets: [backend]
      environment:
        OIDC_ISSUER: oidc_issuer
        OIDC_INTERNAL_URL: oidc_internal_url    # for server-to-server calls, on Keycloak's network
        OIDC_CLIENT_ID: oidc_client_id
        OIDC_CLIENT_SECRET: oidc_client_secret  # generated on the first deploy, then kept
```

The values that differ per instance go in `.compost-local.yaml`. Here the realm also gets its own domain, `login.localhost`: an ingress route with `link: oidc/login` serves only that realm there. The admin console, the `master` realm and other realms are not reachable on it.

```yaml
services:
  oidc:
    - name: login
      realm: sampleapp                          # default: sampleapp-local
      public_url: http://login.localhost:18080  # issuer and login pages use this domain
      redirect_uris: ["http://sampleapp.localhost:18080/*"]
      users:
        - {username: alice, email: alice@example.com, first_name: Alice, last_name: Doe, password: alice-password}
  ingress:
    - name: login
      domains: ["login.localhost"]
      routes:
        - {name: realm, link: oidc/login}
```

Deploy and check:

```sh
compost app deploy --apply -f compost.yaml,.compost-local.yaml

curl -H 'Host: login.localhost' http://localhost:18080/realms/sampleapp/.well-known/openid-configuration
# {"issuer":"http://login.localhost:18080/realms/sampleapp", ...}
curl -o /dev/null -w '%{http_code}\n' -H 'Host: login.localhost' http://localhost:18080/admin/   # 404
docker exec sampleapp-local-backend-1 env | grep OIDC_
```

Log in as `alice` on the realm's account page: http://login.localhost:18080/realms/sampleapp/account/. The sample backend does not use the variables itself; an app would hand them to its OIDC library.

### Keep the values on the server

The overlay can also be stored on the server. Later deploys then need only `compost.yaml`, and the stored values are merged in every time, so the links stay as they are:

```sh
compost app deploy --apply --secret .compost-local.yaml   # stored for sampleapp-local, and deployed
compost app deploy --apply --prefix local                 # compost.yaml from this folder + the stored values
```

`--secret` replaces the stored overlay as a whole. When a new one leaves out a link (say the `login` ingress entry), the next `app deploy` removes it. Run `app deploy --dry-run` first to see what would change.

### Remove the login

Delete the `oidc` entry and the `login` ingress entry, then deploy again. If you stored the overlay, deploy with `--secret .compost-local.yaml` instead, or delete it with `compost secret remove --app sampleapp --prefix local`: the stored values would otherwise bring the login back. compost removes the route and the link record. The realm, client and users stay in Keycloak, unless you add `--purge oidc`:

```sh
compost app deploy --apply -f compost.yaml,.compost-local.yaml --purge oidc
```

## Deploy to a real server

1. On the server: Linux with Docker and compose v2, and a user `compostuser` in the `docker` group who can log in with your SSH key. Connect once with `ssh compostuser@<host>` to accept the host key.
2. Point your domains (here `staging.example.com`, `www.staging.example.com`, `auth.example.com` for Keycloak and `logs.example.com` for OpenObserve) to the server. In `server/compost-server.prod.yaml`, change `acme_email` and the `url` of Keycloak and OpenObserve.
3. Deploy:

   ```sh
   compost server define prod --address <host>
   compost connect prod

   cd samples/server
   cp .compost-server.secrets.example.yaml .compost-server.secrets.yaml   # set real passwords (postgres, keycloak admin, openobserve root)
   compost server deploy --apply -f compost-server.yaml,compost-server.prod.yaml --secret .compost-server.secrets.yaml   # stores it on the server and deploys

   cd ../sampleapp
   cp .compost-staging.example.yaml .compost-staging.yaml                 # set domains and password
   compost app deploy --apply --secret .compost-staging.yaml             # stores it on the server and deploys
   ```

Caddy then gets certificates from Let's Encrypt for the domains, and https://staging.example.com/ shows the page.

The secrets are now stored on the server, so later deploys, from any machine, need only the committed files:

```sh
cd samples/server && compost server deploy --apply -f compost-server.yaml,compost-server.prod.yaml
cd ../sampleapp   && compost app deploy --apply --prefix staging
```

To change a secret, edit the file and deploy again with `--secret`: it replaces the stored copy.

## What compost does with the sample

- **Database:**
  - creates user `sampleapp` and database `sampleapp_staging` in `postgres18`;
  - gives `backend` and `migrate` a `DATABASE_URL` and puts them on the database network.
- **Startup:** runs `migrate` once. Compose starts `backend` only after it finished.
- **Routing:** `/api/*` goes to the backend and everything else to `web`, both through Caddy.
- **Monitoring:** sends the output of all three services to OpenObserve and gives them the OpenTelemetry variables.
- **Login** (when added): creates realm `sampleapp` with a client and the user `alice` in Keycloak before `backend` starts, gives `backend` the `OIDC_*` variables, and routes `login.localhost` to that realm only.
- **Upload:** sends the project folder without `.git`, the `.compost-*.yaml` files and the patterns in [`sampleapp/.compostignore`](sampleapp/.compostignore).
