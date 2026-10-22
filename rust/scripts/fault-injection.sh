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
  local name=$1
  local version=${2:-1.0.0}
  local marker=${3:-first}
  local dir="$WORK/src-$name-$version"
  mkdir -p "$dir/skills/$name/notes"
  printf -- '---\nname: %s\ndescription: Use when %s is the one.\n---\n\n# %s\n\n%s\n' \
    "$name" "$name" "$name" "$marker" >"$dir/skills/$name/SKILL.md"
  local i
  for i in $(seq 1 120); do
    printf 'line %s of %s\n' "$i" "$name" >"$dir/skills/$name/notes/$i.txt"
  done
  "$BIN" pack export --dir "$dir/skills/$name" --license MIT --version "$version" \
    --out "$WORK/$name-$version.hadepack" >/dev/null 2>&1 || return 1
}

fresh_state() {
  local at=$1
  rm -rf "$at"
  mkdir -p "$at"
  HOME="$at" HADES_HOME="$at/.hades" "$BIN" init --provider ollama-cloud --model qwen3.8-27b \
    --root "$at/work" >/dev/null 2>&1
}

h() { HOME="$1" HADES_HOME="$1/.hades" "$BIN" "${@:2}"; }

make_pack alpha 1.0.0 first || { echo "could not build a fixture pack"; exit 2; }
make_pack alpha 1.1.0 second || { echo "could not build a second version"; exit 2; }

# 1. Kill an install at twenty different moments. Every survivor has to be a state that a later install can
#    use: catalog parses, no skill that was never finished, and a re-run succeeds.
state="$WORK/kill"
fresh_state "$state"
leftovers=0
for i in $(seq 1 20); do
  delay=$(awk -v n="$i" 'BEGIN{printf "%.3f", n * 0.0015}')
  h "$state" pack add "$WORK/alpha-1.0.0.hadepack" --yes >/dev/null 2>&1 &
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
again=$(h "$state" pack add "$WORK/alpha-1.0.0.hadepack" --yes 2>&1)
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
head -c 900 "$WORK/alpha-1.0.0.hadepack" >"$WORK/alpha-cut.hadepack"
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
said=$( (ulimit -f 1; h "$state" pack add "$WORK/alpha-1.0.0.hadepack" --yes) 2>&1 )
code=$?
if [ $code -ne 0 ] && [ ! -d "$state/.hades/skills/alpha" ]; then
  ok "a failing write is reported and leaves no skill (exit $code)"
else
  bad "a failing write exited $code and left a skill directory behind"
fi
if h "$state" pack add "$WORK/alpha-1.0.0.hadepack" --yes >/dev/null 2>&1 && \
   [ -f "$state/.hades/skills/alpha/SKILL.md" ]; then
  ok "the same install succeeds once the write limit is gone"
else
  bad "the install did not recover after the write limit was lifted"
fi

# 4. A state directory that cannot be written is an error, not a panic, and not a partial skill. A regular
#    file where the cache directory belongs fails for root as well; `chmod -w` would not, and a test that
#    passes as a person and silently succeeds as root (inside a container, say) is worse than no test.
state="$WORK/readonly"
fresh_state "$state"
mkdir -p "$state/.hades"
rm -rf "$state/.hades/packs"
: >"$state/.hades/packs"
[ -f "$state/.hades/packs" ] || bad "could not put a file where the cache directory belongs"
said=$(h "$state" pack add "$WORK/alpha-1.0.0.hadepack" --yes 2>&1)
code=$?
rm -f "$state/.hades/packs"
if [ $code -ne 0 ] && [ ! -d "$state/.hades/skills/alpha" ] && ! printf '%s' "$said" | grep -q "panicked"; then
  ok "a state that cannot be written is refused cleanly (exit $code)"
else
  bad "a state that cannot be written exited $code: ${said:0:200}"
fi
# The same thing as a person: permission bits only mean something to a non-root caller.
if [ "$(id -u)" != "0" ]; then
  # The archive directory, not the state root: the state root is re-permissioned to 0700 by the lock itself,
  # which is the point of that normalisation and would quietly make the case pass.
  state="$WORK/readonly-chmod"
  fresh_state "$state"
  mkdir -p "$state/.hades/packs/archives"
  chmod -w "$state/.hades/packs/archives"
  said=$(h "$state" pack add "$WORK/alpha-1.0.0.hadepack" --yes 2>&1)
  code=$?
  chmod +w "$state/.hades/packs/archives"
  if [ $code -ne 0 ] && [ ! -d "$state/.hades/skills/alpha" ]; then
    ok "a read-only cache directory is refused cleanly (exit $code)"
  else
    bad "a read-only cache directory exited $code: ${said:0:200}"
  fi
