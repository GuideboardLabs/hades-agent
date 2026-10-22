#!/bin/sh
# The clean-home acceptance test: the packaged install path, in an empty home, with no model and no network.
#
# What this proves: the tarball's installer copies the binary, initialises a home, writes two units, and the
# daemon can be started, asked, and shut down by the binary that was just installed. What it does not prove:
# a real systemd transaction. `systemctl` is a recording shim here, and the units are checked by
# `systemd-analyze verify` when that tool exists. The real supervisor is one command away and is not run by a
# test that must not touch the machine it runs on.
#
# Scratch lives under rust/target/acceptance, never TMPDIR: in some homes TMPDIR points into
# Hermes' cache, and a test that writes there has crossed the boundary the product exists to hold.

set -eu

SCRIPT_DIR=$(CDPATH= cd "$(dirname "$0")" && pwd)
RUST=$(CDPATH= cd "$SCRIPT_DIR/.." && pwd)
BASE="$RUST/target/acceptance"
BIN=""
KEEP=0

while [ $# -gt 0 ]; do
    case "$1" in
        --bin) [ $# -ge 2 ] || { printf 'acceptance: --bin needs a file\n' >&2; exit 2; }; BIN=$2; shift 2 ;;
        --keep) KEEP=1; shift ;;
        --help|-h) printf 'usage: clean-home.sh [--bin FILE] [--keep]\n'; exit 0 ;;
        *) printf 'acceptance: unknown option %s\n' "$1" >&2; exit 2 ;;
    esac
done

if [ -z "$BIN" ]; then
    TARGET=$(sed -n 's/^target-dir *= *"\(.*\)"/\1/p' "$RUST/.cargo/config.toml" 2>/dev/null | head -1)
    [ -n "$TARGET" ] || TARGET="$RUST/target"
    BIN="$TARGET/release/hades"
fi
[ -x "$BIN" ] || {
    printf 'acceptance: no release binary at %s; build it or pass --bin <file>\n' "$BIN" >&2
    exit 2
}

rm -rf "$BASE"
mkdir -p "$BASE/home"
TEMP_HOME="$BASE/home"
unset HADES_HOME

# A systemctl that records and succeeds: the installer's supervisor branch, without the supervisor.
mkdir -p "$BASE/fakebin"
SYSTEMCTL_LOG="$BASE/systemctl.log"
: > "$SYSTEMCTL_LOG"
cat > "$BASE/fakebin/systemctl" <<'EOF'
#!/bin/sh
printf '%s\n' "$*" >> "$SYSTEMCTL_LOG"
if [ "${SYSTEMCTL_FAIL:-0}" = "1" ]; then
    case "$*" in
        *"enable --now"*) exit 1 ;;
    esac
fi
exit 0
EOF
chmod 755 "$BASE/fakebin/systemctl"
export SYSTEMCTL_LOG
PATH="$BASE/fakebin:$PATH"
export PATH

DAEMON_PID=""
cleanup() {
    status=$?
    if [ -n "$DAEMON_PID" ]; then
        kill "$DAEMON_PID" 2>/dev/null || true
    fi
    if [ "$KEEP" -eq 1 ]; then
        printf 'kept: %s\n' "$BASE"
    elif [ "$status" -eq 0 ]; then
        # A passing run leaves nothing behind; a failing one keeps its logs to be read.
        rm -rf "$BASE"
    fi
}
trap cleanup EXIT INT TERM

N=0
ok() {
    N=$((N + 1))
    printf 'ok %d: %s\n' "$N" "$1"
}
fail() {
    printf 'FAIL: %s\n' "$1" >&2
    exit 1
}
has() {
    case "$1" in
        *"$2"*) return 0 ;;
        *) return 1 ;;
    esac
}

# 1. The tarball, built from the binary under test.
"$RUST/package.sh" --out "$BASE/dist" --skip-build --bin "$BIN" > "$BASE/package.log" 2>&1 \
    || { cat "$BASE/package.log" >&2; fail "package.sh exited nonzero"; }
TARBALL=$(sed -n 's/^wrote:  //p' "$BASE/package.log")
[ -f "$TARBALL" ] || fail "package.sh named no tarball"
ok "package.sh writes $TARBALL"

mkdir -p "$BASE/pkg"
tar -xzf "$TARBALL" -C "$BASE/pkg"
PKG=$(find "$BASE/pkg" -mindepth 1 -maxdepth 1 -type d | head -1)
[ -n "$PKG" ] || fail "the tarball has no top-level directory"
( cd "$PKG" && sha256sum -c SHA256SUMS >/dev/null 2>&1 ) || fail "the tarball's SHA256SUMS does not match"
ok "the tarball matches its SHA256SUMS"

