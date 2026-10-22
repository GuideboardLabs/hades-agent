#!/bin/sh
# The claims that a Cargo-shaped repository cannot test, each on a real tree.
#
# The shipped profiles assume Cargo and one browser location. A `.hades-project.json` sets the check for a
# repository that is not Rust, `hades doctor` has to name the browser it would launch, and a vision endpoint
# has to be configurable without the local GGUF files. This script builds a small project of each kind in a
# temporary directory and checks the answer, so a regression in any of the three fails here rather than on
# someone else's machine.
#
# Usage: scripts/cross-env.sh
#        HADES_BIN=/path/to/hades scripts/cross-env.sh
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${HADES_BIN:-$ROOT/target/release/hades}
if [ ! -x "$BIN" ]; then
    echo "no binary at $BIN; run: cargo build --release" >&2
    exit 1
fi

# Under /tmp, not the caller's TMPDIR: the scratch directory this agent uses is inside the live Hermes
# home, which Hades refuses as a root on purpose.
WORK=$(mktemp -d /tmp/hades-cross-env.XXXXXX)
trap 'rm -rf "$WORK"' EXIT
export HADES_HOME="$WORK/home"
mkdir -p "$HADES_HOME"

passed=0
failed=0
pass() {
    passed=$((passed + 1))
    printf 'PASS: %s\n' "$1"
}
fail() {
    failed=$((failed + 1))
    printf 'FAIL: %s\n' "$1"
}
skip() {
    printf 'SKIP: %s\n' "$1"
}

# 1. A Python tree: the recipe's check, not Cargo's.
python_tree="$WORK/python"
mkdir -p "$python_tree"
cat > "$python_tree/test_app.py" <<'PY'
import unittest


class TestApp(unittest.TestCase):
    def test_it_adds(self):
        self.assertEqual(1 + 1, 2)
PY
printf '{"version":1,"verify":"python3 -m unittest -q","protect":["test_app.py"]}\n' \
    > "$python_tree/.hades-project.json"
said=$(cd "$python_tree" && "$BIN" context "make the test pass" --root "$python_tree" 2>&1) || true
if printf '%s' "$said" | grep -q "python3 -m unittest -q"; then
    pass "a Python recipe reaches the context"
else
    fail "the Python recipe did not reach the context: $said"
fi
if (cd "$python_tree" && python3 -m unittest -q >/dev/null 2>&1); then
    pass "the Python check the recipe names runs green"
else
    fail "the Python check the recipe names does not run"
fi

# 2. A Node tree, when Node is present.
node_tree="$WORK/node"
mkdir -p "$node_tree"
cat > "$node_tree/app.test.js" <<'JS'
const test = require('node:test');
const assert = require('node:assert');
test('adds', () => {
    assert.strictEqual(1 + 1, 2);
});
JS
printf '{"name":"fixture","version":"1.0.0","scripts":{"test":"node --test"}}\n' \
    > "$node_tree/package.json"
printf '{"version":1,"verify":"node --test","protect":["app.test.js","package.json"]}\n' \
    > "$node_tree/.hades-project.json"
if command -v node >/dev/null 2>&1; then
    said=$(cd "$node_tree" && "$BIN" context "make the test pass" --root "$node_tree" 2>&1) || true
    if printf '%s' "$said" | grep -q "node --test"; then
        pass "a Node recipe reaches the context"
    else
        fail "the Node recipe did not reach the context: $said"
    fi
    if (cd "$node_tree" && node --test >/dev/null 2>&1); then
        pass "the Node check the recipe names runs green"
    else
        fail "the Node check the recipe names does not run"
    fi
else
    skip "node is not installed, so the Node tree is not exercised"
fi

# 3. A browser that is not in a directory this build knows by name.
browser="$WORK/bin/chromium"
mkdir -p "$WORK/bin"
printf '#!/bin/sh\nexit 0\n' > "$browser"
chmod +x "$browser"
said=$(HADES_CHROME="$browser" "$BIN" doctor 2>&1) || true
if printf '%s' "$said" | grep -q "browser.*$browser"; then
    pass "doctor names the browser it would launch"
else
    fail "doctor did not name the browser: $(printf '%s' "$said" | grep browser || true)"
fi

# 4. A vision endpoint that is not this machine's model files.
cat > "$HADES_HOME/config.json" <<JSON
{"version":1,"provider":"ollama-cloud","model":"deepseek-v4.1-flash","root":"$WORK","vault":"$WORK/mneme",
 "vision":{"url":"http://127.0.0.1:9/v1/chat/completions","model":"vl-8b","key_env":"VISION_KEY"}}
JSON
said=$("$BIN" config 2>&1) || true
if printf '%s' "$said" | grep -q "vision: vl-8b"; then
    pass "a vision endpoint is read from the config"
else
    fail "the vision endpoint was not read: $said"
fi
said=$("$BIN" doctor 2>&1) || true
if printf '%s' "$said" | grep -q "0 failure"; then
    pass "doctor accepts a config with a remote vision endpoint"
else
    fail "doctor reports a failure with a remote vision endpoint: $said"
fi

printf '\n%d passed, %d failed\n' "$passed" "$failed"
[ "$failed" -eq 0 ]
