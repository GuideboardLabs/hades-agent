# rust/

The Rust side of Hades, built as a translation of the Python spec rather than a redesign. One crate per
subsystem, each with the Python test suite as its oracle: the port is done when those behaviours pass here
unchanged.

## Layout

| Crate | Ports | Oracle | State |
|---|---|---|---|
| `session-tree` | `hades/session/tree.py` | `tests/test_hades_session_tree.py` (26 tests) | ported, 23 tests, green |
| `mneme` | `hades/vault.py` | `tests/test_hades_vault.py` (18 tests) | ported, 27 tests, green |
| `governance` | `hades/curator.py` | `tests/test_hades_curator.py` (21 tests) | ported, 22 tests, green |
| `harness` | nothing: it is designed | its own oracles, `tests/{turn,cli,cage,model,graph,daemon,resume,shadow,init,mcp,httpd,patch,service}_oracle.rs` | the turn, the map, the cage, the model, the graph, the daemon, the home, MCP, HTTP, the patch tool, the edit answer, the harness files, the tune loop, the judge, the plan and the units: 532 tests, green |

The first three are ports. `harness` is not: it is the engine, written in Rust from the design doc, because
writing it in Python first would leak dicts and duck typing into types meant to be strict.

`mneme/STORAGE.md` is the C7 decision that H4a built: SQLite is the record, markdown is the export. H4b
wires the Memory seam over it (`harness/src/memory.rs`): a turn recalls one budgeted section before it
acts and records one durable note after it passes; `hades run` recalls once per walk and files at accept,
and the daemon uses the same seam.

Two seams and nothing else in each crate, because varying parameters are values rather than seams.
Everything else is a type, and a rule that can become a type does. Governance is the exception that
proves the rule: it has one seam of its own, the timestamp, because the store and the day arrive already
injected inside the vault it holds.

The Python suite grew twice while porting, because a port is a second reading of the spec and it finds
the rules nothing pins: `fenced` and the recall guard in the vault (15 to 18), and two audit findings
that had never been raised in a test, plus the stale-detail truncation (19 to 21). No behaviour changed.

**The one-writer rule is a type.** In Python, "only the curator writes memory" is a docstring backed by a
test that scans the package for the write calls. Here the move into an area takes a `WritePermit` whose
only constructor is `Vault::permit`, so a write cannot happen without one and every asker names its
holder in one place. The limit is stated in the module: Rust cannot stop a caller asking for a permit,
only a write with none.

Both ports were also diffed against the Python directly, over the same operation sequence, with `diff -r`
and a sha256 per file: four files for the vault, five for the vault plus governance, all identical, the
generated index and a curated note included. The parity run also records that the Python's own audit, pointed
at the Rust-built tree, reports the same counts.

## Toolchain

Install stable Rust with rustup; no custom `CARGO_HOME` or target directory is required.

    rustup toolchain install stable
    cargo --version

## Running it

    cd rust
    cargo fmt --all
    cargo test
    cargo clippy --all-targets -- -D warnings

Cargo writes to `rust/target/`, which is gitignored. The upstream Python reference tree is not part of
this checkout; the Rust tests are self-contained.

## Porting order

Substrate first, then the engine. The substrate is translated because it has a Python oracle: session
tree, then vault, then governance. The engine (the loop, profiles, the router, the approval broker, the
repo map, the evaluator) is **designed** in Rust from the design doc, not translated, because writing it in
Python first would leak dicts and duck typing into types meant to be strict.

## The engine, as far as it goes

One turn of the loop, and the seams it runs on: the model, the tool, the check, the judge, the approval
broker, the context builder. A turn appends six entries to the session log: the request, the answer (a program or an edit), the
result, and the check recorded as a run rather than as a special case.

Three decisions worth knowing before reading the code:

- **Policy runs before the model, not after.** An action that is going to be refused should not have spent a
  model call to be refused. What the gate cannot see yet is the contents of the program, because that is a
  classifier and there is not one; until there is, the policy decides on the capability.
- **The model is a seam, so the loop is drivable without one.** `hades turn --program '<program>'` stands a
  person where the model goes: same turn, same policy, same log, same check. It is not a stub, it is the
  seam being used, and it is why the loop could be finished and proven before any model was wired in.
