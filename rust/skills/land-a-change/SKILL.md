---
name: land-a-change
description: Use when committing work, opening a pull request, or finishing a branch. Covers inspecting the diff first, staging only the change, writing the message, and verifying CI state instead of claiming it.
---

# Land a change

A commit is a claim about what changed. Make the claim true before writing it.

## When to use

- The work is done and needs to be committed.
- A branch is ready for review.
- A change is already committed and the pull request is missing or stale.

## Steps

1. **Read the state.** Run `git status`, `git diff`, and `git log --oneline -5`. Know which files changed,
   which are untracked, and what the branch is based on.
2. **Run the check.** The repository's test, lint and format commands. If there is no check, say so and run
   the closest thing: a build, a syntax check, a smoke command. Do not commit a change the check rejects.
3. **Stage explicitly.** Name the files or hunks that belong to this change. Never `git add -A` or
   `git add .` on a tree with unrelated edits. Never stage a secret, a key, a database, a build output or a
   downloaded artifact.
4. **Write the message.** Subject in the imperative mood, at most 72 characters, answering what changed.
   Body answers why it changed, what was tried and rejected, and how it was verified. No attribution
   trailers unless the person asked for them.
5. **Commit.** One logical change per commit. If the diff contains two changes, split it.
6. **Open the pull request.** Push the branch, then use `gh pr create` with a title, a short summary and a
   test plan that lists the commands actually run. Link the issue it closes. If `gh` is unavailable, print
   the compare URL for the person to open.
7. **Verify the remote state.** Read back `gh pr view --json state,mergeable,statusCheckRollup` or the
   equivalent. A push that printed no error is not a green check.

## Check

The commit exists on the branch, the working tree has no unintended leftovers, the pull request page shows
the expected base and head, and the check's real status is known. Report a red or pending check honestly.

## Pitfalls

- Committing generated files, caches or artifacts because they were already in `git status`.
- Amending or force-pushing a branch someone else is reviewing. Ask first.
- A message that describes the activity ("update files") instead of the change ("retry a check that could
  not run").
- Reporting "CI passed" from a local command. CI is the remote run; say which one you actually saw.
