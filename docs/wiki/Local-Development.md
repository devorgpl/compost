# Local development

`compost dev` runs an app's stack on your own machine. Run `compost dev up` in the app's folder: compost adds the shared apps the app needs, gives the app its own database, bucket and logins on them, and starts it. Every developer runs the same command with the same committed `compost.yaml`, so everyone gets the same stack.

```sh
cd myapp
compost dev up                 # shared apps it lacks, its own data, its containers
compost dev up --host          # or: only the data, for running the app from your IDE
compost dev env -o .env        # the variables for that
compost dev reset --yes        # empty database, bucket, …; same passwords
compost dev stop               # pause it; `dev up` resumes
compost dev down --purge --yes # remove it with its data
```

You need Docker with compose v2. The `dev` commands only work on this machine; they never reach a server over SSH.

## Which machine

`dev` commands use a [local server](Servers-and-Shared-Apps.md#defining-servers), never an SSH one, and ignore the context of `compost connect`. The first that applies wins:

1. `-s <name>` or `COMPOST_SERVER`. It must be a local server.
2. The `dev.server` preference (`compost config set dev.server <name>`).
3. The only local server. If there are several, the current context when it is one of them.
4. Otherwise compost defines a local server named `local` (compost's folder: `~/.compost-server`).

If compost isn't initialized yet, `dev` runs `compost init` first. A machine that already runs the [Quick start](Quick-Start.md) uses its server and shared apps.

## Starting an app: `dev up`

`dev up` reads `compost.yaml` and, if it exists, `.compost-local.yaml`; `-f` gives other files. The instance prefix is `--prefix`, else the files' `prefix`, else `local`, so the instance is e.g. `myapp-local`.

For every link of the app ([Applications](Applications.md#database-links)), `dev up` uses a running shared app that matches it, like `app deploy` does. When none is configured, it adds one, with the settings of `compost server generate`'s setup for this machine:

| Link | Adds |
|---|---|
| `database`, `keyvalue` | The link's `type`. Its version is the starter's (e.g. Postgres 18) if the link's `version` allows it, else the lowest major version the link allows, e.g. `postgres15` for `version: "^15"` |
| `storage` | Garage, named `storage` |
| `ingress` with domains | Caddy on port 18080, plain HTTP |
| `oidc` | Keycloak on `http://auth.localhost:18080`, with the Postgres 18 and Caddy it needs |
| `workflow` | Temporal, with what it needs |
| `monitoring` that isn't `optional` | OpenObserve on `http://logs.localhost:18080` |

A link that pins a shared app with `instance`, e.g. a production server's `temporal-main`, uses the shared app of that name. If this machine has none, it uses the one shared app that matches the link without the pin (`services.workflow[jobs]: local has no `temporal-main`; using temporal`), so projects don't bring their own copies of shared apps. Only when none matches is one added, with the pinned name. To choose another one yourself, set its `instance` in `.compost-local.yaml`. Shared apps that are already configured never change, except for the `host_port` that `--host` adds. Shared apps that are configured but not running are started.

```text
Shared apps on local (local)
  adding storage storage (garage 2) for services.storage[files]
  ingress    caddy-main       unchanged
  database   postgres18       unchanged
  storage    storage          updated
  ...
Deploying myapp-local to local (local)
```

Then `dev up` deploys the app with `--apply`. The `schedule` of the app's own [backup profiles](Backup-and-Restore.md#profiles) is turned off for it (`backup.profiles.daily: not scheduled on this machine`): those schedules are meant for its servers. `compost app backup --profile <name>` still backs it up by hand, and the profiles of the local server's `compost-server.yaml` still apply. Its domains come from the overlay, e.g. `myapp.localhost`: browsers resolve `*.localhost` to this machine, so it is at `http://myapp.localhost:18080/` without DNS changes. `--dry-run` shows the shared apps it would add; the app's own dry run follows when nothing is missing. `--pull` and `--recreate` work as with [`app deploy`](Applications.md#deploying).

The shared apps that `dev up` added are stored in the local server's `compost-server.yaml` (in `~/.compost-server`). If you also deploy that server from an infrastructure folder with `compost server deploy`, the shared apps missing from that folder's file are reported as `orphaned` and keep running. Add them to the file to keep managing them from there.

## Several projects

Each app is its own instance with its own databases, users, buckets and realms on the same shared apps, so projects don't touch each other's data. To switch projects, go to the other folder and run `compost dev up` there; the first project keeps running.

To free memory, pause a project with `compost dev stop`. It stops the app's containers and keeps everything else; `compost app list` shows it as `stopped`, and `compost dev up` starts it again. Shared apps keep running for the other projects.

## Running the app outside Docker

To run the app from your IDE, with hot reload or a debugger, use `--host`:

```sh
compost dev up --host
compost dev env -o .env       # then start the app with these variables
```

`dev up --host`:

- Gives the shared apps that the app's database, key-value and storage links use a `host_port` on 127.0.0.1, unless they have one. The first free port from 15432 (Postgres), 13306 (MariaDB, MySQL), 16379 (Valkey, Redis) and 13900 (Garage's S3 API) is used. This recreates the shared app's container once; its data is kept.
- Sets up the links (database and user, bucket and key, key-value database, realm and client, namespace) and stops the app's containers, if an earlier `dev up` started them. `compost app list` shows the instance as `provisioned`.
- Skips ingress and monitoring links: they need the app's containers.

`compost dev env` prints the variables the links give, as a process on this machine sees them: a database at `127.0.0.1:<host_port>` instead of its Docker name. With `--service`, only that service's variables; without it, those of all services, which must not set a variable differently. `--format` is `dotenv` (the default), `shell` (`export KEY='value'`) or `json`. `-o` writes a file with mode `0600` and warns if git doesn't ignore it: it holds passwords. Add it to `.compostignore` as well, or `dev up` without `--host` uploads it, and Docker Compose reads a `.env` it finds there.

```sh
$ compost dev env
DATABASE_URL=postgres://myapp_local_db:4f1c…@127.0.0.1:15432/myapp_local_db
S3_BUCKET=myapp-local-files
S3_ENDPOINT=http://127.0.0.1:13900
```

Only database, key-value and storage links give addresses on 127.0.0.1. The values of other links are those the app's containers get: an OIDC link's `oidc_issuer` is the public `http://auth.localhost:18080/realms/…`, which works from both, but `oidc_internal_url` only works in Docker.

`dev env` also works after `dev up` without `--host` once the shared apps have a `host_port`, e.g. to run a migration or a database client on this machine.

## Fresh data: `dev reset`

```sh
compost dev reset --yes                      # every link that holds data
compost dev reset --kind database --yes      # only databases
compost dev reset --link files --yes         # only one link
```

`dev reset` stops the app, deletes the data of its links (drops the database and its user, deletes the bucket, the key-value database's keys, the realm, the namespace) and runs `dev up` again in the same mode (`--host` if the app was set up with it), which creates them empty. Passwords and keys stay the same, so a `.env` written by `dev env` keeps working; `--new-credentials` generates new ones. Volumes of the app's own compose services are kept: `dev down --purge` removes those.

Without `--yes`, it asks first.

## Removing an app: `dev down`

`compost dev down` removes the app's containers and its instance folder, like [`app remove`](Applications.md#removing). Its databases, buckets and volumes stay for the next `dev up`.

`compost dev down --purge` also deletes the app's data in every shared app it links to, and the volumes of its compose project. It asks first unless you add `--yes`.

Shared apps keep running; [Quick start](Quick-Start.md#5-clean-up) shows how to stop them.

## A stack for the team

Commit `compost.yaml` and a `.compost-local.example.yaml` with the values for this machine, e.g. `*.localhost` domains:

```yaml
# .compost-local.example.yaml: copy to .compost-local.yaml
prefix: local
services:
  ingress:
    - name: site
      domains: ["myapp.localhost"]
```

Each developer copies it to `.compost-local.yaml` (which stays uncommitted, like every `.compost-*.yaml`) and runs `compost dev up`. The links' `type` and `version` decide which shared apps everyone gets. Passwords are generated per machine.