- **Nothing says more than it knows.** A check that could not run is `could_not_run` and not a pass, a
  check that its deadline killed is `timed_out` and not a failure, a judge that was asked and did not
  answer records `unjudged` with the reason rather than a verdict, and a fault carries its class so an
  infrastructure failure is never charged to the model.

### The question that outlives the process

The design's headline gap is an approval broker that dies with the process, so the run resumes with the
question gone. Here a question is a record in `approvals.jsonl`: it is written before anyone answers it, it
carries the session it was raised in, and it is answered with who decided and how long the decision lasts.

- `Scope::Once` answers the question; `Scope::Session` answers that action for that session, which is what
  stops a person being asked the same question every turn. A person asked the same question every turn stops
  reading it, and that is the failure the scope exists to prevent.
- An answer is not silently revisable: answering a question twice is refused rather than applied, so the
  record stays a record.
- A standing `deny` refuses the next turn without asking again. A standing answer speaks only where the
  policy asks: a policy that allows does not need a person, and a policy that denies is not open to appeal
  by someone answering a question that was never asked.
- One implementation of those rules (`Ledger`) and two places to keep it (in memory, on disk), because two
  copies of a governance rule drift apart and the drift is silent.

The honest limits are in the module: append-only JSONL, a single writer assumed, no lock and no `fsync`, so
a machine that loses power can lose the last line. What it does guarantee is what the design asked for, that
a separate process sees every question that was recorded.

### The map of the repo

A model asked to change a repository it has never seen guesses at the shape of it, and the guess costs a
turn. So the map is built from the tree: which files mention which other files, weighted ten times when the
mention is an import, ranked by a pagerank over that graph, and each file listed with its declarations and
their line numbers so the model can ask for lines rather than whole files.

- A file the goal names is sorted first, and everything below it is the graph's order. The map is read from
  the top and the budget stops it partway, so the file the caller asked about is the last thing that should
  fall off the page.
- It reads source, prose and configuration by extension, and skips build directories, hidden files, binaries
  and anything over 256 KiB. A repository is mostly bytes that are none of a map's business.
- It takes at most half the turn's budget. A map left to fill the budget does fill it, and a model given
  nothing but a map of the repository has not been told what to do to it.
- The map is a contiguous run from the top of the ranking: it stops at the first file that does not fit
  rather than skipping it and listing smaller files below, which would read as the whole list.

**What it costs**, measured: a 13k-file, 123 MB tree takes about two seconds in release, nearly all of it
reading and scanning text. Three fixes that looked right were measured and did not pay, and the numbers are
in the module so they are not tried again. The answer is to not scan the same bytes twice: a map cached by
file size and modification time, which belongs with the daemon.

### The cage

The tool runs a program a model wrote, so `Cage` is what stops that program from reading your API keys. It is
on by default and the log says what it allowed.

The mechanism is Landlock, and the choice was measured rather than assumed: on the machine measured, unprivileged user
namespaces are refused (`unshare -n` is EPERM, `bwrap` fails with "setting up uid map: Permission denied")
because `kernel.apparmor_restrict_unprivileged_userns` is 1, so containers and `bwrap` are not available to
an unprivileged harness. Landlock confines a process from the inside, with no root, no sysctl and no
namespace.

- Deny by default, for the filesystem and for TCP. Read and execute under a short list of system
  directories, read and write under the task's root and the state directory's `tmp`, and no TCP at all.
- The program cannot lift it, and neither can anything it starts: the confinement belongs to the process and
  is inherited. `restrict_self` also requires `no_new_privs`, which the cage sets, so a set-user-id binary
  inside the cage gains nothing.
- A cage that cannot be built stops the run. The program does not get the caller's rights because the cage
  had a bad day.
- What it is not, said rather than implied: it does not hide anything (a denied path exists and the program
  gets `EACCES`, not `ENOENT`), it does not confine UDP or raw sockets, and it does not make `/proc` lie.

The tests are pairs: the same program, the same tree, once caged and once not. Caged, `cat ~/.aws/credentials`
says "Permission denied"; without the cage the same program prints the key out of it. That pair is the evidence, because a refusal on its own could be a broken path.

One consequence worth knowing: a toolchain installed under your home directory is not reachable from inside
the cage, which is why the check runs uncaged. The cage is for what the model wrote.

### The model

