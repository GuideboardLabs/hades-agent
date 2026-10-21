# Hades

A distilled harness, built as Hermes' successor and tested on its own machine.

Hades is a conscious reduction of the Hermes Agent harness: its tool loop, skills, session shape, gateway
and memory seam, rebuilt in Rust. The terminal coding-agent pattern it follows was made familiar by Claude
Code and the Codex CLI; Hades wraps neither. It builds on Linux Landlock for isolation and SQLite FTS5 for
memory. Its lineage and the upstream MIT notice are in `NOTICE`.

**Nothing here touches the running Hermes install.** Not its vault, not its config, not its keys, not its
sessions, not its cache. The relationship is *cloning with tweaks*, never *moving over*: this tree started as a
copy and the Rust harness is new work, but at run time the two systems share no path and no state.

That is a rule with a guard behind it, not a convention to remember.

## Own everything

| Thing | Where | Never |
|---|---|---|
| state (sessions, approvals, snapshots, map cache) | `~/.hades` | `~/.hermes/**` |
| Mneme, the memory record (SQLite, FTS5 recall) | `~/.hades/mneme.db` | `~/Documents/ObsidianVault/**` |
| Mneme's markdown export (six areas, `charter.md`, `TAXONOMY.md`, generated `INDEX.md`) | `~/.hades/mneme` | `~/Documents/ObsidianVault/**` |
| keys | `~/.hades/.env` or the environment | `~/.hermes/.env` |
| short-lived files | `~/.hades/tmp` | the caller's `TMPDIR` when it belongs to the other system |

The record is the database; the folder is its export, written on capture and on filing and rebuilt with
`hades mneme export`. Recall reads `mneme.db`, never the tree, so hand edits reach memory only through
`hades mneme import`. `charter.md` is configuration and `INDEX.md` is a generated view.

A fresh clone plus an empty `~/.hades` is a working install. That is the test: if Hades needs something from
the first machine to start, it has already moved over.

## First run

`hades init` writes the home the rest of the system reads. It is the first artifact a second machine needs:

- `~/.hades/config.json`: the provider, the model preset, the working root and the vault. A daemon or a
  bot is started with no arguments, so it reads this file; a CLI flag is an override on top of it. Secrets
  never go in it: keys and bot tokens stay in `~/.hades/.env`. An optional `profile_models` object names a
  preset per profile; a profile with no entry runs the config's `model`.
- `~/.hades/mneme`: the six areas, `charter.md` from the interview (who the agent is, what the work is
  for, which topics matter), `TAXONOMY.md`, and a generated `INDEX.md`.
- `--non-interactive` is the installer's path: flags and defaults, no prompts.

## The vault

The clone is generic and empty. `hades init` creates six folders and one `START HERE.md` marker in each;
the markers carry `status: marker`, are seeded signage rather than claims, and are left out of the index
and the audit counts.

| Folder | What belongs there |
|---|---|
| `00 Inbox` | Captures. A queue, not memory. |
| `10 Outbox` | Work that leaves the machine, and messages for other agents. Nothing reads it yet. |
| `20 Research` | One finding per note, with a topic and provenance. |
| `30 Projects` | One folder per project, naming its topic and its research. |
| `40 Products` | A project that stopped moving: `active`, `closed`, `inactive` or `frozen`, with `from_project`. |
| `50 Topics` | The spine: `<category>/<topic>/<subtopic>/index.md`. |

`TAXONOMY.md` lists the 26 static categories, taken from Foxforge's topic types: sports, technical,
medical, finance, history, general, science, math, politics, current_events, underground, business, law,
education, travel, animal_care, food, gaming, books, real_estate, automotive, parenting, tv_shows, movies,
music, art. The category never grows; the tree under a topic does. A topic is deliberately general
(`technical/ai-inference`); a narrower thing is a subtopic (`.../kv-cache`), not a second topic for the
same ethos.

