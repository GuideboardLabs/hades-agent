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
# The cross gcc series follows the distribution: 13 on 24.04, 15 and 16 on newer images. Two things have to
# agree, and they are separate packages: `gcc-<series>-aarch64-linux-gnu` carries the compiler, while
# `gcc-aarch64-linux-gnu` is a meta package whose only content is symlinks to the series *its own version*
# chooses. Taking the first series the archive carries made those disagree on a 26.04 image: staging fetched
# 21 MB of gcc-13 and the meta package then left `aarch64-linux-gnu-gcc` pointing at gcc-15, which was never
# downloaded. The generic name was a dangling link, `-x` was false, and the job reported "the staged tree has
# no aarch64-linux-gnu-gcc" while a perfectly good compiler sat beside it. Ask the meta package which series
# it depends on, so the symlinks and the packages are chosen together, and require the series binary itself
# rather than the name the meta package owns.
GCC_SERIES=""
if apt-cache show gcc-aarch64-linux-gnu >/dev/null 2>&1; then
    GCC_SERIES=$(apt-cache depends gcc-aarch64-linux-gnu 2>/dev/null |
        sed -n 's/.*Depends:[[:space:]]*gcc-\([0-9][0-9]*\)-aarch64-linux-gnu.*/\1/p' | head -n 1)
fi
if [ -z "$GCC_SERIES" ]; then
    for series in 16 15 14 13 12; do
        if apt-cache policy "gcc-$series-aarch64-linux-gnu" 2>/dev/null | grep -q 'Candidate: [0-9]'; then
            GCC_SERIES=$series
            break
        fi
    done
fi
if [ -z "$GCC_SERIES" ]; then
    echo "no gcc-<series>-aarch64-linux-gnu package is available; install the ARM64 cross toolchain or set CROSS_PREFIX to a staged one" >&2
    exit 1
fi
GCC_BIN="$PREFIX/root/usr/bin/aarch64-linux-gnu-gcc-$GCC_SERIES"
# The cross compiler is an x86 binary with x86 dependencies of its own: `cc1` wants libisl/libmpc/libmpfr and
# `as` wants libsframe, none of which the runner image is obliged to ship. Staging only the -cross packages
# left every one of them to the image, so a distribution bump could break the job with a missing soname that
# reads as a build error. Take the host-side dependencies from apt itself — the soname follows the gcc
# series (libisl23 today, another name tomorrow), so naming them by hand would break next time — and put
# them in the same prefix, without dragging in glibc or the C++ runtime that belong to the host.
HOST_PACKAGES=""
for pkg in "gcc-$GCC_SERIES-aarch64-linux-gnu" "cpp-$GCC_SERIES-aarch64-linux-gnu" \
    binutils-aarch64-linux-gnu; do
    for dep in $(apt-cache depends "$pkg" 2>/dev/null | sed -n 's/^[[:space:]]*[|]*Depends:[[:space:]]*//p'); do
        case "$dep" in
            *-cross* | libc6 | libc6-dev | libstdc++6 | libgcc-s1) continue ;;
        esac
        case " $HOST_PACKAGES " in
            *" $dep "*) ;;
            *) HOST_PACKAGES="$HOST_PACKAGES $dep" ;;
        esac
    done
done
PACKAGES="gcc-aarch64-linux-gnu gcc-$GCC_SERIES-aarch64-linux-gnu cpp-$GCC_SERIES-aarch64-linux-gnu \
binutils-aarch64-linux-gnu libc6-dev-arm64-cross libc6-arm64-cross linux-libc-dev-arm64-cross \
libgcc-$GCC_SERIES-dev-arm64-cross libgcc-s1-arm64-cross$HOST_PACKAGES"
# The host-side set repeats what the cross packages already depend on; download each name once.
UNIQUE=""
for pkg in $PACKAGES; do
    case " $UNIQUE " in
        *" $pkg "*) ;;
        *) UNIQUE="$UNIQUE $pkg" ;;
    esac
done
PACKAGES=$UNIQUE

if ! command -v cargo >/dev/null 2>&1; then
    echo "cargo is not on PATH" >&2
    exit 1
fi