# The same inputs must produce the same artifact: a checksum that changes on every build is not a
# provenance claim, it is a timestamp.
"$RUST/package.sh" --out "$BASE/dist-again" --skip-build --bin "$BIN" > "$BASE/package-again.log" 2>&1 \
    || { cat "$BASE/package-again.log" >&2; fail "the second package run failed"; }
FIRST=$( sha256sum "$TARBALL" | cut -d' ' -f1 )
SECOND=$( sha256sum "$BASE/dist-again"/*.tar.gz | cut -d' ' -f1 )
[ "$FIRST" = "$SECOND" ] || fail "two package runs produced different tarballs"
ok "packaging is deterministic ($FIRST)"

# Refusals are part of the interface: a typo, a missing binary and a crossing must fail before writing.
set +e
OUT=$(HOME="$TEMP_HOME" "$PKG/install.sh" --bogus 2>&1); CODE=$?
set -e
[ "$CODE" -eq 2 ] || fail "an unknown flag exited $CODE, not 2"
has "$OUT" "unknown option" || fail "an unknown flag is not named: $OUT"
ok "an unknown flag is refused by name"

set +e
OUT=$(HOME="$TEMP_HOME" "$PKG/install.sh" --bin "$BASE/no-such-binary" 2>&1); CODE=$?
set -e
[ "$CODE" -eq 2 ] || fail "a missing --bin exited $CODE, not 2"
has "$OUT" "does not exist" || fail "a missing --bin is not named: $OUT"
ok "a missing --bin is refused by name"

set +e
OUT=$(HOME="$TEMP_HOME" "$PKG/install.sh" --home "$TEMP_HOME/.hermes" --prefix "$TEMP_HOME/never" 2>&1); CODE=$?
set -e
[ "$CODE" -eq 2 ] || fail "another agent's home exited $CODE, not 2"
has "$OUT" "another agent" || fail "the crossing is not named: $OUT"
[ ! -e "$TEMP_HOME/never" ] || fail "a refused install created its prefix"
ok "another agent's home is refused before anything is written"

# A vault is judged by its real path, and the marker is looked for at every parent. Both were misses in the
# binary's guard and in this script: a symlink carries no marker in its name, and a deep note has none within
# the first twelve parents.
VAULT="$TEMP_HOME/notes-vault"
mkdir -p "$VAULT/.obsidian" "$VAULT/Research/notes"
set +e
OUT=$(HOME="$TEMP_HOME" "$PKG/install.sh" --home "$VAULT" --prefix "$TEMP_HOME/never4" 2>&1); CODE=$?
set -e
[ "$CODE" -eq 2 ] || fail "a marked vault exited $CODE, not 2"
has "$OUT" "notes vault" || fail "a marked vault is not named: $OUT"
[ ! -e "$TEMP_HOME/never4" ] || fail "a refused vault created its prefix"
ok "a marked vault is refused before anything is written"

ln -s "$VAULT/Research/notes" "$TEMP_HOME/vault-alias"
set +e
OUT=$(HOME="$TEMP_HOME" "$PKG/install.sh" --home "$TEMP_HOME/vault-alias" --prefix "$TEMP_HOME/never5" 2>&1); CODE=$?
set -e
[ "$CODE" -eq 2 ] || fail "a symlink into a vault exited $CODE, not 2"
has "$OUT" "notes vault" || fail "a symlink into a vault is not named: $OUT"
[ ! -e "$TEMP_HOME/never5" ] || fail "a refused symlink created its prefix"
ok "a symlink into a vault is refused before anything is written"

DEEP="$VAULT"
for level in 1 2 3 4 5 6 7 8 9 10 11 12 13 14; do DEEP="$DEEP/d$level"; done
mkdir -p "$DEEP"
set +e
OUT=$(HOME="$TEMP_HOME" "$PKG/install.sh" --home "$DEEP" --prefix "$TEMP_HOME/never6" 2>&1); CODE=$?
set -e
[ "$CODE" -eq 2 ] || fail "a deep path inside a vault exited $CODE, not 2"
has "$OUT" "notes vault" || fail "a deep path inside a vault is not named: $OUT"
[ ! -e "$TEMP_HOME/never6" ] || fail "a refused deep path created its prefix"
ok "a path fourteen levels into a vault is refused before anything is written"

# The work root and the prefix are written too. Both are checked before the binary is copied or `mkdir` runs.
set +e
OUT=$(HOME="$TEMP_HOME" "$PKG/install.sh" --home "$TEMP_HOME/clean-home" \
  --root "$VAULT/Research/notes" --prefix "$TEMP_HOME/never7" 2>&1); CODE=$?
set -e
[ "$CODE" -eq 2 ] || fail "a work root inside a vault exited $CODE, not 2"
has "$OUT" "notes vault" || fail "a work root inside a vault is not named: $OUT"
[ ! -e "$TEMP_HOME/never7" ] || fail "a refused work root created its prefix"
[ ! -e "$VAULT/Research/notes/hades-work" ] || fail "a refused work root was created"
ok "a work root inside a vault is refused before the binary is copied"

set +e
OUT=$(HOME="$TEMP_HOME" "$PKG/install.sh" --home "$TEMP_HOME/clean-home2" \
  --root "$TEMP_HOME/work-clean" --prefix "$VAULT/prefix" 2>&1); CODE=$?
set -e
[ "$CODE" -eq 2 ] || fail "a prefix inside a vault exited $CODE, not 2"
has "$OUT" "notes vault" || fail "a prefix inside a vault is not named: $OUT"
[ ! -e "$VAULT/prefix" ] || fail "a refused prefix was created"
ok "a prefix inside a vault is refused before anything is written"

set +e
OUT=$(HOME="$TEMP_HOME" "$PKG/install.sh" --with-gateway --prefix "$TEMP_HOME/never2" 2>&1); CODE=$?
set -e
[ "$CODE" -eq 2 ] || fail "--with-gateway without a token exited $CODE, not 2"
has "$OUT" "HADES_TELEGRAM_TOKEN" || fail "the missing token is not named: $OUT"
[ ! -e "$TEMP_HOME/never2" ] || fail "a refused --with-gateway created its prefix"
ok "--with-gateway without a token is refused before anything is written"

# A half-finished init is a state the binary refuses; the installer must name it before copying a binary.
mkdir -p "$TEMP_HOME/partial/.hades/mneme/00 Inbox"
set +e
OUT=$(HOME="$TEMP_HOME" "$PKG/install.sh" --home "$TEMP_HOME/partial/.hades" --prefix "$TEMP_HOME/never3" 2>&1); CODE=$?
set -e
[ "$CODE" -eq 2 ] || fail "a half-finished home exited $CODE, not 2"
has "$OUT" "half-finished init" || fail "a half-finished home is not named: $OUT"
[ ! -e "$TEMP_HOME/never3" ] || fail "a refused half-finished home created its prefix"
ok "a half-finished home is refused with the remediation named"

BEFORE=$( cd "$PKG" && find . -type f -exec sha256sum {} + | sort )

# 2. The install, in an empty home.
INSTALL_LOG="$BASE/install.log"
HOME="$TEMP_HOME" "$PKG/install.sh" \
    --prefix "$TEMP_HOME/.local" \
    --home "$TEMP_HOME/.hades" \
    --root "$TEMP_HOME/work" > "$INSTALL_LOG" 2>&1 \
    || { cat "$INSTALL_LOG" >&2; fail "install.sh exited nonzero"; }
ok "the packaged installer exits 0 in an empty home"

B="$TEMP_HOME/.local/bin/hades"
[ -x "$B" ] || fail "the binary is not executable at $B"
VERSION_TEXT=$(HOME="$TEMP_HOME" "$B" --version)
case "$VERSION_TEXT" in
    "hades "[0-9]*.[0-9]*.[0-9]*) ;;
    *) fail "--version printed $VERSION_TEXT" ;;
esac
ok "the installed binary answers $VERSION_TEXT"

[ -f "$TEMP_HOME/.hades/config.json" ] || fail "no config.json"
for area in "00 Inbox" "10 Outbox" "20 Research" "30 Projects" "40 Products" "50 Topics"; do
    [ -d "$TEMP_HOME/.hades/mneme/$area" ] || fail "the vault is missing $area"
done
[ -f "$TEMP_HOME/.hades/mneme/charter.md" ] || fail "no charter"
[ -f "$TEMP_HOME/.hades/mneme/INDEX.md" ] || fail "no index"
[ -d "$TEMP_HOME/work" ] || fail "the work root was not made"
ok "init writes the config, six areas, the charter and the index"

AUDIT=$(HOME="$TEMP_HOME" HADES_HOME="$TEMP_HOME/.hades" "$B" audit)
has "$AUDIT" "inbox: 0" || fail "the clean audit is not clean: $AUDIT"
ok "a clean home audits clean"

# 3. A pack exported from this home. The writer is deterministic and the reader is the same build that
#    wrote it; a §2-conformant reader must accept the writer's own archive.
mkdir -p "$TEMP_HOME/.hades/skills/pack-skill" "$TEMP_HOME/.hades/harness/research"
printf '%s\n' '---' 'name: pack-skill' 'contract: acceptance' '---' '' 'Always record the command and its exit code.' \
    > "$TEMP_HOME/.hades/skills/pack-skill/SKILL.md"
printf '%s\n' 'You are a research harness.' > "$TEMP_HOME/.hades/harness/research/system.md"
PACK_A="$BASE/pack-a.hadepack"
PACK_B="$BASE/pack-b.hadepack"
OUT=$(HOME="$TEMP_HOME" "$B" pack export guideboard/acceptance --from "$TEMP_HOME/.hades" \
    --skill pack-skill --profile research --title "Acceptance pack" \
    --out "$PACK_A" 2>&1) || fail "pack export failed: $OUT"
[ -f "$PACK_A" ] || fail "pack export named no file: $OUT"
OUT=$(HOME="$TEMP_HOME" "$B" pack export guideboard/acceptance --from "$TEMP_HOME/.hades" \
    --skill pack-skill --profile research --title "Acceptance pack" \
    --out "$PACK_B" 2>&1) || fail "the second pack export failed: $OUT"
cmp -s "$PACK_A" "$PACK_B" || fail "two pack exports differ"
ok "pack export is deterministic"

INSPECT=$(HOME="$TEMP_HOME" "$B" pack inspect "$PACK_A")
has "$INSPECT" "items: 1 skill, 1 harness, 0 case" || fail "inspect lost an item: $INSPECT"
has "$INSPECT" "signature: unsigned" || fail "inspect is not honest about the signature: $INSPECT"
ok "pack inspect reads the writer's archive"

PLAN=$(HOME="$TEMP_HOME" "$B" pack plan "$PACK_A" --state "$TEMP_HOME/.hades")
has "$PLAN" "duplicate" || fail "plan did not call the re-export a duplicate: $PLAN"
ok "pack plan classifies a re-export as duplicate"

# A pack exported from a home it is then installed into is a duplicate by construction. The fit check
# therefore builds a second source home and applies that pack to the installed home.
mkdir -p "$TEMP_HOME/.hades-source/skills/fit-skill" "$TEMP_HOME/.hades-source/harness/fit"
printf '%s\n' '---' 'name: fit-skill' 'contract: acceptance' '---' '' 'Use the fit source.' \
    > "$TEMP_HOME/.hades-source/skills/fit-skill/SKILL.md"
printf '%s\n' 'You are a fit harness.' > "$TEMP_HOME/.hades-source/harness/fit/system.md"
PACK_FIT="$BASE/fit.hadepack"
OUT=$(HOME="$TEMP_HOME" "$B" pack export guideboard/fit --from "$TEMP_HOME/.hades-source" \
    --skill fit-skill --profile fit --title "Fit pack" --out "$PACK_FIT" 2>&1) \
    || fail "the fit pack export failed: $OUT"
printf '%s\n' '{"items":[{"kind":"skill","identity":"fit-skill","action":"install"},{"kind":"harness","identity":"fit","action":"install"}]}' \
    > "$BASE/decisions.json"
OUT=$(HOME="$TEMP_HOME" "$B" pack apply "$PACK_FIT" --plan "$BASE/decisions.json" \
    --state "$TEMP_HOME/.hades" 2>&1) || fail "pack apply failed: $OUT"
has "$OUT" "applied: 2 item(s)" || fail "apply did not report two items: $OUT"
[ -f "$TEMP_HOME/.hades/skills/proposed/fit-skill/SKILL.md" ] || fail "apply did not install the skill as a proposal"
[ -f "$TEMP_HOME/.hades/harness/fit/system.md" ] || fail "apply did not install the harness"
ok "pack apply installs accepted rows and leaves the skill proposed"

LIST=$(HOME="$TEMP_HOME" "$B" pack list --state "$TEMP_HOME/.hades")
has "$LIST" "fit-skill" || fail "pack list lost the installed skill: $LIST"
has "$LIST" "harness/fit" || fail "pack list lost the installed harness: $LIST"
ok "pack list reads the catalog"

OUT=$(HOME="$TEMP_HOME" "$B" pack uninstall guideboard/fit --state "$TEMP_HOME/.hades" 2>&1) \
    || fail "pack uninstall failed: $OUT"
has "$OUT" "removed: 2" || fail "uninstall did not report two removals: $OUT"
[ ! -e "$TEMP_HOME/.hades/skills/proposed/fit-skill" ] || fail "uninstall left the skill"
[ ! -e "$TEMP_HOME/.hades/harness/fit" ] || fail "uninstall left the harness"
ok "pack uninstall removes the live copies"

set +e
OUT=$(HOME="$TEMP_HOME" "$B" pack export guideboard/empty --from "$TEMP_HOME/.hades" --out "$BASE/empty.hadepack" 2>&1); CODE=$?
set -e
[ "$CODE" -eq 2 ] || fail "a selector-less export exited $CODE, not 2"
has "$OUT" "at least one" || fail "a selector-less export is not refused by name: $OUT"
ok "pack export refuses a pack with no selector"

set +e
TOPIC=$(HOME="$TEMP_HOME" HADES_HOME="$TEMP_HOME/.hades" "$B" topic technical "Acceptance Probe" --summary "The clean-home probe can build a topic.")
CODE=$?
set -e
[ "$CODE" -eq 0 ] || fail "the topic verb exited $CODE: $TOPIC"
has "$TOPIC" "50 Topics/technical/acceptance-probe/index.md" || fail "the topic verb did not write the tree: $TOPIC"
ok "the topic verb writes a topic through the store"

STATUS=$(HOME="$TEMP_HOME" "$B" status --state "$TEMP_HOME/.hades")
has "$STATUS" "daemon: none" || fail "the first status is not honest: $STATUS"
ok "status with no daemon says none"

# 3. The units.
UNIT_DIR="$TEMP_HOME/.config/systemd/user"
DAEMON_UNIT="$UNIT_DIR/hades-daemon.service"
GATEWAY_UNIT="$UNIT_DIR/hades-gateway.service"
[ -f "$DAEMON_UNIT" ] || fail "no daemon unit"
[ -f "$GATEWAY_UNIT" ] || fail "no gateway unit"
grep -q "^ExecStart=$TEMP_HOME/.local/bin/hades daemon --state $TEMP_HOME/.hades$" "$DAEMON_UNIT" \
    || fail "the daemon unit does not name the installed binary and home"
grep -q "^ExecStop=-$TEMP_HOME/.local/bin/hades shutdown --state $TEMP_HOME/.hades$" "$DAEMON_UNIT" \
    || fail "the daemon unit has no graceful stop"
grep -q "^Requires=hades-daemon.service$" "$GATEWAY_UNIT" \
    || fail "the gateway does not require the daemon"
grep -q "^Environment=HADES_HOME=$TEMP_HOME/.hades$" "$DAEMON_UNIT" \
    || fail "the unit does not carry the home"
ok "both units name the install and the gateway requires the daemon"

if command -v systemd-analyze >/dev/null 2>&1; then
    # Hermetic on purpose: with the ambient HOME, `--user` also parses the host's own units and prints their
    # warnings. Pointed at the temp home it reads only what this install wrote.
    HOME="$TEMP_HOME" XDG_CONFIG_HOME="$TEMP_HOME/.config" \
        systemd-analyze --user verify "$DAEMON_UNIT" "$GATEWAY_UNIT" > "$BASE/verify.log" 2>&1 \
        || { cat "$BASE/verify.log" >&2; fail "systemd-analyze verify rejected a unit"; }
    ok "systemd-analyze --user verify accepts both units"
else
    ok "systemd-analyze is absent; the units are checked as text only"
fi

grep -q -- "--user daemon-reload" "$SYSTEMCTL_LOG" || fail "the installer never reloaded the manager"
grep -q -- "--user enable --now hades-daemon.service" "$SYSTEMCTL_LOG" \
    || fail "the installer never enabled the daemon"
ok "the installer asks systemd to reload and enable the daemon"

# 4. The daemon lifecycle, from the installed binary.
HOME="$TEMP_HOME" "$B" daemon --state "$TEMP_HOME/.hades" > "$BASE/daemon.log" 2>&1 &
DAEMON_PID=$!
i=0
while [ "$i" -lt 200 ]; do
    [ -S "$TEMP_HOME/.hades/daemon.sock" ] && break
    i=$((i + 1))
    sleep 0.05
done
[ -S "$TEMP_HOME/.hades/daemon.sock" ] || { cat "$BASE/daemon.log" >&2; fail "the daemon never bound its socket"; }

STATUS=$(HOME="$TEMP_HOME" "$B" status --state "$TEMP_HOME/.hades")
has "$STATUS" "daemon: pid $DAEMON_PID" || fail "status does not name the daemon pid: $STATUS"
ok "status names the running daemon (pid $DAEMON_PID)"

HOME="$TEMP_HOME" "$B" shutdown --state "$TEMP_HOME/.hades" > "$BASE/shutdown.log" 2>&1 \
    || fail "shutdown exited nonzero"
wait "$DAEMON_PID" 2>/dev/null || true
DAEMON_PID=""
[ ! -e "$TEMP_HOME/.hades/daemon.sock" ] || fail "shutdown left the socket behind"
[ ! -e "$TEMP_HOME/.hades/daemon.json" ] || fail "shutdown left the pid file behind"
ok "shutdown ends the daemon and leaves nothing a client would mistake for it"

# 5. The first refusal a new user meets, and it is named rather than generic.
set +e
GATEWAY=$(HOME="$TEMP_HOME" "$B" gateway --state "$TEMP_HOME/.hades" 2>&1)
GATEWAY_CODE=$?
set -e
[ "$GATEWAY_CODE" -eq 2 ] || fail "gateway exit was $GATEWAY_CODE, not 2"
has "$GATEWAY" "there is no \`gateway.telegram\` section" \
    || fail "the gateway refusal does not name what is missing: $GATEWAY"
ok "the gateway refuses a home with no gateway.telegram section, by name"

# 6. A second install is a no-op for the home and the units.
CONFIG_BEFORE=$(sha256sum "$TEMP_HOME/.hades/config.json" "$TEMP_HOME/.hades/mneme/charter.md")
UNITS_BEFORE=$(sha256sum "$DAEMON_UNIT" "$GATEWAY_UNIT")
HOME="$TEMP_HOME" "$PKG/install.sh" \
    --prefix "$TEMP_HOME/.local" \
    --home "$TEMP_HOME/.hades" \
    --root "$TEMP_HOME/work" > "$BASE/install-second.log" 2>&1 \
    || { cat "$BASE/install-second.log" >&2; fail "the second install exited nonzero"; }
CONFIG_AFTER=$(sha256sum "$TEMP_HOME/.hades/config.json" "$TEMP_HOME/.hades/mneme/charter.md")
UNITS_AFTER=$(sha256sum "$DAEMON_UNIT" "$GATEWAY_UNIT")
[ "$CONFIG_BEFORE" = "$CONFIG_AFTER" ] || fail "the second install changed the home"
[ "$UNITS_BEFORE" = "$UNITS_AFTER" ] || fail "the second install changed the units"
ok "a second install leaves the home and the units byte-identical"

AFTER=$( cd "$PKG" && find . -type f -exec sha256sum {} + | sort )
[ "$BEFORE" = "$AFTER" ] || fail "the install wrote into the package tree"
ok "the install writes nothing into the package tree"

# 7. The guard still holds in a home that has never held another agent's install.
set +e
mkdir -p "$TEMP_HOME/notes/.obsidian"
GUARD=$(HOME="$TEMP_HOME" "$B" audit --vault "$TEMP_HOME/notes" 2>&1)
GUARD_CODE=$?
set -e
[ "$GUARD_CODE" -eq 2 ] || fail "the guard exit was $GUARD_CODE, not 2"
has "$GUARD" "a notes vault" || fail "the guard did not name the vault: $GUARD"
ok "a notes vault is refused in a clean home"

# 8. The supervisor is the optional half: a manager that answers but refuses to enable must not turn a
# complete install into a failed one.
set +e
OUT=$(SYSTEMCTL_FAIL=1 HOME="$TEMP_HOME" "$PKG/install.sh" \
    --prefix "$TEMP_HOME/.local" --home "$TEMP_HOME/.hades" --root "$TEMP_HOME/work" 2>&1); CODE=$?
set -e
[ "$CODE" -eq 0 ] || fail "a failing enable made the installer exit $CODE"
has "$OUT" "enable failed; the binary and the home are installed" \
    || fail "a failing enable is not reported honestly: $OUT"
ok "a failing systemd enable leaves a complete install and a named fallback"

printf '\nPASS: clean-home acceptance (%d checks)\n' "$N"
