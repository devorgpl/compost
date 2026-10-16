# About
Compost (compose + host), previously known as Depap and Dockernetes, is a CLI (compost) and WebUI (compost-ui) to manage docker-based server deployment.

# System parts
:
## Server
User can define one or more servers to manage. Access to server is based on ssh connection or operate on localhost.

On the server compost can install and manage:
- shared applications
- user applications

## Shared applications

When installing new applications they can use already preinstalled dependencies.

Shared applications have predefined templates and scripts with possible overrides for different versions.

### Ingress
Ingress is a reverse proxy for installed services. It manage domain to service link, TLS and ACME certificates.
Currently only Caddy is planned for this role, but the list can be extended.

User application can mark its internal service as exposed and current Ingress configuration will be updated. 

Unregistering User application will remove its configuration.

### Database
Multiple instances can coexist. Currently postgres only. Deployment definition includes tags for each (like: postgres-test), db name (like: postgres) and version (like 16) so the application installation can verify if specific type it's available on the server to use.

Applications can be connected to Database using database-link. It's a configuration set that keeps variables for connection. Link can be attached/detached from application. Link is responsible for checking if required db/schema exists and creating one if not.

### OIDC
Similar as for database. Currently Keycloak only. Oidc-link can create realms, clients and users.

### S3
Similar as for database. Link can point to local implementation or external.

### Logs
Application to collect logs. Currently OpenObserve. It can be attached to any docker pod to collect its output.

### Metrics
Application to collect metrics.

## User applications
User creates docker-compose file with all services that application needs.

User creates compost.yaml file describing how this application should be deployed.

User creates .compost-deploy.yaml file with exact deployment values.

# Configuration

## Server
All shared applications are described in compost-server.yaml.
```
shared:
  ingress:
    - name: caddy-main
      type: caddy
      version: 1.0
      configuration:
        ports: [80, 443]
  database:
    - name: postgres16
      type: postgres
      version: 16-slim
      configuration:
        ssl-enabled: true
```

Additional configuration and secrets can be defined in .compost-server-deployment.yaml
```
shared:
  database:
    - name: postgres16
      configuration:
        root-password: xafdfaera
backup:
  destination:
    s3:
      S3_KEY: asdfasdf
      S3_BUCKET: namebucket
    
```

## User application
in compost.yaml user can define a structure how an application should be deployed
```
services:
  ingress:
    - name: ui-front
      optional: true
      links:
        - name: main-port
          link: webapp
          port: 8080
          path: "/ui"
        - name: api-front
          link: backend
          port: 5000
          path: "/api"
  database:
    - name: user-data
      type: postgres
      links:
        - name: user-data
          link: backend
          environment: # database link will expose environment variables that can be mapped to different names
            PG_HOST: db-host
            PG_PASS: db-password
app-services:
  - name: webapp
    service: front-webapp # docker compose service name
  - name: backend
    service: backend-app
```

in .compost-deploy.yaml user can define specific deployment values and secrets. This file can be placed outside the user application repository.
```
services:
  ingress:
    - name: ui-front
      domains: ["main.domain.tld"]
  database:
    - name: user-data
      configuration:
        db-name: userdb
        db-password: dfjskdfs
```
# Actions

## connect/disconnect
On connect (with server name as a param)
Uses ~/.compost/servers.yaml to check if it's on list
Creates ~/.compost/current_context file with current server name 
From now there is no need to add --server_name parameter to run commands

On disconnect removes current_context file.

## init
Initializes compost.

## server
### server define
Defines server in local configuration.
### server state
Check and display server state
### server deploy/server install
Deploys server configuration from working directory
- --apply; also applies and starts applications
- -f <filename(s)>; use custom filename, can be coma-separated or multiple occurrences (-f f1.yaml -f f2.yaml) 
## app
### app state
Connects to server and displays application state.
### app install/app deploy
Deploys current application to server.
## connector
### connector list
Display connector tree (shared_app->shared_app_name->connectors[]) with their state
- shared app exists and it's state
- connected app exists and it's state

# Compost implementation details
Written in Rust. It can work as:
- as a standalone binary
- as a server with WebUI and satelites
- as a docker image server with WebUI and satelites.
- satelite can work as Docker image with mounted socket to docker on host.

