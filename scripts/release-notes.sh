#!/bin/sh
# Print release notes (Markdown) for a tag: the commits since the previous
# `v*` tag, plus install instructions. Version bumps (`Version <x>`, and the
# older `Next version`) are left out.
#
#   scripts/release-notes.sh v0.2.0 [https://gitea.example.com/owner/compost]
#
# With the repository URL, the notes also link the full diff and use real
# download URLs.
set -eu

tag="${1:?usage: release-notes.sh <tag> [repository URL]}"
repo_url="${2:-}"

previous="$(git describe --tags --abbrev=0 --match 'v*' "$tag^" 2>/dev/null || true)"
if [ -n "$previous" ]; then
    range="$previous..$tag"
    echo "## Changes since $previous"
else
    range="$tag"
    echo "## Changes"
fi
echo
git log --no-merges --reverse --pretty='- %s (%h)' \
    --invert-grep --grep='^Version [0-9]' --grep='^Next version$' "$range"
if [ -n "$previous" ] && [ -n "$repo_url" ]; then
    echo
    echo "Full diff: $repo_url/compare/$previous...$tag"
fi

download="${repo_url:-<repository URL>}/releases/download/$tag"
cat <<EOF

## Install

compost is a single portable binary (static, runs on any Linux). Pick the archive for your system:

| System | Archive |
|---|---|
| Linux x86_64 | \`compost-$tag-x86_64-unknown-linux-musl.tar.gz\` |
| Linux arm64 | \`compost-$tag-aarch64-unknown-linux-musl.tar.gz\` |

On macOS, build from source: \`cargo install --locked --git ${repo_url:-<repository URL>} --tag $tag\`.

\`\`\`sh
target=x86_64-unknown-linux-musl
curl -fsSLO $download/compost-$tag-\$target.tar.gz
curl -fsSLO $download/SHA256SUMS
sha256sum --ignore-missing -c SHA256SUMS
tar -xzf compost-$tag-\$target.tar.gz
install -m 755 compost-$tag-\$target/compost ~/.local/bin/compost
\`\`\`
EOF
