#!/usr/bin/env bash
# Print one version's section of RELEASES.md, for a release body.
#
# `gh release create --notes-file RELEASES.md` publishes every release ever written. The section for the tag
# being published is the part a person downloading it wants.
#
# usage: scripts/release-notes.sh <version>
set -uo pipefail

VERSION=${1:?usage: release-notes.sh <version>}
HERE=$(cd "$(dirname "$0")" && pwd)
FILE=$(cd "$HERE/../.." && pwd)/RELEASES.md

awk -v want="## $VERSION" '
  $0 == want { found = 1; next }
  found && /^## / { exit }
  found { print }
' "$FILE" | sed -e '1{/^$/d}'

grep -qF "## $VERSION" "$FILE" || { echo "no section for $VERSION in $FILE" >&2; exit 1; }
