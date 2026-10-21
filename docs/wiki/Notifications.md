# Notifications

The server tells you when a scheduled backup fails, when a shared app or an instance goes down, and when its disk fills up, and again when each is fine. On request, it also reports the addresses CrowdSec bans, and newer images for its shared apps. It sends through **communication apps**: shared apps of the kind `communication`, one per channel. A communication app runs nothing by itself. compost runs its image when it has something to send, with the channel's settings and credentials in a file only the server user can read.

```sh
compost server generate --with matrix    # or webhook, slack, mattermost, discord, email
# add `notifications: {send_to: [ops-matrix]}` to compost-server.yaml
compost server deploy --apply --secret .compost-server.secrets.yaml
compost notify test                      # a test message to every channel
```

## What is sent

```yaml
# compost-server.yaml
notifications:
  send_to:
    - ops-chat                                  # every event but bans, bans_daily and updates
    - {to: ops-mail, events: [backup, disk]}    # only these
    - {to: security, events: [bans_daily, updates]}  # bans and updates only go to who lists them
  disk_free_below: 10                           # percent, default 10
  daily_at: "08:00"                             # when bans_daily is sent and updates checked, default 08:00
```

| Event | Sent when | Severity |
|---|---|---|
| `backup` | A [scheduled backup](Backup-and-Restore.md#scheduled-backups) failed (with its error), and when that profile's next run works | `error`, then `ok` |
| `health` | A shared app isn't running, or an instance has a container that isn't healthy (as `server state` and `app state` judge it), at two checks in a row; and when it's fine again | `error`, then `ok` |
| `disk` | Free space on the disk with `~/.compost-server` is below `disk_free_below` percent; and when it is 2 points above it again | `warning`, then `ok` |
| `bans` | The [CrowdSec](Servers-and-Shared-Apps.md#caddysecurebuild-ingress) agent banned addresses since the last check, or would have with a simulated scenario: one message per check, one line per address with its sites and each alert's scenario and id | `warning`, or `info` when all were simulated |
| `bans_daily` | Every day at `daily_at` (the server's time zone): how many addresses were banned in the last 24 hours, their alerts by scenario, the addresses with the most alerts, and how many bans are in force | `info`, also on a day without bans |
| `updates` | Checked every day at `daily_at`: a shared app's tag has a newer image in its registry, or compost has a newer release, that wasn't reported before. One message lists all of them, each shared app with the command that updates it | `info` |

`bans` and `bans_daily` need a `caddysecurebuild` ingress, and only go to channels that list them in `events`: a server facing the internet bans scanners many times a day. Bans you add yourself (`caddy-main.ban`) and the community blocklist's aren't reported, only those of the server's own scenarios. Neither is an address that was banned already: CrowdSec still reads the requests the bans block, so a scanner that keeps going sets off more scenarios with requests that get 403, and each extends its ban. The first check after you subscribe to `bans` only notes where CrowdSec's alerts are, so older bans aren't sent; the first `bans_daily` comes at the first check after `daily_at`, which can be right away.

`updates` runs what [`server images --check`](Servers-and-Shared-Apps.md#deploying-shared-apps) does, on the server, and also only goes to channels that list it: a moving tag like `postgres:18` gets a rebuilt image every week or two. It is sent when the list has something new, so an update you leave for later isn't sent again every day, until another newer image or release comes. A check that can't reach a registry or GitHub tries again the next day; its errors are in `~/.compost-server/logs/backup.log`. [Updates](Updates.md) has the whole procedure.

`backup`, `health` and `disk` are sent once when they change, not on every check. The checks run on the server, from the same crontab line as scheduled backups: `server deploy` installs it (and compost itself, see [Scheduled backups](Backup-and-Restore.md#scheduled-backups)) when `send_to` names a channel, and it runs every five minutes. While a deploy holds the server's lock, health isn't checked, so a deploy restarting containers sends nothing. Instances count once deployed with `--apply`. `server state` lists what is currently reported under `alerts (notified)`, and the last state of each check is kept in `state.yaml`.

A channel that can't be reached is logged as a warning in `~/.compost-server/logs/backup.log`, and that message isn't sent again; check channels with `compost notify test`.

## Channels

```yaml
# compost-server.yaml
shared:
  communication:
    - name: ops-chat
      type: matrix
      configuration:
        homeserver: https://matrix.example.org
        room: "!abc123:example.org"
    - name: ops-mail
      type: email
      configuration:
        smtp_url: smtps://smtp.example.com:465
        username: alerts@example.com
        from: alerts@example.com
        to: [ops@example.com, oncall@example.com]
```

```yaml
# .compost-server.secrets.yaml
shared:
  communication:
    - name: ops-chat
      configuration: {access_token: syt_…}
    - name: ops-mail
      configuration: {password: …}
```

| Type | Keys | Secret keys |
|---|---|---|
| `webhook` | `url`; `headers` (a map, e.g. `{Authorization: Bearer …}`). Receives JSON: `title`, `severity`, `message`, `server`, `event`, `time` | `url`, `headers` |
| `slack`, `mattermost` | `webhook_url` of an incoming webhook | `webhook_url` |
| `discord` | `webhook_url` of a channel webhook | `webhook_url` |
| `matrix` | `homeserver`; `room`, the room ID under Room settings → Advanced (`!…:server`, or `!` and 43 characters with no server part for rooms of version 12, the default for new rooms on current homeservers); `access_token` of an account that has joined the room | `access_token` |
| `email` | `smtp_url` (`smtps://host:465`, or `smtp://host:587` with STARTTLS); `username`, `password` (optional); `from`; `to` (a list); `require_tls` (default `true`: never send in clear text) | `password` |

Secret keys go in the secrets file. They are left out of the stored `compost-server.yaml` and kept in the app's env file on the server, so later deploys don't need them again; a new value replaces the old one. All values must fit on one line.

Matrix messages are not end-to-end encrypted. In an encrypted room, the homeserver accepts them and Element shows them with a warning.

`server deploy` builds compost's image for communication apps on the server (`compost-communication:<hash>`, from [`images/communication`](https://github.com/devorgpl/compost/tree/master/images/communication): Alpine with `curl` and `jq`), checks that it answers, and shows the app as `on demand` in `server state`. It has no containers, so `server logs` has nothing to show and server backups don't save anything of it.

## Test message

```sh
compost notify test                                  # every communication app
compost notify test --to ops-mail --message "Hello from prod"
```

Each channel says `sent` or why it failed, e.g. `HTTP 403: …` from the service; the command fails if any channel failed.

## Your own image

`image` replaces compost's image with any image that implements the contract, e.g. for a channel compost doesn't have:

```yaml
shared:
  communication:
    - name: ops-pager
      type: webhook                # the type decides the variables compost writes
      image: registry.example.com/pager-notify:2
      configuration: {url: https://pager.example.com/api}
```

compost pulls it on `server deploy` (again with `--pull`) and runs it like its own:

```sh
docker run --rm -i --env-file <file> <image> send --severity <info|warning|error|ok> --title <text> [--server <name>] [--event <id>]  < message
docker run --rm <image> describe
```

- `send` reads the message from stdin (it can have several lines) and exits with `0` once the message is delivered; otherwise with another status and the reason on stderr.
- `describe` prints JSON with at least `"api": 1`; deploys fail without it.
- `--event` is `backup`, `health`, `disk`, `bans`, `bans_daily` or `updates` (none for `notify test`), `--server` the server's name as `compost server define` gave it.
- The env file has `COMMUNICATION_API=1`, `COMMUNICATION_TYPE`, and each configuration key as `COMMUNICATION_<KEY>` in capitals (`to` as a comma-separated list, each header as `COMMUNICATION_HEADER_<n>=Name: value`).
- Keep credentials off command lines inside the container, where the host's `ps` shows them: compost's image passes them to `curl` in a config file.
