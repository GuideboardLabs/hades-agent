#!/usr/bin/env python3
"""A real stdio MCP server, small enough to read.

The oracle starts this as a child process, so it proves the client's process, framing,
initialisation, discovery and call paths rather than mocking them. It advertises one tool
that may run (`echo`), one that is never on the allowlist (`secret`), and one that sleeps
so a timeout can be measured (`slow`).
"""

import json
import sys
import time

TOOLS = [
    {
        "name": "echo",
        "description": "Echo the text back.",
        "inputSchema": {
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"],
        },
    },
    {
        "name": "secret",
        "description": "A tool the config never allows.",
        "inputSchema": {"type": "object", "properties": {}},
    },
    {
        "name": "slow",
        "description": "Sleep past a one second deadline.",
        "inputSchema": {"type": "object", "properties": {}},
    },
]


def answer(request_id, result):
    print(json.dumps({"jsonrpc": "2.0", "id": request_id, "result": result}), flush=True)


for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    if request_id is None:
        continue
    if method == "initialize":
        answer(
            request_id,
            {
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "echo-fixture", "version": "1.0"},
            },
        )
    elif method == "tools/list":
        answer(request_id, {"tools": TOOLS})
    elif method == "tools/call":
        params = message.get("params") or {}
        name = params.get("name")
        arguments = params.get("arguments") or {}
        if name == "echo":
            answer(
                request_id,
                {
                    "content": [{"type": "text", "text": str(arguments.get("text", ""))}],
                    "isError": False,
                },
            )
        elif name == "secret":
            answer(
                request_id,
                {
                    "content": [{"type": "text", "text": "the secret tool ran"}],
                    "isError": False,
                },
            )
        elif name == "slow":
            time.sleep(5)
            answer(
                request_id,
                {"content": [{"type": "text", "text": "too late"}], "isError": False},
            )
        else:
            answer(
                request_id,
                {
                    "content": [{"type": "text", "text": "no such tool"}],
                    "isError": True,
                },
            )
    else:
        answer(request_id, {})
