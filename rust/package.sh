#!/bin/sh
# Build the Hades release tarball: the binary, the installer and the installer's own README.
#
# No upload and no signing. The checksum beside the artifact is the whole provenance claim; a signature
# would be a claim about a key, and Hades has no distribution key yet.

set -eu

usage() {
    cat <<'EOF'
usage: package.sh [--out DIR] [--bin FILE] [--target TRIPLE] [--skip-build]

  --out DIR       where dist/ lives (default: rust/dist)
  --bin FILE      package this binary instead of building
  --target TRIPLE name the artifact after this triple (default: the host's)
  --skip-build    fail rather than build when there is no binary
EOF
}

die() {
    printf 'package: %s\n' "$1" >&2
    exit 1
}

SCRIPT_DIR=$(CDPATH= cd "$(dirname "$0")" && pwd)
OUT="$SCRIPT_DIR/dist"
BIN=""
TARGET_LABEL=""
SKIP=0

while [ $# -gt 0 ]; do
    case "$1" in
        --out) [ $# -ge 2 ] || die "--out needs a directory"; OUT=$2; shift 2 ;;
        --bin) [ $# -ge 2 ] || die "--bin needs a file"; BIN=$2; shift 2 ;;
        --target) [ $# -ge 2 ] || die "--target needs a triple"; TARGET_LABEL=$2; shift 2 ;;
        --skip-build) SKIP=1; shift ;;
        --help|-h) usage; exit 0 ;;
        *) die "unknown option: $1 (try --help)" ;;
    esac
done

VERSION=$(sed -n 's/^version *= *"\([0-9][^"]*\)".*/\1/p' "$SCRIPT_DIR/harness/Cargo.toml" | head -1)
[ -n "$VERSION" ] || die "could not read the version from harness/Cargo.toml"

case "$(uname -m)" in
    x86_64) ARCH=x86_64 ;;
    aarch64|arm64) ARCH=aarch64 ;;
    *) ARCH=$(uname -m) ;;
esac
TRIPLE="$ARCH-unknown-linux-gnu"
# The musl build is the same architecture and a different libc, so the name has to say which one it is.
[ -n "$TARGET_LABEL" ] && TRIPLE="$TARGET_LABEL"
NAME="hades-$VERSION-$TRIPLE"

if [ -z "$BIN" ]; then
    if [ "$SKIP" -eq 1 ]; then
        die "--skip-build needs --bin: there is no binary to package"
    fi
    command -v cargo >/dev/null 2>&1 || die "no cargo on PATH; pass --bin <file> or build elsewhere"
    ( cd "$SCRIPT_DIR" && cargo build --release --locked ) || die "the release build failed"
    TARGET=$( cd "$SCRIPT_DIR" && cargo metadata --format-version 1 --no-deps 2>/dev/null \
        | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p' | head -1 )
    [ -n "$TARGET" ] || TARGET="$SCRIPT_DIR/target"
    BIN="$TARGET/release/hades"
fi
[ -f "$BIN" ] || die "$BIN does not exist"
[ -x "$BIN" ] || die "$BIN is not executable"

STAGE_ROOT="$OUT/stage"
STAGE="$STAGE_ROOT/$NAME"
rm -rf "$STAGE_ROOT"
mkdir -p "$STAGE/bin"
cp "$BIN" "$STAGE/bin/hades"
chmod 755 "$STAGE/bin/hades"
cp "$SCRIPT_DIR/install.sh" "$STAGE/install.sh"
chmod 755 "$STAGE/install.sh"
cp "$SCRIPT_DIR/README.install.md" "$STAGE/README.install.md"
( cd "$STAGE" && sha256sum bin/hades install.sh README.install.md > SHA256SUMS )

mkdir -p "$OUT"
TARBALL="$OUT/$NAME.tar.gz"
rm -f "$TARBALL"
if tar --version 2>/dev/null | grep -q 'GNU tar'; then
    tar --sort=name --mtime='@0' --owner=0 --group=0 --numeric-owner \
        -C "$STAGE_ROOT" -cf - "$NAME" | gzip -n > "$TARBALL"
else
    tar -C "$STAGE_ROOT" -czf "$TARBALL" "$NAME"
fi
( cd "$OUT" && sha256sum "$NAME.tar.gz" > "$NAME.tar.gz.sha256" )

printf 'wrote:  %s\n' "$TARBALL"
printf 'sha256: %s\n' "$( sha256sum "$TARBALL" | cut -d' ' -f1 )"
