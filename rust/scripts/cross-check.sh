#!/bin/sh
# Cross-check the workspace for aarch64 without root.
#
# `libsqlite3-sys` compiles SQLite with a C compiler, so the ARM check needs a cross gcc. Installing one
# wants root; this stages the same distribution packages under a prefix the user owns and points the build
# at them. The two
# wrappers exist because the extracted `as` and `ar` link against libbfd and libopcodes, which the loader
# cannot find without a search path, and setting that path for the whole build would follow every other
# tool too.
#
# Everything machine-specific comes from the environment: `CROSS_PREFIX` says where to stage, and the
# toolchain is whatever `cargo` is on `PATH`.
#
# Usage: scripts/cross-check.sh            (stages once, then checks)
#        scripts/cross-check.sh --stage-only
#        CROSS_PREFIX=/some/dir scripts/cross-check.sh
set -eu

STAGE_ONLY=0
case "${1:-}" in
    --stage-only) STAGE_ONLY=1 ;;
    "") ;;
    *) echo "usage: cross-check.sh [--stage-only]" >&2; exit 2 ;;
esac

PREFIX=${CROSS_PREFIX:-${XDG_CACHE_HOME:-$HOME/.cache}/hades-cross}
TARGET=aarch64-unknown-linux-gnu
# The cross gcc series follows the distribution: 13 on 24.04, 14 or 15 on newer images. `ubuntu-latest` moved
# under us once already, and a hardcoded 13 made the whole staging step fail on a runner where every other
# package existed. Ask apt which series is present instead of assuming.
GCC_SERIES=""
for series in 13 14 15 16 12; do
    if apt-cache policy "gcc-$series-aarch64-linux-gnu" 2>/dev/null | grep -q 'Candidate: [0-9]'; then
        GCC_SERIES=$series
        break
    fi
done
if [ -z "$GCC_SERIES" ]; then
    echo "no gcc-<series>-aarch64-linux-gnu package is available; install the ARM64 cross toolchain or set CROSS_PREFIX to a staged one" >&2
    exit 1
fi
PACKAGES="gcc-aarch64-linux-gnu gcc-$GCC_SERIES-aarch64-linux-gnu cpp-$GCC_SERIES-aarch64-linux-gnu \
binutils-aarch64-linux-gnu libc6-dev-arm64-cross libc6-arm64-cross linux-libc-dev-arm64-cross \
libgcc-$GCC_SERIES-dev-arm64-cross libgcc-s1-arm64-cross"

if ! command -v cargo >/dev/null 2>&1; then
    echo "cargo is not on PATH" >&2
    exit 1
fi

if [ ! -x "$PREFIX/root/usr/bin/aarch64-linux-gnu-gcc" ]; then
    echo "staging the cross toolchain under $PREFIX"
    mkdir -p "$PREFIX/debs" "$PREFIX/root" "$PREFIX/bin"
    # `apt-get download` needs no privileges; `dpkg-deb -x` unpacks to a prefix we own. One package missing on
    # a newer image should be named and skipped, not take the whole step down before anything is compiled;
    # the check below is what decides whether the staging is usable.
    # A fresh runner's package lists can be old enough that every download fails, and the old form of this
    # loop swallowed the reason: the job then died on "the staged tree has no aarch64-linux-gnu-gcc" with no
    # word about why. Refresh the lists when we are allowed to, and keep the last line of each failure.
    sudo -n apt-get update -qq >/dev/null 2>&1 || apt-get update -qq >/dev/null 2>&1 || true
    (cd "$PREFIX/debs" && for package in $PACKAGES; do
        if ! apt-get download "$package" >"$package.log" 2>&1; then
            echo "cross-check: apt-get download $package failed: $(tail -n 1 "$package.log" 2>/dev/null)"
        fi
    done)
    for deb in "$PREFIX"/debs/*.deb; do
        [ -f "$deb" ] || continue
        dpkg-deb -x "$deb" "$PREFIX/root"
    done
fi
if [ ! -x "$PREFIX/root/usr/bin/aarch64-linux-gnu-gcc" ]; then
    echo "the staged tree has no aarch64-linux-gnu-gcc; the cross toolchain could not be staged" >&2
    exit 1
fi

cat > "$PREFIX/bin/as" <<WRAPPER
#!/bin/sh
exec env LD_LIBRARY_PATH=$PREFIX/root/usr/lib/x86_64-linux-gnu \\
    $PREFIX/root/usr/bin/aarch64-linux-gnu-as "\$@"
WRAPPER

cat > "$PREFIX/bin/ar" <<WRAPPER
#!/bin/sh
exec env LD_LIBRARY_PATH=$PREFIX/root/usr/lib/x86_64-linux-gnu \\
    $PREFIX/root/usr/bin/aarch64-linux-gnu-ar "\$@"
WRAPPER

cat > "$PREFIX/bin/cc" <<WRAPPER
#!/bin/sh
exec $PREFIX/root/usr/bin/aarch64-linux-gnu-gcc --sysroot=${HADES_CROSS_SYSROOT:-$PREFIX/root} -B$PREFIX/bin "\$@"
WRAPPER

chmod +x "$PREFIX/bin/as" "$PREFIX/bin/ar" "$PREFIX/bin/cc"

if [ "$STAGE_ONLY" -eq 1 ]; then
    exit 0
fi

# The cc crate reads `CC_<target>`, and a shell cannot put a hyphen in a variable name, so the target is
# translated once. `sh` here is dash, which has no `${var//-/_}`.
CC_NAME="CC_$(echo "$TARGET" | tr '-' '_')"
AR_NAME="AR_$(echo "$TARGET" | tr '-' '_')"
eval "$CC_NAME=\$PREFIX/bin/cc"
eval "$AR_NAME=\$PREFIX/bin/ar"
export "$CC_NAME" "$AR_NAME"

cd "$(dirname "$0")/.."
echo "checking the workspace for $TARGET"
cargo check --workspace --target "$TARGET"
