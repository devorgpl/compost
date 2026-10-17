# New features plan

Candidate features, taken from what similar tools offer (Kamal, Uncloud, Coolify, Dokploy, CapRover, Dokku, Composure) and checked against compost 1.2 (commands and `docs/wiki`). Ranked by value against effort. Nothing here is decided or scheduled.

## Biggest gaps

1. **Rollback: `app rollback [--to N]`.** Kamal and Coolify both have it. compost replaces `sources/` and the override in place, so there is no previous release to go back to. Keeping the last N rendered releases per instance (sources, override, `compost.yaml`, image digests) would make it cheap.
2. **A real plan in `--dry-run`.** Uncloud shows exactly what will change and asks to confirm. compost's dry run prints only "N files to upload" plus the links. A diff of compose, override and Caddy sites against what is on the server, plus which containers would be recreated, would make `--apply` much safer.
3. **Scheduled backups, an S3 target and retention.** The `s3` target is documented as not implemented, and Coolify sells scheduled S3 backups as a headline feature. Options: a systemd timer or cron entry written by `server deploy` (`backup.schedule`, `backup.keep`), then upload to S3 (or through rclone).
4. **Day-2 commands: `app logs [service] [-f]`, `app exec <service> -- cmd`, `app run` (a one-off container, e.g. migrations).** Kamal and Dokku have them. Today users ssh in and work out the compose project and `-f` flags themselves.

## Next tier

5. **Zero-downtime deploys.** Kamal and Uncloud have them. compost runs `up -d`, so a recreated service is briefly down. For services behind the ingress: start the new container, wait for its healthcheck, switch the Caddy upstream, then stop the old one.
6. **Build locally and send the image without a registry.** Uncloud does this, and Kamal builds locally. Today `build: true` builds on the server, which needs CPU and RAM there and uploads the sources.
7. **Preview environments,** like Coolify and Dokploy's pull-request previews. Instances (`prefix`) and ingress domains already exist. Missing: `--prefix` from the branch name, wildcard domains (e.g. `<prefix>.preview.example.com`), removing stale instances, and a GitHub Action for CI.
8. **Notifications** when a deploy or backup fails or `state` is not OK, as Coolify does with Slack, Discord and email: a webhook target first, then Matrix or email.

## Nice to have

9. **More shared app types:** Redis/Valkey, MySQL/MariaDB, S3-compatible storage (Garage, MinIO), an SMTP relay. Coolify's many one-click services are its main draw, and the link model (a database or user per instance) works the same way for these.
10. **`server bootstrap`:** install Docker and create `compostuser` over SSH, as Kamal and Uncloud do. Today Docker is a requirement users install themselves.
11. **`server prune`:** remove old images and build cache, and show disk usage.
12. **One app on several servers (`-s a,b`):** a small step toward Uncloud's multi-machine model without becoming a cluster.

## Suggested order

Start with 1, 2 and 4. They stay mostly within `app deploy` and the state file, don't change how compost works, and are what users miss soon after the first deploy. 3 is the strongest selling point after that.
