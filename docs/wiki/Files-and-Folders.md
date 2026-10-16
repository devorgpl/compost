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
├── backups/                          archives of the `server` backup target
├── secrets/apps/<app>/<prefix>.yaml  overlay stored by `secret deploy`
├── shared/
│   ├── ingress/caddy-main/
│   │   ├── compose.yaml
│   │   ├── conf/Caddyfile
│   │   ├── conf/sites/<instance>-<entry>.caddy
│   │   ├── conf/custom/<site>.caddy  `sites` of compost-server.yaml
│   │   ├── data/                     certificates
│   │   └── config/
│   ├── database/postgres18/
│   │   ├── compose.yaml
│   │   ├── data/                     PGDATA
│   │   └── secrets/postgres.env      root password
│   └── oidc/keycloak/
│       ├── compose.yaml
│       ├── compose.override.yaml     networks and database settings from its links
│       └── secrets/keycloak.env      admin user and password
├── connectors/                       link records, one per instance and entry
│   ├── ingress/caddy-main/<instance>-<entry>.yaml
│   ├── database/postgres18/root-connector.yaml, <instance>-<entry>.yaml
│   │                                 (for an external database: how to reach it, and root credentials)
│   └── oidc/keycloak/root-connector.yaml, <instance>-<entry>.yaml
└── apps/<app>/<prefix>/
    ├── sources/                      uploaded files; .compost/ holds the compose_overrides
    ├── compose/compost.override.yaml networks and variables added to your services
    ├── compost.yaml                  effective app config, without secrets
    └── compost-env.yaml              shared apps this instance uses; links still to remove
```

Files with secrets (`secrets/`, connector records of databases and OIDC, `compost.override.yaml`) have mode `0600`, their folders `0700`. Only compost writes `state.yaml`.

## Docker names

| Object | Name |
|---|---|
| Instance compose project | `<app>-<prefix>`, e.g. `myapp-staging` |
| Shared app compose project | `compost-<kind>-<name>`, e.g. `compost-database-postgres18` |
| Ingress network | `compost-ingress` |
| Database network | `compost-db-<name>`; the database's host name is `<name>` |
| OIDC network | `compost-oidc-<name>`; host name `<name>` |
| A service's name on the ingress network | `<app>-<prefix>-<service>` |

So you can use plain Docker commands:

```sh
docker compose -p myapp-staging logs -f backend
docker compose -p compost-database-postgres18 exec postgres psql -U postgres
```
