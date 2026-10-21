#!/bin/sh
# Newer versions of what compost pins: the images and Go modules that
# `compost images` lists (each type's `bundled`). For compost developers;
# changes nothing. Needs curl and jq, no Docker.
#
#   scripts/outdated.sh        # `just outdated`
#
# - An exact tag (`crowdsecurity/crowdsec:v1.8.1`): the newest tag of the same
#   shape in its registry (`vX.Y.Z`, `X.Y.Z-alpine`, …).
# - A moving tag (`caddy:2-alpine`, `alpine:3.20`): a newer major (or minor)
#   tag of the same shape; its patch releases come by themselves.
# - A Go module: its latest release in the Go module proxy, or for a commit,
#   the newest commit on its default branch.
#
# Each newer one is printed with the source line that sets it.
set -eu

cargo build --quiet
bin=target/debug/compost
tab="$(printf '\t')"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# get URL: the body of a GET, following a registry's Bearer challenge
# (anonymous token) and `Link: …; rel="next"` pages. Prints nothing on errors.
get() {
    url="$1"
    token=""
    while [ -n "$url" ]; do
        auth=""
        [ -z "$token" ] || auth="Authorization: Bearer $token"
        code="$(curl -sS -o "$tmp/body" -D "$tmp/headers" -w '%{http_code}' ${auth:+-H "$auth"} "$url" || echo 000)"
        if [ "$code" = 401 ] && [ -z "$token" ]; then
            challenge="$(tr -d '\r' <"$tmp/headers" | sed -n 's/^[Ww][Ww][Ww]-[Aa]uthenticate: Bearer //p')"
            realm="$(printf '%s' "$challenge" | sed -n 's/.*realm="\([^"]*\)".*/\1/p')"
            service="$(printf '%s' "$challenge" | sed -n 's/.*service="\([^"]*\)".*/\1/p')"
            scope="$(printf '%s' "$challenge" | sed -n 's/.*scope="\([^"]*\)".*/\1/p')"
            [ -n "$realm" ] || return 0
            token="$(curl -fsS -G "$realm" --data-urlencode "service=$service" --data-urlencode "scope=$scope" 2>/dev/null |
                jq -r '.token // .access_token // empty')"
            [ -n "$token" ] || return 0
            continue
        fi
        [ "$code" = 200 ] || return 0
        cat "$tmp/body"
        echo
        next="$(tr -d '\r' <"$tmp/headers" | sed -n 's/^[Ll]ink: <\([^>]*\)>; rel="next".*/\1/p')"
        case "$next" in
        "") url="" ;;
        http*) url="$next" ;;
        *) url="$(printf '%s' "$url" | sed 's|^\(https://[^/]*\).*|\1|')$next" ;;
        esac
    done
}

# tags IMAGE-WITHOUT-TAG: every tag of the repository, one per line.
tags() {
    name="$1"
    first="${name%%/*}"
    case "$first" in
    *.* | *:* | localhost) registry="$first" repo="${name#*/}" ;;
    *) registry=registry-1.docker.io repo="$name" ;;
    esac
    case "$registry:$repo" in registry-1.docker.io:*/*) ;; registry-1.docker.io:*) repo="library/$repo" ;; esac
    get "https://$registry/v2/$repo/tags/list?n=1000" | jq -r '.tags[]?' 2>/dev/null
}

# shape TAG: a pattern for tags like it, digits replaced, e.g. `^v[0-9]+\.[0-9]+\.[0-9]+$`.
shape() {
    printf '%s' "$1" | sed -e 's/[.]/\\./g' -e 's/[0-9][0-9]*/[0-9]+/g' -e 's/^/^/' -e 's/$/$/'
}

# where TEXT...: the lines of code (not comments, not tests) that contain one
# of them, `file:line` each.
where() {
    : >"$tmp/where"
    for text in "$@"; do
        grep -rnF --include='*.rs' --include='Dockerfile' -- "$text" src images 2>/dev/null |
            while IFS=: read -r file line code; do
                case "$(printf '%s' "$code" | sed 's/^[[:space:]]*//')" in //* | '#'*) continue ;; esac
                tests="$(grep -n -m 1 '^#\[cfg(test)\]' "$file" | cut -d: -f1)"
                [ -z "$tests" ] || [ "$line" -lt "$tests" ] || continue
                echo "$file:$line"
            done >>"$tmp/where"
    done
    sort -u -t: -k1,1 -k2,2n "$tmp/where"
}

