#!/usr/bin/env python3
"""Speak the MCP stdio protocol to a server and check its answers.

This is the test the build-an-mcp-server skill asks for: initialize, tools/list, then one valid call and
one call with a missing field. It prints the protocol errors instead of hiding them behind a client.
"""

from __future__ import annotations

import argparse
import json
import queue
import subprocess
import threading
import time


class Stream(threading.Thread):
    def __init__(self, pipe, lines: queue.Queue[str]) -> None:
        super().__init__(daemon=True)
        self.pipe = pipe
        self.lines = lines

    def run(self) -> None:
        try:
            for line in self.pipe:
                self.lines.put(line)
        except Exception:
            pass


class Probe:
    def __init__(self, command: list[str], timeout: float) -> None:
        self.command = command
        self.timeout = timeout
        self.out: queue.Queue[str] = queue.Queue()
        self.err: list[str] = []
        self.proc = subprocess.Popen(
            command,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
        )
        Stream(self.proc.stdout, self.out).start()
        threading.Thread(target=self._drain_stderr, daemon=True).start()
        self.next_id = 1

    def _drain_stderr(self) -> None:
        if self.proc.stderr is None:
            return
        for line in self.proc.stderr:
            self.err.append(line.rstrip())
            if len(self.err) > 40:
                self.err.pop(0)

    def send(self, method: str, params: dict | None = None) -> dict:
        if self.proc.stdin is None:
            raise RuntimeError("server stdin is closed")
        ident = self.next_id
        self.next_id += 1
        message = {"jsonrpc": "2.0", "id": ident, "method": method}
        if params is not None:
            message["params"] = params
        self.proc.stdin.write(json.dumps(message) + "\n")
        self.proc.stdin.flush()
        deadline = time.monotonic() + self.timeout
        while time.monotonic() < deadline:
            if self.proc.poll() is not None:
                raise RuntimeError(f"server exited {self.proc.returncode}: {' '.join(self.err[-3:])}")
            try:
                line = self.out.get(timeout=min(0.2, max(0.01, deadline - time.monotonic())))
            except queue.Empty:
                continue
            try:
                answer = json.loads(line)
            except json.JSONDecodeError:
                continue
            if answer.get("id") == ident:
                return answer
        raise TimeoutError(f"{method} did not answer in {self.timeout:g}s")

    def close(self) -> None:
        if self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(2)
            except subprocess.TimeoutExpired:
                self.proc.kill()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--timeout", type=float, default=10.0)
    parser.add_argument("--tool", help="also call this tool")
    parser.add_argument("--args", default="{}", help="JSON arguments for --tool")
    parser.add_argument("--json", action="store_true")
    parser.add_argument("server", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.server[1:] if args.server[:1] == ["--"] else args.server
    if not command:
        parser.error("give the server command after --")
    try:
        arguments = json.loads(args.args)
    except json.JSONDecodeError as error:
        parser.error(f"--args is not JSON: {error}")

    probe = Probe(command, args.timeout)
    report: dict = {"command": command, "ok": False, "steps": []}
    try:
        init = probe.send(
            "initialize",
            {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "hades-mcp-probe", "version": "1"},
            },
        )
        report["initialize"] = init
        if "result" not in init:
            report["error"] = "initialize returned no result"
            return finish(report, args.json)
        listed = probe.send("tools/list")
        report["tools"] = listed
        tools = (listed.get("result") or {}).get("tools")
        if not isinstance(tools, list):
            report["error"] = "tools/list returned no tools array"
            return finish(report, args.json)
        report["tool_names"] = [tool.get("name") for tool in tools if isinstance(tool, dict)]
        for tool in tools:
            if not isinstance(tool, dict) or not tool.get("name") or not tool.get("inputSchema"):
                report["error"] = "a tool is missing name or inputSchema"
                return finish(report, args.json)
        if args.tool:
            called = probe.send("tools/call", {"name": args.tool, "arguments": arguments})
            report["call"] = called
            if "result" not in called:
                report["error"] = "tools/call returned no result"
                return finish(report, args.json)
        report["ok"] = True
        return finish(report, args.json)
    except Exception as error:
        report["error"] = str(error)
        return finish(report, args.json)
    finally:
        probe.close()


def finish(report: dict, as_json: bool) -> int:
    if as_json:
        print(json.dumps(report, indent=2))
    else:
        print("ok" if report["ok"] else "not ok")
        if report.get("tool_names"):
            print("tools: " + ", ".join(report["tool_names"]))
        if report.get("error"):
            print("error: " + report["error"])
    return 0 if report["ok"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
