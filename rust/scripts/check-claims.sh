#!/usr/bin/env bash
# The version a release says it is, everywhere it says it.
#
# Twice an audit found the README advertising the previous version while the manifest had moved. A person
# reading the repository cannot see that; a grep can.
#
# usage: scripts/check-claims.sh
set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
status=0

version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$ROOT/rust/harness/Cargo.toml" | head -1)
[ -n "$version" ] || { echo "FAIL no version in rust/harness/Cargo.toml"; exit 1; }
echo "manifest version: $version"

for pattern in "version-$version-blue" "| **Version** | $version |"; do
  if grep -qF "$pattern" "$ROOT/README.md"; then
    echo "ok   README says $version"
  else
    echo "FAIL README has no '$pattern'"
    status=1
  fi
done

# The release notes' newest section must be the same version, or the notes describe a build nobody has.
newest=$(grep -m1 '^## ' "$ROOT/RELEASES.md" | sed 's/^## //')
if [ "$newest" = "$version" ]; then
  echo "ok   RELEASES.md newest section is $version"
else
  echo "FAIL RELEASES.md newest section is '$newest', manifest is $version"
  status=1
fi

exit "$status"
