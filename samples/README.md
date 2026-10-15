# depap samples

| Folder | What |
|---|---|
| [`server/`](server) | Shared applications: Caddy (ingress) and Postgres (database) |
| [`sampleapp/`](sampleapp) | Web page + Python API + migration, linked to both |

```
sampleapp/
├── depap.yaml                     how to deploy (no secrets)
├── .depap-staging.example.yaml    staging values incl. secrets → copy to .depap-staging.yaml
├── .depap-local.example.yaml      values for trying it on this machine
├── docker-compose.yaml            web, backend, migrate
├── web/                           static page (nginx, port 8080)
└── backend/                       API on port 5000: /api/hits, /api/health
```

Files with secrets are committed only as `*.example.yaml`. depap never loads those by itself. Copy them to the real names, which [`.gitignore`](.gitignore) keeps out of Git.

## Try it on this machine

Needs Docker with compose v2. Uses port 18080 and plain HTTP.

```sh
depap init
depap server define dev --local          # depap folder on this machine: ~/.depap-server
depap connect dev

cd samples/server
depap server deploy --apply -f depap-server.yaml,depap-server.local.yaml

cd ../sampleapp
cp .depap-local.example.yaml .depap-local.yaml
depap app deploy --apply -f depap.yaml,.depap-local.yaml

curl -H 'Host: sampleapp.localhost' http://localhost:18080/api/hits   # {"hits": 1, "mode": "local"}
# Browsers resolve *.localhost to this machine: open http://sampleapp.localhost:18080/
```

Look around:

```sh
depap app state -f depap.yaml,.depap-local.yaml
depap connector list
```

Clean up:

```sh
depap app remove --app sampleapp --prefix local --purge database --yes
```

The shared apps keep running. Stop them with `docker compose -p depap-ingress-caddy-main down` and `docker compose -p depap-database-postgres16 down`. Their files are in `~/.depap-server/shared/`.

## Deploy to a real server

1. On the server: Linux with Docker and compose v2, and a user `depapuser` in the `docker` group who can log in with your SSH key. Connect once with `ssh depapuser@<host>` to accept the host key.
2. Point your domains (here `staging.example.com`, `www.staging.example.com`) to the server, and change `acme_email` in `server/depap-server.yaml`.
3. Deploy:

   ```sh
   depap server define prod --address <host>
   depap connect prod

   cd samples/server
   cp .depap-server.secrets.example.yaml .depap-server.secrets.yaml   # set a real password
   depap server deploy --apply          # loads depap-server.yaml + .depap-server.secrets.yaml

   cd ../sampleapp
   cp .depap-staging.example.yaml .depap-staging.yaml                 # set domains and password
   depap app deploy --apply -f depap.yaml,.depap-staging.yaml
   ```

Caddy then gets certificates from Let's Encrypt for the domains, and https://staging.example.com/ shows the page.

## What depap does with the sample

- **Database:**
  - creates user `sampleapp` and database `sampleapp_staging` in `postgres16`;
  - gives `backend` and `migrate` a `DATABASE_URL` and puts them on the database network.
- **Startup:** runs `migrate` once. Compose starts `backend` only after it finished.
- **Routing:** `/api/*` goes to the backend and everything else to `web`, both through Caddy.
- **Upload:** sends the project folder without `.git`, the `.depap-*.yaml` files and the patterns in [`sampleapp/.depapignore`](sampleapp/.depapignore).
