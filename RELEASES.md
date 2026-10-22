# Releases

What each version adds, newest first. The README describes what the product is; this file records
what changed, and which limits each release closed.

## 0.1.10

Six findings from the audit of the pack installer, all reproduced before being fixed, plus one the new
concurrency test found on its own.

- **Concurrent installs could swap archives under each other.** Every install staged to
  `archives/incoming.hadepack`, so two installs into one state shared one file: the second could replace the
  first's bytes between verification and parsing. Each install now stages to a name of its own opened with
  `create_new`, the staged file is hashed again after the parser has read it, and a difference is a refusal.
- **The catalog write had the same collision.** `.catalog-<pid>.tmp` is shared between installs in one process;
  one thread renamed the other's file away. It is now per-write and `create_new`. This one was not in the review;
  the test written for the first finding caught it.
- **`--digest` is honoured for shelf ids.** It was forwarded only for local files, so a pinned digest could be
  silently replaced by whatever the refreshed index named. A pin that disagrees with the shelf is refused before
  anything is fetched.
- **Namespaced ids install.** `author/tool` was a valid id and an invalid filename. Cache entries are named by
  digest.
- **A local import is measured before it is read**, through a bounded read rather than after the allocation.
- **The tool list comes from the tool registry.** `KNOWN_TOOLS` was a stale second copy that refused `browser`,
  `write_file`, `ask` and `spawn`; validation now asks the list the act surface and the ledger already use, and a
  test keeps them equal.
- **A failed `pack find --refresh` is disclosed**, in text and as an error in `--json`, instead of searching the
  built-in catalog and reporting no match.

1069 tests, 12 live browser tests, acceptance 31/31, aarch64 check green, cross-env 7/7.

## 0.1.9

- **The menu says when it was read.** A cached index now records its fetch time, and `pack menu` reports it
  ("read 2 hours ago", "9 days ago, older than a week"); the shipped copy says that instead of a date, because
  it is as old as the binary. A catalog that looks current when it is not is worse than one that admits it.
- **The shelf's URL is the real one**, `raw.githubusercontent.com/GuideboardLabs/hades-agent-packs`, and the
  menu shipped in this build is the one that repository publishes.
- **Nothing else changes behaviour.** `--refresh` still reads the live shelf, and a shelf that cannot be read
  is still named rather than silently replaced.

## 0.1.8

- **A pack that arrives as a file is added.** `hades pack add ./thing.hadepack --yes` installs without a
  shelf, which closes the sharing loop: one person exports, the other adds. `--digest <sha256>` checks the
  file against what the sender said and refuses a mismatch before the archive is read; without it the file's
  own digest is printed for the person to compare.
- **`pack menu` and `pack find` read the same shelf.** Both use the cached index when this machine has one and
  the copy this build ships otherwise; `--refresh` reads the live shelf, and a shelf that cannot be read is
  named rather than silently replaced. A listing command no longer opens a socket to say nothing new.
- **The plan is the same on both paths.** A file and a shelf row run through one installer, so the digest
  check, the preflight and the decisions cannot drift between them.

## 0.1.7

Export was the part that was not easy, and it is the part that makes a pack worth having.

- **A built-in skill exports.** Export resolved skills through the installed list only, so
  `skills/builtin/<name>` looked like no skill at all; the one most likely to be worth sharing was the one
  the command refused. It now resolves installed, proposed and built in, in that order.
- **A folder exports.** `hades pack export --dir ./my-skill` packs a folder that was never installed, which
  is the authoring case: a skill written anywhere can be shared without seeding a state directory first.
- **The flags are inferred.** The id defaults to the skill's name, the summary to its `SKILL.md`
  description, the keywords to its tags. `hades pack export my-skill` needs no other argument, writes
  `<id>-<version>.hadepack` in the working directory, and prints a menu row with the digest to paste into a
  shelf.
- **One extension.** Export, the shelf builder and the menu all say `.hadepack`.

## 0.1.6

- **`hades pack add <id>`.** One command from the shelf to a plan: the index is fetched, the archive named by
  the row is fetched as bytes, its sha256 is compared with the digest the menu published, and a mismatch is
  refused before the manifest is parsed. Without `--yes` nothing is written past the cache; with it the plan
  is applied and the pack is active.
- **A signed-shaped digest is now a check, not a record.** The archive path had no way to compare the digest
  to anything and the text fetch decoded bodies lossily, which would have corrupted exactly the bytes worth
  checking. `fetch::bytes` keeps the body intact and the add path compares before reading.
- **A row may name an absolute archive URL.** That is how a licensed shelf lives somewhere other than the
  public repository. `HADES_PACKS_URL` points at a mirror and `HADES_PACKS_ALLOW_PRIVATE=1` allows that
  mirror to be a local address; the default refuses both a plain-http base and a private address.
- **A pack's licence is its own.** The runtime is MIT and that does not reach installed content; a commercial
  pack declares `license` and `derivatives: forbidden`, and its bytes belong on a non-public shelf.

