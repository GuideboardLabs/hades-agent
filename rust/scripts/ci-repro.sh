#!/bin/sh
# Run the gate in the image CI uses, with a similar core count, so a test that depends on this machine fails here
# first.
#
# The workflow's failing targets were correct locally and wrong on the runner. That is the definition of a
# host-dependent test, and the only way to catch it before pushing is to run where the runner runs: a stock
# Ubuntu, two cores, no browser, nothing installed beyond a toolchain.
#
# usage: scripts/ci-repro.sh [--image TAG] [--cpus N] [-- <cargo test args>]
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
IMAGE=ubuntu:24.04
CPUS=2
OUT=${OUT:-${TMPDIR:-/tmp}/hades-ci}

while [ $# -gt 0 ]; do
  case "$1" in
    --image) IMAGE=${2:?--image needs a tag}; shift 2 ;;
    --cpus) CPUS=${2:?--cpus needs a number}; shift 2 ;;
    --) shift; break ;;
    *) break ;;
  esac
done

mkdir -p "$OUT"
echo "gate in $IMAGE with $CPUS cpus; target dir $OUT"

docker run --rm --cpus "$CPUS" \
  -v "$OUT:/out" \
  -v "$ROOT:/src:ro" \
  "$IMAGE" bash -c '
set -e
export DEBIAN_FRONTEND=noninteractive
export PATH=/root/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
apt-get update -qq >/dev/null
apt-get install -y -qq curl build-essential pkg-config ca-certificates >/dev/null
if ! command -v cargo >/dev/null 2>&1; then
  curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable >/dev/null
fi
rustup component add clippy rustfmt >/dev/null 2>&1 || true
cd /src/rust
export CARGO_TARGET_DIR=/out/target
cargo test --workspace 2>&1 | grep -E "^(test |failures:|test result: FAILED|error\[|error:)" | grep -v "^test .* \.\.\. ok$" | tail -40
' "$@"
