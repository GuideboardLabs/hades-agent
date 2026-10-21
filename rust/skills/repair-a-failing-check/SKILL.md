---
name: repair-a-failing-check
description: Use when a build, test or lint command fails. Reproduce it, read the real error, make the smallest change that addresses the cause, and rerun the same check.
version: 1
---

# Repair a failing check

The goal is a green check for the right reason. A check that passes because it was weakened is worse than a red one.

## When to use

- A test, build, type check or lint command fails.
- A change worked locally and fails in CI or on another machine.
- A failure is intermittent.

## Steps

1. **Run the command yourself.** Copy the exact command and the exact failure. Do not work from a summary.
2. **Read the first real error.** Later errors are often consequences. Find the line, the file and the assertion.
3. **Reproduce it small.** Run the single test, the single package, or the single file. A smaller loop is faster and clearer.
4. **State a cause in one sentence.** "The parser accepts an empty key because the guard runs after the split."
5. **Make the smallest change that addresses the cause.** Prefer the fix at the cause over a guard at the symptom.
6. **Rerun the same command.** Not a different one, not a subset that you expect to pass.
7. **Run the wider check once.** The narrower loop protects speed; the wider one protects the tree.

## Rules

- Never delete, skip or weaken a test to get green. If the test is wrong, fix the test and say why it was wrong.
- Never change the check to match the bug.
- Read the code before editing it. A patch against a guessed line is a second bug.
- If two attempts fail for different reasons, stop and re-read the failure. The model is not the problem yet.
- Keep a note of what you tried. A repair loop that forgets attempt one repeats it.

## Script

Run the check and record each attempt:

```bash
python3 "$(hades skills path repair-a-failing-check)/scripts/check_loop.py" --attempts 3 -- cargo test parser
```

The JSON form reports the exit code, duration and signature of every attempt, so a failure that moves is
visible before anyone calls it flaky.

## Pitfalls

- Caching. A stale build can hide the fix or repeat a failure that no longer exists.
- Environment. A command that works in your shell can fail under a service with a minimal PATH.
- Flaky tests. Rerun once to see whether the failure moves. If it moves, fix the source of nondeterminism before the assertion.