## 0.1.5

The base install carries a shelf, not a pack library.

- **`hades pack menu` and `hades pack find <words>`.** The catalog of skill, tool and test packs is embedded
  in the binary at build time, so a machine with no network can still answer "does a pack already do this?".
  `find` matches the id, title, summary and keywords, and every word of the query has to land. An empty
  result says so in words instead of printing nothing, because that is the case where writing a skill is the
  right move.
- **`pack.json` may carry `keywords`.** Optional, additive to format 1, so an older manifest still parses.
  It exists so the menu is generated from one file instead of a second list that drifts.
- **The shelf is data about packs, not permission to run one.** The menu only reads what exists; installing
  is still `pack inspect` → `plan` → `apply` with a reviewed decision file.

## 0.1.4

A review of 0.1.3 found three places where an existing guarantee stopped short of a path that could escape
it. Each was reproduced before it was changed.

- **The fallback fetch read `~/.curlrc`.** The `curl` process was started without `-q`, so curl loaded the
  user's config file, and that file can name a proxy, extra URLs or a relaxed TLS check. Clearing the
  environment does not clear it, so the slow path could be routed away from the address that was checked
  while the pooled path could not. `-q` is the first argument now. `tests/fetch_config.rs` writes a
  `.curlrc` naming a proxy, runs the subprocess path, and asserts the destination answers and the proxy sees
  nothing; with the argument removed the same test fails, which is how the defect was confirmed.
- **A model's shell outlived its timeout.** The shell tool wrapped the command in `timeout N sh -c …` and ran
  it without a wall-clock deadline. `timeout` ends what it launches, but the runner keeps reading the pipes a
  background child inherited, so a command could stall far past its stated limit. The shell now runs under
  the runner's own deadline: the process group is killed, the readers are cancelled, and the result says
  `[timed out after Ns]` rather than returning partial output as a finished run.
- **DNS came out of no budget.** The remaining time was read before resolution, so a slow lookup could hand
  the transfer the full allowance again and double the declared deadline. The clock is read after the
  lookup; a hop with nothing left is refused instead of started.
- **Strict vision identity.** `HADES_VISION_STRICT=1` refuses a local vision endpoint that answers `/health`
  but does not say which weights it loaded, instead of accepting it as the configured model. The default is
  unchanged, because an older server without `/props` would be refused for no safety gain.

## 0.1.3

A follow-up audit of 0.1.2 found one trust-boundary mismatch between the two fetch paths and three smaller
edges. Each was reproduced before it was changed.

- **The pooled fetch inherited proxy settings that the process path blocks.** The subprocess path clears
  every spelling of `HTTP_PROXY`, `HTTPS_PROXY` and `ALL_PROXY` before it runs, because a proxy would take
  the connection away from the address that was resolved and checked. libcurl reads the same variables from
  the runtime's environment, so the fast path could be re-routed while the slow path could not. Every pooled
  transfer now sets `CURLOPT_PROXY` to an empty string after its reset. The regression test points the
  variables at a listener and asserts that nothing arrives.
- **The pool disappeared on libcurl older than 7.85.** `CURLOPT_PROTOCOLS_STR` does not exist there, the
  configuration call failed, and the transfer fell back to the process path silently. The pool now uses the
  `CURLOPT_PROTOCOLS` bitmask on older libraries, which is the same HTTP/HTTPS-only restriction, and
  `hades doctor` prints the transport, the version and which form is in use.
- **Two local vision instances shared one unit name.** The unit is named after its port
  (`hades-vision-8123`), so a second process on another port cannot stop the first one's server. Before
  reusing a healthy endpoint the runtime asks `/props` which weights are loaded and restarts its own unit if
  the answer is another model.
- **Cancelled readers left threads behind.** The pipe readers blocked in `read`, so a process that escaped
  the group could keep them alive after the deadline. The descriptors are non-blocking and polled with a
  cancellation flag, so a timed-out run's readers end within one poll interval.

## 0.1.2

A re-audit of 0.1.1 found one regression and five smaller edges. Each was reproduced before it was changed.

- **Local vision cold start.** `systemd-run` exits 0 as soon as the unit exists, while the model is still
  loading. 0.1.1 treated any exit as a failed launch, so a first local vision call failed on a cold start.
  Exit 0 now moves on to watching the unit and `/health`; only a non-zero exit is an error, and a wait that
  runs out stops the unit by name. The test starts a launcher that exits 0 at once and a health endpoint that
  answers only on the third probe.
- **A timed-out run kept the shell's status.** A shell that exited 0 while a background child held the pipe
  produced `status: 0` with `timed_out: true`, and `passed()` said true. The status is normalised to -1 and
  `passed()` requires that the run did not time out.
- **The fetch pin was malformed for IPv6.** `CURLOPT_CONNECT_TO` entries are colon-separated, so an IPv6
  address has to be bracketed; without it a pinned fetch fails wherever IPv6 is preferred.
