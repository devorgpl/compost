# AI agents

compost's documentation also comes in forms that AI coding agents (Claude Code, Codex and others) read well. All of them are made from these wiki pages, so they say the same, and each release carries the ones of its version.

| File | Content |
|---|---|
| `llms.txt` | What compost is and a list of these pages with links, as [llmstxt.org](https://llmstxt.org) describes |
| `llms-full.txt` | Every page in one file, to give an agent the whole documentation at once |
| Agent Skill `compost` | `SKILL.md` (concepts, workflows, safety rules, which page to read for what) and these pages in `references/` |

The skill tells the agent to run `--dry-run` before deploying, and to ask you before `--apply`, removals, restores and changes to stored secrets. It never adds `--yes` on its own, and never commits or prints overlays with passwords.

## From the compost binary

The installed compost carries the files of its own version, and needs no compost home or server for them:

```sh
compost agent skill                          # writes .claude/skills/compost/ in the current folder
compost agent skill --dir ~/.claude/skills   # for all your projects
compost agent skill --dir .agents/skills     # for agents that read skills from there
compost agent llms                           # print llms.txt
compost agent llms --full                    # print llms-full.txt
```

`agent skill` replaces the `compost` skill in that folder, so run it again after updating compost. It refuses to overwrite a `compost/SKILL.md` that isn't compost's skill. You can commit the skill to your app or infrastructure repository so everyone's agent gets it.

## From a release

Every [release](https://gitrepo.dev.org.pl/dev.org.pl/compost/releases) has `llms.txt`, `llms-full.txt` and `compost-skill-vX.Y.Z.tar.gz`, listed in its `SHA256SUMS`. The archive holds the `compost` skill folder:

```sh
tar -xzf compost-skill-vX.Y.Z.tar.gz -C ~/.claude/skills
```

## From the repository

[`docs/agents`](https://gitrepo.dev.org.pl/dev.org.pl/compost/src/branch/master/docs/agents) holds the same files for master, which may describe features not released yet. An agent can fetch [`llms.txt`](https://gitrepo.dev.org.pl/dev.org.pl/compost/raw/branch/master/docs/agents/llms.txt) or [`llms-full.txt`](https://gitrepo.dev.org.pl/dev.org.pl/compost/raw/branch/master/docs/agents/llms-full.txt) from there.
