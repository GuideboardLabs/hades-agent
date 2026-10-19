# Hades Agent

A governed agent runtime in Rust. One binary, four crates, its own state at `~/.hades`.

Hades runs a conversation, a scheduled job, or a governed piece of work with a check and an attempt budget.
It is built for the work that has to survive being trusted: long-horizon changes where the point is not a
plausible answer but a receipt you can read afterwards.

## Build and install

```sh
cd rust
cargo build --release
./install.sh --bin target/release/hades --home ~/.hades --root ~/.hades/work
hades init
hades doctor
```

`install.sh` renders two user units, `hades-daemon.service` and `hades-gateway.service`, and does not start
them unless you ask. The gateway needs `HADES_TELEGRAM_TOKEN` and an `allowed_users` list in
`~/.hades/config.json`.

## What it does

- **Chat with tools.** Shell, files, search, memory, skills, MCP, delegation, scheduled jobs.
- **A native browser, not a wrapper.** One Rust Chrome DevTools Protocol client over Chromium's
  `--remote-debugging-pipe`, with no Python or Playwright in the tree. It exposes an observe-act loop:
  `observe` returns a compact accessibility tree with refs, `click`, `type` and `press` return a fresh observation, and
  `wait` waits for a condition with a deadline. It handles tabs and popups, same-origin and cross-origin
  (OOPIF) frames, dialogs, downloads, uploads, network history and blocking, cookies, storage, and isolated
  browser contexts. It launches the patched Chromium binary when one is present and falls back to stock
  Chrome. `HADES_CHROME` names a binary explicitly; `HADES_PROXY` (credentials are answered through Fetch),
  `HADES_TIMEZONE`, `HADES_LOCALE`, `HADES_HEADED`, and `HADES_HUMANIZE` are the other launch controls.
- **Typed delegation on a shared budget.** A delegated task names a JSON Schema; the worker must return an
  object that fits it, and the parent gets `result.json` instead of a paragraph. Every worker draws from the
  conversation's single model-call allowance, so a message cannot become an unbounded tree.
- **Recursive spawn, bounded.** `spawn` creates one child graph with its own session and a result schema.
  The child returns to its parent, depth caps at two, and the same allowance pays for the whole tree. A
  failed child still leaves `result.json`, so the parent has a typed failure rather than a silence.
- **Governed runs.** `hades run "<goal>"` builds a program, runs a check, retries within bounds, and stops
  with a decision you can read. A plan is written before a program when the profile asks for one. The run
  graph declares every recovery edge; each node is a loop.
- **Bounded branches.** `--branch n` runs up to four candidate programs from the same tree, each in its own
  session, rolls a failed candidate back, and keeps the first one the check accepts. The attempts budget is
  shared, so width costs model calls, not a second budget. This is speculation inside one agent, not a DAG
  of agents. When no candidate passes, the failure says which one got furthest, using the check's optional
  `HADES-CHECK` progress line; partial progress orders failures and never substitutes for a strict pass.
- **Five profiles, five permission shapes.** `survey` (read-only recon), `fix` (one narrow change),
  `sweep` (planned change across files), `verify` (read-only check on a different model), `record` (memory
  only). The role is enforced in the tool schema and again in `execute`; it is not a prompt.
- **A cage, a floor, and a ledger.** Landlock denies reads outside the workspace and the state scratch; a
  deterministic floor refuses writes to protected paths and destructive commands; every allow and refusal
  is a ledger line.
- **A small, general skill set.** Eleven built-in procedures ship in the binary and are seeded into
  `~/.hades/skills/builtin/` by `hades init`: plan a change, repair a failing check, review a diff,
  research with sources, write for humans, safety before acting, keep memory useful, create a skill, land a
  change, build an MCP server, and run and verify. The last four close the staples every other harness
  ships. Nothing personal is inherited. `hades skills seed` writes any that are missing after an upgrade.
- **Evidence, with the verifier withheld.** A pass alone does not write a proposal. `hades distill` drafts a
  runbook, `--run` proves it in a fresh copy of the session's first snapshot, and only then is a proposal
  written under `~/.hades/skills/proposed/`. The proposal records the check's fingerprint, never the check
  command, because a skill that leaks its verifier turns every later run into a game against the same test.
- **Distill and replay.** `hades explain <session>` writes a compact turn map. `hades distill <session>`
  drafts a runbook from a verified session, gates it for absolute paths, secrets and verifier leakage, then
  `--run` executes it in a fresh copy of the session's first snapshot. Only a runbook that passes that
  independent replay becomes a proposal. `hades replay <runbook>` runs the same proof on demand; the check
  is deliberately not stored, so you pass it with `--check`.
- **A holdout before adoption.** `hades tune --holdout <cmd>` runs a protected check the candidate never
  saw. A candidate that regresses the holdout is never adopted, even if it wins the visible check.
- **Sessions as trees.** Append-only JSONL logs with fork, so a branch of history is a first-class thing.
- **Jobs that stop.** `until: YYYY-MM-DD` is enforced by the scheduler, not left as prose for a model.

## Commands

```
hades init                 write the home, config and vault skeleton
hades chat                 a conversation, with the tool cage
hades run <goal>           a governed walk: program, check, attempts, --branch n
hades explain <session>    a compact map of turns: action, state, tokens, decisive
hades distill <session>    draft a runbook from a verified run; --run proves it in a fresh tree
hades replay <runbook>     execute a runbook in a fresh root against --check <cmd>
hades tune <goal>          probe candidate harnesses; --holdout guards adoption
hades job <spec>           write a scheduled job
hades jobs                 list them
hades profiles             the five shapes and their parameters
hades route <goal>         which profile the rules pick, and why
hades scoreboard           what finished runs say per profile
hades skills list|seed|promote|reject
hades session list|show <id>
hades fork <session>       copy a chain to an entry, leaving the source alone
hades acp                  an ACP server on stdio for an editor
hades doctor [--services on] what this install is; writes nothing
hades config               the config a headless process reads
hades audit                the governance audit over the record
```

`hades help` is the full list, including the flags.

## What it is not

- **Not a wrapper over other agents.** It does not shell out to Codex or Claude Code for the hard part. The
  coding loop is here.
- **Not a general assistant catalog.** One governed path per capability is the design; breadth is not.
- **No Discord.** Telegram is the transport that exists.
- **No per-domain egress proxy.** The network is open or closed for a role, not filtered by host.
- **No automatic rollback command yet.** An adoption keeps `versions/<seconds>/`; restoring is a copy.

## Layout

`rust/` is the product: `harness` (CLI, chat, engine, gateway, cage, floor), `session-tree` (logs and fork),
`mneme` (vault), `governance` (audit). The upstream Hermes Agent Python tree used as the porting
reference is not part of this checkout; the Rust runtime is self-contained.
It is reference only. Nothing under `rust/` builds or runs it. See `NOTICE` for the lineage and `HADES.md`
for the design rule that keeps the two systems apart.

## Tests

`cd rust && cargo test --workspace` runs the lot, including oracles that drive the real binary and the real
CLI. Acceptance lives in `rust/acceptance/`.
