# compost samples

| Folder | What |
|---|---|
| [`server/`](server) | Shared applications: Caddy (ingress), Postgres (database) and Keycloak (OIDC) |
| [`sampleapp/`](sampleapp) | Web page + Python API + migration, linked to both |

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

Needs Docker with compose v2. Uses port 18080 and plain HTTP.

```sh
compost init
compost server define dev --local          # compost folder on this machine: ~/.compost-server
compost connect dev

cd samples/server
compost server deploy --apply -f compost-server.yaml,compost-server.local.yaml

cd ../sampleapp
cp .compost-local.example.yaml .compost-local.yaml
compost app deploy --apply -f compost.yaml,.compost-local.yaml

curl -H 'Host: sampleapp.localhost' http://localhost:18080/api/hits   # {"hits": 1, "mode": "local"}
# Browsers resolve *.localhost to this machine: open http://sampleapp.localhost:18080/
# Keycloak: http://auth.localhost:18080/, user `admin`, password in
# ~/.compost-server/shared/oidc/keycloak/secrets/keycloak.env
```

Look around:

```sh
compost app state -f compost.yaml,.compost-local.yaml
compost connector list
```

Clean up:

```sh
compost app remove --app sampleapp --prefix local --purge database --yes
```

The shared apps keep running. Stop them with `docker compose -p compost-ingress-caddy-main down`, `docker compose -p compost-database-postgres18 down` and `docker compose -p compost-oidc-keycloak down`. Their files are in `~/.compost-server/shared/`.

## Deploy to a real server

1. On the server: Linux with Docker and compose v2, and a user `compostuser` in the `docker` group who can log in with your SSH key. Connect once with `ssh compostuser@<host>` to accept the host key.
2. Point your domains (here `staging.example.com`, `www.staging.example.com`) to the server, and change `acme_email` in `server/compost-server.yaml`.
3. Deploy:

   ```sh
   compost server define prod --address <host>
   compost connect prod

   cd samples/server
   cp .compost-server.secrets.example.yaml .compost-server.secrets.yaml   # set a real password
   compost server deploy --apply          # loads compost-server.yaml + .compost-server.secrets.yaml

   cd ../sampleapp
   cp .compost-staging.example.yaml .compost-staging.yaml                 # set domains and password
   compost app deploy --apply -f compost.yaml,.compost-staging.yaml
   ```

Caddy then gets certificates from Let's Encrypt for the domains, and https://staging.example.com/ shows the page.

## What compost does with the sample

- **Database:**
  - creates user `sampleapp` and database `sampleapp_staging` in `postgres18`;
  - gives `backend` and `migrate` a `DATABASE_URL` and puts them on the database network.
- **Startup:** runs `migrate` once. Compose starts `backend` only after it finished.
- **Routing:** `/api/*` goes to the backend and everything else to `web`, both through Caddy.
- **Upload:** sends the project folder without `.git`, the `.compost-*.yaml` files and the patterns in [`sampleapp/.compostignore`](sampleapp/.compostignore).
