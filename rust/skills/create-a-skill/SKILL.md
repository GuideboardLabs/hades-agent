---
name: create-a-skill
description: Use when a workflow has repeated twice, or when asked to turn a procedure, checklist or prompt into a reusable skill. Covers where a Hades skill lives, the frontmatter that makes it load, and how to keep the body short enough to stay useful.
---

# Create a skill

A skill is a directory with a `SKILL.md` inside. The agent reads every skill's name and description before a
task, then loads the body only when the description matches. That is why the description is the routing
surface and the body is the procedure.

## When to use

- The same instructions have been pasted or reconstructed more than once.
- A checklist, command sequence or domain rule keeps being repeated in a prompt.
- A person asks how to add a procedure, or asks for a workflow to be made reusable.

Do not create a skill for a single fact, a one-off task, or a topic with no procedure. A skill is a task
shape, not a subject index.

## Where it lives

- A person's skill: `<state>/skills/<name>/SKILL.md`, for example `~/.hades/skills/release-a-build/SKILL.md`.
  `state` defaults to `~/.hades`.
- A promoted proposal: `hades skills promote <name>` moves it from `skills/proposed/` to
  `skills/<name>/SKILL.md` and flips `status: proposed` to `status: active`.
- A built-in: shipped in the binary and seeded into `skills/builtin/`. Changing one on disk survives upgrades;
  seeding never overwrites an existing file.

The agent reads only `SKILL.md` files in directories. A flat `skills/<name>.md` is invisible.

## Frontmatter

```yaml
---
name: release-a-build
description: Use when cutting a release: version bump, changelog, tag, and the exact check that proves the artifact.
---
```

- `name` must match the directory name, lowercase with hyphens.
- `description` must say what the skill does and when to load it. Start with "Use when ...".
- A missing description means the skill is silently never offered.
- Do not put secrets, tokens, personal paths or machine-specific state in a skill.

## The body

Write four sections, in this order, and keep the file short enough to read in one pass:

1. **When to use**: the concrete trigger, including the words a request would contain.
2. **Steps**: the procedure, numbered, with the exact command or tool call.
3. **Check**: how to prove it worked. A command, a file, an exit status. "Looks right" is not a check.
4. **Pitfalls**: the two or three ways this goes wrong, each with the fix.

Put long reference material in `references/<topic>.md` and link it from the body. The body loads when the
skill is used; a reference loads only when the procedure needs it.

## Verify before calling it done

- `hades skills list` shows it under built-in, installed or proposed, with the expected name.
- Read `<state>/skills/<name>/SKILL.md` and confirm the body is the procedure you wrote, not a placeholder.
- Read the description as if you were the router: would it fire for the request that should use it, and stay
  quiet for the requests that should not?
- Run the procedure once on a real case. A skill that has never been followed is a draft.

## Pitfalls

- A description that lists topics instead of triggers. The skill then loads too often or never.
- A body that restates the system prompt. Keep only what is specific to this procedure.
- Writing the skill before the procedure has worked twice. The second run is where the steps that matter
  show up.