else
  skip "running as root, so permission bits are not a refusal; the path-type case above covers it"
fi

# 5. Two installs of the same pack at once. The result must be one skill and a catalog that parses; ordering is
#    not promised, consistency is.
state="$WORK/same-twice"
fresh_state "$state"
h "$state" pack add "$WORK/alpha-1.0.0.hadepack" --yes >/dev/null 2>&1 &
first=$!
h "$state" pack add "$WORK/alpha-1.0.0.hadepack" --yes >/dev/null 2>&1 &
second=$!
wait "$first"
wait "$second"
lines=$(h "$state" pack list 2>/dev/null | grep -c '^  skill' || true)
if [ "$lines" = "1" ] && [ -f "$state/.hades/skills/alpha/SKILL.md" ]; then
  ok "two simultaneous installs of one pack leave one catalog row and one skill"
else
  bad "two simultaneous installs left $lines catalog row(s)"
fi

# 6. Two different packs, installed at the same moment, as two processes rather than two threads. Each reads
#    the catalog, changes its own row and writes the whole file back; without a lock the second write forgets
#    the first install and leaves a skill on disk that the catalog does not know about.
state="$WORK/lost-update"
fresh_state "$state"
make_pack beta 1.0.0 beta >/dev/null 2>&1 || bad "could not build a second pack"
h "$state" pack add "$WORK/alpha-1.0.0.hadepack" --yes >/dev/null 2>&1 &
first=$!
h "$state" pack add "$WORK/beta-1.0.0.hadepack" --yes >/dev/null 2>&1 &
second=$!
wait "$first"
wait "$second"
rows=$(h "$state" pack list 2>/dev/null | grep -c '^  skill' || true)
if [ "$rows" = "2" ] && [ -f "$state/.hades/skills/alpha/SKILL.md" ] && \
   [ -f "$state/.hades/skills/beta/SKILL.md" ]; then
  ok "two packs installed as two processes leave two catalog rows"
else
  bad "two processes installing different packs left ${rows} catalog row(s)"
fi

# 7. A replace, killed mid-way. The old skill is moved aside before the new one is renamed into place, and that
#    is the window where an interrupted install could leave nothing at all. After every kill the path must be
#    either a complete skill or gone (which the next run repairs); it must never be a half-copied directory,
#    and the final run must land the new version.
state="$WORK/replace"
fresh_state "$state"
h "$state" pack add "$WORK/alpha-1.0.0.hadepack" --yes >/dev/null 2>&1
printf '{"items":[{"kind":"skill","identity":"alpha","action":"replace"}]}\n' >"$WORK/decisions.json"
torn=0
gone=0
for i in $(seq 1 15); do
  delay=$(awk -v n="$i" 'BEGIN{printf "%.3f", n * 0.0012}')
  h "$state" pack apply "$WORK/alpha-1.1.0.hadepack" --plan "$WORK/decisions.json" --activate \
    >/dev/null 2>&1 &
  pid=$!
  sleep "$delay"
  kill -9 "$pid" 2>/dev/null
  wait "$pid" 2>/dev/null
  skill="$state/.hades/skills/alpha"
  if [ -d "$skill" ] && [ ! -f "$skill/SKILL.md" ]; then
    torn=$((torn + 1))
  elif [ ! -d "$skill" ]; then
    gone=$((gone + 1))
  fi
done
final=$(h "$state" pack apply "$WORK/alpha-1.1.0.hadepack" --plan "$WORK/decisions.json" --activate 2>&1)
if [ -f "$state/.hades/skills/alpha/SKILL.md" ] && grep -q second "$state/.hades/skills/alpha/SKILL.md"; then
  ok "a replace survives fifteen kills: ${torn} torn directories, ${gone} absent, final run lands 1.1.0"
else
  bad "a replace did not land after the kills: ${final:0:200}"
fi

# 7. A genuinely full filesystem, when the machine will give us a small one. This is the case `ulimit` only
#    approximates: the tar stage succeeds and the write fails partway through.
if sudo -n true 2>/dev/null; then
  mkdir -p "$WORK/tiny"
  if sudo -n mount -t tmpfs -o size=96k tmpfs "$WORK/tiny" 2>/dev/null; then
    state="$WORK/tiny/state"
    mkdir -p "$state"
    h "$state" init --provider ollama-cloud --model qwen3.8-27b --root "$state/work" >/dev/null 2>&1
    said=$(h "$state" pack add "$WORK/alpha-1.0.0.hadepack" --yes 2>&1)
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
