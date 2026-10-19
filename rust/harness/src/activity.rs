//! One line for every action a layer decided: what was attempted, which layer ruled, what it said and
//! why, and the path it named.
//!
//! The session log is the narrative of a governed turn. This is the flat ledger across the surfaces that
//! had none: a tool call made from a chat, and a tool call made by a scheduled job. They are the same
//! seam, so they write the same shape. A governed turn already records its decisions in its session log
//! and is not duplicated here.
//!
//! Append-only, one file per state directory. A failed write goes to stderr and never stops the action:
//! the ledger is evidence, not a gate. That limit is stated because a ledger that can block work is a new
//! policy by accident.

use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};

/// How long an action is allowed to be in the line. A line must stay small enough to append atomically
/// and short enough to read; the full argument is in the tool call, not here.
const CLIP: usize = 160;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub at: i64,
    /// `chat:<id>`, `job:<id>`, or whatever surface is speaking. One string, so two ledgers can be read
    /// together without guessing which column is the identity.
    pub actor: String,
    /// `mode`, `floor`, or `surface` when nothing objected.
    pub layer: String,
    /// The tool and the one argument that says what it would do.
    pub action: String,
    /// `allow`, `refuse` (a layer said no without a person), `ask` (a question was raised), or `stop` (a
    /// person ended work). A stop has its own word because the question it answers is "who stopped what",
    /// the same way `allow` answers "who allowed what".
    pub decision: String,
    pub reason: String,
    /// The path the action named, when it named one. Empty when it did not: a shell command has no one
    /// artifact, and inventing one would be worse than leaving it out.
    pub artifact: String,
}

pub fn path(state: &Path) -> PathBuf {
    state.join("activity.jsonl")
}

/// A stop, recorded like any other decision. One function so the daemon, the CLI and a cancelled job write
/// the same word and the same shape; a second spelling would make `hades activity` answer two questions.
/// Best effort by the same contract as [`record`]: the stop already happened, and the ledger cannot undo it.
pub fn stop(state: &Path, actor: &str, action: &str, reason: &str, artifact: &str) {
    let line = Line {
        at: epoch(),
        actor: actor.to_string(),
        layer: "surface".to_string(),
        action: action.to_string(),
        decision: "stop".to_string(),
        reason: reason.to_string(),
        artifact: artifact.to_string(),
    };
    if let Err(said) = record(state, &line) {
        eprintln!("activity: could not record {action}: {said}");
    }
}

fn epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

