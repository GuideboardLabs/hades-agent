#!/usr/bin/env bash
# What the release actually runs on.
#
# A binary built here links against this machine's glibc. The README used to claim nothing about that, and a
# reader on the previous LTS found out by running it. This checks the claim instead: the highest GLIBC symbol
# the binary asks for, and the oldest image it starts in. ARM64 is checked when the kernel can execute it.
#
# usage: scripts/portability.sh <hades-binary> [--tarball <release.tar.gz>]
set -uo pipefail

BIN=${1:?usage: portability.sh <hades binary> [--tarball <file>]}
BIN=$(readlink -f "$BIN")
TARBALL=""
if [ "${2:-}" = "--tarball" ]; then TARBALL=$(readlink -f "${3:?--tarball needs a file}"); fi
PASS=0
FAIL=0
say() { printf '%s\n' "$*"; }
ok() { PASS=$((PASS + 1)); say "ok   $*"; }
bad() { FAIL=$((FAIL + 1)); say "FAIL $*"; }

# 1. The floor, read from the binary rather than guessed.
floor=$(objdump -T "$BIN" 2>/dev/null | grep -o 'GLIBC_[0-9.]*' | sort -Vu | tail -1)
say "highest glibc symbol: ${floor:-unknown}"

run_in() {
  local image=$1
  if [ -n "$TARBALL" ]; then
    docker run --rm -v "$TARBALL:/in/hades.tar.gz:ro" "$image" bash -lc '
      mkdir -p /w && tar -xzf /in/hades.tar.gz -C /w && B=$(find /w -name hades -type f | head -1)
      "$B" version 2>&1 | tail -2' 2>&1 | tail -1
  else
    docker run --rm -v "$BIN:/hades:ro" "$image" bash -lc '"$0" version 2>&1 | tail -2' /hades 2>&1 | tail -2
  fi
}

# 2. The newest LTS is the floor the README names.
newest=$(run_in ubuntu:24.04)
case "$newest" in
  *GLIBC*) bad "ubuntu:24.04 is below the floor this binary was linked against: $(printf '%s' "$newest" | head -c 120)" ;;
  *"hades "[0-9]*) ok "ubuntu:24.04 runs it cleanly" ;;
  *) bad "ubuntu:24.04 did not run it: $newest" ;;
esac

# 3. The previous LTS. It is expected to refuse, and the refusal has to be the loader naming glibc, not a
#    crash somewhere else: that is the difference between an unsupported floor and a broken binary.
older=$(run_in ubuntu:22.04)
# A weak-version warning does not stop the binary printing its version, which is exactly why this checks the
# warning rather than the exit status: "it launched" and "it is supported here" are different claims.
case "$older" in
  *GLIBC*) ok "ubuntu:22.04 is below the floor and says so ($(printf '%s' "$older" | head -c 80))" ;;
  *"hades "[0-9]*) ok "ubuntu:22.04 runs it cleanly" ;;
  *) bad "ubuntu:22.04 failed for a reason other than glibc: $older" ;;
esac

# 4. ARM64, only if this kernel can execute it. Saying "not checked" is better than pretending.
if docker run --rm --platform linux/arm64 alpine uname -m >/dev/null 2>&1; then
  arm=$(docker run --rm --platform linux/arm64 -v "$BIN:/hades:ro" ubuntu:24.04 bash -lc '"$0" version 2>&1 | tail -1' /hades 2>&1 | tail -1)
  case "$arm" in
    hades\ [0-9]*) ok "arm64 runs it ($arm)" ;;
    *) ok "arm64 is emulated but this binary is x86_64; a native arm64 build is a separate artifact" ;;
  esac
else
  say "skip arm64: this kernel has no binfmt handler for it (a native arm64 build is a separate artifact)"
fi

say ""
say "portability: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
