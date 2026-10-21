# Updates

Nothing on a server updates by itself: a shared app runs the image it was deployed with until a `server deploy --apply` starts another one. This page shows how to see what runs, find out what is newer, get told when something is, and update.

## What changes, and how

| What | Example | Gets newer with |
|---|---|---|
| A **moving tag** | `caddy:2-alpine`, `postgres:18`, `valkey/valkey:9` | Its publisher pushing a new image behind the tag: a patch release, or a rebuild for its base system. `server deploy --apply --pull` fetches it |
| An **exact tag** | `crowdsecurity/crowdsec:v1.8.1`, `dxflrs/garage:vX.Y.Z` | A compost release that pins a newer one, or your `image` |
| An image **compost builds** | `caddysecurebuild`'s Caddy, the communication image | Its `FROM` images, as a moving tag (`--pull` rebuilds on the newest), and its pinned Go modules, with a compost release |
| A **major version** | Postgres 17 to 18 | Your `version`, and for databases a move of the data (see [Major versions](#major-versions)) |
| Your **apps' images** | `ghcr.io/you/shop:2` | Your releases and `app deploy --apply --pull`; compost doesn't check them |

[Bundled images](Bundled-Images.md) lists the image and version every type uses by default, and which of them move.

## What runs

`compost images` prints the defaults of the compost you have installed; `compost server images` what each shared app's containers run, with the version, when the image was built and pulled, and since when it runs:

```text
ingress caddy-main
  compost-shared-caddy-main: caddy:2-alpine, version v2.11.7
    image d8542f48d34a, built 2026-10-05, pulled 2026-10-21 15:28
    running since 2026-10-21 15:30, deployed with this image 2026-10-21 15:30
```

See [Servers and shared apps](Servers-and-Shared-Apps.md#deploying-shared-apps) for where each value comes from.

## What is newer

```sh
compost server images --check
```

asks each image's registry what its tag points to now (on the server, with `docker buildx imagetools`), and GitHub whether compost has a newer release. It changes nothing. A shared app with a newer image gets a line like `newer image for caddy:2-alpine: version v2.11.8 (running v2.11.7)` and the command that updates only it.

A newer image is not necessarily a security fix, and the newest image can still have known vulnerabilities: compost doesn't know about them. Read the release notes of what you run (Caddy, Postgres, Keycloak …), and to find known vulnerabilities, scan the images, e.g. `trivy image caddy:2-alpine` or `docker scout cves caddy:2-alpine`.

## Getting told

Add `updates` to a channel's `events` ([Notifications](Notifications.md)):

```yaml
# compost-server.yaml
notifications:
  send_to:
    - ops-chat                                      # backups, health, disk
    - {to: ops-chat, events: [updates]}             # and newer images
  daily_at: "08:00"                                 # when they are checked
```

Deploy it with `compost server deploy --apply`. Every day at `daily_at`, the server runs the same check and sends one message when there is something it didn't report before:

```text
Newer images for caddy-main, postgres18
ingress caddy-main: newer image for caddy:2-alpine: version v2.11.8 (running v2.11.7)
  compost server deploy --apply --pull --shared caddy-main
database postgres18: newer image for postgres:18-alpine: version 18.2 (running 18.1)
  compost server deploy --apply --pull --shared postgres18
`compost server images --check` shows the details.
```

An update you leave for later isn't sent again every day: the next message comes with the next newer image or compost release.

## Updating a shared app

```sh
compost server images --check                                  # what is newer
compost server backup --shared postgres18                      # before a database
compost server deploy --dry-run --pull --shared postgres18     # pulled; recreated if a newer image comes
compost server deploy --apply --pull --shared postgres18
compost server images                                          # the new version, and since when
```

- `--shared` limits the deploy, `--pull` and the restarts to the shared apps it names, so updating Caddy doesn't also move Postgres to a newer image behind its tag. Without it, `--pull` updates all of them.
- The container is recreated on the new image; its data is kept. Apps using it lose it for that moment: requests to Caddy fail while it restarts, apps reconnect to their database. Before Caddy is replaced, its configuration is validated with the new image, and the old one keeps running if that fails.
- To update at your pace instead of the publisher's, set `image` to an exact tag (`postgres:18.1-alpine`) or a digest (`postgres:18-alpine@sha256:…`). `--pull` then fetches nothing new: you change `image` to update.
- To go back, set `image` to the exact tag of the version that ran before (`server images` showed it) and deploy with `--apply`.

## Major versions

A database's data only works with its major version: Postgres 18 does not start on Postgres 17's data, so changing `version` of a database that has data is not an update. Run the new major version as another shared app (`postgres18` next to `postgres17`), move each app to it as in [Moving an app's data to another database](Backup-and-Restore.md#moving-an-apps-data-to-another-database), and remove the old one when no app uses it.

Temporal takes one minor version at a time (see [temporal](Servers-and-Shared-Apps.md#temporal-workflow)). Keycloak, OpenObserve, Mailpit and Garage have a default image for their default major version only; for another one, set `image`.

## Updating compost

Install the new release ([Installation](Installation.md#update-and-uninstall)), then:

```sh
compost images                                      # its defaults; compare with Bundled images
compost server deploy --dry-run                     # which shared apps a newer pinned image recreates
compost server deploy --apply
```

A release changes exact tags and the modules of the images compost builds; the dry run shows which shared apps get `recreated`. The deploy also installs the new compost on the server when it has scheduled backups or notifications, so the daily check runs the new version.
