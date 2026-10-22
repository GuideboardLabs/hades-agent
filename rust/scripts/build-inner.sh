#!/bin/sh
# The build that happens inside a target image. Called by portable-build.sh from a container, and directly by
# CI, where the job's own image is the target and nesting a second container is not possible.
#
# POSIX sh: the Alpine image has no bash.
#
# usage: build-inner.sh <gnu|musl> <out dir> <source dir>
set -eu

FLAVOUR=${1:?usage: build-inner.sh <gnu|musl> <out> <source>}
OUT=${2:?usage: build-inner.sh <gnu|musl> <out> <source>}
SRC=${3:?usage: build-inner.sh <gnu|musl> <out> <source>}

export DEBIAN_FRONTEND=noninteractive
export PATH=/root/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

if command -v apk >/dev/null 2>&1; then
  apk add --no-cache curl bash build-base pkgconf ca-certificates >/dev/null 2>&1 || { echo APK_FAILED; exit 3; }
else
  apt-get update -qq >/dev/null 2>&1 || { echo APT_UPDATE_FAILED; exit 3; }
  apt-get install -y -qq curl build-essential pkg-config ca-certificates >/dev/null 2>&1 || { echo APT_INSTALL_FAILED; exit 3; }
fi

if ! command -v cargo >/dev/null 2>&1; then
  curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --profile minimal --default-toolchain stable >/dev/null || { echo RUSTUP_FAILED; exit 3; }
fi

case "$FLAVOUR" in
  gnu)
    NAME=hades-portable
    cd "$SRC"
    CARGO_TARGET_DIR="$OUT/target" cargo build --release --locked -p harness 2>&1 | tail -4
    ARTIFACT="$OUT/target/release/hades"
    ;;
  musl)
    NAME=hades-musl
    rustup target add x86_64-unknown-linux-musl >/dev/null 2>&1 || { echo TARGET_FAILED; exit 3; }
    cd "$SRC"
    CARGO_TARGET_DIR="$OUT/target-musl" cargo build --release --locked -p harness \
      --target x86_64-unknown-linux-musl 2>&1 | tail -4
    ARTIFACT="$OUT/target-musl/x86_64-unknown-linux-musl/release/hades"
    ;;
  *)
    echo "unknown flavour: $FLAVOUR" >&2
    exit 2
    ;;
esac

cp "$ARTIFACT" "$OUT/$NAME"
chmod +x "$OUT/$NAME"
echo BUILD_OK "$OUT/$NAME"
