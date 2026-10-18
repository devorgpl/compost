# Notifications

compost sends notifications through **communication apps**: shared apps of the kind `communication`, one per channel. A communication app runs nothing by itself. compost runs its image when it has something to send, with the channel's settings and credentials in a file only the server user can read.

```sh
compost server generate --with matrix    # or webhook, slack, mattermost, discord, email
compost server deploy --apply --secret .compost-server.secrets.yaml
compost notify test                      # a test message to every channel
```

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
| `matrix` | `homeserver`; `room`, the room ID (`!…:server`, under Room settings → Advanced); `access_token` of an account that has joined the room | `access_token` |
| `email` | `smtp_url` (`smtps://host:465`, or `smtp://host:587` with STARTTLS); `username`, `password` (optional); `from`; `to` (a list); `require_tls` (default `true`: never send in clear text) | `password` |

Secret keys go in the secrets file. They are left out of the stored `compost-server.yaml` and kept in the app's env file on the server, so later deploys don't need them again; a new value replaces the old one. All values must fit on one line.

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
- The env file has `COMMUNICATION_API=1`, `COMMUNICATION_TYPE`, and each configuration key as `COMMUNICATION_<KEY>` in capitals (`to` as a comma-separated list, each header as `COMMUNICATION_HEADER_<n>=Name: value`).
- Keep credentials off command lines inside the container, where the host's `ps` shows them: compost's image passes them to `curl` in a config file.
