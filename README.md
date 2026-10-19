<p align="center">
  <img src="assets/hades-logo.jpg" alt="Hades Agent, a blue-flamed skull" width="720">
</p>

<h1 align="center">Hades Agent</h1>

<p align="center">
  <strong>A governed agent runtime in Rust.</strong><br>
  One binary. Four crates. Its own state.
</p>

<p align="center">
  <a href="https://www.rust-lang.org/"><img alt="Rust 2024" src="https://img.shields.io/badge/rust-2024-orange?logo=rust"></a>
  <img alt="Tests" src="https://img.shields.io/badge/tests-610%20passing-brightgreen">
  <img alt="License" src="https://img.shields.io/badge/license-MIT-blue">
  <img alt="Status" src="https://img.shields.io/badge/status-active-brightgreen">
</p>

Hades runs a conversation, a scheduled job, or a piece of long-horizon work with a check and an attempt
budget. The point is not a plausible answer. The point is a result you can inspect afterwards: what ran,
what was allowed, what was refused, what changed, and why the run stopped.

It is a single-user runtime built around one rule:

> **A rule that can be a type is a type.**

A read job is not asked to avoid writing. It is handed no tool that writes. A memory change cannot happen
without a write permit. A refusal is a recorded decision, not a sentence in a prompt.

## Lineage and inspirations