- **libcurl is initialised explicitly.** `curl_global_init` runs once through a `OnceLock` before any handle
  exists, which matters on distributions that still ship a libcurl from before 7.84, where the explicit call
  is what makes first use thread-safe.
- **A failed seal is fatal.** A runtime that cannot make itself non-dumpable no longer proceeds to accept
  model-executed commands; `HADES_ALLOW_PTRACE=1` is the explicit way past it. Help and version output still
  print, with the reason.
- **The pool comment was wrong.** `MAX_HANDLES` bounds idle retained handles, not concurrent transfers; the
  comment says so now.
- **Local vision port override.** `HADES_VISION_PORT` moves the local server off 8123, for a host where that
  port is taken or two models are wanted.

The `PR_SET_DUMPABLE` guarantee is stated more narrowly in the README and `HADES.md`: per process, reset by
`execve`.

## 0.1.1

This release answers an external audit of 0.1.0. Five defects were reproduced before they were changed; one
reported defect did not survive checking and is recorded as such.

- **A caged child could read the runtime's own environment.** Both cage modes grant `/proc`, and a cleared
  child environment says nothing about the parent's `/proc/<pid>/environ`. The runtime clears
  `PR_SET_DUMPABLE` at startup, which closes that read for the whole tree; `HADES_ALLOW_PTRACE=1` opts out.
- **A background child could hold a deadline open.** The shell exiting stopped the clock, then two pipe
  readers were joined with no timeout: `sh -c 'sleep 300 &'` under a 2 s deadline waited 300 s. Readers report
  on a channel, the deadline covers the streams, and the group is killed when it passes.
- **Fetching was serialized again by the pool.** One easy handle behind a mutex held across the transfer
  turned four research workers into a queue. Handles are a pool, leased and returned, with the transfer
  outside any lock: four 250 ms fetches finish in 0.26 s.
- **The vision launch was silent and misspelled.** `GGML_VK_DISABLE_HOST_VISIBLE_VIDMEM` is the variable
  llama.cpp reads; the build set a near-miss, discarded `systemd-run`'s stderr, and polled for four minutes
  after a launch that had already failed. It reports the exit and the unit's last lines in seconds now, and
  the image tool can use a configured OpenAI-compatible endpoint instead of a local model.
- **A screenshot could be written through a symlink.** The final component is refused when it is a link, a
  symlinked subdirectory is caught by resolving the parent, and the write uses `O_NOFOLLOW`.
- **Browser discovery** gained the distribution layouts and a `PATH` search; `hades doctor` names the binary
  it would launch.
- **The project recipe reached fewer commands than it should.** `run`, `turn` and `submit` read
  `.hades-project.json`; `context` and `tune` did not, so a Python tree was prepared for `cargo test`. All
  five read it now.
- **The browser's session lock is per state**, so two state directories no longer queue behind each other.
- `scripts/cross-env.sh` runs the checks a Cargo-shaped tree cannot: a Python project and a Node project
  through their own recipes, a browser named by `HADES_CHROME`, and a vision endpoint in the config.

The reported "unbounded model response" was already bounded: the body is capped at 8 MiB while it is read.

## 0.1.0

This release hardens the boundaries the earlier versions drew, and takes the last measured cost out of the
fetch path.

- **The cage reports what it can enforce.** Unix-socket rules apply on Landlock ABI 9 and the cage says so
  when the kernel is older; `--cage strict` narrows path grants on those kernels. The syscalls use `libc`
  numbers, so the cage compiles on ARM64.
- **Chromium keeps its sandbox.** `--no-sandbox` is gone from the default launch. Screenshots are confined
  to the state's `shots` directory, every browser path resolves under the browser directory, and one
  Chromium is held per state directory.
- **A failed browser launch explains itself.** Chromium's stderr goes to `<state>/browser/chromium.log`, and
  a failed first call reports its last line and the child's exit status. A state directory too long for
  Chromium's socket is refused with the number and the limit instead of aborting inside the browser.
- **Command output is bounded while it is read.** A `command` call caps output as it arrives and kills the
  process group on timeout, so a background child cannot outlive the call. The cage gets a writable
  `TMPDIR` of its own.
- **The HTTP surface has a connection bound**, and the context builder counts tool payloads and compacts
  completed turns rather than dropping messages.
- **The safety floor is named as a heuristic.** `--policy strict` is a deny list over the same text, and
  the docs say which one is a bound and which is a filter. Audit write gaps are reported, and an
  interrupted run is named after a crash.
- **Packs replace a case transactionally**, with caps that bind before the work starts.
- **A fetch no longer starts a process.** One libcurl handle, loaded at run time, serves the whole run:
  0.53 ms per fetch against 9.16 ms on loopback, 19.5 ms against 67.5 ms for a page on the internet. The
  process path remains as the fallback and every guarantee is tested on both.
- **The aarch64 check needs no root.** `scripts/cross-check.sh` stages the cross compiler in a cache
  directory and checks the workspace; CI runs the same check with the distribution package.