`hades models` lists the ways this build can reach one; `turn --model <preset>` uses one. The build
ships the cloud presets it can actually reach (`deepseek-v4.1-flash`, `kimi-k2.7-code`) and no local
ones, because a llama.cpp server on port 8110 is a fact about a machine, not about Hades. A local model
is declared in the config's `endpoints` map, and then behaves like any other name:

```json
"endpoints": {
  "local-9b": {
    "url": "http://127.0.0.1:8110/v1/chat/completions",
    "model": "local-9b",
    "context_tokens": 8192,
    "thinking": false
  }
}
```

`hades turn --model local-9b`, the gateway and a connector all resolve it the same way: the surface's
own provider first, then this map, then a shipped preset.

A **provider** is the broader shape, and the one to use when a provider offers more than one model:
one base URL and one key, with a list of models behind them.

```json
"providers": {
  "groq": {
    "base_url": "https://api.groq.com/openai/v1",
    "key_env": "GROQ_API_KEY",
    "models": ["llama-3.3-70b", "qwen-2.5-72b"],
    "context_length": 131072
  }
}
```

`--model groq/qwen-2.5-72b` is explicit; a bare `qwen-2.5-72b` resolves when exactly one provider lists
it. Three transports are implemented: `chat_completions`, `anthropic_messages` and `responses`. A plain
call works in any of them; the tool loop needs chat completions and faults by name otherwise, because a
tool schema translated blind is worse than a sentence saying it is not supported yet.

`fallback_providers` is a list of model names tried in order when the chosen one fails before producing
anything. The chain is built once, every name is validated with the config, and the error from a chain
that runs out names each provider and its own fault.

The turn is one-shot: the model is asked once, the answer is applied, the check runs. A model that needs
three attempts needs a caller that runs three turns, which is the next section.

Three things real calls taught, each now a test:

- **A thinking model returns an empty `content`.** Ask one for a one-line program and it can think until
  the token budget is gone: the reasoning field is full, `content` is empty, `finish_reason: length`. The
  fault says that, and names `--thinking off`, which is what a local endpoint asks for.
- **The reasoning field has two names.** llama.cpp's template writes `reasoning_content`; the cloud
  endpoint writes `reasoning`. Reading only the first made the client blame the endpoint for a cause the
  endpoint had already stated in a field nobody read.
- **A pinned model version dies with a 410.** `--model deepseek-v4-flash` reached the endpoint and came
  back `deepseek-v4-flash:0731 was retired on September 25, 2026`. The presets carry unversioned aliases
  now, and the endpoint's real model list is in the preset table rather than in a guess.

### A judge, and a plan before the program

Two things the profiles advertised and the engine did not do are now real. Both are deliberately smaller
than they sound.

**The judge is a second call, not a gate.** When a profile has `judge: true`, `assemble` builds an
`Endpoint` from the same client and the turn attaches it; the walker carries it through `Parts` so `hades run`
and the daemon are judged too. It reads the goal, the program, the output and the check's state, and answers
one JSON object with a verdict and a reason. A fault, a malformed answer or a verdict without a reason is
`unjudged` with the reason, never a failed turn, and the check still decides acceptance. The receipt carries
the verdict:

```
judge:   met: The program wrote 'hello' into greet.txt in the working directory and printed its contents,
         and the check passed.
```

It is the same endpoint with a different prompt, so it is a second opinion and not a second model. A
`judge.model` setting is a later decision; the README will not call it one until it exists. The gateway's own
chat loop is not a profile execution (no `Seams`, no session log), so a chat is never judged; only profile
turns are.

**A plan is an entry, not a paragraph.** `plan_first` used to be a field nothing read. Now the turn asks for
a plan before it asks for a program, records it as `Kind::Plan`, and refuses to run if the plan is empty:
that is `Unwritten`, so the attempt edge retries while the bounds hold and no program runs on a plan that
was not written. One plan per task, because a retry after a failed check already has it; a reused session
whose goal changed is asked again, so a plan never becomes another task's instructions; a planner that
keeps returning an empty plan is asked once per attempt, because no plan landed. The plan survives a cut:
`Session::view` keeps it, `Learned` reads it, and the prompt carries it as `the plan you wrote`, so a
mid-sweep compaction does not hide the standing instruction. There is no review step and the doc no longer
claims one; the plan is evidence a person can read, `hades attach` shows its text, and `hades run` prints its
size:

```
plan:    258 chars
```

The live log is the proof of ordering: `user`, `plan`, `tool_call`, `tool_result`, the check, the checkpoint.

### The harness is a file tree

The text around the model is as much of a parameter as the model is, so it is not a constant. `hades harness`
reports what a run would load and where each part came from; with no argument it lists every directory under
`$HADES_HOME/harness`:

```
profile: fix
dir:     ~/.hades/harness/fix
system:  ~/.hades/harness/fix/system.md (1204 bytes)
guidance: ~/.hades/harness/fix/tool_guidance.md (302 bytes)
effective: 1506 bytes
```

Two files, and only two. `system.md` replaces the built-in prompt; `tool_guidance.md` is appended under its
own heading. An empty file is refused by name, over 32 KiB is refused, and an explicit `--harness` path that
is not a directory is an error rather than a silent fallback. The fallback is the built-in prompt, and it is
what every run used before this existed.

What is deliberately **not** a file: the floor, the policy, the approval broker and the check. A harness that
can edit its own guardrails is a governance hole, and `hades harness` has no `guardrails.md` to offer.

### Tuning it: propose, probe, compose

`hades tune` is the loop Harness² describes, with the check replacing the behavioral contrast and the
governed floor left outside the editable set. It runs the goal under the base harness for a reference, asks a
model for two candidates (one invariant: planning, tool use, context, recovery; one domain: the task's own
knowledge), writes them as harness directories, probes each under the same check in a state of its own, and
keeps the best by pass first and cost second. `--candidate <dir>` replaces the editor entirely, so the same
command is a manual A/B.

The editor never sees the check. The transcript it reads has every verify program replaced by
`[the check ran; its program is withheld]`, the check text redacted wherever it appears, and a candidate that
reproduces the check is refused. That is the anti-gaming guard, not a nicety: an editor that can read
`grep -q 42 out.txt` writes a prompt that produces `42`, not one that solves the task.

A live run on one goal, `deepseek-v4.1-flash`, base prompt deliberately thin:

| variant | check | answers | model ms | answer bytes |
|---|---|---|---|---|
| base | passed | 1 | 514 | 68 |
| invariant | passed | 1 | 892 | 144 |
| domain | passed | 1 | 472 | 73 |

`domain` was adopted on cost. A second run with `--adopt` against the same candidate took base 720 ms and
candidate 367 ms, copied the winner into the base directory, and the next `hades harness` reported the 461-byte
system file and its 190-byte guidance. Every tune writes `EXPERIENCE.md` under
`~/.hades/harness/library/<stamp>-<slug>/` with the table, the candidates, the adoption and the editor's
lesson, so a failed candidate is evidence that the base is still the best harness rather than a lost run.

What this is not: a self-modifying agent. Nothing is adopted without `--adopt`, the check is never editable,
and the probe runs the same binary a person runs.

### The graph

A loop runs one cycle until a condition the model invents at runtime. A graph declares the control flow, so
the path taken when something goes wrong is a property of the program rather than of the model's
improvisation. The distinction is not aesthetic: a loop asks the model to invent a recovery while it is
already confused, which is why a loop demos well and a graph is what you want at three in the morning.

`hades run <goal> --model <preset>` walks it. The nodes are the units of work this harness already had, the
conditions are the outcomes a turn already returned, and the graph adds only the two things that were
missing: where each outcome goes, and how many times the whole thing may try.

```
attempt --Passed-----> accept --Always--> done
   |
   +----Failed-------> attempt          (while the bounds hold)
   +----Exhausted----> gave_up          (the bounds are a condition, not a detour in the walk)
   +----CouldNotRun--> gave_up          (no check: not evidence either way)
   +----TimedOut-----> gave_up (2)      (killed before a verdict; a no-verdict exit, not a failure)
   +----Faulted------> gave_up          (a wrong endpoint does not fix itself)
   +----Refused------> gave_up          (a denial is not a question: no answer changes a policy)
   +----Asked--------> hand --Always--> awaiting
```

The graph is data, which is what makes it testable, and the test that matters is
`every_condition_an_attempt_can_produce_has_a_declared_edge`: an undeclared condition is a state nobody
thought about, which is exactly what a loop discovers at three in the morning. Every edge also carries the
reason it exists, and a test asserts none of them is blank, because an edge with no reason is a line nobody
can delete. A walk records one checkpoint per node entered, with the edge that led there, and one more when
it stops, which carries the reason the walk stopped.

