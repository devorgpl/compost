# depap

Deploy docker-compose applications to your own servers, with shared Caddy (HTTPS) and Postgres.

depap is one CLI on your machine. It manages servers over SSH, or this machine directly, and installs nothing on them except plain files and Docker containers. See [the MVP specification](docs/SPECIFICATION_MVP.md) and the [samples](samples/README.md).

## Install

depap is a single portable binary with no dependencies. Linux builds are static and run on any distribution. It needs `ssh` on your machine; servers need Docker with compose v2.

**With the install script:** detects your system, verifies the checksum, and installs to `~/.local/bin`.

```sh
curl -fsSL https://gitrepo.dev.org.pl/dev.org.pl/depap/raw/branch/master/scripts/install.sh | sh
```

For a specific release or another directory, pass options after `sh -s --`, e.g. `... | sh -s -- --version v0.1.0 --to /usr/local/bin`.

**By hand:** download the archive for your system and `SHA256SUMS` from the [releases page](https://gitrepo.dev.org.pl/dev.org.pl/depap/releases), then:

```sh
sha256sum --ignore-missing -c SHA256SUMS
tar -xzf depap-v0.1.0-x86_64-unknown-linux-musl.tar.gz
install -m 755 depap-v0.1.0-x86_64-unknown-linux-musl/depap ~/.local/bin/depap
depap --version
```

| System | Target |
|---|---|
| Linux x86_64 | `x86_64-unknown-linux-musl` |
| Linux arm64 | `aarch64-unknown-linux-musl` |

**macOS:** there are no prebuilt binaries yet, because linking for macOS needs Apple's SDK, which only a macOS machine has. Build from source with Rust instead:

```sh
cargo install --locked --git https://gitrepo.dev.org.pl/dev.org.pl/depap --tag v0.1.0
```

To update, install again. To uninstall, delete the binary and `~/.depap`.

## Quick start

```sh
depap init
depap server define prod --address 203.0.113.10   # SSH user depapuser by default
depap connect prod
depap server state
```

Then follow [samples/README.md](samples/README.md).

## Backup and restore

```sh
depap app backup myapp-prod                       # links, sources and volumes → myapp-prod-<time>.depap.tar.gz
depap app backup myapp-prod --link main-db --format raw   # just the SQL dump
depap app restore myapp-prod-….depap.tar.gz --prefix review -f review.yaml   # a copy of the app
depap server backup --target server               # shared apps, kept in the server's backups/ folder
depap server restore prod-….depap.tar.gz          # on a new server, then `depap app restore`
```

See §10.13–§10.18 of [docs/SPECIFICATION_MVP.md](docs/SPECIFICATION_MVP.md).

## Development

Needs Rust, plus [just](https://just.systems), `rustfmt` and `clippy`.

```sh
just            # list recipes
just ci         # formatting, clippy, tests: run before committing
just try init   # run depap with a throwaway home in target/
```

## Releases

Gitea Actions ([`.gitea/workflows`](.gitea/workflows)):
- **Every branch push and pull request:** runs `just ci`.
- **A pushed `v*` tag:** tests, builds the portable binaries and publishes a release with notes generated from the commits since the previous tag.

```sh
# 1. set `version` in Cargo.toml, commit
git tag v0.1.0
git push origin v0.1.0
```

The workflows need a Gitea Actions runner with the `ubuntu-latest` label. They install everything else themselves.

To build the release archives locally, run `scripts/ci/setup.sh release`, which needs rustup, then `just dist`.
