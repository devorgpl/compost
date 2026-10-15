#!/bin/sh
# Install the portable depap binary from a Gitea release.
#
#   curl -fsSL https://gitrepo.dev.org.pl/dev.org.pl/depap/raw/branch/master/scripts/install.sh | sh
#   ... | sh -s -- --version v0.1.0 --to /usr/local/bin
#
# Options:
#   --url URL        Gitea server (default: https://gitrepo.dev.org.pl)
#   --repo OWNER/NAME
#                    repository (default: dev.org.pl/depap)
#   --version TAG    e.g. v0.1.0 (default: the latest release)
#   --to DIR         install directory (default: ~/.local/bin)
#
# Downloads the archive for this system, checks it against SHA256SUMS and
# copies the single `depap` binary. Nothing else is installed.
set -eu

url="https://gitrepo.dev.org.pl" repo="dev.org.pl/depap" version="" to="$HOME/.local/bin"
while [ $# -gt 0 ]; do
    case "$1" in
    --url) url="${2%/}"; shift 2 ;;
    --repo) repo="$2"; shift 2 ;;
    --version) version="$2"; shift 2 ;;
    --to) to="$2"; shift 2 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done
if [ -z "$url" ] || [ -z "$repo" ]; then
    echo "usage: install.sh --url URL --repo OWNER/NAME [--version TAG] [--to DIR]" >&2
    exit 2
fi

case "$(uname -s)-$(uname -m)" in
Linux-x86_64) target=x86_64-unknown-linux-musl ;;
Linux-aarch64 | Linux-arm64) target=aarch64-unknown-linux-musl ;;
Darwin-*)
    echo "no prebuilt depap for macOS; build it with: cargo install --locked --git $url/$repo" >&2
    exit 1
    ;;
*) echo "no depap build for $(uname -s) $(uname -m)" >&2; exit 1 ;;
esac

if [ -z "$version" ]; then
    version="$(curl -fsSL "$url/api/v1/repos/$repo/releases/latest" |
        sed -n 's/.*"tag_name":"\([^"]*\)".*/\1/p')"
    [ -n "$version" ] || { echo "cannot find the latest release of $repo" >&2; exit 1; }
fi

name="depap-$version-$target"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
echo "Downloading $name"
curl -fsSL -o "$tmp/$name.tar.gz" "$url/$repo/releases/download/$version/$name.tar.gz"
curl -fsSL -o "$tmp/SHA256SUMS" "$url/$repo/releases/download/$version/SHA256SUMS"

expected="$(grep " $name.tar.gz\$" "$tmp/SHA256SUMS" | cut -d ' ' -f 1)"
if command -v sha256sum >/dev/null; then
    actual="$(sha256sum "$tmp/$name.tar.gz" | cut -d ' ' -f 1)"
else
    actual="$(shasum -a 256 "$tmp/$name.tar.gz" | cut -d ' ' -f 1)"
fi
if [ -z "$expected" ] || [ "$expected" != "$actual" ]; then
    echo "checksum mismatch for $name.tar.gz" >&2
    exit 1
fi

tar -xzf "$tmp/$name.tar.gz" -C "$tmp"
mkdir -p "$to"
cp "$tmp/$name/depap" "$to/depap"
chmod 755 "$to/depap"
echo "Installed $("$to/depap" --version) to $to/depap"
case ":$PATH:" in
*":$to:"*) ;;
*) echo "Add $to to your PATH to run it as \`depap\`." ;;
esac
