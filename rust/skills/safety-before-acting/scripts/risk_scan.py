#!/usr/bin/env python3
"""Classify a command or message before it runs.

The scan is deliberately conservative: it names what pattern it saw and what could be lost. A hit is a
reason to ask a person, not proof the command is wrong.
"""

from __future__ import annotations

import argparse
import json
import re

RULES = [
    ("critical", "delete a filesystem root", re.compile(r"\brm\s+-[a-z]*r[a-z]*f[a-z]*\s+(/|~|\$HOME|\*)")),
    ("critical", "format a device", re.compile(r"\bmkfs(?:\.[a-z0-9]+)?\b", re.I)),
    ("critical", "write raw bytes to a device", re.compile(r"\bdd\b[^\n]*\bof=/dev/", re.I)),
    ("critical", "force-push a branch", re.compile(r"\bgit\s+push\b[^\n]*(--force|-f)\b", re.I)),
    ("critical", "open permissions on a root", re.compile(r"\bchmod\s+-R\s+777\s+/")),
    ("critical", "pipe a remote script into a shell", re.compile(r"\b(curl|wget)\b[^\n]*\|\s*(ba|z)?sh\b", re.I)),
    ("critical", "drop a database or table", re.compile(r"\bDROP\s+(DATABASE|TABLE)\b", re.I)),
    ("critical", "fork bomb", re.compile(r":\(\)\s*\{.*\};\s*:")),
    ("high", "recursive delete", re.compile(r"\brm\s+-[a-z]*r", re.I)),
    ("high", "stop or restart the machine", re.compile(r"\b(shutdown|reboot|poweroff|halt)\b", re.I)),
    ("high", "kill without a graceful stop", re.compile(r"\bkill\s+-9\b")),
    ("high", "recursive ownership change", re.compile(r"\bchown\s+-R\b")),
    ("high", "run as root", re.compile(r"\bsudo\b")),
    ("high", "stop or mask a service", re.compile(r"\bsystemctl\s+(stop|disable|mask)\b", re.I)),
    ("high", "prune docker state", re.compile(r"\bdocker\s+system\s+prune\b", re.I)),
    ("high", "discard local commits", re.compile(r"\bgit\s+reset\s+--hard\b")),
    ("high", "delete untracked files", re.compile(r"\bgit\s+clean\s+-[a-z]*f", re.I)),
    ("high", "publish or merge without review", re.compile(r"\b(npm\s+publish|gh\s+pr\s+merge)\b", re.I)),
    ("high", "truncate a file", re.compile(r"\btruncate\s+-s\s*0\b", re.I)),
    ("medium", "delete a path", re.compile(r"\brm\s+")),
    ("medium", "delete through find", re.compile(r"\bfind\b[^\n]*\s-delete\b")),
    ("medium", "delete through xargs", re.compile(r"\bxargs\b[^\n]*\brm\b")),
    ("medium", "edit a file in place", re.compile(r"\bsed\s+-i\b")),
    ("medium", "discard working-tree edits", re.compile(r"\bgit\s+checkout\s+--\s+(\.|/)|git\s+restore\s+\.", re.I)),
]
ORDER = {"low": 0, "medium": 1, "high": 2, "critical": 3}


def scan(text: str) -> dict:
    hits = []
    for severity, label, pattern in RULES:
        for match in pattern.finditer(text):
            hits.append(
                {
                    "severity": severity,
                    "rule": label,
                    "match": match.group(0).strip(),
                    "line": text.count("\n", 0, match.start()) + 1,
                }
            )
    highest = "low"
    for hit in hits:
        if ORDER[hit["severity"]] > ORDER[highest]:
            highest = hit["severity"]
    return {"highest": highest, "hits": hits}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", nargs=argparse.REMAINDER, help="the command text to scan")
    parser.add_argument("--file", help="read the text from a file instead")
    parser.add_argument("--fail-on", choices=ORDER, default="high")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    if args.file:
        from pathlib import Path

        text = Path(args.file).read_text(encoding="utf-8", errors="replace")
    else:
        text = " ".join(args.command).strip()
    if not text:
        parser.error("give a command or --file")
    result = scan(text)
    if args.json:
        print(json.dumps(result, indent=2))
    else:
        print(f"risk: {result['highest']}")
        for hit in result["hits"]:
            print(f"  {hit['severity']}: {hit['rule']}: {hit['match']}")
    return 2 if ORDER[result["highest"]] >= ORDER[args.fail_on] else 0


if __name__ == "__main__":
    raise SystemExit(main())
