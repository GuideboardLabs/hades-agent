//! The CLI, as a skeleton with a spine.
//!
//! Four of the commands need no model at all, and that is on purpose: `profiles`, `route`, `context` and
//! `audit` inspect the harness, and inspecting a harness should never require paying for a round trip.
//!
//! `turn` is the fifth, and it takes the program on the command line. That is not a stub. The model is a
//! seam, and until a model is wired to it, a person standing in for the model is using the same seam the
//! model will use: same turn, same policy, same log, same check. What it proves today is that the loop
//! works; what it proves tomorrow is that the model is not special.
//!
//! Exit codes: `0` the command did what it said, `1` the turn ran and the check failed, `2` the harness
//! could not do it at all (bad arguments, an unreadable vault, a fault).

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use serde_json::Value;

use mneme::NoteStore;

use crate::Assembly;
use crate::Engine;
use crate::Harness;
use crate::ScriptedModel;
use crate::approval::{ApprovalBroker, OnDisk, PendingId, Scope};
use crate::boundary;
use crate::config;
use crate::context::{self, Budgeted, Context, Learned, Section};
use crate::fault::Fault;
use crate::graph::{BaselineGate, Bounds, Ended, Remembering, Runner};
use crate::memory::{Memory, MnemeMemory, OutcomeNote};
use crate::model::{self, Key, OpenAi};
use crate::policy::{self, Decision};
use crate::profile::{self, Profile};
use crate::repomap::{Cache, Map, seed_words};
use crate::route::{Router, Rules};
use crate::shadow::{Checkpointer, Shadows, SnapshotId, Subject};
use crate::task::TaskSpec;
use crate::turn::Outcome;
use crate::verify::State;

const USAGE: &str = "\
hades — the harness, one turn or a whole graph

  hades tui [--root <dir>] [--state <dir>] [--model <preset>] [--no-motion]
      native terminal workspace (also: hades chat, or hades with no arguments in a terminal)
      --demo previews the interface offline; --help lists terminal options and shortcuts

  hades profiles
      the profiles this build ships, and their parameters

  hades harness [--profile <id>] [--harness <dir>]
      the prompt files a run would load, and where each part came from; with no
      argument, every harness directory under ~/.hades/harness

  hades tune <goal> [--candidate <dir>]... [--adopt] [--verify <cmd>] [--holdout <cmd>] [--profile <id>]
      run the goal under the base harness, edit two candidates from the transcript
      (or take given candidate directories), probe each against the same check, and
      keep the best. --adopt copies the winner into the profile's harness directory
      and numbers the harness it replaced, so the change is one command to undo.
      The floor, the policy and the check are never editable, and the editor never
      sees the check's text

  hades refine [--rollback <n>] [--profile <id>] [--harness <dir>] [--state <dir>]
      the numbered harness generations and what each one recorded; --rollback <n>
      puts those files back. A generation captured under a different floor, policy
      or set of profile defaults is refused

  hades route <goal>
      which profile the rules pick for a goal, and why

  hades scoreboard [--state <dir>]
      what our own finished runs say per profile: runs, pass rate, mean time, and
      what the router would pick. Rules always win; this answers only the fall-through

  hades skills [list | seed | promote <name> [--force] | reject <name> | verify <name> | report]
               [--state <dir>] [--profile <id>]
      the built-in procedures, proposals from a run that took more than one attempt, and
      the decision to load one. A proposal needs a filled-in `contract`, at least two
      `regression` cases and one `holdout`, and a passing `skills verify` receipt before
      promotion; `--force` promotes anyway and records the warning. `skills report`
      prints proposals per 20 accepted runs and the regression pass rate of installed
      skills. `verify` runs the proposal in a throwaway state and never touches the live
      `skills/` directory

  hades cases [--profile <id>] [--state <dir>] [--sized] [--json]
      the failure corpus: every failed run that was pinned as a task, its status,
      the ceiling it failed under, and the size of its tree. `--sized` stats the trees

  hades case <id> | forget <id> [--profile <id>] [--state <dir>]
      one case in full, or retire it: the case line stays as evidence, the tree goes

  hades eval [--case <id>]... [--profile <id>] [--against <harness-dir>]
             [--state <dir>] [--model <preset>] [--attempts <n>] [--minutes <m>]
             [--check-minutes <m>] [--include-partial] [--include-stale] [--dry-run]
             [--keep] [--json]
      replay the corpus against this harness: a green start is stale, a red one runs the
      case's goal again under its own policy, model and full bounds. --against runs the
      current harness and a candidate, and exits 1 when the candidate regresses a case

  hades explain <session> [--state <dir>]
      a turn map for one finished session: tool, check state, tokens, and the turns
      that decided the result. Reads the log; no model is asked

  hades distill <session> [--run] [--state <dir>] [--model <preset>]...
      rewrite a verified session into a runbook, gate it, and with --run replay it in
      a fresh copy of the session's first snapshot. Only a passing replay writes a
      skill proposal. The check command never reaches the runbook

  hades replay <runbook> --check <cmd> --root <dir> [--state <dir>]...
      run a saved runbook in the tree you name, under the tree's own check

  hades doctor [--state <dir>] [--config <file>] [--services on]
      what this install is: home, config, keys, sessions, jobs, skills, scoreboard,
      binary, and with --services on the two user units. Writes nothing

  hades config [--state <dir>] [--config <file>]
      the config a headless process reads, field by field. It holds no secrets

  hades session [list | show <id> | handover [<id>]] [--state <dir>]
      the logs under the state directory, one log entry by entry, or the harness's
      handover for a run that gave up or was stopped: goal, attempts, best candidate,
      blockers, the case it became, and a next step only when one can be derived

  hades update [--from <path>]
      what is running; with --from, install that file over the binary atomically.
      Restarting the units is left to the person

  hades context <goal> [--root <dir>]
      the bundle a turn would build, what fits and what was dropped

  hades audit [--vault <dir>] [--db <file>] [--config <file>] [--refresh]
      run the governance audit over the record, or over a tree imported into a temporary one;
      --refresh imports the config's vault into the record first

  hades init [--non-interactive] [--config <file>] [--vault <dir>] [--root <dir>]
             [--model <preset>] [--provider <name>] [--name <name>] [--goal <text>]
             [--topics <a,b>] [--db <file>]
      write the config, the record and the Mneme vault: six areas, the charter, a generated index

  hades service install [--prefix <dir>] [--home <dir>] [--out <dir>] [--force]
      write the two systemd user units for this install, rendered from the prefix and
      the home; a unit written by something other than this command is refused unless
      --force replaces it

  hades version
      the version of this build (`--version` and `-V` are the same)

  hades topic <category> <name...> [--parent <topic>] [--summary <text>]
              [--provenance <text>] [--vault <dir>] [--db <file>] [--config <file>]
      create a general topic under one of the 26 categories, or a subtopic with --parent

  hades research <topic> <title...> [--slug <slug>] [--source-url <url>] [--summary <text>]
                 [--provenance <text>] [--vault <dir>] [--db <file>] [--config <file>]
      write one durable finding under an existing topic

  hades project <name...> --topic <topic> --research <ref> [--research <ref>]...
                [--summary <text>] [--provenance <text>] [--vault <dir>] [--db <file>]
      create a project hub; every research reference must resolve to a note in the record

  hades product <name...> --from <project> [--topic <topic>]
                [--status active|closed|inactive|frozen] [--summary <text>]
                [--provenance <text>] [--vault <dir>] [--db <file>]
      freeze a project as a product; the topic is inherited from the project when omitted

  hades mneme import <dir> [--db <file>] [--vault <dir>]
      read a tree of markdown notes into the record; fenced paths are reported, never read

  hades mneme export [--force] [--db <file>] [--vault <dir>]
      write the record out as markdown; a file a hand edited is refused unless --force

  hades mneme upgrade [--retire-old] [--force] [--db <file>] [--vault <dir>]
      migrate a 0.0.9 vault to the six-area tree, seed the taxonomy and markers, and
      export. --retire-old moves the stale 40 Topics folder beside the vault, never deletes

  hades approvals [--state <dir>]
      every question that has been asked, with its options and what came of it

  hades answer <id> <option|allow|deny> [--note <text>] [--scope once|session] [--by <who>]
      answer a question, from this process or any other; `other` is a decline with a note

  hades log --asks <session> [--state <dir>]
      every question and answer for one session, as the record has them

  hades taste [--state <dir>]
      what the answers have taught: recommendations, streaks, and deny proposals
  hades taste forget <n|all> [--state <dir>]
      remove one line of the ledger, or all of them

  hades undo <session> [--to <n|latest>] [--state <dir>] [--root <dir>]
      put the tree back the way a shadow snapshot found it

  hades map <goal...> [--root <dir>] [--state <dir>] [--tokens <n>]
      the tree ranked for a goal, and what the map cache did

  hades daemon [--state <dir>] [--socket <path>]
      own the sessions under one state directory, in the foreground

  hades http [--bind 127.0.0.1:8787] [--state <dir>] [--token-file <path>] [--once]
      one read-and-control API over the daemon, for a local client. Every bind needs a
      token: HADES_HTTP_TOKEN, --token-file, or one generated at <state>/http.token and
      reused on restart. The token is never taken as an argument because argv is visible
      in `ps`; the generated file is 0600

  hades status [--state <dir>]
      what the daemon knows, or what the logs say when none is running

  hades fork <session> [--at <n>] [--state <dir>]
      a new session carrying the chain up to entry <n> (default: the head) verbatim, with a
      meta entry saying where it came from. The source is never touched, and the fork is
      the whole thread to that point, not a copy of one message

  hades acp [--root <dir>] [--state <dir>] [--config <file>] [--profile <id>]
            [--policy guarded|open] [--verify <cmd>] [--program <cmd>]...
      the Agent Client Protocol over stdio, one JSON-RPC object per line. An editor
      drives the same governed run: tool calls stream as session/update, and a floor
      or policy question becomes session/request_permission. --program is the offline
      probe, exactly as it is for `hades turn`

  hades gateway [--state <dir>] [--config <file>] [--once]
      answer Telegram messages as daemon ops: a message is a goal, /status and /stop are quick,
      an approval is an inline button. --once handles one batch and returns

  hades submit <goal> [--program <p>]... [--state <dir>] [--root <dir>] [--attempts <n>]
      [--minutes m] [--budget-tokens n] [--baseline red|off] [--strict-model]
      hand a run to the daemon and follow it. The budgets are the run's, not one call's: a
      budget of zero tokens is refused before the run is taken on. The baseline gate is the
      run's too: a green check is refused before the first write unless --baseline off

  hades attach <session> [--state <dir>]
      print a session's entries, then follow the ones that are written next

  hades models
      the ways this can reach a model, and what each one needs

  hades tools [--mode read|act] [--depth 0|1]
      what a surface is handed; a read surface has no tool that changes anything

  hades mcp list [--config <file>]
  hades mcp tools <server> [--config <file>]
  hades mcp call <server> <tool> [--args <json>] [--config <file>]
      the MCP servers this config names, what each one offers, and a call to one allowed tool

  hades jobs [--state <dir>]
      the standing goal files, with the fields a correction edits

  hades goal set <text> --chat <id> [--session <id>] [--model <preset>] [--check <cmd>]
                 [--budget <calls>] [--executor chat] [--state <dir>]
  hades goal list [--state <dir>]
  hades goal pause <id> [--reason <text>] [--state <dir>]
  hades goal resume <id> [--state <dir>]
  hades goal clear <id> [--state <dir>]
      a goal kept for one chat: the gateway heartbeat continues it when the session is idle,
      and the recorded check decides when it is done. A spent budget pauses it, and `resume`
      puts a paused goal back to work. `list` reads files and calls no model

  hades activity [--state <dir>] [--limit n]
      the last decided actions, one line each, oldest first

  hades stop [--session <id>] [--run <n>] [--state <dir>]
      stop running work: one session's run, one numbered run, or every run. A stop is a request:
      the run ends at its next boundary, and what already ran is not undone

  hades cancel <job> [--state <dir>]
      remove a standing job and its pending retry, so it stops being due

  hades shutdown [--state <dir>]
      stop every run and end the daemon. Stopping is not undo

  hades run <goal> --model <preset> [--attempts n] [--minutes m] [--budget-tokens n] [--branch n]
             [--baseline red|off] [--case auto|off|force] [--attack <cmd>] [--check-minutes m]
             [--strict-model] [...]
      walk the graph: try, accept a pass, retry a failure while the bounds hold, hand a
      refusal to a person. --case controls the failure corpus: a giving-up run becomes a
      pinned, replayable case by default; off writes none and force keeps a partial tree.
      --attack adds a second oracle to a security fix: the attack must fail once the
      check passes, and an attack that cannot run is a fault rather than a pass.
      --budget-tokens caps the spend the endpoint reports for the whole
      run, prompt and generated together; the end names which bound ran out. --branch n runs
      n candidate turns from the same tree and keeps the first pass (1 to 4); the attempts
      budget is shared, so width is model calls, not a second budget. --baseline red (the
      default) runs the check before the first write and refuses a run whose check already
      passes; use off for a refactor that keeps a green check green. --protect <path> is
      repeatable: the check command and every protected file are hashed before the first
      write and again after the check, and an edit ends the run as refused. `--protect
      inline-tests` freezes every `#[cfg(test)]` item in every `*.rs`, which is how the
      shipped profiles cover tests that live inside `src/`. `--protect manifest:<path>` freezes
      a manifest except its dependency tables, so `cargo add` is not a change and a
      redirected `[[test]]` target is. With no protected path the receipt says `ruler:
      command only`. The shipped fix and sweep profiles freeze `tests/`, the build
      configuration (`.cargo/config.toml`, `rust-toolchain.toml`, `build.rs`), the manifest's
      structure, and every inline test module.

  hades turn <goal> --model <preset> [--root <dir>] [--profile <id>]
      [--policy open|guarded] [--verify <command>] [--state <dir>] [--session <id>]
      [--cage on|off] [--temperature t] [--max-tokens n] [--context n] [--thinking on|off]
      [--strict-model]
      run one turn, with a model answering

  hades turn <goal> --program <program> [...]
      run one turn with the program given on the command line, no model asked

  hades turn <goal> --model <name> --endpoint <url> [--key env:VAR|<VAR>]
      reach a model this build does not know about

Policies: open runs the program, guarded asks first and remembers the answer for the session.
A key is named, never typed: `--key env:OLLAMA_API_KEY` or `--key OLLAMA_API_KEY` for Hades' own file,
which is ~/.hades/.env. Hades does not read the live install's keys.
`hades init` writes ~/.hades/config.json with the provider, the model preset, the working root and the
vault, then creates ~/.hades/mneme. A run with no --model uses the config's preset and a --model is an
override; `--config <file>` reads a different file. Secrets never go in the config: they stay in .env.
`--thinking off` asks a local template for no thinking, which is what a thinking model needs before it can
answer with one program instead of a paragraph about one.
`--strict-model` holds a run to the one model it names. The endpoint and the key are checked before a
session is opened, so a request that cannot work fails while it is free, and a fault from the first call
names that model. Nothing is substituted for it.
The cage is on by default: the program may read the system and write in the task's root and in the state
directory's tmp, and it may not read your home directory or reach the network. --cage off turns it off.
A shadow snapshot of the tree is taken before each program runs, so a change can be undone: --shadow off
turns that off, and `hades undo <session>` puts the latest one back.
Exit codes: 0 done, 1 the turn ran and the check failed, 2 the harness could not do it
";

/// Run the CLI. Returns the exit code, and writes everything it has to say to `out` so a test can read it.
pub fn run(args: &[String], out: &mut dyn Write) -> i32 {
    let mut input = std::io::stdin().lock();
    run_with(args, out, &mut input)
}

/// The same, with the answers supplied instead of read from the terminal.
///
/// The interview is the only command that reads input, and a prompt is hard to test through a pty. A seam
/// here is the difference between testing the interview and testing a subprocess.
pub fn run_with(args: &[String], out: &mut dyn Write, input: &mut dyn BufRead) -> i32 {
    let (text, code) = match dispatch(args, out, input) {
        Ok(pair) => pair,
        Err(message) => (format!("{message}\n\n{USAGE}"), 2),
    };
    let _ = out.write_all(text.as_bytes());
    code
}

fn dispatch(
    args: &[String],
    out: &mut dyn Write,
    input: &mut dyn BufRead,
) -> Result<(String, i32), String> {
    let command = args.first().map(String::as_str).unwrap_or("help");
    let rest = &args[1..];

    // A command's own `--help` is the same answer as `hades help`. A person asking what flags a command
    // takes should not be told that the flag they asked with is not one of them.
    if rest.iter().any(|arg| arg == "--help" || arg == "-h") {
        return Ok((USAGE.to_string(), 0));
    }

    match command {
        "help" | "--help" | "-h" => Ok((USAGE.to_string(), 0)),
        "version" | "--version" | "-V" => Ok((format!("hades {}\n", env!("CARGO_PKG_VERSION")), 0)),
        "profiles" => profiles(rest),
        "harness" => harness_cmd(rest),
        "tune" => tune(rest),
        "refine" => refine(rest),
        "route" => route(rest),
        "skills" => skills_cmd(rest),
        "cases" => cases_cmd(rest),
        "case" => case_cmd(rest),
        "eval" => eval_cmd(rest),
        "explain" => explain(rest),
        "distill" => distill(rest),
        "replay" => replay(rest),
        "doctor" => doctor(rest),
        "config" => config_cmd(rest),
        "session" => session_cmd(rest),
        "update" => update(rest),
        "scoreboard" => scoreboard(rest),
        "context" => context(rest),
        "audit" => audit(rest),
        "init" => init(rest, out, input),
        "topic" => create_topic_cmd(rest),
        "research" => create_research_cmd(rest),
        "project" => create_project_cmd(rest),
        "product" => create_product_cmd(rest),
        "service" => service(rest),
        "mneme" => mneme(rest),
        "approvals" => approvals(rest),
        "answer" => answer(rest),
        "taste" => taste_cmd(rest),
        "log" => log_cmd(rest),
        "turn" => turn(rest),
        "run" => run_graph(rest),
        "fork" => fork(rest),
        "undo" => undo(rest),
        "map" => map(rest),
        "daemon" => daemon(rest),
        "http" => http(rest),
        "acp" => acp(rest, input),
        "gateway" => gateway(rest),
        "status" => status(rest),
        "submit" => submit(rest),
        "attach" => attach(rest),
        "models" => models(),
        "tools" => tools(rest),
        "mcp" => mcp(rest),
        "jobs" => jobs(rest),
        "goal" => goal_cmd(rest),
        "activity" => activity(rest),
        "stop" => stop(rest),
        "cancel" => cancel(rest),
        "shutdown" => shutdown(rest),
        other => Err(format!("there is no command `{other}`")),
    }
}

/// The same flags, built from a request over the socket.
///
/// One assembly path for two surfaces: a submitted run is described as the flags a person would have typed,
/// so the daemon cannot drift into building a run the CLI would not have built. `goal` is positional because
/// that is what `hades turn <goal>` makes it.
pub(crate) fn flags_from(request: &Value) -> Result<Flags, String> {
    let mut flags = Flags {
        positional: Vec::new(),
        named: BTreeMap::new(),
        repeated: BTreeMap::new(),
    };

    let goal = text(request, "goal");
    if goal.is_empty() {
        return Err("a submitted run needs a `goal`".to_string());
    }
    flags.positional.push(goal);

    const NAMED: [&str; 23] = [
        "root",
        "profile",
        "policy",
        "verify",
        "state",
        "session",
        "cage",
        "shadow",
        "model",
        "endpoint",
        "key",
        "temperature",
        "max-tokens",
        "context",
        "attempts",
        "minutes",
        "budget-tokens",
        "baseline",
        "strict-model",
        "protect",
        "case",
        "attack",
        "check-minutes",
    ];

    for key in NAMED {
        let Some(value) = request.get(key) else {
            continue;
        };
        let given = match value {
            Value::String(text) => text.clone(),
            Value::Number(number) => number.to_string(),
            Value::Bool(flag) => flag.to_string(),
            // `protect` is repeatable, like `program`: a list is the shape a caller uses when it has more
            // than one path to freeze.
            Value::Array(list) if key == "protect" => {
                flags.repeated.insert(
                    key.to_string(),
                    list.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect(),
                );
                continue;
            }
            _ => {
                return Err(format!("`{key}` has to be a string or a number"));
            }
        };
        // One path and a list of paths mean the same thing. `protect_of` reads the repeatable form, so a
        // string left only in `named` was silently dropped and the caller believed the path was frozen.
        if key == "protect" {
            flags.repeated.insert(key.to_string(), vec![given]);
            continue;
        }
        flags.named.insert(key.to_string(), given);
    }

    if let Some(list) = request.get("program").and_then(Value::as_array) {
        flags.repeated.insert(
            "program".to_string(),
            list.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
        );
    }

    Ok(flags)
}

fn text(payload: &Value, key: &str) -> String {
    payload
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// The flags a command accepts, parsed strictly: an unknown flag is an error rather than something
/// ignored, because a silently ignored `--profil` is a run on the wrong profile.
pub(crate) struct Flags {
    positional: Vec<String>,
    named: BTreeMap<String, String>,
    /// Flags given more than once, in order. `--program` is the one that matters: a graph asks a model for a
    /// program per attempt, so a person driving one by hand needs to say what the model answers each time.
    repeated: BTreeMap<String, Vec<String>>,
}

fn parse(args: &[String], known: &[&str]) -> Result<Flags, String> {
    parse_with(args, known, &[])
}

/// `switches` are flags that take no value. They are recorded as `true` in `named`, so a caller reads one
/// switch the same way it reads any other flag and there is no second field in [`Flags`] to keep in step.
fn parse_with(args: &[String], known: &[&str], switches: &[&str]) -> Result<Flags, String> {
    let mut flags = Flags {
        positional: Vec::new(),
        named: BTreeMap::new(),
        repeated: BTreeMap::new(),
    };

    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        match arg.strip_prefix("--") {
            None => flags.positional.push(arg.clone()),
            Some(body) => {
                let (name, value) = match body.split_once('=') {
                    Some((name, value)) => (name.to_string(), value.to_string()),
                    None if switches.contains(&body) => (body.to_string(), "true".to_string()),
                    None => {
                        index += 1;
                        let value = args
                            .get(index)
                            .ok_or_else(|| format!("`--{body}` needs a value"))?;
                        (body.to_string(), value.clone())
                    }
                };
                if !known.contains(&name.as_str()) && !switches.contains(&name.as_str()) {
                    return Err(format!("`--{name}` is not a flag this command takes"));
                }
                flags
                    .repeated
                    .entry(name.clone())
                    .or_default()
                    .push(value.clone());
                flags.named.insert(name, value);
            }
        }
        index += 1;
    }

    Ok(flags)
}

/// The completion cap a run sends: the declared `--max-tokens`, or the shipped default. One reader, so the
/// daemon and the CLI cannot reserve different amounts for the same request.
pub(crate) fn call_tokens(flags: &Flags) -> u64 {
    flags
        .named
        .get("max-tokens")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(crate::graph::DEFAULT_CALL_TOKENS)
}

/// Whether a switch was given. `--non-interactive=off` is accepted because a wrapper script that builds
/// its arguments from a variable should not have to choose between two spellings.
fn switched(flags: &Flags, name: &str) -> bool {
    match flags.named.get(name).map(String::as_str) {
        None => false,
        Some("off") | Some("false") | Some("no") => false,
        Some(_) => true,
    }
}

/// The ways this can reach a model, said out loud instead of buried in a default.
fn models() -> Result<(String, i32), String> {
    let mut text = String::from(
        "A preset names an endpoint and a model. A local one needs a server on the port it names, because \
         the harness does not start servers; a cloud one needs its key, which is read from the environment \
         or from ~/.hades/.env and is never printed.\n\n",
    );
    for preset in model::presets() {
        text.push_str(&format!(
            "{:<20} {}\n{:<20} {}{}\n",
            preset.id,
            preset.model,
            "",
            preset.endpoint,
            match &preset.key {
                Some(Key::Env(var)) => format!("\n{:<20} key: ${var}", ""),
                Some(Key::Home(var)) => format!("\n{:<20} key: {var} in ~/.hades/.env", ""),
                None => String::new(),
            }
        ));
        text.push_str(&format!("{:<20} {}\n\n", "", preset.note));
    }
    text.push_str(
        "Anything else: turn --model <name> --endpoint <url> [--key env:VAR|<VAR>] [--temperature t] \
         [--max-tokens n]\n",
    );
    Ok((text, 0))
}

