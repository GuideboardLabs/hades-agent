#!/bin/sh
# The build that happens inside a target image. Called by portable-build.sh from a container, and directly by
# CI, where the job's own image is the target and nesting a second container is not possible.
#
# POSIX sh: the Alpine image has no bash.
#
# Two rules, both from a failed audit:
#   - a pipeline reports the status of its last command, so `cargo build | tail` can fail and say nothing. The
#     build writes to a log and its exit status is checked before anything is copied.
#   - an output directory outlives a failed build. The previous binary is removed first, and the new one must
#     report the version in the manifest, so a stale artifact cannot be packaged as this run's.
#
# usage: build-inner.sh <gnu|musl> <out dir> <source dir>
set -eu

FLAVOUR=${1:?usage: build-inner.sh <gnu|musl> <out> <source>}
OUT=${2:?usage: build-inner.sh <gnu|musl> <out> <source>}
SRC=${3:?usage: build-inner.sh <gnu|musl> <out> <source>}

export DEBIAN_FRONTEND=noninteractive
export PATH=/root/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

WANT=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$SRC/harness/Cargo.toml" | head -1)
[ -n "$WANT" ] || { echo "no version in $SRC/harness/Cargo.toml"; exit 2; }

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

mkdir -p "$OUT"
LOG=$OUT/build-$FLAVOUR.log

case "$FLAVOUR" in
  gnu)
    NAME=hades-portable
    cd "$SRC"
    CARGO_TARGET_DIR="$OUT/target" cargo build --release --locked -p harness >"$LOG" 2>&1 || {
      echo BUILD_FAILED; tail -20 "$LOG"; exit 4;
    }
    ARTIFACT="$OUT/target/release/hades"
    ;;
  musl)
    NAME=hades-musl
    rustup target add x86_64-unknown-linux-musl >/dev/null 2>&1 || { echo TARGET_FAILED; exit 3; }
    cd "$SRC"
    CARGO_TARGET_DIR="$OUT/target-musl" cargo build --release --locked -p harness \
      --target x86_64-unknown-linux-musl >"$LOG" 2>&1 || {
      echo BUILD_FAILED; tail -20 "$LOG"; exit 4;
    }
    ARTIFACT="$OUT/target-musl/x86_64-unknown-linux-musl/release/hades"
    ;;
  *)
    echo "unknown flavour: $FLAVOUR" >&2
    exit 2
    ;;
esac

tail -4 "$LOG"
[ -f "$ARTIFACT" ] || { echo "no binary at $ARTIFACT after a successful build"; exit 5; }

# Never leave the previous build in place: a copy that fails halfway must not look like this run's output.
rm -f "$OUT/$NAME"
cp "$ARTIFACT" "$OUT/$NAME"
chmod +x "$OUT/$NAME"

SAID=$("$OUT/$NAME" version 2>&1 | tail -1)
case "$SAID" in
  *"$WANT"*) ;;
  *) echo "the built binary says '$SAID', the manifest says $WANT"; exit 6 ;;
esac

echo BUILD_OK "$OUT/$NAME" "$SAID"
