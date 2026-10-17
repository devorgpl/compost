#!/bin/sh
# Create (or update) a GitHub release for a tag and upload files to it.
#
#   GH_TOKEN=... scripts/github-release.sh v0.1.0 notes.md dist/*.tar.gz dist/SHA256SUMS
#
# The repository is GH_REPO (owner/name), or else the one of the current
# folder's git remote. Running it again for the same tag updates the notes and
# replaces files with the same name, so a failed release job can simply be
# re-run. Needs gh, which GitHub's runners have.
set -eu

tag="${1:?usage: github-release.sh <tag> <notes file> <files...>}"
notes="${2:?usage: github-release.sh <tag> <notes file> <files...>}"
shift 2

# A tag with a `-` (v0.2.0-rc.1) is a pre-release.
case "$tag" in *-*) prerelease=true ;; *) prerelease=false ;; esac

if gh release view "$tag" >/dev/null 2>&1; then
    gh release edit "$tag" --title "$tag" --notes-file "$notes" --prerelease="$prerelease" >/dev/null
    gh release upload "$tag" --clobber "$@"
    echo "Updated release $tag"
else
    gh release create "$tag" --verify-tag --title "$tag" --notes-file "$notes" --prerelease="$prerelease" "$@" >/dev/null
    echo "Created release $tag"
fi

gh release view "$tag" --json url --jq .url
