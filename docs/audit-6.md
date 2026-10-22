# The sixth audit, claim by claim

The reviewer's own summary is the accurate one: the operation was fixed, not always its symmetrical operation
or its neighbouring scope. All three findings reproduce in the source, and the first one is the same bug as
last round, one function over.

## 1. Uninstall had the post-commit bug apply had lost (P1, confirmed)

Two problems in one function, and the second is the worse one:

- `append_event(...)?` ran after the files and the catalog had been changed, so a failed event reported a
  completed uninstall as a failure.
- the live items were removed **before** `write_catalog`, so a failed catalog write left rows naming directories
  that no longer existed: uninstall broken for every later command.

The order is now decide, commit the catalog, remove. A catalog that cannot be written leaves everything as it
was, which is the only direction that is recoverable. A removal that fails after the commit is a **leftover**,
reported by name in the receipt, not a failed uninstall. `UninstallReceipt` gained `left_behind` and
`event_error`, the same contract `ApplyReceipt` already had.

Tests: `pack_uninstall.rs` stages both boundaries. The catalog failure uses a debug-only seam
(`HADES_TEST_FAIL=catalog-write`) because an unprivileged process has no honest way to make that write fail;
the event failure uses a directory where `events.jsonl` belongs. The fault script gained the event case as a
process-level run.

## 2. The lock did not cover the home (P2, confirmed)

`<state>/packs/.lock` orders installs that share a state directory. Harness profiles live at
`<home>/harness/<profile>` and `install_harness` also calls `tuning::record_adoption(home, …)`, so two instances
with different states and one home were never ordered against each other.

A second lock at `<home>/harness/.lock` is taken by `apply` and `uninstall` when the pack contains a harness.
Ordering is always state then home, so the pair cannot deadlock. The test holds the home lock on one thread and
asserts a second holder waits, then takes it immediately after the first drops.

## 3. The audit-gap detector could be fooled (P2, confirmed)

`rows_without_events` matched `(pack, version)`. Uninstall a pack, reinstall the same version, lose the second
event, and the first event vouched for the second install.

Every installed row now carries an `install` id, and every apply event carries the ids it installed. The
detector matches by id. Rows written before the field existed have no id and still match the old way, which is
why the check remains useful on an existing state. The test installs once, records the event, replaces the row
with a second install id, and asserts the row is reported.

## 4. The glibc floor was inherited, not chosen (confirmed; partly fixed)

The reviewer is right that 2.39 came from the build host. The release binary is now built inside Ubuntu 20.04
with `scripts/portable-build.sh`, which pins the requirement to what that glibc provides:

```
host build      GLIBC_2.39    runs on 22.04 with a weak-version warning (looks like success)
portable build  GLIBC_2.30    runs cleanly on 20.04, 22.04, 24.04, Debian 11, Debian 12
```

The portable artifact passes the full acceptance suite (31 checks) and the fault script (10 passed, 1 skipped),
so this is not a binary that merely prints its version.

**Not achieved: 2.28.** The Debian 10 build was attempted and the environment refused to run it, so there is no
2.28 artifact and no claim of one. 2.30 covers Ubuntu 20.04+, Debian 11+, RHEL 9+ and Fedora 33+; Debian 10 and
RHEL 8 remain unsupported. The recipe is one line in the script and would work on a machine that will run it.

**Not done: musl, ARM64, NixOS, Alpine.** A static musl binary would remove the glibc question entirely, but it
also changes how libcurl is reached (dlopen does not work in a static musl binary) and would silently move every
fetch to the slower fallback. That is a design decision with a cost, not a build flag, and it is not in this
release. ARM64 has no runtime check here because the kernel cannot execute it; the cross-build still compiles
and passes its tests.

## 5. README version (P3, confirmed)

The badge and the table said 0.1.12 while the manifest said 0.1.13. Both now read from the release they ship
with.

## What this round says about the code

Three audits in a row have found the same shape: a guarantee added in one place and not in its twin. Install
got a truthful receipt, uninstall did not. One state got a lock, the home did not. Events got ids, correlation
did not. The pattern is worth naming because the countermeasure is mechanical: when a guarantee is added, grep
for the other place that does the same thing. `apply`/`uninstall`, `state`/`home`, `row`/`event`.

The next piece of work is the one the reviewer has now asked for twice, and it is not a patch: measure the agent
rather than the installer. Verified completion rate, model calls per completion, compression behaviour,
governance cost against an ungoverned loop.
