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
set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
OUT=/mnt/ssd/hades-portable
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

mkdir -p "$OUT"
echo "building $FLAVOUR in $IMAGE; output in $OUT"

docker run --rm \
  -v "$OUT:/out" \
  -v "$ROOT:/src:ro" \
  "$IMAGE" sh /src/rust/scripts/build-inner.sh "$FLAVOUR" /out /src/rust 2>&1 | tail -6

[ -x "$OUT/$NAME" ] || { echo "no binary at $OUT/$NAME"; exit 1; }
echo "wrote: $OUT/$NAME"
case "$FLAVOUR" in
  gnu) "$HERE/assert-floor.sh" "$OUT/$NAME" --max-glibc 2.30 || exit 1 ;;
  musl) "$HERE/assert-floor.sh" "$OUT/$NAME" --static || exit 1 ;;
esac