# released TAG LIST: whether a moving tag (`13`, `3-alpine`) has a release
# behind it, e.g. `13.1.1` (not only `13.1.1-rc`).
released() {
    number="$(printf '%s' "$1" | sed 's/^\([v0-9.]*\).*/\1/')"
    rest="${1#"$number"}"
    printf '%s\n' "$2" | grep -qE -- "^$(printf '%s' "$number" | sed 's/[.]/\\./g')(\\.[0-9]+)+$(printf '%s' "$rest" | sed 's/[.]/\\./g')\$"
}

# report REFERENCE CURRENT LATEST NOTE LOCATION...
report() {
    printf '%-60s %s -> %s%s\n' "$1" "$2" "$3" "$4"
    shift 4
    where "$@" | sed 's/^/    /'
}

# module PATH@VERSION
module() {
    path="${1%@*}" pinned="${1##*@}"
    # The module is the path or one of its parents (packages of a module aren't modules).
    mod="$path"
    while [ -n "$mod" ] && [ -z "$(get "https://proxy.golang.org/$mod/@v/list")$(get "https://proxy.golang.org/$mod/@latest")" ]; do
        case "$mod" in */*) mod="${mod%/*}" ;; *) mod="" ;; esac
    done
    if [ -z "$mod" ]; then
        echo "$1: not found in the Go module proxy" >&2
        return
    fi
    latest="$(get "https://proxy.golang.org/$mod/@latest" | jq -r '.Version // empty')"
    case "$pinned" in
    v[0-9]*)
        if [ -n "$latest" ] && [ "$latest" != "$pinned" ] &&
            [ "$(printf '%s\n%s\n' "$pinned" "$latest" | sort -V | tail -n 1)" = "$latest" ]; then
            report "$path" "$pinned" "$latest" "" "$1"
        fi
        ;;
    *)
        # A commit: the newest on the default branch.
        for branch in master main; do
            head="$(get "https://proxy.golang.org/$mod/@v/$branch.info")"
            [ -z "$head" ] || break
        done
        hash="$(printf '%s' "$head" | jq -r '.Origin.Hash // empty')"
        if [ -n "$hash" ] && [ "$hash" != "$pinned" ]; then
            day="$(printf '%s' "$head" | jq -r '.Time' | cut -c1-10)"
            report "$path" "$(printf '%s' "$pinned" | cut -c1-12)" "$(printf '%s' "$hash" | cut -c1-12)" " (commit of $day; latest release $latest)" "$1"
        fi
        ;;
    esac
}

# image REFERENCE KIND
image() {
    ref="$1" kind="$2"
    name="${ref%:*}" tag="${ref##*:}"
    case "$tag" in *'<'* | */*) return ;; esac
    case "$tag" in *[0-9]*) ;; *) return ;; esac # `stable`, `latest`: nothing to compare
    pattern="$(shape "$tag")"
    list="$(tags "$name")"
    if [ -z "$list" ]; then
        echo "$ref: cannot list the tags of $name" >&2
        return
    fi
    latest=""
    for candidate in $(printf '%s\n' "$list" | grep -E -- "$pattern" | sort -rV); do
        if [ "$kind" != "moving tag" ] || [ "$candidate" = "$tag" ] || released "$candidate" "$list"; then
            latest="$candidate"
            break
        fi
    done
    [ -n "$latest" ] && [ "$latest" != "$tag" ] || return 0
    [ "$(printf '%s\n%s\n' "$tag" "$latest" | sort -V | tail -n 1)" = "$latest" ] || return 0
    if [ "$kind" = "moving tag" ]; then
        # A major alone (`11`) comes from a default version, not a string to search.
        dotted=""
        case "$tag" in *.*) dotted="\"$tag\"" ;; esac
        report "$ref" "$tag" "$latest" " (moving tag: a newer major or minor)" "$ref" ${dotted:+"$dotted"}
    else
        report "$ref" "$tag" "$latest" "" "$ref" "\"$tag\""
    fi
}

"$bin" images --tsv | sort -u -t "$tab" -k2,2 >"$tmp/list"
# All at once, each with its own folder; printed in the order of the list.
n=0
while IFS="$tab" read -r what ref kind; do
    n=$((n + 1))
    (
        tmp="$tmp/$n"
        mkdir "$tmp"
        case "$kind" in
        built) ;;
        pinned) module "$ref" ;;
        *) image "$ref" "$kind" ;;
        esac
    ) >"$tmp/out.$n" &
    : "$what"
done <"$tmp/list"
wait
i=0
while [ "$i" -lt "$n" ]; do
    i=$((i + 1))
    cat "$tmp/out.$i"
done
newer="$(cat "$tmp"/out.* | grep -cv '^    ' || true)"

if [ "$newer" = 0 ]; then
    echo "Everything compost pins is at its newest version."
else
    echo
    echo "$newer newer. Change them in the source, then \`just docs\` (Bundled-Images.md) and \`just ci\`."
fi
