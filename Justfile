# compost development commands. Run `just` to see this list.

# Local compost home for `just try`, so experiments don't touch ~/.compost
sandbox := justfile_directory() / "target/compost-home"

# List available recipes
default:
    @just --list

# Build a debug binary (target/debug/compost)
build:
    cargo build

# Build an optimized binary (target/release/compost)
release:
    cargo build --release

# Run compost with your real ~/.compost, e.g. `just run server list`
run *args:
    cargo run --quiet -- {{args}}

# Run compost with a throwaway home in target/, e.g. `just try init`
try *args:
    COMPOST_HOME={{sandbox}} cargo run --quiet -- {{args}}

# Delete the `just try` home
try-reset:
    rm -rf {{sandbox}}

# Run tests; extra args filter by name, e.g. `just test merge`
test *args:
    cargo test {{args}}

# Check compile errors quickly without building a binary
check:
    cargo check --all-targets

# Format the code (needs rustfmt)
fmt:
    cargo fmt

# Check formatting and lints, warnings are errors (needs rustfmt + clippy)
lint:
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings

# Everything that should pass before committing
ci: lint test docs-check

# Regenerate docs/wiki/CLI-Reference.md from the commands' help
docs:
    scripts/docs.sh

# Fail if the CLI reference is out of date or a link in docs/wiki is broken
docs-check:
    scripts/docs.sh --check

# Publish docs/wiki to the Gitea wiki, e.g. `just wiki v0.3.0` (needs GITEA_URL, GITEA_REPO, WIKI_TOKEN)
wiki tag:
    scripts/publish-wiki.sh {{tag}}

# Install compost into ~/.cargo/bin
install:
    cargo install --path .

# Remove build output
clean:
    cargo clean

# Portable release archives in dist/ (needs: scripts/ci/setup.sh release)
dist:
    scripts/build-release.sh v$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)

# Set the version and commit it, e.g. `just bump 0.2.0` or `just bump 0.2.1-dev`
bump version:
    scripts/bump-version.sh {{version}}

# Release notes for a tag, e.g. `just release-notes v0.1.0`
release-notes tag:
    scripts/release-notes.sh {{tag}}