## 0.0.20

This release makes a pack fit into a home, and keeps a record of what it changed.

- **`hades pack apply` installs only the rows a person accepted.** The decision file names a kind,
  identity and one of `install`, `replace` or `skip`; a row outside the plan is refused. Skills land
  under `skills/proposed/` unless `--activate` names them active. A conflict may be replaced, and the
  old live path is moved to `packs/replaced/<pack>/<time>/` first. A skill installed as a proposal is
  not loaded as active work.
- **Every installed item lands in a catalog.** `packs/catalog.jsonl` records the pack, version, pack
  digest, item digest, the live digest at install time, the live path and the content path. The content
  is a copy under `packs/<id>/<version>/`; nothing is symlinked. A second apply of the same version is
  refused before it writes.
- **`hades pack uninstall` removes what the pack owns.** It removes a live copy only when its digest
  still matches, so a skill edited after install is kept and named in the receipt. A promoted skill is
  kept as active. A case is retired in its ledger rather than left as a broken directory. Shared
  content is removed only when no remaining row references it, and an uninstall event is appended.
- **`hades pack adapt` makes a local draft.** One model call reads the pack text, the local catalog
  brief and the license. It returns rewritten text, a change list and any unresolved conflicts; the
  result is shown as a diff and written only with `--accept`. Derivatives must be allowed. The draft
  carries `contract: TODO`, empty regression and holdout lists, so it cannot pass as evidence until a
  person fills them in.
- **`hades pack merge` composes a draft, never an overwrite.** The incoming skill and the installed one
  are both read, both contracts and the conflicts are recorded with provenance, and the result is a
  proposal. It is skill-only in this release; harness merge is refused by name.
- **The limits are explicit.** Atomicity is per item, not per pack: a failure after earlier rows have
  landed leaves those rows and names the failure. A pack is still unsigned; digest pinning and a refusal
  for unsigned packs are not shipped, and the reader says so.

## 0.0.19

This release gives a result a container that can leave the machine without losing its evidence.

- **A `.hadepack` is a deterministic tar.gz with a strict manifest.** `pack.json` names the pack, its
  version and license, the Hades range it needs, the roles and tools it asks for, and every skill,
  harness and case under `files/`. `SHA256SUMS` covers each file. The writer sets every directory to
  0755, fixes mtimes, owners and ordering, compresses with `gzip -n`, and records file modes, so the same
  inputs produce the same bytes. A reader rejects an unsafe path, a duplicate name, a symlink or special
  entry, an unknown key, a `code: true` capability, and an archive over the size caps before trusting it.
- **`hades pack export`** writes one. It takes `--skill`, `--profile` and `--case` selectors, records the
  current Hades range, and never reads the live Hermes home. A directory skill is copied as a directory;
  a flat skill becomes `files/skills/<name>/SKILL.md`; a case carries its pinned tree and `case.json`.
- **`hades pack inspect`** reads the manifest, every item, uncompressed bytes, archive hash, signature
  status and the network/vault warnings. It asks no model and changes no state.
- **`hades pack plan`** classifies each item against the live catalog as `new`, `duplicate`, `conflict`,
  `redundant` or `incompatible`. Duplicate and conflict use identity plus content digest; redundancy
  uses 5-gram Jaccard at 0.60 and prints the shared shingles so the heuristic is reviewable. Nothing is
  installed yet: applying a plan is 0.0.20.

## 0.0.18

This release closes the last research limits.

- **A social post can supply the paper it links to.** When a search hit is a social URL, Hades reads the post as raw
  markup, skips navigation and shortener links, resolves the first link that leaves the platform, fetches that
  artifact, and stores it as the source. The report shows the artifact URL with `via` the post; `run.json` and
  `report.json` carry both `url` and `artifact_url`. A post with no visible artifact is a named gate failure, never
  a source by itself, and the artifact fetch is charged to the same budget.
- **`--entailment` audits the quote against the claim.** For each verified claim, one capped model call labels the
  link `supported`, `partial` or `unsupported` and gives a short reason. Labels appear under the claim and in
  `entailments`; token use is on its own ledger line. This is advisory. The byte check still decides provenance.
- **`--workers 1..8` fetches a candidate batch in parallel.** Results are processed in candidate order, so
  `--workers 1` reproduces the serial run exactly. Each fetch still passes the same URL, private-address and
  deadline gates; the budget is checked before a batch starts.

## 0.0.17

This release closes the limits named at 0.0.16. The library reads more kinds of documents, and research
records a disagreement between sources instead of averaging it away.

### The library reads structure, not just bytes

- **A scanned PDF falls back to OCR.** Hades still tries `pdftotext` first. When the text layer is empty it
  rasterizes each page with `pdftoppm` at 300 dpi and runs `tesseract`; the page marker records `ocr=1`, and
  the entry carries the tag `ocr` in `library list`, `library show` and `library list --json`. Page and byte
  caps stop a runaway scan. `--ocr-lang` selects the language pack, and the scratch directory is removed on
  every exit path.