The rules are enforced by both stores, so a file-backed vault and the SQLite record refuse the same note:

- every research note, project and product names a `topic`;
- a topic names one of the 26 categories;
- a durable project names at least one research note in `research:`;
- a product names the project it came from in `from_project:`;
- a product's status is `active`, `closed`, `inactive` or `frozen`.

The path is part of the record. `50 Topics/technical/ai-inference/kv-cache/index.md` is the subtopic
`technical/ai-inference/kv-cache` in `technical`, with `technical/ai-inference` as its parent. A research
note under `20 Research/technical/ai-inference/` takes that folder as its topic when it does not state one.
The frontmatter is the graph; the folders are how a person navigates it, and the generated `INDEX.md` is
the view that cannot drift from either.

An existing 0.0.9 vault is upgraded when the record is opened: schema v3 adds the CHECK constraints the
v1 rebuild dropped, files a legacy flat topic under `general`, and each research or project note takes
its first topic tag as its primary topic. Run `hades mneme upgrade --retire-old` to write the new tree
(`40 Products` and `50 Topics`) and move the stale `40 Topics` folder to `mneme-archive/`.

The vault is built with `hades note topic`, `hades note research`, `hades note project` and
`hades note product`; each goes through `Vault`, so the command cannot author a note the store would
refuse. The top-level `hades topic`, `hades project` and `hades product` names remain aliases;
`hades research` is the research engine, not the note verb. A `research:` reference may
be the full record path, the slug below `20 Research`, or a bare trailing name, and a bare name two notes
share is refused. The audit reports a reference that resolves to nothing as `dangling_research` or
`dangling_project` rather than aborting.

`hades init` refuses a vault that already exists. `hades audit` reads the record the config names;
`hades audit --refresh` imports the config's vault, refreshes `INDEX.md` and audits what is on disk.
`hades audit --vault <dir>` imports a markdown tree into a throwaway record and audits that, which is how
a hand-edited tree is checked without touching the record. A fresh home audits clean: six empty areas, a
charter and an index.

## Research

`hades research <question>` is one evidence-first run. The planner returns bounded subquestions; each
worker sees one subquestion and one source, never another worker's conversation. The run owns the state
under `<state>/research/<run-id>/`: `run.json`, `assignments.jsonl`, `evidence.jsonl`, `failures.jsonl`,
`sources/`, `report.md` and `report.json`.

A source is admitted only when it has an HTTP or `library://` URL, a body above the snippet floor and a
hash. Every sourced claim cites a byte range into the stored canonical text, and `hades research check
<run-id>` re-reads that file and compares the bytes. The check exits 0 for green, 1 for a bad quote or a
missing citation, and 2 when a source cannot be read. It proves provenance, not truth: a real quote can
still be wrapped in a claim that overgeneralizes.

The relevance gate is structural. Each claim is compared with its subquestion on distinctive terms; a
claim that matches only the source's general subject is kept, marked `off-question` and reported under
its own heading. `--strict` turns any off-question claim into a failed check. A contradiction quote goes
through the same byte check and is printed in `## Disputed`; a later support for the same normalized
claim revises it in place and keeps the old text in `history`. One retry is allowed when every draft
from a source fails the byte check, with the mismatch as the hint.

`hades research bench <questions.json>` runs a question set and reports published, claims, verified
quotes, off-question count, sources, wall time, tokens and key-term coverage. Coverage is a reading aid,
not a truth score. `hades research file <run-id> --topic <topic>` files a published, strict-green report
as a Mneme research note.

## The offline library

`hades library` is a local shelf. Every install starts empty: no seed corpus, no sample paper, no
placeholder entry. A document exists only after `hades library add <file-or-directory>` imports it.

PDF text comes from `pdftotext`; EPUB is unpacked with a member cap and a path-traversal check; LaTeX is
converted to Markdown by a deterministic reader; Markdown, text and reStructuredText are stored as text.
The original is kept and hashed, the readable text is chunked into SQLite with FTS5, and `hades library
verify` re-reads every original and recomputes its hash.

