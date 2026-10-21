#!/usr/bin/env python3
"""Summarise a diff: files, line counts, tests touched and code without a test.

This is the mechanical part of a review, not the review. It answers what changed and whether any test
changed with it, so the reader can spend attention on the cases the author missed.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess

TEST = re.compile(r"(^|/)(tests?|__tests__|spec)(/|$)|(?:^|/)[^/]+(?:[._-](?:test|spec))\.[A-Za-z0-9]+$")
SOURCE = re.compile(r"\.(rs|py|ts|tsx|js|jsx|go|java|c|cc|cpp|h|hpp|rb|php|cs|swift|kt|kts|lua|sh|bash)$")


def git(args: list[str]) -> str:
    result = subprocess.run(["git", *args], capture_output=True, text=True)
    if result.returncode != 0:
        raise SystemExit(result.stderr.strip() or f"git {' '.join(args)} failed")
    return result.stdout


def brief(spec: list[str]) -> dict:
    numstat = git(["diff", "--numstat", *spec])
    status = git(["diff", "--name-status", *spec])
    files = []
    for line in numstat.splitlines():
        parts = line.split("\t")
        if len(parts) != 3:
            continue
        added, deleted, path = parts
        files.append(
            {
                "path": path,
                "added": None if added == "-" else int(added),
                "deleted": None if deleted == "-" else int(deleted),
                "test": bool(TEST.search(path)),
                "source": bool(SOURCE.search(path)),
            }
        )
    tests = [item["path"] for item in files if item["test"]]
    sources = [item["path"] for item in files if item["source"] and not item["test"]]
    return {
        "spec": spec,
        "files": files,
        "status": [line for line in status.splitlines() if line],
        "tests_touched": tests,
        "source_without_test": sources if not tests else [],
        "added": sum(item["added"] or 0 for item in files),
        "deleted": sum(item["deleted"] or 0 for item in files),
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", help="compare this ref with HEAD, for example main")
    parser.add_argument("--staged", action="store_true")
    parser.add_argument("--worktree", action="store_true")
    parser.add_argument("--require-tests", action="store_true")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    if args.base and (args.staged or args.worktree):
        parser.error("--base cannot be combined with --staged or --worktree")
    spec = [f"{args.base}...HEAD"] if args.base else ["--cached"] if args.staged else []
    result = brief(spec)
    if args.json:
        print(json.dumps(result, indent=2))
    else:
        print(f"{len(result['files'])} file(s), +{result['added']} -{result['deleted']}")
        for item in result["files"]:
            mark = "test" if item["test"] else "src " if item["source"] else "    "
            print(f"  {mark} {item['path']}  +{item['added']} -{item['deleted']}")
        if result["source_without_test"]:
            print("no test file changed for: " + ", ".join(result["source_without_test"]))
    if args.require_tests and result["source_without_test"]:
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
