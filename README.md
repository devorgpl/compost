# compost

Deploy docker-compose applications to your own servers, with shared Caddy (HTTPS) and Postgres.

compost is one CLI on your machine. It manages servers over SSH, or this machine directly, and installs nothing on them except plain files and Docker containers. See [the MVP specification](docs/SPECIFICATION_MVP.md) and the [samples](samples/README.md).

## Install

compost is a single portable binary with no dependencies. Linux builds are static and run on any distribution. It needs `ssh` on your machine; servers need Docker with compose v2.

**With the install script:** detects your system, verifies the checksum, and installs to `~/.local/bin`.

```sh
curl -fsSL https://gitrepo.dev.org.pl/dev.org.pl/compost/raw/branch/master/scripts/install.sh | sh
```

For a specific release or another directory, pass options after `sh -s --`, e.g. `... | sh -s -- --version v0.1.0 --to /usr/local/bin`.

**By hand:** download the archive for your system and `SHA256SUMS` from the [releases page](https://gitrepo.dev.org.pl/dev.org.pl/compost/releases), then:

```sh
sha256sum --ignore-missing -c SHA256SUMS
tar -xzf compost-v0.1.0-x86_64-unknown-linux-musl.tar.gz
install -m 755 compost-v0.1.0-x86_64-unknown-linux-musl/compost ~/.local/bin/compost
compost --version
```

| System | Target |
|---|---|
| Linux x86_64 | `x86_64-unknown-linux-musl` |
| Linux arm64 | `aarch64-unknown-linux-musl` |

**macOS:** there are no prebuilt binaries yet, because linking for macOS needs Apple's SDK, which only a macOS machine has. Build from source with Rust instead:

```sh
cargo install --locked --git https://gitrepo.dev.org.pl/dev.org.pl/compost --tag v0.1.0
```

To update, install again. To uninstall, delete the binary and `~/.compost`.

## Quick start

```sh
compost init
compost server define prod --address 203.0.113.10   # SSH user compostuser by default
compost connect prod
compost server state
```

Then follow [samples/README.md](samples/README.md).

## Starting a new setup

```sh
# Infrastructure folder: shared apps of the server
compost server generate --with caddy,postgres   # or without --with to choose interactively
compost server deploy --apply                   # this machine; see the printed notes for prod

# Application folder: reads docker-compose.yaml
compost app generate
cp .compost.secrets.example.yaml .compost.secrets.yaml      # domains, passwords
compost secret deploy .compost.secrets.yaml --prefix staging  # kept on the server
compost app deploy --apply --prefix staging

compost config set deploy.apply true            # optional: deploy without --apply
```

## Backup and restore

```sh
compost app backup myapp-prod                       # links, sources and volumes → myapp-prod-<time>.compost.tar.gz
compost app backup myapp-prod --link main-db --format raw   # just the SQL dump
compost app restore myapp-prod-….compost.tar.gz --prefix review -f review.yaml   # a copy of the app
compost server backup --target server               # shared apps, kept in the server's backups/ folder
compost server restore prod-….compost.tar.gz          # on a new server, then `compost app restore`
```

See §10.13–§10.18 of [docs/SPECIFICATION_MVP.md](docs/SPECIFICATION_MVP.md).

## Development

Needs Rust, plus [just](https://just.systems), `rustfmt` and `clippy`.

```sh
just            # list recipes
just ci         # formatting, clippy, tests: run before committing
just try init   # run compost with a throwaway home in target/
```

## Releases

Gitea Actions ([`.gitea/workflows`](.gitea/workflows)):
- **Every branch push and pull request:** runs `just ci`.
- **A pushed `v*` tag:** tests, builds the portable binaries and publishes a release with notes generated from the commits since the previous tag.

Between releases, master carries the next version with a `-dev` suffix (e.g. `0.2.0-dev`), so builds from master never pass for a release. To release it:

```sh
just bump 0.2.0          # sets Cargo.toml and Cargo.lock, commits "Version 0.2.0"
git push && git tag v0.2.0 && git push origin v0.2.0
just bump 0.2.1-dev      # start the next version
git push
```

The tag must match the version in Cargo.toml. Release notes leave out the `Version …` commits.

The workflows need a Gitea Actions runner with the `ubuntu-latest` label. They install everything else themselves.

To build the release archives locally, run `scripts/ci/setup.sh release`, which needs rustup, then `just dist`.
