#!/usr/bin/env bash
# Build the release binary against an older glibc than this machine has, or as a static musl binary.
#
# The binary linked on a modern host asks for GLIBC_2.39 and therefore refuses the previous LTS with a
# weak-version warning that does not stop it from running, which is worse than refusing. Building inside the
# oldest image we can get a toolchain into pins the requirement instead of inheriting it: Ubuntu 20.04 has glibc
# 2.31, and the resulting binary asks for no more than 2.30, so it runs on 20.04 and every later release.
#
# The musl flavour goes further: a static binary asks the host for nothing at all, which is what Alpine and
# NixOS need. The cost is measured rather than assumed: a static musl process cannot dlopen libcurl, so fetches
# go through the `curl` executable, and `hades doctor` says which transport is live on the machine it runs on.
#
# The work itself lives in scripts/build-inner.sh, which CI runs directly too (a container job cannot nest a
# second container). This file is the container, the arguments, and the assertion on the result.
#
# usage: scripts/portable-build.sh [--flavour gnu|musl] [--out DIR] [--image TAG] [--src DIR]
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
OUT=""
FLAVOUR=gnu
IMAGE=""

while [ $# -gt 0 ]; do
  case "$1" in
    --flavour) FLAVOUR=${2:?--flavour needs gnu or musl}; shift 2 ;;
    --out) OUT=${2:?--out needs a directory}; shift 2 ;;
    --image) IMAGE=${2:?--image needs a tag}; shift 2 ;;
    --src) ROOT=${2:?--src needs a repository root}; shift 2 ;;
    *) echo "usage: portable-build.sh [--flavour gnu|musl] [--out DIR] [--image TAG] [--src DIR]" >&2; exit 2 ;;
  esac
done

case "$FLAVOUR" in
  gnu) IMAGE=${IMAGE:-ubuntu:20.04}; NAME=hades-portable ;;
  musl) IMAGE=${IMAGE:-alpine:3.20}; NAME=hades-musl ;;
  *) echo "unknown flavour: $FLAVOUR" >&2; exit 2 ;;
esac

# Docker only accepts absolute bind sources. `--out dist` and `--src .` are the natural things to type in a
# checkout; a relative one becomes a *named volume*, so the build succeeds somewhere else and the checkout
# stays empty. Resolve both before they reach `-v`, and refuse a source that is not a directory.
[ -n "$OUT" ] || OUT=$ROOT/dist
[ -d "$ROOT" ] || { echo "no source directory: $ROOT" >&2; exit 2; }
ROOT=$(cd "$ROOT" && pwd -P)
mkdir -p "$OUT"
OUT=$(cd "$OUT" && pwd -P)
echo "building $FLAVOUR in $IMAGE; output in $OUT"

# An output directory outlives a failed build. Remove the artifact this run is about to produce before the
# build starts, so the check afterwards can only ever see this build's bytes.
rm -f "$OUT/$NAME"

LOG=$(mktemp)
trap 'rm -f "$LOG"' EXIT
status=0
docker run --rm --platform linux/amd64 \
  -v "$OUT:/out" \
  -v "$ROOT:/src:ro" \
  "$IMAGE" sh /src/rust/scripts/build-inner.sh "$FLAVOUR" /out /src/rust >"$LOG" 2>&1 || status=$?

if [ "$status" -ne 0 ]; then
  # Without this the pipeline's status was `tail`'s, and an old executable at the output path satisfied the
  # check below. The log is tailed for a reader; the status is Docker's, and the exit is this script's.
  tail -40 "$LOG" >&2 || true
  echo "docker build failed with status $status; no artifact was written" >&2
  exit "$status"
fi
tail -6 "$LOG"

# The container runs as root, so the artifact belongs to root. Give it back: the person who ran the build owns
# the binary they are about to package, chmod and test.
if [ "$(stat -c %u "$OUT/$NAME" 2>/dev/null || echo 0)" != "$(id -u)" ]; then
  docker run --rm --platform linux/amd64 -v "$OUT:/out" alpine:3.20 chown "$(id -u):$(id -g)" "/out/$NAME" >/dev/null 2>&1 || true
fi

[ -x "$OUT/$NAME" ] || { echo "no binary at $OUT/$NAME"; exit 1; }

# A file that exists is not a build that happened. The binary must name the version this checkout declares,
# or the artifact is from another revision and every later check would agree with it.
expected=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$ROOT/rust/harness/Cargo.toml" | head -1)
[ -n "$expected" ] || { echo "no version in $ROOT/rust/harness/Cargo.toml" >&2; exit 1; }
reported=$("$OUT/$NAME" --version | awk '{print $2}')
[ "$reported" = "$expected" ] || {
  echo "built binary reports $reported, manifest says $expected" >&2
  exit 1
}

echo "wrote: $OUT/$NAME (version $reported)"
case "$FLAVOUR" in
  gnu) "$HERE/assert-floor.sh" "$OUT/$NAME" --max-glibc 2.30 || exit 1 ;;
  musl) "$HERE/assert-floor.sh" "$OUT/$NAME" --static || exit 1 ;;
esac
