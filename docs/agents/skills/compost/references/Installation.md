# Installation

compost is a single binary with no dependencies. Linux builds are static and run on any distribution. On your machine it needs `ssh` (and Docker for apps that [build locally](Applications.md#building-images), an experimental feature); servers need Docker with compose v2 (see [Home](Home.md#requirements)).

## With the install script

The script detects your system, downloads the latest release, verifies its checksum and installs to `~/.local/bin`:

```sh
curl -fsSL https://raw.githubusercontent.com/devorgpl/compost/master/scripts/install.sh | sh
```

Options go after `sh -s --`:

| Option | Meaning |
|---|---|
| `--version vX.Y.Z` | Install this release instead of the latest |
| `--to <dir>` | Install into this directory instead of `~/.local/bin` |

```sh
curl -fsSL https://raw.githubusercontent.com/devorgpl/compost/master/scripts/install.sh | sh -s -- --version vX.Y.Z --to /usr/local/bin
```

## By hand

Download the archive for your system and `SHA256SUMS` from the [releases page](https://github.com/devorgpl/compost/releases):

| System | Archive |
|---|---|
| Linux x86_64 | `compost-vX.Y.Z-x86_64-unknown-linux-musl.tar.gz` |
| Linux arm64 | `compost-vX.Y.Z-aarch64-unknown-linux-musl.tar.gz` |

```sh
sha256sum --ignore-missing -c SHA256SUMS
tar -xzf compost-vX.Y.Z-x86_64-unknown-linux-musl.tar.gz
install -m 755 compost-vX.Y.Z-x86_64-unknown-linux-musl/compost ~/.local/bin/compost
compost --version
```

Each release also has the documentation for AI agents: `llms.txt`, `llms-full.txt` and `compost-skill-vX.Y.Z.tar.gz` (see [AI agents](AI-Agents.md#from-a-release)).

## macOS

There are no prebuilt macOS binaries yet. Build from source with [Rust](https://rustup.rs):

```sh
cargo install --locked --git https://github.com/devorgpl/compost --tag vX.Y.Z
```

## Set up

```sh
compost init
```

This creates `~/.compost` with a new manager key (see [Files and folders](Files-and-Folders.md#on-your-machine)). Every other command except `server generate`, `app generate` and `agent` needs it.

To manage servers that another machine already manages, use its key:

```sh
compost init --key 0d6c2f5e-7c55-4c1e-9a5b-3f1f1f0e2a11
```

The key is in `~/.compost/compost-manager.yaml` on the other machine. A server managed by a different key shows as `FOREIGN` and compost refuses to change it.

## Update and uninstall

To update, install again. Your configuration in `~/.compost` stays.

To uninstall, delete the binary and `~/.compost`. Servers keep running; their files are in `~/.compost-server` of the server user.

Next: [Quick start](Quick-Start.md).
