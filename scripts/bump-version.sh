#!/bin/sh
# Set the version in Cargo.toml and Cargo.lock and commit it as
# "Version <version>" (release-notes.sh leaves these commits out).
#
#   scripts/bump-version.sh 0.2.0       # before tagging v0.2.0
#   scripts/bump-version.sh 0.2.1-dev   # right after the release
set -eu

version="${1:?usage: bump-version.sh <version, e.g. 0.2.0 or 0.2.1-dev>}"
version="${version#v}"
if ! printf '%s\n' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$'; then
    echo "$version is not a version like 0.2.0 or 0.2.1-dev" >&2
    exit 1
fi
if ! git diff --quiet HEAD -- Cargo.toml Cargo.lock; then
    echo "Cargo.toml or Cargo.lock has uncommitted changes" >&2
    exit 1
fi

# The first `version = ` line is the package's own.
awk -v v="$version" '!done && /^version = "/ { $0 = "version = \"" v "\""; done = 1 } 1' \
    Cargo.toml >Cargo.toml.new
mv Cargo.toml.new Cargo.toml
# Updates the compost entry in Cargo.lock without touching dependencies
# (unlike `cargo metadata`, without needing the sources of every platform's).
cargo update --package compost --offline --quiet

git commit -q -m "Version $version" -- Cargo.toml Cargo.lock
echo "Version $version committed"
case "$version" in
    *-dev) ;;
    *) echo "Release it with: git push && git tag v$version && git push origin v$version" ;;
esac
