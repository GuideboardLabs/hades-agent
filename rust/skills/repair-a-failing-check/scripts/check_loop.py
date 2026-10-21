#!/usr/bin/env python3
"""Run one check up to N times and report what changed between attempts.

The loop does not edit anything and does not decide that a failure is flaky. It records the exit status,
the duration and a short signature for each attempt so a person can see whether the failure moved.
"""

from __future__ import annotations

import argparse
import json
import re
import shlex
import subprocess
import time
from pathlib import Path

SIGNAL = re.compile(r"\b(error|failed|failure|panic|assert|exception|traceback)\b", re.I)


def signature(text: str) -> str:
    lines = [line.strip() for line in text.splitlines() if line.strip()]
    for line in lines:
        if SIGNAL.search(line):
            return line[:240]
    return lines[-1][:240] if lines else ""


def run(command: list[str], cwd: Path | None, timeout: float) -> dict:
    start = time.monotonic()
    try:
        result = subprocess.run(
            command,
            cwd=str(cwd) if cwd else None,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
        output = (result.stdout or "") + (result.stderr or "")
        return {
            "code": result.returncode,
            "seconds": round(time.monotonic() - start, 3),
            "signature": signature(output),
            "stdout": result.stdout[-4000:],
            "stderr": result.stderr[-4000:],
        }
    except subprocess.TimeoutExpired as error:
        def as_text(value: object) -> str:
            if isinstance(value, bytes):
                return value.decode("utf-8", "replace")
            return value if isinstance(value, str) else ""

        output = as_text(error.stdout) + as_text(error.stderr)
        return {
            "code": 124,
            "seconds": round(time.monotonic() - start, 3),
            "signature": f"timeout after {timeout:g}s",
            "stdout": "",
            "stderr": output[-4000:],
        }
    except FileNotFoundError:
        return {
            "code": 127,
            "seconds": round(time.monotonic() - start, 3),
            "signature": f"command not found: {command[0]}",
            "stdout": "",
            "stderr": "",
        }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--attempts", type=int, default=3)
    parser.add_argument("--timeout", type=float, default=600.0)
    parser.add_argument("--cwd", type=Path)
    parser.add_argument("--json", action="store_true")
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command:
        parser.error("give the check command after --")
    if args.attempts < 1:
        parser.error("--attempts must be at least 1")

    attempts = []
    for number in range(1, args.attempts + 1):
        attempt = run(command, args.cwd, args.timeout)
        attempt["attempt"] = number
        attempts.append(attempt)
        if attempt["code"] == 0:
            break

    passed = attempts[-1]["code"] == 0
    result = {
        "command": command,
        "passed": passed,
        "attempts": attempts,
        "moved": len({item["signature"] for item in attempts}) > 1,
    }
    if args.json:
        print(json.dumps(result, indent=2))
    else:
        for item in attempts:
            print(f"attempt {item['attempt']}: exit {item['code']} in {item['seconds']:g}s: {item['signature']}")
        print("PASS" if passed else "FAIL")
    return 0 if passed else 1


if __name__ == "__main__":
    raise SystemExit(main())