/// Append one decided action. Best effort by contract: the caller reports a failure and carries on.
pub fn record(state: &Path, line: &Line) -> Result<(), String> {
    std::fs::create_dir_all(state).map_err(|error| error.to_string())?;
    let body = json!({
        "at": line.at,
        "actor": line.actor,
        "layer": line.layer,
        "action": clip(&line.action, CLIP),
        "decision": line.decision,
        "reason": clip(&line.reason, CLIP),
        "artifact": line.artifact,
    });
    let mut text = body.to_string();
    text.push('\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path(state))
        .map_err(|error| error.to_string())?;
    file.write_all(text.as_bytes())
        .map_err(|error| error.to_string())
}

/// Every line in the ledger, oldest first. A line that will not parse is skipped: one bad append must not
/// hide the history around it.
pub fn read(state: &Path) -> Vec<Line> {
    let Ok(text) = std::fs::read_to_string(path(state)) else {
        return Vec::new();
    };
    text.lines().filter_map(parse).collect()
}

fn parse(text: &str) -> Option<Line> {
    let value: Value = serde_json::from_str(text).ok()?;
    Some(Line {
        at: value.get("at")?.as_i64()?,
        actor: value.get("actor")?.as_str()?.to_string(),
        layer: value.get("layer")?.as_str()?.to_string(),
        action: value.get("action")?.as_str()?.to_string(),
        decision: value.get("decision")?.as_str()?.to_string(),
        reason: value
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        artifact: value
            .get("artifact")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    })
}

/// The second half of a line: the tool and the one argument that matters. A command, a path, a query, a
/// URL, in that order, because those are what a person looks for when scanning the ledger.
pub fn action(tool: &str, args: &Value) -> String {
    // An MCP call names a server and a tool; the arguments are the server's business, not the ledger's.
    if tool == "mcp" {
        let server = args.get("server").and_then(Value::as_str).unwrap_or("");
        let verb = args.get("action").and_then(Value::as_str).unwrap_or("list");
        let said = match args.get("tool").and_then(Value::as_str) {
            Some(name) => format!("mcp: {server}/{name}"),
            None if server.is_empty() => format!("mcp: {verb}"),
            None => format!("mcp: {server} {verb}"),
        };
        return clip(&said, CLIP);
    }
    for key in ["command", "path", "file", "query", "url", "name", "title"] {
        if let Some(value) = args.get(key).and_then(Value::as_str) {
            return clip(&format!("{tool}: {}", first_line(value)), CLIP);
        }
    }
    clip(&format!("{tool}: {args}"), CLIP)
}

/// The path an action named, for the artifact column: only a real filesystem path, not a URL or a shell
/// word.
pub fn artifact(args: &Value) -> String {
    args.get("path")
        .or_else(|| args.get("file"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .unwrap_or("")
        .to_string()
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or("").trim().to_string()
}

fn clip(text: &str, most: usize) -> String {
    if text.chars().count() <= most {
        return text.to_string();
    }
    let mut out: String = text.chars().take(most).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../target/scratch")
            .join(format!("activity-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    fn line(n: i64) -> Line {
        Line {
            at: 1_000 + n,
            actor: "job:2".to_string(),
            layer: "floor".to_string(),
            action: format!("shell: rm -f note-{n}.txt"),
            decision: "refuse".to_string(),
            reason: "permanent delete".to_string(),
            artifact: String::new(),
        }
    }

    #[test]
    fn every_decision_is_one_line_in_order() {
        let state = scratch("order");
        record(&state, &line(1)).expect("first");
        record(&state, &line(2)).expect("second");
        let all = read(&state);
        assert_eq!(all.len(), 2, "{all:?}");
        assert_eq!(all[0].action, "shell: rm -f note-1.txt");
        assert_eq!(all[1].at, 1_002);
        assert_eq!(all[1].reason, "permanent delete");
    }

    #[test]
    fn a_line_that_will_not_parse_is_skipped_not_fatal() {
        let state = scratch("torn");
        record(&state, &line(1)).expect("good");
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(path(&state))
            .expect("append");
        file.write_all(b"{\"at\": 9, torn").expect("torn");
        let all = read(&state);
        assert_eq!(all.len(), 1, "{all:?}");
        assert_eq!(all[0].action, "shell: rm -f note-1.txt");
    }

    #[test]
    fn the_action_names_the_argument_a_person_looks_for() {
        assert_eq!(
            action("shell", &json!({"command": "ls -la\nsecond line"})),
            "shell: ls -la"
        );
        assert_eq!(
            action("read_file", &json!({"path": "/work/a.md"})),
            "read_file: /work/a.md"
        );
        assert_eq!(
            action("browser", &json!({"action": "click", "target": "Send"})),
            "browser: {\"action\":\"click\",\"target\":\"Send\"}"
        );
        assert_eq!(artifact(&json!({"path": "/work/a.md"})), "/work/a.md");
        assert_eq!(artifact(&json!({"command": "ls"})), "");
    }

    #[test]
    fn a_long_argument_is_clipped_to_one_readable_line() {
        let long = "x".repeat(400);
        let said = action("shell", &json!({ "command": long }));
        assert!(said.chars().count() <= CLIP + 7, "{}", said.chars().count());
        assert!(said.ends_with('…'), "{said}");
    }
}
