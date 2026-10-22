#!/usr/bin/env bash
# Assert what a built binary claims about itself, so the claim cannot drift when the build host changes.
#
# Two things are checked because they are the two that were wrong before:
#   - the highest glibc symbol version the binary needs, against the floor the README names;
#   - whether a musl build is actually static, rather than linked against a musl that happens to be there.
#
# usage: assert-floor.sh <binary> [--max-glibc 2.30] [--static]
set -uo pipefail

BIN=${1:?usage: assert-floor.sh <binary> [--max-glibc X.Y] [--static]}
MAX=""
STATIC=0
shift
while [ $# -gt 0 ]; do
  case "$1" in
    --max-glibc) MAX=${2:?--max-glibc needs a version}; shift 2 ;;
    --static) STATIC=1; shift ;;
    *) echo "usage: assert-floor.sh <binary> [--max-glibc X.Y] [--static]" >&2; exit 2 ;;
  esac
done

[ -x "$BIN" ] || { echo "FAIL no executable at $BIN"; exit 1; }

status=0

if [ "$STATIC" = "1" ]; then
  # `file` says "static-pie linked" for a static position-independent binary and "statically linked" for the
  # older non-PIE form. Both ask the host for nothing; the check must not depend on which one the linker chose.
  if file -b "$BIN" | grep -qE "static-pie linked|statically linked"; then
    echo "ok   static binary"
  else
    echo "FAIL binary is not static: $(file -b "$BIN")"
    status=1
  fi
fi

floor=$("${OBJDUMP:-objdump}" -T "$BIN" 2>/dev/null | grep -oE 'GLIBC_[0-9]+\.[0-9]+' | sort -Vu | tail -1)
if [ -z "$floor" ]; then
  if [ "$STATIC" = "1" ]; then
    echo "ok   no glibc symbols at all"
  else
    echo "FAIL no glibc symbols found; is this a glibc binary?"
    status=1
  fi
else
  echo "glibc floor: $floor"
  if [ -n "$MAX" ]; then
    # Version-wise comparison: 2.9 is older than 2.30, which string ordering gets wrong.
    worst=$(printf '%s\n' "GLIBC_$MAX" "$floor" | sort -V | tail -1)
    if [ "$worst" = "$floor" ] && [ "$floor" != "GLIBC_$MAX" ]; then
      echo "FAIL $floor is newer than the $MAX floor the release promises"
      status=1
    else
      echo "ok   within the $MAX floor"
    fi
  fi
fi

if [ "${HADES_ASSERT_NO_RUN:-0}" = "1" ]; then
  echo "$BIN: cross binary, version read by the runner"
else
  echo "$BIN version: $("$BIN" version 2>&1 | tail -1)"
fi
exit "$status"