The shelf is per state. A process started with `--state X` cannot read `--state Y`'s books, and the
agent's `library` tool uses the same resolved shelf. `hades research <question> --offline` plans,
searches the shelf, fetches a library document and runs the same byte check; the report says `mode
"offline library"`.

The canonical text is stripped HTML, not the raw response; the parser version is stored with each source.
Script, style, noscript, template, SVG and iframe bodies are removed before tags. If the local SearXNG
result set does not match two query terms, search falls back to DuckDuckGo's HTML endpoint through Jina
Reader. Both the parser and the fallback are dependencies the run records, not silent assumptions.

## Installing

A release tarball is one binary, one installer and one README. On a machine with an empty home:

```
tar xzf hades-<version>-x86_64-unknown-linux-gnu.tar.gz
cd hades-<version>-x86_64-unknown-linux-gnu
./install.sh
```

The installer copies `~/.local/bin/hades`, runs `hades init --non-interactive` once, writes
`~/.config/systemd/user/hades-daemon.service` and `hades-gateway.service`, and, when a user systemd manager
answers, reloads it and enables the daemon. It never runs `sudo` and never touches `~/.hermes`. A second run
replaces the binary and the units and leaves an existing home alone; `--prefix`, `--home`, `--root`,
`--no-init`, `--no-service`, `--no-start` and `--with-gateway` are the ways out.

The daemon is started by default. The gateway is not: it needs `HADES_TELEGRAM_TOKEN` in `~/.hades/.env`
and a `gateway.telegram` section with non-empty `allowed_users` and `verify`. Once those exist,
`install.sh --with-gateway` (or one `systemctl --user enable --now hades-gateway.service`) starts it. A
gateway that cannot start is capped at five tries in a minute and its first line is in
`journalctl --user -u hades-gateway`.

`hades service install` is the unit renderer on its own: it writes both files from a `--prefix` and a
`--home`, and a unit it did not generate is refused unless `--force` replaces it. The first line of each
generated unit says so. `hades --version` names the build.

The proof is `rust/acceptance/clean-home.sh`: it packages the binary, extracts the tarball, installs into an
empty home with a recording `systemctl`, starts and shuts the daemon down, and checks the units with
`systemd-analyze verify`. It makes no model call and uses no network.

## The guard

`hades::boundary` refuses the live paths by name, for every command that takes a directory:

```
$ hades audit --vault ~/Documents/ObsidianVault
refusing the live Hermes vault under your home directory: it is the live Hermes vault (the second brain, fences and all).
Hades has its own vault and state, and pointing it here is the one thing the separation exists to prevent
```

Exit code 2, and no fallback. Paths are compared by parts after `.` and `..` are resolved, so
`ObsidianVault-notes` is somebody else's directory and is not refused; a symlink or a relative path does not
walk around the rule.

The vault here also carries fences of its own. A fenced path is reported by the audit and does **not** fail it:
the directory is the vault, the notes outside the fences are the corpus, and the corpus is something you walk.
`governance::report::AuditReport` keeps them in two lists for exactly that reason.

## What a turn remembers

A turn reads Mneme before it acts and writes to it after it establishes something. The read is one
section, `what Mneme remembers`: notes matched by the goal's words through FTS5, unioned with the
charter's topics, ordered topic first and cut to at most a tenth of the profile's window. An empty read
is a fact with a reason (`NoMatch`, `BudgetTooSmall`), not an error.

A note can be corrected without losing it. `remember` takes `supersedes`, the path of the note the new
one replaces; the old note is marked `superseded_by` and the new one `supersedes`, both as vault relative
paths. Recall still shows the old note, but after every current note and marked `[history, replaced by
X]`, and the budget is spent on what is true now before it is spent on what used to be. Two notes with
the same words would otherwise compete, and the older, more specific one tends to win. Nothing is
deleted: the record keeps the correction and what it corrected.