Hades is a distilled harness, not a new kind of agent. Its direct ancestor is
[Hermes Agent](https://github.com/NousResearch/hermes-agent) by Nous Research, released under the MIT
License. Hades began as a fork of that harness and was then rebuilt as a Rust runtime. The tool loop, the
skill model, the session shape, the gateway and the memory seam are all distillations of ideas proved
there. The upstream Python tree is not shipped in this checkout.

The interaction model also owes a debt to the terminal coding agents that made plan, act, check and retry
feel normal, especially [Claude Code](https://github.com/anthropics/claude-code) and the
[Codex CLI](https://github.com/openai/codex). Hades does not wrap either one. The loop is local.

The isolation model builds on [Linux Landlock](https://landlock.io/). The memory record builds on
[SQLite FTS5](https://www.sqlite.org/fts5.html). Those are foundations, not branding.

If you want the original harness this distills, start with Hermes Agent. Hades is the smaller, stricter
reading of it: the same shape, with the loop, the permissions and the evidence trail made explicit.

---

## At a glance

| | |
|---|---|
| **Language** | Rust 2024 |
| **Shape** | One `hades` binary, four crates |
| **State** | `~/.hades`: config, sessions, memory, jobs, approvals, ledger |
| **Work profiles** | `survey`, `fix`, `sweep`, `verify`, `record` |
| **Isolation** | Landlock cage plus a deterministic safety floor |
| **Memory** | SQLite record with a markdown export and FTS5 recall |
| **Interfaces** | CLI, ACP over stdio, HTTP read/control surface, Telegram gateway |
| **Browser** | Native Chrome DevTools Protocol client over Chromium's pipe |
| **Tests** | 610 passing, 13 ignored live tests |

---

## Why this exists

Most agent runtimes optimize for the next response. Hades optimizes for the run that has to survive being
trusted.

- A change runs inside a declared profile with a fixed permission shape.
- Every file write goes through governance, and every refusal is a ledger line.
- A run writes a plan when the profile asks for one, then a program, then a check.
- Retries are bounded, and every recovery edge is declared in the run graph.
- A finished run leaves a receipt that names the prompt, the tools, the check, the cost and the decision.
- Memory is written by the curator, never by every passing thought.
- A skill is not adopted because a run passed once. It has to survive replay and a protected holdout.

The result is a small, opinionated runtime for work where "the model said it worked" is not evidence.

---

## Governed runs

```sh
hades run "make the failing test pass"
```

`hades run` walks a graph:

1. Read the repository map and the goal.
2. Write a plan when the profile asks for one.
3. Write a program or a structured edit.
4. Run it in a Landlock cage.
5. Run the check.
6. Retry within the attempt budget, with the check's output as the next observation.
7. Stop with a decision that names what happened.

`--branch n` runs up to four candidate programs from the same tree. Each candidate gets its own session.
A failed candidate is rolled back, and the first candidate the check accepts is kept. The attempt budget is
shared, so width costs model calls, not a second budget. When nothing passes, the failure says which
candidate got furthest, using the check's optional `HADES-CHECK` progress line. Partial progress orders the
failure. It never turns a failed check into a pass.

---

## Permissions are shapes, not suggestions

| Profile | Shape | What it can do |
|---|---|---|
| `survey` | Read-only | Inspect, search, map, explain |
| `fix` | Narrow write | One small, checked change |
| `sweep` | Planned write | A planned change across files |
| `verify` | Read-only | Check the work on a different model |
| `record` | Memory only | Write the durable note |

The role is enforced in the tool schema and checked again in `execute`. A profile cannot gain a tool by
asking for it in prose. Approvals are separate from permissions: a standing shell approval does not answer
a safety-floor category.

---

## The cage, the floor, and the ledger

- **Landlock** denies reads outside the workspace and the state scratch. The process cannot browse the
  rest of the filesystem just because it has a shell.
- **The floor** refuses permanent deletes, money movement, credential changes, new security access and
  outbound messages before a tool runs.
- **The ledger** records every allow and every refusal, including refusals that never reached a model.
- **The boundary** refuses `~/.hermes` and the live knowledge vault by default, so a coding run cannot
  wander into the second brain.

The network is open or closed for a role. It is not filtered per host. That limitation is deliberate for
now and is listed again below.

---

## Sessions are trees

Sessions are append-only JSONL logs. A fork copies a chain to an entry and leaves the source byte-identical.
A branch of history is a first-class object, not a copied chat.

```sh
hades session list
hades session show <id>
hades explain <session>
hades fork <session> --at <n>
```

Compaction is a view over the tree. A cut does not erase the turns behind it. Plans survive a cut and stay
visible to later turns.

---

## Memory with one writer

Mneme is a SQLite record with a markdown export:

- FTS5 search plus exact-topic recall under a token budget.
- `supersedes` retirement in both directions, so a stale note can be retired without deleting history.
- Curator-only writes through a `WritePermit`. The permit is a type, not a convention.
- One durable note per accepted run, written after the check passes.
- A markdown export that stays readable without the database.

The store is local. Nothing is sent to a hosted memory service.

---

## A native browser, not a wrapper

The browser is a Rust module that speaks Chrome DevTools Protocol over the pipe Chromium opens with
`--remote-debugging-pipe`. The browser path has no Python, Playwright or Node dependency. It launches the
patched Chromium binary when one is present and falls back to stock Chrome.

The model sees one tool with an observe-act loop:

- `observe` returns a compact accessibility tree with stable refs, accessible names, values, checked and
  disabled state, links, headings and focus.
- `click`, `type` and `press` act on a ref or a selector and return a fresh observation.
- `wait` waits for text, a URL fragment or a selector with a deadline.
- Tabs and popups are real CDP targets.
- Same-origin and cross-origin (OOPIF) frames are entered through their own sessions.
- Dialogs are answered from the event loop instead of hanging the call.
- Downloads, uploads, file chooser interception, network history, blocking, response bodies, cookies and
  storage are available.
- Isolated browser contexts keep separate cookie jars.
- Proxy credentials are answered through Fetch authentication.
- `HADES_HEADED`, `HADES_HUMANIZE`, `HADES_TIMEZONE`, `HADES_LOCALE` and `HADES_PROXY` control the launch.

`eval` remains an escape hatch. It can do things the textual safety floor cannot see, and that limit is
stated rather than hidden.

---

## Delegation with a ceiling

A delegated task names a JSON Schema. The worker has to return an object that fits it, and the parent gets a
`result.json` instead of a paragraph. Every worker draws from the conversation's single model-call
allowance, so a message cannot become an unbounded tree.

`spawn` creates one child graph with its own session and result schema. The child returns to its parent,
depth caps at two, and the same allowance pays for the whole tree. A failed child still leaves
`result.json`, so the parent gets a typed failure instead of silence.

---

## Jobs that stop

Standing goals live under `<state>/jobs` and are re-read on every scheduler tick.

- `read` or `act` mode.
- Intent fields: `inputs`, `outputs`, `done_when`, `stop_when`, `until`, `never`.
- Three tries per occurrence, with 30s and 120s backoff.
- A pending retry survives a daemon restart.
- `until: YYYY-MM-DD` is enforced by the scheduler, not left as prose for a model.

The daemon starts by default. The gateway does not.

---

## Evidence before memory

A pass alone does not write a proposal.

```sh
hades distill <session>
hades distill <session> --run
hades replay <runbook> --check "cargo test"
```

`hades distill` drafts a runbook from a verified session and gates it for absolute paths, secrets and
verifier leakage. `--run` proves it in a fresh copy of the session's first snapshot. Only a runbook that
passes that independent replay becomes a proposal.

The proposal records the check's fingerprint, never the check command. A skill that leaks its verifier
turns every later run into a game against the same test.

`hades tune --holdout <cmd>` adds a protected check the candidate never saw. A candidate that regresses the
holdout is never adopted, even if it wins the visible check.

---

## Skills

Eleven built-in procedures ship in the binary and are seeded into `~/.hades/skills/builtin/` by
`hades init`:

- plan a change
- repair a failing check
- review a diff
- research with sources
- write for humans
- safety before acting
- keep memory useful
- create a skill
- land a change
- build an MCP server
- run and verify

Nothing personal is inherited. `hades skills seed` writes any missing built-ins after an upgrade.
Proposals are reviewed before promotion.

---

## Interfaces

- **CLI.** `hades chat`, `hades run`, `hades job`, `hades doctor`, and the rest of the command set.
- **ACP.** `hades acp` serves the Agent Client Protocol over stdio for an editor.
- **HTTP.** A local read-and-control surface for status, sessions and approvals.
- **Telegram.** Long polling with a numeric user allowlist and an approval flow. The gateway is opt-in.

There is no Discord transport.

---

## Architecture

```text
┌──────────────────────────────────────────────────────────────┐
│ hades                                                        │
│ CLI · daemon · gateway · ACP · HTTP                          │
└──────────────────────────────┬───────────────────────────────┘
                               │
┌──────────────────────────────▼───────────────────────────────┐
│ harness                                                      │
│ turn · run · chat · tools · profiles · cage · floor · judge   │
└───────────┬──────────────────┬──────────────────┬────────────┘
            │                  │                  │
┌───────────▼────────┐ ┌───────▼─────────┐ ┌──────▼───────────┐
│ session-tree       │ │ mneme           │ │ governance       │
│ JSONL logs and fork│ │ SQLite + markdown│ │ permits and audit│
└────────────────────┘ └─────────────────┘ └──────────────────┘
```

The Rust crates are new work. `session-tree`, `mneme` and `governance` were ported from a Python
specification and are covered by parity oracles that drive both shapes. `harness` is designed in Rust.

The upstream Hermes Agent Python tree used as the porting reference is not part of this checkout. The Rust
runtime is self-contained. See `NOTICE` for the lineage.

---

## Install

### Requirements

- Linux with Landlock support. A recent kernel is recommended.
- Stable Rust with the 2024 edition.
- A systemd user manager if you want the daemon or gateway units.
- Chromium, Chrome or the patched CloakBrowser build if you want the browser tool.
- No Python or Node runtime is required for the Rust product.

### Build from source

```sh
git clone https://github.com/GuideboardLabs/hades-agent.git
cd hades-agent/rust

cargo build --release
./install.sh --bin target/release/hades --home ~/.hades --root ~/.hades/work

hades init
hades doctor
```

`install.sh` writes:

- `~/.local/bin/hades`
- `~/.hades/` for config, sessions, memory and approvals
- `~/.hades/work/` as the default working tree
- `hades-daemon.service` and `hades-gateway.service` under the user systemd directory

It does not start the services unless you ask. It never uses `sudo`, downloads anything, or reads
`~/.hermes`.

### First run

```sh
hades doctor
hades models
hades chat
```

Provider keys belong in `~/.hades/.env`, never in `config.json`. The gateway additionally needs
`HADES_TELEGRAM_TOKEN` and a non-empty `gateway.telegram.allowed_users` list.

---

## Command map

| Command | What it does |
|---|---|
| `hades init` | Write the home, config and vault skeleton |
| `hades chat` | A conversation with the tool cage |
| `hades run <goal>` | A governed walk: plan, program, check, attempts, `--branch n` |
| `hades explain <session>` | A compact turn map: action, state, tokens, decision |
| `hades distill <session>` | Draft a runbook from a verified run; `--run` proves it |
| `hades replay <runbook>` | Execute a runbook in a fresh root against `--check <cmd>` |
| `hades tune <goal>` | Probe candidate harnesses; `--holdout` guards adoption |
| `hades job <spec>` | Write a scheduled job |
| `hades jobs` | List scheduled jobs |
| `hades profiles` | Show the five profiles and their parameters |
| `hades route <goal>` | Show which profile the rules pick, and why |
| `hades scoreboard` | What finished runs say per profile |
| `hades skills list\|seed\|promote\|reject` | Manage built-in and proposed skills |
| `hades session list\|show <id>` | Inspect the session tree |
| `hades fork <session>` | Copy a chain to an entry, leaving the source alone |
| `hades acp` | Serve ACP on stdio for an editor |
| `hades doctor [--services on]` | Describe this install; writes nothing |
| `hades config` | Print the config a headless process reads |
| `hades audit` | Run the governance audit over the record |

`hades help` lists every command and flag.

---

## Configuration

```text
~/.hades/
├── config.json        non-secret settings
├── .env               provider keys and the Telegram token, mode 600
├── sessions/          append-only JSONL session trees
├── mneme/             SQLite record and markdown export
├── jobs/              standing goals and schedules
├── approvals/         append-only approval ledger
├── skills/            built-in, installed and proposed skills
└── work/              default working tree for runs
```

Keep `~/.hades` out of version control. The repository contains no state, no sessions, no keys and no
provider configuration.

---

## Security model

- **Least authority by profile.** A read job has no mutating tool at all.
- **Two independent checks.** The tool schema and `execute` both enforce the profile.
- **Landlock.** Reads are limited to the workspace and state scratch.
- **Deterministic floor.** Destructive commands, credential changes, money movement, new access and
  outbound messages are refused before execution.
- **Approval ledger.** Approvals survive a daemon restart. A standing shell approval does not answer a
  safety-floor category.
- **Curator-only memory.** Writes need a `WritePermit` whose constructor is the vault itself.
- **Verifier withholding.** Distilled runbooks store a check fingerprint, not the command.
- **No secrets in the repository.** Keys are read from the environment or `~/.hades/.env` and are never
  printed.

---

## Tests

```sh
cd rust
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Current state: **610 passing, 0 failing, 13 ignored**.

The ignored tests start a real Chromium and exercise live browser behavior. Run them on a machine with a
browser installed:

```sh
cargo test -p harness --lib browser::tests -- --ignored --test-threads=1
```

Acceptance scripts live in `rust/acceptance/`.

---

## Limits

These are current limits, not aspirations hidden in a roadmap:

- Linux only. Landlock and systemd units are part of the design.
- Telegram is the only messaging transport.
- The network is open or closed per role. There is no per-domain egress proxy.
- Restoring an adoption is a copy from `versions/<seconds>/`; there is no automatic rollback command yet.
- The browser needs an external Chromium binary. Vision is optional and needs local GGUF model files.
- `eval` can act outside the textual safety floor. It is an explicit escape hatch.

---

## Documentation

- [`HADES.md`](HADES.md): the design rule, the architecture and the long-form reasoning.
- [`rust/README.md`](rust/README.md): the crate map and porting notes.
- [`NOTICE`](NOTICE): lineage and upstream attribution.
- [`LICENSE`](LICENSE): MIT.

---

## License

MIT. Copyright (c) 2026 Guideboard Labs. Portions copyright (c) 2025 Nous Research, derived from Hermes Agent.
See [`NOTICE`](NOTICE) for details.
