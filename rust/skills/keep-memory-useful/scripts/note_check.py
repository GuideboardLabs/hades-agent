#!/usr/bin/env python3
"""Check that a stored note is findable and carries its provenance.

The point is not to grade the prose. It is to catch the three failures that make a note useless later:
no summary to retrieve on, no source to trust, and a credential that should never have been written.
"""

from __future__ import annotations

import argparse
import json
import re
from pathlib import Path

SECRETS = [
    ("private key", re.compile(r"BEGIN [A-Z ]*PRIVATE KEY")),
    ("aws key", re.compile(r"\bAKIA[0-9A-Z]{16}\b")),
    ("github token", re.compile(r"\bgh[pousr]_[A-Za-z0-9]{20,}\b")),
    ("openai key", re.compile(r"\bsk-[A-Za-z0-9]{20,}\b")),
]
DATE = re.compile(r"^\d{4}-\d{2}-\d{2}$")


def frontmatter(text: str) -> tuple[dict[str, str] | None, str]:
    if not text.startswith("---\n"):
        return None, "the note must start with --- frontmatter"
    end = text.find("\n---", 4)
    if end < 0:
        return None, "the frontmatter is not closed"
    fields = {}
    for line in text[4:end].splitlines():
        if ":" in line and not line.lstrip().startswith("#"):
            key, value = line.split(":", 1)
            fields[key.strip()] = value.strip().strip('"').strip("'")
    return fields, text[end + 4 :]


def check(path: Path) -> dict:
    text = path.read_text(encoding="utf-8", errors="replace")
    fields, body = frontmatter(text)
    errors: list[str] = []
    warnings: list[str] = []
    if fields is None:
        return {"path": str(path), "ok": False, "errors": [body], "warnings": []}
    summary = fields.get("summary", "")
    if not summary:
        errors.append("summary is missing; retrieval has nothing to match")
    elif len(summary) < 20:
        errors.append("summary is shorter than 20 characters")
    elif len(summary) > 240:
        warnings.append("summary is longer than 240 characters")
    if not fields.get("title"):
        errors.append("title is missing")
    if not DATE.match(fields.get("date", "")):
        errors.append("date must be YYYY-MM-DD")
    if not any(fields.get(key) for key in ("source", "source_url", "provenance")):
        errors.append("no source, source_url or provenance field")
    if len(body.strip()) < 40:
        errors.append("body is too short to carry a claim")
    for label, pattern in SECRETS:
        if pattern.search(text):
            errors.append(f"possible {label} in the note")
    return {"path": str(path), "ok": not errors, "errors": errors, "warnings": warnings}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("path", type=Path)
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