The write is one durable note per accepted turn: the goal, the constraints it established, the check and
its output, the session and run. It goes through the curator, so a note without a summary, a topic or a
provenance is refused and the turn says `recorded none (<why>)`. A refused run, a question, a failed
check or a failed write never changes the turn's exit code: the session log keeps what happened, and the
record is what the next turn needs.

`--memory off` runs a turn without either half. A machine with no `hades init` has no record, so a turn
runs as it did before and says `not recorded (no record is open)`.

The graph has the same memory. `hades run` recalls once before the walk, puts the section in front of
every attempt, and files one note at the accept terminal, so a retry does not pay the search twice and a
refused run files nothing. The daemon uses the same seam, so a submitted run is not a second-class one.

## The gateway

`hades gateway` attaches Telegram to the daemon that already owns the sessions. A plain message is a
conversation: the gateway runs its own agent loop over the model, the workspace and the same tools the
CLI has, and answers in prose. `/run <goal>` is the one door to the governed harness, where a program,
a check and attempts apply. `/status` is quick, and a policy question arrives as a message with Allow
and Deny buttons. A plain message never chooses a root, a profile, a policy or a model; those come from
the config the daemon reads, because a stranger's sentence must not set them.

Stopping is three verbs, because one word for three promises hides one of them. `/stop` asks the work in
this chat to end (`/stop all` asks every run); it never ends the daemon. `/cancel <job>` removes a
standing job and the pending retry beside it, so it stops being due; it does not undo what the job already
ran. `/shutdown` stops every run and ends the daemon, and the gateway exits with it. A stop is a request:
the walk honors it before its next attempt, or after a model call and before the program runs. A model
call or a shell command already in flight finishes first, so a stop can be preceded by a side effect.
The same three verbs exist at the CLI as `hades stop`, `hades cancel` and `hades shutdown`.

Two settings live in `config.json` under `gateway.telegram`: `allowed_users`, a list of numeric Telegram
ids, and `policy`, `guarded` by default. An absent section or an empty list refuses to start. The token is
`HADES_TELEGRAM_TOKEN` in `~/.hades/.env` or the environment, never in the JSON, and it is never printed.

A goal is session state, not a schedule. `hades goal set <text> --chat <id> --check <command>` records one
under `<state>/goals`, and the gateway's heartbeat continues it in that chat when the session has no live
run and its last entry is older than `gateway.heartbeat` seconds (default 900). The agent claims done by
calling the goal tool, and the goal's recorded check decides; a failing check leaves the goal open with the
verdict recorded. Each continuation is bounded by the goal's `--budget` of model calls, and a goal that
spends it pauses with `budget` as the reason. `hades goal list` reads the files and calls no model, so it
answers on a machine whose endpoint is down. `hades goal pause <id>` stops the heartbeat until a person
resumes it, and `hades goal clear <id>` closes it.

The poller stays free while a run happens: each run gets its own daemon socket and thread, so the
approval that run is blocked on can reach a person and come back on the poller's connection. The update
offset is one file under the state directory, so a restart replays at most one batch.

Discord is not built: its transport decision (public webhook versus a WebSocket client) is still open.
The gateway runs against live Telegram; the oracle still drives the same loop through a fake transport
and a scripted daemon socket, so the shape is tested without the network.

## The model seam streams

A model call used to be one POST and one body. It is now a stream when somebody is watching:
`Harness::run` takes an optional feed, `http::stream` reads the endpoint's SSE frames, and each
`delta.reasoning` or `delta.content` reaches the caller as it is written. A caller with no watcher
passes `None` and gets the single-POST path unchanged, so the CLI and every benchmark still
measure exactly what they measured before.

