# Files and folders

## On your machine

`~/.compost`, or `$COMPOST_HOME`:

```
~/.compost/
├── compost-manager.yaml    manager key; copy it (or use `init --key`) to manage the same servers elsewhere
├── servers.yaml            servers from `server define`
├── config.yaml             preferences from `compost config`
├── current_context         server name from `connect`
└── ssh/                    shared SSH connection sockets
```

## In an app repository

```
myapp/
├── docker-compose.yaml                compose file, never changed by compost
├── compost.yaml                       how to deploy; commit
├── .compost.secrets.example.yaml      example overlay from `app generate`; commit
├── .compost-staging.yaml              overlay with secrets; don't commit
├── .compostignore                     not uploaded (gitignore syntax)
├── .claude/skills/compost/            Agent Skill from `compost agent skill`, if you install it there
└── …
```

Not uploaded with the sources: `.git/`, files given with `-f`, `.compost-*.yaml`, `.compost.*.yaml` and `.compostignore` patterns. With `deployment.source_dir`, the uploaded folder is that one instead, and `compost.yaml` can live in a separate deployment repository ([Applications](Applications.md#sources-in-another-folder)).

## In an infrastructure folder

```
infra/
├── compost-server.yaml                    shared apps; commit
├── compost-server.prod.yaml               overrides for the real server; commit
├── .compost-server.secrets.example.yaml   commit
└── .compost-server.secrets.yaml           secrets; don't commit
```

## On the server

`~/.compost-server` of the server user, or `server_home` from `servers.yaml`. compost writes only here and to Docker.

```
~/.compost-server/
├── manager_key                       key of the compost installation managing this server
├── compost-server.yaml               effective server config, without secrets
├── state.yaml                        status of shared apps and instances
├── lock/                             exists while a command changes the server
├── tmp/                              short-lived files, e.g. client credentials for external databases
├── backups/                          archives of the `server` backup target; a profile's are named `<instance or server>-<profile>-<time>…`
├── secrets/apps/<app>/<prefix>.yaml  overlay stored by `secret deploy` or `app deploy --secret`
├── secrets/server.yaml               shared apps' secrets stored by `server deploy --secret`
├── shared/
│   ├── ingress/caddy-main/
│   │   ├── compose.yaml
│   │   ├── compose.override.yaml     from its `monitoring`, if set
│   │   ├── conf/Caddyfile
│   │   ├── conf/sites/<instance>-<entry>.caddy
│   │   ├── conf/custom/<site>.caddy  `sites` of compost-server.yaml
│   │   ├── data/                     certificates
│   │   ├── config/
│   │   ├── build/Dockerfile          caddysecurebuild: the image built on the server
│   │   ├── conf/crowdsec/            caddysecurebuild: the CrowdSec agent's log sources, ban profile, whitelist
│   │   ├── logs/access.log           caddysecurebuild: access log of every site, read by CrowdSec
│   │   └── secrets/crowdsec.env      caddysecurebuild: key of the bouncer in Caddy
│   ├── database/postgres18/
│   │   ├── compose.yaml
│   │   ├── data/                     PGDATA
│   │   └── secrets/postgres.env      root password
│   ├── oidc/keycloak/
│   │   ├── compose.yaml
│   │   ├── compose.override.yaml     networks and database settings from its links
│   │   └── secrets/keycloak.env      admin user and password
│   └── monitoring/monitoring/
│       ├── compose.yaml              openobserve and collector
│       ├── compose.override.yaml     ingress network, if `url` is set
│       ├── data/                     OpenObserve's data
│       ├── collector/config.yaml
│       ├── collector/targets/<instance>-<entry>.yaml   metrics endpoints of a link
│       └── secrets/                  openobserve.env, collector.env: root email and password
├── connectors/                       link records, one per instance and entry
│   ├── ingress/caddy-main/<instance>-<entry>.yaml
│   ├── database/postgres18/root-connector.yaml, <instance>-<entry>.yaml
│   │                                 (for an external database: how to reach it, and root credentials)
│   ├── oidc/keycloak/root-connector.yaml, <instance>-<entry>.yaml
│   └── monitoring/monitoring/root-connector.yaml, <instance>-<entry>.yaml
│                                     (root-connector.yaml: URL and root login)
└── apps/<app>/<prefix>/
    ├── sources/                      uploaded files; .compost/ holds the compose_overrides
    ├── compose/compost.override.yaml networks and variables added to your services
    ├── compost.yaml                  effective app config, without secrets
    ├── compost-env.yaml              shared apps this instance uses; links still to remove
    └── releases/<n>/                 a release for `app rollback`: sources/, compost.yaml, release.yaml (time, images)
```

Files with secrets (`secrets/`, connector records of databases and OIDC, `root-connector.yaml` files, `compost.override.yaml`) have mode `0600`, their folders `0700`. Only compost writes `state.yaml`; a compost that does not know its layout (its `schema`) refuses to use it and asks to be upgraded.

## Docker names

| Object | Name |
|---|---|
| Instance compose project | `<app>-<prefix>`, e.g. `myapp-staging` |
| Shared app compose project | `compost-<kind>-<name>`, e.g. `compost-database-postgres18` |
| Shared app compose service | `compost-shared-<name>`, e.g. `compost-shared-postgres18`; OpenObserve's collector is `compost-shared-<name>-collector`, the CrowdSec agent of a `caddysecurebuild` ingress `compost-shared-<name>-crowdsec`. Compose makes the service name a host name on every network the container joins, so it is unique on purpose: an app's own `postgres` service stays `postgres` |
| Shared app container | `compost-<kind>-<name>`, like its project, e.g. `compost-database-postgres18`; OpenObserve's collector is `compost-monitoring-<name>-collector`, a CrowdSec agent `compost-ingress-<name>-crowdsec` |
| Built image | `compost-caddysecurebuild-<name>:<hash>` of a `caddysecurebuild` ingress |
| Release image | `compost-release/<app>-<prefix>:<n>-<service>`: the image a service ran in release `<n>`, kept for `app rollback` |
| Volumes | `compost-ingress-<name>_crowdsec-data` and `_crowdsec-config`: state of a CrowdSec agent |
| Ingress network | `compost-ingress`; a `caddysecurebuild` ingress also has `compost-ingress-<name>_crowdsec`, which only Caddy and its CrowdSec agent join |
| Database network | `compost-db-<name>`; the database's host name is `<name>` |
| OIDC network | `compost-oidc-<name>`; host name `<name>` |
| Monitoring network | `compost-monitoring-<name>`; the collector's host name is `<name>` |
| A service's name on the ingress and monitoring networks | `<app>-<prefix>-<service>`; for a shared app its service name, `compost-shared-<name>` |

So you can use plain Docker commands:

```sh
docker compose -p myapp-staging logs -f backend
docker compose -p compost-database-postgres18 exec compost-shared-postgres18 psql -U postgres
docker exec -it compost-database-postgres18 psql -U postgres
```

From your machine, `compost app logs` and `compost server logs` show the same output without logging in to the server (see [Logs](Applications.md#logs)).
