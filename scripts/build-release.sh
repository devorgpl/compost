#!/bin/sh
# Build portable compost binaries for a release tag into dist/:
#   dist/compost-<tag>-<target>.tar.gz   (compost binary, README.md, licenses)
#   dist/compost-skill-<tag>.tar.gz      (the Agent Skill folder `compost`)
#   dist/llms.txt, dist/llms-full.txt    (documentation for AI agents)
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
# One cargo run for all targets, so they build in parallel.
set --
for target in $targets; do
    rustup target add "$target" >/dev/null
    set -- "$@" --target "$target"
done
cargo zigbuild --release --locked "$@"

for target in $targets; do
    name="compost-$tag-$target"
    mkdir "dist/$name"
    cp "target/$target/release/compost" README.md LICENSE-APACHE LICENSE-MIT "dist/$name/"
    tar -C dist -czf "dist/$name.tar.gz" "$name"
    rm -r "dist/$name"
done

# Documentation for AI agents, as generated in docs/agents by `just docs`.
tar -C docs/agents/skills -czf "dist/compost-skill-$tag.tar.gz" compost
cp docs/agents/llms.txt docs/agents/llms-full.txt dist/

(cd dist && sha256sum ./*.tar.gz ./*.txt | sed 's| \./| |' >SHA256SUMS)
cat dist/SHA256SUMS