The daemon relays the pieces as `{"progress": "reasoning|content", "delta": "..."}` frames on the
submit socket, written by the same thread that follows the log, so frames and entries cannot
interleave. The gateway edits them into the live card under the same one-second throttle: the
thinking as it happens, the answer as it is written, and then the program block once the turn
turns it into one. An endpoint that ignores `stream` produces no deltas and the turn falls back to
one POST.

## Working in parallel

A message that splits into parts does not have to be worked one part at a time. The `delegate` tool takes
one to four tasks and runs each in its own thread with its own conversation, the same tools and the same
workspace, then hands the results back to the conversation that asked. A worker cannot delegate again: the
tree is one level deep by construction.

Every task carries a result contract. A task is `{"goal": "...", "result_schema": {...}}`; the worker is told
to return one JSON object that satisfies the schema, and the parent receives `result.json` with `status`,
`result` and `raw`. A missing required field or a wrong type is `invalid`, not a prose report the parent has
to interpret. A bare string task is still accepted and gets the default `{"report": "<prose>"}` envelope.

A worker can also spawn a child of its own. `spawn` is the recursive node: one goal, one result schema, one
child graph that runs to completion and returns to the worker that called it. A child cannot return to the
root and cannot outlive the depth cap (two). The parent receives `result.json`, so a failed grandchild is a
typed failure at the parent, not a missing message.

The model calls are one shared allowance, not one budget per worker. The person's loop and every worker draw
from the same counter, so four workers cannot turn one message into an unbounded tree. When the allowance is
too small to cover a delegation, the tool refuses before spawning anything.

Workers are invisible as machinery. Their tool calls do not stream into the card; the card shows that a
worker started and what it was asked, and the person sees the reports arrive as part of the answer.

## The hard floor

Five kinds of action are not the agent's to take on its own: a permanent delete, money moving, a
credential or permission change, new security access, and a message that reaches a person. The list is
checked in code, not in a prompt, because the source behind it measured a forbidden action attempted in
17.4% of runs at maximum reasoning effort.

`harness/src/floor.rs` is the check. A chat has nobody to ask, so `agent::execute` returns the refusal to
the model as the tool's result: it names the category, says a person decides, and says not to look for
another route to the same end. A governed turn has a person, so `Turn::once` raises a broker question
once the program exists, with the category as the action. That question is asked even under
`--policy open`, and a standing Allow for the shell does not answer it. A session-scoped Allow for the
category does, because a person said yes to that category in that session.

It reads the tool name and its arguments as text: the command line of `shell`, the path of `write_file`,
and the control named to `browser`. It cannot see a page whose button says "Continue", a command built at
run time out of pieces, or a delete done by a binary nobody listed. It is a floor, not a sandbox.

## Work that runs itself

A job is a standing goal file under `<state>/jobs`, one file per job, and the file is the record. The body
is the goal; the frontmatter holds the machine fields (`id`, `chat`, `schedule`, `mode`, `last`) and the
statement of intent: what it reads, what the answer is, when it is done, when to stop, and what it must
never do. Every scheduler tick reads the file again, so a correction is the next run's behaviour rather
than a change nobody notices until a restart. `hades jobs` prints the files with those fields; the `cron`
tool creates, lists and removes them, and its reply names the file so a person knows what to edit.

Two schedules are enough: `every 30m` or `every 6h`, measured from the last run, and `daily 07:30` in the
machine's local time. The old single list, `<state>/cron.json`, is imported into files once when the
gateway starts and then renamed, so a job created before this shape existed is not stranded.

A file that will not parse does not run, and both `hades jobs` and the `cron` tool name it with the
reason: a person who edits a job is the one who needs to know the edit took it out. The scheduler re-reads
the one file before stamping `last`, so an edit made between the load and the run is kept.

