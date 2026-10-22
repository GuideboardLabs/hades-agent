#!/usr/bin/env bash
# The artifact on a filesystem that is not this machine's.
#
# The host run (`cross-machine.sh`) still shares a kernel, a /proc, a container runtime and a home that has
# seen other Hades runs. This one extracts the release inside a container image with nothing from the host
# except the tarball and the two test scripts, so a leak into `$HOME`, a dependency on a host path, or a
# assumption about the host's tools shows up as a failure here.
#
# Requires docker and network access for the shelf install.
set -uo pipefail

TARBALL=${1:?usage: cross-machine-container.sh <tarball>}
[ -f "$TARBALL" ] || { echo "no tarball at $TARBALL"; exit 2; }
TARBALL=$(readlink -f "$TARBALL")
HERE=$(cd "$(dirname "$0")" && pwd)
IMAGE=${HADES_TEST_IMAGE:-archlinux:latest}

say() { printf '%s\n' "$*"; }
PASS=0
FAIL=0
ok() { PASS=$((PASS + 1)); say "ok   $*"; }
bad() { FAIL=$((FAIL + 1)); say "FAIL $*"; }

say "image: $IMAGE"
if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  say "no local image $IMAGE; pulling"
  docker pull "$IMAGE" >/dev/null 2>&1 || { say "could not pull $IMAGE"; exit 2; }
fi

# One container, one script, one exit code. Everything the container needs is mounted read-only; everything
# it writes lives in /work, which is a tmpfs inside the container and disappears with it.
out=$(docker run --rm --network bridge \
  -v "$TARBALL:/in/hades.tar.gz:ro" \
  -v "$HERE:/tests:ro" \
  "$IMAGE" bash -lc '
set -uo pipefail
mkdir -p /work /work/home
tar -xzf /in/hades.tar.gz -C /work
ROOT=$(find /work -maxdepth 2 -type d -name "hades-*" | head -1)
BIN="$ROOT/bin/hades"
export HOME=/work/home

echo "version: $($BIN version)"

# The installer, from the tarball, into a home inside the container.
if bash "$ROOT/install.sh" --prefix /work/app --home /work/home >/tmp/install.log 2>&1; then
  echo "install: ok"
else
  echo "install: failed"; tail -5 /tmp/install.log; exit 0
fi

# What the install is for: a daemon that starts, answers, and stops, from the installed binary. The full
# acceptance suite needs the build tree (it re-packages), so it stays a host test; this is the part that has
# to work on a machine with nothing but the tarball.
STATE=/work/state; mkdir -p "$STATE"
HOME="$STATE" "$BIN" init --provider ollama-cloud --model qwen3.8-27b --root "$STATE/work" >/dev/null 2>&1
HOME="$STATE" "$BIN" daemon --state "$STATE/.hades" >/tmp/daemon.log 2>&1 &
DAEMON=$!
ready=no
for _ in $(seq 1 100); do
  if HOME="$STATE" "$BIN" status --state "$STATE/.hades" >/tmp/status.log 2>&1; then ready=yes; break; fi
  sleep 0.1
done
echo "daemon: $ready"
HOME="$STATE" "$BIN" shutdown --state "$STATE/.hades" >/dev/null 2>&1
wait "$DAEMON" 2>/dev/null
if grep -q "daemon" /tmp/status.log; then echo "status: answers"; else echo "status: $(head -1 /tmp/status.log)"; fi

# The published shelf, from inside the container.
STATE=/work/shelf; mkdir -p "$STATE"
HOME=$STATE "$BIN" init --provider ollama-cloud --model qwen3.8-27b --root "$STATE/work" >/dev/null 2>&1
if HOME=$STATE "$BIN" pack add plan-a-change --yes >/tmp/add.log 2>&1; then
  echo "shelf: installed"
else
  echo "shelf: failed"; tail -3 /tmp/add.log
fi
if HOME=$STATE "$BIN" pack add plan-a-change --yes 2>&1 | grep -q "already installed"; then
  echo "shelf: second install is a skip"
else
  echo "shelf: second install was not a skip"
fi

# The fault cases, against this binary, inside this filesystem.
if bash /tests/fault-injection.sh "$BIN" >/tmp/fault.log 2>&1; then
  echo "fault: $(grep -E "^fault injection:" /tmp/fault.log)"
else
  echo "fault: failures"; grep -E "^FAIL" /tmp/fault.log | head -3
fi
' 2>&1)

say "$out" | sed -e 's/^/  /'

printf '%s' "$out" | grep -q "^version: hades [0-9]" && ok "the binary runs on a filesystem that is not the host's" || bad "the binary did not run in the container"
printf '%s' "$out" | grep -q "^install: ok" && ok "the tarball's installer completes in the container" || bad "the installer failed in the container"
printf '%s' "$out" | grep -q "^daemon: yes" && ok "the installed daemon starts and answers in the container" || bad "the daemon did not answer in the container"
printf '%s' "$out" | grep -q "^shelf: installed" && ok "a pack installs from the published shelf from inside the container" || bad "the shelf install failed in the container"
printf '%s' "$out" | grep -q "^shelf: second install is a skip" && ok "a second install is a skip in the container" || bad "a second install was not a skip in the container"
printf '%s' "$out" | grep -qE "^fault: fault injection: [0-9]+ passed, 0 failed" && ok "the fault cases pass in the container" || bad "the fault cases failed in the container"

say ""
say "cross-machine (container): $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
