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
  <img alt="Version" src="https://img.shields.io/badge/version-0.1.22-blue">
  <img alt="Tests" src="https://img.shields.io/badge/tests-1095%20passing-brightgreen">
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
| **Version** | 0.1.22 |
| **Language** | Rust 2024 |
| **Shape** | One `hades` binary, four crates |
| **State** | `~/.hades`: config, sessions, memory, jobs, approvals, activity, taste |
| **Work profiles** | `survey`, `fix`, `sweep`, `verify`, `record` |
| **Isolation** | Landlock cage plus a deterministic safety floor |
| **Platform** | Linux, x86-64 and ARM64. x86-64: `hades-portable` needs glibc 2.30+ (Ubuntu 20.04+, Debian 11+, RHEL 9+); `hades-musl` is static, needing `curl` and GNU `tar`. ARM64: `hades-arm64-gnu` needs glibc 2.31+ (Ubuntu 20.04+, Debian 11+); `hades-arm64-musl` is static with the same curl/tar requirement. All four built by `scripts/portable-build.sh` and `scripts/arm64-build.sh`, and the ARM64 ones are run under qemu before release |
| **Memory** | SQLite record with a markdown export and FTS5 recall |
| **Interfaces** | Native TUI, CLI, ACP over stdio, HTTP read/control surface, Telegram gateway |
| **Browser** | Native Chrome DevTools Protocol client over Chromium's pipe |
| **Tests** | 1095 passing, 13 ignored (12 live browser) |

---

## Releases

What each version added, newest first: [`RELEASES.md`](RELEASES.md).

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

`--strict-model` holds a run to the model it names. `turn`, `run` and `submit` accept it. The endpoint and
the key are checked before a session is opened, so a request that cannot work is refused while the refusal
is free. A fault from the first call names that model, and nothing is substituted for it: a strict run
either used the model it asked for or says it could not.

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
  rest of the filesystem just because it has a shell. `--cage on` is the default and keeps `/run` readable;
  `--cage strict` drops it, which is the only way to close a Docker socket on a kernel whose Landlock
  cannot refuse a Unix-socket connect (ABI 8 and below). `hades doctor` names the ABI, every socket it can
  reach, and the one flag that would close it.
- **The floor** refuses permanent deletes, money movement, credential changes, new security access and
  outbound messages before a tool runs. It reads the tool name and its arguments as text, and the receipt
  says so: `floor: command-text heuristic (not a sandbox)`. It cannot see a button, a command assembled at
  run time, or a delete performed by a binary nobody listed.
- **`--policy strict`** adds a deny list of primitives (`rm -rf`, `rm -fr`, `mkfs`, `shred`,
  `of=/dev/sd|nvme|vd|hd`) ahead of the ordinary floor and ahead of any permission question. It is a named
  list, not a proof: `sh -c 'r''m -rf /'` still gets through, and the test that covers it asserts exactly
  that.
- **The ledger** records every allow and every refusal, including refusals that never reached a model. A
  write that fails is counted and named: `hades doctor` prints `activity gaps: N`, and a turn whose ledger
  lost a line prints `audit: incomplete`.
- **A run that died leaves a marker.** `<state>/inflight/<session>.json` is written when a run starts and
  removed when it ends, so `hades status` and `hades doctor` can report `interrupted: N` with the session,
  the run, the start time and the goal. Nothing is replayed automatically.
- **A model's shell is bounded by the runner, not by `timeout`.** The tool call runs under the same clock and
  process-group kill as a goal check, so a shell that exits while a background child keeps its output open
  still ends at the deadline, and the result says it timed out instead of passing partial output off as a
  finished run.
- **The process seals itself.** `PR_SET_DUMPABLE` is cleared at startup, so a caged child cannot read the
  runtime's own environment out of `/proc/<parent>/environ`; a cleared child environment is not enough on
  its own. The flag belongs to the process and is reset by `execve`, so the guarantee is about the runtime's
  own entry, which is the one holding the keys, not a permanent restriction on everything it starts. A
  failure to set it stops the process, because a runtime that keeps running unsealed is a runtime that
  accepted the risk silently; `HADES_ALLOW_PTRACE=1` is the explicit way to accept it.
- **The boundary** refuses another agent's home and any notes vault by default, so a coding run cannot
  wander into somebody's notes.

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