if [ ! -x "$GCC_BIN" ]; then
    echo "staging the cross toolchain under $PREFIX (gcc-$GCC_SERIES)"
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
if [ ! -x "$GCC_BIN" ]; then
    # Name the two things that matter: which compiler was expected, and what the staging actually produced.
    # "the cross toolchain could not be staged" sent the last reader looking at the network when the fault was
    # one package's symlink pointing at a series nobody downloaded.
    echo "cross-check: expected the aarch64 compiler at $GCC_BIN after staging gcc-$GCC_SERIES-aarch64-linux-gnu" >&2
    echo "cross-check: the staged tree has: $(ls "$PREFIX/root/usr/bin" 2>/dev/null | grep -E '^aarch64-linux-gnu-(gcc|as|ar)' | tr '\n' ' ')" >&2
    exit 1
fi

cat > "$PREFIX/bin/as" <<WRAPPER
#!/bin/sh
exec env LD_LIBRARY_PATH=$PREFIX/root/usr/lib/x86_64-linux-gnu:$PREFIX/root/lib/x86_64-linux-gnu \\
    $PREFIX/root/usr/bin/aarch64-linux-gnu-as "\$@"
WRAPPER

cat > "$PREFIX/bin/ar" <<WRAPPER
#!/bin/sh
exec env LD_LIBRARY_PATH=$PREFIX/root/usr/lib/x86_64-linux-gnu:$PREFIX/root/lib/x86_64-linux-gnu \\
    $PREFIX/root/usr/bin/aarch64-linux-gnu-ar "\$@"
WRAPPER

# The series binary, not the generic name: `aarch64-linux-gnu-gcc` belongs to the meta package and points at
# whichever series that package's own version chose, which is a dangling link whenever the series differs.
# The staged library path comes first so `cc1` finds the same libisl the driver was staged with.
#
# `-fuse-ld=bfd` is appended last, after the caller's arguments, because gcc takes the last `-fuse-ld` and
# rustc asks for lld: it passes `-fuse-ld=lld` plus a `-B` at `lib/rustlib/<host>/bin/gcc-ld`, which is where
# its own `ld.lld` lives. lld resolves library search paths against `--sysroot` but not the absolute paths
# inside a linker script, and Debian 11's `libc.so` is one: `GROUP ( /lib/aarch64-linux-gnu/libc.so.6 ... )`
# made it look for that path on the build host and the link died with `cannot open
# /lib/aarch64-linux-gnu/libc.so.6`. GNU ld, which the staged binutils ships and this wrapper names, prefixes
# those paths with the sysroot. A staged toolchain links with the linker it staged.
cat > "$PREFIX/bin/cc" <<WRAPPER
#!/bin/sh
exec env LD_LIBRARY_PATH=$PREFIX/root/usr/lib/x86_64-linux-gnu:$PREFIX/root/lib/x86_64-linux-gnu \\
    $GCC_BIN --sysroot=\${HADES_CROSS_SYSROOT:-$PREFIX/root} -B$PREFIX/bin "\$@" -fuse-ld=bfd
WRAPPER

chmod +x "$PREFIX/bin/as" "$PREFIX/bin/ar" "$PREFIX/bin/cc"

# A compiler that exists is not a compiler that works, and a staging step that only checks for a file reports
# its own fault from inside `cargo check`, where it reads as a source error. Compile one file and read the
# machine out of the object: e_machine 0xb7 is AArch64.
printf 'int hades_cross_probe(void) { return 0; }\n' >"$PREFIX/probe.c"
if ! "$PREFIX/bin/cc" -c -o "$PREFIX/probe.o" "$PREFIX/probe.c" >"$PREFIX/probe.log" 2>&1; then
    echo "cross-check: the staged compiler at $GCC_BIN cannot compile a C file:" >&2
    head -n 3 "$PREFIX/probe.log" >&2
    exit 1
fi
if [ "$(od -An -tx1 -j18 -N2 "$PREFIX/probe.o" | tr -d ' \n')" != "b700" ]; then
    echo "cross-check: the staged compiler produced an object that is not AArch64" >&2
    exit 1
fi

if [ "$STAGE_ONLY" -eq 1 ]; then
    echo "staged gcc-$GCC_SERIES under $PREFIX; it compiles an AArch64 object"
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
