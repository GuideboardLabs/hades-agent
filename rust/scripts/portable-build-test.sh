#!/usr/bin/env bash
# The wrapper around a container build must fail when the container fails.
#
# The regression this guards is not theoretical: the script ran Docker in a pipeline under `set -uo pipefail`,
# and `tail` returned 0, so a failed build looked green and an executable left at the output path from the
# previous run was verified, packaged and shipped. This test puts a fake Docker on PATH and an old binary at
# the output path, and asserts the three things that close it:
#
#   1. the script exits non-zero, with Docker's status,
#   2. the old artifact is gone before the build starts, and
#   3. the bind source is absolute even when `--src .` was typed.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

mkdir -p "$TMP/bin" "$TMP/src/rust/harness" "$TMP/src/rust/scripts" "$TMP/out"
printf 'version = "9.9.9"\n' >"$TMP/src/rust/harness/Cargo.toml"
printf '#!/bin/sh\n' >"$TMP/src/rust/scripts/build-inner.sh"

# An executable from "the previous build", planted where the wrapper will look.
printf '#!/bin/sh\necho "hades 9.9.8"\n' >"$TMP/out/hades-portable"
chmod +x "$TMP/out/hades-portable"

cat >"$TMP/bin/docker" <<'FAKE'
#!/bin/sh
printf '%s\n' "$@" >"${DOCKER_ARGS:?}"
exit "${FAKE_DOCKER_STATUS:-3}"
FAKE
chmod +x "$TMP/bin/docker"

fail() {
  printf 'FAIL %s\n' "$1" >&2
  exit 1
}

set +e
PATH="$TMP/bin:$PATH" DOCKER_ARGS="$TMP/args" \
  bash "$HERE/portable-build.sh" --src "$TMP/src" --out "$TMP/out" >"$TMP/out.log" 2>"$TMP/err.log"
status=$?
set -e

[ "$status" -eq 3 ] || fail "expected exit 3 from a failed docker build, got $status"
grep -q "docker build failed with status 3" "$TMP/err.log" \
  || fail "the failure did not name docker's status: $(cat "$TMP/err.log")"
# A host with a foreign-architecture image cached (this one had alpine:3.20 as linux/arm64) otherwise picks it
# and dies with exec format error. The build is for x86-64 and must say so.
grep -qx -- "--platform" "$TMP/args" || fail "the build did not pin a platform: $(cat "$TMP/args")"
grep -qx "linux/amd64" "$TMP/args" || fail "the build did not pin linux/amd64: $(cat "$TMP/args")"
[ ! -e "$TMP/out/hades-portable" ] \
  || fail "the old artifact survived a failed build"

# The bind source must be absolute even when the caller typed `--src .`.
mkdir -p "$TMP/out-relative"
set +e
(
  cd "$TMP/src"
  PATH="$TMP/bin:$PATH" DOCKER_ARGS="$TMP/args-relative" \
    bash "$HERE/portable-build.sh" --src . --out "$TMP/out-relative" >/dev/null 2>&1
)
set -e
expected_src=$(cd "$TMP/src" && pwd -P)
grep -qx -- "-v" "$TMP/args-relative" || fail "no -v in docker args: $(cat "$TMP/args-relative")"
grep -qx "$expected_src:/src:ro" "$TMP/args-relative" \
  || fail "the source bind is not absolute: $(cat "$TMP/args-relative")"

# A missing source is refused before Docker is even called.
set +e
PATH="$TMP/bin:$PATH" DOCKER_ARGS="$TMP/args-missing" \
  bash "$HERE/portable-build.sh" --src "$TMP/nope" --out "$TMP/out" >/dev/null 2>"$TMP/missing.log"
status=$?
set -e
[ "$status" -eq 2 ] || fail "a missing --src should exit 2, got $status"

echo "PASS portable-build refuses a failed build, a stale artifact, a relative source, and a missing source"
