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

# The compiler this repository pins in rust-toolchain.toml, not whatever `stable` means on the day the
# image is built.
WANT_TOOLCHAIN=1.99.1

if command -v apk >/dev/null 2>&1; then
  apk add --no-cache curl bash build-base pkgconf ca-certificates >/dev/null 2>&1 || { echo APK_FAILED; exit 3; }
else
  apt-get update -qq >/dev/null 2>&1 || { echo APT_UPDATE_FAILED; exit 3; }
  apt-get install -y -qq curl build-essential pkg-config ca-certificates >/dev/null 2>&1 || { echo APT_INSTALL_FAILED; exit 3; }
fi

if ! command -v cargo >/dev/null 2>&1; then
  # A pipeline into `sh` cannot be checked before it runs, so the installer is a file. rustup publishes a
  # sha256 beside each rustup-init; a mismatch stops here rather than installing something else. The
  # checksum travels over the same channel as the binary, so this catches a truncated or swapped download,
  # not a compromised server: that limit is stated rather than hidden.
  case "$(uname -m)" in
    x86_64) rustup_host=x86_64 ;;
    aarch64 | arm64) rustup_host=aarch64 ;;
    *) echo "RUSTUP_NO_TARGET"; exit 3 ;;
  esac
  # The glibc rustup-init cannot exec on a musl host — Alpine has no ELF loader for it, and the shell
  # reports that as `not found`. The musl build is static and runs anywhere, so the installer that
  # matches the HOST libc is the right one on both. `apk` is the signal this file already uses for the
  # package manager; it is the musl signal too. The old `curl | sh` hid this by re-downloading in script
  # mode, which is exactly the path the checksum gate exists to close.
  if command -v apk >/dev/null 2>&1; then
    rustup_target="$rustup_host-unknown-linux-musl"
  else
    rustup_target="$rustup_host-unknown-linux-gnu"
  fi
  base="https://static.rust-lang.org/rustup/dist/$rustup_target"
  curl --proto "=https" --tlsv1.2 -sSf "$base/rustup-init" -o /tmp/rustup-init || { echo RUSTUP_FAILED; exit 3; }
  want=$(curl --proto "=https" --tlsv1.2 -sSf "$base/rustup-init.sha256" | awk '{print $1}') || {
    echo RUSTUP_CHECKSUM_MISSING; exit 3;
  }
  got=$(sha256sum /tmp/rustup-init | awk '{print $1}')
  [ -n "$want" ] && [ "$want" = "$got" ] || { echo RUSTUP_CHECKSUM_MISMATCH; exit 3; }
  chmod +x /tmp/rustup-init
  /tmp/rustup-init -y --profile minimal --default-toolchain "$WANT_TOOLCHAIN" >/dev/null || {
    echo RUSTUP_FAILED; exit 3;
  }
  rm -f /tmp/rustup-init
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
