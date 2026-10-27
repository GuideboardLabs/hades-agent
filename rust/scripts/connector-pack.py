#!/usr/bin/env python3
"""Pack the Discord connector as an installable connector pack.

A connector pack is a tar.gz holding `connector.json`, a `bin/` tree and a `SHA256SUMS` that covers
every file except itself. This script builds the binary, writes the manifest, hashes the tree and
prints the archive path. It refuses to pack a binary it did not just build.

Usage: python3 rust/scripts/connector-pack.py [--out dist]
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
RUST = ROOT / "rust"
CRATE = RUST / "connectors" / "discord"
VERSION = "0.1.0"
ID = "discord"


def digest(path: Path) -> str:
    sha = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            sha.update(block)
    return sha.hexdigest()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", default="dist")
    parser.add_argument("--profile", default="release")
    args = parser.parse_args()

    target = "release" if args.profile == "release" else "debug"
    env = dict(os.environ)
    env.setdefault("CARGO_TARGET_DIR", str(RUST / "target"))
    build = subprocess.run(
        [
            env.get("CARGO", "cargo"),
            "build",
            "-p",
            "hades-connector-discord",
            *(["--release"] if target == "release" else []),
        ],
        cwd=RUST,
        env=env,
    )
    if build.returncode != 0:
        print("cargo build failed", file=sys.stderr)
        return 1

    binary = Path(env["CARGO_TARGET_DIR"]) / target / "hades-connector-discord"
    if not binary.is_file():
        print(f"no binary at {binary}", file=sys.stderr)
        return 1

    name = f"hades-connector-{ID}-{VERSION}"
    stage = Path(args.out) / name
    if stage.exists():
        shutil.rmtree(stage)
    (stage / "bin").mkdir(parents=True)
    shutil.copy2(binary, stage / "bin" / "hades-connector-discord")
    (stage / "bin" / "hades-connector-discord").chmod(0o755)
    # The pack's own words and its own animation live here, not in the harness: a connector that is
    # fully removable has to carry what it says.
    prompt = CRATE / "prompt.md"
    if prompt.is_file():
        shutil.copy2(prompt, stage / "prompt.md")
    for extra in ("LICENSE", "NOTICE"):
        source = ROOT / extra
        if source.is_file():
            shutil.copy2(source, stage / extra)

    manifest = {
        "format": 1,
        "kind": "connector",
        "id": ID,
        "version": VERSION,
        "title": "Discord",
        "summary": "Discord gateway connector for Hades: DMs and guild mentions, approvals as buttons.",
        "license": "MIT",
        "protocol": {"min": 1, "max": 1},
        "requires_hades": {"min": "0.1.22"},
        "platform": "linux-x86_64",
        "entrypoint": "bin/hades-connector-discord",
        "prompt": "prompt.md",
        "loading": {"glyph": "💀", "span": 5, "step_ms": 240},
        "config_schema": {
            "allowed_users": {"type": "array<string>", "required": True},
            "allowed_guilds": {"type": "array<string>", "default": []},
            "require_mention": {"type": "boolean", "default": True},
            "model": {
                "type": "string",
                "optional": True,
                "note": "defaults to the gateway's model; name one only to route this surface elsewhere",
            },
            "provider": {
                "type": "object",
                "optional": True,
                "note": "not needed to install: the gateway's provider is used unless this is set",
            },
            "prompt": {"type": "string", "note": "replaces the pack's prompt.md"},
            "loading": {"type": "object", "note": "glyph, span, step_ms"},
            "channels": {"type": "object", "note": "per-channel mode/policy/root/model"},
        },
        "signature": None,
    }
    (stage / "connector.json").write_text(json.dumps(manifest, indent=2) + "\n")
    (stage / "README.md").write_text(
        "# Hades Discord connector\n\n"
        "A surface pack for Hades. Install it with `hades pack add <this.tar.gz>` and run it with\n"
        "`hades connector run discord --config <config>`. The token is read from\n"
        "`HADES_CONNECTOR_DISCORD_TOKEN`. The surface answers on the gateway's model and provider,\n"
        "the same ones the rest of the install uses; `gateway.connectors.discord` only has to say\n"
        "who may talk to it and where its channels are.\n"
    )

    sums = []
    for path in sorted(stage.rglob("*")):
        if path.is_file() and path.name != "SHA256SUMS":
            sums.append(f"{digest(path)}  {path.relative_to(stage).as_posix()}")
    (stage / "SHA256SUMS").write_text("\n".join(sums) + "\n")

    archive = Path(args.out) / f"{name}-linux-x86_64.tar.gz"
    if archive.exists():
        archive.unlink()
    subprocess.run(
        ["tar", "-czf", str(archive), "-C", str(stage.parent), name],
        check=True,
    )
    print(archive)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
