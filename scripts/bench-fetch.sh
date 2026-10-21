#!/usr/bin/env bash
# Measure the fetch path before replacing it.
#
# The review asked whether spawning a `curl` per fetch is a bottleneck worth a persistent client. A
# preference is not an answer; this is 50 sequential fetches against one local fixture, through the same
# invocation `harness/src/fetch.rs` makes, next to the same 50 requests over one keep-alive connection in
# a single process. The difference is the cost of the process, not of the network.
#
#   ./scripts/bench-fetch.sh [count]
#
# Prints wall time for both arms, the per-fetch delta, the process count, and curl's peak RSS.
set -euo pipefail

COUNT=${1:-50}
PORT=${PORT:-8791}
WORK=$(mktemp -d)
SERVER=""
cleanup() {
    [ -n "$SERVER" ] && kill "$SERVER" 2>/dev/null || true
    rm -rf "$WORK"
}
trap cleanup EXIT

# An 8 KiB page: big enough that the body is real, small enough that the fixture is not the variable.
python3 - "$WORK/page.html" <<'PY'
import sys
body = ("<html><body><p>" + ("evidence " * 900) + "</p></body></html>")
open(sys.argv[1], "w").write(body)
PY

python3 -m http.server "$PORT" --bind 127.0.0.1 --directory "$WORK" >/dev/null 2>&1 &
SERVER=$!
for _ in $(seq 50); do
    if curl -sS -o /dev/null "http://127.0.0.1:$PORT/page.html" 2>/dev/null; then break; fi
    sleep 0.05
done

URL="http://127.0.0.1:$PORT/page.html"
HDR="$WORK/hdr"
BODY="$WORK/body"
# The exact flags fetch.rs builds: no redirects, protocols pinned, size capped, headers to a file.
CURL=(curl -sS --max-time 20 --proto =http,https --proto-redir =http,https --max-redirs 0
      --max-filesize 4194304 -A "Mozilla/5.0 (compatible; Hades/0.21)" -D "$HDR" -o "$BODY" "$URL")

# Three warm fetches, so the number is steady-state rather than the first page-cache miss.
for _ in 1 2 3; do "${CURL[@]}" >/dev/null; done

start=$(date +%s.%N)
for _ in $(seq "$COUNT"); do
    "${CURL[@]}" >/dev/null
done
end=$(date +%s.%N)
curl_total=$(python3 -c "print(f'{$end - $start:.3f}')")

# One curl, measured for memory: the peak is what a per-fetch process costs while it lives.
rss=$(/usr/bin/time -v "${CURL[@]}" 2>&1 >/dev/null | awk -F': ' '/Maximum resident set size/ {print $2}')

# The same COUNT requests over one connection, in one process. This is the shape a persistent client has;
# it is not a proposal, it is the floor the current path is compared against.
start=$(date +%s.%N)
python3 - "$URL" "$COUNT" <<'PY' >/dev/null
import http.client, sys, urllib.parse
url = urllib.parse.urlsplit(sys.argv[1])
count = int(sys.argv[2])
conn = http.client.HTTPConnection(url.hostname, url.port)
for _ in range(count):
    conn.request("GET", url.path, headers={"User-Agent": "Mozilla/5.0 (compatible; Hades/0.21)"})
    response = conn.getresponse()
    response.read()
conn.close()
PY
end=$(date +%s.%N)
keepalive_total=$(python3 -c "print(f'{$end - $start:.3f}')")

python3 - "$curl_total" "$keepalive_total" "$COUNT" "$rss" <<'PY'
import sys
curl_total, keep_total, count, rss = float(sys.argv[1]), float(sys.argv[2]), int(sys.argv[3]), sys.argv[4]
curl_ms = curl_total / count * 1000
keep_ms = keep_total / count * 1000
win = (curl_total - keep_total) / curl_total * 100 if curl_total else 0.0
print(f"fetches:            {count}")
print(f"curl per fetch:     {curl_total:.3f}s total, {curl_ms:.2f} ms/fetch ({count} processes)")
print(f"one connection:     {keep_total:.3f}s total, {keep_ms:.2f} ms/fetch (1 process)")
print(f"process cost:       {curl_ms - keep_ms:.2f} ms/fetch, {win:.1f}% of the current path")
print(f"curl peak RSS:      {rss} KiB")
print(f"verdict:            {'over 20% -> open a task for a persistent client' if win >= 20 else 'under 20% -> close the finding as accepted'}")
PY
