#!/usr/bin/env bash
# Build the release binary against an older glibc than this machine has.
#
# The binary linked on a modern host asks for GLIBC_2.39 and therefore refuses the previous LTS with a
# weak-version warning that does not stop it from running, which is worse than refusing. Building inside the
# oldest image we can get a toolchain into pins the requirement instead of inheriting it: Ubuntu 20.04 has glibc
# 2.31, and the resulting binary asks for no more than 2.30, so it runs on 20.04 and every later release.
#
# The build happens in a container, so it needs no cross toolchain on the host. It writes one file and the
# target directory it used, both under --out.
#
# usage: scripts/portable-build.sh [--out DIR] [--image ubuntu:20.04]
set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
OUT=/mnt/ssd/hades-portable
IMAGE=ubuntu:20.04

while [ $# -gt 0 ]; do
  case "$1" in
    --out) OUT=${2:?--out needs a directory}; shift 2 ;;
    --image) IMAGE=${2:?--image needs a tag}; shift 2 ;;
    *) echo "usage: portable-build.sh [--out DIR] [--image TAG]" >&2; exit 2 ;;
  esac
done

mkdir -p "$OUT"
echo "building in $IMAGE; output in $OUT"

docker run --rm -v "$OUT:/out" -v "$ROOT:/src:ro" "$IMAGE" bash -c '
set -e
export DEBIAN_FRONTEND=noninteractive
export PATH=/root/.cargo/bin:/usr/local/bin:/usr/bin:/bin
apt-get update -qq >/dev/null 2>&1 || { echo APT_UPDATE_FAILED; exit 3; }
apt-get install -y -qq curl build-essential pkg-config ca-certificates >/dev/null || { echo APT_INSTALL_FAILED; exit 3; }
curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable >/dev/null \
  || { echo RUSTUP_FAILED; exit 3; }
cd /src/rust
CARGO_TARGET_DIR=/out/target cargo build --release --locked -p harness 2>&1 | tail -4
cp /out/target/release/hades /out/hades-portable && chmod +x /out/hades-portable
echo BUILD_OK
' 2>&1 | tail -8

[ -x "$OUT/hades-portable" ] || { echo "no binary at $OUT/hades-portable"; exit 1; }
floor=$(objdump -T "$OUT/hades-portable" | grep -oE 'GLIBC_[0-9]+\.[0-9]+' | sort -Vu | tail -1)
echo "wrote: $OUT/hades-portable"
echo "glibc floor: ${floor:-unknown}"
