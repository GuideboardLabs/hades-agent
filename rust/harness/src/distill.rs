//! Turn a verified session into a runbook, then prove the runbook in a fresh tree.
//!
//! This is the harness-level version of verified trajectory rewriting: a successful run is evidence, not
//! yet a procedure. The transcript can contain leftover state, harness-control messages, and the check
//! itself. A runbook is the minimal, portable description of what worked. It becomes a skill proposal only
//! after a clean replay against the same check.
//!
//! The planner never sees the check command. The gate below refuses any runbook that contains it, any
//! absolute personal path, a secret-shaped token, or a missing section. The replay is the second gate.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::model::OpenAi;

/// One tool turn, read back from the session log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub n: usize,
    pub tool: String,
    pub program: String,
    pub output: String,
    pub state: String,
    pub tokens: u64,
    /// The check turn that passed, or the last action turn before it. These are the turns a runbook should
    /// describe; observation-heavy turns can be summarized away.
    pub decisive: bool,
}

/// What a finished session says, without reading the whole transcript twice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facts {
    pub session: String,
    pub goal: String,
    pub root: PathBuf,
    pub profile: String,
    pub check: String,
    pub check_state: String,
    pub accepted: bool,
    /// The terminal node the log recorded, when it recorded one.
    pub ended: Option<String>,
    /// The typed cause of a giving-up, as the walk wrote it. `None` when the run did not give up, and for a
    /// log written before the cause was recorded.
    pub exhausted: Option<String>,
    /// The human sentence the walk ended with, which is the only place a token or time overrun is explained.
    pub reason: Option<String>,
    /// The last verdict the log recorded, by its written name. `None` for a log written before the
    /// checkpoint carried one.
    pub verdict: Option<String>,
    pub turns: Vec<Turn>,
    pub snapshot: Option<PathBuf>,
}

impl Facts {
    /// The ladder's view of this log. Every fact is read from an entry the walk already wrote; nothing is
    /// inferred from exit codes or from the sentence a surface printed.
    pub fn claim(&self) -> crate::claim::Facts {
        crate::claim::Facts {
            attempted: !self.turns.is_empty(),
            check: crate::verify::State::from_name(&self.check_state),
            verdict: self
                .verdict
                .as_deref()
                .and_then(crate::judge::Verdict::from_name),
            accepted: self.accepted,
        }
    }
}

