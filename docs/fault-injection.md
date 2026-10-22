# Fault injection, and a machine that is not this one

The fourth audit ended with a recommendation: stop reading the installer and break it while it runs. This is
that run. Two scripts, both in `rust/scripts/`, both committed so the result is repeatable:

- `fault-injection.sh <hades>` kills installs, truncates archives, fails writes, makes the state read-only,
  races two installs of one pack, and mounts a 96 KiB filesystem when the machine will allow it.
- `cross-machine.sh <tarball>` extracts the packaged release into an otherwise empty directory, gives it a
  home and a umask it has never seen, and runs the acceptance suite, a real install from the published shelf,
  and the fault cases against that binary instead of the build tree.

## What the fault cases found

Six things, four of which the static reviews did not and could not see.

**1. `pack add` was not idempotent.** Installing the same pack twice was a conflict, not a skip. The installed
copy of a skill carries the installer's own frontmatter (`status:`, `contract:`, `regression:`), so its digest
can never equal the archive's, and the plan compared exactly those two digests. The catalog is the record of
what was installed and from which archive, so the question is now asked of it: same `pack_digest` and unchanged
`live_digest` means duplicate. A command you cannot safely re-run is a command that fails under supervision.

**2. A killed replace made the state unrepairable by the command that created it.** `install_skill` moves the
existing directory to a backup, then renames the new one into place. Killed between those two calls, the
catalog still names a skill whose path is gone. The next `add` planned an install, and `apply` refused with
"already installed; uninstall it before applying again" because it looked at the catalog instead of the disk.
It now refuses only when the live path is actually there, which is the condition it was always meant to test.

**3. The loser of a race deleted the winner.** Two installs of one pack both find no existing target, both
copy to a temp directory, both try to rename it into place; one fails with `ENOTEMPTY`, and its error path
called `remove_live(target)` unconditionally. The losing process removed the directory the winning process had
just installed. The error path now removes the target only if this call placed it.

**4. An interrupted install leaked its staging file.** `kill -9` during the write leaves
`incoming-<pid>-<nanos>-<n>.hadepack` behind: 8 files after 20 kills, and no bound on how many a person
accumulating interrupted installs would collect. Install now sweeps staging files whose pid is gone; the next
install removes them.

**5. A truncated archive was reported as a race.** A file handed over without a pin failed as "the staged file
is not the bytes that were verified", which is the message for bytes that changed under the parser. The real
cause was that it is not a tar. The two failures are now told apart, and the message says which.

**6. A plan that needs a person crashed the command.** `pack add` mapped install/replace/skip and nothing else,
so a row whose default was `ask` ended in "the plan has an action this build does not know: `ask`". It now names
the item and the two commands that resolve it.

## What the fault cases did not find

- The digest check held under every kill: no truncated or swapped archive was ever installed.
- A failing write (exit 153, killed by `SIGXFSZ`) left no skill directory and the next run recovered.
- An unwritable state was refused with exit 2 and no panic.
- Two simultaneous installs of one pack end with one catalog row and one skill (after fix 3).
- A 96 KiB tmpfs was not available here — `sudo -n` is refused — so disk-full is covered by the `ulimit -f`
  case, which fails the write at the same point with `EFBIG` instead of `ENOSPC`.

## Cross-machine

`cross-machine.sh` on the packaged `hades-0.1.10-x86_64-unknown-linux-gnu.tar.gz`: 5 of 5.

- the extracted binary answers its version;
- the full clean-home acceptance suite passes from the tarball (31 checks) with a fresh home and umask 022;
- a pack installs from `raw.githubusercontent.com/GuideboardLabs/hades-agent-packs` with no build tree, no
  state, and the network as the only input;
- installing it again is a skip;
- the fault cases pass against the packaged binary (6 of 6, 1 skipped).

An earlier version of this script ran under `env -i` and failed acceptance at `systemd-analyze verify`; the
tool needs a session bus. Isolating the home and the umask is the point. Starving the whole environment tested
the harness, not the artifact.

## One thing that did not reproduce

A single run of `cargo test --workspace` reported one failing unit test in `-p harness --lib` (545 passed, 1
failed) and the name was lost to the output filter. Eleven further runs, including eight consecutive
unfiltered runs of that binary, were clean. It is recorded here rather than dropped, because a flaky test in a
suite this size is a finding whether or not it repeats on demand; a bisection with `--test-threads=1` is the
next step if it appears again.

## The shape of what remains

Every failure above is in the same family: two steps that were each correct, separated by a moment where the
process can stop. The fix in each case was to make the moment either impossible to observe (staging names,
`create_new`) or recoverable (repair by disk state, sweep by pid, do not delete what you did not place). The
execution runtime has had years of this; the installer has had a week.
