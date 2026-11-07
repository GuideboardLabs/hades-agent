#!/usr/bin/env bash
# Print the suite's test count without the rest of the gate.
#
# Why this exists: the README badge, the README table, the README prose and the release notes all have to
# state the count the suite actually produces, and the release workflow refuses to publish when they
# disagree. Writing that number down before running the suite means guessing, and a guess that is one high
# fails `check-claims.sh` at the END of a seven minute gate: two releases in a row were spent on exactly
# that, cutting the tag only after a second full run.
#
# So: run this, take the number, then write it into the four claim homes. The gate then passes first time.
# It skips fmt and clippy on purpose, because neither can change the count.
#
# usage: scripts/count-tests.sh
#
# A machine that needs a memory cap or a target directory off the root disk sets that in the environment:
#
#   systemd-run --user --scope -p MemoryMax=22G -- bash scripts/count-tests.sh
set -euo pipefail

cd "$(dirname "$0")/.."

# Run through the same bounded wrapper the gate and CI use, so a hanging test cannot hold this forever and
# the count is produced by the same command that produces it everywhere else.
bash scripts/ci-test.sh >/dev/null

# `ci-test.sh` writes this file only when every target passed, so a partial run cannot report a short count
# that then gets written into the docs as if it were the whole suite.
count="$(cat .test-count)"
echo "$count"
