#!/bin/sh
# Build portable depap binaries for a release tag into dist/:
#   dist/depap-<tag>-<target>.tar.gz   (depap binary + README.md)
#   dist/SHA256SUMS
#
#   scripts/build-release.sh v0.1.0
#
# The binaries are static (musl): they run on any Linux of that CPU type.
# Needs rustup, zig and cargo-zigbuild (scripts/ci/setup.sh release).
# Set TARGETS to build other targets. macOS targets cannot be built here:
# linking needs Apple's SDK, which only a macOS machine has.
set -eu

tag="${1:?usage: build-release.sh <tag, e.g. v0.1.0>}"
targets="${TARGETS:-x86_64-unknown-linux-musl aarch64-unknown-linux-musl}"

version="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)"
if [ "$tag" != "v$version" ]; then
    echo "tag $tag does not match the version in Cargo.toml ($version)" >&2
    exit 1
fi

rm -rf dist
mkdir -p dist
for target in $targets; do
    echo "== $target"
    rustup target add "$target" >/dev/null
    cargo zigbuild --release --locked --target "$target"

    name="depap-$tag-$target"
    mkdir "dist/$name"
    cp "target/$target/release/depap" README.md "dist/$name/"
    tar -C dist -czf "dist/$name.tar.gz" "$name"
    rm -r "dist/$name"
done

(cd dist && sha256sum ./*.tar.gz | sed 's| \./| |' >SHA256SUMS)
cat dist/SHA256SUMS
