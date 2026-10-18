# Production setup

From an empty Linux server to your app on HTTPS. Shared apps are configured in an infrastructure folder (e.g. a Git repository), and each app in its own repository.

## 1. Prepare the server

- Linux with Docker Engine 24 or newer and the compose v2 plugin.
- A user (by default `compostuser`) in the `docker` group, who can log in with your SSH key. compost needs no root.
- Ports 80 and 443 open, and your domains' DNS records pointing to the server, so Caddy can get certificates from Let's Encrypt.

Connect once by hand, so SSH asks you to accept the host key. compost never turns off host key checking.

```sh
ssh compostuser@203.0.113.10
```

## 2. Define it

```sh
compost init                                     # once per machine
compost server define prod --address 203.0.113.10
compost connect prod
compost server state                             # prod (…): UNINITIALIZED
```

`server define` takes `--user`, `--port`, `--identity-file` and `--server-home` (see [Servers and shared apps](Servers-and-Shared-Apps.md#defining-servers)). It only writes your local config, nothing on the server. For one command with a different key, add `--ssh-key-file <path>`.

Every command that talks to a server picks it in this order: `-s <name>` → the `COMPOST_SERVER` variable → the context set by `connect`.

## 3. Shared apps

In your infrastructure folder:

```sh
compost server generate --with caddy,postgres    # or without --with to pick from a list
```

This writes:

| File | Content | Commit? |
|---|---|---|
| `compost-server.yaml` | The shared apps, set up for this machine (Caddy on port 18080, no TLS) | yes |
| `compost-server.prod.yaml` | What a real server changes: ports 80/443, [`tls: auto`](Servers-and-Shared-Apps.md#tls), `acme_email` | yes |
| `.compost-server.secrets.example.yaml` | The secret keys with placeholders | yes, as an example |

Set `acme_email` in `compost-server.prod.yaml`. Then copy the secrets example and set real passwords, or remove keys you want compost to generate:

```sh
cp .compost-server.secrets.example.yaml .compost-server.secrets.yaml   # never commit this one
compost server deploy --apply -f compost-server.yaml,compost-server.prod.yaml --secret .compost-server.secrets.yaml
compost server state
```

Generated passwords are kept on the server and reused. The Postgres `root_password` and Keycloak `admin_password` can't change after the first deploy. `--secret` also stores the file on the server, so later deploys, from any machine, need only `-f compost-server.yaml,compost-server.prod.yaml`; deploy with `--secret` again to replace it ([Secrets](Secrets.md#stored-on-the-server)).

## 4. Your app

In the app's repository, next to `docker-compose.yaml`:

```sh
compost app generate                             # compost.yaml + .compost.secrets.example.yaml
```

Edit `compost.yaml`: uncomment the ingress routes and database links you need (see [Applications](Applications.md)). Then give each instance its values. You can keep them in a local file:

```sh
cp .compost.secrets.example.yaml .compost-staging.yaml        # prefix, domains, passwords
compost app deploy --apply -f compost.yaml,.compost-staging.yaml
```

or store them on the server, so any machine (and CI) deploys with `compost.yaml` alone:

```sh
compost secret deploy .compost-staging.yaml --prefix staging
compost app deploy --apply --prefix staging
```

See [Secrets](Secrets.md). Files named `.compost-*.yaml` and `.compost.*.yaml` are never uploaded with the sources.

## 5. Day to day

```sh
compost app deploy --apply --prefix staging      # deploy a new version: same command
compost app deploy --dry-run --prefix staging    # what would change, without changing anything
compost app list
compost app state myapp-staging
compost connector list
```

To skip `--apply` on every deploy:

```sh
compost config set deploy.apply true             # all servers
compost config set deploy.apply false --for-server prod   # but not this one
```

Without `--apply` (and with `deploy.apply` off), deploys only upload and render files; nothing is restarted.

## 6. Backups

Set where backups go in `compost-server.yaml` and deploy it:

```yaml
backup:
  target: server       # archives stay on the server, in ~/.compost-server/backups
  profiles:
    daily: {keep: 7, schedule: "0 3 * * *"}   # the shared apps and every app at 03:00; the 7 newest archives of each are kept
```

The next `server deploy` sets the schedule up on the server (it needs `cron` there); `compost server state` shows the last runs, and `compost backup run daily` runs it now.

See [Backup and restore](Backup-and-Restore.md), also for moving to a new server.

## Checklist

- [ ] DNS for every domain points to the server before the first deploy with `tls: auto` (see [TLS](Servers-and-Shared-Apps.md#tls)).
- [ ] `.compost-server.secrets.yaml` and `.compost-<env>.yaml` are in `.gitignore`.
- [ ] `~/.compost/compost-manager.yaml` is backed up: without the key you can't manage the server (or use `init --key` with the value from the server's `~/.compost-server/manager_key`).
- [ ] Backups run and you have tried a restore.