/// What a surface is handed. The rule "a read job has no tool that changes anything" is a property of
/// this list, so a person can read it instead of trusting the prose.
fn tools(args: &[String]) -> Result<(String, i32), String> {
    let flags = parse(args, &["mode", "depth"])?;
    let mode = match flags.named.get("mode").map(String::as_str) {
        None => crate::mode::Mode::Act,
        Some(text) => crate::mode::Mode::parse(text)
            .ok_or_else(|| format!("no mode named `{text}`: say read or act"))?,
    };
    let depth = match flags.named.get("depth").map(String::as_str) {
        None => 0,
        Some(text) => text
            .parse::<u8>()
            .map_err(|_| format!("`--depth {text}` is not a number"))?
            .min(1),
    };
    let schema = crate::agent::schemas(depth, mode);
    let mut text = format!(
        "mode: {} ({})\n",
        mode.name(),
        if mode.acts() {
            "may act; the hard floor stands in front"
        } else {
            "no tool that changes anything"
        }
    );
    for tool in schema.as_array().into_iter().flatten() {
        if let Some(name) = tool
            .pointer("/function/name")
            .and_then(serde_json::Value::as_str)
        {
            text.push_str(&format!("  {name}\n"));
        }
    }
    Ok((text, 0))
}

/// `hades mcp`: the same registry and the same refusal the chat tool uses, without a model. One
/// implementation means the person at a terminal and the model in a chat cannot get different answers
/// about what is allowed.
fn mcp(args: &[String]) -> Result<(String, i32), String> {
    let action = args.first().map(String::as_str).unwrap_or("list");
    let rest = &args[1..];
    let flags = parse_with(rest, &["config", "args"], &[])?;
    if !matches!(action, "list" | "tools" | "call") {
        return Err(format!(
            "`{action}` is not an mcp action: say list, tools or call"
        ));
    }
    let path = config_path(&flags)?;
    let config = config::load(&path)?.ok_or_else(|| {
        format!(
            "{} does not exist; run `hades init` first, then add `mcp.servers` to it",
            path.display()
        )
    })?;
    let registry = crate::mcp::Mcp::new(config.mcp, Some(config.root));
    let mut request = serde_json::json!({ "action": action });
    match action {
        "tools" => {
            let server = flags
                .positional
                .first()
                .ok_or_else(|| "say which server: `hades mcp tools <server>`".to_string())?;
            request["server"] = Value::String(server.clone());
        }
        "call" => {
            let server = flags
                .positional
                .first()
                .ok_or_else(|| "say which server: `hades mcp call <server> <tool>`".to_string())?;
            let tool = flags
                .positional
                .get(1)
                .ok_or_else(|| "say which tool: `hades mcp call <server> <tool>`".to_string())?;
            request["server"] = Value::String(server.clone());
            request["tool"] = Value::String(tool.clone());
            if let Some(text) = flags.named.get("args") {
                let arguments: Value = serde_json::from_str(text)
                    .map_err(|error| format!("`--args` is not JSON: {error}"))?;
                if !arguments.is_object() {
                    return Err("`--args` must be a JSON object".to_string());
                }
                request["arguments"] = arguments;
            }
        }
        _ => {}
    }
    match registry.run(&request) {
        Ok(text) => Ok((format!("{text}\n"), 0)),
        Err(said) => Ok((format!("{said}\n"), 1)),
    }
}

/// The standing goal files, as a person would read them. This is the surface a correction edits, so the
/// path and every statement of intent are shown, not only the schedule.
fn jobs(args: &[String]) -> Result<(String, i32), String> {
    let flags = parse(args, &["state"])?;
    let state = match flags.named.get("state") {
        Some(dir) => PathBuf::from(dir),
        None => hades_home()?,
    };
    let (jobs, problems) = crate::cron::load(&state);
    let mut text = String::new();
    for job in &jobs {
        let path = crate::cron::path_of(&state, job.id)
            .map(|path| crate::cron::relative(&state, &path))
            .unwrap_or_default();
        text.push_str(&format!(
            "{} · {} · {} · {}\n",
            job.id,
            job.schedule,
            job.mode.name(),
            path
        ));
        text.push_str(&format!(
            "  goal: {}\n",
            job.goal.lines().next().unwrap_or("")
        ));
        for (label, value) in [
            ("inputs", &job.inputs),
            ("outputs", &job.outputs),
            ("done_when", &job.done_when),
            ("stop_when", &job.stop_when),
            ("until", &job.until),
            ("never", &job.never),
        ] {
            if !value.is_empty() {
                text.push_str(&format!("  {label}: {value}\n"));
            }
        }
        text.push_str(&format!(
            "  last: {}\n",
            if job.last == 0 {
                "never".to_string()
            } else {
                job.last.to_string()
            }
        ));
        if let Some(entry) = crate::retry::latest(&state, job.id) {
            text.push_str(&format!(
                "  last attempt: {} of {} {}\n",
                entry.attempt,
                crate::retry::ATTEMPTS,
                entry.outcome
            ));
        }
        if let Some(pending) = crate::retry::read(&state, job.id) {
            text.push_str(&format!(
                "  retrying: attempt {} of {} at {}\n",
                pending.attempt + 1,
                crate::retry::ATTEMPTS,
                pending.at
            ));
        }
    }
    for problem in &problems {
        text.push_str(&format!("not run: {problem}\n"));
    }
    if text.is_empty() {
        text = format!("no jobs under {}\n", crate::cron::dir(&state).display());
    }
    Ok((text, 0))
}

/// The epoch second a goal file records. One clock for `set`, `pause` and `clear`, so a listing reads
/// as one event sequence.
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

/// `hades goal`: the file surface a person uses when the agent should keep working across turns.
///
/// `list` reads the goal files and nothing else, which is why it needs no config and no model: the
/// question "what is Hades still working on" must be answerable on a machine whose endpoint is down.
fn goal_cmd(args: &[String]) -> Result<(String, i32), String> {
    let action = args.first().map(String::as_str).unwrap_or("list");
    let rest = &args[1..];
    let flags = parse(
        rest,
        &[
            "state", "chat", "session", "model", "check", "budget", "executor", "reason",
        ],
    )?;
    let state = match flags.named.get("state") {
        Some(dir) => PathBuf::from(dir),
        None => hades_home()?,
    };
    let now = now();
    match action {
        "set" => {
            let text = flags.positional.join(" ");
            if text.trim().is_empty() {
                return Err("`hades goal set` needs the goal text".to_string());
            }
            let chat: i64 = flags
                .named
                .get("chat")
                .ok_or_else(|| {
                    "a goal needs --chat <id>; a goal with no chat cannot be re-entered".to_string()
                })?
                .parse()
                .map_err(|_| "`--chat` has to be a number".to_string())?;
            let budget: u32 = match flags.named.get("budget") {
                Some(value) => value
                    .parse()
                    .map_err(|_| format!("`--budget {value}` is not a number of calls"))?,
                None => crate::gateway::goal::DEFAULT_BUDGET,
            };
            let session = flags
                .named
                .get("session")
                .cloned()
                .unwrap_or_else(|| format!("tg-{chat}"));
            let new = crate::gateway::goal::NewGoal {
                text,
                session,
                chat,
                model: flags.named.get("model").cloned().unwrap_or_default(),
                check: flags.named.get("check").cloned().unwrap_or_default(),
                budget,
                executor: flags
                    .named
                    .get("executor")
                    .cloned()
                    .unwrap_or_else(|| "chat".to_string()),
            };
            match crate::gateway::goal::set(&state, new, now) {
                Ok(goal) => Ok((
                    format!(
                        "goal {} is open for chat {} (session {}); last heartbeat: {}\n",
                        goal.id,
                        goal.chat,
                        goal.session,
                        crate::gateway::goal::heartbeat_text(goal.heartbeat)
                    ),
                    0,
                )),
                Err(refusal) => Err(format!("{refusal} ({})", refusal.name())),
            }
        }
        "list" => {
            let (goals, problems) = crate::gateway::goal::load(&state);
            let mut text = String::new();
            for goal in &goals {
                text.push_str(&crate::gateway::goal::describe(goal));
                text.push('\n');
            }
            for problem in &problems {
                text.push_str(&format!("not read: {problem}\n"));
            }
            if text.is_empty() {
                text = format!(
                    "no goals under {}\n",
                    crate::gateway::goal::dir(&state).display()
                );
            }
            Ok((text, 0))
        }
        "pause" | "resume" | "clear" => {
            let id: u64 = flags
                .positional
                .first()
                .ok_or_else(|| format!("`hades goal {action}` needs a goal number"))?
                .parse()
                .map_err(|_| "a goal number is a whole number".to_string())?;
            let outcome = match action {
                "pause" => {
                    let reason = flags
                        .named
                        .get("reason")
                        .cloned()
                        .unwrap_or_else(|| "paused by a person".to_string());
                    crate::gateway::goal::pause(&state, id, &reason, now)
                }
                "resume" => crate::gateway::goal::resume(&state, id, now),
                _ => crate::gateway::goal::clear(&state, id, now),
            };
            match outcome {
                Ok(goal) => Ok((
                    format!(
                        "goal {} is {}{}\n",
                        goal.id,
                        goal.state.name(),
                        if goal.reason.is_empty() {
                            String::new()
                        } else {
                            format!(": {}", goal.reason)
                        }
                    ),
                    0,
                )),
                Err(refusal) => Err(format!("{refusal} ({})", refusal.name())),
            }
        }
        other => Err(format!(
            "`{other}` is not a goal action: say set, list, pause, resume or clear"
        )),
    }
}

fn activity(args: &[String]) -> Result<(String, i32), String> {
    let flags = parse(args, &["state", "limit"])?;
    let state = match flags.named.get("state") {
        Some(dir) => PathBuf::from(dir),
        None => hades_home()?,
    };
    let limit: usize = match flags.named.get("limit") {
        Some(text) => text
            .parse()
            .map_err(|_| format!("`{text}` is not a number of lines"))?,
        None => 20,
    };
    let all = crate::activity::read(&state);
    if all.is_empty() {
        return Ok((
            format!(
                "no activity under {}\n",
                crate::activity::path(&state).display()
            ),
            0,
        ));
    }
    let start = all.len().saturating_sub(limit);
    let mut text = String::new();
    for line in &all[start..] {
        let mut row = format!(
            "{} {:<10} {:<8} {:<7} {}",
            stamp(line.at),
            line.actor,
            line.layer,
            line.decision,
            line.action
        );
        if !line.reason.is_empty() {
            row.push_str(&format!(" ({})", line.reason));
        }
        if !line.artifact.is_empty() {
            row.push_str(&format!(" -> {}", line.artifact));
        }
        text.push_str(row.trim_end());
        text.push('\n');
    }
    Ok((text, 0))
}

/// Stop running work. One verb with a scope: a session, a numbered run, or every run. It never ends the
/// daemon and never deletes a job, because those are different promises and a surface that blurs them lies.
fn stop(args: &[String]) -> Result<(String, i32), String> {
    let flags = parse(args, &["state", "session", "run"])?;
    let state = state_of(&flags)?;
    let socket = crate::daemon::live(&state).ok_or_else(|| {
        format!(
            "no daemon is running under {}; start one with `hades daemon --state {}`",
            state.display(),
            state.display()
        )
    })?;

    let mut request = serde_json::json!({"op": "stop", "by": who()});
    if let Some(session) = flags.named.get("session") {
        request["session"] = serde_json::json!(session);
    }
    if let Some(run) = flags.named.get("run") {
        let run: u64 = run
            .parse()
            .map_err(|_| format!("`{run}` is not a run number"))?;
        request["run"] = serde_json::json!(run);
    }

    let answer =
        crate::daemon::ask(&socket, request).map_err(|fault| fault.detail().to_string())?;
    if answer.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(answer
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("the daemon refused the stop")
            .to_string());
    }
    let stopping = answer
        .get("stopping")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if stopping.is_empty() {
        return Ok(("nothing is running; stopping is not undo\n".to_string(), 0));
    }
    let mut text = String::from("stopping:\n");
    for run in &stopping {
        text.push_str(&format!(
            "  run {} in {}\n",
            run.get("run").and_then(Value::as_u64).unwrap_or(0),
            run.get("session").and_then(Value::as_str).unwrap_or("?")
        ));
    }
    text.push_str("a run ends at its next boundary; what already ran is not undone\n");
    Ok((text, 0))
}

/// Cancel a standing job: the file and its pending retry. The one function the chat also calls, so the two
/// surfaces cannot disagree about what cancellation removes.
fn cancel(args: &[String]) -> Result<(String, i32), String> {
    let flags = parse(args, &["state"])?;
    let state = state_of(&flags)?;
    let id = flags
        .positional
        .first()
        .ok_or_else(|| "this command needs a job number, as in `hades cancel 3`".to_string())?
        .parse::<u64>()
        .map_err(|_| "that is not a job number".to_string())?;
    let path = crate::cron::cancel(&state, id, &who())?;
    Ok((
        format!("cancelled job {id}: {path} is gone and any pending retry is cleared\n"),
        0,
    ))
}

/// Stop every run and end the daemon. The one verb that ends the process; `stop` never does.
fn shutdown(args: &[String]) -> Result<(String, i32), String> {
    let flags = parse(args, &["state"])?;
    let state = state_of(&flags)?;
    let socket = crate::daemon::live(&state).ok_or_else(|| {
        format!(
            "no daemon is running under {}; there is nothing to shut down",
            state.display()
        )
    })?;
    let answer = crate::daemon::ask(&socket, serde_json::json!({"op": "shutdown", "by": who()}))
        .map_err(|fault| fault.detail().to_string())?;
    if answer.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(answer
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("the daemon refused the shutdown")
            .to_string());
    }
    let count = answer.get("stopping").and_then(Value::as_u64).unwrap_or(0);
    Ok((
        format!(
            "stopping {count} run(s); the daemon on {} is exiting\n",
            socket.display()
        ),
        0,
    ))
}

/// Who is asking, for the ledger. The daemon records the string it is given rather than guessing an identity
/// from the request, which is the same rule the approval broker follows.
fn who() -> String {
    format!(
        "cli:{}",
        std::env::var("USER").unwrap_or_else(|_| "unknown".to_string())
    )
}

/// A time a person can read. The ledger stores epoch seconds because that is unambiguous; the CLI shows
/// the local clock, and falls back to the number when `date` is not there.
fn stamp(at: i64) -> String {
    std::process::Command::new("date")
        .arg("-d")
        .arg(format!("@{at}"))
        .arg("+%m-%d %H:%M:%S")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| at.to_string())
}

pub(crate) fn goal(flags: &Flags) -> Result<String, String> {
    flags
        .positional
        .first()
        .cloned()
        .ok_or_else(|| "this command needs a goal".to_string())
}

/// Who answers this turn: a model when one is named, the person at the keyboard when a program is given.
///
/// The preset path and the raw path differ only in where the endpoint and the key come from, so the
/// temperature and the token ceiling are applied once, after either.
fn answerer(
    flags: &Flags,
    config: Option<&config::Config>,
    harness: Option<&crate::harness_files::Files>,
    profile_model: Option<&str>,
) -> Result<Answerer, String> {
    let mut open = match flags.named.get("model") {
        Some(model_id) => match flags.named.get("endpoint") {
            Some(endpoint) => {
                let mut open = OpenAi::new(endpoint.clone(), model_id.clone());
                if let Some(spec) = flags.named.get("key") {
                    open = open.with_key(key_spec(spec));
                }
                open
            }
            None => {
                let preset = model::preset(model_id).ok_or_else(|| {
                    format!(
                        "no model named `{model_id}`: try {}",
                        model::presets()
                            .iter()
                            .map(|preset| preset.id)
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })?;
                model::from_preset(&preset)
            }
        },
        None => {
            if let Some(endpoint) = flags.named.get("endpoint") {
                return Err(format!(
                    "`--endpoint {endpoint}` needs `--model <name>` beside it: an endpoint does not say \
                     which model to ask for"
                ));
            }
            // Every `--program` in order, so a walk can be driven by hand: one program per attempt. A
            // program is a person at the keyboard, and a person outranks a config file.
            if let Some(programs) = flags
                .repeated
                .get("program")
                .filter(|programs| !programs.is_empty())
            {
                return Ok(Answerer::Scripted(ScriptedModel::new(programs.clone())));
            }
            // No flag names an answerer, so the config does. This is the seam a daemon or a bot stands
            // on: it was started with no arguments and has nobody to ask, but it does have a file.
            match config {
                Some(config) => {
                    let chosen = profile_model.unwrap_or(&config.model);
                    let preset = model::preset(chosen).ok_or_else(|| {
                        format!(
                            "the model `{chosen}` has no preset in this build. Run `hades init --model \
                             <preset>` again, name a profile model that exists, or pass --model: {}",
                            model::presets()
                                .iter()
                                .map(|preset| preset.id)
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    })?;
                    model::from_preset(&preset)
                }
                None => {
                    return Err(
                        "this needs an answerer: `--model <preset>` to ask a model, `--program <program>` \
                         to answer as the person at the keyboard (repeat it to script several attempts), \
                         or `hades init` to write a config that names a preset. `hades models` lists the \
                         presets."
                            .to_string(),
                    );
                }
            }
        }
    };

    if let Some(temperature) = flags.named.get("temperature") {
        open = open.with_temperature(
            temperature
                .parse::<f32>()
                .map_err(|_| format!("`--temperature {temperature}` is not a number"))?,
        );
    }
    if let Some(max) = flags.named.get("max-tokens") {
        open = open.with_max_tokens(
            max.parse::<u32>()
                .map_err(|_| format!("`--max-tokens {max}` is not a number"))?,
        );
    }
    if let Some(context) = flags.named.get("context") {
        open = open.with_context(
            context
                .parse::<usize>()
                .map_err(|_| format!("`--context {context}` is not a number"))?,
        );
    }
    match flags.named.get("thinking").map(String::as_str) {
        None => {}
        Some("on") => open = open.with_thinking(true),
        Some("off") => open = open.with_thinking(false),
        Some(other) => return Err(format!("`--thinking {other}`: say on or off")),
    }
    if let Some(files) = harness {
        open = open.with_system(files.effective());
    }
    Ok(Answerer::Model(open))
}

/// Where a key comes from: `env:NAME` for the environment, anything else for the file Hermes keeps on this
/// machine. A key typed on the command line would land in the shell's history and in the process table, so
/// this takes the *name* of a key and never a key.
fn key_spec(spec: &str) -> Key {
    match spec.strip_prefix("env:") {
        Some(var) => Key::Env(var.to_string()),
        None => Key::Home(spec.to_string()),
    }
}

/// Who answers. An enum rather than a boxed trait object, because there are two and the CLI prints which.
pub(crate) enum Answerer {
    Model(OpenAi),
    Scripted(ScriptedModel),
}

impl Answerer {
    /// The window this answerer works inside, so the turn's budget can be capped by it. A person at the
    /// keyboard has no window to respect, so the profile's own budget stands.
    fn context(&self) -> usize {
        match self {
            Answerer::Model(open) => open.context(),
            Answerer::Scripted(_) => usize::MAX,
        }
    }

    pub(crate) fn harness(&self) -> &dyn Harness {
        match self {
            Answerer::Model(open) => open,
            Answerer::Scripted(scripted) => scripted,
        }
    }

    /// What a person may see: where the answer came from. Never a key, which is not in here to leak.
    pub(crate) fn describe(&self) -> String {
        match self {
            Answerer::Model(open) => match open.thinking() {
                Some(false) => format!("{} (thinking off)", open.display()),
                Some(true) => format!("{} (thinking on)", open.display()),
                None => open.display(),
            },
            Answerer::Scripted(_) => {
                "scripted: the program came from --program, no model was asked".to_string()
            }
        }
    }

    /// Hold the answerer to the model it names. The preflight is the whole point of the flag: a request
    /// whose endpoint or key cannot work is refused here, while the refusal costs nothing, instead of
    /// being discovered by the call it would have spent. The client keeps the flag so a fault from that
    /// call names the model rather than leaving the reader to wonder what else was tried.
    fn strict(self) -> Result<Self, String> {
        match self {
            Answerer::Model(open) => {
                let open = open.with_strict();
                open.preflight().map_err(|fault| {
                    format!("the strict model request was refused: {}", fault.detail())
                })?;
                Ok(Answerer::Model(open))
            }
            Answerer::Scripted(_) => Err(
                "`--strict-model` names no model: a --program answers instead of one. Pass \
                 --model <preset>, or drop --strict-model"
                    .to_string(),
            ),
        }
    }
}

/// The `--protect` paths a run declared. A repeated flag is a list, and each value is a file or a
/// directory rule relative to the run's root. Refused here, before the model is asked.
pub(crate) fn protect_of(
    flags: &Flags,
    root: &std::path::Path,
) -> Result<Vec<crate::profile::Protect>, String> {
    let mut rules = Vec::new();
    if let Some(values) = flags.repeated.get("protect") {
        for value in values {
            rules.push(crate::profile::Protect::parse(value, root)?);
        }
    }
    Ok(rules)
}

fn root_of(flags: &Flags) -> Result<PathBuf, String> {
    let config = config_of(flags)?;
    root_with(flags, config.as_ref())
}

/// The root a run works in: the flag, then the config, then the directory the process stands in.
fn root_with(flags: &Flags, config: Option<&config::Config>) -> Result<PathBuf, String> {
    if let Some(root) = flags.named.get("root") {
        return guarded(Path::new(root));
    }
    // A config names the tree a run is rooted in, so a daemon started with no arguments has a working
    // directory before anyone is there to pass one.
    if let Some(config) = config {
        return guarded(&config.root);
    }
    std::env::current_dir()
        .map_err(|error| format!("no working directory: {error}"))
        .and_then(|dir| guarded(&dir))
}

/// The config a command reads: `--config` when given, otherwise the one under the Hades home.
fn config_of(flags: &Flags) -> Result<Option<config::Config>, String> {
    config::load(&config_path(flags)?)
}

fn config_path(flags: &Flags) -> Result<PathBuf, String> {
    match flags.named.get("config") {
        Some(path) => guarded(Path::new(path)),
        None => Ok(config::path_in(&hades_home()?)),
    }
}

/// Where Hades keeps its own things, and never the live install's.
///
/// `HADES_HOME` exists for two callers: an installer that is staging a home before it moves it, and a
/// test that must not see the home of the machine it is running on. An empty value is not a home.
fn hades_home() -> Result<PathBuf, String> {
    match std::env::var("HADES_HOME") {
        Ok(dir) if !dir.is_empty() => Ok(PathBuf::from(dir)),
        _ => {
            let home = std::env::var("HOME")
                .map_err(|_| "no HOME and no HADES_HOME, so pass an explicit path".to_string())?;
            Ok(PathBuf::from(home).join(".hades"))
        }
    }
}

/// Every directory a command is about to touch goes through here first.
///
/// One function rather than a check per command: a guard that has to be remembered at each call site is a
/// guard that will be forgotten at the newest one.
fn guarded(path: &Path) -> Result<PathBuf, String> {
    let home =
        std::env::var("HOME").map_err(|_| "no HOME, so pass an explicit path".to_string())?;
    let home = PathBuf::from(home);
    if let Some(reason) = boundary::foreign(path, &home) {
        return Err(boundary::refusal(path, reason));
    }
    Ok(path.to_path_buf())
}

/// The state directory, which is Hades' own by default and never inside the live install.
fn state_of(flags: &Flags) -> Result<PathBuf, String> {
    match flags.named.get("state") {
        Some(dir) => guarded(Path::new(dir)),
        None => default_state(),
    }
}

/// The harness this build would load, where it came from, and how large it is.
fn harness_cmd(args: &[String]) -> Result<(String, i32), String> {
    let flags = parse(args, &["profile", "harness"])?;
    if let Some(dir) = flags.named.get("harness") {
        let files = crate::harness_files::Files::load(Some(dir), "explicit")?;
        return Ok((files.describe("explicit"), 0));
    }
    if let Some(id) = flags.named.get("profile") {
        if profile::named(id).is_none() {
            return Err(format!("no profile named `{id}`"));
        }
        let files = crate::harness_files::Files::load(None, id)?;
        return Ok((files.describe(id), 0));
    }

    let mut text = format!("built-in system prompt: {} bytes\n", model::SYSTEM.len());
    let dir = hades_home()?.join("harness");
    let mut names = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                names.push(entry.file_name().to_string_lossy().to_string());
            }
        }
    }
    names.sort();
    if names.is_empty() {
        text.push_str(&format!(
            "no harness directories under {}; every run uses the built-in prompt\n",
            dir.display()
        ));
    } else {
        text.push_str(&format!("harness directories under {}:\n", dir.display()));
        for name in names {
            let files = crate::harness_files::Files::load(None, &name)?;
            text.push_str(&format!(
                "  {:<12} system {}, guidance {}\n",
                name,
                files
                    .system_path
                    .as_ref()
                    .map(|_| format!("{} bytes", files.system.len()))
                    .unwrap_or_else(|| "built-in".to_string()),
                files
                    .guidance_path
                    .as_ref()
                    .map(|_| format!("{} bytes", files.guidance.len()))
                    .unwrap_or_else(|| "none".to_string())
            ));
        }
    }
    Ok((text, 0))
}