**The return path.** An accepted result becomes a constraint for what comes next; a failed attempt becomes a
`Try` that the next attempt is told about, program and check output both, capped and truncated. This is the
half that makes a retry worth having, and it is measurable: in a real run the model's context grew 141, 271,
449 tokens across the three attempts, which is the failed attempt reaching the next prompt rather than a
claim that it does.

**A loop is still right inside a node.** Attempts repeat `Seams::turn`, and `Bounds` (attempts, minutes)
decides when they stop. What the graph adds is not "no loops": it is that the number of them, what each one
tells the next, and where a person has to be asked are all declared in one place.

Two things the real runs taught, and neither came from the model:

- **The instruction was ambiguous about what an answer is.** The local 3B answered with a script and the
  words "save as stats.sh", then appended `chmod +x stats.sh` to a program that never wrote the file. Its
  reading was "my answer is the file's contents"; the harness's is "your answer is a program that runs".
  The instruction now says the program is run and not saved, and a test holds the clause.
- **A check can encode an ambiguity the goal never resolves.** "How many regular files are in this
  directory" stopped being answerable once the script existed inside it. Three attempts could not rescue
  that, and the failure looked like the model's. A retry loop is only worth what the check says: with the
  goal and the check agreeing, the same task passed on the first attempt.

Deliberately absent still: a unified-diff parser. The checkpointer and the daemon
that holds the seams across many turns are built, the repo map's size-and-mtime cache is in place, and the
`patch` tool edits by anchored replacement rather than by rewriting a file, and a governed run can answer
with an edit as well as with a program: the same rule, planned across every file before anything is
written, logged as `tool: patch`. A refused edit is `unwritten`, which is its own condition and its own
retry edge, because a check of a tree nothing touched is not evidence.

### The scoreboard

Rules answer what a goal sounds like; they cannot answer how that kind of work has actually gone. Every
finished walk appends one row to `<state>/scoreboard.jsonl` (profile, passed, attempts, millis, at), and
`hades scoreboard` turns the rows into runs, pass rate and mean time per profile.

The router consults it only when no rule matched. It abstains until a profile has three finished runs, it
ranks pass rate before time, and the reason it records names the evidence ("3 of 3 runs, 900 ms mean")
rather than just the winner. A named `--profile` always wins. Failures are recorded too: a scoreboard of
passes only makes every profile look perfect.

## Where the work stands

As of this session: 552 Rust tests green across the workspace, `cargo fmt --check` clean,
`clippy --workspace --all-targets -- -D warnings` clean. The Python spec is untouched and still green on its
three files; its Rust mirrors are the three `port_oracle.rs` suites. The three substrate ports are
byte-identical to the Python on the same operations, and the parity receipts record it.

Built and driven from the binary: `hades init`, the turn, the ranked repo map, the Landlock cage, shadow
snapshots and `undo`, the durable approval broker, the daemon, the Telegram gateway, standing goal files and
retry, memory on a SQLite record, the hard floor and the activity ledger, one governed MCP client (stdio
JSON-RPC, one process per configured server, an allowlist checked before the ledger says allow), and the
HTTP surface (`hades http`: health, status, sessions and session tail, approvals, jobs, activity, submit,
answer, stop), and the `patch` tool (exact, unique anchors applied in order, one atomic write, floor parity
with `write_file`), and the harness as a file tree (`hades harness`, `--harness` on every run) with the
propose-probe-compose loop over it (`hades tune`, with the check withheld from the editor and adoption behind
`--adopt`). The live gateway has answered real messages and real approvals; the MCP path has run a real
server process from both the CLI and the chat tool; the HTTP surface has taken a live submit (202 with the
daemon's run number), answered a real question as `by: "http"`, and reported a dead daemon as
`daemon: false` on health and 503 on status.

What is left is the Discord transport (deliberately last). Delivery landed with the engine: `package.sh`
builds the release tarball, `install.sh` installs it in one command, `hades service install` renders the two
systemd user units, and `acceptance/clean-home.sh` proves the whole path in an empty home with no model and no
network. The vault's daily notes for 2026-10-17 and 2026-10-18 carry the rules that must not be lost.
