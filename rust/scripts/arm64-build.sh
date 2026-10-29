#!/usr/bin/env bash
# Build the two ARM64 release artifacts and run them.
#
# The cross toolchains are staged without root, the same way `cross-check.sh` stages gcc: `apt-get download`
# plus `dpkg-deb -x` for the Debian/Ubuntu packages, and the upstream musl.cc tarball for the static build.
# qemu-user is staged the same way and is what makes "ARM64 support" a measured claim rather than a compile.
#
# usage: arm64-build.sh [--out DIR] [--flavour gnu|musl|both]
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
OUT=$ROOT/dist
FLAVOUR=both
PREFIX=${HADES_CROSS_PREFIX:-${XDG_CACHE_HOME:-$HOME/.cache}/hades-cross}
CARGO=${CARGO:-cargo}

while [ $# -gt 0 ]; do
  case "$1" in
    --out) OUT=${2:?--out needs a directory}; shift 2 ;;
    --flavour) FLAVOUR=${2:?--flavour needs gnu, musl or both}; shift 2 ;;
    *) echo "usage: arm64-build.sh [--out DIR] [--flavour gnu|musl|both]" >&2; exit 2 ;;
  esac
done

case "$FLAVOUR" in
  gnu | musl | both) ;;
  *) echo "unknown flavour: $FLAVOUR" >&2; exit 2 ;;
esac

mkdir -p "$OUT"
OUT=$(cd "$OUT" && pwd -P)

# A build script is run from wherever the caller stands; the workspace is beside this script.
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/rust/target}
MANIFEST=$ROOT/rust/Cargo.toml

rustup target list --installed | grep -qx aarch64-unknown-linux-gnu ||
  rustup target add aarch64-unknown-linux-gnu