- **An EPUB is read in spine order.** The extractor follows `META-INF/container.xml` to the OPF package,
  then the manifest and spine, instead of trusting the zip listing. EPUB3 `nav` and EPUB2 `ncx` supply the
  table of contents; footnote asides stay as footnote text; image `alt` text is folded into the body.
- **LaTeX math becomes readable Markdown math.** `\[...\]`, `\(...\)`, `equation`, `align` and `gather`
  convert to display and inline math, and escaped dollars survive. This is still a reading view, not
  rendered math.

### Research can record a dispute

- **`--judge on|off`** (default on) compares claims from different sources on the same subquestion, in
  capped pairs. A contradictory pair is written to `## Cross-source disputes` in the report and to
  `disputes` in `run.json` and `report.json`. Both quotes are still byte-verified; the section records a
  conflict, not a verdict.
- **`--judge off`** keeps the old behavior. Judge token counts are recorded separately on the usage ledger,
  and older runs still load because every new field is serde-defaulted.
- **`hades research bench <questions.json> --grades <grades.json>`** compares key-term coverage with a human
  verdict. The bench prints `grades: N pass, N partial, N fail` and
  `coverage predicted N/M grades; this is a calibration, not truth`. A grade that names a question the
  bench does not have is a named error, not a silent skip.
- **The parent-log race test waits 30 seconds**, not 10, so a loaded full-suite run cannot turn endpoint
  latency into a false log-collision failure.

## 0.0.16

### The offline library

Hades can read a local shelf with no network.

- **Every install starts empty.** `hades library list` on a fresh state prints that the shelf is empty. There is no seed corpus, no sample paper, no placeholder entry. A document exists only after you add it.
- **`hades library add <file-or-directory>`** imports PDF, EPUB, LaTeX, Markdown, plain text and reStructuredText. Directories are walked. The original is kept on disk and the readable text is extracted into the shelf.
- **PDF text comes from `pdftotext`** (poppler), not a browser or a model. EPUB is unpacked with a member cap and refuses path traversal. LaTeX is converted to Markdown by a small deterministic reader, so a paper stored as source remains readable without a TeX installation.
- **The index is SQLite with FTS5.** `hades library search <words>` returns ranked passages; `hades library read <id>` prints the stored text; `hades library verify` re-reads every original and recomputes its hash.
- **`hades research <question> --offline`** plans, searches the shelf, fetches a library document and runs the same byte-verified quote check. The report records `mode "offline library"`.
- **The shelf is per state.** A process started with `--state X` cannot read `--state Y`'s books. The agent's `library` tool uses the same resolved shelf.
- **`--strict` upgrades an off-question claim from a warning to a failed check.** The normal check still exits 0 so an honest report with a flagged claim remains inspectable.

### Research depth

- **A structural relevance gate.** Each extracted claim is compared with its subquestion on distinctive terms. A claim that only matches the source's general subject is marked `off-question`, kept in the report under its own heading, and counted. It is never silently promoted into findings.
- **Fan-out is deduplicated and ranked.** Search hits are de-duplicated by normalized URL; primary documents are fetched before secondary pages, and social posts last.
- **Contradictions are first-class evidence.** An extractor may return a `contradicts` quote. The quote goes through the same byte check and appears in a `## Disputed` section instead of being averaged away.
- **A claim can be revised in place.** When a later source supports the same normalized claim with a new quote, the old text and support are kept in `history`; the report marks the claim as revised.
- **One retry on a bad quote.** If every draft from a source fails the byte check, the extractor is called once more with the mismatch as a hint. A second failure is recorded, not repeated.
- **`hades research bench <questions.json>`** runs a question set and reports published, claims, verified quotes, off-question count, sources, wall time, tokens and key-term coverage. Coverage is a reading aid, not a truth score.
- **`hades research file <run-id> --topic <category/topic>`** files a published, strict-green report as a Mneme research note with provenance and a `hades://research/<id>` source link.

The check proves provenance, not truth. The relevance gate catches a claim that is about the wrong subject, but it cannot prove that a correctly quoted sentence is representative, current or complete.

## 0.0.15

Research is an evidence-first run, not a chat that summarizes links.

