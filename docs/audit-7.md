# The seventh audit, claim by claim

The reviewer's final source audit of v0.1.21. Eight findings, seven of them source-confirmed, plus two
findings the reviewer raised against the v0.1.21 boundary itself. Every one reproduced; none was closed by a
sentence.

The plan for this round was reviewed adversarially before the code changed. That review found three more
holes of the same class, which are recorded here as findings 9 to 11 because they are part of what shipped.

## 1. `HADES_HOME` bypassed the guard (P1, confirmed)

`hades_home()` returned `HADES_HOME` (or `$HOME/.hades`) untouched, and `default_state()`, the default config
path, the derived vault, the key file and the seeded skills were all built from it. `--state` was checked;
`HADES_HOME` was not. `HADES_HOME=~/NotesVault hades init` wrote a config, a vault and eleven skills into a
marked vault.

The guard now lives in `boundary::hades_home()`, called once, and `boundary::guard()` is the single path rule
used by the CLI, the TUI and the vision reader. The refusal names the path and the reason.

Tests: `a_hades_home_inside_a_vault_is_refused_before_init_writes`,
`a_hades_home_inside_another_agents_home_is_refused`, `a_clean_hades_home_still_initialises` in
`tests/cli_oracle.rs`; each spawns the built binary with its own environment.

## 2. The vault walk did not agree with the filesystem (P1, confirmed)

Three defects, not one:

- **Symlinks were not followed.** `work -> NotesVault/Research` passed, because no parent of the spelling
  `work` carries the marker.
- **The walk stopped after twelve parents.** A note deep enough in a tree was judged as if the vault were
  not there.
- **`..` was resolved before symlinks.** `work/../notes` where `work -> /Vault/sub` really means
  `/Vault/notes`; the lexical pass answered a different path in both directions.

`boundary::resolve()` now walks components the way the kernel does: `.` drops, `..` pops the **resolved**
path, a symlink is replaced by its target (`read_link`, so a dangling link is followed), and a loop is capped
at 40 hops and then treated as unresolvable, which is what the kernel answers too. The marker search goes to
`/`. Both sides of the other-agent comparison are resolved, so a symlinked `$HOME` cannot invert the rule.

`guarded()` still returns the caller's path unchanged: resolution is internal to the comparison. Canonical
paths leaking into `service install` would have rewritten `Environment=HADES_HOME=` and broken the service
tests on any host whose temp path is a symlink.

Tests: `a_symlink_into_a_vault_is_refused`, `a_dangling_symlink_into_a_vault_is_refused`,
`a_dot_dot_after_a_symlink_is_resolved_after_the_link`, `a_deep_path_below_a_vault_is_refused`,
`a_symlinked_home_does_not_invert_the_other_agent_rule`, plus the existing relative, lookalike and `..` cases.

The installer had its own copy of the rule and its own copy of the miss. `rust/install.sh` now resolves with
`readlink` and a hop cap in POSIX shell, checks every parent, and checks `--root` and `--prefix` as well as
`--home` **before** it copies a binary or creates a directory. `clean-home.sh` gained five cases: marked
vault, symlink into a vault, fourteen levels deep, root inside a vault, prefix inside a vault.

## 3. The portable builder could ship a stale binary (P1, confirmed)

`set -uo pipefail`, then `docker ... | tail -6`; the pipeline reported `tail`'s status, and the only check
afterwards was `[ -x "$OUT/$NAME" ]`, which an artifact from the previous run satisfies.

The script is now `set -euo pipefail`, removes the target artifact before the build, captures Docker's status
without a pipeline, prints the log tail on failure and exits with Docker's status. It also refuses a binary
whose `--version` does not match the manifest, because a file that exists is not a build that happened.

Running the gate found one more hole in the same script: this host had `alpine:3.20` cached as a `linux/arm64`
image, so the musl build selected it and died with `exec format error`. Both container runs now pin
`--platform linux/amd64`, and the builder test asserts the flag is present, because a build that silently
builds for the wrong architecture is the same class of failure as one that silently ships yesterday's binary.

Test: `scripts/portable-build-test.sh` plants a fake `docker` that exits 3 and an old executable at the
output path, and asserts exit 3, the named status, and that the old file is gone.

## 4. Every network retry received the original allowance (P2, confirmed)

`remaining_seconds(deadline)` was computed once per URL and passed to every address attempt. A slow failed
connect therefore spent the budget twice.

`try_addresses()` asks the clock before each attempt. It also keeps the informative failure: an expiry after
a refused connect returns the connect error, not "the deadline passed". Both are tests
(`a_second_address_is_handed_what_the_first_did_not_spend`, `expiry_after_a_failure_keeps_the_failure`).

## 5. IPv4-first ordering could hide IPv6, and `dedup` was adjacent-only (P2/P3, confirmed)

`sort_by_key(|a| !a.is_ipv4())` put every IPv4 answer first and `dedup()` removed only neighbours, so
`[A, B, A]` kept A twice and four IPv4 answers could exhaust the three attempts before the healthy IPv6
answer was tried.

`order_addresses()` deduplicates first and then interleaves families. Privacy is still judged on every raw
answer before that, so deduplication cannot hide a private second answer. Tests:
`a_repeated_answer_is_kept_once`, `one_ipv6_answer_survives_four_ipv4_answers`.

