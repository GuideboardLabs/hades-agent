#!/usr/bin/env bash
# Run the workspace suite and name every test that failed.
#
# `cargo test` prints the names and exits 101. On a runner that is enough for a person reading the whole log, and
# useless for anyone reading the run summary: the job said "2 targets failed" and the names were a thousand lines
# up. GitHub renders `::error` lines as annotations, which appear on the run itself and are readable through the
# API, so a failure that is not reproducible on a workstation still says which test it was.
#
# usage: scripts/ci-test.sh [cargo test args]
set -uo pipefail

OUT=$(mktemp)
trap 'rm -f "$OUT"' EXIT

cargo test --workspace --no-fail-fast "$@" 2>&1 | tee "$OUT"
status=${PIPESTATUS[0]}

if [ "$status" -ne 0 ]; then
  {
    echo
    echo "failed tests:"
    grep -E '^test .+ \.\.\. FAILED$' "$OUT" | sed -E 's/^test (.*) \.\.\. FAILED$/\1/' | sort -u | while read -r name; do
      echo "  $name"
      printf '::error title=failing test::%s\n' "$name"
    done
    # The name says which test; the sentence after `panicked at` says why. Both go in the annotation, because
    # a run whose log needs a signed-in account otherwise reports that something failed and nothing else.
    grep -A1 "panicked at" "$OUT" | grep -v "panicked at" | grep -v "^--$" | sed 's/^[[:space:]]*//' | head -5 |
      while IFS= read -r message; do
        [ -n "$message" ] && printf '::error title=why::%s\n' "$(printf '%s' "$message" | cut -c1-280)"
      done
    grep -E '^error(\[E[0-9]+\])?:' "$OUT" | head -20 | while read -r line; do
      printf '::error title=build::%s\n' "$line"
    done
  } | tee -a "$OUT"
fi

exit "$status"
