#!/usr/bin/env python3
"""Lint a change plan for the parts that make it checkable.

A plan that names no command, no file and no risk is a wish. The lint does not judge whether the plan is
good; it refuses to let those three blanks pass as if they were filled.
"""

from __future__ import annotations

import argparse
import json
import re
from pathlib import Path

RUNNER = re.compile(
    r"`(?:cargo|pytest|python3?|npm|pnpm|yarn|go\s+test|make|just|ctest|bash|sh|node|deno|bun)\b[^`]*`",
    re.I,
)
PATH = re.compile(r"(?<![\w/])(?:[\w.-]+/)+[\w.-]+|\b[\w.-]+\.(?:rs|py|ts|tsx|js|go|toml|yaml|yml|json|md)\b")


def lint(text: str) -> dict:
    errors: list[str] = []
    warnings: list[str] = []
    if not RUNNER.search(text):
        errors.append("no exact check command in backticks (cargo test, pytest, npm test, ...)")
    if not PATH.search(text):
        errors.append("no file path named")
    if not re.search(r"\brisk", text, re.I):
        warnings.append("no risk section or risk sentence")
    if not re.search(r"out of scope|will not|not do", text, re.I):
        warnings.append("no explicit exclusion")
    if not re.search(r"\bgoal\b", text[:800], re.I):
        warnings.append("goal is not stated near the top")
    if len(text.splitlines()) > 120:
        warnings.append("plan is longer than 120 lines; it may be a survey")
    return {"ok": not errors, "errors": errors, "warnings": warnings}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("path", type=Path)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    result = lint(args.path.read_text(encoding="utf-8", errors="replace"))
    if args.json:
        print(json.dumps(result, indent=2))
    else:
        print("ok" if result["ok"] else "not ok")
        for line in result["errors"]:
            print(f"error: {line}")
        for line in result["warnings"]:
            print(f"warning: {line}")
    return 0 if result["ok"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
