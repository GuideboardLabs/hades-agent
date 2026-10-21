#!/usr/bin/env python3
"""Read the tree before a commit: status, diff, staged secrets and the check result.

This command never stages, commits or pushes. It exists so the decision to land a change is made from the
actual state, not from the last line of a test run.
"""

from __future__ import annotations

import argparse
import json
import re
import shlex
import subprocess
from pathlib import Path

SECRETS = [
    ("private key", re.compile(r"BEGIN [A-Z ]*PRIVATE KEY")),
    ("aws key", re.compile(r"\bAKIA[0-9A-Z]{16}\b")),
    ("github token", re.compile(r"\bgh[pousr]_[A-Za-z0-9]{20,}\b")),
    ("openai key", re.compile(r"\bsk-[A-Za-z0-9]{20,}\b")),
    ("slack token", re.compile(r"\bxox[baprs]-[A-Za-z0-9-]{10,}\b")),
    ("assigned secret", re.compile(r"(?i)\b(password|secret|token|api[_-]?key)\s*[:=]\s*['\"][^'\"]{8,}['\"]")),
]


def git(*args: str) -> str:
    result = subprocess.run(["git", *args], capture_output=True, text=True)
    return result.stdout if result.returncode == 0 else ""


def run_check(command: str, timeout: float) -> dict:
    try:
        result = subprocess.run(
            shlex.split(command), capture_output=True, text=True, timeout=timeout
        )
        return {
            "command": command,
            "code": result.returncode,
            "output": (result.stdout + result.stderr)[-4000:],
        }
    except FileNotFoundError:
        return {"command": command, "code": 127, "output": "command not found"}
    except subprocess.TimeoutExpired:
        return {"command": command, "code": 124, "output": f"timeout after {timeout:g}s"}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="append", default=[], help="a command that must pass; repeatable")
    parser.add_argument("--timeout", type=float, default=600.0)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()

    status = [line for line in git("status", "--porcelain=v1").splitlines() if line]
    staged = git("diff", "--cached", "--unified=0")
    secrets = []
    for line in staged.splitlines():
        if not line.startswith("+") or line.startswith("+++"):
            continue
        for label, pattern in SECRETS:
            if pattern.search(line):
                secrets.append({"kind": label, "line": line[1:].strip()[:240]})
    checks = [run_check(command, args.timeout) for command in args.check]
    failed = [check for check in checks if check["code"] != 0]
    result = {
        "status": status,
        "staged_stat": git("diff", "--cached", "--stat").strip(),
        "unstaged_stat": git("diff", "--stat").strip(),
        "secrets": secrets,
        "checks": checks,
        "ok": not secrets and not failed,
    }
    if args.json:
        print(json.dumps(result, indent=2))
    else:
        print("staged:" if result["staged_stat"] else "staged: nothing")
        if result["staged_stat"]:
            print(result["staged_stat"])
        if status:
            print("working tree:")
            for line in status:
                print(f"  {line}")
        for secret in secrets:
            print(f"SECRET {secret['kind']}: {secret['line']}")
        for check in checks:
            print(f"check exit {check['code']}: {check['command']}")
        print("OK" if result["ok"] else "NOT OK")
    return 0 if result["ok"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