Every job has a mode. `read` is the default, and its schema contains no tool that changes anything: no
shell, no `write_file`, no `remember`, no browser, no scheduling. `act` is explicit, and the hard floor
still stands in front of it. The mode belongs to the job and to the `World` the loop runs against, so a
delegated worker inherits it, and a job file written before the field existed reads as `read`: an old job
loses tools it may have used and never gains one it was not given. `hades tools --mode read|act` prints
what a surface is handed, which is the rule made readable rather than promised.

The last-run stamp is written before the work starts. A crash therefore delays a job instead of repeating
it every twenty seconds, and a restart does not rerun the morning's work. A job made from a chat reports
to that chat and nowhere else; a probe with no chat refuses to create one rather than run silently. If the
stamp cannot be written the job does not run: an unstamped window would otherwise fire again on the next
tick.

A failure is tried again, but not for ever: three tries for one occurrence, thirty seconds and then two
minutes apart, and the chat hears once, after the last one. Every try appends a line to
`<state>/attempts/<id>.jsonl` with the occurrence key (`<id>@<stamp>`), the attempt number, start and end,
the outcome and the error; `<state>/retry/<id>.json` holds the pending try, so a restart resumes it instead
of losing it. A retry is told it is attempt N of three for the same key, with the previous failure in front
of it, because a model told only "try again" repeats the step that already ran. The key names the
occurrence; it does not make an `act` job's side effects repeatable. If the first try already sent
something, the second cannot know. Keep an unattended job's steps idempotent.

## Verified replay

A skill proposal used to be a note about a run. It captured the winning program and, with it, the check
command that judged the run. That is a quiet leak: a procedure that carries its verifier teaches every
later run to satisfy the sentence rather than the goal. Proposals now record a check fingerprint, never the
command, and the redaction is the same one `<state>/tuning` already uses when it withholds the verify
program.

A pass is evidence, not a procedure. `hades explain <session>` writes the compact map: each turn's tool,
its files, its check state, its tokens, and whether it changed the outcome. `hades distill <session>` gives
that map to a planner, gets back a runbook with four headings, and gates the text before it is written:
no absolute paths, no secrets, no verifier, no state that only exists in the author's tree. With `--run`,
the runbook is executed in a fresh copy of the session's first snapshot, under the same check. A runbook
that fails its own replay is not a proposal; a runbook that passes becomes one, and `hades replay` can
prove it again later in any clean root.

The check is deliberately not stored. A stored check is a check the next author can tune against, and the
fingerprint is enough to notice that the verifier changed. `--check` is required at replay time.

Checks are still binary, but they may report partial progress on one line:

```
HADES-CHECK {"gate": false, "passed": 2, "total": 5}
```

`gate` is the strict pass; the counts only order failures. A branch that passes nothing keeps the report
of the candidate that got furthest, and `tune` uses the same ordering, but no amount of partial progress
is adopted as a pass. The number is there to aim the next repair, not to lower the bar.

`hades tune --holdout <cmd>` runs a second check the candidate never saw. Adoption requires the visible
check to pass and the holdout not to regress. Without it, a tuning loop that improves one benchmark and
calls it general improvement looks exactly like a real gain.

## The failure corpus

A failed run is a task, not a story. When a walk gives up, the harness pins the tree it started from, the
goal, the check, the policy, the model and the whole ceiling it ran under, and files it as a case. A case
is identified by profile, goal, check and tree digest, so recording the same failure again appends a
`seen` line rather than a second tree. It is one store, with one explicit cap and `hades case forget`.

`hades eval` puts the corpus back on the bench. The check runs first in a throwaway copy: a green start is
`stale` and costs no model call, a truncated or missing tree is `broken`, and a red start replays the goal
under the case's own policy and bounds. `--against <harness-dir>` runs the current harness and a candidate
over every case and names each `holds`, `fixed`, `regression`, `still failing` or `base drifted`, keeping
the two arms separate so a candidate's pass cannot rewrite the base's history. Exit 0 is no regression, 1
is a regression, 2 is a corpus that cannot be replayed, which is not evidence either way.
`tune --adopt` runs the same comparison for the selected candidate and refuses an improvement that breaks
a pinned case; `--no-regress` is the one way past it and the receipt says the corpus was skipped.

