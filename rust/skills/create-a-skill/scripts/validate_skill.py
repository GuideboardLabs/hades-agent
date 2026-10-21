#!/usr/bin/env python3
"""Validate one Hades skill before it is promoted.

The checks are the ones that make a skill loadable: a frontmatter block, a name that matches the folder,
a description that starts with "Use when", and a body that keeps the procedure short. Content judgement
stays with the reader; this only catches the mechanical mistakes.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

NAME = re.compile(r"^[a-z0-9]+(?:-[a-z0-9]+)*$")
SECRETS = [
    ("private key", re.compile(r"BEGIN [A-Z ]*PRIVATE KEY")),
    ("aws key", re.compile(r"\bAKIA[0-9A-Z]{16}\b")),
    ("github token", re.compile(r"\bgh[pousr]_[A-Za-z0-9]{20,}\b")),
    ("openai key", re.compile(r"\bsk-[A-Za-z0-9]{20,}\b")),
    ("slack token", re.compile(r"\bxox[baprs]-[A-Za-z0-9-]{10,}\b")),
]
RECOMMENDED = ("when to use", "steps", "check", "pitfalls")


def split_frontmatter(text: str) -> tuple[dict[str, str] | None, str]:
    if not text.startswith("---\n"):
        return None, "the file must start with --- on its own line"
    end = text.find("\n---", 4)
    if end < 0:
        return None, "the frontmatter block is not closed with ---"
    fields: dict[str, str] = {}
    for line in text[4:end].splitlines():
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        if ":" not in line:
            continue
        key, value = line.split(":", 1)
        fields[key.strip()] = value.strip().strip('"').strip("'")
    return fields, text[end + 4 :]


def check(path: Path) -> dict:
    problems: list[str] = []
    warnings: list[str] = []
    if path.is_dir():
        path = path / "SKILL.md"
    if not path.is_file():
        return {"path": str(path), "ok": False, "errors": ["there is no SKILL.md"], "warnings": []}
    text = path.read_text(encoding="utf-8", errors="replace")
    fields, body = split_frontmatter(text)
    if fields is None:
        return {"path": str(path), "ok": False, "errors": [body], "warnings": []}

    name = fields.get("name", "")
    description = fields.get("description", "")
    folder = path.parent.name
    if not name:
        problems.append("name is missing")
    elif not NAME.match(name):
        problems.append("name must be lowercase words separated by hyphens")
    elif name != folder:
        problems.append(f"name `{name}` does not match the folder `{folder}`")
    if not description:
        problems.append("description is missing")
    elif not description.lower().startswith("use when"):
        problems.append("description must start with `Use when`")
    elif len(description) > 400:
        problems.append("description is longer than 400 characters")
    if len(text) > 16_000:
        problems.append("SKILL.md is longer than 16,000 characters; move detail to references/")

    lowered = body.lower()
    if "## when to use" not in lowered:
        problems.append("body has no `## When to use` section")
    for section in RECOMMENDED[1:]:
        if f"## {section}" not in lowered:
            warnings.append(f"body has no `## {section}` section")
    if "todo" in lowered:
        warnings.append("body still contains TODO")
    for label, pattern in SECRETS:
        if pattern.search(text):
            problems.append(f"possible {label} in the file")

    return {
        "path": str(path),
        "ok": not problems,
        "errors": problems,
        "warnings": warnings,
        "name": name,
        "description_chars": len(description),
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("path", type=Path, help="a skill folder or its SKILL.md")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    result = check(args.path)
    if args.json:
        print(json.dumps(result, indent=2))
    else:
        print(f"{result['path']}: {'ok' if result['ok'] else 'not ok'}")
        for line in result["errors"]:
            print(f"error: {line}")
        for line in result["warnings"]:
            print(f"warning: {line}")
    return 0 if result["ok"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
