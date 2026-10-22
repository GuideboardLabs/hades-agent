#!/usr/bin/env bash
# The numbers a release says it is, everywhere it says them.
#
# Twice an audit found the README advertising the previous version while the manifest had moved. The same
# drift happened to the test count: README said 1,079 while the newest release section said 1,081. A person
# reading the repository cannot see that; a grep can.
#
# The count has four homes (badge, table, prose, newest release section). They must agree with each other, and
# `--tests N` makes them agree with a suite that actually ran. CI passes the number `ci-test.sh` produced, so
# a release note cannot claim a run that did not happen.
#
# usage: scripts/check-claims.sh [--root DIR] [--tests N]
set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
EXPECTED_TESTS=""

while [ $# -gt 0 ]; do
  case "$1" in
    --root) ROOT=${2:?--root needs a directory}; shift 2 ;;
    --tests) EXPECTED_TESTS=${2:?--tests needs a number}; shift 2 ;;
    *) echo "usage: check-claims.sh [--root DIR] [--tests N]" >&2; exit 2 ;;
  esac
done

status=0
fail() {
  echo "FAIL $1"
  status=1
}

version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$ROOT/rust/harness/Cargo.toml" | head -1)
[ -n "$version" ] || { echo "FAIL no version in rust/harness/Cargo.toml"; exit 1; }
echo "manifest version: $version"

for pattern in "version-$version-blue" "| **Version** | $version |"; do
  if grep -qF "$pattern" "$ROOT/README.md"; then
    echo "ok   README says $version"
  else
    fail "README has no '$pattern'"
  fi
done

# The release notes' newest section must be the same version, or the notes describe a build nobody has.
newest=$(grep -m1 '^## ' "$ROOT/RELEASES.md" | sed 's/^## //')
if [ "$newest" = "$version" ]; then
  echo "ok   RELEASES.md newest section is $version"
else
  fail "RELEASES.md newest section is '$newest', manifest is $version"
fi

# --- the test count -----------------------------------------------------------------------------------
badge=$(sed -nE 's/.*badge\/tests-([0-9]+)%20passing.*/\1/p' "$ROOT/README.md" | head -1)
table=$(sed -nE 's/^\| \*\*Tests\*\* \| ([0-9]+) passing.*/\1/p' "$ROOT/README.md" | head -1)
prose=$(grep -oE '\*\*[0-9]+ passing' "$ROOT/README.md" | head -1 | tr -cd '0-9')
notes=$(awk '/^## /{n++} n==1{print}' "$ROOT/RELEASES.md" | grep -oE '[0-9]+ tests' | head -1 | cut -d' ' -f1)

for label in badge table prose notes; do
  eval "value=\$$label"
  [ -n "$value" ] || fail "no test count in $label"
done

counts="$badge $table $prose $notes"
unique=$(printf '%s\n' $counts | sort -u | wc -l)
if [ "$unique" -eq 1 ]; then
  echo "ok   test count agrees in four places: $badge"
else
  fail "test counts disagree: badge=$badge table=$table prose=$prose release=$notes"
fi

if [ -n "$EXPECTED_TESTS" ]; then
  if [ "$badge" = "$EXPECTED_TESTS" ] && [ "$table" = "$EXPECTED_TESTS" ] &&
    [ "$prose" = "$EXPECTED_TESTS" ] && [ "$notes" = "$EXPECTED_TESTS" ]; then
    echo "ok   the suite that just ran reported the claimed count: $EXPECTED_TESTS"
  else
    fail "the suite reported $EXPECTED_TESTS; README badge=$badge table=$table prose=$prose release=$notes"
  fi
fi

exit "$status"
