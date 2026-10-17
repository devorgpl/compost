# Secrets

Passwords and other per-instance values (domains, database credentials, OIDC users) live in **overlays**: YAML files merged over `compost.yaml`. You can keep an overlay on your machine and pass it with `-f`, or store it on the server with `compost secret deploy`.

## Rules

- Commit only examples: `.compost.secrets.example.yaml`, `.compost-staging.example.yaml`. compost never loads `*.example.yaml` by itself.
- Keep the real files out of Git, e.g. with `.compost-*.yaml` and `.compost.*.yaml` in `.gitignore`, keeping the examples with `!*.example.yaml`.
- Files named `.compost-*.yaml` or `.compost.*.yaml`, and every file passed with `-f`, are never uploaded with the sources.
- Passwords you leave out are generated on the first deploy (32 hexadecimal characters; OpenObserve's root password starts with `Compost-` to meet its rules), stored on the server and reused by later deploys.
- On the server, secret files have mode `0600`. Secrets never appear in output, `--dry-run` (shown as `***`), stored configs or remote command lines.

## Local overlays

```sh
compost app deploy --apply -f compost.yaml,.compost-staging.yaml
```

Simple, but every machine that deploys needs the file.

## Overlays stored on the server

```sh
compost secret deploy .compost-staging.yaml [--prefix staging]
compost app deploy --apply --prefix staging      # compost.yaml + the stored overlay
compost app deploy --apply --secret .compost-staging.yaml   # store it and deploy, in one step
compost secret list
compost secret remove --app myapp --prefix staging
```

The stored overlay must name its app (`name: myapp`). The prefix is `--prefix`, else the file's `prefix`, else `main`. It is kept at `~/.compost-server/secrets/apps/<app>/<prefix>.yaml`.

`app deploy` merges it right after the first file: `compost.yaml` → stored overlay → other `-f` files. Local overlays can still override stored values.

`app deploy --secret <file>` uses the file in place of the stored overlay and stores it with the instance's files, so later deploys need only `compost.yaml`. The file may leave out `name`: the app is the one of the `-f` files, and a different `name` is an error. The prefix is `--prefix`, else the file's, else the other files'. With `--dry-run` nothing is stored. Like `-f` files, it is never uploaded with the sources.

- `secret deploy` and `app deploy --secret` replace the stored overlay as a whole. If the new one leaves out a link that only the old one defined (say an ingress entry with its domains), the next `app deploy` drops that link. Run `app deploy --dry-run` first to see the effect.
- `app remove` keeps the stored overlay, so deploying the instance again restores it as it was. `secret remove` deletes it.
- `app backup` saves it in the archive and `app restore` puts it back; such an archive then holds secrets.

## Shared apps

Secrets of shared apps go in `.compost-server.secrets.yaml`, next to `compost-server.yaml`, which `server deploy` loads by default:

```yaml
shared:
  database:
    - name: postgres18
      configuration:
        root_password: change-me-to-a-long-random-string
  oidc:
    - name: keycloak
      configuration:
        admin_password: change-me-to-a-long-random-string
  monitoring:
    - name: monitoring
      configuration:
        root_password: Change-me-to-a-long-random-string-1
```

Lists merge by `name`, so only the secret keys are repeated. The passwords are optional. None can change after the first deploy, except an external database's `root_password`, which a new value replaces. Once on the server, they are kept by later deploys without the file. Generated ones are on the server:

```sh
cat ~/.compost-server/shared/database/postgres18/secrets/postgres.env
grep PASSWORD ~/.compost-server/shared/oidc/keycloak/secrets/keycloak.env
grep root_ ~/.compost-server/connectors/monitoring/monitoring/root-connector.yaml
```

### Stored on the server

As with apps, the file can be kept on the server, so any machine deploys with `compost-server.yaml` alone:

```sh
compost server deploy --apply --secret .compost-server.secrets.yaml   # store it and deploy
compost server deploy --apply                                         # later: compost-server.yaml + the stored file
compost secret list
compost secret remove --shared
```

It is kept at `~/.compost-server/secrets/server.yaml` (mode `0600`). `server deploy` merges it right after the first file: `compost-server.yaml` → stored file → other files (`-f`, or `.compost-server.secrets.yaml` when it exists). Local files can still override stored values.

- `--secret` uses the file in place of the stored one and replaces it as a whole. With `--dry-run` nothing is stored. Without `--apply` it is stored with the other files.
- Entries for shared apps that the config doesn't have are left out, with a note: removing a shared app from `compost-server.yaml` doesn't break deploys.
- `server backup` saves it, and `server restore` puts it back when the server has none.

## Notes

- Every instance linked to the same shared app is on that shared app's Docker network. Networks don't isolate instances from each other yet.
- Apps send data to a monitoring app's collector without credentials: any container on its network can. Only the collector holds OpenObserve's password.
- Server backups hold root passwords and link credentials. Keep archives as safe as the server.
