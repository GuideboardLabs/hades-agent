#!/usr/bin/env bash
# Build the packs repository against THIS checkout of base, and fail if it does not compile.
#
# Why this exists: a pack depends on a released tag, never on the working tree. That keeps the
# packs repository independent, but it also means a change here can break a pack and no job would
# notice until someone built that pack against a new release. This is that job.
#
# It rewrites the pack's `harness` dependency to a path into this checkout for the duration of the
# build only. The committed manifest keeps its tag; nothing here is written back.
#
# Usage: rust/scripts/pack-contract.sh [--packs-dir DIR]
set -euo pipefail

BASE="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PACKS="${PACKS_DIR:-$BASE/../hades-packs}"

while [ $# -gt 0 ]; do
  case "$1" in
    --packs-dir) PACKS="$2"; shift 2 ;;
    *) echo "pack-contract: unknown argument $1" >&2; exit 64 ;;
  esac
done

if [ ! -f "$PACKS/Cargo.toml" ]; then
  echo "pack-contract: no packs checkout at $PACKS" >&2
  echo "  clone it: git clone https://github.com/GuideboardLabs/hades-agent-packs" >&2
  exit 66
fi

# The crates in the packs repo that build against base. Add one line per connector pack.
CONNECTORS="packs/discord"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
cp -a "$PACKS/Cargo.toml" "$work/Cargo.toml"
for crate in $CONNECTORS; do
  mkdir -p "$work/$(dirname "$crate")"
  cp -a "$PACKS/$crate" "$work/$crate"
done
sed -i 's|"packs/discord"|"'$work'/packs/discord"|' "$work/Cargo.toml"

for crate in $CONNECTORS; do
  manifest="$work/$crate/Cargo.toml"
  # One substitution, and it is the whole point of the job: whatever tag the pack pins, build the
  # bytes in this checkout instead.
  perl -0pi -e 's|harness = \{[^}]*\}|harness = { path = "'"$BASE"'/rust/harness" }|' "$manifest"
  echo "pack-contract: building $crate against $BASE/rust/harness"
  cargo test --manifest-path "$work/Cargo.toml" -p "$(sed -n 's/^name = "\(.*\)"/\1/p' "$manifest" | head -1)"
done

echo "pack-contract: every connector pack compiles and passes its tests against this checkout"