/// One probe: run the same goal under one harness directory, in a state of its own, and read the session
/// back as a receipt. The child is this binary, so the probe exercises exactly the path a person runs.
/// Run the protected holdout in the same tree the probe left behind. A missing or broken holdout is
/// unmeasured, not a pass: `None` keeps the candidate adoptable only on the training check.
fn holdout_passes(flags: &Flags, command: &str) -> Option<bool> {
    let config = config_of(flags).ok().flatten();
    let root = root_with(flags, config.as_ref()).ok()?;
    crate::command::run(command, &root, 8_000, None)
        .ok()
        .map(|run| run.passed())
}

fn probe(
    flags: &Flags,
    goal: &str,
    harness: Option<&std::path::Path>,
    state: &std::path::Path,
    label: &str,
) -> Result<crate::tuning::Receipt, String> {
    let exe = std::env::current_exe()
        .map_err(|error| format!("could not find this binary to probe with: {error}"))?;
    let mut command = std::process::Command::new(exe);
    command.arg("run").arg(goal);
    for (name, value) in &flags.named {
        if matches!(
            name.as_str(),
            "harness"
                | "candidate"
                | "program"
                | "session"
                | "state"
                | "adopt"
                | "holdout"
                | "case"
                | "attack"
                | "auto"
                | "regress"
                | "no-regress"
                | "check-minutes"
        ) {
            continue;
        }
        if name == "config" {
            let path = std::fs::canonicalize(value).unwrap_or_else(|_| PathBuf::from(value));
            command.arg("--config").arg(path);
        } else if value == "true" {
            // A switch is forwarded by name. Forwarding `--auto true` gives the child a flag it does not take
            // or a stray positional goal; a switch this child does not know is not forwarded at all.
            if matches!(name.as_str(), "strict-model") {
                command.arg(format!("--{name}"));
            }
        } else {
            command.arg(format!("--{name}")).arg(value);
        }
    }
    if let Some(programs) = flags.repeated.get("program") {
        for program in programs {
            command.arg("--program").arg(program);
        }
    }
    if !flags.named.contains_key("minutes") {
        command.arg("--minutes").arg("3");
    }
    // A tune measures what a harness does with a model. A check that is already green would let the probe
    // pass without asking one, which is a measurement of nothing: the gate is for a fix, not for a probe.
    command.arg("--baseline").arg("off");
    // A probe is a measurement, not a failure of the real tree: recording a case from it would pin a tree
    // whose goal is the probe's and whose check is the tuning check. Fixed, not inherited.
    command.arg("--case").arg("off");
    if let Some(harness) = harness {
        command.arg("--harness").arg(harness);
    }
    command
        .arg("--state")
        .arg(state)
        .arg("--session")
        .arg(label);
    let output = command
        .output()
        .map_err(|error| format!("could not start a probe: {error}"))?;

    let log = state.join("sessions").join(format!("{label}.jsonl"));
    if !log.is_file() {
        let said = String::from_utf8_lossy(&output.stderr);
        let tail: String = said
            .chars()
            .rev()
            .take(600)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        return Err(format!(
            "probe `{label}` wrote no session; the run said: {}",
            tail.trim()
        ));
    }
    let mut receipt = crate::tuning::receipt(&log);
    if receipt.awaiting && receipt.check.is_empty() {
        // The probe parked on the hard floor: nobody was there to answer, so the arm was never measured.
        // Calling that `exit 2` reads as "every candidate failed", which is a wrong conclusion to draw
        // from a question that was never answered.
        receipt.check = "awaiting (approval)".to_string();
        receipt.passed = false;
    } else if receipt.check.is_empty() {
        receipt.check = format!("exit {}", output.status.code().unwrap_or(-1));
        receipt.passed = output.status.success();
    }
    Ok(receipt)
}

/// Why a corpus replay blocks adoption, by exit code. `None` is a clean corpus. A corpus that could not run
/// is not a pass: an unverified candidate is refused for the same reason a regressing one is.
fn gate_refusal(code: i32) -> Option<&'static str> {
    match code {
        0 => None,
        2 => Some(
            "the candidate was not adopted: the corpus could not be replayed, so nothing proves it is safe",
        ),
        _ => Some(
            "the candidate was not adopted: a harness that fixes the target and breaks a pinned case is not an improvement",
        ),
    }
}

/// Propose, probe, compose. With `--candidate`, the proposals are directories and no model is asked; without
/// it, two candidates are edited from a reference run and then probed against the same check.
fn tune(args: &[String]) -> Result<(String, i32), String> {
    let flags = parse_with(
        args,
        &[
            "root",
            "profile",
            "policy",
            "verify",
            "holdout",
            "state",
            "session",
            "cage",
            "program",
            "model",
            "endpoint",
            "key",
            "temperature",
            "max-tokens",
            "context",
            "thinking",
            "attempts",
            "minutes",
            "shadow",
            "config",
            "memory",
            "harness",
            "candidate",
        ],
        &[
            "adopt",
            "auto",
            "regress",
            "no-regress",
            "include-partial",
            "include-stale",
        ],
    )?;
    let goal = goal(&flags)?;
    let config = config_of(&flags)?;
    let root = root_with(&flags, config.as_ref())?;
    let (routed, _) = Rules::shipped().pick(&TaskSpec::new(goal.clone(), root.clone()));
    let profile = match flags.named.get("profile") {
        Some(id) => profile::named(id).ok_or_else(|| format!("no profile named `{id}`"))?,
        None => profile::named(routed.as_str())
            .ok_or_else(|| format!("the rules picked {routed}, which this build does not ship"))?,
    };
    let check = flags
        .named
        .get("verify")
        .cloned()
        .unwrap_or_else(|| profile.verify.clone());
    let holdout = flags.named.get("holdout").cloned();
    let base = crate::harness_files::Files::load(
        flags.named.get("harness").map(String::as_str),
        profile.id.as_str(),
    )?;
    let state = state_of(&flags)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|waited| waited.as_millis())
        .unwrap_or(0);
    let work = state.join("tune").join(stamp.to_string());
    std::fs::create_dir_all(&work)
        .map_err(|error| format!("could not make {}: {error}", work.display()))?;

    // The reference run. Its transcript is what an editor sees, with the check withheld.
    let base_label = format!("tune_{stamp}_base");
    let base_state = work.join("base").join("state");
    let mut base_receipt = probe(&flags, &goal, base.dir.as_deref(), &base_state, &base_label)?;
    if let Some(command) = &holdout {
        base_receipt.holdout = holdout_passes(&flags, command);
    }
    let base_log = base_state
        .join("sessions")
        .join(format!("{base_label}.jsonl"));
    let said = crate::tuning::transcript(&base_log, &check, 6_000);
    let mut rows = vec![crate::tuning::Row {
        name: "base".to_string(),
        receipt: base_receipt.clone(),
        note: format!("reference; {}", base.line()),
    }];
    let mut lessons: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    let mut dirs: Vec<(String, String)> = Vec::new();
    let mut sources: std::collections::BTreeMap<String, std::path::PathBuf> =
        std::collections::BTreeMap::new();
    // The probe session each candidate was measured in, so an adoption can name the evidence it came from.
    let mut labels: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();

    let given: Vec<String> = flags.repeated.get("candidate").cloned().unwrap_or_default();
    let mut candidates: Vec<(String, std::path::PathBuf, String)> = Vec::new();

    if given.is_empty() {
        let answerer = answerer(&flags, config.as_ref(), Some(&base), None)?;
        let open = match answerer {
            Answerer::Model(open) => open,
            Answerer::Scripted(_) => {
                return Err(
                    "tuning without --candidate needs a model to edit the harness".to_string(),
                );
            }
        };
        let guidance_text = if base.guidance.trim().is_empty() {
            "(none)".to_string()
        } else {
            clip_text(&base.guidance, 2_000)
        };
        let tasks = [
            (
                "invariant",
                "Candidate A, invariant: fix one agentic deficiency visible in the transcript: planning, \
                 tool use, context management, recovery from an error, or delegation. Do not add domain \
                 knowledge.",
            ),
            (
                "domain",
                "Candidate B, domain: fix one task-specific gap visible in the transcript: a piece of \
                 knowledge or a procedure this task needs and the prompt does not give. Do not restate \
                 general agent advice.",
            ),
        ];
        for (name, instruction) in tasks {
            let user = format!(
                "The harness around a frozen coding model is being edited.\n\n\
                 Current system prompt:\n{}\n\n\
                 Current tool guidance:\n{}\n\n\
                 One run under this harness, with the check's program withheld:\n{}\n\n\
                 Goal: {}\n\n{}\n\n\
                 Address one concrete failure visible above. You may rewrite the system prompt and the tool \
                 guidance. Never mention a check, test, grader, expected answer, score, or rubric. Do not add \
                 tools. Keep the system prompt under 2,000 characters.\n\n\
                 Reply with one JSON object and nothing else: \
                 {{\"reason\":\"...\",\"system\":\"...\",\"guidance\":\"...\",\"lesson\":\"...\"}}",
                clip_text(&base.system, 4_000),
                guidance_text,
                said,
                goal,
                instruction
            );
            let messages = serde_json::json!([
                { "role": "system", "content": "You edit the text around a coding model. You answer with one JSON object." },
                { "role": "user", "content": user },
            ]);
            let reply = open
                .chat(messages.as_array().expect("an array"), None, &|_, _| {})
                .map_err(|fault| {
                    format!(
                        "the editor ended in a {} fault: {}",
                        fault.class(),
                        fault.detail()
                    )
                })?;
            let (system, guidance, reason, lesson) =
                crate::tuning::candidate_from(&reply.content, &check)?;
            let dir = work.join(name);
            std::fs::create_dir_all(&dir)
                .map_err(|error| format!("could not make {}: {error}", dir.display()))?;
            std::fs::write(dir.join("system.md"), &system)
                .map_err(|error| format!("could not write the candidate: {error}"))?;
            if !guidance.is_empty() {
                std::fs::write(dir.join("tool_guidance.md"), &guidance)
                    .map_err(|error| format!("could not write the guidance: {error}"))?;
            }
            lessons.insert(name.to_string(), lesson);
            sources.insert(name.to_string(), dir.clone());
            dirs.push((name.to_string(), dir.display().to_string()));
            candidates.push((name.to_string(), dir, reason));
        }
    } else {
        for (index, dir) in given.iter().enumerate() {
            let files = crate::harness_files::Files::load(Some(dir), "explicit")?;
            let found = std::path::Path::new(dir)
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| format!("candidate-{}", index + 1));
            // `base` is the reference row's name; a candidate directory called base would collide with it
            // in the table and be skipped by the choice.
            let name = if found == "base" {
                format!("candidate-{}", index + 1)
            } else {
                found
            };
            dirs.push((name.clone(), dir.clone()));
            sources.insert(name.clone(), std::path::PathBuf::from(dir));
            candidates.push((name, std::path::PathBuf::from(dir), files.line()));
        }
    }

    // Refused before a probe runs, not after: a candidate that edits the guardrail is not a candidate whose
    // cost or pass rate is worth measuring.
    for (name, dir, _) in &candidates {
        crate::tuning::refuses(dir, &check)
            .map_err(|said| format!("candidate `{name}` is refused: {said}"))?;
    }

    for (index, (name, dir, reason)) in candidates.iter().enumerate() {
        let label = format!("tune_{stamp}_{}", slug_label(name));
        labels.insert(name.clone(), label.clone());
        let state = work.join(format!("probe-{}", index + 1)).join("state");
        let mut receipt = probe(&flags, &goal, Some(dir), &state, &label)?;
        if let Some(command) = &holdout {
            receipt.holdout = holdout_passes(&flags, command);
        }
        rows.push(crate::tuning::Row {
            name: name.clone(),
            receipt,
            note: reason.clone(),
        });
    }

    let pick = crate::tuning::choose(&rows);
    let mut adopted = match pick {
        Some(at) if crate::tuning::beats_base(&rows[0], &rows[at]) => Some(at),
        _ => None,
    };

    let mut adopted_line = "none (base kept)".to_string();
    let mut gate_line = String::new();
    let mut refused = false;
    if let Some(at) = adopted {
        let name = rows[at].name.clone();
        // `--auto` is the same decision with the person out of the loop for a candidate that already beat the
        // base on evidence: it still keeps a copy of what it replaced, so the change is one command to undo.
        if switched(&flags, "adopt") || switched(&flags, "auto") {
            let target = match &base.dir {
                Some(dir) => dir.clone(),
                None => hades_home()?.join("harness").join(profile.id.as_str()),
            };
            std::fs::create_dir_all(&target)
                .map_err(|error| format!("could not make {}: {error}", target.display()))?;
            let source = sources
                .get(&name)
                .ok_or_else(|| format!("no candidate directory recorded for `{name}`"))?;
            // The corpus gate runs before the copy, on the base and the selected candidate only: a
            // candidate compared to a stale base result is not a comparison. `--no-regress` is the one
            // way past it, and the receipt says the corpus was skipped rather than implying it passed.
            if switched(&flags, "no-regress") {
                gate_line = "corpus: skipped (--no-regress)\n".to_string();
            } else {
                let cases = crate::cases::load(&state, profile.id.as_str());
                if cases.is_empty() {
                    gate_line = "corpus: no cases for this profile\n".to_string();
                } else {
                    let ceiling: u32 = cases.iter().map(|case| case.bounds.attempts).sum();
                    gate_line = format!(
                        "corpus: {} case(s), 2 arms, up to {ceiling} model calls per arm\n",
                        cases.len()
                    );
                    let mut passthrough = Vec::new();
                    for key in [
                        "endpoint",
                        "key",
                        "temperature",
                        "max-tokens",
                        "context",
                        "program",
                    ] {
                        if let Some(value) = flags.named.get(key) {
                            passthrough.push(format!("--{key}"));
                            passthrough.push(value.clone());
                        }
                    }
                    let options = crate::eval::options_from(
                        Vec::new(),
                        vec![profile.id.as_str().to_string()],
                        Some(target.clone()),
                        Some(source.clone()),
                        flags.named.get("model").cloned(),
                        None,
                        None,
                        None,
                        switched(&flags, "include-partial"),
                        switched(&flags, "include-stale"),
                        false,
                        false,
                        passthrough,
                    );
                    let report = crate::eval::execute(&state, &options)?;
                    if let Some(why) = gate_refusal(report.code) {
                        refused = true;
                        gate_line.push_str(&report.text);
                        gate_line.push_str(&format!("\n{why}"));
                    } else {
                        gate_line.push_str("corpus: no regression\n");
                    }
                }
            }
            if refused {
                adopted_line =
                    format!("none (the corpus refused the candidate; {name} was not copied)");
                adopted = None;
            } else {
                // One call, so the copy and the number that makes it undoable cannot come apart. `--auto` takes
                // the same path: the decision is automatic, the record of it is not optional.
                let generation = crate::tuning::record_adoption(
                    &hades_home()?,
                    profile.id.as_str(),
                    &target,
                    &state,
                    source,
                    labels.get(&name).map(String::as_str).unwrap_or("tune"),
                    &rows[at].receipt.check,
                    &crate::tuning::guard_digest(),
                )?;
                adopted_line = match generation {
                    Some(generation) => format!(
                        "{name} (copied to {}; generation {} keeps what it replaced)",
                        target.display(),
                        generation.id
                    ),
                    None => format!(
                        "{name} (copied to {}; it was already the harness, so no generation was written)",
                        target.display()
                    ),
                };
            }
        } else {
            adopted_line = format!("{name} (not copied; pass --adopt)");
        }
    }

    let lesson = adopted
        .map(|at| lessons.get(&rows[at].name).cloned().unwrap_or_default())
        .unwrap_or_default();
    let library = hades_home()?
        .join("harness")
        .join("library")
        .join(format!("{stamp}-{}", crate::tuning::slug(&goal)));
    std::fs::create_dir_all(&library)
        .map_err(|error| format!("could not make {}: {error}", library.display()))?;
    let note = crate::tuning::experience(
        &goal,
        &check,
        &rows,
        adopted.map(|at| rows[at].name.as_str()),
        &lesson,
        &dirs,
    );
    std::fs::write(library.join("EXPERIENCE.md"), note)
        .map_err(|error| format!("could not write the library note: {error}"))?;

    let mut text = format!(
        "tune {stamp}: {goal}\ncheck: {}\n\n",
        if check.is_empty() {
            "none"
        } else {
            check.as_str()
        }
    );
    for row in &rows {
        text.push_str(&format!(
            "{:<10} {:<7} {:<7} {:>2} answer(s) {:>6} ms {:>6} bytes  {}\n",
            row.name,
            if row.receipt.awaiting {
                "asked"
            } else if row.receipt.passed {
                "passed"
            } else {
                "failed"
            },
            match row.receipt.holdout {
                Some(true) => "hold-ok",
                Some(false) => "hold-no",
                None => "-",
            },
            row.receipt.answers,
            row.receipt.millis,
            row.receipt.answer_bytes,
            if row.receipt.awaiting {
                "awaiting (approval)"
            } else {
                row.note.as_str()
            }
        ));
    }
    let parked = rows.iter().filter(|row| row.receipt.awaiting).count();
    if parked > 0 {
        text.push_str(&format!(
            "\nsuggestion: {parked} probe(s) stopped at an approval question; pass --policy open to \
             measure unattended.\n"
        ));
    }
    if !gate_line.is_empty() {
        text.push('\n');
        text.push_str(&gate_line);
    }
    text.push_str(&format!(
        "\nadopted: {adopted_line}\nlibrary: {}\n",
        library.display()
    ));
    Ok((text, if refused { 1 } else { 0 }))
}

/// The numbered generations of a profile's harness, and the one command that puts one back.
///
/// A generation is the harness an adoption replaced, so rolling back to `n` is undoing that adoption. The
/// files restored are exactly the ones the manifest names: a skill added after the generation is left where
/// it is, because undoing a harness edit is not deleting work done since.
fn refine(args: &[String]) -> Result<(String, i32), String> {
    let flags = parse(args, &["profile", "state", "harness", "rollback"])?;
    let home = hades_home()?;
    let state = state_of(&flags)?;
    let profile = match flags.named.get("profile") {
        Some(id) => id.clone(),
        None => sole_profile(&home)?,
    };
    let harness = match flags.named.get("harness") {
        Some(dir) => guarded(Path::new(dir))?,
        None => home.join("harness").join(&profile),
    };
    let Some(wanted) = flags.named.get("rollback") else {
        return Ok((generation_list(&home, &profile)?, 0));
    };
    let id: u64 = wanted
        .parse()
        .map_err(|_| format!("`--rollback {wanted}` is not a generation number"))?;
    let generation = crate::tuning::restore(
        &home,
        &profile,
        &harness,
        &state,
        id,
        &crate::tuning::guard_digest(),
    )?;
    let mut text = format!(
        "restored generation {} of `{profile}`: {} file(s) into {}\n",
        generation.id,
        generation.files.len(),
        harness.display()
    );
    for file in &generation.files {
        text.push_str(&format!("  {}\n", file.path));
    }
    Ok((text, 0))
}

fn generation_list(home: &Path, profile: &str) -> Result<String, String> {
    let generations = crate::tuning::generations(home, profile)?;
    let mut text = format!(
        "harness generations for `{profile}` under {}\n",
        crate::tuning::generations_root(home, profile).display()
    );
    if generations.is_empty() {
        text.push_str("  none; `hades tune --adopt` writes one for every harness it replaces\n");
        return Ok(text);
    }
    for generation in generations {
        text.push_str(&format!(
            "  {:<3} parent {:<4} {:>2} file(s)  check {:<12} {}\n",
            generation.id,
            generation
                .parent
                .map(|parent| parent.to_string())
                .unwrap_or_else(|| "-".to_string()),
            generation.files.len(),
            if generation.check.is_empty() {
                "not run"
            } else {
                generation.check.as_str()
            },
            generation.source_session
        ));
    }
    Ok(text)
}

/// The profile to work on when the caller named none: the only one that has generations. Guessing between
/// two is how the wrong harness gets restored.
fn sole_profile(home: &Path) -> Result<String, String> {
    let root = home.join("control").join("harness");
    let mut found = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&root) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if entry.path().is_dir() && !crate::tuning::generations(home, &name)?.is_empty() {
                found.push(name);
            }
        }
    }
    found.sort();
    match found.len() {
        1 => Ok(found.remove(0)),
        0 => Err(format!(
            "no harness generations under {}; `hades tune --adopt` writes one",
            root.display()
        )),
        _ => Err(format!(
            "several profiles have generations ({}): pass --profile",
            found.join(", ")
        )),
    }
}