/// Load a session by id from `<state>/sessions/<id>.jsonl`.
pub fn load(state: &Path, session: &str) -> Result<Facts, String> {
    let path = state.join("sessions").join(format!("{session}.jsonl"));
    let text = std::fs::read_to_string(&path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let mut goal = String::new();
    let mut root = PathBuf::new();
    let mut profile = String::new();
    let mut check = String::new();
    let mut check_state = String::new();
    let mut accepted = false;
    let mut ended = None;
    let mut exhausted = None;
    let mut reason = None;
    let mut verdict = None;
    let mut turns: Vec<Turn> = Vec::new();
    let mut open: Option<usize> = None;

    for line in text.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let kind = entry
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let payload = entry.get("payload").unwrap_or(&Value::Null);
        match kind {
            "user" => {
                if let Some(value) = payload.get("goal").and_then(Value::as_str) {
                    goal = value.to_string();
                }
                if let Some(value) = payload.get("root").and_then(Value::as_str) {
                    root = PathBuf::from(value);
                }
                if let Some(value) = payload.get("profile").and_then(Value::as_str) {
                    profile = value.to_string();
                }
            }
            "tool_call" => {
                let tool = payload
                    .get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_string();
                let program = payload
                    .get("program")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let tokens = payload
                    .get("model")
                    .and_then(|model| model.get("generated_tokens"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                turns.push(Turn {
                    n: turns.len() + 1,
                    tool,
                    program,
                    output: String::new(),
                    state: String::new(),
                    tokens,
                    decisive: false,
                });
                open = Some(turns.len() - 1);
            }
            "tool_result" => {
                if let Some(turn) = open.take().and_then(|index| turns.get_mut(index)) {
                    turn.output = payload
                        .get("output")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    turn.state = payload
                        .get("state")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .or_else(|| {
                            payload
                                .get("status")
                                .and_then(Value::as_i64)
                                .map(|status| status.to_string())
                        })
                        .unwrap_or_default();
                }
            }
            "checkpoint" => {
                if let Some(value) = payload.get("check").and_then(Value::as_str) {
                    check_state = value.to_string();
                }
                // The last checkpoint's verdict is the verdict about the last attempt. A terminal
                // checkpoint with no verdict leaves an earlier one in place, which is the attempt the
                // run actually ended on when the terminal node did not judge.
                if let Some(value) = payload.get("verdict").and_then(Value::as_str) {
                    verdict = Some(value.to_string());
                }
                if let Some(value) = payload
                    .get("command")
                    .and_then(Value::as_str)
                    .filter(|value| !value.trim().is_empty())
                {
                    check = value.to_string();
                }
                if payload.get("ended").and_then(Value::as_bool) == Some(true) {
                    let node = payload
                        .get("node")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let condition = payload
                        .get("condition")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if condition == "Passed" || node == "done" {
                        accepted = true;
                    }
                    // The last terminal checkpoint wins: a forked or branched session can carry a candidate's
                    // end before its own, and the run that was asked about is the one at the end of the log.
                    ended = Some(node.to_string());
                    exhausted = payload
                        .get("exhausted")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    reason = payload
                        .get("reason")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                }
            }
            _ => {}
        }
    }

    // The last passing check turn, and the action immediately before it, are the two turns a runbook has to
    // explain. Failed checks are still useful as recovery notes, but they are not the path.
    if let Some(index) = turns
        .iter()
        .rposition(|turn| turn.tool == "verify" && turn.state == "passed")
    {
        turns[index].decisive = true;
        if let Some(previous) = turns[..index]
            .iter()
            .rposition(|turn| turn.tool != "verify")
        {
            turns[previous].decisive = true;
        }
    }

    // A snapshot with no files still has a manifest. That is a valid empty starting tree, not a missing
    // snapshot: the first run in an empty directory produces exactly this shape.
    let snapshot_dir = state.join("shadows").join(session).join("1");
    let snapshot_manifest = state.join("shadows").join(session).join("1.json");
    let snapshot = snapshot_manifest.is_file().then_some(snapshot_dir);

    Ok(Facts {
        session: session.to_string(),
        goal,
        root,
        profile,
        check,
        check_state,
        accepted,
        ended,
        exhausted,
        reason,
        verdict,
        turns,
        snapshot,
    })
}

/// The compact turn map `hades explain` prints and `hades distill` gives the planner.
pub fn turn_table(facts: &Facts) -> String {
    let mut text = format!(
        "session {}  profile {}  goal {}\ncheck {}  accepted {}\n\n",
        facts.session,
        facts.profile,
        one_line(&facts.goal, 120),
        if facts.check.is_empty() {
            "none"
        } else {
            facts.check_state.as_str()
        },
        facts.accepted
    );
    text.push_str("  #  tool     state         tokens  detail\n");
    for turn in &facts.turns {
        let detail = if turn.tool == "verify" {
            "the check".to_string()
        } else {
            one_line(&turn.program, 70)
        };
        text.push_str(&format!(
            "{:>3}  {:<8} {:<13} {:>6}  {}{}\n",
            turn.n,
            turn.tool,
            if turn.state.is_empty() {
                "-"
            } else {
                turn.state.as_str()
            },
            turn.tokens,
            if turn.decisive { "* " } else { "  " },
            detail
        ));
    }
    text
}

/// The transcript a planner may see: check text removed, verify programs withheld, output clipped.
pub fn sanitized(facts: &Facts, cap: usize) -> String {
    let mut text = String::new();
    for turn in &facts.turns {
        if turn.tool == "verify" {
            text.push_str(&format!(
                "\n[verify: {}]\n",
                if turn.state.is_empty() {
                    "ran"
                } else {
                    turn.state.as_str()
                }
            ));
            continue;
        }
        text.push_str(&format!("\n--- {}\n", turn.tool));
        text.push_str(&format!(
            "{}\n",
            crate::tuning::redact(&turn.program, &facts.check)
        ));
        if !turn.output.trim().is_empty() {
            let output = crate::tuning::redact(&turn.output, &facts.check);
            text.push_str(&format!("{}\n", clip(&output, 400)));
        }
        if text.len() > cap {
            break;
        }
    }
    clip(&text, cap)
}

/// The deterministic reviewer. A runbook that fails here never reaches a replay, let alone a proposal.
pub fn gate(name: &str, runbook: &str, check: &str) -> Result<(), String> {
    let name = name.trim();
    if name.len() < 3 || name.len() > 48 {
        return Err("the runbook name must be 3 to 48 characters".to_string());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err("the runbook name must be lowercase letters, digits and hyphens".to_string());
    }
    let text = runbook.trim();
    if text.len() < 200 {
        return Err("the runbook is too short to be a procedure".to_string());
    }
    if text.len() > 8_000 {
        return Err("the runbook is longer than 8,000 characters".to_string());
    }
    for section in ["## When to use", "## Steps", "## Check"] {
        if !text.contains(section) {
            return Err(format!("the runbook is missing `{section}`"));
        }
    }
    let check = check.trim();
    if !check.is_empty() && (text.contains(check) || normalize(text).contains(&normalize(check))) {
        return Err("the runbook contains the check's command".to_string());
    }
    for forbidden in ["/home/", "/Users/", "~/.hades", "HADES-CHECK"] {
        if text.contains(forbidden) {
            return Err(format!("the runbook contains `{forbidden}`"));
        }
    }
    for secret in ["sk-", "ghp_", "xoxb-", "xoxp-", "AKIA", "-----BEGIN"] {
        if text.contains(secret) {
            return Err("the runbook contains a secret-shaped token".to_string());
        }
    }
    Ok(())
}

/// Ask the model for one runbook. The reply is a JSON object so the name, summary and body cannot drift
/// into prose the caller has to parse with a heuristic.
pub fn plan(open: &OpenAi, facts: &Facts) -> Result<(String, String, String), String> {
    let user = format!(
        "A coding run was verified. Write a reusable runbook for the same kind of task.\n\n\
         Goal: {}\n\n\
         Turn map (a * marks the decisive check and the action before it):\n{}\n\n\
         Sanitized transcript:\n{}\n\n\
         Rules:\n\
         - Describe the procedure, not this one session. No session ids, no absolute paths, no host names.\n\
         - Never quote or paraphrase the verification command. Say what the final check proves.\n\
         - Keep the steps executable by another agent in a fresh copy of the same tree.\n\
         - Include a recovery note for the failure the transcript actually hit.\n\
         - Reply with one JSON object and nothing else:\n\
         {{\"name\":\"lowercase-hyphen-name\",\"summary\":\"one line\",\"runbook\":\"## When to use\\n...\\n## Steps\\n1. ...\\n## Check\\n...\\n## Pitfalls\\n- ...\"}}",
        facts.goal,
        turn_table(facts),
        sanitized(facts, 6_000)
    );
    let messages = json!([
        {
            "role": "system",
            "content": "You write short, portable runbooks for coding agents. You answer with one JSON object."
        },
        { "role": "user", "content": user }
    ]);
    let reply = open
        .chat(messages.as_array().expect("an array"), None, &|_, _| {})
        .map_err(|fault| {
            format!(
                "the planner ended in a {} fault: {}",
                fault.class(),
                fault.detail()
            )
        })?;
    let value = parse_object(&reply.content)?;
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let summary = value
        .get("summary")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let runbook = value
        .get("runbook")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if name.is_empty() || runbook.is_empty() {
        return Err("the planner returned no name or no runbook".to_string());
    }
    Ok((name, summary, runbook))
}

/// Write the draft under `<state>/runbooks/<name>.md`.
pub fn write_runbook(
    state: &Path,
    name: &str,
    summary: &str,
    runbook: &str,
    facts: &Facts,
) -> Result<PathBuf, String> {
    let dir = state.join("runbooks");
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("could not make {}: {error}", dir.display()))?;
    let path = dir.join(format!("{name}.md"));
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0);
    let text = format!(
        "---\nname: {name}\nstatus: draft\nsession: {session}\nprofile: {profile}\ncheck_fingerprint: {fingerprint}\nat: {at}\nsummary: {summary}\n---\n\n# {name}\n\n{runbook}\n",
        session = facts.session,
        profile = facts.profile,
        fingerprint = crate::skills::fingerprint(&facts.check),
        summary = summary.trim(),
        runbook = runbook.trim()
    );
    std::fs::write(&path, text)
        .map_err(|error| format!("could not write {}: {error}", path.display()))?;
    Ok(path)
}

