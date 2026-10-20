#!/bin/sh
# Publish the compost Agent Skill to devorgpl/agent-skills, where
# `gh skill install devorgpl/agent-skills compost` finds it:
#
#   SKILLS_TOKEN=... scripts/publish-skills.sh v0.3.0
#
# skills/compost there becomes an exact copy of docs/agents/skills/compost:
# files missing here are deleted, and nothing else in that repository is
# touched. The commit is tagged compost-<tag>, which users install with
# `compost@compost-<tag>` or `--pin compost-<tag>`. agent-skills has no GitHub
# releases on purpose: without them, `gh skill install` takes the default
# branch, which only stable releases update. Running it again for a tag that
# is already published does nothing.
#
# SKILLS_TOKEN is a fine-grained personal access token with Contents read and
# write access to devorgpl/agent-skills. SKILLS_REMOTE replaces the
# repository's URL (e.g. a local path for testing); the token is then not
# needed. Needs git.
set -eu

tag="${1:?usage: publish-skills.sh <tag>}"
skill=docs/agents/skills/compost
[ -f "$skill/SKILL.md" ] || { echo "run this from the repository root ($skill/SKILL.md not found)" >&2; exit 1; }
skill="$(pwd)/$skill"
skill_tag="compost-$tag"

if [ -n "${SKILLS_REMOTE:-}" ]; then
    remote="$SKILLS_REMOTE"
    set --
else
    : "${SKILLS_TOKEN:?set SKILLS_TOKEN}"
    remote="${GITHUB_SERVER_URL:-https://github.com}/devorgpl/agent-skills.git"
    # The token goes in a header, so it is never part of a URL or printed by git.
    auth="$(printf 'x-access-token:%s' "$SKILLS_TOKEN" | base64 | tr -d '\n')"
    set -- -c "http.extraHeader=Authorization: Basic $auth"
fi

if git "$@" ls-remote --exit-code --tags "$remote" "refs/tags/$skill_tag" >/dev/null; then
    echo "agent-skills already has $skill_tag"
    exit 0
fi

repo="$(mktemp -d)"
trap 'rm -rf "$repo"' EXIT

git "$@" clone -q --depth 1 "$remote" "$repo"
rm -rf "$repo/skills/compost"
mkdir -p "$repo/skills"
cp -R "$skill" "$repo/skills/compost"

cd "$repo"
set -- -c user.name=compost-release -c user.email=compost-release@localhost "$@"
git add -A skills/compost
if git diff --cached --quiet; then
    echo "skills/compost unchanged since the last release"
else
    git "$@" commit -q -m "compost $tag"
fi
git "$@" tag -a -m "compost $tag" "$skill_tag"
git "$@" push -q --atomic origin HEAD "refs/tags/$skill_tag"
echo "Published the compost skill as $skill_tag"
