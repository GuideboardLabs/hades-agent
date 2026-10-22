# Portability: what runs where, and what it costs

Two artifacts, one platform claim.

| artifact | built in | asks the host for | runs on |
|---|---|---|---|
| `hades-portable` | Ubuntu 20.04 (glibc 2.31) | glibc ≥ 2.30, libgcc | Ubuntu 20.04+, Debian 11+, RHEL 9+, Fedora 33+ |
| `hades-musl` | Alpine 3.20 | nothing | any x86-64 Linux, including Alpine, NixOS and the glibc systems above |

Both are built by `scripts/portable-build.sh` in a container, from a read-only copy of the source, so the
artifact does not inherit the build machine's libc by accident. `scripts/portability.sh` checks a binary
against `ubuntu:20.04`, `22.04` and `24.04`, and `hades doctor` reports the machine's own reading.

## Why the glibc floor is 2.30 and not 2.28

2.28 was the target in the review: Debian 10 and RHEL 8. The build on `debian:10` was attempted and the
environment refused to run it, so there is no 2.28 artifact and no claim of one. 2.30 is the highest symbol the
20.04 build asks for, which covers Ubuntu 20.04+, Debian 11+, RHEL 9+ and Fedora 33+. A 2.28 build is one line
in the script on a machine that will run it; what is missing is the build, not the recipe.

## The musl build's one real cost

A static musl binary cannot `dlopen`, so the in-process libcurl pool is unavailable and fetches go through the
`curl` executable. That is not a guess: `hades doctor` prints which transport is live, and the two are timed in
`docs/performance.md`.

What it means in practice:

- On glibc systems, use `hades-portable`: full transport, no `curl` dependency.
- On Alpine or NixOS, use `hades-musl` and install `curl` and GNU `tar` (`apk add curl tar`). The fallback is
  slower per request; it is not slower in correctness, and the fetch budget, proxy clearing, `CONNECT_TO`
  pinning and body cap are the same code. GNU tar is not optional: pack archives are byte-reproducible because
  the build shells out to it, and busybox tar is a different program with a different archive. Both missing
  pieces are named in the error and in `hades doctor`.
- A musl binary missing `curl` fails with a message that names it, rather than silently succeeding slowly.

## What portability does not cover

- **Landlock** is a kernel facility, not a distribution one. A binary that runs on an old kernel may be unable
  to cage a run. `hades doctor` reports the ABI and `compatibility: reduced: landlock` when it is missing.
  Nothing silently downgrades: a governed run on a kernel without Landlock is refused, and the override is the
  existing `--cage off` flag, which says what it did.
- **ARM64** compiles and passes its tests, but there is no runtime check in this release: the build machine's
  kernel cannot execute arm64 containers. A cross-built binary that has never run is not evidence, so it is not
  claimed as supported.
- **NixOS** needs the musl artifact or `nix-ld`; the glibc binary's ELF interpreter is not at `/nix/store`. The
  musl build sidesteps the question entirely, which is why it exists.
- **Alpine** has no systemd user manager, so the service units are not available; the daemon can still be run
  directly. That is a packaging difference, stated rather than solved. Its busybox `tar` is not GNU tar, so
  pack archives need `apk add tar`; `hades doctor` reports `tar` as a capability and the failure names it.
- **Chromium** is reported as reduced when it is not installed, which on a minimal Alpine image it is not.
