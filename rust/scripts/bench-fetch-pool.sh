#!/bin/sh
# Time the fetch tool itself, both ways: through the shared libcurl handle and through the process.
#
# The fixture is a local page, so the number is what the tool spends per fetch rather than what the
# network costs. The comparison is the point: `HADES_FETCH_POOL=off` forces the child process, which is
# what every fetch used before the pool existed.
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
COUNT=${1:-50}
PORT=${2:-8791}
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"; [ -n "${server:-}" ] && kill "$server" 2>/dev/null || true' EXIT

mkdir -p "$WORK/site"
# 8 KiB, the same size the original benchmark used.
python3 - "$WORK/site/index.html" <<'PY'
import sys
open(sys.argv[1], 'w').write('<' + 'x' * 8180 + '>')
PY

cd "$WORK/site"
python3 -m http.server "$PORT" >/dev/null 2>&1 &
server=$!
sleep 1

cd "$ROOT"
export HADES_FETCH_ALLOW_PRIVATE=1

# The build directory is whatever cargo resolves: `CARGO_TARGET_DIR`, a config file, or `target/`.
cargo build --release -q --example bench_fetch
binary="$(cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')/release/examples/bench_fetch"

echo "pooled (one libcurl handle):"
"$binary" "http://127.0.0.1:$PORT/" "$COUNT"
echo "per process (HADES_FETCH_POOL=off):"
HADES_FETCH_POOL=off "$binary" "http://127.0.0.1:$PORT/" "$COUNT"
