#!/bin/sh
# Prepare a Debian/Ubuntu CI runner for building depap.
#
#   scripts/ci/setup.sh            Rust (from rust-toolchain.toml), just, test tools
#   scripts/ci/setup.sh release    also zig and cargo-zigbuild for portable builds
#
# Safe to run again; installed tools are skipped.
set -eu

JUST_VERSION=1.45.0
ZIG_VERSION=0.15.2
ZIGBUILD_VERSION=0.23.4

as_root() {
    if [ "$(id -u)" = 0 ]; then "$@"; else sudo "$@"; fi
}

# System packages: tests need ssh and sh tools, cargo needs a C linker.
packages=""
command -v curl >/dev/null || packages="$packages curl ca-certificates"
command -v git >/dev/null || packages="$packages git"
command -v ssh >/dev/null || packages="$packages openssh-client"
command -v jq >/dev/null || packages="$packages jq"
command -v xz >/dev/null || packages="$packages xz-utils"
command -v cc >/dev/null || packages="$packages build-essential"
if [ -n "$packages" ]; then
    as_root apt-get update -qq
    # shellcheck disable=SC2086 # word splitting is intended
    as_root env DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends $packages
fi

# Rust: the toolchain from rust-toolchain.toml.
if ! command -v rustup >/dev/null; then
    curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none
fi
export PATH="$HOME/.cargo/bin:$PATH"
rustup toolchain install
# Later workflow steps find cargo, just and zig here.
if [ -n "${GITHUB_PATH:-}" ]; then
    echo "$HOME/.cargo/bin" >>"$GITHUB_PATH"
fi

if ! command -v just >/dev/null; then
    curl -sSf https://just.systems/install.sh | bash -s -- --tag "$JUST_VERSION" --to "$HOME/.cargo/bin"
fi

if [ "${1:-}" = release ]; then
    if ! command -v zig >/dev/null; then
        arch="$(uname -m)"
        index="$(curl -sSf https://ziglang.org/download/index.json)"
        url="$(printf '%s\n' "$index" | jq -r --arg v "$ZIG_VERSION" --arg a "$arch-linux" '.[$v][$a].tarball')"
        sum="$(printf '%s\n' "$index" | jq -r --arg v "$ZIG_VERSION" --arg a "$arch-linux" '.[$v][$a].shasum')"
        curl -sSfL -o /tmp/zig.tar.xz "$url"
        echo "$sum  /tmp/zig.tar.xz" | sha256sum -c -
        mkdir -p "$HOME/.local/zig"
        tar -xJf /tmp/zig.tar.xz -C "$HOME/.local/zig" --strip-components=1
        ln -sf "$HOME/.local/zig/zig" "$HOME/.cargo/bin/zig"
        rm /tmp/zig.tar.xz
    fi
    command -v cargo-zigbuild >/dev/null ||
        cargo install --locked "cargo-zigbuild@$ZIGBUILD_VERSION"
fi

rustc --version
just --version
