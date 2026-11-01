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
exec bash scripts/ci-test.sh "$@"
