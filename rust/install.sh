#!/bin/sh
# Hades, installed in one command.
#
# This script copies a binary, initialises a home once, writes the two systemd user units and starts the
# daemon. It never runs sudo, never downloads anything, and never reads another agent's install.
#
# Two layouts are supported: a release tarball (`bin/hades` beside this script) and a checkout
# (`Cargo.toml` here or one directory up), which is built with the caller's toolchain.

set -eu

usage() {
    cat <<'EOF'
usage: install.sh [options]

  --prefix DIR     where the binary goes (default: ~/.local)
  --home DIR       Hades' own home (default: ~/.hades or HADES_HOME)
  --root DIR       the working tree the agent runs in (default: ~/hades-work)
  --bin FILE       install this binary instead of building or using bin/hades
  --build          build from source even if a packaged binary is present
  --name NAME      the agent's name for `hades init` (default: Hades)
  --goal TEXT      what the work is for
  --topics a,b     the topics the charter starts with
  --no-init        do not run `hades init`
  --no-service     do not write the systemd user units
  --no-start       write the units but do not enable or start the daemon
  --with-gateway   also enable and start the Telegram gateway unit
  --help           this text

The daemon is started when a systemd user manager answers. The gateway is not started unless
--with-gateway is given, because it needs HADES_TELEGRAM_TOKEN and a `gateway.telegram` section
first. A failed gateway's first line is in `journalctl --user -u hades-gateway`.
EOF
}

die() {
    printf 'install: %s\n' "$1" >&2
    exit "${2:-1}"
}

say() {
    printf '%s\n' "$1"
}

[ -n "${HOME:-}" ] || die "no HOME in the environment; pass --prefix and --home" 2

PREFIX="${HOME%/}/.local"
HOME_DIR="${HADES_HOME:-${HOME%/}/.hades}"
ROOT="${HOME%/}/hades-work"
BIN=""
BUILD=0
DO_INIT=1
DO_SERVICE=1
DO_START=1
WITH_GATEWAY=0
NAME=""
GOAL=""
TOPICS=""

while [ $# -gt 0 ]; do
    case "$1" in
        --prefix) [ $# -ge 2 ] || die "--prefix needs a directory" 2; PREFIX=$2; shift 2 ;;
        --home) [ $# -ge 2 ] || die "--home needs a directory" 2; HOME_DIR=$2; shift 2 ;;
        --root) [ $# -ge 2 ] || die "--root needs a directory" 2; ROOT=$2; shift 2 ;;
        --bin) [ $# -ge 2 ] || die "--bin needs a file" 2; BIN=$2; shift 2 ;;
        --name) [ $# -ge 2 ] || die "--name needs a value" 2; NAME=$2; shift 2 ;;
        --goal) [ $# -ge 2 ] || die "--goal needs a value" 2; GOAL=$2; shift 2 ;;
        --topics) [ $# -ge 2 ] || die "--topics needs a value" 2; TOPICS=$2; shift 2 ;;
        --build) BUILD=1; shift ;;
        --no-init) DO_INIT=0; shift ;;
        --no-service) DO_SERVICE=0; shift ;;
        --no-start) DO_START=0; shift ;;
        --with-gateway) WITH_GATEWAY=1; shift ;;
        --help|-h) usage; exit 0 ;;
        *) die "unknown option: $1 (try --help)" 2 ;;
    esac
done

SCRIPT_DIR=$(CDPATH= cd "$(dirname "$0")" && pwd)

if [ "$WITH_GATEWAY" -eq 1 ] && [ "$DO_SERVICE" -eq 0 ]; then
    die "--with-gateway needs the units; drop --no-service" 2
fi
if [ "$WITH_GATEWAY" -eq 1 ] && [ "$DO_START" -eq 0 ]; then
    die "--with-gateway says start it; drop --no-start" 2
fi
if [ "$WITH_GATEWAY" -eq 1 ]; then
    token_found=0
    if [ -n "${HADES_TELEGRAM_TOKEN:-}" ]; then
        token_found=1
    elif [ -f "$HOME_DIR/.env" ] && grep -q '^HADES_TELEGRAM_TOKEN=' "$HOME_DIR/.env"; then
        token_found=1
    fi
    [ "$token_found" -eq 1 ] \
        || die "--with-gateway needs HADES_TELEGRAM_TOKEN in the environment or in $HOME_DIR/.env" 2
fi

