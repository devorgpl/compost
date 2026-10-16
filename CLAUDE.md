# compost

Rust CLI that deploys docker-compose apps to servers over SSH. `README.md` has the development commands; run `just ci` before committing.

## Documentation

User documentation is `docs/wiki/*.md`. The release workflow publishes it to the Gitea wiki for every release tag without a `-` (`scripts/publish-wiki.sh`), so the wiki shows the latest release and `docs/wiki` shows master. Nobody edits the wiki itself.

When a change is visible to users (a command, option, config key, default, output, file on disk), update the docs in the same commit:

- Commands and options: write the clap `///` doc comments in `src/cli/*.rs` well, then run `just docs` to regenerate `CLI-Reference.md`. Never edit that page by hand; `just ci` fails when it is out of date.
- Then update the guide pages that describe the behavior:

  | Area | Page |
  |---|---|
  | `compost.yaml`, overlays, links (ingress, database, oidc), `app *` | `Applications.md`, `Configuration.md` |
  | `compost-server.yaml`, shared app types, `server *`, `connector list` | `Servers-and-Shared-Apps.md`, `Configuration.md` |
  | `secret *`, generated passwords | `Secrets.md` |
  | `app/server backup/restore`, `backup list` | `Backup-and-Restore.md` |
  | Files in `~/.compost`, the app repo or `~/.compost-server`, Docker names | `Files-and-Folders.md` |
  | `config`, env vars, merge rules | `Configuration.md` |
  | Install, release artifacts | `Installation.md` |
  | A new feature worth a walkthrough | `Examples.md`, and `Home.md` if it changes what compost is |

- A new page also goes in `_Sidebar.md` and the table in `Home.md`. Pages are flat (no folders), named `Words-With-Dashes.md`, and link to each other as `[Text](Page.md#heading)`; `just ci` checks those links.
- Pages don't name versions: use `vX.Y.Z` in examples.
- Check what you write against the code, not `docs/design/`: the specs there are design history and are not updated.
- `samples/README.md` and `samples/` are part of the docs: keep them working.

To publish by hand (backfill, or a failed release step): `just wiki v0.3.0` with `GITEA_URL`, `GITEA_REPO` and `WIKI_TOKEN` set.