A security case carries a second oracle. `--attack <cmd>` succeeds while the vulnerability is present, so
a pass is `check passed AND attack failed`; a green check beside a working exploit is red, and an attack
that cannot run is a fault rather than a blocked one. The attack command, and any script it names, are
frozen by the ruler with the check, and the case carries the attack so the gate refuses a candidate that
revives it.

## The ledger

Every tool call a chat or a scheduled job decides writes one line to `<state>/activity.jsonl`: when
(`at`, epoch seconds), who (`actor`: `chat:<id>` or `job:<id>`), which layer ruled (`mode`, `floor`, or
`surface` when nothing objected), what was attempted (`shell: rm -f keep.txt`), the decision (`allow`,
`refuse`, `ask`, or `stop`), the reason, and the path the action named when it named one. `hades activity
[--limit n]` prints the last twenty, oldest first, with the local clock.

A stop writes a line of its own with the decision `stop`, because "who stopped what" is the same question
as "who allowed what" and should be answered from the same file. The daemon records the stops it carries
out (a run, every run, or a shutdown) with the caller's own name; cancelling a job records its file as the
artifact. A stop that found nothing in flight is still recorded, and says so.

The line is written before the tool runs, and a write that fails goes to stderr without stopping the
action: the ledger is evidence, not a gate. It is the flat record across the surfaces that had none. A
governed turn already writes its decisions into its session log, where the whole conversation is, and is
not duplicated here. A line says what was decided, not what came back; the session log or the chat is
where the result lives.

## The web, without a wrapper

The browser is a Rust module that speaks Chrome DevTools Protocol over the pipe Chromium opens with
`--remote-debugging-pipe`: JSON messages separated by a NUL byte on file descriptors 3 and 4. There is no
Python, Playwright or Node in the tree. The module launches a Chromium binary directly, with a persistent
profile under `<state>/browser/profile`; when the binary is the patched CloakBrowser build it also passes the
fingerprint flags, so the stealth profile is kept without the Python launcher.

The tool is an observe-act loop, not a primitive list. `observe` returns the title, URL, headings, readable text and
the interactive elements with stable refs from the accessibility tree. `click`, `type` and `press` return a fresh observation after the
action. `wait` waits for text, a URL fragment or a selector with a deadline. `screenshot` is for when text is
not enough, and `eval` is the escape hatch. A stale ref is not a dead end: the next call reports the current
page so the model can recover.

Tabs are real targets. `tabs` lists them, `new_tab` opens one, `tab` switches, and `close_tab` closes one. A
click that opens a popup reports it; the model can switch to it. Frames are real contexts, including
cross-origin OOPIFs: `frames` lists the tree, `frame` enters one by index or id, and `observe`, `click` and
`type` then route through that frame's own CDP session. `frame main` returns to the top document. Isolated
browser contexts are available through `contexts`, `new_context` and `close_context`.

Dialogs do not hang the loop. A `Page.javascriptDialogOpening` event is answered immediately and reported in
the next observation; `HADES_DIALOG_ACCEPT=1` accepts instead of dismissing. Downloads go to
`<state>/browser/downloads` and `downloads` lists them. `upload` attaches a local file to a file input
through `DOM.setFileInputFiles`, and an intercepted file chooser tells the model which ref to use.

`network` lists recent requests, `network_body` returns a response body, and `block` and `unblock` manage
`Network.setBlockedURLs`. `cookies`, `cookie_set`, `cookie_delete` and `cookie_clear` use the Network cookie
API. `storage` reads localStorage or sessionStorage, `storage_set` writes a key, and `storage_clear` clears
one or both.