- Six areas: Inbox, Outbox, Research, Projects, Products, Topics.
- Topics are a tree under 26 fixed categories; subtopics nest without inventing a new category.
- Every research note, project and product names a topic; a durable project names its research; a product names the project it came from.
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

Standing jobs live under `<state>/jobs` and are re-read on every scheduler tick. A chat's standing goal is a different record, under `<state>/goals`, and the gateway's heartbeat continues it.

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

- **Terminal workspace.** `hades`, `hades tui`, or `hades chat`: streaming chat, framed messages, sessions, jobs, approvals, activity, skills, and usage.
- **CLI.** `hades chat`, `hades run`, `hades jobs`, `hades doctor`, and the rest of the command set.
- **ACP.** `hades acp` serves the Agent Client Protocol over stdio for an editor.
- **HTTP.** A local read-and-control surface for status, sessions and approvals.
- **Telegram.** Long polling with a numeric user allowlist and an approval flow. The gateway is opt-in.

Discord is not built in. It is a connector pack installed from the shelf: it answers on the
gateway's model and provider unless its own config routes it elsewhere, and it carries its own
opening prompt and working animation, both editable without rebuilding anything.

---

## Terminal workspace

![The native Hades terminal workspace, with illustrative offline demo data](assets/tui-preview.png)

[View the animated flame preview](assets/tui-flame.gif).

```sh
hades                         # open the configured workspace in an interactive terminal
hades tui --root /path/to/repo
hades tui --state /path/to/state   # runtime state directory (default `~/.hades`)
hades tui --resume ID              # branch a saved conversation into a new session file
hades tui --demo               # offline demonstration; no model calls or state writes
hades tui --no-motion          # static skull, no animated flame
hades tui --no-mouse           # preserve normal terminal mouse selection
hades tui --snapshot out.json --width 160 --height 64   # one frame, no terminal
hades tui --help
```

The banner shows the resolved workspace path, build version, a read-only comparison with GitHub's
`main` revision, and the current or most recent task brief. Its centered skull uses the supplied HTML's
true-color Unicode half-block artwork and blue-flame animation, ported into native Rust. The face stays
static while the flame colors and outline move at about 24 frames per second. The square artwork scales
to 32 columns on larger terminals and 24 on smaller ones. `--no-motion` shows the original static colors.
The artwork's outer black background blends into the surrounding slate banner; enclosed black details
in the eyes and nose remain intact. The original artwork and its import provenance are in
`assets/tui-skull.*`; the runtime embeds only
27 KiB of RGB data and needs no image decoder or browser. Frames throughout the app use filled title
bars, outward-facing half-block sides, and half-height bottom borders with joined corners. Speaker names
sit inside the top border: grey for user messages and the composer, deeper blue for Hades, and sky blue
for tool calls and the other panels. Frames use consistent row and column gutters; the workspace card
fits its content, and the input follows short conversations with a single-row gap before settling at
the bottom for longer transcripts. It grows from a single line as you type. The palette uses sky blue accents,
pale blue text, slate backgrounds, and red, green, and yellow accents for syntax and status.
Smaller terminals use a compact banner; below 56 columns or 18 rows, the screen asks to be resized.

The seven views are Conversation, Sessions, Jobs, Approvals, Activity, Skills, and Usage. They include
a searchable command palette, model preset picker, multiline editor, bracketed paste, input history,
conversation search, expandable tool output, an optional workspace inspector, session branching,
plain text export, pending approval decisions, and cancellation of scheduled jobs. Jobs and skills
are inspected from the existing runtime state; their creation and installation remain available
through the runtime's tools and CLI.

| Key | Action |
|---|---|
| Enter | Send a message, take the selected question option, or open the selected item |
| Alt+Enter / Ctrl+J | Insert a newline |
| Ctrl+P | Search commands |
| F2 | Choose a model preset for this session |
| Tab | Move between views |
| Shift+Tab | Cycle Read → Plan → Act in Conversation; move back a view elsewhere |
| F3 / F4 / F5 | Sessions / approvals / refresh |
| Page Up / Page Down | Scroll conversation or overlay |
| Ctrl+Home / Ctrl+End | Conversation start / follow latest output |
| Ctrl+F | Highlight matching conversation lines |
| Ctrl+T / Ctrl+B | Tool details / workspace inspector |
| Ctrl+N / Ctrl+E | New conversation / export |
| Esc | Collapse a question; close overlay, clear search, or request cancellation |
| Ctrl+C | Request cancellation while busy; quit when idle |
| Ctrl+Q | Save and quit; wait for a running operation's cancellation boundary |
| F1 | Help; Page Up / Page Down scroll it |