fn slug_label(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

fn clip_text(text: &str, cap: usize) -> String {
    if text.len() <= cap {
        return text.to_string();
    }
    let mut end = cap;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[truncated]", &text[..end])
}

fn profiles(_args: &[String]) -> Result<(String, i32), String> {
    let mut text = String::new();
    text.push_str(
        "id      role    model                 effort  ctx     judge plan  edits shadow attempts minutes net  check\n",
    );
    for profile in profile::shipped() {
        text.push_str(&format!(
            "{:<7} {:<7} {:<21} {:<7} {:<7} {:<5} {:<5} {:<5} {:<6} {:<8} {:<7} {:<4} {}\n",
            profile.id.as_str(),
            profile.role.name(),
            profile.model.as_deref().unwrap_or("inherit"),
            profile.effort.name(),
            profile.context_tokens,
            profile.judge,
            profile.plan_first,
            profile.edits,
            profile.shadow,
            profile.attempts,
            profile.minutes,
            profile.network,
            profile.verify,
        ));
    }
    Ok((text, 0))
}

/// The failure corpus: replay the whole thing, or compare a candidate against the current harness.
fn eval_cmd(args: &[String]) -> Result<(String, i32), String> {
    let flags = parse_with(
        args,
        &[
            "state",
            "config",
            "profile",
            "case",
            "harness",
            "against",
            "model",
            "endpoint",
            "key",
            "temperature",
            "max-tokens",
            "context",
            "attempts",
            "minutes",
            "check-minutes",
            "program",
        ],
        &[
            "include-partial",
            "include-stale",
            "dry-run",
            "keep",
            "json",
        ],
    )?;
    let state = state_of(&flags)?;
    let number = |key: &str| -> Result<Option<u64>, String> {
        match flags.named.get(key) {
            Some(value) => {
                Ok(Some(value.parse::<u64>().map_err(|_| {
                    format!("`--{key} {value}` is not a number")
                })?))
            }
            None => Ok(None),
        }
    };
    let attempts = number("attempts")?.map(|value| value.max(1) as u32);
    let minutes = number("minutes")?;
    let check_minutes = number("check-minutes")?;
    // A scripted program is forwarded for the same reason the endpoint is: a replay is a real run, and a test
    // that cannot name a model should still be able to replay a case.
    let mut passthrough = Vec::new();
    if let Some(programs) = flags.repeated.get("program") {
        for program in programs {
            passthrough.push("--program".to_string());
            passthrough.push(program.clone());
        }
    }
    let options = crate::eval::options_from(
        flags.repeated.get("case").cloned().unwrap_or_default(),
        flags.repeated.get("profile").cloned().unwrap_or_default(),
        flags.named.get("harness").map(PathBuf::from),
        flags.named.get("against").map(PathBuf::from),
        flags.named.get("model").cloned(),
        attempts,
        minutes,
        check_minutes,
        switched(&flags, "include-partial"),
        switched(&flags, "include-stale"),
        switched(&flags, "dry-run"),
        switched(&flags, "keep"),
        passthrough,
    );
    let report = crate::eval::execute(&state, &options)?;
    if switched(&flags, "json") {
        return Ok((format!("{}\n", report.json), report.code));
    }
    Ok((report.text, report.code))
}

/// The failure corpus: what has been pinned, its status, and the two decisions on one case.
///
/// A case is a task with a starting tree, not a transcript. `hades eval` replays them; this surface only
/// reads the ledger and, for `forget`, retires one.
fn cases_cmd(args: &[String]) -> Result<(String, i32), String> {
    let flags = parse_with(args, &["state", "profile", "config"], &["json", "sized"])?;
    let state = state_of(&flags)?;
    let wanted: Vec<String> = match flags.named.get("profile") {
        Some(profile) => vec![crate::cases::safe_profile(profile)],
        None => crate::cases::profiles(&state),
    };
    if wanted.is_empty() {
        return Ok((
            format!(
                "no cases under {}: a giving-up run records one by default\n",
                crate::cases::cases_dir(&state).display()
            ),
            0,
        ));
    }
    let sized = switched(&flags, "sized");
    if switched(&flags, "json") {
        let all: Vec<serde_json::Value> = wanted
            .iter()
            .flat_map(|profile| crate::cases::load(&state, profile))
            .map(|case| {
                serde_json::json!({
                    "id": case.id,
                    "profile": case.profile,
                    "goal": case.goal,
                    "check": case.check,
                    "status": case.status,
                    "cause": case.cause,
                    "attempts_used": case.attempts_used,
                    "bounds": {
                        "attempts": case.bounds.attempts,
                        "minutes": case.bounds.minutes,
                        "budget_tokens": case.bounds.tokens,
                        "width": case.bounds.width,
                    },
                    "tree": case.tree,
                    "snapshot": case.snapshot,
                    "source": case.source,
                    "partial": case.partial,
                    "bytes": case.bytes,
                    "files": case.files,
                    "attack": case.attack,
                    "at": case.at,
                })
            })
            .collect();
        return Ok((format!("{}\n", serde_json::json!(all)), 0));
    }
    let mut text = String::new();
    for profile in &wanted {
        text.push_str(&crate::cases::table(&state, profile, sized));
        let (bytes, open) = crate::cases::size(&state, profile);
        text.push_str(&format!(
            "  {open} open, {bytes} bytes{} \n",
            if bytes > crate::cases::WARN_BYTES {
                format!(
                    " (past the {} MB warning; `hades case forget <id>` retires one)",
                    crate::cases::WARN_BYTES / (1024 * 1024)
                )
            } else {
                String::new()
            }
        ));
    }
    Ok((text, 0))
}

/// One case in full, or the decision to retire it.
fn case_cmd(args: &[String]) -> Result<(String, i32), String> {
    let flags = parse_with(args, &["state", "profile", "config"], &[])?;
    let state = state_of(&flags)?;
    let action = flags.positional.first().map(String::as_str).unwrap_or("");
    let (verb, id) = if action == "forget" {
        (
            "forget",
            flags
                .positional
                .get(1)
                .map(String::as_str)
                .ok_or_else(|| "forget needs a case id".to_string())?,
        )
    } else {
        ("show", action)
    };
    if id.is_empty() {
        return Err("say `hades case <id>` or `hades case forget <id>`".to_string());
    }
    let profile = match flags.named.get("profile") {
        Some(profile) => {
            let profile = crate::cases::safe_profile(profile);
            if crate::cases::find(&state, &profile, id)?.is_none() {
                return Err(format!("there is no case `{id}` under profile {profile}"));
            }
            profile
        }
        None => crate::cases::profiles(&state)
            .into_iter()
            .find(|profile| {
                crate::cases::find(&state, profile, id)
                    .ok()
                    .flatten()
                    .is_some()
            })
            .ok_or_else(|| format!("there is no case `{id}` in {}", state.display()))?,
    };
    if verb == "forget" {
        let case = crate::cases::forget(&state, &profile, id)?;
        return Ok((
            format!(
                "retired {} ({}); its tree is gone, the case line remains\n",
                case.id, case.goal
            ),
            0,
        ));
    }
    let case = crate::cases::find(&state, &profile, id)?
        .ok_or_else(|| format!("there is no case `{id}`"))?;
    let mut text = format!(
        "id:       {}\nprofile:  {}\nstatus:   {}\ngoal:     {}\ncheck:    {}\ncause:    {} after {} attempt(s)\nbounds:   {}\npolicy:   {}\nmodel:    {}\nsnapshot: {}\nsource:   {}\ntree:     {} ({} files, {} bytes)\npartial:  {} ({})\nignored:  {}\n",
        case.id,
        case.profile,
        case.status,
        case.goal,
        case.check,
        case.cause,
        case.attempts_used,
        case.bounds.line(),
        case.policy,
        case.model,
        case.snapshot,
        case.source,
        case.tree,
        case.files,
        case.bytes,
        case.partial,
        case.skipped_special,
        case.ignored.join(", ")
    );
    if let Some(ruler) = &case.ruler {
        text.push_str(&format!("ruler:    {}\n", ruler.get(..12).unwrap_or(ruler)));
    }
    if let Some(attack) = &case.attack {
        text.push_str(&format!("attack:   {attack}\n"));
    }
    text.push_str(&format!(
        "log:      {}\n",
        crate::cases::profile_dir(&state, &case.profile)
            .join(&case.id)
            .join("runs")
            .display()
    ));
    Ok((text, 0))
}

/// Proposals from finished work, and the two decisions a person can take on one. A proposal is never loaded
/// until it is promoted, because a skill is an instruction.
fn skills_cmd(args: &[String]) -> Result<(String, i32), String> {
    let flags = parse_with(
        args,
        &[
            "state",
            "profile",
            "model",
            "endpoint",
            "key",
            "temperature",
            "max-tokens",
            "context",
        ],
        &["force"],
    )?;
    let state = state_of(&flags)?;
    let action = flags
        .positional
        .first()
        .map(String::as_str)
        .unwrap_or("list");
    match action {
        "list" => {
            let proposed = crate::skills::proposed(&state);
            let installed = crate::skills::installed(&state);
            let builtin = crate::skills::builtin(&state);
            let mut text = format!(
                "built-in under {} ({}):\n",
                crate::skills::builtin_dir(&state).display(),
                builtin.len()
            );
            if builtin.is_empty() {
                text.push_str("  none (run `hades init` to seed them)\n");
            }
            for skill in &builtin {
                text.push_str(&format!("  {}\n", skill.name));
            }
            text.push_str(&format!(
                "proposed under {} ({}):\n",
                crate::skills::proposed_dir(&state).display(),
                proposed.len()
            ));
            if proposed.is_empty() {
                text.push_str("  none\n");
            }
            for skill in &proposed {
                text.push_str(&format!("  {}\n", skill.name));
            }
            text.push_str(&format!(
                "installed under {} ({}):\n",
                crate::skills::installed_dir(&state).display(),
                installed.len()
            ));
            if installed.is_empty() {
                text.push_str("  none\n");
            }
            for skill in &installed {
                text.push_str(&format!("  {}\n", skill.name));
            }
            Ok((text, 0))
        }
        "seed" => {
            let written = crate::builtin::write_all(&state)?;
            Ok((
                format!(
                    "seeded {written} built-in skill(s) into {}\n",
                    crate::skills::builtin_dir(&state).display()
                ),
                0,
            ))
        }
        "promote" => {
            let name = flags
                .positional
                .get(1)
                .ok_or_else(|| "promote needs a proposal name".to_string())?;
            let path = crate::skills::promote_with(&state, name, switched(&flags, "force"))?;
            Ok((format!("promoted: {}\n", path.display()), 0))
        }
        "verify" => skills_verify(&state, &flags),
        "report" => Ok((skills_report(&state), 0)),
        "reject" => {
            let name = flags
                .positional
                .get(1)
                .ok_or_else(|| "reject needs a proposal name".to_string())?;
            crate::skills::reject(&state, name)?;
            Ok((format!("rejected: {name}\n"), 0))
        }
        other => Err(format!(
            "`skills {other}`: say list, seed, promote <name>, reject <name>, verify <name> or report"
        )),
    }
}

/// Run one proposal in a throwaway state, over the cases it names. The live `skills/` is never touched: the
/// proposal is installed in a copy, the corpus is copied beside it, and the receipt records what passed.
fn skills_verify(state: &Path, flags: &Flags) -> Result<(String, i32), String> {
    let name = flags
        .positional
        .get(1)
        .ok_or_else(|| "verify needs a proposal name".to_string())?;
    let path = crate::skills::proposed_dir(state).join(format!("{name}.md"));
    let text = std::fs::read_to_string(&path)
        .map_err(|_| format!("there is no proposal called `{name}`"))?;
    let evidence = crate::skills::evidence(&text);
    if evidence.regression.len() < 2 || evidence.holdout.is_empty() {
        return Err(format!(
            "`{name}` names {} regression case(s) and {} holdout; verify needs at least two regressions and one holdout",
            evidence.regression.len(),
            evidence.holdout.len()
        ));
    }
    let profile = flags
        .named
        .get("profile")
        .cloned()
        .or_else(|| crate::skills::proposal_profile(&text))
        .ok_or_else(|| format!("`{name}` names no profile; pass --profile"))?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0);
    let thrown = state
        .join("skills")
        .join("verify")
        .join(format!("{name}-{stamp}"));
    // The throwaway state is the real state minus the proposal: the corpus is what is being tested, and a
    // copy is the only way to install a procedure without it becoming live.
    crate::scratch::state_dir(&thrown)
        .map_err(|error| format!("could not make {}: {error}", thrown.display()))?;
    if state.join("cases").is_dir() {
        crate::cases::copy_dir(&state.join("cases"), &thrown.join("cases"))?;
    }
    let active = thrown.join("skills").join(name);
    crate::scratch::state_dir(&active)
        .map_err(|error| format!("could not make {}: {error}", active.display()))?;
    std::fs::write(
        active.join("SKILL.md"),
        text.replace("status: proposed", "status: active"),
    )
    .map_err(|error| format!("could not write the throwaway skill: {error}"))?;
    let mut passthrough = Vec::new();
    for key in ["endpoint", "key", "temperature", "max-tokens", "context"] {
        if let Some(value) = flags.named.get(key) {
            passthrough.push(format!("--{key}"));
            passthrough.push(value.clone());
        }
    }
    let mut cases = evidence.regression.clone();
    cases.extend(evidence.holdout.clone());
    let options = crate::eval::options_from(
        cases.clone(),
        vec![profile.clone()],
        None,
        None,
        flags.named.get("model").cloned(),
        None,
        None,
        None,
        false,
        false,
        false,
        false,
        passthrough,
    );
    let report = crate::eval::execute(&thrown, &options)?;
    let passed = report.code == 0;
    let receipt = serde_json::json!({
        "name": name,
        "profile": profile,
        "cases": cases,
        "passed": passed,
        "code": report.code,
        "at": stamp,
        "text": report.text,
    });
    let receipt_path = crate::skills::verification_path(state, name);
    std::fs::write(&receipt_path, format!("{receipt}\n"))
        .map_err(|error| format!("could not write {}: {error}", receipt_path.display()))?;
    let mut out = report.text;
    out.push_str(&format!(
        "\nverified: {} ({})\nreceipt: {}\n",
        if passed { "passed" } else { "failed" },
        if evidence.contract.as_deref().unwrap_or("TODO") == "TODO" {
            "the contract is still TODO"
        } else {
            "the contract is filled in"
        },
        receipt_path.display()
    ));
    Ok((out, report.code))
}

/// Library hygiene, two numbers and no score: are proposals generalizing, and are installed skills still
/// correct? A number with no measurement behind it is worse than no number, so each is reported with its
/// denominator or with the reason it cannot be computed.
fn skills_report(state: &Path) -> String {
    let records = crate::scoreboard::load(state);
    let standings = crate::scoreboard::standings(&records);
    let accepted: usize = standings.iter().map(|standing| standing.passes).sum();
    let proposals = crate::skills::proposed(state);
    let proposals_text = if accepted == 0 {
        "new proposals: unmeasured (no accepted runs in the scoreboard)".to_string()
    } else {
        format!(
            "new proposals: {} over {} accepted run(s) = {:.2} per 20",
            proposals.len(),
            accepted,
            proposals.len() as f64 / accepted as f64 * 20.0
        )
    };
    let mut cases_by_profile: std::collections::BTreeMap<String, Vec<crate::cases::Case>> =
        std::collections::BTreeMap::new();
    for profile in crate::cases::profiles(state) {
        cases_by_profile.insert(profile.clone(), crate::cases::load(state, &profile));
    }
    let mut lines = Vec::new();
    for skill in crate::skills::installed(state) {
        let Ok(text) = std::fs::read_to_string(&skill.path) else {
            continue;
        };
        let evidence = crate::skills::evidence(&text);
        if evidence.regression.is_empty() {
            continue;
        }
        let profile = crate::skills::proposal_profile(&text).unwrap_or_default();
        let corpus = cases_by_profile.get(&profile).cloned().unwrap_or_default();
        let mut passed = 0;
        let mut measured = 0;
        for id in &evidence.regression {
            let Some(case) = corpus.iter().find(|case| &case.id == id) else {
                continue;
            };
            measured += 1;
            if case.status == "pass" || case.status == "fixed" {
                passed += 1;
            }
        }
        if measured == 0 {
            lines.push(format!(
                "{:<24} regression: unmeasured (none of its cases are in the corpus)",
                skill.name
            ));
        } else {
            lines.push(format!(
                "{:<24} regression: {passed}/{measured} ({:.0}%)",
                skill.name,
                passed as f64 / measured as f64 * 100.0
            ));
        }
    }
    let mut text = format!("{proposals_text}\n");
    if lines.is_empty() {
        text.push_str("installed skills: none with a regression set\n");
    } else {
        text.push_str("installed skills with a regression set:\n");
        for line in lines {
            text.push_str(&format!("  {line}\n"));
        }
    }
    text
}

/// What is running, and installing a build over it when one is named. A copy is written beside the target
/// and renamed onto it, so a reader never sees half a binary; restarting the units is left explicit because
/// an update that silently restarts a bot is an update that drops a message.
fn update(args: &[String]) -> Result<(String, i32), String> {
    let flags = parse(args, &["state", "from"])?;
    let current = crate::doctor::running_binary();
    let mut text = format!(
        "binary: {}\nversion: {}\n",
        current.display(),
        env!("CARGO_PKG_VERSION")
    );
    match flags.named.get("from") {
        None => {
            text.push_str("pass --from <path> to install a build over this binary\n");
            text.push_str("restart after: systemctl --user restart hades-daemon hades-gateway\n");
            Ok((text, 0))
        }
        Some(from) => {
            let source = std::path::PathBuf::from(from);
            if !source.is_file() {
                return Err(format!("{} is not a file", source.display()));
            }
            let beside = current.with_extension("new");
            std::fs::copy(&source, &beside)
                .map_err(|error| format!("could not stage {}: {error}", beside.display()))?;
            std::fs::rename(&beside, &current)
                .map_err(|error| format!("could not replace {}: {error}", current.display()))?;
            text.push_str(&format!("installed: {}\n", source.display()));
            text.push_str("restart: systemctl --user restart hades-daemon hades-gateway\n");
            Ok((text, 0))
        }
    }
}

/// What this install is, checked without a model. Failures set the exit code; warnings do not, because a
/// fresh home is allowed to be incomplete.
fn doctor(args: &[String]) -> Result<(String, i32), String> {
    let flags = parse(args, &["state", "config", "services"])?;
    let home = state_of(&flags)?;
    let config = config_path(&flags)?;
    let found = crate::doctor::check(
        &home,
        &config,
        &crate::doctor::running_binary(),
        switched(&flags, "services"),
    );
    Ok(crate::doctor::render(&found))
}

/// The config a headless process would read, printed field by field. It holds no secrets by construction;
/// the note under it says where they do live, so nobody goes looking for a key in this file.
fn config_cmd(args: &[String]) -> Result<(String, i32), String> {
    let flags = parse(args, &["state", "config"])?;
    let path = config_path(&flags)?;
    let mut text = format!("config: {}\n", path.display());
    match config::load(&path) {
        Ok(Some(config)) => {
            text.push_str(&format!("provider: {}\n", config.provider));
            text.push_str(&format!("model: {}\n", config.model));
            text.push_str(&format!("root: {}\n", config.root.display()));
            text.push_str(&format!("vault: {}\n", config.vault.display()));
            text.push_str(&format!("mcp servers: {}\n", config.mcp.servers.len()));
            text.push_str("\nkeys and tokens live in .env, never in this file\n");
            Ok((text, 0))
        }
        Ok(None) => {
            text.push_str("no config file; `hades init` writes one\n");
            Ok((text, 0))
        }
        Err(said) => Ok((format!("{text}{said}\n"), 1)),
    }
}

/// `session list` and `session show <id>`: reading the log a person is about to debug, without a daemon.
fn explain(args: &[String]) -> Result<(String, i32), String> {
    let flags = parse(args, &["state"])?;
    let session = flags
        .positional
        .first()
        .ok_or_else(|| "explain needs a session id".to_string())?;
    let state = state_of(&flags)?;
    let facts = crate::distill::load(&state, session)?;
    let mut text = crate::distill::turn_table(&facts);
    // How it ended, in the walk's own terms. The cause is printed as the word the walk recorded rather than
    // inferred from the sentence: a run that ran out of tokens must not be read as a run that ran out of
    // attempts.
    if let Some(node) = &facts.ended {
        text.push_str(&format!("\nended: {node}\n"));
    }
    if let Some(cause) = &facts.exhausted {
        text.push_str(&format!("cause: {cause}\n"));
    }
    if let Some(reason) = &facts.reason {
        text.push_str(&format!("why: {reason}\n"));
    }
    // What the log supports, stated rather than left for a reader to infer from the exit code. A run
    // whose check passed and whose judge said `not_yet` is accepted; this line is where that shows.
    let claim = crate::claim::ladder(&facts.claim());
    text.push_str(&format!("\n{}\n", claim.line()));
    text.push_str(&format!(
        "\nroot: {}\nsnapshot: {}\n",
        facts.root.display(),
        facts
            .snapshot
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "none".to_string())
    ));
    Ok((text, 0))
}

fn distill(args: &[String]) -> Result<(String, i32), String> {
    let flags = parse_with(
        args,
        &[
            "state",
            "config",
            "model",
            "endpoint",
            "key",
            "temperature",
            "max-tokens",
            "context",
            "thinking",
            "profile",
            "policy",
            "minutes",
        ],
        &["run"],
    )?;
    let session = flags
        .positional
        .first()
        .ok_or_else(|| "distill needs a session id".to_string())?;
    let state = state_of(&flags)?;
    let facts = crate::distill::load(&state, session)?;
    if !facts.accepted {
        return Err(format!(
            "session `{session}` did not finish accepted (check {}); only a verified run can be distilled",
            if facts.check_state.is_empty() {
                "never ran"
            } else {
                facts.check_state.as_str()
            }
        ));
    }
    if facts.check.trim().is_empty() {
        return Err(
            "the session has no check command, so a replay could not be verified".to_string(),
        );
    }
    if facts.snapshot.is_none() {
        return Err("the session has no first snapshot, so a replay has no clean tree".to_string());
    }
    let config = config_of(&flags)?;
    let answerer = answerer(&flags, config.as_ref(), None, None)?;
    let open = match answerer {
        Answerer::Model(open) => open,
        Answerer::Scripted(_) => {
            return Err("distill needs a model to write the runbook".to_string());
        }
    };
    let (name, summary, runbook) = crate::distill::plan(&open, &facts)?;
    crate::distill::gate(&name, &runbook, &facts.check)?;
    let path = crate::distill::write_runbook(&state, &name, &summary, &runbook, &facts)?;
    let mut text = format!(
        "runbook: {}\nname: {name}\nsummary: {summary}\n",
        path.display()
    );
    text.push_str(&crate::distill::turn_table(&facts));
    if !switched(&flags, "run") {
        text.push_str("\ndraft only; pass --run to prove it in a fresh tree before it can become a proposal\n");
        return Ok((text, 0));
    }
    let (code, replay_text, replay_session) =
        replay_fresh(&flags, &state, &name, &runbook, &facts)?;
    text.push_str(&format!(
        "\nreplay: {}\n{}\n",
        replay_session.display(),
        replay_text
    ));
    if code != 0 {
        text.push_str("\nreplay failed; no proposal was written\n");
        return Ok((text, 1));
    }
    crate::distill::mark_verified(&path, &replay_session.display().to_string())?;
    let proposal = crate::skills::propose_runbook(
        &state,
        &name,
        &summary,
        &runbook,
        &facts.session,
        &replay_session.display().to_string(),
        &facts.profile,
        &facts.check,
    )?;
    text.push_str(&format!("proposal: {}\n", proposal.display()));
    Ok((text, 0))
}

fn replay(args: &[String]) -> Result<(String, i32), String> {
    let flags = parse(
        args,
        &[
            "state",
            "check",
            "root",
            "config",
            "model",
            "endpoint",
            "key",
            "temperature",
            "max-tokens",
            "context",
            "thinking",
            "profile",
            "policy",
            "minutes",
        ],
    )?;
    let path = flags
        .positional
        .first()
        .ok_or_else(|| "replay needs a runbook path".to_string())?;
    let runbook = read_runbook(Path::new(path))?;
    let check = flags
        .named
        .get("check")
        .ok_or_else(|| {
            "replay needs --check <cmd>; the check is deliberately not stored in the runbook"
                .to_string()
        })?
        .clone();
    let root = flags
        .named
        .get("root")
        .map(PathBuf::from)
        .ok_or_else(|| "replay needs --root <dir>, a clean tree for the run".to_string())?;
    let state = state_of(&flags)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis())
        .unwrap_or(0);
    let replay_state = state.join("replay").join(stamp.to_string());
    let label = format!("replay-{stamp}");
    let goal = format!("Follow this verified runbook to complete the task.\n\n{runbook}");
    let profile = flags.named.get("profile").map(String::as_str);
    let (code, text, session) =
        run_replay(&flags, &root, &replay_state, &label, &goal, &check, profile)?;
    Ok((format!("{}\nsession: {}\n", text, session.display()), code))
}

fn replay_fresh(
    flags: &Flags,
    state: &Path,
    name: &str,
    runbook: &str,
    facts: &crate::distill::Facts,
) -> Result<(i32, String, PathBuf), String> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis())
        .unwrap_or(0);
    let base = state.join("runbooks").join(name);
    let root = base.join(format!("replay-{stamp}"));
    crate::distill::copy_snapshot(facts, &root)?;
    let replay_state = base.join(format!("replay-{stamp}-state"));
    let label = format!("replay-{name}-{stamp}");
    let goal = format!("Follow this verified runbook to complete the task.\n\n{runbook}");
    let profile = flags
        .named
        .get("profile")
        .map(String::as_str)
        .unwrap_or(facts.profile.as_str());
    run_replay(
        flags,
        &root,
        &replay_state,
        &label,
        &goal,
        &facts.check,
        Some(profile),
    )
}

