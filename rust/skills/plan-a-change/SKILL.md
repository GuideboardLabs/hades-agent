---
name: plan-a-change
description: Use before a change that touches more than one file or more than one concern. Write a short plan with the files, the check and the risks, then change one thing at a time.
version: 1
---

# Plan a change

A plan is a cheap way to find the wrong assumption before it costs an edit. Skip it for a one-line fix. Write it for anything that crosses files, touches a public surface, or has more than one plausible approach.

## When to use

- The change touches two or more files, or two or more concerns.
- You are unsure which of two approaches is right.
- The work has a check that could pass for the wrong reason.

## Steps

1. **Restate the goal in one sentence.** If you cannot, the goal is not ready.
2. **Name the check.** Write the exact command that will decide whether this worked. If there is no check, say so and propose one.
3. **List the files you expect to touch.** Read each one before editing it. A file you have not read is a guess.
4. **Write the smallest sequence of steps.** One step should leave the tree buildable, or say why it cannot.
5. **Name the risks.** Call out data loss, migration, compatibility, secrets, and anything that is hard to undo.
6. **Name what you will not do.** Scope is a list of exclusions as much as inclusions.

## Rules

- A plan is not a promise. If step 2 teaches you something that contradicts step 1, revise the plan and say why.
- Prefer reversible steps. A rename, a flag, or a parallel path is easier to abandon than a schema change.
- Do not plan around a check you have not run once. Run it on the current tree to see the baseline.
- Keep the plan where the work is: a note, a file, or the session. If it only exists in your head, it cannot be reviewed.

## Pitfalls

- A plan that lists every file in the repo is a survey, not a plan.
- A plan with no check is a hope.
- A plan that assumes the test suite is green without running it is a guess.