- **`hades research <question>`** plans subquestions, searches, fetches, extracts and checks. Each worker gets one subquestion and one source; the run owns the state, so nothing depends on a growing conversation.
- **Every published claim carries a byte-verified quote.** `report.md` and `report.json` cite claims, and `hades research check <run-id>` re-reads the stored canonical text and compares the byte range. Exit 0 is green, 1 is a bad quote or missing citation, 2 is an unreadable source.
- **A run is a directory, not a transcript.** `run.json`, `assignments.jsonl`, `evidence.jsonl`, `failures.jsonl`, `sources/`, `report.md` and `report.json` live under `<state>/research/<run-id>/`. `hades research list` and `show` read it without asking a model.
- **The source gate refuses snippets and empty pages.** A source needs an HTTP URL, a body of at least 400 characters and a hash; a search snippet is never evidence.
- **`hades note research`** is now the vault-note verb. `hades note topic|project|product` are the same creation verbs, and the old top-level `topic`, `project` and `product` names remain aliases.
- **The HTML parser drops code before prose.** Script, style, noscript, template, SVG and iframe bodies are removed before tags, so a JavaScript-heavy page is not mistaken for a wall of evidence.
- **Search falls back when the local result set is blind.** If SearXNG cannot match two query terms, Hades reads DuckDuckGo's HTML results through Jina Reader rather than treating a single-token match as a source.

The check proves provenance, not truth. A quote can be real while the claim around it overgeneralizes; the report says so, and the extractor prompt now requires platform, driver, version and condition qualifiers to survive into the claim.

## 0.0.14

- **`config.json` can set a model per profile.** `profile_models` maps a shipped profile id to a preset; a profile with no entry inherits the config's `model`. An unknown id or preset is refused when the config is read, with the valid names in the message.
- **`hades route` prints the effective model**, including a `profile_models` override, so the choice is visible without starting a run.

## 0.0.13

- **Profiles can name a model, and inherit one when they do not.** `--model` still wins, then `profile_models.<id>` from `config.json`, then the profile's own setting, then the config's model. No shipped profile pins a model, so `verify` no longer asks for a preset this build does not ship.
- **The per-profile setting is validated when the config is read.** An unknown profile id or an unknown preset is refused by name, not silently ignored.
- **`hades profiles` and `hades route` say `inherit`** where a profile has no model of its own.

## 0.0.12

The limits left by 0.0.11 are closed, or written down where they are decisions.

- **An import is one transaction.** A failure after the first note rolls back every row it wrote, including the exported hash. The success path and the rollback path each have a test.
- **`hades audit --refresh`** imports the config's vault, refreshes `INDEX.md` and audits what is on disk. It is refused with `--vault`, where the import already happens on a throwaway record.
- **The clean-home acceptance script names six areas** and probes one creation verb, so the packaged install is checked against the vault the binary actually writes.
- **The residual limits have a ledger** in `HADES.md`: the TUI is a chat surface, Discord is deferred, the network is per role, WEFT stays read-only, and `skills report` does not estimate unmeasured numbers.

## 0.0.11

The vault can be built from the command line, and its links are resolved rather than declared.

- **Four creation verbs.** `hades topic <category> <name...> [--parent <topic>]`, `hades research <topic> <title>` (now `hades note research`; the top-level name is the engine), `hades project <name> [--topic <topic>] [--research <ref>...]`, and `hades product <name> --from <project>` write through `Vault`, so the CLI cannot author a note the store would refuse. Each refreshes `INDEX.md`.
- **A reference resolves to one note.** `research: [...]` and `from_project:` accept the full record path (`technical/ai-inference/kv-cache/kv-cache-at-32k`), the slug below its area (`kv-cache-at-32k`), or a bare trailing name (`kv-cache-at-32k`). A bare name two notes share is refused as ambiguous instead of guessing.
- **A dangling link is a finding, not a crash.** `hades audit` reports `dangling_research` and `dangling_project` with the missing name and the note that named it. `hades audit --vault` imports leniently for exactly this reason; `hades mneme import` stays strict.
- **An import reads the whole tree before it writes a row.** A tree that would leave an unresolvable reference is refused with zero rows written, including when the research note sorts after the project that names it.
- **Upgrading is a command.** `hades mneme upgrade [--retire-old]` writes the six-area tree, and with `--retire-old` moves the stale `40 Topics` folder to `mneme-archive/` rather than deleting it. Schema v3 adds the CHECK constraints the v1 rebuild dropped and files a legacy flat topic under `general`.
- **Outbox is explicitly inert.** The index says nothing reads it, and recall skips both Outbox and Inbox; a message left there cannot become context by accident.

## 0.0.10

The vault is a clone of WEFT, generic and empty, with marking posts a fresh agent can navigate.

- **Six areas.** `00 Inbox`, `10 Outbox` (for work that leaves the machine and, later, messages for other agents), `20 Research`, `30 Projects`, `40 Products`, `50 Topics`.
- **Topics are a tree.** The 26 categories are fixed from Foxforge's topic types, written to `TAXONOMY.md`; a topic is general (`technical/ai-inference`) and narrower things nest beneath it (`.../kv-cache`) instead of becoming a second topic for the same ethos.
- **The rules are structural.** Every research note, project and product names a topic; a durable project names its research; a product names the project it came from and is `active`, `closed`, `inactive` or `frozen`. Both the SQLite store and the file vault refuse a note that breaks a rule.
- **A fresh install reads as empty.** `hades init` seeds one `status: marker` note per area, and markers are excluded from the index, the audit counts and recall.
- **The index is a view.** `INDEX.md` groups topics by category, research by topic, and shows each project's research count and each product's origin; `mneme import` refreshes it so a vault cannot audit correctly and read as empty.
- **The record migrates.** Schema v2 rebuilds the note table for Products and nested slugs, keeps every row, tag and hash, and backfills each note's primary topic from its first tag.

