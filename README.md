# compost

Deploy docker-compose applications to your own servers, with shared Caddy (HTTPS), Postgres and Keycloak.

compost is one CLI on your machine. It manages servers over SSH, or this machine directly, and installs nothing on them except plain files and Docker containers (and, for scheduled backups, a copy of itself and a crontab line).

**Documentation:** the [wiki](https://github.com/devorgpl/compost/wiki) covers the latest release; [`docs/wiki`](docs/wiki/Home.md) in this repository is its source and already describes master. Design history is in [`docs/design`](docs/design/SPECIFICATION_MVP.md). For AI agents, [`docs/agents`](docs/agents) has `llms.txt`, `llms-full.txt` and a compost Agent Skill (`compost agent skill` installs it; see [AI agents](docs/wiki/AI-Agents.md)).

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/devorgpl/compost/master/scripts/install.sh | sh
```

Linux x86_64 and arm64, static binaries. Other options and macOS: [Installation](docs/wiki/Installation.md).

## Quick start

```sh
compost init
compost server define prod --address 203.0.113.10   # SSH user compostuser by default
compost connect prod

# Infrastructure folder: shared apps of the server
compost server generate --with caddy,postgres
compost server deploy --apply -f compost-server.yaml,compost-server.prod.yaml

# Application folder: reads docker-compose.yaml
compost app generate
cp .compost.secrets.example.yaml .compost-staging.yaml        # prefix, domains, passwords
compost app deploy --apply -f compost.yaml,.compost-staging.yaml
```

To try it on this machine first, follow [Quick start](docs/wiki/Quick-Start.md) with the [samples](samples/README.md).

## Development

Needs Rust, plus [just](https://just.systems), `rustfmt` and `clippy`.

```sh
just            # list recipes
just ci         # formatting, clippy, tests, docs check: run before committing
just try init   # run compost with a throwaway home in target/
just docs       # regenerate CLI-Reference.md and docs/agents after changing commands, options or docs/wiki
just outdated   # newer versions of the images and Go modules compost pins, with the lines that set them
```

User-visible changes update the matching page in [`docs/wiki`](docs/wiki/Home.md) in the same commit.

## Releases

GitHub Actions ([`.github/workflows`](.github/workflows)):
- **Every branch push and pull request:** runs `just ci`.
- **A pushed `v*` tag:** tests, builds the portable binaries and publishes a release with notes generated from the commits since the previous tag. For tags without a `-`, it also publishes `docs/wiki` to the repository's wiki (`scripts/publish-wiki.sh`; `just wiki v0.3.0` does the same by hand) and the Agent Skill to [devorgpl/agent-skills](https://github.com/devorgpl/agent-skills) as `compost-v0.3.0` (`scripts/publish-skills.sh`; `just skills v0.3.0`).

Between releases, master carries the next version with a `-dev` suffix (e.g. `0.2.0-dev`), so builds from master never pass for a release. To release it:

```sh
just bump 0.2.0          # sets Cargo.toml and Cargo.lock, commits "Version 0.2.0"
git push && git tag v0.2.0 && git push origin v0.2.0
just bump 0.2.1-dev      # start the next version
git push
```

The tag must match the version in Cargo.toml. Release notes leave out the `Version …` commits.

The workflows run on GitHub's `ubuntu-latest` runners and install everything else themselves. The wiki must exist (enable it and create a first page once). The job's own token can't push to a wiki, so add a fine-grained personal access token with Contents read and write access to the repository as the `WIKI_TOKEN` Actions secret.

To build the release archives locally, run `scripts/ci/setup.sh release`, which needs rustup, then `just dist`.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in compost by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