Configuration for server is stored in COMPOST_SERVER_HOME, it's ~/.compost-server/ folder by default (but can be in /etc/compost-server and then ~/.compost-server/compost_home.env points to it).

## Shared applications extensions
Shared applications have predefined templates and scripts with possible overrides for different type/versions.
Definitions are part of compost installation.
Each shared application defines:
- templates
- folder structure inside shared/<shared-app-type>/<shared-app-name> and responsibilities (as tags, needed for example to do backup/restore or cleanup/reset)
- code to generate files from templates (like docker-compose.yaml)
- what is exposed
Some code is generic (like add external network to docker-compose)

It registers itself in internal application registry.

It exposes an interface to interact.
- before_apply_application
- apply_application
- after_apply_application

## Standalone version
It connects to remote server and operates on it.

## Server version
Deployed parent server expose WebUI and API for satelites.
On remote server it runs as satelite that connects and register itself on Parent server. Then waits for commands from Parent server.
On parent server the satelite is autoapproved (or manualy approved) as a server connector.
Parent server can use SSH connector as other option (with SSH key defined).

# Use cases
## Initialize
1. User runs `compost init`
2. It creates an entry in COMPOST_HOME (~/.compost) in file compost-manager.yaml
- manager_key=generated uuid
3. If it exists, display information and exit
## Define and get server (CLI)
Requires #Initialize step
1. User runs `compost server define --server_name <server_name> --server_address <server_address> --user compostuser`
2. It creates an entry in COMPOST_HOME (~/.compost) in file servers.yaml
- with server name and address
- server connector (SSH as default with SSH key in ~/.ssh/id_rsa and username compostuser)
- COMPOST_SERVER_HOME (path on remote server, default ~/.compost-server)
3. User runs `compost server state --server_name <server_name>`
4. It checks if server is reachable and if configuration compost-server.yaml is there and if manager_key is there and equals local manager_key 
5. It reads remote compost-server.yaml file and checks if all defined services are there and running
6. It diplays all services and their status

## Ingress and Postgres on server
Requires #Define and get server step
1. User defines services in compost-server.yaml file in working directory
```
shared:
  ingress:
    - name: caddy-main
      type: caddy
      version: 1.0
      configuration:
        ports: [80, 443]
  database:
    - name: postgres16
      type: postgres
      version: 16-slim
      configuration:
        ssl-enabled: true
```
and .compost-server-secrets.yaml
```
shared:
  database:
    - name: postgres16
      configuration:
        # password for root-user (postgres)
        root-password: asdfasdf
```
2. User runs `compost server deploy --server_name <server_name>`
3. It runs "state" steps and if it's not OK then exit
4. Finds built-in snippets for shared services defined in compost-server.yaml file
5. It creates remote structure in COMPOST_SERVER_HOME
- shared/ingress/caddy-main; with generated docker-compose in services folder; with generated configuration in conf folder;
- shared/database/postgres16; with generated docker-compose in services folder; with generated configuration in conf folder; with secrets in secrets folder;
- connectors/database/postgres16; with generated root-connector.yaml that contains network_name, service_name, service_port, root_db, root_user, root_password that match posetgres16 values.
6. It creates entries in compost-server.yaml file in COMPOST_SERVER_HOME; sets "state" for each service as "candidate"
7. User runs `compost server deploy --apply --server_name <server_name>`
8. It runs all standard "deploy" steps
9. It starts all services defined and mark their state as "deployed"
10. It runs "state" again to check if everything is ok