fn run_replay(
    flags: &Flags,
    root: &Path,
    state: &Path,
    label: &str,
    goal: &str,
    check: &str,
    profile: Option<&str>,
) -> Result<(i32, String, PathBuf), String> {
    let exe = std::env::current_exe()
        .map_err(|error| format!("could not find this binary to replay with: {error}"))?;
    let mut command = std::process::Command::new(exe);
    command.arg("run").arg(goal);
    for (name, value) in &flags.named {
        if matches!(
            name.as_str(),
            "state"
                | "session"
                | "root"
                | "verify"
                | "check"
                | "holdout"
                | "profile"
                | "run"
                | "candidate"
                | "program"
                | "case"
                | "attack"
                | "auto"
                | "regress"
                | "no-regress"
                | "check-minutes"
        ) {
            continue;
        }
        if name == "config" {
            let path = std::fs::canonicalize(value).unwrap_or_else(|_| PathBuf::from(value));
            command.arg("--config").arg(path);
        } else if value == "true" {
            if matches!(name.as_str(), "strict-model") {
                command.arg(format!("--{name}"));
            }
        } else {
            command.arg(format!("--{name}")).arg(value);
        }
    }
    command
        .arg("--root")
        .arg(root)
        .arg("--state")
        .arg(state)
        .arg("--session")
        .arg(label)
        .arg("--verify")
        .arg(check)
        // A replay is evidence for a runbook, not a failure of the tree the runbook came from. A case here
        // would pin a copy whose goal is the runbook text.
        .arg("--case")
        .arg("off");
    if let Some(profile) = profile.filter(|profile| !profile.is_empty()) {
        command.arg("--profile").arg(profile);
    }
    if !flags.named.contains_key("policy") {
        // A replay runs in a throwaway copy of a snapshot. A policy prompt would stop it with nobody to
        // answer, which measures the approval broker instead of the runbook.
        command.arg("--policy").arg("open");
    }
    if !flags.named.contains_key("minutes") {
        command.arg("--minutes").arg("5");
    }
    let output = command
        .output()
        .map_err(|error| format!("could not start the replay: {error}"))?;
    let log = state.join("sessions").join(format!("{label}.jsonl"));
    if !log.is_file() {
        let said = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "the replay wrote no session; it said: {}",
            clip_text(said.trim(), 600)
        ));
    }
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.trim().is_empty() {
        text.push_str(&format!("\n{}", stderr.trim()));
    }
    Ok((
        output.status.code().unwrap_or(-1),
        clip_text(text.trim(), 4_000),
        log,
    ))
}

fn read_runbook(path: &Path) -> Result<String, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let body = match text.strip_prefix("---") {
        Some(rest) => match rest.split_once("\n---") {
            Some((_, after)) => after.trim_start(),
            None => return Err(format!("{} has no closing frontmatter", path.display())),
        },
        None => text.as_str(),
    };
    if body.trim().is_empty() {
        return Err(format!("{} has no runbook body", path.display()));
    }
    Ok(body.trim().to_string())
}

fn session_cmd(args: &[String]) -> Result<(String, i32), String> {
    let flags = parse(args, &["state", "session"])?;
    let home = state_of(&flags)?;
    let dir = home.join("sessions");
    let action = flags
        .positional
        .first()
        .map(String::as_str)
        .unwrap_or("list");
    match action {
        "list" => {
            let mut rows: Vec<(String, usize, u64)> = Vec::new();
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().is_none_or(|kind| kind != "jsonl") {
                        continue;
                    }
                    let count = std::fs::read_to_string(&path)
                        .map(|text| text.lines().count())
                        .unwrap_or(0);
                    let at = entry
                        .metadata()
                        .and_then(|meta| meta.modified())
                        .ok()
                        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|since| since.as_secs())
                        .unwrap_or(0);
                    rows.push((
                        path.file_stem()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .to_string(),
                        count,
                        at,
                    ));
                }
            }
            rows.sort_by_key(|row| std::cmp::Reverse(row.2));
            let mut text = format!("{} log(s) under {}\n", rows.len(), dir.display());
            for (id, count, at) in rows {
                text.push_str(&format!("{id}  {count} entries  modified {at}\n"));
            }
            Ok((text, 0))
        }
        "handover" => {
            // The record the run wrote before it stopped, plus the case link the surface appended after
            // the case existed. Later records override earlier ones field by field.
            let wanted = flags
                .named
                .get("session")
                .cloned()
                .or_else(|| flags.positional.get(1).cloned());
            let path = match wanted {
                Some(id) => dir.join(format!("{id}.jsonl")),
                None => std::fs::read_dir(&dir)
                    .ok()
                    .into_iter()
                    .flatten()
                    .flatten()
                    .filter(|entry| entry.path().extension().is_some_and(|kind| kind == "jsonl"))
                    .max_by_key(|entry| entry.metadata().and_then(|meta| meta.modified()).ok())
                    .map(|entry| entry.path())
                    .ok_or_else(|| format!("no session logs under {}", dir.display()))?,
            };
            let text = std::fs::read_to_string(&path)
                .map_err(|error| format!("could not read {}: {error}", path.display()))?;
            let mut merged = serde_json::Map::new();
            for line in text.lines() {
                let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
                    continue;
                };
                let payload = entry.get("payload").unwrap_or(&serde_json::Value::Null);
                if payload.get("node").and_then(serde_json::Value::as_str) != Some("handover") {
                    continue;
                }
                for (key, value) in payload.as_object().into_iter().flatten() {
                    if key == "node" || value.is_null() {
                        continue;
                    }
                    merged.insert(key.clone(), value.clone());
                }
            }
            if merged.is_empty() {
                return Ok((
                    format!(
                        "no handover in {}: none is written for an accepted run\n",
                        path.display()
                    ),
                    0,
                ));
            }
            let field = |key: &str| {
                merged
                    .get(key)
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string()
            };
            let attempts = merged
                .get("attempts")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let mut out = format!("session: {}\n", field("session"));
            out.push_str(&format!("goal:    {}\n", field("goal")));
            out.push_str(&format!(
                "ended:   {} after {} attempt(s)\n",
                field("ended"),
                attempts
            ));
            out.push_str(&format!(
                "check:   {}\n",
                match field("last_check").as_str() {
                    "" => "not run".to_string(),
                    other => other.to_string(),
                }
            ));
            out.push_str(&format!(
                "next:    {}\n",
                match field("next").as_str() {
                    "" => "no next step was recorded".to_string(),
                    other => other.to_string(),
                }
            ));
            let blockers: Vec<String> = merged
                .get("blockers")
                .and_then(serde_json::Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            out.push_str(&format!(
                "blockers: {}\n",
                if blockers.is_empty() {
                    "none recorded".to_string()
                } else {
                    blockers.join("; ")
                }
            ));
            if let Some(case) = merged.get("case_id").and_then(serde_json::Value::as_str) {
                out.push_str(&format!("case:    {case}\n"));
            }
            if let Some(candidate) = merged
                .get("best_candidate")
                .and_then(serde_json::Value::as_str)
            {
                out.push_str(&format!("best candidate:\n{candidate}\n"));
            }
            Ok((out, 0))
        }
        "show" => {
            let id = flags
                .positional
                .get(1)
                .ok_or_else(|| "show needs a session id".to_string())?;
            let store = session_tree::FileStore::new(&dir);
            let session = session_tree::Session::load(
                store,
                session_tree::SystemClock,
                session_tree::SessionId::new(id),
            )
            .map_err(|fault| format!("{fault:?}"))?;
            let mut text = format!("session {id}, {} entries\n", session.len());
            for entry in session.entries() {
                let payload = entry.payload.to_string();
                let shown: String = payload.chars().take(120).collect();
                text.push_str(&format!(
                    "{:>4}  {:<12} {}\n",
                    entry.at,
                    entry.kind.as_str(),
                    shown
                ));
            }
            Ok((text, 0))
        }
        other => Err(format!("`session {other}`: say list or show <id>")),
    }
}

/// The scoreboard as data, printed. No model, no daemon: it reads one file.
fn scoreboard(args: &[String]) -> Result<(String, i32), String> {
    let flags = parse(args, &["state"])?;
    let state = state_of(&flags)?;
    Ok((
        crate::scoreboard::report(&crate::scoreboard::load(&state)),
        0,
    ))
}

fn route(args: &[String]) -> Result<(String, i32), String> {
    let flags = parse(args, &["root", "config", "state"])?;
    let goal = goal(&flags)?;
    let root = root_of(&flags)?;
    let task = TaskSpec::new(goal, root);
    let state = state_of(&flags)?;

    let (mut id, mut reason) = Rules::shipped().pick(&task);
    if reason.as_str() == "no rule matched, so the fallback profile ran"
        && let Some(best) = crate::scoreboard::choose(&crate::scoreboard::load(&state))
        && profile::named(&best.profile).is_some()
    {
        reason = crate::route::RoutingReason::new(crate::scoreboard::reason(&best));
        id = crate::profile::ProfileId::new(best.profile.clone());
    }
    let found = profile::named(id.as_str())
        .ok_or_else(|| format!("the rules picked {id}, which this build does not ship"))?;

    Ok((
        format!(
            "profile: {}\nmodel:   {}\nreason:  {}\n",
            found.id,
            found.model.as_deref().unwrap_or("inherit"),
            reason.as_str()
        ),
        0,
    ))
}

fn context(args: &[String]) -> Result<(String, i32), String> {
    let flags = parse(args, &["root", "profile", "config", "memory"])?;
    let goal = goal(&flags)?;
    let root = root_of(&flags)?;
    let task = TaskSpec::new(goal.clone(), root);

    let profile = match flags.named.get("profile") {
        Some(id) => profile::named(id).ok_or_else(|| format!("no profile named `{id}`"))?,
        None => {
            let (id, _) = Rules::shipped().pick(&task);
            profile::named(id.as_str())
                .ok_or_else(|| format!("the rules picked {id}, which this build does not ship"))?
        }
    };

    let memo = remembered(&flags, &goal, (profile.context_tokens / 10).min(2_000));
    let mut offered = sections(&task, &profile, profile.context_tokens)
        .map_err(|fault| fault.detail().to_string())?;
    if let Some((name, text)) = memo.section.clone() {
        offered.insert(0, Section::new(name, text));
    }
    let builder = Budgeted::new(offered);
    let bundle = builder
        .build(&task, &profile.budget())
        .map_err(|fault| fault.to_string())?;

    let mut text = String::new();
    text.push_str(&format!(
        "profile: {} ({} context tokens)\nbundle:  {} tokens\n\nsections in:\n",
        profile.id,
        profile.context_tokens,
        bundle.tokens()
    ));
    for section in bundle.sections() {
        text.push_str(&format!(
            "  {:<10} {:<9} {}\n",
            section.name(),
            format!("{} tokens", crate::context::estimate(section.text())),
            preview(section.text())
        ));
    }
    text.push_str("\nsections dropped:\n");
    if bundle.dropped().is_empty() {
        text.push_str("  none\n");
    } else {
        for name in bundle.dropped() {
            text.push_str(&format!("  {name}\n"));
        }
    }

    text.push_str(&format!(
        "\nmemory: recalled {}{}\n",
        memo.recalled,
        memo.reason
            .map(|why| format!(" ({why})"))
            .unwrap_or_default()
    ));

    Ok((text, 0))
}

/// The sections a turn offers. One implementation, in `context`, so the turn and the graph cannot drift
/// into offering different prompts for the same work.
fn sections(task: &TaskSpec, profile: &Profile, budget: usize) -> Result<Vec<Section>, Fault> {
    // A one-shot inspection with nowhere to keep a cache, so it runs cold and does not pretend otherwise.
    let mut cache = crate::repomap::Cache::new();
    context::sections(task, profile, budget, &Learned::new(), &mut cache)
}

/// The audit, over the record.
///
/// A tree named with `--vault` is imported once into a throwaway database under `~/.hades/tmp`, so
/// there is one audit implementation and a receipt can still be taken for a tree that was never
/// opened. The temporary files are removed before the command returns.
fn audit(args: &[String]) -> Result<(String, i32), String> {
    let flags = parse_with(args, &["vault", "config", "db"], &["refresh"])?;
    let (root, explicit) = match flags.named.get("vault") {
        Some(path) => (guarded(Path::new(path))?, true),
        // The config exists so that a command with no flags can find the home `hades init` made. An
        // audit is the first command that has to work that way.
        None => match config_of(&flags)? {
            Some(config) => (guarded(&config.vault)?, false),
            None => {
                return Err(
                    "audit needs a vault: pass `--vault <dir>`, or run `hades init` so the config \
                     names one"
                        .to_string(),
                );
            }
        },
    };
    if explicit && switched(&flags, "refresh") {
        return Err(
            "`--refresh` applies to the record; `--vault` already imports the tree it audits"
                .to_string(),
        );
    }

    let (db, temporary) = if explicit {
        let dir = hades_home()?.join("tmp");
        fs::create_dir_all(&dir).map_err(|error| format!("{}: {error}", dir.display()))?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        (
            dir.join(format!("audit-{}-{stamp}.db", std::process::id())),
            true,
        )
    } else {
        (db_path(&flags)?, false)
    };

    let outcome = (|| {
        let store = mneme::SqliteStore::open(&db, &root).map_err(|fault| fault.to_string())?;
        let empty = store.is_empty().map_err(|fault| fault.to_string())?;
        // `--vault` is a read-only probe of a tree: it imports into a throwaway record and must not
        // write the user's INDEX.md. The config's record is the one an operator builds, so a
        // refresh there also refreshes the view, exactly as `hades mneme import` does.
        let refreshed = !explicit && (empty || switched(&flags, "refresh"));
        if explicit || refreshed {
            store
                .import_tree_lenient(&root, &[])
                .map_err(|fault| fault.to_string())?;
        }
        let vault = mneme::Vault::new(root.clone(), store, mneme::SystemClock);
        if refreshed {
            vault
                .write_index(None, &[])
                .map_err(|fault| fault.to_string())?;
        }
        let curator = governance::Curator::new(vault, governance::SystemTimestamp);
        curator.audit().map_err(|refusal| refusal.to_string())
    })();

    if temporary {
        for suffix in ["", "-wal", "-shm"] {
            let _ = fs::remove_file(format!("{}{suffix}", db.display()));
        }
    }

    let report = outcome?;

    let mut text = format!("vault: {}\n", root.display());
    for line in report.lines() {
        text.push_str(&line);
        text.push('\n');
    }
    let code = if report.ok() { 0 } else { 1 };
    Ok((text, code))
}

/// Open the record for a vault. `db` defaults to `<home>/mneme.db`, beside the export.
fn record(
    root: &Path,
    db: &Path,
) -> Result<mneme::Vault<mneme::SqliteStore, mneme::SystemClock>, String> {
    let store = mneme::SqliteStore::open(db, root)
        .map_err(|fault| format!("the record could not be opened: {fault}"))?;
    Ok(mneme::Vault::new(
        root.to_path_buf(),
        store,
        mneme::SystemClock,
    ))
}

fn creation_root(flags: &Flags) -> Result<PathBuf, String> {
    match flags.named.get("vault") {
        Some(path) => guarded(Path::new(path)),
        None => match config_of(flags)? {
            Some(config) => guarded(&config.vault),
            None => Err(
                "this command needs a vault: pass `--vault <dir>`, or run `hades init` so \
                 the config names one"
                    .to_string(),
            ),
        },
    }
}

fn required_summary(flags: &Flags) -> Result<&str, String> {
    flags
        .named
        .get("summary")
        .map(String::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| "this note needs `--summary <text>`".to_string())
}

