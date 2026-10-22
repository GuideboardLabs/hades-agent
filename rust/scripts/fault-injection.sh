#!/usr/bin/env bash
# Fault injection for the pack installer.
#
# Static review found the races; this is the other half: break the machine while an install is running and
# check what is left behind. Every case asserts on state a person can read afterwards (`pack list`, the skill
# on disk, the cache directory), not on the exit code alone, because an install that fails and leaves a
# half-written skill is worse than one that fails cleanly.
#
# usage: scripts/fault-injection.sh <hades-binary>
set -uo pipefail

BIN=${1:?usage: fault-injection.sh <hades binary>}
BIN=$(readlink -f "$BIN")
WORK=$(mktemp -d "${TMPDIR:-/tmp}/hades-fault.XXXXXX")
PASS=0
FAIL=0
SKIP=0

say() { printf '%s\n' "$*"; }
ok() { PASS=$((PASS + 1)); say "ok   $*"; }
bad() { FAIL=$((FAIL + 1)); say "FAIL $*"; }
skip() { SKIP=$((SKIP + 1)); say "skip $*"; }

cleanup() {
  # A tmpfs mount from the disk-full case is unmounted here if the case failed before it could.
  mountpoint -q "$WORK/tiny" 2>/dev/null && sudo -n umount "$WORK/tiny" 2>/dev/null
  rm -rf "$WORK"
}
trap cleanup EXIT

# A pack with enough files that a kill has somewhere to land in the middle of the write.
make_pack() {
  local name=$1 dir="$WORK/src-$1"
  mkdir -p "$dir/skills/$name/notes"
  printf -- '---\nname: %s\ndescription: Use when %s is the one.\n---\n\n# %s\n' "$name" "$name" "$name" \
    >"$dir/skills/$name/SKILL.md"
  local i
  for i in $(seq 1 120); do
    printf 'line %s of %s\n' "$i" "$name" >"$dir/skills/$name/notes/$i.txt"
  done
  "$BIN" pack export --dir "$dir/skills/$name" --license MIT --out "$WORK/$name.hadepack" \
    >/dev/null 2>&1 || return 1
}

fresh_state() {
  local at=$1
  rm -rf "$at"
  mkdir -p "$at"
  HOME="$at" HADES_HOME="$at/.hades" "$BIN" init --provider ollama-cloud --model qwen3.8-27b \
    --root "$at/work" >/dev/null 2>&1
}

h() { HOME="$1" HADES_HOME="$1/.hades" "$BIN" "${@:2}"; }

make_pack alpha || { echo "could not build a fixture pack"; exit 2; }

# 1. Kill an install at twenty different moments. Every survivor has to be a state that a later install can
#    use: catalog parses, no skill that was never finished, and a re-run succeeds.
state="$WORK/kill"
fresh_state "$state"
leftovers=0
for i in $(seq 1 20); do
  delay=$(awk -v n="$i" 'BEGIN{printf "%.3f", n * 0.0015}')
  h "$state" pack add "$WORK/alpha.hadepack" --yes >/dev/null 2>&1 &
  pid=$!
  sleep "$delay"
  kill -9 "$pid" 2>/dev/null
  wait "$pid" 2>/dev/null
  if ! h "$state" pack list >/dev/null 2>&1; then
    bad "the catalog did not parse after a kill at ${delay}s (iteration $i)"
    break
  fi
  if [ -d "$state/.hades/skills/alpha" ] && [ ! -f "$state/.hades/skills/alpha/SKILL.md" ]; then
    bad "a skill directory without a SKILL.md survived a kill at ${delay}s (iteration $i)"
    break
  fi
done
leftovers=$(find "$state/.hades/packs/archives" -name 'incoming-*' 2>/dev/null | wc -l)
again=$(h "$state" pack add "$WORK/alpha.hadepack" --yes 2>&1)
after=$(find "$state/.hades/packs/archives" -name 'incoming-*' 2>/dev/null | wc -l)
if [ $? -eq 0 ] && [ -f "$state/.hades/skills/alpha/SKILL.md" ]; then
  if [ "$after" = "0" ]; then
    ok "twenty kills, catalog intact, re-install succeeds, ${leftovers} staged file(s) swept"
  else
    ok "twenty kills, catalog intact, re-install succeeds, but ${after} staged file(s) were left behind"
  fi
else
  bad "a re-install after the kills did not succeed: $again"
fi

