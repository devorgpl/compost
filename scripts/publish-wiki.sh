#!/bin/sh
# Publish docs/wiki to the repository's Gitea wiki, as the documentation of a
# release:
#
#   GITEA_URL=https://gitea.example.com GITEA_REPO=owner/compost WIKI_TOKEN=... \
#     scripts/publish-wiki.sh v0.3.0
#
# The wiki becomes an exact copy of docs/wiki: pages missing there are deleted,
# so edit the pages in this repository, not in the wiki. Links to `Page.md` are
# rewritten to `Page`, and _Footer.md names the release. Running it again with
# nothing changed makes no commit.
#
# WIKI_REMOTE replaces the wiki's URL (e.g. a local path for testing); the
# token is then not needed. Needs git.
set -eu

tag="${1:?usage: publish-wiki.sh <tag>}"
pages=docs/wiki
[ -f "$pages/Home.md" ] || { echo "run this from the repository root ($pages/Home.md not found)" >&2; exit 1; }

if [ -n "${WIKI_REMOTE:-}" ]; then
    remote="$WIKI_REMOTE"
    set --
else
    : "${GITEA_URL:?set GITEA_URL}" "${GITEA_REPO:?set GITEA_REPO}" "${WIKI_TOKEN:?set WIKI_TOKEN}"
    remote="${GITEA_URL%/}/$GITEA_REPO.wiki.git"
    # The token goes in a header, so it is never part of a URL or printed by git.
    auth="$(printf 'compost:%s' "$WIKI_TOKEN" | base64 | tr -d '\n')"
    set -- -c "http.extraHeader=Authorization: Basic $auth"
fi

wiki="$(mktemp -d)"
trap 'rm -rf "$wiki"' EXIT

if ! git "$@" clone -q --depth 1 "$remote" "$wiki" 2>"$wiki.err"; then
    cat "$wiki.err" >&2
    rm -f "$wiki.err"
    echo "Cannot clone the wiki. Enable it in the repository settings and create any first page, so its repository exists." >&2
    exit 1
fi
rm -f "$wiki.err"

find "$wiki" -maxdepth 1 -name '*.md' -exec rm -f {} +
for page in "$pages"/*.md; do
    # Page links: `](Some-Page.md)` and `](Some-Page.md#part)` → `](Some-Page)`, `](Some-Page#part)`.
    sed -E 's/\]\(([A-Za-z0-9_-]+)\.md(#[^)]*)?\)/](\1\2)/g' "$page" >"$wiki/$(basename "$page")"
done
repo_url="${GITEA_URL:+${GITEA_URL%/}/${GITEA_REPO:-}}"
cat >"$wiki/_Footer.md" <<EOF
Documentation for compost $tag${repo_url:+ · [releases]($repo_url/releases)} · Generated from \`docs/wiki\` in the repository: edit it there, changes made here are overwritten by the next release.
EOF

cd "$wiki"
git add -A
if git diff --cached --quiet; then
    echo "Wiki already up to date for $tag"
    exit 0
fi
git -c user.name=compost-release -c user.email=compost-release@localhost \
    commit -q -m "Documentation for $tag"
git "$@" push -q origin HEAD
echo "Published the wiki for $tag"