# Another agent's home and a notes vault are refused here as well as in the binary: the installer is the first
# thing a person runs, and a refusal that comes after a binary was copied and a work root made is a refusal
# that has already written.
#
# The vault walk resolves the path first. A symlink into a vault carries no marker in its own name, and a note
# deep in a tree carries no marker above it for the first few parents; both were misses in the binary's guard,
# so both are checked here with the same rule: the real path, and every parent to the root.
resolve_path() {
    case "$1" in
        /*) p=$1 ;;
        *) p="$PWD/$1" ;;
    esac
    out=""
    rest=${p#/}
    hops=0
    while [ -n "$rest" ]; do
        comp=${rest%%/*}
        case "$rest" in
            */*) rest=${rest#*/} ;;
            *) rest="" ;;
        esac
        case "$comp" in
            ""|.) continue ;;
            ..)
                out=${out%/*}
                continue
                ;;
        esac
        candidate="$out/$comp"
        if [ -L "$candidate" ]; then
            hops=$((hops + 1))
            if [ "$hops" -gt 40 ]; then
                # A loop the kernel would refuse with ELOOP. Keep the spelling; nothing can be written through it.
                out="$candidate"
                continue
            fi
            target=$(readlink "$candidate")
            case "$target" in
                /*) out=""; rest="${target#/}${rest:+/$rest}" ;;
                *) rest="$target${rest:+/$rest}" ;;
            esac
            continue
        fi
        out="$candidate"
    done
    printf '%s' "${out:-/}"
}

is_vault() {
    dir=$(resolve_path "$1")
    while :; do
        if [ -d "$dir/.obsidian" ]; then
            return 0
        fi
        [ "$dir" = "/" ] && return 1
        parent=$(dirname "$dir")
        [ "$parent" = "$dir" ] && return 1
        dir=$parent
    done
}

is_below() {
    case "$1" in
        "$2"|"$2"/*) return 0 ;;
    esac
    return 1
}

refuse_crossing() {
    label=$1
    dir=$2
    if is_below "$(resolve_path "$dir")" "$AGENT_HOME"; then
        die "refusing $dir: it belongs to another agent. Hades keeps its own state; pass a different $label" 2
    fi
    if is_vault "$dir"; then
        die "refusing $dir: it is somebody's notes vault. Pass a different $label" 2
    fi
}

AGENT_HOME=$(resolve_path "${HOME%/}/.hermes")
refuse_crossing --home "$HOME_DIR"
# The work root and the prefix are written too: a root made inside a vault is a refusal that has already
# written, and a binary copied into one is worse.
refuse_crossing --root "$ROOT"
refuse_crossing --prefix "$PREFIX"

# A half-finished init leaves a populated vault and no config. `hades init` refuses that by design, so the
# installer names the state before it copies a binary rather than relaying a failure after one.
if [ ! -f "$HOME_DIR/config.json" ] && [ -d "$HOME_DIR/mneme" ] && [ -n "$(ls -A "$HOME_DIR/mneme" 2>/dev/null)" ]; then
    die "refusing $HOME_DIR: it has a vault but no config.json (a half-finished init). Move the old home aside, or pass --home <another dir>" 2
fi

# 1. Resolve the binary.
if [ -n "$BIN" ]; then
    [ -f "$BIN" ] || die "--bin $BIN does not exist" 2
    [ -x "$BIN" ] || die "--bin $BIN is not executable" 2
elif [ "$BUILD" -eq 0 ] && [ -e "$SCRIPT_DIR/bin/hades" ]; then
    [ -x "$SCRIPT_DIR/bin/hades" ] || die "the packaged bin/hades is not executable; the tarball is damaged" 2
    BIN="$SCRIPT_DIR/bin/hades"
elif [ -f "$SCRIPT_DIR/Cargo.toml" ] || [ -f "$SCRIPT_DIR/../Cargo.toml" ]; then
    SOURCE="$SCRIPT_DIR"
    [ -f "$SOURCE/Cargo.toml" ] || SOURCE="$SCRIPT_DIR/.."
    command -v cargo >/dev/null 2>&1 || die "no cargo on PATH, and no bin/hades beside the installer; pass --bin <file>"
    say "build: $SOURCE"
    ( cd "$SOURCE" && cargo build --release --locked ) || die "the release build failed"
    TARGET=$( cd "$SOURCE" && cargo metadata --format-version 1 --no-deps 2>/dev/null \
        | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p' | head -1 )
    [ -n "$TARGET" ] || TARGET="$SOURCE/target"
    BIN="$TARGET/release/hades"
    [ -x "$BIN" ] || die "the build finished but $BIN is not there"
else
    die "no binary to install: put bin/hades beside this script, pass --bin <file>, or run it from the rust/ checkout" 2
fi

# 2. Install the binary atomically: a half-copied executable is worse than the old one.
mkdir -p "$PREFIX/bin"
TEMP_BIN="$PREFIX/bin/.hades.new.$$"
trap 'rm -f "$TEMP_BIN"' EXIT INT TERM
cp "$BIN" "$TEMP_BIN"
chmod 755 "$TEMP_BIN"
mv -f "$TEMP_BIN" "$PREFIX/bin/hades"
trap - EXIT INT TERM
say "binary: $( "$PREFIX/bin/hades" --version )"
say "        $PREFIX/bin/hades"

# 3. Initialise once. An existing config is a home that is already somebody's.
if [ "$DO_INIT" -eq 0 ]; then
    say "home:   skipped (--no-init)"
elif [ -f "$HOME_DIR/config.json" ]; then
    # An upgrade still needs the procedures this version ships. Seeding never overwrites a file that is
    # already there, so a person's edits to a built-in survive the install.
    HADES_HOME="$HOME_DIR" "$PREFIX/bin/hades" skills seed >/dev/null 2>&1 || true
    say "home:   already initialised at $HOME_DIR, left alone"
else
    mkdir -p "$ROOT"
    if [ -n "$NAME" ]; then
        init_name=$NAME
    else
        init_name="Hades"
    fi
    init_home() {
        set -- init --non-interactive --root "$ROOT" --name "$init_name"
        if [ -n "$GOAL" ]; then set -- "$@" --goal "$GOAL"; fi
        if [ -n "$TOPICS" ]; then set -- "$@" --topics "$TOPICS"; fi
        HADES_HOME="$HOME_DIR" "$PREFIX/bin/hades" "$@"
    }
    init_home >/dev/null || die "hades init failed; the home is at $HOME_DIR"
    say "home:   $HOME_DIR"
fi

# 4. Units are files; starting them is a separate, optional step.
if [ "$DO_SERVICE" -eq 1 ]; then
    UNITS=$(HADES_HOME="$HOME_DIR" "$PREFIX/bin/hades" service install --prefix "$PREFIX" --home "$HOME_DIR") \
        || die "the systemd units could not be written"
    printf '%s\n' "$UNITS"
else
    say "units:  skipped (--no-service)"
fi

# 5. Start, when there is a manager to ask.
systemd_answers() {
    command -v systemctl >/dev/null 2>&1 && systemctl --user show-environment >/dev/null 2>&1
}

if [ "$DO_START" -eq 1 ] && [ "$DO_SERVICE" -eq 1 ]; then
    if systemd_answers; then
        if systemctl --user daemon-reload \
            && systemctl --user enable --now hades-daemon.service >/dev/null; then
            say "started: hades-daemon.service (hades status names it once it is up)"
        else
            # The binary and home are installed. A manager that answers but fails here is a supervisor
            # problem, not an install failure, and exit 1 would make a good install look broken.
            say "start:  enable failed; the binary and the home are installed"
            say "        systemctl --user daemon-reload && systemctl --user enable --now hades-daemon.service"
        fi
    else
        say "start:  skipped, systemd --user is not reachable"
        say "        systemctl --user daemon-reload && systemctl --user enable --now hades-daemon.service"
    fi
fi

if [ "$WITH_GATEWAY" -eq 1 ]; then
    systemd_answers || die "--with-gateway needs a reachable systemd user manager"
    systemctl --user daemon-reload
    systemctl --user enable --now hades-gateway.service >/dev/null
    say "started: hades-gateway.service (its first failure is in journalctl --user -u hades-gateway)"
fi

say ""
say "hades is installed."
say "  binary: $PREFIX/bin/hades"
say "  home:   $HOME_DIR"
say "  work:   $ROOT"
if [ "$DO_SERVICE" -eq 1 ]; then
    say "  units:  ${HOME%/}/.config/systemd/user/hades-daemon.service, hades-gateway.service"
fi
say ""
say "next:"
say "  hades status                       what the daemon knows"
say "  hades audit                        the vault and the record"
say "  \$EDITOR $HOME_DIR/.env             HADES_TELEGRAM_TOKEN and provider keys, never config.json"
