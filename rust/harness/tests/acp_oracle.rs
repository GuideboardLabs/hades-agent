//! The ACP oracle: the real binary, spoken to over pipes, exactly as an editor would.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

struct Acp {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<Value>,
}

impl Acp {
    fn start(name: &str, program: &str, verify: &str) -> (Acp, PathBuf, PathBuf) {
        let dir = harness::scratch::root().join(format!("hades-acp-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("root");
        let state = dir.join("state");
        std::fs::create_dir_all(&root).expect("a root");
        std::fs::create_dir_all(&state).expect("a state");
        let mut child = Command::new(env!("CARGO_BIN_EXE_hades"))
            .env("HADES_HOME", dir.join("home"))
            .args([
                "acp",
                "--root",
                root.to_str().unwrap(),
                "--state",
                state.to_str().unwrap(),
                "--policy",
                "guarded",
                "--verify",
                verify,
                "--program",
                program,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("the ACP server starts");
        let stdin = child.stdin.take().expect("a stdin");
        let stdout = child.stdout.take().expect("a stdout");
        let (sender, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(value) = serde_json::from_str::<Value>(&line)
                    && sender.send(value).is_err()
                {
                    return;
                }
            }
        });
        (
            Acp {
                child,
                stdin,
                lines,
            },
            root,
            state,
        )
    }

    fn send(&mut self, message: Value) {
        writeln!(self.stdin, "{message}").expect("a write to the server");
        self.stdin.flush().expect("a flush");
    }

    /// Read until the message with `id`, returning it and everything seen on the way.
    fn until(&self, id: i64, seconds: u64) -> (Value, Vec<Value>) {
        let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
        let mut seen = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(value) => {
                    if value.get("id").and_then(Value::as_i64) == Some(id) {
                        return (value, seen);
                    }
                    seen.push(value);
                }
                Err(RecvTimeoutError::Timeout) => {
                    panic!("no answer to {id}; saw {seen:#?}");
                }
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("the server closed; saw {seen:#?}");
                }
            }
        }
    }
}

impl Drop for Acp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn initialize(acp: &mut Acp) -> Value {
    acp.send(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": { "protocolVersion": 1, "clientCapabilities": {}, "clientInfo": { "name": "oracle", "version": "0" } }
    }));
    acp.until(1, 10).0
}

fn new_session(acp: &mut Acp, root: &Path) -> String {
    acp.send(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "session/new",
        "params": { "cwd": root.to_str().unwrap(), "mcpServers": [] }
    }));
    let (reply, _) = acp.until(2, 10);
    reply["result"]["sessionId"]
        .as_str()
        .expect("a session id")
        .to_string()
}

#[test]
fn acp_initializes_opens_a_session_and_runs_a_prompt_with_a_permission_round_trip() {
    let (mut acp, root, _state) = Acp::start("round-trip", "printf ok > ok.txt", "test -f ok.txt");
    let hello = initialize(&mut acp);
    assert_eq!(hello["result"]["protocolVersion"], 1);
    assert_eq!(hello["result"]["agentInfo"]["name"], "hades");
    assert_eq!(hello["result"]["agentCapabilities"]["loadSession"], false);

    let session = new_session(&mut acp, &root);
    acp.send(json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "session/prompt",
        "params": {
            "sessionId": session.clone(),
            "prompt": [{ "type": "text", "text": "create ok.txt" }]
        }
    }));

    let mut asked = false;
    let mut updates: Vec<Value> = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let final_reply = loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        let value = acp
            .lines
            .recv_timeout(left)
            .expect("a reply before the deadline");
        if value.get("method").and_then(Value::as_str) == Some("session/request_permission") {
            asked = true;
            let request_id = value["id"].as_i64().expect("a request id");
            acp.send(json!({
                "jsonrpc": "2.0",
                "id": request_id,
                "result": { "outcome": { "outcome": "selected", "optionId": "allow-session" } }
            }));
            continue;
        }
        if value.get("method").and_then(Value::as_str) == Some("session/update") {
            updates.push(value);
            continue;
        }
        if value.get("id").and_then(Value::as_i64) == Some(3) {
            break value;
        }
    };

    assert!(asked, "guarded must ask before the shell runs");
    assert_eq!(
        final_reply["result"]["stopReason"], "end_turn",
        "{final_reply}"
    );
    assert!(
        std::fs::read_to_string(root.join("ok.txt"))
            .expect("the program ran")
            .contains("ok")
    );
    let kinds: Vec<&str> = updates
        .iter()
        .filter_map(|update| update["params"]["update"]["sessionUpdate"].as_str())
        .collect();
    assert!(kinds.contains(&"tool_call"), "{kinds:?}");
    assert!(kinds.contains(&"tool_call_update"), "{kinds:?}");
    assert!(kinds.contains(&"agent_message_chunk"), "{kinds:?}");

    acp.send(json!({ "jsonrpc": "2.0", "id": 4, "method": "nope" }));
    let (error, _) = acp.until(4, 10);
    assert_eq!(error["error"]["code"], -32601, "{error}");

    acp.send(json!({
        "jsonrpc": "2.0",
        "id": 5,
        "method": "session/fork",
        "params": { "sessionId": session.clone(), "at": 1 }
    }));
    let (forked, _) = acp.until(5, 10);
    let child = forked["result"]["sessionId"].as_str().expect("a forked id");
    assert_ne!(child, session.as_str());
    assert!(child.contains("-fork-1"), "{child}");
}

#[test]
fn acp_cancel_stops_the_run_and_nothing_after_it_runs() {
    let (mut acp, root, _state) = Acp::start(
        "cancel",
        "sleep 5; printf done > done.txt",
        "test -f never.txt",
    );
    initialize(&mut acp);
    let session = new_session(&mut acp, &root);
    acp.send(json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "session/prompt",
        "params": {
            "sessionId": session,
            "prompt": [{ "type": "text", "text": "sleep and write" }]
        }
    }));

    // Wait until the run is real, then cancel it.
    let mut asked = false;
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        if let Ok(value) = acp.lines.recv_timeout(Duration::from_millis(200))
            && value.get("method").and_then(Value::as_str) == Some("session/request_permission")
        {
            asked = true;
            let request_id = value["id"].as_i64().expect("a request id");
            acp.send(json!({
                "jsonrpc": "2.0",
                "id": request_id,
                "result": { "outcome": { "outcome": "selected", "optionId": "allow-session" } }
            }));
            break;
        }
    }
    assert!(asked, "the run should have asked before running");
    // The server answers, re-runs, and the program is now sleeping. Cancel it.
    std::thread::sleep(Duration::from_millis(300));
    acp.send(json!({
        "jsonrpc": "2.0",
        "method": "session/cancel",
        "params": { "sessionId": session }
    }));
    let (reply, _) = acp.until(3, 15);
    assert_eq!(reply["result"]["stopReason"], "cancelled", "{reply}");
    assert!(
        !root.join("done.txt").exists(),
        "the program finished after a cancel"
    );
}