Direct conversation uses Hades' existing tool loop. `Shift+Tab` cycles Read → Plan → Act; Plan mode has
no mutating tools and asks at real forks before proposing a plan. `/mode read`, `/mode plan` and
`/mode act` do the same. When a question is open the bar owns the composer: Up/Down selects, Enter answers,
Esc collapses it, and typing moves into the input and selects `Other`. `Other` on a permission is deny
plus a steer, never an allow. `/run GOAL` submits a guarded run through an already-running `hades daemon`.
The same questions appear in Approvals; Enter there opens the bar on that row. A stop affects that
conversation's run. An operation already in flight may finish before cancellation is observed. The TUI
does not change the executor's isolation or approval guarantees.

Conversations and drafts are checkpointed under `STATE/tui/sessions/` at turn start, every five seconds
while working, on completion, and on exit. Writes use a private temporary file, fsync, and atomic rename.
Resuming branches into a new session file to avoid two terminals overwriting the same conversation.
Interrupted checkpoints are marked explicitly. Model context may trim complete older turns when the
context budget is tight; the visible conversation remains in its saved record. `/clear` hides the old
messages while preserving that record and model context; `/new` starts fresh. Exports go to
`STATE/tui/exports/`. The Sessions view lists terminal conversations; governed run trees remain in the
existing session-tree store and CLI.

Provider detection follows the configured model preset or explicit `--endpoint`. Credentials use the
existing environment / `HADES_HOME/.env` resolver. A custom provider can be opened with:

```sh
hades tui --model MODEL --endpoint https://provider.example/v1/chat/completions --key ENV_NAME
```

The full-width usage strip reads actual account data where supported:

