#!/usr/bin/env bash
# check-claims.sh is itself a claim, so it gets a fixture: one good set, then each of the four count homes
# broken on its own. A checker that cannot fail is decoration.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
CHECK="$HERE/check-claims.sh"

write_fixture() {
  rm -rf "$TMP/root"
  mkdir -p "$TMP/root/rust/harness"
  printf 'version = "1.2.3"\n' >"$TMP/root/rust/harness/Cargo.toml"
  cat >"$TMP/root/README.md" <<'EOF'
<img alt="Version" src="https://img.shields.io/badge/version-1.2.3-blue">
<img alt="Tests" src="https://img.shields.io/badge/tests-4242%20passing-brightgreen">

| **Version** | 1.2.3 |
| **Tests** | 4242 passing, 13 ignored (12 live browser) |

Current state: **4242 passing, 0 failing, 13 ignored** across the workspace.
EOF
  cat >"$TMP/root/RELEASES.md" <<'EOF'
## 1.2.3

The fixture.

4242 tests, 12 live browser tests.

## 1.2.2

4040 tests.
EOF
}

fail() {
  printf 'FAIL %s\n' "$1" >&2
  exit 1
}

write_fixture
bash "$CHECK" --root "$TMP/root" --tests 4242 >/dev/null \
  || fail "the good fixture did not pass"

# Each count home, broken alone. The checker must notice every one.
for home in badge table prose release; do
  write_fixture
  case "$home" in
    badge) sed -i 's/tests-4242/tests-4241/' "$TMP/root/README.md" ;;
    table) sed -i 's/| 4242 passing/| 4241 passing/' "$TMP/root/README.md" ;;
    prose) sed -i 's/\*\*4242 passing/**4241 passing/' "$TMP/root/README.md" ;;
    release) sed -i 's/^4242 tests/4241 tests/' "$TMP/root/RELEASES.md" ;;
  esac
  set +e
  bash "$CHECK" --root "$TMP/root" --tests 4242 >/dev/null 2>&1
  code=$?
  set -e
  [ "$code" -ne 0 ] || fail "a wrong count in the $home was accepted"
done

# The suite's own number is checked too: a consistent README that claims a run other than this one fails.
write_fixture
set +e
bash "$CHECK" --root "$TMP/root" --tests 9999 >/dev/null 2>&1
code=$?
set -e
[ "$code" -ne 0 ] || fail "a count that disagrees with the run was accepted"

# And the version checks still work.
write_fixture
sed -i 's/version-1.2.3-blue/version-1.2.2-blue/' "$TMP/root/README.md"
set +e
bash "$CHECK" --root "$TMP/root" --tests 4242 >/dev/null 2>&1
code=$?
set -e
[ "$code" -ne 0 ] || fail "a stale version badge was accepted"

echo "PASS check-claims catches a wrong count in each of four places, a wrong run, and a stale version"