`HADES_CHROME` names a different binary. With no override, the module takes the newest
`~/.cloakbrowser/chromium-*/chrome`, then `/usr/bin/google-chrome`, then the snap. A stock binary still gives
the CDP surface; only the fingerprint patches are missing. `HADES_PROXY` and `HADES_PROXY_BYPASS` pass a
proxy to Chromium; credentials in the URL are answered through `Fetch.authRequired`, so an authenticated
proxy works without a helper. `HADES_TIMEZONE` and `HADES_LOCALE` override emulation; `HADES_WEBRTC_IP`
passes the patched binary's WebRTC IP flag; `HADES_HEADED=1` drops headless mode; `HADES_HUMANIZE=1` uses a
curved mouse path and per-character text entry for ref-based clicks and typing. Humanized typing streams the
whole string; long input is sent in small chunks instead of being truncated. `observe` accepts `text_limit`
and `item_limit`, and `eval` and `network_body` accept `full` for a 100,000-character return.

## Seeing

The chat model reads text, so a picture is a second call to a second model: Qwen3-VL 8B with its
projector, served by the Vulkan llama.cpp build on loopback, started on first use and left running.
The `image` tool takes a path and a question, shrinks the picture to 768 px wide first, and returns the
answer as text, which is why the chat model can talk about a screenshot without ever seeing pixels.

A photo sent to the bot is downloaded with `getFile`, saved under `<state>/inbox`, described the same
way, and handed to the conversation as what the picture shows. The token never appears in a log or in
the JSON; the file endpoints are the only place it travels.

## What the Rust tree is

`rust/` is the harness: `session-tree` (append-only logs), `vault` (notes and fences), `governance` (audit and
curation), `harness` (turn, graph, daemon, CLI, and the failure corpus in `cases.rs` and `eval.rs`). The
Python under `agent/`, `hermes_state*.py` and the rest is
the copy this tree started from, kept as reference for behaviour that was ported. The executable spec for the
substrate is the Python; the engine is the Rust.

Tests run inside `rust/target/scratch`, never the machine's temp directory, because under the live install
`TMPDIR` points into that install's cache and a test that writes there has crossed the same boundary the guard
protects.

## Not yet

Hades installs and is supervised now, but it is still a project under construction rather than the main
place. It does not read or write WEFT: its vault is its own, and the fences it carries are its own. The
Telegram gateway runs against a real bot, and Discord is the one transport still not built. When Hades
becomes the main place, that is a decision with a migration in it, not something that happens by pointing
one flag at a path.

## Residual limits

These are decisions, not omissions. Each has a reason and the seam it would change.

- **Verifier independence is a preset away.** No shipped profile pins a model: a profile inherits the
  run's model and may name a preset instead. `verify` is a different role, but it is the same voice until
  a second usable preset exists. The separation is data, not a second string that may not resolve.
- **The TUI is a chat surface.** `cases`, `eval` and `skills verify` are CLI verbs with receipts. A palette
  entry that cannot show the receipt would be a weaker second surface, so the TUI links to `/run` and the
  terminal instead.
- **Telegram is the only transport.** Discord is deferred; `gateway/` is transport-shaped, so it can land
  without moving the core.
- **The network is open or closed per role.** There is no per-domain egress proxy. Research fetches
  through the same gate as any other fetch, and a search fallback is a network dependency the run
  records rather than a policy change.
- **The provenance check is not a truth check.** A quote can verify while the claim around it adds a
  condition the source never stated. The structural relevance gate catches a claim about the wrong
  subject and the extractor prompt asks qualifiers to survive, but neither can prove a correctly quoted
  sentence is representative, current or complete.
- **A social URL is still refused on its own.** The gate requires a linked artifact, and artifact-link
  extraction is not built; until then an X post cannot be a source by itself.
- **Hades writes only its own vault.** WEFT is read by its owner, not by Hades; `mneme export` is the
  one-way path.
- **`skills report` says which numbers are unmeasured.** It does not estimate them.
