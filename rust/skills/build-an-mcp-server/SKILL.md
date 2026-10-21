---
name: build-an-mcp-server
description: Use when a capability should be reachable as an MCP tool, or when a server must be added to Hades. Covers the stdio JSON-RPC shape, a precise tool schema, secrets, a local test, and the config entry with an explicit allow list.
---

# Build an MCP server

An MCP server is a small program that advertises tools over JSON-RPC and answers one call at a time. Build
it for a capability that is naturally a function: a lookup, a converter, a fetcher, a narrow API. Do not
build one to wrap a workflow that a skill can already describe.

## When to use

- A repeated action needs typed arguments and a structured result.
- A system has an API and the agent should reach it without a shell command assembled at run time.
- A person asks to add a server, or a skill keeps shelling out to the same script.

## Steps

1. **Pick the transport.** stdio, one process per session. The server reads a JSON-RPC line from stdin and
   writes one JSON object per line to stdout. Logs go to stderr, never stdout.
2. **Answer the three methods.** `initialize` returns the protocol version (`2024-11-05` for Hades) and the
   server name. `tools/list` returns name, description and JSON Schema for each tool. `tools/call` receives
   `{name, arguments}` and returns `{content: [{type: "text", text: "..."}], isError?}`.
3. **Write one tool per action.** A tool named `search_notes` with `{query, limit}` beats a tool named `run`
   with a free-text command. The description is what the model routes on: say what it returns and when to
   use it.
4. **Validate at the edge.** Reject missing arguments, clamp limits, set a timeout, and cap the result text.
   Hades already caps a result at 30,000 characters, but a server should not rely on the client to protect
   the prompt.
5. **Keep secrets out of arguments and files.** Read them from the environment. In the Hades config, a value
   beginning `env:` names a variable in the environment or `~/.hades/.env`; anything else is a literal.
6. **Test before registering.** Speak the protocol directly: send `initialize`, `tools/list`, then one
   `tools/call` with a valid argument and one with a missing field. Check the error, not only the happy path.
7. **Register with an explicit allow list.** Hades refuses to expose a tool the person did not name. `allow`
   is a list of tool names; `*` means all and should be rare.

```json
{
  "mcp": {
    "servers": {
      "notes": {
        "command": "python3",
        "args": ["/path/to/server.py"],
        "env": {"NOTES_TOKEN": "env:NOTES_TOKEN"},
        "allow": ["search_notes", "read_note"],
        "timeout_s": 30
      }
    }
  }
}
```

## Check

Probe the server through the protocol before registering it:

```bash
python3 "$(hades skills path build-an-mcp-server)/scripts/mcp_probe.py" \
  --tool search_notes --args '{"query": "release"}' -- python3 server.py
```

It sends `initialize`, `tools/list` and one call, validates the shape and prints the server's own error.

The server answers `tools/list`, a valid call returns content, an invalid call returns a clear error, and
`hades doctor` still passes. A tool that is advertised but not in `allow` must be refused when called.

## Pitfalls

- Printing a log line to stdout breaks the JSON-RPC stream. Use stderr.
- A vague description makes the model call the wrong tool. Name the return shape and the trigger.
- A server that starts on every message is costly. Reuse is the client's job; keep startup cheap and
  stateless where possible.
- Passing a token in `args` leaks it into the process table and the config. Use `env:`.
