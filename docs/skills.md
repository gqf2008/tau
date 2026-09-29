# Skills and project instructions

tau reads the working directory before the first request: **skills** —
directories of instructions a model loads only when a task calls for one —
and **project instructions** (`AGENTS.md`). Both are conventions you may
already have from other agents, so moving a workflow onto tau does not mean
re-authoring it.

```
<working directory>/              the directory tau was started in
  AGENTS.md                       project instructions, into the system prompt
  .agents/skills/hello/SKILL.md   a skill: one manifest line, body on demand
  .claude/skills/…                same, second root
  .goose/skills/…                 same, third root
```

## Skills

A skill is a directory holding a `SKILL.md`. Three roots are searched, in
this order, each relative to the working directory:

| root |
|------|
| `.agents/skills` |
| `.claude/skills` |
| `.goose/skills` |

Every subdirectory of a root is a skill. Its name comes from the
frontmatter, or from the directory name when the frontmatter does not name
one; the description has no fallback. The first root to define a name wins
it, and a later directory claiming the same name is ignored with a line on
stderr.

```markdown
---
name: release
description: Cut a release: changelog, version bump, tag, publish
---

# Cutting a release

1. …
```

Nothing about the skill is in the conversation except one manifest line —
`- release: Cut a release: changelog, version bump, tag, publish` — so a
body of any size costs nothing until it is wanted. The model calls
`load_skill` with the name to read the body, or with `path` to read a
supporting file that ships with the skill (`scripts/publish.sh`,
`templates/issue.md`, …).

`load_skill` is an ordinary built-in tool: `--tools load_skill` selects it,
`--no-builtin-tools` removes it, and
`tau --tools load_skill --demo -p hello` runs it for real against the skill
named `hello`. In a directory with no skills nothing registers it, so such
a run is exactly what it was before skills existed.

The skill directory is the boundary. `load_skill` reads `SKILL.md` and the
files under the skill's own directory, and refuses anything that climbs out
— a rooted path (`/etc/passwd`, `C:/Windows/win.ini`) or a `..` component —
before the filesystem is asked.

## Project instructions

The `AGENTS.md` files from the working directory **up to the repository
root** (the first directory containing `.git`) go into the system prompt,
root-first: the repository's guidance, then the subdirectory's, so the more
specific file reads as the amendment it is. A file above the repository root
is not this project's guidance and is not read.

## What the run says

Every agent run prints what it found, `none` included, so the startup log
can be read rather than guessed at:

```
[tau] skills: hello, release
[tau] project instructions: /repo/AGENTS.md
[tau] project instructions: /repo/crates/AGENTS.md
```

## The system prompt

A session's system prompt is the `--system` text, then what the directory
offered:

```
<--system text>

# Project instructions (/repo/AGENTS.md)

…

# Skills

The working directory offers the skills below. …

- hello: greets the reader in a set way
```

The manifest is advertised only to a run that has the tool to serve it: if
`--no-builtin-tools`, or a `--tools` list, left `load_skill` out, the
instructions still go in and the manifest does not — naming skills the model
cannot read would invite calls that must fail. A session whose directory
offers neither gets no system prompt at all, and the request keeps the shape
it always had.

## Discovery never fails a run

A `SKILL.md` that cannot be read, a directory with no `SKILL.md` in it, a
missing description, a name clash: each is one line on stderr and a skip.
The same posture as a torn session tail — a directory someone is halfway
through editing must not be the reason a run does not start.

## Deliberate limits

- **No user-level root.** `~/.tau/skills` does not exist; skills come from
  the working directory. (pi has `~/.pi/skills`; tau has no equivalent yet.)
- **Skills do not walk up.** The three roots are relative to the working
  directory, so run tau where the skills are. `AGENTS.md` does walk up,
  because instructions are inherited context and skills are a library.
- **The format is read, not validated.** `name` and `description` are
  `key: value` lines between `---` markers; the Agent Skills spec's name
  rules (lowercase, hyphenated, 64 characters) are not enforced. A YAML
  parser is not worth two scalars.
- **No `disable-model-invocation`.** Every discovered skill is in the
  manifest.

## Testing

`scripts/validate.sh` step 3c builds a directory with one skill and two
`AGENTS.md` files, runs tau in it, and asserts on the provider request the
loopback mock captured: the manifest and the instructions are in the system
prompt, the body is not inlined, and `load_skill` is advertised. Step 1d's
startup line covers the default tool set;
`crates/tau-core/src/skills.rs` covers discovery, the climb refusals and the
root-first order of the instructions.
