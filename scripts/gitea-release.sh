#!/bin/sh
# Create (or update) a Gitea release for a tag and upload files to it.
#
#   GITEA_URL=https://gitea.example.com GITEA_REPO=owner/compost GITEA_TOKEN=... \
#     scripts/gitea-release.sh v0.1.0 notes.md dist/*.tar.gz dist/SHA256SUMS
#
# Running it again for the same tag updates the notes and replaces files with
# the same name, so a failed release job can simply be re-run.
# Needs curl and jq. JSON goes to jq with printf, never echo: dash's echo
# turns the `\n` escapes in JSON strings into real newlines.
set -eu

tag="${1:?usage: gitea-release.sh <tag> <notes file> <files...>}"
notes="${2:?usage: gitea-release.sh <tag> <notes file> <files...>}"
shift 2
: "${GITEA_URL:?set GITEA_URL}" "${GITEA_REPO:?set GITEA_REPO}" "${GITEA_TOKEN:?set GITEA_TOKEN}"

api="${GITEA_URL%/}/api/v1/repos/$GITEA_REPO"
response="$(mktemp)"
trap 'rm -f "$response"' EXIT

# call METHOD URL [curl args...]: prints the body, fails on HTTP errors except 404.
call() {
    method="$1"
    url="$2"
    shift 2
    status="$(curl -sS -o "$response" -w '%{http_code}' -X "$method" \
        -H "Authorization: token $GITEA_TOKEN" "$@" "$url")"
    case "$status" in
    2??) cat "$response" ;;
    404) return 4 ;;
    *)
        echo "$method $url failed with HTTP $status:" >&2
        cat "$response" >&2
        exit 1
        ;;
    esac
}

# A tag with a `-` (v0.2.0-rc.1) is a pre-release.
case "$tag" in *-*) prerelease=true ;; *) prerelease=false ;; esac
payload="$(jq -n --arg tag "$tag" --rawfile body "$notes" --argjson pre "$prerelease" \
    '{tag_name: $tag, name: $tag, body: $body, draft: false, prerelease: $pre}')"

if release="$(call GET "$api/releases/tags/$tag")"; then
    id="$(printf '%s\n' "$release" | jq -r .id)"
    release="$(call PATCH "$api/releases/$id" -H 'Content-Type: application/json' -d "$payload")"
    echo "Updated release $tag"
else
    release="$(call POST "$api/releases" -H 'Content-Type: application/json' -d "$payload")"
    id="$(printf '%s\n' "$release" | jq -r .id)"
    echo "Created release $tag"
fi

for file in "$@"; do
    name="$(basename "$file")"
    old="$(printf '%s\n' "$release" | jq -r --arg n "$name" '.assets[]? | select(.name == $n) | .id')"
    if [ -n "$old" ]; then
        call DELETE "$api/releases/$id/assets/$old" >/dev/null
    fi
    call POST "$api/releases/$id/assets?name=$name" -F "attachment=@$file" >/dev/null
    echo "  uploaded $name"
done

printf '%s\n' "$release" | jq -r .html_url