fn provenance(flags: &Flags) -> String {
    flags
        .named
        .get("provenance")
        .map(String::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .unwrap_or("hand")
        .to_string()
}

fn created_text(
    vault: &mneme::Vault<mneme::SqliteStore, mneme::SystemClock>,
    path: &Path,
) -> String {
    let relative = mneme::relative_posix(path, vault.root());
    format!(
        "created: {relative}\nindex: {}\n",
        vault.root().join(mneme::INDEX_NAME).display()
    )
}

/// `hades topic <category> <name>`: a general topic, or a subtopic with `--parent`.
fn create_topic_cmd(args: &[String]) -> Result<(String, i32), String> {
    let flags = parse_with(
        args,
        &["vault", "db", "config", "parent", "summary", "provenance"],
        &[],
    )?;
    let category = flags
        .positional
        .first()
        .ok_or_else(|| "topic needs a category and a name".to_string())?;
    let name = flags.positional[1..].join(" ");
    if name.trim().is_empty() {
        return Err("topic needs a name after the category".to_string());
    }
    let summary = required_summary(&flags)?;
    let root = creation_root(&flags)?;
    let vault = record(&root, &db_path(&flags)?)?;
    let path = vault
        .create_topic(
            category,
            &name,
            flags.named.get("parent").map(String::as_str),
            summary,
            &provenance(&flags),
        )
        .map_err(|fault| fault.to_string())?;
    Ok((created_text(&vault, &path), 0))
}

/// `hades research <topic> <title>`: a durable finding under an existing topic.
fn create_research_cmd(args: &[String]) -> Result<(String, i32), String> {
    let flags = parse_with(
        args,
        &[
            "vault",
            "db",
            "config",
            "summary",
            "provenance",
            "slug",
            "source-url",
        ],
        &[],
    )?;
    let topic = flags
        .positional
        .first()
        .ok_or_else(|| "research needs a topic and a title".to_string())?;
    let title = flags.positional[1..].join(" ");
    if title.trim().is_empty() {
        return Err("research needs a title after the topic".to_string());
    }
    let summary = required_summary(&flags)?;
    let root = creation_root(&flags)?;
    let vault = record(&root, &db_path(&flags)?)?;
    let path = vault
        .create_research(
            topic,
            &title,
            flags.named.get("slug").map(String::as_str),
            summary,
            &provenance(&flags),
            flags.named.get("source-url").map(String::as_str),
        )
        .map_err(|fault| fault.to_string())?;
    Ok((created_text(&vault, &path), 0))
}

/// `hades project <name> --topic <topic> --research <ref>`: a hub over existing research.
fn create_project_cmd(args: &[String]) -> Result<(String, i32), String> {
    let flags = parse_with(
        args,
        &[
            "vault",
            "db",
            "config",
            "summary",
            "provenance",
            "topic",
            "research",
        ],
        &[],
    )?;
    let name = flags.positional.join(" ");
    if name.trim().is_empty() {
        return Err("project needs a name".to_string());
    }
    let topic = flags
        .named
        .get("topic")
        .ok_or_else(|| "project needs `--topic <topic>`".to_string())?;
    let research = flags.repeated.get("research").cloned().unwrap_or_default();
    if research.is_empty() {
        return Err("project needs at least one `--research <ref>`".to_string());
    }
    let summary = required_summary(&flags)?;
    let root = creation_root(&flags)?;
    let vault = record(&root, &db_path(&flags)?)?;
    let path = vault
        .create_project(&name, topic, &research, summary, &provenance(&flags))
        .map_err(|fault| fault.to_string())?;
    Ok((created_text(&vault, &path), 0))
}

/// `hades product <name> --from <project>`: freeze a project as its own thing.
fn create_product_cmd(args: &[String]) -> Result<(String, i32), String> {
    let flags = parse_with(
        args,
        &[
            "vault",
            "db",
            "config",
            "summary",
            "provenance",
            "topic",
            "from",
            "status",
        ],
        &[],
    )?;
    let name = flags.positional.join(" ");
    if name.trim().is_empty() {
        return Err("product needs a name".to_string());
    }
    let from = flags
        .named
        .get("from")
        .ok_or_else(|| "product needs `--from <project>`".to_string())?;
    let status = flags
        .named
        .get("status")
        .map(String::as_str)
        .unwrap_or("frozen");
    let summary = required_summary(&flags)?;
    let root = creation_root(&flags)?;
    let vault = record(&root, &db_path(&flags)?)?;
    let path = vault
        .create_product(
            &name,
            from,
            flags.named.get("topic").map(String::as_str),
            status,
            summary,
            &provenance(&flags),
        )
        .map_err(|fault| fault.to_string())?;
    Ok((created_text(&vault, &path), 0))
}

/// The record, when a config names a vault and `--memory` is not off. `None` is a machine without a
/// home yet: a turn still runs, it just cannot remember, and the output says so.
fn open_memory(flags: &Flags) -> Result<Option<MnemeMemory>, String> {
    if matches!(flags.named.get("memory").map(String::as_str), Some("off")) {
        return Ok(None);
    }
    let Some(config) = config_of(flags)? else {
        return Ok(None);
    };
    // A named state with no vault or config of its own is an isolated install. Its record lives beside it,
    // not in the live home: a test that points at /tmp must not recall what the live run learned. An explicit
    // `--vault` or `--config` is a caller who named their record, and is honoured.
    let isolated = flags.named.contains_key("state")
        && !flags.named.contains_key("vault")
        && !flags.named.contains_key("config");
    if isolated {
        let state = state_of(flags)?;
        let vault = state.join("vault");
        if !vault.is_dir() {
            return Ok(None);
        }
        return MnemeMemory::open(&vault, state.join("mneme.db")).map(Some);
    }
    MnemeMemory::open(&config.vault, db_path(flags)?).map(Some)
}

/// What a goal recalls, and the memory object a caller needs to record afterwards.
pub(crate) struct Remembered {
    pub(crate) memory: Option<MnemeMemory>,
    pub(crate) section: Option<(String, String)>,
    pub(crate) recalled: usize,
    pub(crate) reason: Option<String>,
    /// The topics the recall used, so the record can file under the same ones.
    pub(crate) topics: Vec<String>,
}

pub(crate) fn remembered(flags: &Flags, goal: &str, budget: usize) -> Remembered {
    let memory = match open_memory(flags) {
        Ok(memory) => memory,
        Err(error) => {
            return Remembered {
                memory: None,
                section: None,
                recalled: 0,
                reason: Some(error),
                topics: Vec::new(),
            };
        }
    };
    let Some(memory) = memory else {
        return Remembered {
            memory: None,
            section: None,
            recalled: 0,
            reason: None,
            topics: Vec::new(),
        };
    };
    let topics = memory
        .vault()
        .read_charter()
        .map(|charter| charter.topics)
        .unwrap_or_default();
    match memory.recall(goal, &topics, budget) {
        Ok(recall) => Remembered {
            section: if recall.hits.is_empty() {
                None
            } else {
                Some(("what Mneme remembers".to_string(), recall.text))
            },
            recalled: recall.hits.len(),
            reason: recall.reason.map(|reason| format!("{reason:?}")),
            topics,
            memory: Some(memory),
        },
        Err(error) => Remembered {
            memory: Some(memory),
            section: None,
            recalled: 0,
            reason: Some(error),
            topics: Vec::new(),
        },
    }
}

fn db_path(flags: &Flags) -> Result<PathBuf, String> {
    if let Some(path) = flags.named.get("db") {
        return guarded(Path::new(path));
    }
    // Beside the config, not beside the process: a staged home (`--config /tmp/install/config.json`)
    // must keep its record with it, and a test that names a config must never touch ~/.hades.
    let config = config_path(flags)?;
    let parent = config
        .parent()
        .ok_or_else(|| format!("{} has no directory", config.display()))?;
    Ok(parent.join("mneme.db"))
}

/// `hades mneme import|export`: the explicit verbs for the markdown surface.
fn mneme(args: &[String]) -> Result<(String, i32), String> {
    let action = args.first().map(String::as_str).unwrap_or("");
    let rest = &args[1..];
    let flags = parse_with(rest, &["db", "config", "vault"], &["force", "retire-old"])?;

    let root = match flags.named.get("vault") {
        Some(path) => guarded(Path::new(path))?,
        None => match config_of(&flags)? {
            Some(config) => guarded(&config.vault)?,
            None => {
                return Err(
                    "mneme needs a vault: pass `--vault <dir>`, or run `hades init` so the config \
                     names one"
                        .to_string(),
                );
            }
        },
    };
    let vault = record(&root, &db_path(&flags)?)?;

    match action {
        "import" => {
            let source = flags
                .positional
                .first()
                .ok_or_else(|| "mneme import needs a directory of notes".to_string())?;
            let source = guarded(Path::new(source))?;
            let report = vault
                .store()
                .import_tree(&source, &[])
                .map_err(|fault| fault.to_string())?;
            // The index is a view of the record. An import that did not refresh it would leave a
            // vault that audits correctly and reads as empty, which is the drift the map exists to
            // prevent.
            vault
                .write_index(None, &[])
                .map_err(|fault| fault.to_string())?;
            let mut text = format!("imported: {} from {}\n", report.imported, source.display());
            text.push_str(&format!("fenced: {}\n", report.fenced.len()));
            for path in &report.fenced {
                text.push_str(&format!("  {path}\n"));
            }
            Ok((text, 0))
        }
        "export" => {
            let report = vault
                .store()
                .export_tree(&root, switched(&flags, "force"))
                .map_err(|fault| fault.to_string())?;
            let mut text = format!(
                "written: {}\nunchanged: {}\nrefused: {}\n",
                report.written,
                report.unchanged,
                report.refused.len()
            );
            for path in &report.refused {
                text.push_str(&format!(
                    "  {} (hand edited; --force overwrites)\n",
                    path.display()
                ));
            }
            let code = if report.refused.is_empty() { 0 } else { 1 };
            Ok((text, code))
        }
        "upgrade" => {
            // Seed the map and the marking posts, then write the six-area tree. The record has
            // already migrated when `record` opened it, so a v1 topic is under `general` before the
            // export runs.
            vault.init(None).map_err(|fault| fault.to_string())?;
            let report = vault
                .store()
                .export_tree(&root, switched(&flags, "force"))
                .map_err(|fault| fault.to_string())?;
            let mut text = format!(
                "written: {}\nunchanged: {}\nrefused: {}\n",
                report.written,
                report.unchanged,
                report.refused.len()
            );
            for path in &report.refused {
                text.push_str(&format!(
                    "  {} (hand edited; --force overwrites)\n",
                    path.display()
                ));
            }
            let old = root.join("40 Topics");
            if old.is_dir() {
                let files = vault
                    .store()
                    .walk(&old)
                    .map(|paths| paths.len())
                    .unwrap_or(0);
                if switched(&flags, "retire-old") {
                    let dest = retire_old_topics(&root)?;
                    text.push_str(&format!(
                        "old: moved {files} file(s) from 40 Topics to {}\n",
                        dest.display()
                    ));
                } else {
                    text.push_str(&format!(
                        "old: {files} file(s) under 40 Topics kept; rerun with --retire-old to move \
                         them out of the vault\n"
                    ));
                }
            }
            let code = if report.refused.is_empty() { 0 } else { 1 };
            Ok((text, code))
        }
        other => Err(format!(
            "`mneme {other}` is not a command; try `hades mneme import`, `export` or `upgrade`"
        )),
    }
}

/// Move the pre-0.0.10 `40 Topics` export out of the vault. Never delete: the record is the source,
/// but a hand edit that was never imported is still a person's file.
fn retire_old_topics(root: &Path) -> Result<PathBuf, String> {
    let old = root.join("40 Topics");
    let parent = root
        .parent()
        .ok_or_else(|| format!("{} has no parent to archive beside", root.display()))?;
    let archive = parent.join("mneme-archive");
    fs::create_dir_all(&archive).map_err(|error| format!("{}: {error}", archive.display()))?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let mut dest = archive.join(format!("40-topics-{stamp}"));
    let mut counter = 1;
    while dest.exists() {
        dest = archive.join(format!("40-topics-{stamp}-{counter}"));
        counter += 1;
    }
    fs::rename(&old, &dest).map_err(|error| {
        format!(
            "{}: {error}; the old folder was left in place",
            old.display()
        )
    })?;
    Ok(dest)
}

/// `hades service install`: the two units, rendered for this install.
///
/// The prefix is where the binary will live, not where this one is running from. A test run out of
/// `target/release` must not write a unit that points into a build directory, and an installer that stages a
/// prefix before moving it can render the unit for the destination.
fn service(args: &[String]) -> Result<(String, i32), String> {
    let action = args.first().map(String::as_str).unwrap_or("");
    let rest = &args[1..];
    let flags = parse_with(rest, &["prefix", "home", "out"], &["force"])?;

    match action {
        "install" => {
            let prefix = match flags.named.get("prefix") {
                Some(path) => guarded(Path::new(path))?,
                None => home_dir()?.join(".local"),
            };
            let home = match flags.named.get("home") {
                Some(path) => guarded(Path::new(path))?,
                None => hades_home()?,
            };
            let out = match flags.named.get("out") {
                Some(path) => guarded(Path::new(path))?,
                None => home_dir()?.join(".config").join("systemd").join("user"),
            };
            let written = crate::service::write(&prefix, &home, &out, switched(&flags, "force"))?;
            let mut text = String::new();
            for path in &written {
                text.push_str(&format!("installed: {}\n", path.display()));
            }
            text.push_str(
                "\nnext:\n  systemctl --user daemon-reload\n  systemctl --user enable --now hades-daemon.service\n",
            );
            text.push_str(&format!(
                "\ngateway (not started): the unit is written. It needs HADES_TELEGRAM_TOKEN in \
                 {}/.env, and `gateway.telegram` with non-empty `allowed_users` and `verify` in {}. \
                 Then:\n  systemctl --user enable --now hades-gateway.service\n",
                home.display(),
                home.join(config::NAME).display()
            ));
            Ok((text, 0))
        }
        other => Err(format!(
            "`service {other}` is not a command; try `hades service install`"
        )),
    }
}

/// `$HOME`, or a named reason. The service command needs the config directory, which is not Hades' home.
fn home_dir() -> Result<PathBuf, String> {
    std::env::var("HOME")
        .map(PathBuf::from)
        .map_err(|_| "no HOME, so the systemd user directory cannot be guessed".to_string())
}

/// The command a second machine needs first: one file for the gateway to read, and one home for the
/// memory to live in.
///
/// It does three things in the order a person would: refuses if there is already a vault, asks who the
/// agent is and what the work is for, then writes. The refusal comes before the questions on purpose. A
/// person should not answer three questions and then be told the work was never going to happen.
///
/// `--non-interactive` is the installer's path: every answer is a flag or a default, and no prompt is
/// printed, so an installer's log stays a log rather than a transcript of a conversation.
fn init(
    args: &[String],
    out: &mut dyn Write,
    input: &mut dyn BufRead,
) -> Result<(String, i32), String> {
    let flags = parse_with(
        args,
        &[
            "config", "vault", "root", "model", "provider", "name", "goal", "topics", "db",
        ],
        &["non-interactive"],
    )?;

    let config_path = config_path(&flags)?;
    let vault_path = match flags.named.get("vault") {
        Some(path) => guarded(Path::new(path))?,
        None => {
            // Beside the config, so a home is one directory: moving it moves the config and the vault
            // together, and no flag has to be updated.
            let parent = config_path
                .parent()
                .ok_or_else(|| format!("{} has no parent directory", config_path.display()))?;
            parent.join("mneme")
        }
    };
    refuse_existing(&vault_path)?;

    let model_id = flags
        .named
        .get("model")
        .cloned()
        .unwrap_or_else(|| "deepseek-v4.1-flash".to_string());
    let preset = model::preset(&model_id).ok_or_else(|| {
        format!(
            "no model named `{model_id}`: try {}",
            model::presets()
                .iter()
                .map(|preset| preset.id)
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;
    let provider = flags
        .named
        .get("provider")
        .cloned()
        .unwrap_or_else(|| config::provider_for(preset.endpoint).to_string());
    let root = root_with(&flags, None)?;

    let mut name = flags
        .named
        .get("name")
        .cloned()
        .unwrap_or_else(|| "Hades".to_string());
    let mut goal = flags.named.get("goal").cloned().unwrap_or_default();
    let mut topics = parse_topics(flags.named.get("topics").map(String::as_str).unwrap_or(""));

    if !switched(&flags, "non-interactive") {
        let _ = writeln!(out, "hades init");
        let _ = writeln!(out, "  config: {}", config_path.display());
        let _ = writeln!(out, "  vault:  {}", vault_path.display());
        let _ = writeln!(out, "  model:  {} ({provider})", preset.id);
        let _ = writeln!(out);

        match ask(out, input, "What should the agent be called?", &name) {
            None => {
                // EOF is not an error, but silence that chooses the defaults is a fact worth printing:
                // an installer that forgot `--non-interactive` should see why its prompts vanished.
                let _ = writeln!(
                    out,
                    "  no answer on stdin; using the defaults. `--non-interactive` is the installer's flag."
                );
            }
            Some(answer) => {
                name = answer;
                if let Some(answer) = ask(out, input, "What is the work for?", &goal) {
                    goal = answer;
                }
                let listed = topics.join(", ");
                if let Some(answer) = ask(
                    out,
                    input,
                    "Which topics matter? (comma separated)",
                    &listed,
                ) {
                    topics = parse_topics(&answer);
                }
            }
        }
        let _ = writeln!(out);
    }

    let vault = record(&vault_path, &db_path(&flags)?)?;
    vault
        .init(Some((name.as_str(), goal.as_str(), topics.as_slice())))
        .map_err(|fault| format!("the vault could not be created: {fault}"))?;
    // The record is the source and the tree is its export: init must leave six folders a person can
    // open, not only rows in a database beside them.
    vault
        .store()
        .export_tree(&vault_path, false)
        .map_err(|fault| format!("the vault was written but its export failed: {fault}"))?;

    let config = config::Config {
        provider: provider.clone(),
        model: preset.id.to_string(),
        root: root.clone(),
        vault: vault_path.clone(),
        mcp: crate::mcp::McpConfig::default(),
    };
    config::save(&config_path, &config).map_err(|error| {
        format!(
            "{error}\nthe vault was created at {} but the config was not written; point `--vault` at a \
             new directory for the next attempt",
            vault_path.display()
        )
    })?;

    // A fresh install ships a small, general skill set. It is embedded in the binary and written here, so
    // a release tarball needs no second directory and a person can read and edit what the agent follows.
    let seeded = crate::builtin::write_all(&hades_home()?)?;

    let areas = mneme::Area::ALL
        .iter()
        .map(|area| area.folder())
        .collect::<Vec<_>>()
        .join(", ");
    // The key file is the Hades home's own, which is not always `~/.hades`: an installer or a test sets
    // HADES_HOME, and a receipt that names the wrong path is the first lie a new install tells.
    let keys = hades_home()?.join(".env");
    let skill_dir = crate::builtin::dir(&hades_home()?);
    let mut text = format!(
        "config:  {}\n  provider: {provider}\n  model:    {}\n  root:     {}\n  vault:    {}\n\
         vault:   {}\n  areas:   {areas}\n  charter: name {:?}, {} topic(s)\n  index:   {}\n\
         skills:  {seeded} built-in seeded at {}\n  keys:    {}, never the config\nnext:    hades audit\n",
        config_path.display(),
        preset.id,
        root.display(),
        vault_path.display(),
        vault_path.display(),
        name,
        topics.len(),
        vault_path.join(mneme::INDEX_NAME).display(),
        skill_dir.display(),
        keys.display()
    );
    text.push_str(&format!(
        "\nopen charter: {}\n",
        vault_path.join(mneme::charter::CHARTER_NAME).display()
    ));
    Ok((text, 0))
}

/// An existing vault is refused; an empty directory is written into.
///
/// The check is on content rather than existence because `mkdir -p` in an installer is not a vault, and
/// refusing it would turn a harmless step into a failed install. A second `hades init` finds six areas
/// and a charter in the directory, which is a vault by any reading.
fn refuse_existing(vault: &Path) -> Result<(), String> {
    if !vault.exists() {
        return Ok(());
    }
    let occupied = if vault.is_dir() {
        std::fs::read_dir(vault)
            .map_err(|error| format!("{}: {error}", vault.display()))?
            .next()
            .is_some()
    } else {
        true
    };
    if occupied {
        return Err(format!(
            "refusing to initialise {}: the vault already exists. `hades init` never writes over a home; \
             move the old one aside, or pass `--vault <another dir>`",
            vault.display()
        ));
    }
    Ok(())
}

/// One question, one answer.
///
/// `None` means stdin ended before an answer, which is a state and not a fault: a scripted installer
/// without `--non-interactive` gets the defaults and a line saying so. A blank line takes the default.
fn ask(
    out: &mut dyn Write,
    input: &mut dyn BufRead,
    question: &str,
    default: &str,
) -> Option<String> {
    if default.is_empty() {
        let _ = writeln!(out, "{question}");
    } else {
        let _ = writeln!(out, "{question} [{default}]");
    }
    let _ = out.flush();
    let mut line = String::new();
    match input.read_line(&mut line) {
        Ok(0) => None,
        Ok(_) => {
            let given = line.trim();
            Some(if given.is_empty() {
                default.to_string()
            } else {
                given.to_string()
            })
        }
        Err(_) => None,
    }
}

/// A comma-separated answer as topics: trimmed, empty gone, and no topic twice.
fn parse_topics(text: &str) -> Vec<String> {
    let mut topics: Vec<String> = Vec::new();
    for part in text.split(',') {
        let topic = part.trim();
        if topic.is_empty() {
            continue;
        }
        if !topics.iter().any(|held| held.eq_ignore_ascii_case(topic)) {
            topics.push(topic.to_string());
        }
    }
    topics
}

/// Everything `turn` and `run` both have to decide, built through the one constructor in [`crate::engine`].
///
/// The two commands used to have their own copy of this, and the copies had already drifted: the walk's
/// cage did not ask the language runtimes where they live, so a Python program that ran fine under `turn`
/// died under `run` before it executed a line. Three callers (the daemon is the third) is what makes this
/// an extraction rather than a tidy-up.
pub(crate) fn assemble(
    flags: &Flags,
    default_session: &str,
) -> Result<
    (
        Engine,
        Answerer,
        crate::harness_files::Files,
        Option<crate::judge::Endpoint>,
    ),
    String,
> {
    // Read once: the root and the model both come from it, and a config that changed between the two
    // reads would assemble a run that is rooted in one tree and talking to the other's model.
    let config = config_of(flags)?;
    let goal = goal(flags)?;
    let root = root_with(flags, config.as_ref())?;
    let task = TaskSpec::new(goal.clone(), root.clone());
    let state = state_of(flags)?;

    let (mut routed, mut reason) = Rules::shipped().pick(&task);
    // Rules first, then our own scoreboard: only when nothing matched, and only when the scoreboard has
    // enough finished runs to have an opinion. A named profile always wins, because a person who typed
    // `--profile` has said what they want.
    if !flags.named.contains_key("profile")
        && reason.as_str() == "no rule matched, so the fallback profile ran"
        && let Some(best) = crate::scoreboard::choose(&crate::scoreboard::load(&state))
        && profile::named(&best.profile).is_some()
    {
        reason = crate::route::RoutingReason::new(crate::scoreboard::reason(&best));
        routed = crate::profile::ProfileId::new(best.profile.clone());
    }
    let profile = match flags.named.get("profile") {
        Some(id) => profile::named(id).ok_or_else(|| format!("no profile named `{id}`"))?,
        None => profile::named(routed.as_str())
            .ok_or_else(|| format!("the rules picked {routed}, which this build does not ship"))?,
    };
    let mut profile = profile;
    if let Some(command) = flags.named.get("verify") {
        profile.verify = command.clone();
    }

    // The window the model actually has caps the turn's budget. Asking a model with an 8K window for 32K of
    // context does not produce a bigger window: it produces a server-side truncation, or a refusal, and
    // either way the bundle that was built is not the bundle that was sent.
    let harness = crate::harness_files::Files::load(
        flags.named.get("harness").map(String::as_str),
        profile.id.as_str(),
    )?;
    let answerer = answerer(
        flags,
        config.as_ref(),
        Some(&harness),
        profile.model.as_deref(),
    )?;
    // A strict request is checked while the check is free. An endpoint or a key that cannot work is
    // refused here, with the model and the provider named, rather than by the first call: a call that
    // cannot succeed is a call the caller was told would not be spent.
    let answerer = if switched(flags, "strict-model") {
        answerer.strict()?
    } else {
        answerer
    };
    // The judge is the same client with a different prompt. A scripted turn has no judge, and the turn
    // records that honestly rather than pretending a person's program was judged.
    let judged = profile.judge;
    let judge = match (&answerer, judged) {
        (Answerer::Model(open), true) => Some(crate::judge::Endpoint::new(open.clone())),
        _ => None,
    };
    if answerer.context() < profile.context_tokens {
        profile.context_tokens = answerer.context();
    }
    // The same number also caps the bundle for a caller that is not a model: a scripted run still has a
    // budget, and the compaction contract has to be reachable without an endpoint to ask.
    if let Some(context) = flags.named.get("context") {
        let asked = context
            .parse::<usize>()
            .map_err(|_| format!("`--context {context}` is not a number"))?;
        profile.context_tokens = profile.context_tokens.min(asked);
    }

    // Policy is data, named on the command line, so what the run was allowed to do is visible in the call
    // rather than buried in a default. Guarded is the default, because a harness whose default is allow has
    // already decided that the model gets the machine.
    let policy_name = flags
        .named
        .get("policy")
        .map(String::as_str)
        .unwrap_or(policy::GUARDED);
    let policy = policy::named(policy_name).ok_or_else(|| {
        format!(
            "no policy named `{policy_name}`: try {}",
            policy::shipped().join(" or ")
        )
    })?;

    let session_id = flags
        .named
        .get("session")
        .cloned()
        .unwrap_or_else(|| default_session.to_string());

    let caged = match flags.named.get("cage").map(String::as_str) {
        None | Some("on") => true,
        Some("off") => false,
        Some(other) => return Err(format!("`--cage {other}`: say on or off")),
    };
    // On by default: the design asks for a shadow snapshot of every tool effect, and a safety net that has
    // to be remembered is one that will be forgotten. `--shadow off` is there because a snapshot costs a
    // copy of the tree, and what it costs is measured in `shadow.rs` rather than guessed at.
    let shadow = match flags.named.get("shadow").map(String::as_str) {
        None if profile.shadow => Some(state.join("shadows")),
        None | Some("off") => None,
        Some("on") => Some(state.join("shadows")),
        Some(other) => return Err(format!("`--shadow {other}`: say on or off")),
    };
    // A verifier deadline belongs to the check, not the walk. `--check-minutes` is what makes a cold
    // `cargo test` viable in a replay whose tree has no `target/`.
    let check_minutes = match flags.named.get("check-minutes") {
        Some(value) => Some(
            value
                .parse::<u64>()
                .map_err(|_| format!("`--check-minutes {value}` is not a number"))?
                .max(1),
        ),
        None => None,
    };
    // The second oracle. Empty is not a command: an attack that cannot run is unmeasured, not blocked, and
    // a run that asked for one with no text is a mistake worth refusing before the model is called.
    let attack = match flags.named.get("attack").map(String::as_str) {
        Some(text) if text.trim().is_empty() => {
            return Err("`--attack` needs a command".to_string());
        }
        Some(text) => Some(text.to_string()),
        None => None,
    };

    let engine = Engine::open(Assembly {
        state,
        root,
        goal,
        session_id,
        profile,
        policy,
        policy_name: policy_name.to_string(),
        reason,
        caged,
        shadow,
        prompt: Some(harness.prompt().fact()),
        check_minutes,
        attack,
    })?;
    Ok((engine, answerer, harness, judge))
}

fn turn(args: &[String]) -> Result<(String, i32), String> {
    let flags = parse_with(
        args,
        &[
            "program",
            "root",
            "profile",
            "policy",
            "verify",
            "state",
            "session",
            "cage",
            "model",
            "endpoint",
            "key",
            "temperature",
            "max-tokens",
            "context",
            "thinking",
            "shadow",
            "config",
            "memory",
            "harness",
        ],
        &["strict-model"],
    )?;
    let goal = goal(&flags)?;
    let (mut engine, answerer, harness, judge) = assemble(&flags, "cli")?;
    let mut session = engine.session()?;
    // The log, read back into what this turn is told. A second run on a session id is a resume, and a resume
    // that hands the model the same prompt as the first attempt is a second roll of the same dice.
    let learned = Learned::from_log(&session);
    let memo = remembered(
        &flags,
        &goal,
        (engine.profile().context_tokens / 10).min(2_000),
    );
    let section = memo
        .section
        .as_ref()
        .map(|(name, text)| (name.as_str(), text.as_str()));
    let context = engine.context_with_memory(&learned, section)?;

    let seams = engine.seams(&context, answerer.harness());
    let seams = match &judge {
        Some(judge) => seams.judging(judge),
        None => seams,
    };

    let before = session.len();
    let turn = seams.turn(&mut session).map_err(|fault| {
        format!(
            "the turn ended in a {} fault: {}",
            fault.class(),
            fault.detail()
        )
    })?;

    let plan_chars = session.entries()[before..]
        .iter()
        .find(|entry| entry.kind == session_tree::Kind::Plan)
        .map(|entry| {
            entry
                .payload
                .get("plan")
                .and_then(serde_json::Value::as_str)
                .map(|plan| plan.chars().count())
                .unwrap_or(0)
        });

    if let Err(said) = engine.save_cache() {
        eprintln!("repomap: the cache was not written, {said}");
    }
    let (hits, misses) = (engine.cache().hits(), engine.cache().misses());

    // What this turn read is history for the next one. If it is too much of the budget, cut it now: the
    // summary is the harness's, and the log keeps every entry.
    let compacted = crate::compact::if_due(&mut session, &learned, engine.profile().context_tokens)
        .map_err(|fault| format!("the compaction could not be written: {fault}"))?;

    // Clotho: only an accepted run becomes a durable note. A refusal is written to the session log, not
    // to memory, and a write that fails is reported in the output rather than turned into a turn error.
    let memory_line = match (&memo.memory, &turn.outcome) {
        (Some(memory), Outcome::Ran { verification, .. })
            if verification.state == State::Passed =>
        {
            let after = Learned::from_log(&session);
            let constraints: Vec<String> = after
                .constraints()
                .iter()
                .map(|constraint| constraint.text().to_string())
                .collect();
            let summary = constraints
                .last()
                .cloned()
                .unwrap_or_else(|| goal.chars().take(160).collect());
            let topics = memory
                .vault()
                .read_charter()
                .map(|charter| charter.topics)
                .unwrap_or_default();
            let note = OutcomeNote {
                goal: goal.clone(),
                session: session.id().as_str().to_string(),
                run: 1,
                attempt: 1,
                title: goal.chars().take(80).collect(),
                summary,
                constraints,
                check: flags.named.get("verify").cloned().unwrap_or_default(),
                files: Vec::new(),
                topics,
            };
            match memory.record(&note) {
                Ok(recorded) => {
                    format!(
                        "recalled {}, recorded {}",
                        memo.recalled,
                        recorded.path.display()
                    )
                }
                Err(error) => {
                    format!("recalled {}, recorded none ({error})", memo.recalled)
                }
            }
        }
        (Some(_), _) => format!(
            "recalled {}, recorded none (the run was not accepted)",
            memo.recalled
        ),
        (None, _) => format!(
            "recalled {}, not recorded (no record is open)",
            memo.recalled
        ),
    };

    let mut text = format!(
        "session: {} ({} entries, {} appended)\nprofile: {}\npolicy:  {}\nmodel:   {}\nharness: {}\nplan:    {}\ncontext: {} tokens\ncage:    {}\nshadow:  {}\nmap:     {hits} hits, {misses} misses\n",
        session.id(),
        session.len(),
        session.len() - before,
        engine.profile().id,
        engine.policy_name(),
        answerer.describe(),
        harness.line(),
        match plan_chars {
            Some(chars) => format!("{chars} chars"),
            None => "none".to_string(),
        },
        engine.profile().context_tokens,
        engine.cage(),
        engine.shadows(),
    );
    text.push_str(&format!("history: {}\n", history(&learned)));
    text.push_str(&format!(
        "memory:  {memory_line}{}\n",
        if memo.recalled == 0 {
            memo.reason
                .as_ref()
                .map(|why| format!(" ({why})"))
                .unwrap_or_default()
        } else {
            String::new()
        }
    ));
    if let Some(id) = compacted {
        text.push_str(&format!(
            "compacted: #{} (history {} -> {} tokens, written by the harness)\n",
            id.0,
            learned.tokens(),
            Learned::from_log(&session).tokens()
        ));
    }
    for entry in &session.entries()[before..] {
        text.push_str(&format!(
            "  #{:<3} {:<18} {}\n",
            entry.id.0,
            entry.kind.as_str(),
            brief(&entry.payload)
        ));
    }
    text.push_str(&format!("\n{}\n", turn.outcome.summary()));
    text.push_str(&format!("log: {}\n", engine.log_path().display()));

    // A question nobody can answer is a run that hangs, so the way to answer it is printed with it. The id
    // goes in without the `#` because this line is a command, and a command has to parse.
    if let Outcome::Awaiting { question } = &turn.outcome {
        text.push_str(&format!(
            "answer it with: hades answer {} allow --scope session --state {}\n",
            question.id.0,
            engine.state().display()
        ));
    }

    let code = match turn.outcome {
        Outcome::Ran { verification, .. } => match verification.state {
            State::Passed => 0,
            State::Failed => 1,
            State::TimedOut => 2,
            State::CouldNotRun => 2,
        },
        Outcome::Awaiting { .. } => 0,
        Outcome::Refused { .. } => 1,
        Outcome::Unwritten { .. } => 1,
        Outcome::Stopped => 1,
    };
    Ok((text, code))
}

/// The baseline gate a run was asked for. Red is the default: a patching run that cannot show the check
/// failing first cannot show that it fixed anything.
pub(crate) fn baseline_of(flags: &Flags) -> Result<BaselineGate, String> {
    match flags.named.get("baseline").map(String::as_str) {
        None | Some("red") => Ok(BaselineGate::Red),
        Some("off") => Ok(BaselineGate::Off),
        Some(other) => Err(format!("`--baseline {other}`: say red or off")),
    }
}

/// The attack a run was asked for, read in one place so the daemon and the CLI cannot disagree about
/// whether a security case has a second oracle.
pub(crate) fn attack_of(flags: &Flags) -> Option<String> {
    flags
        .named
        .get("attack")
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

/// Whether a run records the failure it produced. Default is on: a corpus that depends on remembering a
/// flag is empty when it matters.
pub(crate) fn case_mode(flags: &Flags) -> Result<crate::cases::Mode, String> {
    match flags.named.get("case") {
        None => Ok(crate::cases::Mode::Auto),
        Some(text) => crate::cases::Mode::parse(text),
    }
}

/// Run the graph: attempts, the return path, and the declared way out. The command a person uses when the
/// task is not one the model gets right first time.
fn run_graph(args: &[String]) -> Result<(String, i32), String> {
    let flags = parse_with(
        args,
        &[
            "root",
            "profile",
            "policy",
            "verify",
            "state",
            "session",
            "cage",
            "program",
            "model",
            "endpoint",
            "key",
            "temperature",
            "max-tokens",
            "context",
            "thinking",
            "attempts",
            "minutes",
            "budget-tokens",
            "branch",
            "baseline",
            "shadow",
            "config",
            "memory",
            "harness",
            "protect",
            "case",
            "attack",
            "check-minutes",
        ],
        &["strict-model"],
    )?;
    let goal = goal(&flags)?;
    let score_state = state_of(&flags)?;
    // The budget is read and refused before the engine is assembled or the model is asked: a run that
    // cannot spend what it was told to is a mistake worth reporting while it costs nothing.
    let budget_tokens = match flags.named.get("budget-tokens") {
        Some(value) => {
            let tokens = value
                .parse::<u64>()
                .map_err(|_| format!("`--budget-tokens {value}` is not a number"))?;
            let max_tokens = match flags.named.get("max-tokens") {
                Some(value) => Some(
                    value
                        .parse::<u64>()
                        .map_err(|_| format!("`--max-tokens {value}` is not a number"))?,
                ),
                None => None,
            };
            Some(crate::graph::budget_tokens(tokens, max_tokens)?)
        }
        None => None,
    };
    let gate = baseline_of(&flags)?;
    let attack = flags.named.get("attack").cloned();
    let attack_for_runner = attack.clone();
    let (mut engine, answerer, harness, judge) = assemble(&flags, "run")?;
    let protect = protect_of(&flags, engine.root())?;
    let mut session = engine.session()?;

    let attempts = match flags.named.get("attempts") {
        Some(value) => value
            .parse::<u32>()
            .map_err(|_| format!("`--attempts {value}` is not a number"))?,
        None => engine.profile().attempts,
    };
    let minutes = match flags.named.get("minutes") {
        Some(value) => value
            .parse::<u64>()
            .map_err(|_| format!("`--minutes {value}` is not a number"))?,
        None => engine.profile().minutes,
    };
    let branch = match flags.named.get("branch") {
        Some(value) => {
            let width = value
                .parse::<u32>()
                .map_err(|_| format!("`--branch {value}` is not a number"))?;
            if !(1..=4).contains(&width) {
                return Err(format!(
                    "`--branch {width}`: say 1 to 4; wider is not better, it is costlier"
                ));
            }
            width
        }
        None => 1,
    };
    let bounds = Bounds::new(attempts, minutes).with_width(branch);
    let bounds = match budget_tokens {
        Some(tokens) => bounds.with_tokens(tokens),
        None => bounds,
    };
    let call_tokens = flags
        .named
        .get("max-tokens")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(crate::graph::DEFAULT_CALL_TOKENS);

    let before = session.len();
    let policy_name = engine.policy_name().to_string();
    let model_name = answerer.describe();
    let memo = remembered(
        &flags,
        &goal,
        (engine.profile().context_tokens / 10).min(2_000),
    );
    let mut runner = Runner::new(
        engine.parts(
            answerer.harness(),
            judge
                .as_ref()
                .map(|judge| judge as &dyn crate::judge::Evaluator),
        ),
        &mut session,
    )
    .with_bounds(bounds)
    .with_call_tokens(call_tokens)
    .with_baseline(gate)
    .with_protect(protect)
    .with_scoreboard(&score_state)
    .with_identity(model_name, policy_name);
    if let Some(attack) = &attack_for_runner {
        runner = runner.with_attack(attack.clone());
    }
    if let (Some(section), Some(writer)) = (memo.section.clone(), memo.memory.as_ref()) {
        runner = runner.with_memory(Remembering {
            section: Section::new(section.0, section.1),
            writer,
            topics: memo.topics.clone(),
        });
    }
    let walked = runner.run().map_err(|fault| {
        format!(
            "the run ended in a {} fault: {}",
            fault.class(),
            fault.detail()
        )
    })?;

    // The failure corpus: a giving-up run becomes a task with a pinned tree. A case that cannot be written
    // is a warning and a receipt line, never a failed run; the run already happened.
    let case_line = {
        let check = engine.profile().verify.clone();
        match crate::cases::record(
            &score_state,
            &walked,
            &goal,
            session.id().as_str(),
            &check,
            attack.as_deref(),
            case_mode(&flags)?,
        ) {
            Ok(crate::cases::Recorded::Case(case)) => {
                // The handover was written before the case existed, so the link is appended now rather than
                // left for a reader to infer from a receipt line.
                let _ = session.append(
                    session_tree::Kind::Checkpoint,
                    serde_json::json!({ "node": "handover", "case_id": case.id }),
                );
                format!(
                    "case:    {} recorded ({} files, tree {})\n",
                    case.id,
                    case.files,
                    case.tree.get(..12).unwrap_or(&case.tree)
                )
            }
            Ok(crate::cases::Recorded::Skipped(why)) => {
                format!("case:    not recorded ({why})\n")
            }
            Err(said) => {
                eprintln!("case: not recorded, {said}");
                format!("case:    not recorded ({said})\n")
            }
        }
    };

    if let Err(said) = engine.save_cache() {
        eprintln!("repomap: the cache was not written, {said}");
    }
    let (hits, misses) = (engine.cache().hits(), engine.cache().misses());
    // A pass is evidence, not a procedure. It does not write a proposal by itself: a proposal is the
    // product of `hades distill --run`, where the runbook has survived a replay in a fresh tree.
    // The walker cuts its own history at the end, before it returns, so the entry is already in the log.
    let compacted = session.entries()[before..]
        .iter()
        .any(|entry| entry.kind == session_tree::Kind::Compaction);
    let plan_chars = session.entries()[before..]
        .iter()
        .find(|entry| entry.kind == session_tree::Kind::Plan)
        .map(|entry| {
            entry
                .payload
                .get("plan")
                .and_then(serde_json::Value::as_str)
                .map(|plan| plan.chars().count())
                .unwrap_or(0)
        });
    let branch_line = if walked.branches.is_empty() {
        String::new()
    } else {
        let winner = walked
            .branches
            .iter()
            .find(|candidate| candidate.passed)
            .map(|candidate| candidate.session.as_str())
            .unwrap_or("none");
        format!(
            "branches: {} candidate(s), first pass {winner}\n",
            walked.branches.len()
        )
    };
    let judge_line = match &walked.verdict {
        Some(judged) => match &judged.identity {
            Some(identity) => format!(
                "{}: {} [{}]",
                judged.verdict.name(),
                judged.reason,
                identity.short()
            ),
            None => format!("{}: {}", judged.verdict.name(), judged.reason),
        },
        None if engine.profile().judge => "unjudged (no judged attempt)".to_string(),
        None => "unjudged (the profile did not ask)".to_string(),
    };
    let baseline_line = match &walked.baseline {
        Some(baseline) => format!(
            "baseline: {} (exit {}){}\n",
            baseline.state.name(),
            baseline
                .status
                .map(|status| status.to_string())
                .unwrap_or_else(|| "none".to_string()),
            match &baseline.attack {
                Some(attack) => format!(
                    "\n{}",
                    attack.line().replacen("attack:", "attack (before):", 1)
                ),
                None => String::new(),
            }
        ),
        None => String::new(),
    };
    // The attack is a second oracle, so the receipt carries both sides of it: what the exploit did before the
    // attempt and what it did after. A run with no attack prints neither line.
    let attack_line = match &walked.last_attack {
        Some(report) => format!(
            "{}\n",
            report.line().replacen("attack:", "attack (after):", 1)
        ),
        None => String::new(),
    };
    let ruler_line = match &walked.ruler {
        Some(ruler) if ruler.files.is_empty() && ruler.rules == 0 => {
            format!("ruler:   {} (command only)\n", ruler.short())
        }
        Some(ruler) if ruler.files.is_empty() => format!(
            "ruler:   {} (command only: no protected file matched)\n",
            ruler.short()
        ),
        Some(ruler) => format!(
            "ruler:   {} ({} protected file(s))\n",
            ruler.short(),
            ruler.files.len()
        ),
        None => String::new(),
    };
    // A giving-up run says where its dead end went, so a person can read it rather than guess that memory
    // was told. A run with no memory attached says nothing, because nothing was written.
    let dead_line = match &walked.dead_end {
        Some(path) => format!("dead end: {}\n", path.display()),
        None => String::new(),
    };
    // A run with no boundary says so on its receipt, the way `hades turn` does: a person reading
    // `--cage off` output otherwise sees the same shape as a confined run.
    let cage_line = engine.cage();
    let mut text = format!(
        "session: {} ({} entries, {} appended)\nprofile: {}\npolicy:  {}\nmodel:   {}\nharness: {}\nplan:    {}\njudge:   {judge_line}\n{baseline_line}{attack_line}{ruler_line}cage:    {cage_line}\ncontext: {} tokens\nbounds:  {}\n{branch_line}map:     {hits} hits, {misses} misses\ncompacted: {}\n{case_line}{dead_line}",
        session.id(),
        session.len(),
        session.len() - before,
        engine.profile().id,
        engine.policy_name(),
        answerer.describe(),
        harness.line(),
        match plan_chars {
            Some(chars) => format!("{chars} chars"),
            None => "none".to_string(),
        },
        engine.profile().context_tokens,
        bounds.declared(),
        if compacted {
            "yes, by the harness"
        } else {
            "no"
        },
    );
    text.push_str("\nthe walk:\n");
    for visit in &walked.visits {
        text.push_str(&format!(
            "  {:<10} {:<12} {}\n",
            visit.node,
            format!("{:?}", visit.when),
            visit.why.clone().unwrap_or_default()
        ));
    }
    text.push_str(&format!("\n{}\n", walked.ended.line()));
    // A run that cannot claim acceptance says so on its end line: the difference between a check that
    // passed and a result a judge accepted is a fact about the run, not something the exit code hides.
    let claim = crate::claim::ladder(&walked.claim_facts());
    if !claim.accepted() {
        text.push_str(&format!("{}\n", claim.line()));
    }
    if let Some(cause) = walked.ended.cause() {
        text.push_str(&format!("cause: {}\n", cause.name()));
    }
    let memory_line = match (&walked.recorded, &walked.memory_error) {
        (Some(path), _) => format!("recalled {}, recorded {}", memo.recalled, path.display()),
        (None, Some(said)) if memo.memory.is_some() => {
            format!("recalled {}, recorded none ({said})", memo.recalled)
        }
        (None, _) if memo.memory.is_some() => format!(
            "recalled {}, recorded none (the run was not accepted)",
            memo.recalled
        ),
        (None, _) => format!(
            "recalled {}, not recorded (no record is open)",
            memo.recalled
        ),
    };
    text.push_str(&format!("memory:  {memory_line}\n"));
    for (index, tried) in walked.tried().iter().enumerate() {
        text.push_str(&format!(
            "  attempt {} ({}): {}\n",
            index + 1,
            tried.state(),
            preview(tried.program())
        ));
    }
    // What it cost, per attempt. A graph that reports only its shape tells a person nothing about whether a
    // retry was cheap, and the numbers are already here.
    for (index, usage) in walked.spent.iter().enumerate() {
        if usage.is_known() {
            let rate = if usage.millis > 0 {
                format!(
                    "{:.1} t/s",
                    usage.generated_tokens as f64 / (usage.millis as f64 / 1000.0)
                )
            } else {
                "no timing".to_string()
            };
            let reasoning = if usage.reasoning_chars > 0 {
                format!(", {} chars of reasoning", usage.reasoning_chars)
            } else {
                String::new()
            };
            text.push_str(&format!(
                "  attempt {} spent: {} prompt / {} generated tokens in {} ms ({rate}){reasoning}\n",
                index + 1,
                usage.prompt_tokens,
                usage.generated_tokens,
                usage.millis
            ));
        }
    }
    if let Ended::Awaiting { question, .. } = &walked.ended {
        text.push_str(&format!(
            "answer it with: hades answer {} allow --scope session --state {}\n",
            question.id.0,
            engine.state().display()
        ));
    }
    text.push_str(&format!("log: {}\n", engine.log_path().display()));
    Ok((text, walked.ended.code()))
}

/// The first line of a section, so `context` shows what the model will actually read rather than only how
/// big it was.
fn preview(text: &str) -> String {
    let first = text.lines().next().unwrap_or("");
    let cut = a_boundary(first, 60);
    if cut < first.len() {
        format!("{}…", &first[..cut])
    } else {
        first.to_string()
    }
}

/// The questions, and what came of them. Reads the record, so it shows what any process would see.
fn approvals(args: &[String]) -> Result<(String, i32), String> {
    let flags = parse(args, &["state"])?;
    let state = state_of(&flags)?;
    let broker = OnDisk::open(state.join("approvals.jsonl"))
        .map_err(|fault| format!("could not open the record of questions: {}", fault.detail()))?;
    let questions = broker
        .questions()
        .map_err(|fault| format!("could not read the record of questions: {}", fault.detail()))?;
    if questions.is_empty() {
        return Ok(("no questions have been asked\n".to_string(), 0));
    }
    let mut text = String::new();
    for question in &questions {
        text.push_str(&question_text(question, true));
        text.push('\n');
    }
    Ok((text, 0))
}

/// One question, its options and its answer, in the form both `approvals` and `log --asks` print.
fn question_text(question: &crate::approval::Question, with_session: bool) -> String {
    let request = &question.request;
    let mut text = format!("#{}", request.id.0);
    if with_session {
        text.push_str(&format!("  {}", request.session));
    }
    text.push('\n');
    match &request.ask {
        Some(ask) => {
            text.push_str(&format!("  {}\n", ask.prompt));
            if !ask.why.is_empty() {
                text.push_str(&format!("  why: {}\n", ask.why));
            }
            for choice in &ask.options {
                text.push_str(&format!(
                    "  {} {:<14} {}{}\n",
                    if choice.recommended { "▸" } else { " " },
                    choice.key,
                    choice.label,
                    if choice.recommended {
                        "  (recommended)"
                    } else {
                        ""
                    }
                ));
            }
        }
        None => text.push_str(&format!(
            "  {}\n  why: {}\n",
            request.action, request.reason
        )),
    }
    match &question.answer {
        Some(answer) => {
            let mut line = format!(
                "  -> {} {} by {}",
                answer.decision.name(),
                answer.scope.name(),
                answer.by
            );
            if let Some(option) = &answer.option {
                line.push_str(&format!(" option {option}"));
            }
            if let Some(note) = &answer.note {
                line.push_str(&format!(" note: {note}"));
            }
            text.push_str(&line);
            text.push('\n');
        }
        None => text.push_str("  -> waiting\n"),
    }
    text
}

/// Answer a question. The option key is the normal form; `allow` and `deny` remain for a legacy question
/// with no options, and `other` is the decline-with-a-steer.
fn answer(args: &[String]) -> Result<(String, i32), String> {
    let flags = parse(args, &["scope", "by", "state", "note"])?;
    let id = flags
        .positional
        .first()
        .ok_or_else(|| "answer needs a question id".to_string())?
        .trim_start_matches('#')
        .parse::<u64>()
        .map_err(|_| "the question id is a number".to_string())?;
    let given = flags
        .positional
        .get(1)
        .cloned()
        .ok_or_else(|| "answer needs an option key, allow, or deny".to_string())?;
    let note = flags.named.get("note").map(String::as_str);
    let scope = match flags
        .named
        .get("scope")
        .map(String::as_str)
        .unwrap_or("once")
    {
        "once" => Scope::Once,
        "session" => Scope::Session,
        other => return Err(format!("`{other}` is not a scope: say once or session")),
    };
    let by = flags
        .named
        .get("by")
        .cloned()
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_else(|| "unknown".to_string());
    let state = state_of(&flags)?;
    let broker = OnDisk::open(state.join("approvals.jsonl"))
        .map_err(|fault| format!("could not open the record of questions: {}", fault.detail()))?;
    let questions = broker
        .questions()
        .map_err(|fault| format!("could not read the record of questions: {}", fault.detail()))?;
    let question = questions
        .iter()
        .find(|question| question.request.id == PendingId(id))
        .ok_or_else(|| format!("no question #{id} was ever asked"))?;
    let ask = question.request.ask.clone();
    let option = match given.as_str() {
        "allow" => (Decision::Allow, None),
        "deny" => (Decision::Deny, None),
        "other" => {
            let decision = match ask.as_ref().map(|ask| ask.kind) {
                Some(crate::question::AskKind::Choice) => Decision::Allow,
                _ => Decision::Deny,
            };
            (decision, Some("other"))
        }
        key if ask.as_ref().is_some_and(|ask| ask.option(key).is_some()) => {
            (Decision::Allow, Some(key))
        }
        other => {
            let keys = ask
                .as_ref()
                .map(|ask| {
                    ask.options
                        .iter()
                        .map(|choice| choice.key.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_else(|| "allow, deny".into());
            return Err(format!("`{other}` is not an option; keys: {keys}"));
        }
    };

    // One writer for the question record: a live daemon owns it and the answer goes to it.
    let payload = serde_json::json!({
        "op": "answer",
        "id": id,
        "decision": option.0.name(),
        "scope": scope.name(),
        "option": option.1.unwrap_or(""),
        "note": note.unwrap_or(""),
        "by": by,
    });
    if let Some(socket) = crate::daemon::live(&state) {
        let reply =
            crate::daemon::ask(&socket, payload).map_err(|fault| fault.detail().to_string())?;
        let code = if reply.get("ok").and_then(serde_json::Value::as_bool) == Some(true) {
            0
        } else {
            1
        };
        return Ok((format!("{reply}\n"), code));
    }

    let answer = match option.1 {
        Some("other") => broker
            .resolve_answer(PendingId(id), option.0, scope, Some("other"), note, &by)
            .map_err(|fault| fault.detail().to_string())?,
        Some(key) => broker
            .resolve_choice(PendingId(id), key, note, &by)
            .map_err(|fault| fault.detail().to_string())?,
        None => broker
            .resolve(PendingId(id), option.0, scope, &by)
            .map_err(|fault| fault.detail().to_string())?,
    };

    // Learn from the answer where the session log can tell us the root. A missing log is not an error:
    // the answer is recorded, it just cannot teach this root.
    if let Some(ask) = &ask
        && let Ok(facts) = crate::distill::load(&state, &question.request.session)
    {
        let _ = crate::taste::record(&state, &facts.root, ask, &answer);
    }

    Ok((
        format!(
            "question #{id} answered {} for {} by {}\n{}\n",
            answer.decision.name(),
            answer.scope.name(),
            answer.by,
            if answer.scope == Scope::Session {
                "the next turn in that session will not ask again"
            } else {
                "the next turn will ask again"
            }
        ),
        0,
    ))
}

/// Every question and answer for one session, as the record has them.
fn log_cmd(args: &[String]) -> Result<(String, i32), String> {
    let flags = parse_with(args, &["state"], &["asks"])?;
    if !flags.named.contains_key("asks") {
        return Err("log takes one form: `hades log --asks <session>`".to_string());
    }
    let session = flags
        .positional
        .first()
        .ok_or_else(|| "log --asks needs a session id".to_string())?;
    let state = state_of(&flags)?;
    let broker = OnDisk::open(state.join("approvals.jsonl"))
        .map_err(|fault| format!("could not open the record of questions: {}", fault.detail()))?;
    let questions = broker
        .questions()
        .map_err(|fault| format!("could not read the record of questions: {}", fault.detail()))?;
    let mine: Vec<_> = questions
        .iter()
        .filter(|question| &question.request.session == session)
        .collect();
    if mine.is_empty() {
        return Ok((format!("no questions for {session}\n"), 0));
    }
    let mut text = String::new();
    for question in mine {
        text.push_str(&question_text(question, false));
        text.push('\n');
    }
    Ok((text, 0))
}

/// What the answers have taught, and how to forget it.
fn taste_cmd(args: &[String]) -> Result<(String, i32), String> {
    let flags = parse(args, &["state"])?;
    let state = state_of(&flags)?;
    match flags.positional.first().map(String::as_str) {
        None => {
            let entries = crate::taste::read(&state)
                .map_err(|fault| format!("could not read taste: {}", fault.detail()))?;
            if entries.is_empty() {
                return Ok(("taste: no answers recorded yet\n".to_string(), 0));
            }
            let mut text = String::new();
            for (root, family) in crate::taste::families(&entries) {
                let learned = crate::taste::learn(&entries, &root, &family);
                let count = entries
                    .iter()
                    .filter(|entry| entry.root == root && entry.kind == family)
                    .count();
                text.push_str(&format!("{root}\n  {family}: {count} answers"));
                if let Some(key) = &learned.recommendation {
                    text.push_str(&format!(", recommendation {key}"));
                }
                if learned.streak > 0 {
                    text.push_str(&format!(", {} in a row agreed", learned.streak));
                }
                text.push_str(if learned.auto {
                    ", asking is off\n"
                } else {
                    "\n"
                });
            }
            for (root, family, count) in crate::taste::deny_proposals(&entries) {
                text.push_str(&format!(
                    "proposal: {root} {family}: {count} declines with Other; consider a never rule\n"
                ));
            }
            Ok((text, 0))
        }
        Some("forget") => {
            let which = flags
                .positional
                .get(1)
                .ok_or_else(|| "taste forget needs a line number or all".to_string())?;
            let removed = if which == "all" {
                crate::taste::forget(&state, None)
            } else {
                let index = which
                    .parse::<usize>()
                    .map_err(|_| "a taste line is a number, or all".to_string())?;
                crate::taste::forget(&state, Some(index))
            }
            .map_err(|fault| fault.detail().to_string())?;
            Ok((format!("forgot {removed} taste line(s)\n"), 0))
        }
        Some(other) => Err(format!(
            "`{other}` is not a taste command: try `taste` or `taste forget <n|all>`"
        )),
    }
}

/// Own the sessions under one state directory, in the foreground.
///
/// Held in the foreground on purpose. A daemon that forks itself away is a daemon whose failures go somewhere
/// nobody is reading, and this one has nothing to hide: every fact it holds is a file under the state
/// directory, so it can be killed at any moment and restarted without a word of migration.
fn daemon(args: &[String]) -> Result<(String, i32), String> {
    let flags = parse(args, &["state", "socket"])?;
    let state = state_of(&flags)?;
    let socket = match flags.named.get("socket") {
        Some(path) => PathBuf::from(path),
        None => crate::daemon::Daemon::socket_path(&state),
    };

    let daemon =
        crate::daemon::Daemon::bind(&state, &socket).map_err(|fault| fault.detail().to_string())?;
    // Announced before the loop, because a client that waits for the reply this command returns at the end
    // would wait forever. Nothing depends on reading it: the socket is also in `daemon.json`.
    eprintln!(
        "hades daemon: listening on {}\npublishing {}\n",
        daemon.socket().display(),
        state.join("daemon.json").display()
    );
    daemon.serve().map_err(|fault| fault.detail().to_string())?;
    Ok((format!("the daemon on {} stopped\n", socket.display()), 0))
}

/// The ACP front end. It owns stdio for its lifetime, so it returns no text: every byte is a JSON-RPC
/// message written by [`crate::acp::serve`]. `input` is the lock the caller already holds.
fn acp(args: &[String], input: &mut dyn BufRead) -> Result<(String, i32), String> {
    let flags = parse_with(
        args,
        &[
            "root", "state", "config", "profile", "policy", "verify", "program",
        ],
        &[],
    )?;
    let config = config_of(&flags)?;
    let root = root_with(&flags, config.as_ref())?;
    let state = state_of(&flags)?;
    let policy = flags
        .named
        .get("policy")
        .cloned()
        .unwrap_or_else(|| "guarded".to_string());
    let options = crate::acp::Options {
        root,
        state,
        config: flags.named.get("config").map(std::path::PathBuf::from),
        profile: flags.named.get("profile").cloned(),
        policy,
        verify: flags.named.get("verify").cloned(),
        programs: flags.repeated.get("program").cloned().unwrap_or_default(),
    };
    let code = crate::acp::serve(options, input);
    Ok((String::new(), code))
}

/// The HTTP surface. A thin client of the daemon: it binds a listener and turns each request into an op on
/// the socket, or a read of the same files the daemon answers from.
fn http(args: &[String]) -> Result<(String, i32), String> {
    let flags = parse_with(args, &["bind", "state", "token-file"], &["once"])?;
    let state = state_of(&flags)?;
    let bind = match flags.named.get("bind") {
        Some(text) => text.parse::<std::net::SocketAddr>().map_err(|_| {
            format!(
                "`--bind {text}` is not an IP:port; a hostname is not resolved, so the address the \
                 process bound is the address a client can check"
            )
        })?,
        None => std::net::SocketAddr::from(([127, 0, 0, 1], 8787)),
    };
    // Mandatory on every bind, loopback included: a page the browser visits can POST to 127.0.0.1, so
    // loopback is not a trust boundary. A missing token is generated once and reused, so a restart does
    // not break every client.
    let token = token_of(&flags, &state)?;
    let once = flags.named.contains_key("once");
    crate::httpd::Server {
        state,
        bind,
        token,
        once,
    }
    .serve()?;
    Ok((String::new(), 0))
}

/// A token from the environment or from a file's first line, never from argv: argv is visible in `ps` for
/// the life of the process, so a token passed as a flag would be a token in every process listing.
fn token_of(flags: &Flags, state: &std::path::Path) -> Result<String, String> {
    if let Ok(token) = std::env::var("HADES_HTTP_TOKEN") {
        let token = token.trim().to_string();
        if !token.is_empty() {
            return Ok(token);
        }
    }
    if let Some(path) = flags.named.get("token-file") {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("could not read {path}: {error}"))?;
        let token = text.lines().next().unwrap_or_default().trim().to_string();
        if token.is_empty() {
            return Err(format!(
                "{path} is empty; its first line has to be the token"
            ));
        }
        return Ok(token);
    }
    // Reuse before generate: rotating on every start would break every client on every restart.
    let path = state.join("http.token");
    if let Ok(text) = std::fs::read_to_string(&path) {
        let token = text.lines().next().unwrap_or_default().trim().to_string();
        if !token.is_empty() {
            return Ok(token);
        }
    }
    let token = new_token()?;
    crate::scratch::state_dir(state)
        .map_err(|error| format!("could not make {}: {error}", state.display()))?;
    // Created with the mode, not chmod'd after: a token that exists at the umask for even a moment is a
    // token another local user can read. `private_file` stays as the assertion that it holds.
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .map_err(|error| format!("could not write {}: {error}", path.display()))?;
        file.write_all(format!("{token}\n").as_bytes())
            .map_err(|error| format!("could not write {}: {error}", path.display()))?;
    }
    crate::scratch::private_file(&path)
        .map_err(|error| format!("could not protect {}: {error}", path.display()))?;
    eprintln!(
        "hades http: no token configured; generated one at {} (mode 0600)",
        path.display()
    );
    Ok(token)
}

/// A fresh token from the kernel's random source. Hex, because a token is pasted into a header and a
/// shell by hand as often as it is read by a program.
fn new_token() -> Result<String, String> {
    use std::io::Read as _;
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut bytes))
        .map_err(|error| format!("could not read /dev/urandom for a token: {error}"))?;
    let mut text = String::with_capacity(64);
    for byte in bytes {
        text.push_str(&format!("{byte:02x}"));
    }
    Ok(text)
}

/// What the daemon knows; or, when none is running, what the logs say. The state is on disk either way, so
/// asking without a daemon is a read and not a failure.
/// The Telegram surface: a message becomes one of the daemon's ops. Nothing here chooses a root, a
/// profile or a model from the message; those come from the config the daemon already reads.
fn gateway(args: &[String]) -> Result<(String, i32), String> {
    let flags = parse_with(args, &["state", "config"], &["once"])?;
    let state = state_of(&flags)?;
    let config = config_path(&flags)?;
    let settings = crate::gateway::settings(&config)?;
    let token = crate::model::secret("HADES_TELEGRAM_TOKEN")?;
    let socket = crate::daemon::live(&state).ok_or_else(|| {
        format!(
            "no daemon is running under {}; start one with `hades daemon --state {}`",
            state.display(),
            state.display()
        )
    })?;
    let once = flags.named.contains_key("once");
    crate::gateway::serve(
        crate::gateway::telegram::Telegram::new(token),
        socket,
        state,
        settings,
        once,
    )?;
    Ok((String::new(), 0))
}

fn status(args: &[String]) -> Result<(String, i32), String> {
    let flags = parse(args, &["state"])?;
    let state = state_of(&flags)?;

    let (sessions, heading) = match crate::daemon::live(&state) {
        Some(socket) => {
            let reply = crate::daemon::ask(&socket, serde_json::json!({"op": "status"}))
                .map_err(|fault| fault.detail().to_string())?;
            let sessions = reply
                .get("sessions")
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default();
            (
                sessions,
                format!(
                    "daemon: pid {} on {}, up {} ms\n",
                    reply
                        .get("pid")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0),
                    socket.display(),
                    reply
                        .get("uptime_ms")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0)
                ),
            )
        }
        None => (
            crate::daemon::sessions_on_disk(&state).map_err(|fault| fault.detail().to_string())?,
            format!(
                "daemon: none running under {} (these are the logs)\n",
                state.display()
            ),
        ),
    };

    if sessions.is_empty() {
        return Ok((format!("{heading}no sessions have been written\n"), 0));
    }
    let mut text = format!(
        "{heading}\n{:<14} {:>7}  {:<18} {}\n",
        "session", "entries", "state", "run"
    );
    for session in &sessions {
        let run = session
            .get("run")
            .and_then(serde_json::Value::as_u64)
            .map(|run| format!("run {run}"))
            .unwrap_or_default();
        text.push_str(&format!(
            "{:<14} {:>7}  {:<18} {}\n",
            string_of(session, "session"),
            session
                .get("entries")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            string_of(session, "state"),
            run
        ));
    }
    Ok((text, 0))
}

/// Hand a run to the daemon and print what comes back, entry by entry, as it is written.
fn submit(args: &[String]) -> Result<(String, i32), String> {
    let flags = parse_with(
        args,
        &[
            "root",
            "profile",
            "policy",
            "verify",
            "state",
            "session",
            "cage",
            "shadow",
            "model",
            "endpoint",
            "key",
            "program",
            "temperature",
            "max-tokens",
            "context",
            "thinking",
            "attempts",
            "minutes",
            "budget-tokens",
            "branch",
            "baseline",
            "protect",
            "config",
            "memory",
            "harness",
            "case",
            "attack",
            "check-minutes",
        ],
        &["strict-model"],
    )?;
    // Refused here as well as in the daemon: a person who typed a budget of zero should learn it from the
    // terminal they typed it in, not from a round trip.
    if let Some(value) = flags.named.get("budget-tokens") {
        let tokens = value
            .parse::<u64>()
            .map_err(|_| format!("`--budget-tokens {value}` is not a number"))?;
        // The cap this submit will send, when it names one: the floor depends on it, and checking against
        // the default while the request asks for another cap is how a budget that cannot run got through.
        let cap = flags
            .named
            .get("max-tokens")
            .map(|value| {
                value
                    .parse::<u64>()
                    .map_err(|_| format!("`--max-tokens {value}` is not a number"))
            })
            .transpose()?;
        crate::graph::budget_tokens(tokens, cap)?;
    }
    let state = state_of(&flags)?;
    let socket = crate::daemon::live(&state).ok_or_else(|| {
        format!(
            "no daemon is running under {}; start one with `hades daemon --state {}`",
            state.display(),
            state.display()
        )
    })?;

    let goal = flags
        .positional
        .first()
        .cloned()
        .ok_or_else(|| "submit needs a goal".to_string())?;
    let mut request = serde_json::Map::new();
    request.insert("op".to_string(), serde_json::json!("submit"));
    request.insert("goal".to_string(), serde_json::json!(goal));
    // The state directory the daemon is reached through, so a run cannot be described to one directory and
    // recorded in another.
    request.insert(
        "state".to_string(),
        serde_json::json!(state.display().to_string()),
    );
    for (key, value) in &flags.named {
        request.insert(key.clone(), serde_json::json!(value));
    }
    request.insert(
        "program".to_string(),
        serde_json::json!(flags.repeated.get("program").cloned().unwrap_or_default()),
    );
    request.insert(
        "protect".to_string(),
        serde_json::json!(flags.repeated.get("protect").cloned().unwrap_or_default()),
    );

    let (mut text, last) = follow_socket(&socket, &serde_json::Value::Object(request))?;
    let code = last
        .get("code")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(1) as i32;
    text.push_str(&format!(
        "\n{}\n",
        match last.get("ended").and_then(serde_json::Value::as_str) {
            Some(ended) => ended.to_string(),
            None => match last.get("fault").and_then(serde_json::Value::as_str) {
                Some(fault) => format!("the run ended in a fault: {fault}"),
                None => "the daemon said nothing about how it ended".to_string(),
            },
        }
    ));
    Ok((text, code))
}

/// Print a session's entries, then follow the ones written next. Ctrl-C detaches; the run does not care,
/// because it is the daemon's and not this terminal's.
fn attach(args: &[String]) -> Result<(String, i32), String> {
    use std::io::{BufRead, BufReader, Write};

    let flags = parse(args, &["state"])?;
    let session = flags
        .positional
        .first()
        .cloned()
        .ok_or_else(|| "attach needs a session".to_string())?;
    let state = state_of(&flags)?;
    let socket = crate::daemon::live(&state).ok_or_else(|| {
        format!(
            "no daemon is running under {}; start one with `hades daemon --state {}`",
            state.display(),
            state.display()
        )
    })?;

    let mut stream = std::os::unix::net::UnixStream::connect(&socket)
        .map_err(|error| format!("could not reach {}: {error}", socket.display()))?;
    writeln!(
        stream,
        "{}",
        serde_json::json!({"op": "watch", "session": session, "from": "start"})
    )
    .map_err(|error| format!("could not send the request: {error}"))?;

    // Printed as they arrive rather than returned, because this is a view of a file that is still being
    // written and there is no end to return at.
    for line in BufReader::new(stream).lines() {
        let line = line.map_err(|error| format!("the daemon went away: {error}"))?;
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let Some(entry) = value.get("entry") else {
            if let Some(error) = value.get("error").and_then(serde_json::Value::as_str) {
                return Err(error.to_string());
            }
            continue;
        };
        println!(
            "  #{:<3} {:<18} {}",
            entry
                .get("id")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            string_of(entry, "kind"),
            brief(entry.get("payload").unwrap_or(&serde_json::Value::Null))
        );
        let _ = std::io::stdout().flush();
    }
    Ok((String::new(), 0))
}

/// One request whose answer is a stream: the entries as they are written, then one line saying how it went.
/// A `null` line is skipped, so a client can be written against a daemon that says nothing between entries.
fn follow_socket(
    socket: &Path,
    request: &serde_json::Value,
) -> Result<(String, serde_json::Value), String> {
    use std::io::{BufRead, BufReader, Write};

    let mut stream = std::os::unix::net::UnixStream::connect(socket)
        .map_err(|error| format!("could not reach {}: {error}", socket.display()))?;
    writeln!(stream, "{request}")
        .map_err(|error| format!("could not send the request: {error}"))?;

    let mut text = String::new();
    let mut last = serde_json::Value::Null;
    for line in BufReader::new(stream).lines() {
        let line = line.map_err(|error| format!("the daemon went away: {error}"))?;
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(error) => return Err(format!("the daemon said something unreadable: {error}")),
        };
        if let Some(error) = value.get("error").and_then(serde_json::Value::as_str) {
            return Err(error.to_string());
        }
        match value.get("entry") {
            Some(entry) => text.push_str(&format!(
                "  #{:<3} {:<18} {}\n",
                entry
                    .get("id")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0),
                string_of(entry, "kind"),
                brief(entry.get("payload").unwrap_or(&serde_json::Value::Null))
            )),
            None => {
                let finished = value
                    .get("finished")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                if finished {
                    last = value;
                    break;
                }
                if let Some(run) = value.get("run").and_then(serde_json::Value::as_u64) {
                    text.push_str(&format!(
                        "run {run} on session {}\n",
                        string_of(&value, "session")
                    ));
                }
            }
        }
    }
    Ok((text, last))
}