# 2. A truncated archive is what a dropped connection looks like. The digest is the only thing that can catch
#    it, because the tar header can still be valid.
head -c 900 "$WORK/alpha.hadepack" >"$WORK/alpha-cut.hadepack"
state="$WORK/truncated"
fresh_state "$state"
said=$(h "$state" pack add "$WORK/alpha-cut.hadepack" --yes 2>&1)
if [ $? -ne 0 ] && printf '%s' "$said" | grep -qE "hashes to|not a readable pack"; then
  if [ ! -d "$state/.hades/skills/alpha" ]; then
    ok "a truncated archive is refused before it is installed and leaves no skill"
  else
    bad "a truncated archive was refused but a skill directory survived"
  fi
else
  bad "a truncated archive was not refused: $said"
fi

# 3. The write itself fails. `ulimit -f` makes every write past 1 KiB fail, which is the same code path a full
#    disk takes (EFBIG here, ENOSPC there) without filling a real disk.
state="$WORK/write-fails"
fresh_state "$state"
said=$( (ulimit -f 1; h "$state" pack add "$WORK/alpha.hadepack" --yes) 2>&1 )
code=$?
if [ $code -ne 0 ] && [ ! -d "$state/.hades/skills/alpha" ]; then
  ok "a failing write is reported and leaves no skill (exit $code)"
else
  bad "a failing write exited $code and left a skill directory behind"
fi
if h "$state" pack add "$WORK/alpha.hadepack" --yes >/dev/null 2>&1 && \
   [ -f "$state/.hades/skills/alpha/SKILL.md" ]; then
  ok "the same install succeeds once the write limit is gone"
else
  bad "the install did not recover after the write limit was lifted"
fi

# 4. A state directory that cannot be written is an error, not a panic, and not a partial skill.
state="$WORK/readonly"
fresh_state "$state"
mkdir -p "$state/.hades/packs/archives"
chmod -w "$state/.hades/packs"
said=$(h "$state" pack add "$WORK/alpha.hadepack" --yes 2>&1)
code=$?
chmod +w "$state/.hades/packs"
if [ $code -ne 0 ] && [ ! -d "$state/.hades/skills/alpha" ] && ! printf '%s' "$said" | grep -q "panicked"; then
  ok "an unwritable state is refused cleanly (exit $code)"
else
  bad "an unwritable state exited $code: $said"
fi

# 5. Two installs of the same pack at once. The result must be one skill and a catalog that parses; ordering is
#    not promised, consistency is.
state="$WORK/same-twice"
fresh_state "$state"
h "$state" pack add "$WORK/alpha.hadepack" --yes >/dev/null 2>&1 &
first=$!
h "$state" pack add "$WORK/alpha.hadepack" --yes >/dev/null 2>&1 &
second=$!
wait "$first"
wait "$second"
lines=$(h "$state" pack list 2>/dev/null | grep -c '^  skill' || true)
if [ "$lines" = "1" ] && [ -f "$state/.hades/skills/alpha/SKILL.md" ]; then
  ok "two simultaneous installs of one pack leave one catalog row and one skill"
else
  bad "two simultaneous installs left $lines catalog row(s)"
fi

# 6. A genuinely full filesystem, when the machine will give us a small one. This is the case `ulimit` only
#    approximates: the tar stage succeeds and the write fails partway through.
if sudo -n true 2>/dev/null; then
  mkdir -p "$WORK/tiny"
  if sudo -n mount -t tmpfs -o size=96k tmpfs "$WORK/tiny" 2>/dev/null; then
    state="$WORK/tiny/state"
    mkdir -p "$state"
    h "$state" init --provider ollama-cloud --model qwen3.8-27b --root "$state/work" >/dev/null 2>&1
    said=$(h "$state" pack add "$WORK/alpha.hadepack" --yes 2>&1)
    code=$?
    sudo -n umount "$WORK/tiny" 2>/dev/null
    if [ $code -ne 0 ]; then
      ok "a full filesystem is reported as a failed install (exit $code)"
    else
      bad "a 96k filesystem accepted a larger pack"
    fi
  else
    skip "a tmpfs cannot be mounted here, so disk-full is covered by the ulimit case only"
  fi
else
  skip "no passwordless sudo, so disk-full is covered by the ulimit case only"
fi

say ""
say "fault injection: $PASS passed, $FAIL failed, $SKIP skipped"
[ "$FAIL" -eq 0 ]