/// Flip a draft to verified and name the replay that proved it.
pub fn mark_verified(path: &Path, replay: &str) -> Result<(), String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let text = text.replace("status: draft", "status: verified");
    let text = text.replacen(
        "check_fingerprint:",
        &format!("replay: {replay}\ncheck_fingerprint:"),
        1,
    );
    std::fs::write(path, text)
        .map_err(|error| format!("could not write {}: {error}", path.display()))
}

/// Copy the session's first snapshot into a fresh replay root. The first snapshot is the tree before the
/// successful program ran, which is the only honest starting point for a replay.
pub fn copy_snapshot(facts: &Facts, dest: &Path) -> Result<(), String> {
    let source = facts.snapshot.as_ref().ok_or_else(|| {
        "this session has no first snapshot, so it cannot be replayed".to_string()
    })?;
    if dest.exists() {
        std::fs::remove_dir_all(dest)
            .map_err(|error| format!("could not clear {}: {error}", dest.display()))?;
    }
    std::fs::create_dir_all(dest)
        .map_err(|error| format!("could not make {}: {error}", dest.display()))?;
    if source.is_dir() {
        copy_tree(source, dest)?;
    }
    Ok(())
}

fn copy_tree(source: &Path, dest: &Path) -> Result<(), String> {
    let entries = std::fs::read_dir(source)
        .map_err(|error| format!("could not read {}: {error}", source.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let target = dest.join(entry.file_name());
        let kind = entry
            .file_type()
            .map_err(|error| format!("could not inspect {}: {error}", path.display()))?;
        if kind.is_dir() {
            std::fs::create_dir_all(&target)
                .map_err(|error| format!("could not make {}: {error}", target.display()))?;
            copy_tree(&path, &target)?;
        } else if kind.is_file() {
            std::fs::copy(&path, &target)
                .map_err(|error| format!("could not copy {}: {error}", path.display()))?;
        }
    }
    Ok(())
}

fn parse_object(text: &str) -> Result<Value, String> {
    let trimmed = text.trim();
    let body = if let Some(rest) = trimmed.strip_prefix("```") {
        let rest = rest.strip_prefix("json").unwrap_or(rest).trim_start();
        rest.strip_suffix("```").unwrap_or(rest).trim()
    } else {
        trimmed
    };
    serde_json::from_str(body).map_err(|error| format!("the planner did not return JSON: {error}"))
}

fn normalize(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn one_line(text: &str, cap: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    clip(&line, cap)
}

fn clip(text: &str, cap: usize) -> String {
    if text.len() <= cap {
        return text.to_string();
    }
    let mut end = cap;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = crate::scratch::root().join(format!("distill-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sessions")).expect("a scratch state");
        dir
    }

    fn fixture(state: &Path) -> Facts {
        let entries = [
            json!({"kind":"user","payload":{"goal":"make the parser strict","root":"/work","profile":"fix"}}),
            json!({"kind":"tool_call","payload":{"tool":"shell","program":"cargo fix --allow-dirty","model":{"generated_tokens":10}}}),
            json!({"kind":"tool_result","payload":{"tool":"shell","status":0,"output":"fixed"}}),
            json!({"kind":"tool_call","payload":{"tool":"verify","program":"cargo test --quiet"}}),
            json!({"kind":"tool_result","payload":{"tool":"verify","state":"passed","output":""}}),
            json!({"kind":"checkpoint","payload":{"check":"passed","command":"cargo test --quiet"}}),
            json!({"kind":"checkpoint","payload":{"condition":"Passed","ended":true,"node":"done"}}),
        ];
        let text = entries
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(state.join("sessions/s1.jsonl"), text).expect("a session log");
        load(state, "s1").expect("facts")
    }

    #[test]
    fn a_verified_session_reads_back_with_its_check_and_decisive_turns() {
        let state = scratch("read");
        let facts = fixture(&state);
        assert!(facts.accepted);
        assert_eq!(facts.check, "cargo test --quiet");
        assert_eq!(facts.turns.len(), 2);
        assert!(facts.turns[1].decisive);
        assert!(facts.turns[0].decisive);
        let table = turn_table(&facts);
        assert!(table.contains("verify"), "{table}");
        assert!(table.contains("*"), "{table}");
    }

    #[test]
    fn the_gate_refuses_the_check_absolute_paths_and_secrets() {
        let runbook = "## When to use\nA parser that accepts a trailing comma but rejects a missing operand.\n\n## Steps\n1. Read the parser entry point and the failing case.\n2. Make the smallest grammar change that preserves the existing accept list.\n3. Run the project's parser tests, then the full check.\n\n## Check\nThe parser accepts the documented examples and rejects the malformed ones.\n\n## Pitfalls\n- Do not widen the grammar to satisfy one example.";
        assert!(
            gate("strict-parser", runbook, "cargo test --quiet").is_ok(),
            "{:?}",
            gate("strict-parser", runbook, "cargo test --quiet")
        );
        assert!(gate("strict-parser", runbook, "Run the project's parser tests").is_err());
        assert!(
            gate(
                "strict-parser",
                "## When to use\n/home/me\n## Steps\n## Check",
                ""
            )
            .is_err()
        );
        assert!(
            gate(
                "strict-parser",
                "## When to use\n## Steps\nsk-abc\n## Check",
                ""
            )
            .is_err()
        );
    }

    #[test]
    fn the_sanitized_transcript_withholds_the_verify_program() {
        let state = scratch("sanitize");
        let facts = fixture(&state);
        let text = sanitized(&facts, 4_000);
        assert!(!text.contains("cargo test --quiet"), "{text}");
        assert!(text.contains("[verify: passed]"), "{text}");
        assert!(text.contains("cargo fix --allow-dirty"), "{text}");
    }
}
