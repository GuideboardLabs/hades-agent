#!/usr/bin/env bash
# Fail when a released artifact names a path on the machine that built it.
#
# rustc embeds the absolute paths it is handed: the checkout, CARGO_HOME (the registry sources of every
# dependency, which is where most of the strings come from) and CARGO_TARGET_DIR. An artifact that says
# /home/<someone>/... or /mnt/<disk>/... publishes a build directory nobody can use and tells a reader
# nothing about the release.
#
# The rule is a path segment, not the token `/home/`: the binary legitimately contains the text
# `/home/` inside a validation list ("forbidden in a distilled runbook"), and that string is not a build
# path. A leak looks like `/home/<name>/...` or `/mnt/<disk>/...`, so a segment has to follow the
# directory, and it has to be a real name (`/home//Users` has an empty segment and does not match).
#
# usage: assert-no-build-paths.sh <artifact> [more artifacts...]
set -euo pipefail

[ $# -gt 0 ] || {
  echo "usage: assert-no-build-paths.sh <artifact>..." >&2
  exit 2
}

status=0
for artifact in "$@"; do
  [ -f "$artifact" ] || {
    echo "FAIL no artifact at $artifact" >&2
    exit 1
  }
  found=$(strings -a "$artifact" | grep -E '/(home|mnt)/[^/[:space:]]' | sort -u | head -5 || true)
  if [ -n "$found" ]; then
    echo "FAIL $artifact names a build path:" >&2
    printf '%s\n' "$found" >&2
    status=1
  else
    echo "build paths: none in $artifact"
  fi
done
exit "$status"
