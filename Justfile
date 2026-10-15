# depap development commands. Run `just` to see this list.

# Local depap home for `just try`, so experiments don't touch ~/.depap
sandbox := justfile_directory() / "target/depap-home"

# List available recipes
default:
    @just --list

# Build a debug binary (target/debug/depap)
build:
    cargo build

# Build an optimized binary (target/release/depap)
release:
    cargo build --release

# Run depap with your real ~/.depap, e.g. `just run server list`
run *args:
    cargo run --quiet -- {{args}}

# Run depap with a throwaway home in target/, e.g. `just try init`
try *args:
    DEPAP_HOME={{sandbox}} cargo run --quiet -- {{args}}

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
ci: lint test

# Install depap into ~/.cargo/bin
install:
    cargo install --path .

# Remove build output
clean:
    cargo clean

# Portable release archives in dist/ (needs: scripts/ci/setup.sh release)
dist:
    scripts/build-release.sh v$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)

# Release notes for a tag, e.g. `just release-notes v0.1.0`
release-notes tag:
    scripts/release-notes.sh {{tag}}