- **Ollama Cloud:** included and purchased credit balances, or legacy session and weekly limits,
  from the documented [`/api/balance`](https://docs.ollama.com/api/balance) endpoint.
- **OpenRouter:** the current API key's effective budget or limit and remaining balance from
  [`/api/v1/key`](https://openrouter.ai/docs/api/api-reference/api-keys/get-current-api-key).
- **Other providers:** an explicit unavailable state for account quota. Local inference has no
  remote account quota. Consumer subscription limits are not inferred from API token counts.

Account requests refresh at most once a minute and have bounded network timeouts. Provider-reported
conversation tokens are shown separately, including usage-bearing delegated chat calls. Missing token
usage is left unreported; daemon accounting remains in its own run record. Quota, update checks, model
work, and state discovery run off the render thread. The update check never installs or fetches Git
changes; `main differs` means the revisions differ, not necessarily that the local branch is behind.

The native TUI adds Ratatui and Crossterm to the existing Rust binary. It requires no web server,
browser, Python, Node, or consumer integration. Like the existing runtime, it targets Linux; Windows
users can build and run it inside WSL2. A true-color terminal with a Unicode monospace font works best.
Keep the runtime state directory on WSL's Linux filesystem, such as `~/.hades`, so Unix sockets work.
The sandbox tolerates absent default system directories (for example `/lib32` on a 64-bit-only
installation); explicit workspace and scratch directories still have to exist.
The handled filesystem rights follow the running
[Landlock ABI](https://docs.kernel.org/userspace-api/landlock.html). Kernel limitations are included in
the cage description: TCP restriction requires ABI 4, device ioctl restriction ABI 5, and truncation
restriction ABI 3. A TUI cannot add restrictions the running kernel does not expose.

### Terminal verification

```sh
cd rust
cargo test -p harness tui::tests --lib
cargo clippy -p harness --all-targets -- -D warnings
cargo build -p harness --bin hades
python3 harness/tests/tui_pty.py target/debug/hades  # optional Linux PTY integration test
target/debug/hades tui --snapshot /tmp/hades.json --width 160 --height 64 --frame 3
```

The snapshot is the actual renderer's cell buffer, with explicitly labeled offline demo data, suitable
for reproducible visual checks. Fifteen `tui::tests` cover responsive views, animation, input, quota
response formats, streamed usage, a real local HTTP/SSE tool-call exchange, private session storage,
and resume, and run as part of `cargo test --workspace`. The PTY check exercises the command palette,
paste, view switching, resizing, clean exit, terminal restoration, and the absence of demo writes.
The original branch validation, the merge revalidation, and the kernel limitation are recorded in
the release notes.
To reimport the artwork from its original HTML, run
`python3 harness/tests/import_tui_skull.py /path/to/index.html` from `rust/` with Pillow installed.
This development helper extracts the embedded PNG without executing the HTML's JavaScript.

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
- `libcurl.so.4` is used when present, for connection reuse across fetches, and is not required: without
  it the tool runs one `curl` process per fetch. Neither is needed at build time.
- A C compiler for the bundled SQLite, and a cross toolchain only if the aarch64 check is wanted.
- No Python or Node runtime is required for the Rust product.

### Build from source

Nothing in the repository names a path on a particular machine. A build directory, a cache or a toolchain
location belongs in `$CARGO_HOME/config.toml` or the environment; a checkout is expected to build into
`./target` wherever it lands.

```sh
git clone https://github.com/GuideboardLabs/hades-agent.git
cd hades-agent/rust

cargo build --release
./install.sh

hades doctor
```

`install.sh` takes `--prefix`, `--home`, `--root`, `--bin`, `--no-init`, `--no-service`, `--no-start` and
`--with-gateway`; the defaults are `~/.local`, `~/.hades` and `~/hades-work`. It writes:

- `~/.local/bin/hades`
- `~/.hades/` for config, sessions, memory and approvals
- `~/hades-work/` as the default working tree, unless `--root` names another
- `hades-daemon.service` and `hades-gateway.service` under the user systemd directory

It starts the daemon when a user systemd manager answers; the gateway starts only with `--with-gateway`,
because it needs `HADES_TELEGRAM_TOKEN` and an allowed-users list first. It never uses `sudo`, never reads
another agent's home, and never fetches anything of its own.

### First run

```sh
hades doctor
hades models
hades chat
```

Provider keys belong in `~/.hades/.env`, never in `config.json`. The gateway additionally needs
`HADES_TELEGRAM_TOKEN` and a non-empty `gateway.telegram.allowed_users` list.

### Setting a model per profile

Every profile runs the config's `model` unless it is given its own. Add a `profile_models` object to
`config.json`:

```json
{
  "model": "deepseek-v4.1-flash",
  "profile_models": {
    "verify": "deepseek-v4.1-flash"
  }
}
```

Each key must name a shipped profile (`hades profiles`) and each value a preset (`hades models`). A typo
is refused when the config is read, not silently ignored. Precedence is `--model`, then
`profile_models.<id>`, then the profile's own setting, then the config's `model`.

---

## Command map

| Command | What it does |
|---|---|
| `hades init` | Write the home, config and vault skeleton |
| `hades` / `hades tui` / `hades chat` | Native terminal workspace; `--demo` opens an offline preview |
| `hades run <goal>` | A governed walk: plan, program, check, attempts, `--branch n`, `--budget-tokens n` |
| `hades explain <session>` | A compact turn map: action, state, tokens, decision |
| `hades distill <session>` | Draft a runbook from a verified run; `--run` proves it |
| `hades replay <runbook>` | Execute a runbook in a fresh root against `--check <cmd>` |
| `hades tune <goal>` | Probe candidate harnesses; `--holdout` guards adoption, the corpus gate refuses a regression, each adoption is a numbered generation |
| `hades cases` / `hades case <id>` | List the failure corpus, print one case, `forget <id>` retires its tree |
| `hades eval [--against <dir>]` | Replay the corpus: stale, holds, fixed, regression, still failing; `--dry-run` prices it |
| `hades refine --rollback <n>` | Restore the files named in harness generation n |
| `hades jobs` | The standing jobs, with the fields a correction edits; the agent's `job` tool writes one |
| `hades cancel <job>` | Remove a standing job and its pending retry |
| `hades goal set\|list\|pause\|resume\|clear` | A goal kept for one chat; the gateway heartbeat continues it when idle |
| `hades profiles` | Show the five profiles and their parameters |
| `hades approvals` | Every question with its options, recommendation and answer |
| `hades answer <id> <option\|allow\|deny>` | Answer from any process; `--note` carries a steer, `--scope once\|session` sets how long it holds |
| `hades log --asks <session>` | One session's questions and answers |
| `hades taste [forget <n\|all>]` | What the answers taught, and how to forget it |
| `hades route <goal>` | Show which profile the rules pick, and why |
| `hades scoreboard` | What finished runs say per profile |
| `hades skills list\|seed\|promote\|reject\|verify\|report` | Manage built-in and proposed skills; `verify` runs a proposal's own cases |
| `hades session list\|show <id>\|handover [id]` | Inspect the session tree, or read the harness-written handover |
| `hades fork <session>` | Copy a chain to an entry, leaving the source alone |
| `hades acp` | Serve ACP on stdio for an editor |
| `hades doctor [--services on]` | Describe this install; writes nothing |
| `hades config` | Print the config a headless process reads |
| `hades topic\|project\|product` | Create a topic, a project or a product through the store |
| `hades note <kind>` | Write one note of a given kind into the store |
| `hades research <question>` | One evidence-first run: plan, search, fetch, extract, verify; `--offline` reads only the local library |
| `hades library add\|list\|search\|show\|verify\|remove` | The local shelf of documents, with FTS5 search and a hash check |
| `hades mneme import\|export\|upgrade` | Move a vault in or out of the record; `upgrade` writes the new tree and can retire `40 Topics` |
| `hades pack menu\|find\|add` | Read the shipped shelf of skill, tool and test packs, search it before writing a skill by hand, and add one with a digest check before anything is read |
| `hades pack export\|inspect\|plan\|apply\|uninstall\|list` | Move skills, a harness and cases between installs as a signed-shaped archive with a decision file |
| `hades mcp list\|tools\|call` | The MCP servers the config names, what each offers, and a call to one allowed tool |
| `hades context <goal>` / `hades map <goal>` | Build the context for a goal, or rank the tree for one and say what the map cache did |
| `hades daemon\|http\|status\|submit\|attach` | Own the sessions under one state directory, or reach a running one |
| `hades service install` / `hades update` / `hades version` | Write the two user units, replace the binary atomically, print the build |
| `hades undo <session>` | Put a tree back the way a shadow snapshot found it |
| `hades audit` | Run the governance audit over the record; `--refresh` imports the config's vault first |

`hades help` lists every command and flag.

---

## Configuration

```text
~/.hades/
├── config.json        non-secret settings
├── .env               provider keys and the Telegram token, mode 600
├── sessions/          append-only JSONL session trees
├── mneme/             SQLite record and markdown export
├── jobs/              standing jobs and schedules
├── goals/             standing goals for chats, one file each
├── children/          admitted child records for the gateway
├── control/           numbered harness generations
├── approvals/         append-only approval ledger
├── skills/            built-in, installed and proposed skills
├── library/           documents added to the local shelf
├── browser/           Chromium profile and its log, one per state
├── shots/             screenshots, confined to this directory
├── shadows/           snapshots an undo restores from
├── attempts/          what each governed attempt returned
├── runbooks/          distilled procedures and their proofs
├── tmp/               short-lived files the cage may write
└── work/              working tree when config names none
```

`install.sh` sets the root to `~/hades-work` unless `--root` names another tree; `<state>/work` is the
fallback a run uses when the config names no root.

Keep `~/.hades` out of version control. The repository contains no state, no sessions, no keys and no
provider configuration.

### Providers

A provider is one endpoint, one key and any number of models. It is the shape to use when a model is
not a shipped preset, because the URL and key are written once instead of once per model:

```json
{
  "model": "groq/llama-3.3-70b",
  "providers": {
    "groq": {
      "base_url": "https://api.groq.com/openai/v1",
      "api_mode": "chat_completions",
      "key_env": "GROQ_API_KEY",
      "models": ["llama-3.3-70b", "qwen-2.5-72b"],
      "context_length": 131072
    }
  }
}
```

A model is `provider/model`, which is explicit and always wins. A bare name resolves through the
providers' own `models` lists when exactly one provider names it. `key_env` is the name of the
variable, looked up in the environment and then `~/.hades/.env`; the config never holds the value.
`base_url` is the root of the API, `hades models` prints the URL a call actually goes to, and a full
`.../chat/completions` URL is used as given so a proxy that rewrites the path still works.

`api_mode` says how the provider speaks. This build speaks `chat_completions`; naming anything else is
refused when the config is read rather than sending an OpenAI body to an endpoint that wanted
something else. `context_length` is the window the client asks for, and `thinking` is passed only when
you set it, because an unknown template should be sent nothing.

Providers are also how a gateway surface gets a model that is not the system's default: the connector
section may name `model: "groq/llama-3.3-70b"`, and it resolves through this same map. Nothing about a
provider is compiled in; `hades models` lists what the file names, and `hades config` prints the file
back field by field.

### A check that is not Cargo

The shipped `fix` and `sweep` profiles run `cargo test --quiet`. A repository that is not Rust says what its
check is in one file at its root, and the file names the command and the files that command reads as a pair:

```json
{ "version": 1, "verify": "python3 -m unittest -q", "protect": ["test_app.py"] }
```

```json
{ "version": 1, "verify": "node --test", "protect": ["app.test.js", "package.json"] }
```

`hades init --recipe` writes an example. `hades doctor` prints `recipe: built-in cargo` when a tree carries
none, and the command it read when it does. The pair is deliberate: a recipe that changed the check without
saying what the check reads would let a run swap `cargo test` for `true` and still look governed. `--verify`
overrides the command for one run and cannot remove the protection the recipe declared.

### A vision endpoint that is not this machine

The image tool starts a local Qwen3-VL GGUF by default. A host without those model files, or without a user
systemd manager, can point it at any OpenAI-compatible endpoint instead:

```json
{
  "vision": {
    "url": "https://vision.example/v1/chat/completions",
    "model": "qwen3-vl-8b",
    "key_env": "VISION_API_KEY"
  }
}
```

`key_env` names the variable, never the value. `HADES_VISION_URL` and `HADES_VISION_MODEL_NAME` override the
file for one process. `hades config` prints which of the two is configured.

A local vision server is asked `/props` before it is reused, so a port answered by another model is
restarted rather than trusted. A server that does not answer `/props` is accepted, because an older build
would otherwise be refused for no safety gain; `HADES_VISION_STRICT=1` makes that case a refusal instead,
which is the setting to use when the port is shared with anything else.

---

## Packs

The base install carries a shelf, not the packs. `hades pack menu` prints what exists, `hades pack find
<words>` narrows it by id, title, summary or keyword, and an archive is installed with the same inspect →
plan → apply path a pack exported from another home uses. The index ships inside the binary, so the shelf is
readable offline and without a network call.

The point is the order of operations: check what exists before writing a skill of your own. A pack carries
its manifest, licence and declared capabilities, and may not declare a guardrail, a role or a cage rule,
because a download that could set those would be a download that could disable its own limits. Installing is
a person's command with a printed plan, not something a model does on its own.

Sharing is one command. `hades pack export <skill>` resolves the skill wherever it lives (installed,
proposed, or built in), takes the summary and the keywords from its `SKILL.md`, writes
`<id>-<version>.hadepack` in the working directory, and prints a menu row ready to paste into a shelf.
`hades pack export --dir ./my-skill` does the same for a folder that was never installed, so a skill written
outside Hades can be shared without seeding a state first.

`hades pack add <id>` fetches the archive named by the menu, hashes the bytes that arrived, and refuses when
the digest is not the one the menu published, before the manifest is parsed. Without `--yes` it prints the
plan and writes nothing; with it, the plan is applied and the pack is active.

A pack that arrives as a file is added the same way: `hades pack add ./thing.hadepack --digest <sha256>
--yes`. The digest is optional, and when it is given the file must hash to it; when it is not, the file's own
digest is printed so it can be compared with what the sender said. `hades pack menu` reads the copy this
build ships unless this machine has cached a live one, and says how old a cached shelf is; `--refresh` reads
it again. `HADES_PACKS_URL` points the
shelf at a mirror, and `HADES_PACKS_ALLOW_PRIVATE=1` is what lets that mirror be a local address.

The catalog is generated from a packs repository (`tools/build.py` there writes `MENU.md` and `INDEX.json`
from the pack directories) and copied into `docs/packs/` at release. That shelf publishes three skills and
the Discord connector. A connector row is the one kind that ships code: it names its platform triple, installs
through the connector path with a digest per file, and `pack add` requires `--yes` for it where a skill is
planned and applied.

A pack's licence is its own: this runtime is MIT, and that does not reach a pack installed into it. A
commercial pack is a pack whose manifest says so (`license` and `derivatives: forbidden`), and its bytes
belong on a shelf that is not public. A row may name an absolute `https://` archive URL, so a licensed shelf
can live anywhere; a public menu stays public content.

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

Current state: **1095 passing, 0 failing, 13 ignored** across the workspace. Twelve of the ignored are the live browser suite and pass with a browser and a display; the thirteenth waits on a local model.

The ignored tests start a real Chromium and exercise live browser behavior. Run them on a machine with a
browser installed:

```sh
cargo test -p harness --lib browser::tests -- --ignored --test-threads=1
```

Acceptance scripts live in `rust/acceptance/`. `scripts/cross-env.sh` runs the checks a Cargo-shaped tree
cannot: a Python project and a Node project through their own recipes, a browser named by `HADES_CHROME`, and a
vision endpoint in the config.

---

## Limits

These are current limits, not aspirations hidden in a roadmap:

- Linux only. Landlock and systemd units are part of the design.
- Telegram is the only messaging transport.
- The network is open or closed per role. There is no per-domain egress proxy.
- Harness rollback restores the files named in a generation manifest. It does not roll back the model, the sessions or the state directory.
- Part II of the 0.0.3 plan (satellite transport, generation swap in service, the evolve loop) is not shipped.
- The `preferences.md` prompt block and `taste adopt` are not shipped. A CLI answer can only teach a root when the session log supplies it; a TUI answer always knows its root.
- Taste adapts a recommendation and can stop asking a choice. It does not infer a preference the person never chose.
- The browser needs an external Chromium binary. Vision is optional and needs local GGUF model files.
- Chromium binds a Unix socket under its `TMPDIR`, so the state directory has to leave room for it: past 61
  characters of `TMPDIR`, the bind fails inside Chromium and it aborts. `hades` refuses such a state with
  the numbers instead of launching. The browser's own stderr is kept at `<state>/browser/chromium.log`.
- `scripts/cross-check.sh` checks the workspace for `aarch64-unknown-linux-gnu` without root: it stages the
  cross gcc under a prefix and points `CC_<target>` at it. `scripts/arm64-build.sh` goes further and builds
  both ARM64 release artifacts against staged sysroots, then runs them under `qemu-aarch64` (also staged
  without root). The release workflow runs the same checks on the downloaded bytes; a machine that cannot
  stage qemu reports a skip, and CI passes `--require` so a skip is a failure there.
- The live browser suite is `cargo test -p harness --lib browser:: -- --ignored` (add `DISPLAY=:0` for the
  headed case). Twelve tests; all twelve pass on this machine.
- `eval` can act outside the textual safety floor. It is an explicit escape hatch.
- The floor reads text. `--policy strict` is a deny list over the same text, so an obfuscated command
  (`r''m -rf`, `$(printf rm)`) is not caught by either. The cage is the bound the kernel enforces.
- Crash recovery reports an interrupted run and does not replay it. The marker is in `<state>/inflight/`.
- Fetches share a pool of libcurl handles, loaded at run time, so a run keeps its connections and TLS
  sessions: 0.53 ms per fetch on a loopback fixture and 19.5 ms against a page on the internet, where a
  process per fetch costs 9.2 ms and 67.5 ms. `libcurl.so.4` is optional; without it, or with
  `HADES_FETCH_POOL=off`, the tool falls back to one `curl` process per fetch and behaves identically. Both
  paths disable proxies, including the environment variables libcurl would otherwise read, because a proxy
  would take the connection somewhere other than the address that was checked; the `curl` process is also
  started with `-q` first, so a `~/.curlrc` cannot set one either. libcurl before 7.85 gets the
  legacy protocol bitmask instead of the string option, so Ubuntu 22.04 keeps the pool rather than silently
  dropping to the slow path; `hades doctor` prints which transport is live and which protocol options it
  uses. The numbers and the guarantees both paths keep are recorded in the 0.1.x
  release notes.
- Unix-socket connects are refused by Landlock only on ABI 9 (Linux 7.1). This is a property of the
  kernel, not of the cage: on an ABI 8 kernel the same protection is `--cage strict`, which narrows the
  paths a caged program may reach. `hades doctor` prints the ABI and names the flag.

---

## Documentation

- [`HADES.md`](HADES.md): the design rule, the architecture and the long-form reasoning.
- [`RELEASES.md`](RELEASES.md): what each version added, newest first.
- [`rust/README.md`](rust/README.md): the crate map and porting notes.
- [`NOTICE`](NOTICE): lineage and upstream attribution.
- [`LICENSE`](LICENSE): MIT.

---

## License

MIT. Copyright (c) 2026 Guideboard Labs. Portions copyright (c) 2025 Nous Research, derived from Hermes Agent.
See [`NOTICE`](NOTICE) for details.