## Static web application
1. User defines compost.yaml
```
name: mystaticweb
deployment:
  type: docker-compose
  # docker-compose.yaml is default
  compose_file: docker-compose.yaml 
  build: true
  # attach_sources forced to true if build is true
  attach_sources: true 
services:
  ingress:
    - name: ui-front
      links:
        - name: main-port
          link: webapp
          port: 8080
          path: "/ui"
app-services:
  - name: webapp
    service: front-webapp # docker compose service name
```
and .compost-staging.yaml
```
# by default prefix is taken from app name
prefix: "staging"
services:
  ingress:
    - name: ui-front
      domains: ["staging.domain.tld", "www.staging.domain.tld"]
```
2. User runs `compost app deploy --apply --server_name <server_name> -f compost.yaml -f .compost.yaml`
3. It runs "server state" steps and if it's not OK then exit
4. It creates remote structure in COMPOST_SERVER_HOME
- apps/<app_name>/<app_prefix>/sources; with copy of all files from current directory
- apps/<app_name>/<app_prefix>/compose; with a copy of docker-compose file
- apps/<app_name>/<app_prefix>/compost.yaml; effective content of compost.yaml
5. Finds installed "ingress" service and asks for:
- apply_application; It'll modify docker-compose file to be part of the same docker network as ingress instance (or create separate with additional rules if possible); 
6. docker compose build && docker compose up -d
7. Finds installed "ingress" service and asks for:
- after_apply_application; create file with revproxy rules (real app service name is <prefix>-front-webapp);It'll restart ingress service to apply new revproxy rules

## Web application with API
1. User defines compost.yaml
```
name: mystaticweb
deployment:
  type: docker-compose
  # docker-compose.yaml is default
  compose_file: docker-compose.yaml 
  build: true
  # attach_sources forced to true if build is true
  attach_sources: true 
services:
  database:
    - name: system-data
      type: postgres
      version: ">16"
      links:
        - name: backend-db
          links: ["backend", "migration"]
          db_name: myappdb
          useprefix: true
          environment:
            POSTGRES_USER: db-user
            POSTGRES_PASSWORD: db-password
            POSTGRES_DB: db-name
            POSTGRES_HOST: db-host
            # url can be created from other variables, so it looks reduntant here, but possible
            POSTGRES_URL: db-url
  ingress:
    - name: ui-front
      links:
        - name: main-port
          link: webapp
          port: 8080
          path: "/ui"
        - name: backend-port
          link: backend
          port: 5000
          path: "/api"
    - name: api-front
      links:
        - name: backend-port
          link: backend
          port: 5000
          path: "/"
app-services:
  - name: webapp
    service: front-webapp # docker compose service name
  - name: migration
    service: backend-migration # docker compose service name
  - name: backend
    service: backend-webapp # docker compose service name
    environment:
      APP_MODE: "production"
      APP_FEATURE_USERS: true
```
and .compost-staging.yaml
```
# by default prefix is taken from app name
prefix: "staging"
services:
  database:
    - name: system-data
      database_username: myappuser
      database_password: myapppassword
      database_name: mystagingappdb
      useprefix: false
  ingress:
    - name: ui-front
      domains: ["staging.domain.tld", "www.staging.domain.tld"]
    - name: api-front
      domains: ["api.staging.domain.tld"]
app-services:
  - name: backend
    environment:
      APP_MODE: "staging"
```
2. User runs `compost app deploy --apply --server_name <server_name> -f compost.yaml -f .compost.yaml`
3. It runs "server state" steps and if it's not OK then exit
4. It creates remote structure in COMPOST_SERVER_HOME
- apps/<app_name>/<app_prefix>/sources; with copy of all files from current directory
- apps/<app_name>/<app_prefix>/compose; with a copy of docker-compose file
- apps/<app_name>/<app_prefix>/compost.yaml; effective content of compost.yaml
5. Finds installed "ingress" service and asks for:
- apply_application; It'll modify docker-compose file to be part of the same docker network as ingress instance (or create separate with additional rules if possible);
6. Finds installed "postgres" with version greater than 16 service and asks for apply_application;
- pin exact version in apps/<app_name>/<app_prefix>/compost-env.yaml
- It'll check if connection exist, if not then create it (in connectors/database/postgres16/<app_prefix>-<link_name>.yaml)
- based on connection details database would be created if not exists
- It'll set environment variables for database connection based on mappings from database link and store them in app's .env file in compose folder
7. docker compose build && docker compose up -d
8. Finds installed "postgres" service (direct reference already stored) and asks for:
- after_apply_application; nothing here.
8. Finds installed "ingress" service and asks for:
- after_apply_application; create file with revproxy rules (real app service name is <prefix>-front-webapp);It'll restart ingress service to apply new revproxy rules