fn string_of(value: &serde_json::Value, key: &str) -> String {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Put a tree back the way a shadow snapshot found it.
///
/// The map for a tree, and what the cache did, on one line a person can read.
///
/// This exists to make the cache observable from outside a test: run it twice against the same `--state` and
/// the second line is the measurement. Without a `--state` there is nowhere to keep a cache between
/// processes and every run is cold, which is the honest behaviour and not a special case.
fn map(args: &[String]) -> Result<(String, i32), String> {
    let flags = parse(args, &["root", "state", "tokens", "config"])?;
    let goal = flags.positional.join(" ");
    let root = root_of(&flags)?;
    let tokens: usize = match flags.named.get("tokens") {
        Some(count) => count
            .parse()
            .map_err(|_| format!("--tokens wants a number, not {count}"))?,
        None => 2000,
    };
    let cache_path = match flags.named.get("state") {
        Some(dir) => Some(guarded(Path::new(dir))?.join("repomap.json")),
        None => None,
    };

    let mut cache = match &cache_path {
        Some(path) => match Cache::load(path) {
            Ok(cache) => cache,
            // A cache that will not read is a cold run and a line saying why, never a failed map: the map is
            // the product and the cache is an optimisation of it.
            Err(fault) => {
                eprintln!("map: cold, {}", fault.detail());
                Cache::new()
            }
        },
        None => Cache::new(),
    };

    let start = std::time::Instant::now();
    let built = Map::build_cached(&root, &seed_words(&goal), &mut cache)
        .map_err(|fault| fault.detail().to_string())?;
    let took = start.elapsed();
    let hits = cache.hits();
    let misses = cache.misses();

    if let Some(path) = &cache_path
        && let Err(fault) = cache.save(path)
    {
        eprintln!("map: the cache was not written, {}", fault.detail());
    }

    let text = built.render(tokens);
    Ok((
        format!(
            "{} files walked, {hits} hits {misses} misses in {took:.3?}\n\n{text}",
            built.walked()
        ),
        0,
    ))
}

/// A deliberate act, and only ever a deliberate one: a failed check does not roll anything back on its own,
/// because the next attempt has to see what the last program actually left behind. Rolling back is for a
/// person who has read the log and decided.
/// Fork a session at an entry: a new log carrying the chain up to that point verbatim, with a meta entry
/// naming the source. The source is read and never written. Shared by the CLI and ACP so a fork means one
/// thing no matter which door it came through.
pub(crate) fn fork_session(
    state: &Path,
    name: &str,
    at: Option<u64>,
) -> Result<(String, PathBuf, usize), String> {
    let store = session_tree::FileStore::new(state.join("sessions"));
    use session_tree::SessionStore as _;
    let id = session_tree::SessionId::new(name.to_string());
    if !store.exists(&id) {
        return Err(format!(
            "there is no session `{name}` under {}",
            state.display()
        ));
    }
    let session = session_tree::Session::load(store.clone(), session_tree::SystemClock, id)
        .map_err(|fault| format!("could not read session `{name}`: {fault}"))?;
    let child = session
        .fork(at.map(session_tree::EntryId))
        .map_err(|fault| format!("could not fork session `{name}`: {fault}"))?;
    let new_id = child.id().as_str().to_string();
    let path = store.path_of(child.id());
    let length = child.len();
    Ok((new_id, path, length))
}

/// The door to [`fork_session`]. Forking is a read of one log and a write of another; nothing in the run
/// path is touched, so this needs no model, no daemon and no policy.
fn fork(args: &[String]) -> Result<(String, i32), String> {
    let flags = parse(args, &["at", "state"])?;
    let name = flags
        .positional
        .first()
        .ok_or_else(|| "fork needs a session to fork".to_string())?;
    let at = match flags.named.get("at") {
        Some(number) => Some(
            number
                .trim_start_matches('#')
                .parse::<u64>()
                .map_err(|_| format!("`--at {number}` is not an entry number"))?,
        ),
        None => None,
    };
    let state = state_of(&flags)?;
    let (new_id, path, length) = fork_session(&state, name, at)?;
    Ok((
        format!(
            "forked {name} at {} -> {new_id}\n{length} entries\nlog: {}\n",
            at.map(|at| format!("#{at}"))
                .unwrap_or_else(|| "the head".to_string()),
            path.display()
        ),
        0,
    ))
}

fn undo(args: &[String]) -> Result<(String, i32), String> {
    let flags = parse(args, &["to", "state", "root"])?;
    let session = flags
        .positional
        .first()
        .ok_or_else(|| "undo needs a session to undo".to_string())?
        .clone();
    let root = root_of(&flags)?;
    let state = state_of(&flags)?;

    let store = Shadows::new(state.join("shadows"));
    let id = match flags.named.get("to").map(String::as_str) {
        None | Some("latest") => store.latest(&session).ok_or_else(|| {
            format!(
                "session `{session}` has no snapshots under {}",
                store.store().display()
            )
        })?,
        Some(n) => SnapshotId::new(
            session.clone(),
            n.parse::<u64>()
                .map_err(|_| format!("`--to {n}` is not a snapshot number"))?,
        ),
    };

    let subject = Subject::new(session.clone(), &root);
    let restored = store
        .restore(&subject, &id)
        .map_err(|fault| fault.detail().to_string())?;

    Ok((
        format!(
            "restored {} over {}\n{}\n",
            id.label(),
            subject.root().display(),
            restored.line()
        ),
        0,
    ))
}

/// What the log carried into this turn, printed because "it resumed" is a claim and this is the number that
/// makes it checkable rather than asserted.
fn history(learned: &Learned) -> String {
    if learned.is_empty() {
        return "nothing carried over from the log".to_string();
    }
    let mut said = format!(
        "{} tried, {} accepted",
        learned.tries().len(),
        learned.constraints().len()
    );
    if learned.summary().is_some() {
        said.push_str(", and a compacted summary");
    }
    said
}

/// A one-line look at an entry's payload, so the CLI shows what was recorded without dumping JSON.
fn brief(payload: &serde_json::Value) -> String {
    let mut parts = Vec::new();
    for key in [
        "tool",
        "program",
        "snapshot",
        "state",
        "decision",
        "by",
        "question",
        "check",
        "verdict",
        "reason",
        "plan",
        "goal",
        "node",
        "edge",
        "condition",
        "ended",
        "attempt",
    ] {
        if let Some(value) = payload.get(key) {
            let text = match value {
                serde_json::Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            parts.push(format!("{key}={}", text.replace('\n', " ⏎ ")));
        }
    }
    let line = parts.join("  ");
    if line.len() > 160 {
        format!("{}…", &line[..a_boundary(&line, 157)])
    } else {
        line
    }
}

/// Cut a string without cutting a character in half.
fn a_boundary(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// Where a CLI turn keeps its sessions. The CLI is allowed to read the environment; the library is not,
/// which is why this lives here and not in a seam.
fn default_state() -> Result<PathBuf, String> {
    hades_home()
}

#[cfg(test)]
mod gate_tests {
    #[test]
    fn a_corpus_that_could_not_run_is_not_a_pass() {
        assert!(super::gate_refusal(0).is_none());
        let regression = super::gate_refusal(1).expect("a failing corpus refuses adoption");
        assert!(regression.contains("breaks a pinned case"), "{regression}");
        let broken = super::gate_refusal(2).expect("a corpus that could not run refuses adoption");
        assert!(broken.contains("could not be replayed"), "{broken}");
    }
}

#[cfg(test)]
mod profile_model_tests {
    use super::*;

    fn config(model: &str) -> crate::config::Config {
        crate::config::Config {
            provider: "test".to_string(),
            model: model.to_string(),
            root: PathBuf::from("/tmp/hades-profile-model/root"),
            vault: PathBuf::from("/tmp/hades-profile-model/vault"),
            mcp: Default::default(),
        }
    }

    fn flags(model: Option<&str>) -> Flags {
        let mut named = std::collections::BTreeMap::new();
        if let Some(model) = model {
            named.insert("model".to_string(), model.to_string());
        }
        Flags {
            positional: Vec::new(),
            named,
            repeated: std::collections::BTreeMap::new(),
        }
    }

    #[test]
    fn a_profile_model_outranks_the_config() {
        let answerer = answerer(
            &flags(None),
            Some(&config("not-a-preset")),
            None,
            Some("nanbeige-3b"),
        )
        .expect("the profile's preset resolves");
        assert!(answerer.describe().contains("nanbeige-3b"));
    }

    #[test]
    fn the_flag_outranks_the_profile_model() {
        let answerer = answerer(
            &flags(Some("bonsai-27b")),
            Some(&config("not-a-preset")),
            None,
            Some("nanbeige-3b"),
        )
        .expect("the flag's preset resolves");
        assert!(answerer.describe().contains("bonsai-27b"));
    }

    #[test]
    fn with_no_profile_model_the_config_is_used() {
        let answerer = answerer(&flags(None), Some(&config("nanbeige-3b")), None, None)
            .expect("the config's preset resolves");
        assert!(answerer.describe().contains("nanbeige-3b"));
    }
}