# A glibc binary is only as portable as the libc it was linked against. Linking against this host's Ubuntu
# 24.04 sysroot produces GLIBC_2.39, which excludes Ubuntu 20.04/22.04 and Debian 11/12 on ARM64. Debian 11
# ships glibc 2.31, the same generation as Ubuntu 20.04, so its arm64 libc and headers are staged and the
# compiler is pointed at them. No root: the .deb files are unpacked under the cache.
stage_bullseye() {
  local root=$PREFIX/bullseye
  if [ -f "$root/.staged" ]; then
    return 0
  fi
  local base=https://deb.debian.org/debian
  local dists=dists/bullseye/main/binary-arm64
  mkdir -p "$root/debs"
  echo "staging the Debian 11 arm64 sysroot under $root"
  curl -sSL --max-time 120 "$base/$dists/Packages.gz" -o "$root/Packages.gz"
  gzip -dc "$root/Packages.gz" >"$root/Packages"
  for package in libc6-dev libc6 libgcc-s1 linux-libc-dev; do
    local file
    file=$(awk -v want="$package" '
      /^Package: / { name = $2 }
      name == want && /^Filename: / { print $2; exit }
    ' "$root/Packages")
    [ -n "$file" ] || {
      echo "FAIL no $package in the bullseye arm64 index" >&2
      exit 1
    }
    curl -sSL --max-time 120 "$base/$file" -o "$root/debs/$(basename "$file")"
    dpkg-deb -x "$root/debs/$(basename "$file")" "$root"
  done
  # Debian's -dev package ships absolute symlinks (`libm.so -> /lib/aarch64-linux-gnu/libm.so.6`). An absolute
  # link is resolved by the host filesystem, not by `--sysroot`, so the cross linker looks on the build
  # machine for an ARM64 libm and fails. Re-point every absolute link inside the sysroot at its own tree.
  python3 - "$root" <<'PY'
import os, sys
root = sys.argv[1]
for base, _, files in os.walk(root):
    for name in files:
        path = os.path.join(base, name)
        if not os.path.islink(path):
            continue
        target = os.readlink(path)
        if target.startswith("/"):
            os.unlink(path)
            os.symlink(os.path.relpath(os.path.join(root, target.lstrip("/")), base), path)
PY
  # The cross toolchain is unpacked from a .deb built for `/usr`, and its `libm.so`/`libc.so` linker scripts
  # name absolute `/usr/aarch64-linux-gnu/...` paths. `--sysroot` does not rewrite the contents of a script,
  # so the linker reads an x86-64 path that does not exist. Write the two scripts against this sysroot.
  lib="$PREFIX/root/usr/aarch64-linux-gnu/lib"
  # A fresh runner's cross toolchain has the compiler at `usr/bin` but may not have the cross libc package
  # that creates this directory; the sysroot's own libraries are reached with -L, so the directory only has
  # to exist for the two scripts below.
  mkdir -p "$lib"
  mvec=""
  if [ -e "$root/lib/aarch64-linux-gnu/libmvec.so.1" ]; then
    mvec=" AS_NEEDED ( $root/lib/aarch64-linux-gnu/libmvec.so.1 )"
  fi
  cat >"$lib/libm.so" <<EOF
/* GNU ld script
*/
OUTPUT_FORMAT(elf64-littleaarch64)
GROUP ( $root/lib/aarch64-linux-gnu/libm.so.6$mvec )
EOF
  cat >"$lib/libc.so" <<EOF
/* GNU ld script
   Use the shared library, but some functions are only in
   the static library, so try that secondarily.  */
OUTPUT_FORMAT(elf64-littleaarch64)
GROUP ( $root/lib/aarch64-linux-gnu/libc.so.6 $root/usr/lib/aarch64-linux-gnu/libc_nonshared.a  AS_NEEDED ( $root/lib/aarch64-linux-gnu/ld-linux-aarch64.so.1 ) )
EOF
  # The cross toolchain's own libgcc_s was built against the build host's glibc (2.39 here); linking it adds
  # references to pthread_*@GLIBC_2.34 that the 2.31 sysroot cannot satisfy. The bullseye libgcc_s is the one
  # built for this libc, so it is the one the link step must find.
  cp -f "$root/lib/aarch64-linux-gnu/libgcc_s.so.1" "$PREFIX/root/usr/aarch64-linux-gnu/lib/libgcc_s.so.1"
  touch "$root/.staged"
}

build_gnu() {
  stage_bullseye
  # The wrapper generated by cross-check.sh bakes this value in; exporting it here is what makes the linker
  # resolve /lib/aarch64-linux-gnu/* inside the staged Debian 11 tree.
  HADES_CROSS_SYSROOT=$PREFIX/bullseye bash "$HERE/cross-check.sh" --stage-only
  local name=hades-arm64-gnu
  rm -f "$OUT/$name"
  # The sysroot is not part of rustc's fingerprint: without this, the C objects and the final link from the
  # previous sysroot are reused and the floor assertion measures the old build.
  "$CARGO" clean --release --target aarch64-unknown-linux-gnu -p harness -p libsqlite3-sys >/dev/null 2>&1 || true
  CC_aarch64_unknown_linux_gnu=$PREFIX/bin/cc \
    AR_aarch64_unknown_linux_gnu=$PREFIX/bin/ar \
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=$PREFIX/bin/cc \
    RUSTFLAGS="${RUSTFLAGS:-} -L native=$PREFIX/bullseye/lib/aarch64-linux-gnu -L native=$PREFIX/bullseye/usr/lib/aarch64-linux-gnu" \
    "$CARGO" build --release --locked --manifest-path "$MANIFEST" -p harness --target aarch64-unknown-linux-gnu
  cp "$CARGO_TARGET_DIR/aarch64-unknown-linux-gnu/release/hades" "$OUT/$name"
  chmod 755 "$OUT/$name"
  file -b "$OUT/$name" | grep -q 'ARM aarch64' || {
    echo "FAIL $name is not an ARM64 binary: $(file -b "$OUT/$name")" >&2
    exit 1
  }
  LD_LIBRARY_PATH=$PREFIX/root/usr/lib/x86_64-linux-gnu \
    OBJDUMP=$PREFIX/root/usr/bin/aarch64-linux-gnu-objdump \
    HADES_ASSERT_NO_RUN=1 \
    "$HERE/assert-floor.sh" "$OUT/$name" --max-glibc 2.31
  "$HERE/arm64-check.sh" "$OUT/$name" --require
}

build_musl() {
  rustup target list --installed | grep -qx aarch64-unknown-linux-musl ||
    rustup target add aarch64-unknown-linux-musl
  local musl=$PREFIX/musl
  local cc ar flags name=hades-arm64-musl
  if [ -x "$musl/bin/aarch64-linux-musl-gcc" ]; then
    cc=$musl/bin/aarch64-linux-musl-gcc
    ar=$musl/bin/aarch64-linux-musl-ar
    flags="-C target-feature=+crt-static"
  elif python3 -m ziglang version >/dev/null 2>&1; then
    # GitHub-hosted runners cannot reach musl.cc (the connection times out), and the release must not depend
    # on one CDN being reachable from one network. Zig ships a C compiler and a musl libc for this target and
    # is installable from PyPI, which the runners can reach. Rust's own self-contained crt objects collide
    # with Zig's, so that arm is turned off for this link.
    mkdir -p "$PREFIX/zig"
    cat >"$PREFIX/zig/cc" <<'WRAPPER'
#!/usr/bin/env bash
# cc-rs passes `--target=<triple>` and rustc passes the Cortex-A53 erratum workaround; Zig spells the target
# `-target` and its linker rejects that workaround, so both are filtered here.
args=()
for a in "$@"; do
  case "$a" in
    --target=*|-Wl,--fix-cortex-a53-843419) continue ;;
  esac
  args+=("$a")
done
exec python3 -m ziglang cc -target aarch64-linux-musl "${args[@]}"
WRAPPER
    chmod +x "$PREFIX/zig/cc"
    cc=$PREFIX/zig/cc
    ar=ar
    flags="-C target-feature=+crt-static -C link-self-contained=no"
  else
    mkdir -p "$PREFIX"
    echo "staging the aarch64 musl toolchain from musl.cc"
    if curl -sSL --max-time 90 https://musl.cc/aarch64-linux-musl-cross.tgz | tar xz -C "$PREFIX"; then
      rm -rf "$musl"
      mv "$PREFIX/aarch64-linux-musl-cross" "$musl"
      cc=$musl/bin/aarch64-linux-musl-gcc
      ar=$musl/bin/aarch64-linux-musl-ar
      flags="-C target-feature=+crt-static"
    else
      echo "no aarch64 musl C compiler: musl.cc is unreachable and ziglang is not installed" >&2
      echo "install one with 'pip install ziglang', or stage the toolchain under $musl" >&2
      exit 1
    fi
  fi
  rm -f "$OUT/$name"
  CC_aarch64_unknown_linux_musl=$cc \
    AR_aarch64_unknown_linux_musl=$ar \
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=$cc \
    RUSTFLAGS="${RUSTFLAGS:-} $flags" \
    "$CARGO" build --release --locked --manifest-path "$MANIFEST" -p harness --target aarch64-unknown-linux-musl
  cp "$CARGO_TARGET_DIR/aarch64-unknown-linux-musl/release/hades" "$OUT/$name"
  chmod 755 "$OUT/$name"
  file -b "$OUT/$name" | grep -q 'ARM aarch64' || {
    echo "FAIL $name is not an ARM64 binary: $(file -b "$OUT/$name")" >&2
    exit 1
  }
  HADES_ASSERT_NO_RUN=1 "$HERE/assert-floor.sh" "$OUT/$name" --static
  # The artifact is static, not static-PIE: the static-PIE form segfaults under qemu-user on this host, and a
  # release gate that cannot run its own artifact is worse than one that ships the form it can run. The claim
  # in the notes is "static", and this checks it: no interpreter, no glibc.
  "$HERE/arm64-check.sh" "$OUT/$name" --require
}

case "$FLAVOUR" in
  gnu) build_gnu ;;
  musl) build_musl ;;
  both)
    build_gnu
    build_musl
    ;;
esac

echo "ARM64 artifacts in $OUT:"
ls -l "$OUT"/hades-arm64-*
