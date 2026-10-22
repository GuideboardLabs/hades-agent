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
# The build happens in a container, so it needs no cross toolchain on the host.
#
# usage: scripts/portable-build.sh [--flavour gnu|musl] [--out DIR] [--image TAG]
set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
OUT=/mnt/ssd/hades-portable
FLAVOUR=gnu
IMAGE=ubuntu:20.04

while [ $# -gt 0 ]; do
  case "$1" in
    --flavour) FLAVOUR=${2:?--flavour needs gnu or musl}; shift 2 ;;
    --out) OUT=${2:?--out needs a directory}; shift 2 ;;
    --image) IMAGE=${2:?--image needs a tag}; shift 2 ;;
    *) echo "usage: portable-build.sh [--flavour gnu|musl] [--out DIR] [--image TAG]" >&2; exit 2 ;;
  esac
done

case "$FLAVOUR" in
  gnu)
    NAME=hades-portable
    SH=bash
    # Rustup's default toolchain, linked against the image's glibc.
    BUILD='CARGO_TARGET_DIR=/out/target cargo build --release --locked -p harness'
    ARTIFACT=/out/target/release/hades
    ;;
  musl)
    NAME=hades-musl
    # The image has no bash; the script below is POSIX on purpose.
    SH=sh
    # Alpine's own gcc is a musl compiler, so the C in rusqlite's bundle builds against the libc the binary
    # will carry.
    IMAGE=alpine:3.20
    BUILD='rustup target add x86_64-unknown-linux-musl >/dev/null && CARGO_TARGET_DIR=/out/target-musl cargo build --release --locked -p harness --target x86_64-unknown-linux-musl'
    ARTIFACT=/out/target-musl/x86_64-unknown-linux-musl/release/hades
    ;;
  *) echo "unknown flavour: $FLAVOUR" >&2; exit 2 ;;
esac

mkdir -p "$OUT"
echo "building $FLAVOUR in $IMAGE; output in $OUT"

docker run --rm -v "$OUT:/out" -v "$ROOT:/src:ro" "$IMAGE" "$SH" -c '
set -e
export DEBIAN_FRONTEND=noninteractive
export PATH=/root/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
if command -v apk >/dev/null 2>&1; then
  apk add --no-cache curl bash build-base pkgconf ca-certificates >/dev/null || { echo APK_FAILED; exit 3; }
else
  apt-get update -qq >/dev/null 2>&1 || { echo APT_UPDATE_FAILED; exit 3; }
  apt-get install -y -qq curl build-essential pkg-config ca-certificates >/dev/null || { echo APT_INSTALL_FAILED; exit 3; }
fi
curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable >/dev/null \
  || { echo RUSTUP_FAILED; exit 3; }
cd /src/rust
'"$BUILD"' 2>&1 | tail -4
cp '"$ARTIFACT"' /out/'"$NAME"' && chmod +x /out/'"$NAME"'
echo BUILD_OK
' 2>&1 | tail -8

[ -x "$OUT/$NAME" ] || { echo "no binary at $OUT/$NAME"; exit 1; }
echo "wrote: $OUT/$NAME"
case "$FLAVOUR" in
  gnu) echo "glibc floor: $(objdump -T "$OUT/$NAME" | grep -oE 'GLIBC_[0-9]+\.[0-9]+' | sort -Vu | tail -1)" ;;
  musl) echo "linkage: $(file -b "$OUT/$NAME" | grep -o 'statically linked\|dynamically linked' || echo unknown)" ;;
esac
