#!/bin/sh
# Cross-check the workspace for aarch64 without root.
#
# `libsqlite3-sys` compiles SQLite with a C compiler, so the ARM check needs a cross gcc. Installing one
# wants root; this stages the same Ubuntu packages under a prefix and points the build at them. The two
# wrappers exist because the extracted `as` and `ar` link against libbfd and libopcodes, which the loader
# cannot find without a search path, and setting that path for the whole build would follow every other
# tool too.
#
# Usage: scripts/cross-check.sh            (stages once, then checks)
#        CROSS_PREFIX=/some/dir scripts/cross-check.sh
set -eu

PREFIX=${CROSS_PREFIX:-/mnt/ssd/toolchains/cross}
TARGET=aarch64-unknown-linux-gnu
PACKAGES="gcc-aarch64-linux-gnu gcc-13-aarch64-linux-gnu cpp-13-aarch64-linux-gnu \
binutils-aarch64-linux-gnu libc6-dev-arm64-cross linux-libc-dev-arm64-cross libgcc-13-dev-arm64-cross"

export RUSTUP_HOME=${RUSTUP_HOME:-/mnt/ssd/home-cold/rust/rustup}
export CARGO_HOME=${CARGO_HOME:-/mnt/ssd/home-cold/rust/cargo}
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-/mnt/ssd/home-cold/hades-target}
PATH="$CARGO_HOME/bin:$PATH"
export PATH

if [ ! -x "$PREFIX/root/usr/bin/aarch64-linux-gnu-gcc" ]; then
    echo "staging the cross toolchain under $PREFIX"
    mkdir -p "$PREFIX/debs" "$PREFIX/root" "$PREFIX/bin"
    # `apt-get download` needs no privileges; `dpkg-deb -x` unpacks to a prefix we own.
    (cd "$PREFIX/debs" && apt-get download $PACKAGES >/dev/null)
    for deb in "$PREFIX"/debs/*.deb; do
        dpkg-deb -x "$deb" "$PREFIX/root"
    done
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
exec $PREFIX/root/usr/bin/aarch64-linux-gnu-gcc -B$PREFIX/bin "\$@"
WRAPPER

chmod +x "$PREFIX/bin/as" "$PREFIX/bin/ar" "$PREFIX/bin/cc"

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
