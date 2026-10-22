#!/usr/bin/env bash
# Run an ARM64 Hades binary, rather than compiling it and calling that support.
#
# A cross `cargo check` proves the code compiles for another architecture. It does not prove the artifact
# starts, opens SQLite, resolves its own state, fetches a pack or initialises a vault. This host has no binfmt
# registration and no root, so the runner is qemu-user, staged from a Debian package with `apt-get download` +
# `dpkg-deb -x` exactly as the cross toolchain is staged. A static musl build needs no sysroot; a glibc build
# runs with `-L` pointed at the staged arm64 libc.
#
# usage: arm64-check.sh <binary> [--require]
#
# `--require` turns "no emulator" into a failure. CI passes it; a workstation that cannot stage qemu gets a
# named skip instead of a pass it did not earn.
set -uo pipefail

BIN=${1:?usage: arm64-check.sh <binary> [--require]}
shift || true
REQUIRE=0
for arg in "$@"; do
  case "$arg" in
    --require) REQUIRE=1 ;;
    *) echo "usage: arm64-check.sh <binary> [--require]" >&2; exit 2 ;;
  esac
done

HERE=$(cd "$(dirname "$0")" && pwd)
PREFIX=${HADES_CROSS_PREFIX:-${XDG_CACHE_HOME:-$HOME/.cache}/hades-cross}

fail() {
  printf 'FAIL %s\n' "$1" >&2
  exit 1
}

[ -f "$BIN" ] || fail "no binary at $BIN"
# The pack step runs from a temporary directory, and the workflow passes a path relative to the checkout.
# Resolve once so every later invocation names the same file.
BIN=$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")
[ -f "$BIN" ] || fail "no binary at $BIN"

# An ARM64 artifact that is not ARM64 would pass every later check on this host, because it is the host's own
# architecture. Name the machine before running anything.
file -b "$BIN" | grep -qE 'ARM aarch64' || fail "$BIN is not an ARM64 binary: $(file -b "$BIN")"

QEMU=${QEMU_AARCH64:-}
if [ -z "$QEMU" ]; then
  if command -v qemu-aarch64-static >/dev/null 2>&1; then
    QEMU=$(command -v qemu-aarch64-static)
  elif [ -x "$PREFIX/qemu/usr/bin/qemu-aarch64-static" ]; then
    QEMU="$PREFIX/qemu/usr/bin/qemu-aarch64-static"
  fi
fi
if [ -z "$QEMU" ] && command -v apt-get >/dev/null 2>&1; then
  mkdir -p "$PREFIX/qemu" "$PREFIX/qemu-debs"
  if (cd "$PREFIX/qemu-debs" && apt-get download qemu-user-static >/dev/null 2>&1); then
    for deb in "$PREFIX"/qemu-debs/qemu-user-static_*.deb; do
      dpkg-deb -x "$deb" "$PREFIX/qemu" >/dev/null 2>&1 || true
    done
    [ -x "$PREFIX/qemu/usr/bin/qemu-aarch64-static" ] && QEMU="$PREFIX/qemu/usr/bin/qemu-aarch64-static"
  fi
fi
if [ -z "$QEMU" ]; then
  if [ "$REQUIRE" -eq 1 ]; then
    fail "no qemu-aarch64-static and it could not be staged, so the ARM64 artifact was not run"
  fi
  echo "SKIP no qemu-aarch64-static on this machine; $BIN was not run"
  exit 0
fi

SYSCALL_PREFIX=()
if ! file -b "$BIN" | grep -qE 'static-pie linked|statically linked'; then
  # A dynamically linked ARM64 binary needs the ARM64 loader and libc beside qemu. The build job has the
  # sysroot it just compiled against; a separate verifier starts with nothing, so stage the runtime (not the
  # headers) here rather than passing `-L` at a directory that does not exist.
  SYSROOT=""
  if [ -f "$PREFIX/bullseye/lib/ld-linux-aarch64.so.1" ]; then
    SYSROOT=$PREFIX/bullseye
  elif [ -f "$PREFIX/runtime/lib/ld-linux-aarch64.so.1" ]; then
    SYSROOT=$PREFIX/runtime
  else
    ROOT=$PREFIX/runtime
    mkdir -p "$ROOT/debs"
    BASE=https://deb.debian.org/debian
    if curl -sSL --max-time 60 "$BASE/dists/bullseye/main/binary-arm64/Packages.gz" -o "$ROOT/Packages.gz" &&
      gzip -dc "$ROOT/Packages.gz" >"$ROOT/Packages"; then
      for package in libc6 libgcc-s1; do
        file=$(awk -v want="$package" '/^Package: /{name=$2} name==want && /^Filename: /{print $2; exit}' "$ROOT/Packages")
        if [ -n "$file" ] &&
          curl -sSL --max-time 60 "$BASE/$file" -o "$ROOT/debs/$(basename "$file")" &&
          dpkg-deb -x "$ROOT/debs/$(basename "$file")" "$ROOT" >/dev/null 2>&1; then
          :
        fi
      done
    fi
    [ -f "$ROOT/lib/ld-linux-aarch64.so.1" ] && SYSROOT=$ROOT
  fi
  if [ -z "$SYSROOT" ]; then
    [ "$REQUIRE" -eq 1 ] && fail "no ARM64 runtime sysroot for $BIN, so it was not run"
    echo "SKIP no ARM64 runtime sysroot; $BIN was not run"
    exit 0
  fi
  SYSCALL_PREFIX=(-L "$SYSROOT")
fi

run() {
  "$QEMU" "${SYSCALL_PREFIX[@]}" "$@"
}

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
export HOME="$TMP/home"
export HADES_HOME="$TMP/hades"
mkdir -p "$HOME"

out=$(run "$BIN" --version) || fail "the ARM64 binary did not report a version: $out"
case "$out" in
  hades\ [0-9]*) echo "ok   version: $out" ;;
  *) fail "unexpected version output: $out" ;;
esac

out=$(run "$BIN" doctor) || fail "doctor failed under emulation: $out"
echo "$out" | grep -qi "platform" || fail "doctor did not report the platform: $out"
echo "ok   doctor ran"

out=$(run "$BIN" init --non-interactive --provider ollama-cloud --model deepseek-v4.1-flash --root "$TMP/work") \
  || fail "init failed under emulation: $out"
[ -f "$HADES_HOME/config.json" ] || fail "init returned success without writing config.json"
echo "$out" | grep -q "config:" || fail "init did not print its receipt: $out"
echo "ok   init wrote a home"

# The pack is exported from the binary's own built-in skills, so the acceptance is offline and reproducible:
# a release gate that depends on a live third-party shelf fails when GitHub does, not when Hades breaks.
(cd "$TMP" && run "$BIN" pack export plan-a-change --out "$TMP/fixture.hadepack" >/dev/null) \
  || fail "pack export failed under emulation"
[ -f "$TMP/fixture.hadepack" ] || fail "pack export wrote no archive"
first=$(run "$BIN" pack add "$TMP/fixture.hadepack" --yes) || fail "pack add failed under emulation: $first"
echo "$first" | grep -qiE "installed|added|applied" || fail "pack add did not report an install: $first"
second=$(run "$BIN" pack add "$TMP/fixture.hadepack" --yes) || fail "a second pack add failed: $second"
echo "ok   pack export/add installed offline, and a second add succeeded"

out=$(run "$BIN" audit) || fail "audit failed on the home init made: $out"
echo "ok   audit read the vault"

echo "PASS ARM64 artifact ran: version, doctor, init, pack add, audit ($BIN)"
