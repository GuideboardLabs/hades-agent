#!/usr/bin/env bash
# The whole gate, in the order CI runs it, for a person at a terminal.
#
# `cargo test --workspace` has no deadline of its own. Cargo has no per-test timeout, so a test that
# parks holds the command until something outside kills it, and a hang prints no name: the run is a
# terminal that never comes back rather than a failure. The suite is therefore run through
# `scripts/ci-test.sh`, whose deadline is **there** (line 20: 1800 s, `HADES_TEST_TIMEOUT` overrides
# it) and not in this wrapper, so the bound a person gets locally is the same one the runner gets.
#
# usage: scripts/gate.sh [cargo test args]
#
# A machine that needs a memory cap or a target directory off the root disk sets that in the
# environment before calling this, because neither belongs in a tracked file:
#
#   systemd-run --user --scope -p MemoryMax=22G -- bash scripts/gate.sh
set -euo pipefail

cd "$(dirname "$0")/.."

cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
bash scripts/ci-test.sh "$@"

# The count the suite just produced is the count the README and the release notes have to claim, and the
# release workflow refuses to publish when they disagree. A person who runs the gate and then cuts a tag
# should learn that here: the alternative is discovering it in CI, after the tag exists. `ci-test.sh` writes
# `.test-count` only when every target passed, and `set -e` above means a failed run never reaches this line.
#
# A filtered run is excluded on purpose. `scripts/gate.sh --lib` runs a subset, so its total is not the suite's
# total, and checking the docs against it would fail on a correct tree.
if [ "$#" -gt 0 ]; then
  echo "claim check: skipped, because this run was filtered ($*) and its count is not the suite's count" >&2
else
  bash scripts/check-claims.sh --tests "$(cat .test-count)" --ignored "$(cat .test-ignored)"
  # The measurement table in the release notes is compared against the measurement that was saved when it was
  # taken. This is the rule the 0.6.2 defect needed: its notes claimed a cleanliness figure the binary never
  # printed, and nothing compared the two. Offline: two files, no network, deterministic.
  notes_bin="${CARGO_TARGET_DIR:-target}/release/hades"
  if [ -x "$notes_bin" ]; then
    "$notes_bin" read-eval --check-notes ../RELEASES.md harness/tests/fixtures/read-eval-baseline.json ||
      { echo "FAIL the release notes disagree with the saved measurement" >&2; exit 1; }
  fi
fi