## 6. `--src` was not absolute (P3, confirmed)

A relative `--src` became a Docker named volume. Both `--src` and `--out` are resolved with `pwd -P`; a
missing source exits 2 before Docker runs. The builder test types `--src .` and asserts the recorded bind is
absolute.

## 7. The test count disagreed with itself (P3, confirmed)

README said 1,079; the 0.1.21 section said 1,081. `ci-test.sh` now sums the suite's own `test result:` lines
and writes `.test-count` **only on success**. `check-claims.sh` reads the badge, the table, the prose and the
newest release section, requires them to agree, and `--tests N` requires them to agree with the run that just
happened. The release workflow keeps the fast version check first and adds the count check after the tests.
`scripts/claim-check-test.sh` proves the checker fails on each of the four homes independently, on a wrong
run, and on a stale version.

## 8. ARM64 was a compile, not an artifact (roadmap, closed)

Two artifacts are now built and **run**, not cross-checked:

- `hades-arm64-musl`: static, built with the musl.cc aarch64 toolchain.
- `hades-arm64-gnu`: built against a staged Debian 11 arm64 sysroot (glibc 2.31), so it covers Ubuntu 20.04+
  and Debian 11+ on ARM64 rather than inheriting the build host's 2.39.

Both toolchains and qemu-user are staged with `apt-get download` + `dpkg-deb -x` under the cache; no root and
no binfmt registration. `scripts/arm64-check.sh` runs version, doctor, init, an **offline** pack export/add
(exported from the binary's own built-in skill, so the gate does not depend on GitHub), a second add, and
audit. The release workflow builds and runs both, verifies the downloaded bytes again, packages all four
artifacts with their triples, and publishes them.

The first rebuild after the reboot caught a staging chain that a green compile would have hidden, and each
step only became visible after the previous one was fixed:

- Debian's `libc6-dev` ships `libm.so` and twelve other development links as **absolute** symlinks
  (`/lib/aarch64-linux-gnu/libm.so.6`). A symlink is resolved by the host filesystem, not by `--sysroot`, so
  the linker looked for an ARM64 libm on the x86-64 host and refused. Every absolute link inside the sysroot is
  now re-pointed at its own tree.
- The cross toolchain's own `libm.so` and `libc.so` are linker **scripts** naming `/usr/aarch64-linux-gnu/...`.
  `--sysroot` does not rewrite a script's contents, so both scripts are written against the sysroot.
- The toolchain's `libgcc_s.so.1` was built against the build host's glibc 2.39 and contributed
  `pthread_*@GLIBC_2.34` references the 2.31 libc cannot satisfy; the bullseye `libgcc_s` replaces it.
- `gcc`'s link-time search path expects the cross layout `aarch64-linux-gnu/lib`, not Debian's
  `lib/aarch64-linux-gnu`, so `-lpthread` was silently dropped under `--as-needed`. The bullseye multiarch
  directories are added explicitly.

The musl half then failed on the CI runner for a different reason: `musl.cc` does not answer from GitHub's
network (the connection times out after two minutes). A release that depends on one CDN is a release that
fails when that CDN does. When no staged musl toolchain is present, the build now uses Zig's
`aarch64-linux-musl` compiler from PyPI, installed in the release job; the wrapper drops the two arguments
Zig spells differently (`--target=`, and the Cortex-A53 erratum flag its linker rejects) and disables Rust's
self-contained crt objects so they do not collide with Zig's. A machine that already has the musl.cc
toolchain staged still uses it, so the fallback adds no cost locally.

With those four, the GNU artifact links, `assert-floor.sh` reports **GLIBC_2.30** against a 2.31 ceiling, and
`arm64-check.sh --require` runs it through version, doctor, init, an offline pack export/add, a second add and
audit under qemu.

## 9. A config could name a vault the flags would refuse (found in review, fixed)

`open_memory()` opened `config.vault` directly. Every other reader guarded it, so `audit` refused a vault that
`run` would write into from a hand-edited or older `config.json`. The vault is now guarded at open.

## 10. The installer wrote before its first refusal (found in review, fixed)

`--root` was created with `mkdir -p` before `init`, and only `--home` was checked. `--root ~/Vault/notes`
therefore created a directory inside the vault before failing. Both `--root` and `--prefix` are checked before
any write, with acceptance cases.

## 11. The boundary would have failed open on a dangling link (found in review, fixed)

The first implementation canonicalised the deepest existing ancestor, which errors on a dangling symlink and
falls back to lexical parents; `work -> /Vault/not-yet` would then have passed. The component walk uses
`read_link` instead. `a_dangling_symlink_into_a_vault_is_refused` is the regression.

## What this round says about the code

The pattern from the previous three audits held: a guarantee added in one place was missing from its twin.
This time the twins were found before shipping, because the plan was reviewed against the tree first:
`--state`/`HADES_HOME`, binary guard/installer guard, `--home`/`--root`, CLI/TUI/vision, README/release notes.
The countermeasure is the same and is now mechanical: when a rule is added, grep for every other place that
reads the same value.

The reviewer's next questions are measurements, not patches: completion rate on hard tasks, model calls per
verified success, governance overhead, and survival of multi-hour interruptions. They are the next cycle.