## 0.0.9

The learning loop: a failed run becomes a dataset, a fix is measured against what came before, and a
procedure has to say what it is for before it can be loaded.

- **A failure becomes a case.** A giving-up run pins its starting tree, the goal, the oracle, the policy,
  model and the full ceiling it failed under. `hades cases` lists them, `hades case <id>` prints one,
  `hades case forget <id>` retires the tree and keeps the line. A case is a task, not a transcript: the
  copy is atomic, a deduplicated record appends `seen` rather than a second tree, and a tree past the
  snapshot limit is declined with the reason instead of being stored half-written.
- **`hades eval` replays the corpus.** A green start is `stale` with no model call; a red one re-runs the
  goal under the case's own policy and bounds in a throwaway copy. `--against <harness-dir>` runs the
  current harness and a candidate over every case and classifies each as `holds`, `fixed`, `regression`,
  `still failing` or `base drifted`, with exit 1 on a regression and 2 on a broken case or missing tree.
  `--dry-run` prints the cases, arms and attempt ceiling before anything is spent.
- **Adoption is gated.** `tune --adopt` runs the corpus for the base and the selected candidate before it
  copies anything and refuses an improvement that regresses a pinned case, with the case ids and goals on
  the receipt. `--no-regress` is the one way past it and the receipt says the corpus was skipped.
- **A security fix has to attack its own patch.** `--attack <cmd>` adds a second oracle: the attack
  succeeds while the vulnerability is present, so a green check beside a working exploit is a failed
  result, and an attack that cannot run is a fault rather than a blocked one. The attack command and any
  script it names are frozen by the ruler, the receipt names both oracles, and the case carries the attack
  so the gate refuses a candidate that revives it.
- **Dead ends and handover.** A giving-up run records why it was abandoned, deduplicated by goal and check
  fingerprint and capped at 200 per profile by recorded retirement, and recall tells the next run what was
  already tried. The harness also writes a handover before the walk ends: goal, attempts, best candidate,
  blockers, the case it became, and a next step only when one can be derived. `hades session handover`
  prints it.
- **The skill contract.** A proposal now carries `contract`, `regression`, `holdout` and `version`.
  Promotion refuses a `TODO` contract, fewer than two regression cases, or no holdout; `skills verify`
  installs the proposal in a throwaway state, runs the cases it names and writes a receipt promotion
  reads. `--force` promotes with the warning recorded. `skills report` prints proposals per 20 accepted
  runs and the regression pass rate of installed skills, and says when a number is unmeasured.
- **First-attempt rate and judge identity.** The scoreboard prints `first n/m` beside the pass rate, so a
  high pass rate with a low first-attempt rate reads as a slow profile. A judgement now carries the model
  and prompt digest that produced it, and the receipt prints them; a scripted judge says nothing rather
  than implying a model.

## 0.0.8

Truth, boundary and cost: a result cannot be bought by moving the check, a model cannot read what it was
never given, and a pass now carries what it cost.

- **The check is frozen before the first write.** The check command and every file the profile declares
  protected are hashed before the baseline run and re-hashed after every passing check. A change ends the
  run as `RulerChanged`, exit 1, with no accepted claim; the claim ladder refuses at `Attempted` before it
  reads the check, and a distilled case reads the ruler checkpoint from the log rather than trusting a
  green line. `--protect <path>` adds a file or directory for one run; `--protect inline-tests` adds every
  `#[cfg(test)]` item in every `*.rs`, and `--protect manifest:<path>` freezes a manifest's structure. The
  shipped `fix` and `sweep` profiles carry all three, plus `.cargo/config.toml`, `rust-toolchain.toml` and
  `build.rs`: those decide what the check runs, so a `[target.*] runner` entry or a `[[test]] path` cannot
  move the verifier without touching a test file. Inline test modules are frozen where they live, keyed by
  file and item name so an edit above them is not a change, and a manifest keeps its dependency tables free
  so `cargo add` stays a normal fix. Symlinks are entries in their own right and a symlinked directory is
  entered, so a link added under a protected path, or an edit to what it points at, is a change.
- **The browser has a scheme policy.** HTTP, HTTPS and `about:blank` only; `file://` is refused unless a
  fixture opts in. The policy is checked before `Page.navigate`, on every tab refresh (clicks, tab switches,
  redirects) and before observation text is returned; `upload` reads local files only under the same opt-in
  that `file://` needs, because the browser runs outside the cage. Chromium starts with a cleared
  environment and a HOME inside the state directory. The floor records `file:`, `data:`, `javascript:` and `vbscript:` opens as
  security access.
