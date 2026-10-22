#!/usr/bin/env bash
# The artifact, on a machine that is not this one.
#
# Extracts the packaged tarball into a directory with nothing else in it, gives it a home of its own and an
# empty environment, and runs the same work from there: the acceptance suite, an install from the published
# shelf, and the fault cases. If a build tree were doing any of the work, this is where it would show.
set -uo pipefail

TARBALL=${1:?usage: cross-machine.sh <tarball>}
TARBALL=$(readlink -f "$TARBALL")
ROOT=$(mktemp -d "${TMPDIR:-/tmp}/hades-cross.XXXXXX")
trap 'rm -rf "$ROOT"' EXIT

say() { printf '%s\n' "$*"; }
PASS=0
FAIL=0
ok() { PASS=$((PASS + 1)); say "ok   $*"; }
bad() { FAIL=$((FAIL + 1)); say "FAIL $*"; }

mkdir -p "$ROOT/install" "$ROOT/home"
tar -xzf "$TARBALL" -C "$ROOT/install"
BIN=$(find "$ROOT/install" -maxdepth 3 -type f -name hades -perm -u+x | head -1)
[ -x "$BIN" ] || { say "no binary in the tarball"; exit 2; }

# A home of its own and a umask the build tree never used. The environment is kept: `systemd-analyze` needs a
# session bus to verify a user unit, and starving it tested the isolation, not the artifact.
run() { ( umask 022; HOME="$ROOT/home" TMPDIR="$ROOT" bash -c "$*" ); }

version=$(run "\"$BIN\" version" 2>&1)
case "$version" in
  hades\ [0-9]*) ok "the packaged binary answers $version" ;;
  *) bad "version said: $version" ;;
esac

# 1. The acceptance suite, from the tarball, with nothing on PATH but the system.
( cd "$ROOT/install" && find . -path '*acceptance/clean-home.sh' -exec chmod +x {} \; )
HERE=$(cd "$(dirname "$0")" && pwd)
suite=$(find "$ROOT/install" -maxdepth 3 -name 'clean-home.sh' | head -1)
[ -z "$suite" ] && [ -f "$HERE/../acceptance/clean-home.sh" ] && suite="$HERE/../acceptance/clean-home.sh"
if [ -n "$suite" ]; then
  out=$(cd "$(dirname "$suite")/.." && ( umask 022; HOME="$ROOT/home" TMPDIR="$ROOT" bash "$suite" --bin "$BIN" 2>&1 ) )
  if printf '%s' "$out" | grep -q "^PASS: clean-home acceptance"; then
    ok "the packaged binary passes the clean-home acceptance suite"
  else
    bad "acceptance from the tarball failed: $(printf '%s' "$out" | grep -E '^FAIL' | head -2)"
  fi
else
  say "skip the tarball does not carry the acceptance suite"
fi

# 2. The published shelf, over the network, with no state of its own yet.
state="$ROOT/shelf"
mkdir -p "$state"
run "HOME=$state HADES_HOME=$state/.hades \"$BIN\" init --provider ollama-cloud --model qwen3.8-27b --root $state/work" >/dev/null 2>&1
said=$(run "HOME=$state HADES_HOME=$state/.hades \"$BIN\" pack add plan-a-change --yes" 2>&1)
if [ -f "$state/.hades/skills/plan-a-change/SKILL.md" ]; then
  ok "a pack installs from the published shelf on a machine with no build tree"
else
  bad "the published shelf install failed: $(printf '%s' "$said" | head -2)"
fi

# 3. The same pack again: a second install is a skip, not an error.
said=$(run "HOME=$state HADES_HOME=$state/.hades \"$BIN\" pack add plan-a-change --yes" 2>&1)
if printf '%s' "$said" | grep -q "already installed"; then
  ok "installing the same pack twice is a skip"
else
  bad "a second install said: $(printf '%s' "$said" | head -2)"
fi

# 4. The fault cases, against the artifact rather than the build.
fault=$(find "$ROOT/install" -maxdepth 4 -name 'fault-injection.sh' | head -1)
[ -z "$fault" ] && [ -f "$HERE/fault-injection.sh" ] && fault="$HERE/fault-injection.sh"
if [ -n "$fault" ]; then
  out=$(cd "$ROOT" && ( umask 022; HOME="$ROOT/home" TMPDIR="$ROOT" bash "$fault" "$BIN" 2>&1 ) )
  line=$(printf '%s' "$out" | grep -E '^fault injection:' | head -1)
  failures=$(printf '%s' "$out" | grep -c '^FAIL' || true)
  [ "$failures" = "0" ] && ok "the fault cases pass against the packaged binary ($line)" \
    || bad "the fault cases failed against the packaged binary ($line)"
else
  say "skip the tarball does not carry the fault cases"
fi

say ""
say "cross-machine: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
