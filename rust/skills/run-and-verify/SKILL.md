---
name: run-and-verify
description: Use when finishing work that has a runnable surface: a server, a CLI, a web page, a library. Covers launching it, waiting for real readiness, exercising the actual path, reading the evidence, and cleaning up.
---

# Run and verify

A change is not done because the check command passed. It is done when the thing starts, the real path works,
and the evidence is in hand. This skill is for proving behavior, not for repairing a known failure.

## When to use

- A server, API, CLI, web page, worker or library was changed and needs to be exercised.
- A check exists but does not cover the behavior the person asked for.
- A claim of "it works" is about to be made without a run.

For a failing check with a known error, use `repair-a-failing-check` instead.

## Steps

1. **Find the entrypoint.** Read the package manifest, README, Makefile or service unit. Identify the exact
   command, port, URL or binary. Do not guess a port; read it.
2. **Launch it the way a user would.** Run the documented command, in the background if it blocks, with
   stdout and stderr captured to a file. Note the process ID.
3. **Wait for readiness, not for a fixed sleep.** Poll the health endpoint, the port, the socket or a line
   in the log, with a deadline. A sleep that happens to be long enough is not a readiness check.
4. **Exercise the real path.** Send the request, click the flow, run the CLI with the arguments a user
   would use. Capture status code, body, exit code and the relevant log lines.
5. **Try one negative case.** Bad input, missing auth, an empty result, a timeout. A surface that only works
   on the happy path is not verified.
6. **Read the evidence.** Compare what came back with what the change claimed. If the output is ambiguous,
   say what is ambiguous rather than rounding it to a pass.
7. **Stop the process and restore the state.** Kill the PID you started, remove temporary files, and leave
   the workspace as it was unless the run was meant to produce an artifact.

## Check

Wait for readiness before exercising the path:

```bash
python3 "$(hades skills path run-and-verify)/scripts/wait_ready.py" --url http://127.0.0.1:8000/health \
  --pid 12345 --timeout 30 --json
```

It polls the port or URL with a deadline and fails when the process exits. The exact command and port are
named, the process was observed ready, the response or exit code is quoted, the negative case behaved as
specified, and the process is stopped. If any step could not run, say which and why.

## Pitfalls

- A green unit test quoted as proof the running system works. Tests and a live run answer different
  questions.
- Killing by a broad pattern that also matches the current shell. Kill the PID you recorded.
- Leaving a background server bound to a port, so the next run fails for an unrelated reason.
- Reporting a screenshot or a page load as a verified flow without checking the data behind it.
