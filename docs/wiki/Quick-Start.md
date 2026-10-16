# Quick start

Run compost against this machine with the sample app from the repository: a web page and a Python API with a Postgres database, behind Caddy. You need Docker with compose v2. It uses port 18080 and plain HTTP, so it runs next to anything already on ports 80 and 443.

```sh
git clone https://gitrepo.dev.org.pl/dev.org.pl/compost.git
cd compost/samples
```

## 1. Define this machine as a server

```sh
compost init
compost server define dev --local     # compost's folder here: ~/.compost-server
compost connect dev                   # the default server from now on
compost server state                  # dev (local): UNINITIALIZED
```

## 2. Start the shared apps

```sh
cd server
compost server deploy --apply -f compost-server.yaml,compost-server.local.yaml
```

`compost-server.yaml` defines Caddy, Postgres 18, Keycloak and OpenObserve for a real server. `compost-server.local.yaml` is an overlay that changes Caddy to port 18080 without TLS, Keycloak to `http://auth.localhost:18080` and OpenObserve to `http://logs.localhost:18080`. Files given with `-f` are merged in order; see [Configuration](Configuration.md#how-files-are-merged).

compost generates the passwords it needs. Keycloak takes a minute or two to start the first time.

```sh
compost server state
# dev (local): OK
#   Docker …, home /home/you/.compost-server
#   ingress    caddy-main       caddy 2  running
#   database   postgres18       postgres 18  running
#   monitoring monitoring       openobserve 1  running
#   oidc       keycloak         keycloak 26  running
```

## 3. Deploy the app

```sh
cd ../sampleapp
cp .compost-local.example.yaml .compost-local.yaml
compost app deploy --apply -f compost.yaml,.compost-local.yaml
```

compost uploads the folder, builds the images, creates a database and user for the app, passes `DATABASE_URL` to the services that need it, sends their output to OpenObserve, runs the migration and routes `sampleapp.localhost` through Caddy.

```sh
curl -H 'Host: sampleapp.localhost' http://localhost:18080/api/hits   # {"hits": 1, "mode": "local"}
```

Browsers resolve `*.localhost` to this machine, so you can also open http://sampleapp.localhost:18080/.

## 4. Look around

```sh
compost app list                      # instances on the server
compost app state sampleapp-local     # services, domains, database
compost connector list                # shared apps and what is linked to them
```

## 5. Clean up

```sh
compost app remove sampleapp-local --purge database --yes
```

`--purge database` also drops the app's database; without it the data stays for the next deploy. The shared apps keep running. To stop them:

```sh
docker compose -p compost-ingress-caddy-main down
docker compose -p compost-database-postgres18 down
docker compose -p compost-oidc-keycloak down
docker compose -p compost-monitoring-monitoring down
```

Their files are in `~/.compost-server/shared/`.

## Next

- See the sample's logs and metrics: [Examples](Examples.md#logs-and-metrics-in-openobserve).
- Add a Keycloak login to the sample: [Examples](Examples.md#log-in-with-keycloak).
- Set up your own app: [Applications](Applications.md).
- Deploy to a real server: [Production setup](Production-Setup.md).
