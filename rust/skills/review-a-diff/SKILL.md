---
name: review-a-diff
description: Use when reviewing a change before merge. Read the diff and its tests, check correctness, edge cases, security and scope, and report findings with file and line.
version: 1
---

# Review a diff

A review is a search for the case the author did not consider. It is not a summary of the diff.

## When to use

- A change is proposed for merge.
- You are about to call your own work done.
- A patch touches authentication, money, data deletion, or a public API.

## Steps

1. **Read the intent first.** The description, the issue or the commit message tells you what the change is supposed to do.
2. **Read the tests before the code.** The tests say what the author believes. Then read the code and see whether the tests prove it.
3. **Read the diff in order.** For each hunk ask: what input reaches this, what happens at the boundary, and what happens when it fails.
4. **Check the edges.** Empty, null, zero, one, many, duplicate, very large, concurrent, and unauthorized.
5. **Check the failure path.** A thrown error, a partial write, a retry, a timeout, and a rollback.
6. **Check scope.** Did the change alter something it did not need to touch?
7. **Report with evidence.** File, line, input, and the behavior you expect. Separate a bug from a preference.

## Script

Read the mechanical shape of the diff before reading the code:

```bash
python3 "$(hades skills path review-a-diff)/scripts/diff_brief.py" --base main --require-tests
```

It lists changed files, line counts and test files, and exits non-zero when source changed with no test.
It cannot tell you whether the tests prove the behavior.

## Rules

- A finding without a reproduction is a suspicion. Label it.
- Do not rewrite the change in the review. Point at the problem and, when useful, sketch the smallest fix.
- Security findings come first, then correctness, then maintainability, then style.
- If there are no findings, say what you checked. "Looks good" without a list is not a review.
- Review the tests for false confidence: a test that asserts a mock was called does not prove the behavior.

## Severity

- **Blocking:** wrong result, data loss, security hole, broken build.
- **Should fix:** edge case, missing test for a stated behavior, unclear failure.
- **Optional:** naming, structure, style, unless the project has a rule.
