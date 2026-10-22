# The fifth audit, claim by claim

Three findings against 0.1.12. The first one reproduced on the first attempt, which makes it the most useful
review of the five: it was a real lost update, not a theoretical one.

## 1. Concurrent catalog updates lose a row (P1, confirmed and reproduced)

`apply()` read the catalog, changed its local copy, and wrote the whole file back. Two different packs
installing at the same time both wrote a catalog containing only their own row: both skills on disk, one of
them unmanaged.

Reproduced before any fix, three runs out of three, with a test that starts two installs from a barrier:

```
a catalog row was lost: ["beta"]
a catalog row was lost: ["alpha"]
a catalog row was lost: ["alpha"]
```

The fix is an advisory `flock` on `<state>/packs/.lock`, taken at the top of `apply()` and `uninstall()` and
held across plan, preflight, filesystem changes and the catalog commit. Downloads and archive parsing stay
outside it, so two installs still fetch in parallel and only the state mutation is serialised. Waiting is
bounded at 30 seconds: a holder that never finishes is an error with a name, not a command that hangs.

The same test now passes on every run, and the fault script runs the same case as two separate processes (the
auditor's actual scenario), not just two threads.

## 2. `temp_beside` collides within one process (P2, confirmed by inspection)

The name was `.<target>.<label>-<pid>`: two threads installing one skill computed the same path, and the second
call's `remove_dir_all` deleted the first one's half-copied directory. It now carries a counter and the clock and
is created with `create_dir`, which also means a path somebody else prepared is an error instead of a place to
write through. The test starts two staging directories for the same target and asserts they differ.

The state lock makes this harder to reach through `pack apply`, which is why the fix is defensive rather than
demonstrable end to end.

## 3. An install can succeed and report failure (P2, confirmed)

`append_event` ran after the catalog commit and its error was returned with `?`. A disk that filled between the
rename and the event would have produced "failed" for an install that was already on disk.

`ApplyReceipt` now carries `event_error`, and both the command and the receipt text say the install is
committed and the event was not written. `hades doctor` gained a `packs` check: every catalog row must have an
`apply` event naming its pack and version, and a row without one is reported by name.

The test installs with a directory where `events.jsonl` belongs, asserts the committed warning, then removes the
obstruction and asserts the row is named as unrecorded.

## 4. Documentation drift (P3, confirmed)

The README advertised 1,063 tests and 13 ignored; the releases said 1,072 and 12. Both were wrong in different
directions. The counts are now taken from one run of the suite and written once: **1076 passing, 13 ignored**,
of which 12 are the live browser tests.

## 5. Portability (P3, confirmed as a real limitation)

The auditor called the Ubuntu 22.04 and ARM64 checks a coverage recommendation, not a defect. Checked, and it is
a defect of the release, not the code: the binary requires **GLIBC_2.39**, so on 22.04 the loader emits a weak
version warning and carries on, which is worse than refusing, because it looks like success.

`scripts/portability.sh` now measures the claim instead of asserting it: it reads the highest GLIBC symbol from
the binary, runs it in `ubuntu:24.04` (must be clean), in `ubuntu:22.04` (must be below the floor and say so),
and reports ARM64 as untested rather than passed when the kernel has no binfmt handler. The README names the
floor.

Building on the oldest supported image is the real fix, and it is a release-engineering task, not a code change:
a second binary with a different provenance for one release is worse than a named floor.

## What I did not do

- **No ARM64 runtime check.** This kernel cannot execute arm64 containers, and a cross-compiled binary that
  has never run is not evidence. The cross-check target still compiles and passes its tests.
- **No CI matrix.** The scripts are committed, but they run on this machine. Wiring them to a matrix is the
  next step if the release cadence continues.
- **No new distribution features.** The auditor's advice was to stop adding them and start measuring the agent
  itself: verified completion rate, model calls per completion, compression behaviour, governance cost. That is
  the next piece of work, and it is not a patch release.
