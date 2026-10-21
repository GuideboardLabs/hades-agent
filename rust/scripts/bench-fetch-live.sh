#!/bin/sh
# What the process boundary costs on a real page rather than a loopback fixture.
#
# The 0.1.0 benchmark measured 50 fetches of an 8 KiB page on 127.0.0.1 and found 81% of the wall time in
# the process. That fixture has no network, so it answers "what is a process worth" and not "what does a
# fetch cost". This runs the same two arms against a page that is served over TLS from the internet, 20
# times, and prints the share the process takes.
set -eu

TARGET=${1:-https://example.com/}
COUNT=${2:-20}
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

echo "target: $TARGET   count: $COUNT"

# arm 1: one curl process per fetch, the shape fetch.rs builds
start=$(date +%s%N)
i=0
while [ "$i" -lt "$COUNT" ]; do
    curl -sS --proto '=http,https' --proto-redir '=http,https' --max-redirs 0 \
        --max-filesize 4194304 -o "$WORK/body.$i" "$TARGET" 2>/dev/null || true
    i=$((i + 1))
done
end=$(date +%s%N)
spawn_total=$(( (end - start) / 1000000 ))

# arm 2: the same requests over reused connections in one process
start=$(date +%s%N)
i=0
: > "$WORK/config"
while [ "$i" -lt "$COUNT" ]; do
    printf 'url = "%s"\noutput = "%s/body2.%s"\n' "$TARGET" "$WORK" "$i" >> "$WORK/config"
    i=$((i + 1))
done
curl -sS --proto '=http,https' --proto-redir '=http,https' --max-redirs 0 \
    --max-filesize 4194304 --config "$WORK/config" 2>/dev/null || true
end=$(date +%s%N)
reuse_total=$(( (end - start) / 1000000 ))

echo "per-fetch process:   ${spawn_total} ms total, $((spawn_total * 100 / COUNT)) us.. ms/fetch"
echo "one connection kept: ${reuse_total} ms total"
python3 - "$spawn_total" "$reuse_total" "$COUNT" <<'PY'
import sys
a, b, n = float(sys.argv[1]), float(sys.argv[2]), float(sys.argv[3])
per_a, per_b = a / n, b / n
share = (per_a - per_b) / per_a * 100 if per_a else 0
print(f"per fetch: {per_a:.1f} ms vs {per_b:.1f} ms; process share {share:.1f}%")
PY
