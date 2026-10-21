#!/usr/bin/env python3
"""Flag filler, hype, em dashes and sentences that are too long.

The rules are deliberately narrow. This is a mechanical pass before a person reads the draft, not a
substitute for judgement about whether the argument is any good.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

FILLER = [
    (re.compile(r"\bit is important to note\b", re.I), "filler"),
    (re.compile(r"\bin today'?s world\b", re.I), "filler"),
    (re.compile(r"\blet'?s dive in\b", re.I), "filler"),
    (re.compile(r"\bat the end of the day\b", re.I), "filler"),
    (re.compile(r"\bneedless to say\b", re.I), "filler"),
    (re.compile(r"\bplease note that\b", re.I), "filler"),
]
HYPE = [
    (re.compile(r"\b(seamless|revolutionary|game-changing|cutting-edge|world-class|best-in-class)\b", re.I), "hype"),
    (re.compile(r"\b(robust|powerful|comprehensive)\b", re.I), "hype"),
]
WEAK = [
    (re.compile(r"\b(very|really|actually|basically)\b", re.I), "weak modifier"),
]
SENTENCE = re.compile(r"[^.!?\n]+[.!?]?")


def findings(text: str, max_words: int) -> list[dict]:
    found: list[dict] = []
    for pattern, rule in FILLER + HYPE + WEAK:
        for match in pattern.finditer(text):
            found.append(
                {
                    "line": text.count("\n", 0, match.start()) + 1,
                    "column": match.start() - text.rfind("\n", 0, match.start()),
                    "rule": rule,
                    "match": match.group(0),
                }
            )
    for match in re.finditer("\u2014", text):
        found.append(
            {
                "line": text.count("\n", 0, match.start()) + 1,
                "column": match.start() - text.rfind("\n", 0, match.start()),
                "rule": "em dash",
                "match": "\u2014",
            }
        )
    for match in SENTENCE.finditer(text):
        sentence = match.group(0).strip()
        words = len(sentence.split())
        if words > max_words:
            found.append(
                {
                    "line": text.count("\n", 0, match.start()) + 1,
                    "column": match.start() - text.rfind("\n", 0, match.start()) + 1,
                    "rule": f"sentence over {max_words} words",
                    "match": f"{words} words: {sentence[:80]}",
                }
            )
    return sorted(found, key=lambda item: (item["line"], item["column"], item["rule"]))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("path", nargs="?", default="-", help="a text file, or - for stdin")
    parser.add_argument("--max-sentence-words", type=int, default=35)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    text = sys.stdin.read() if args.path == "-" else Path(args.path).read_text(encoding="utf-8")
    found = findings(text, args.max_sentence_words)
    if args.json:
        print(json.dumps(found, indent=2))
    else:
        for item in found:
            print(f"{item['line']}:{item['column']}: {item['rule']}: {item['match']}")
    return 1 if found else 0


if __name__ == "__main__":
    raise SystemExit(main())