- **A model-run shell cannot read the harness' keys.** The child starts from `env_clear` plus PATH, HOME,
  TMPDIR, locale and the toolchain locations; this is the tool every governed run gives the model, not only
  the chat shell. The checker keeps the caller's environment because it is the caller's own command.
- **State is private.** `state_dir` makes 0700 directories and tightens existing ones, and every path that
  opens a session goes through it, so `turn`, `submit` and the daemon no longer leave a 0755 transcript
  behind. Scoreboard rows and the generated HTTP token are 0600, the token created with the mode rather than
  chmod'd after. Snapshots skip credential names (`.env`, `.netrc`, `.npmrc`, `.git-credentials`, `id_*`,
  `credentials`, `*.pem`, `*.key`, `*.p12`, `*.pfx`, `*.tfstate`) and the manifest names what it skipped. A
  directory is never treated as a credential, whatever it is called.
- **The HTTP surface has no open mode.** Every bind needs a token, loopback included. With none configured
  the server generates one at `<state>/http.token` (0600) and reuses it on restart; the path is printed,
  never the token. Behind it, a present `Origin` must name the connected address and a POST must be
  `application/json`.
- **A call reserves its prompt and its cap.** The token budget was checked only after a call reported its
  cost, so a run could overshoot by one completion cap. The runner now estimates the prompt, adds the call
  cap, and refuses to start a call the allowance cannot cover.
- **Cost sits beside the result.** The tuning receipt prints prompt and generated tokens beside the check,
  and the scoreboard reports tokens per pass rather than only a total.

## 0.0.7

The ask bar: a question is part of the input, not a modal, and the answer is one record any surface can
read.

- **A question anchored to the composer.** A floor refusal or a model fork draws on the input's top edge:
  the reason in one line, ranked options, one recommended and preselected, and `Other` reached by moving
  past the last row. Enter takes the selection. Typing moves into the input and selects `Other`, so a typed
  answer declines the recommendation by construction. `Esc` collapses the bar to one line; it never stops
  the run.
- **Two axes, kept separate.** The floor decides whether an action may run. A per-run budget decides
  whether the model may interrupt: two blocking and three advisory questions, enforced in code. A spent
  budget tells the model to decide and name its assumption instead of asking again.
- **Advisory questions continue the run.** The recommendation is taken, the assumption is recorded, and a
  later answer is delivered at the next step boundary with the assumption it replaces. Nothing already
  done is rewound.
- **Plan mode.** Read → Plan → Act, cycled with `Shift+Tab` or `/mode plan`. Plan mode has no mutating
  tools and asks at real forks before writing a plan: goal or scope, architecture, and how a step should be
  done when more than one way is valid.
- **One record, any surface.** Questions live in `approvals.jsonl`. `hades approvals` prints every option
  and the recommendation, `hades answer <id> <key|other>` answers from another terminal, and
  `hades log --asks <session>` prints one session. The F4 Approvals view lists the same options; Enter
  opens the bar on that row. A live daemon remains the only writer.
- **Taste, small and inspectable.** `state/taste.jsonl` records one line per answered question, scoped to
  its root. Three consistent choices move the recommendation; five agreeing choices stop that family being
  asked; permissions are never silenced; two `Other` declines are printed as a proposal, not applied.
  `hades taste` shows counts, recommendation, streak and proposals; `hades taste forget <n|all>` removes
  lines.

It follows 0.0.4's admission handles, goals and budgets. The session format is unchanged.

<details>
<summary>Earlier: what 0.0.4 added</summary>

0.0.4 was Part I of the 0.0.3 plan: five changes to the runtime itself, with the session format unchanged.

- **Admission handles.** `spawn` returns `{child_id, session, state:"admitted"}` before the child's first model call. A gateway-owned registry caps live children at 16, records them under `<state>/children/`, and marks a crashed gateway's children `lost` on restart instead of resuming them silently. `tell`, `children` and `result` are the tools a parent uses to work with a live child.
- **Goals that outlive a turn.** A goal is session state (`open`, `paused`, `done`, `cleared`) with its chat, model, check and budget. The gateway heartbeat re-enters an idle session on schedule. A goal closes when its recorded check passes or when a person closes it. When its total budget of model calls is spent it pauses with `budget` recorded, and `hades goal resume` puts it back to work.
- **Budgets that name their cause.** `hades run` and `hades submit` take `--budget-tokens`; the end of a run names `attempts`, `minutes` or `tokens`, and the scoreboard sums the spend the endpoint reported.
- **Numbered harness generations.** Every `tune --adopt` writes `~/.hades/control/harness/<profile>/<n>/` (under `HADES_HOME`) with a manifest naming every captured file, its hash, and the harness and state roots it came from. `hades refine --rollback <n>` restores exactly those files in two passes, and refuses to cross a floor change. The floor and policy guard lists are enumerated by the type system rather than by hand.
- **Strict model requests.** `--strict-model` preflights the endpoint and key before a session exists. A configured but unreachable endpoint fails on the first call naming the model instead of substituting another.

</details>
