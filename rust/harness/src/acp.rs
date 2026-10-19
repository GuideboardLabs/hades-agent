//! ACP: the editor protocol, spoken over stdio, as a front end over the same governed run.
//!
//! Zed's Agent Client Protocol is JSON-RPC 2.0, one object per line. This server answers `initialize`,
//! `session/new`, `session/prompt`, `session/fork` and `session/cancel`, and turns a Hades run into the updates a client
//! renders: a tool call per program, a completion per result, an agent message with the outcome.
//!
//! **The run is not reimplemented here.** `session/prompt` starts this same binary with `hades run`, reads the
//! session log while it is written, and translates each entry. That is deliberate: the cage, the hard floor,
//! the judge, the plan gate and the ledger are the product, and a second execution path inside the protocol
//! adapter would be a second place for them to drift.
//!
//! **Permission requests are the run's own questions.** Under `guarded` a run parks at `awaiting` with an
//! `approval_request` entry. The server sends `session/request_permission` with `allow-once`,
//! `allow-session` and `deny`, records the chosen answer with the same `hades answer` a person would use, and
//! re-runs the goal in the same session; the standing session answer lets the retry through. A client that
//! answers `cancelled` is a deny plus a cancelled stop.
//!
//! Stated limits, because a protocol adapter that overstates itself is worse than none: `session/load` is not
//! offered (no history replay), `fs/*` and `terminal/*` are not offered (the client's editor is not used as a
//! file system), images and audio are not accepted, client MCP servers are not attached, and one prompt is one
//! graph run rather than a chat turn. The session is the Hades session log either way, so a prompt after a
//! prompt continues the same thread.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

/// What the server was told on the command line.
#[derive(Debug, Clone)]
pub struct Options {
    pub root: PathBuf,
    pub state: PathBuf,
    pub config: Option<PathBuf>,
    pub profile: Option<String>,
    pub policy: String,
    pub verify: Option<String>,
    /// Scripted answers for the offline probe: each run gets these `--program` values in order.
    pub programs: Vec<String>,
}

#[derive(Debug)]
struct Session {
    cwd: PathBuf,
    stop: Arc<AtomicBool>,
}

#[derive(Clone)]
struct Server {
    options: Arc<Options>,
    out: Arc<Mutex<std::io::Stdout>>,
    /// Requests this server sent to the client, waiting for a response keyed by the JSON-RPC id.
    pending: Arc<Mutex<HashMap<String, mpsc::Sender<Value>>>>,
    sessions: Arc<Mutex<HashMap<String, Arc<Session>>>>,
    counter: Arc<Mutex<u64>>,
}

/// Read JSON-RPC from `input` until it closes. The caller owns the lock; nothing here reads stdin again.
pub fn serve(options: Options, input: &mut dyn BufRead) -> i32 {
    let server = Server {
        options: Arc::new(options),
        out: Arc::new(Mutex::new(std::io::stdout())),
        pending: Arc::new(Mutex::new(HashMap::new())),
        sessions: Arc::new(Mutex::new(HashMap::new())),
        counter: Arc::new(Mutex::new(1)),
    };
    for line in input.lines() {
        let Ok(line) = line else {
            break;
        };
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(&line) {
            Ok(message) => server.dispatch(message),
            Err(_) => server.send(&json!({
                "jsonrpc": "2.0",
                "id": Value::Null,
                "error": { "code": -32700, "message": "the line is not JSON" }
            })),
        }
    }
    0
}

impl Server {
    fn next(&self) -> u64 {
        let mut counter = self.counter.lock().unwrap();
        let value = *counter;
        *counter += 1;
        value
    }

    fn send(&self, message: &Value) {
        let mut out = self.out.lock().unwrap();
        let _ = writeln!(out, "{message}");
        let _ = out.flush();
    }

    fn respond(&self, id: &Value, result: Value) {
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }

    fn fail(&self, id: &Value, code: i64, message: impl Into<String>) {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message.into() }
        }));
    }

    fn dispatch(&self, message: Value) {
        let id = message.get("id").cloned();
        if let Some(method) = message.get("method").and_then(Value::as_str) {
            let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
            match id {
                Some(id) => self.request(id, method, params),
                None => self.notify(method, params),
            }
            return;
        }
        // A response to a request this server made (the permission question).
        let Some(id) = id else {
            return;
        };
        let key = id.to_string();
        if let Some(waiting) = self.pending.lock().unwrap().remove(&key) {
            let _ = waiting.send(message);
        }
    }

    fn request(&self, id: Value, method: &str, params: Value) {
        match method {
            "initialize" => self.respond(
                &id,
                json!({
                    "protocolVersion": 1,
                    "agentCapabilities": {
                        "loadSession": false,
                        "promptCapabilities": {
                            "image": false,
                            "audio": false,
                            "embeddedContext": false
                        },
                        "sessionCapabilities": { "fork": {} }
                    },
                    "agentInfo": { "name": "hades", "version": env!("CARGO_PKG_VERSION") },
                    "authMethods": []
                }),
            ),
            "session/new" => {
                let cwd = params
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(PathBuf::from)
                    .unwrap_or_else(|| self.options.root.clone());
                if !cwd.is_dir() {
                    self.fail(
                        &id,
                        -32602,
                        format!("no such working directory: {}", cwd.display()),
                    );
                    return;
                }
                let number = self.next();
                let session_id = format!("acp-{}-{number}", std::process::id());
                let session = Arc::new(Session {
                    cwd,
                    stop: Arc::new(AtomicBool::new(false)),
                });
                self.sessions
                    .lock()
                    .unwrap()
                    .insert(session_id.clone(), session);
                self.respond(&id, json!({ "sessionId": session_id }));
            }
            "session/prompt" => {
                let Some(session_id) = params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                else {
                    self.fail(&id, -32602, "a prompt needs a sessionId");
                    return;
                };
                let goal = prompt_text(&params);
                if goal.trim().is_empty() {
                    self.fail(&id, -32602, "a prompt needs at least one text part");
                    return;
                }
                if !self.sessions.lock().unwrap().contains_key(&session_id) {
                    self.fail(&id, -32602, format!("no session `{session_id}`"));
                    return;
                }
                let server = self.clone();
                std::thread::spawn(move || server.prompt(id, session_id, goal));
            }
            "session/fork" => {
                let Some(parent) = params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                else {
                    self.fail(&id, -32602, "a fork needs a sessionId");
                    return;
                };
                let at = params.get("at").and_then(Value::as_u64);
                let cwd = self
                    .sessions
                    .lock()
                    .unwrap()
                    .get(&parent)
                    .map(|session| session.cwd.clone())
                    .unwrap_or_else(|| self.options.root.clone());
                match crate::cli::fork_session(&self.options.state, &parent, at) {
                    Ok((new_id, _, _)) => {
                        self.sessions.lock().unwrap().insert(
                            new_id.clone(),
                            Arc::new(Session {
                                cwd,
                                stop: Arc::new(AtomicBool::new(false)),
                            }),
                        );
                        self.respond(&id, json!({ "sessionId": new_id }));
                    }
                    Err(message) => self.fail(&id, -32603, message),
                }
            }
            other => self.fail(&id, -32601, format!("there is no method `{other}`")),
        }
    }

    fn notify(&self, method: &str, params: Value) {
        if method == "session/cancel"
            && let Some(session_id) = params.get("sessionId").and_then(Value::as_str)
            && let Some(session) = self.sessions.lock().unwrap().get(session_id).cloned()
        {
            session.stop.store(true, Ordering::SeqCst);
        }
    }

    fn prompt(&self, id: Value, session_id: String, goal: String) {
        let Some(session) = self.sessions.lock().unwrap().get(&session_id).cloned() else {
            self.fail(&id, -32602, format!("no session `{session_id}`"));
            return;
        };
        session.stop.store(false, Ordering::SeqCst);

        for attempt in 0..4 {
            // A retry appends to the same log. Everything already there belongs to the previous attempt,
            // including the question that was just answered, so the reader starts where the new run starts.
            let before = self.log_lines(&session_id);
            let mut child = match self.start(&session_id, &session, &goal) {
                Ok(child) => child,
                Err(message) => {
                    self.fail(&id, -32603, message);
                    return;
                }
            };
            let question = self.stream(&session_id, &session, &mut child, before);
            let status = child.wait();
            if session.stop.load(Ordering::SeqCst) {
                self.message(&session_id, "stopped by the client");
                self.respond(&id, json!({ "stopReason": "cancelled" }));
                return;
            }
            let Some(question) = question else {
                let said = match status.map(|status| status.code()) {
                    Ok(Some(0)) => "the run was accepted".to_string(),
                    Ok(Some(code)) => format!("the run ended with status {code}"),
                    _ => "the run ended".to_string(),
                };
                self.message(&session_id, said);
                self.respond(&id, json!({ "stopReason": "end_turn" }));
                return;
            };
            let choice = self.ask(&session_id, &question);
            let allow = match choice.as_str() {
                "allow-once" => Some("once"),
                "allow-session" => Some("session"),
                _ => None,
            };
            let Some(scope) = allow else {
                let _ = self.answer(&question, false, "once");
                self.message(
                    &session_id,
                    "the client denied the question, so nothing ran",
                );
                self.respond(&id, json!({ "stopReason": "refusal" }));
                return;
            };
            if let Err(message) = self.answer(&question, true, scope) {
                self.fail(&id, -32603, message);
                return;
            }
            if attempt == 3 {
                self.message(
                    &session_id,
                    "the run kept asking; stopped after four questions",
                );
                self.respond(&id, json!({ "stopReason": "max_tokens" }));
                return;
            }
        }
    }

    fn start(&self, session_id: &str, session: &Session, goal: &str) -> Result<Child, String> {
        let exe = std::env::current_exe()
            .map_err(|error| format!("could not find the hades binary: {error}"))?;
        let mut command = std::process::Command::new(exe);
        command
            .arg("run")
            .arg(goal)
            .arg("--root")
            .arg(&session.cwd)
            .arg("--state")
            .arg(&self.options.state)
            .arg("--policy")
            .arg(&self.options.policy)
            .arg("--session")
            .arg(session_id)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        if let Some(config) = &self.options.config {
            command.arg("--config").arg(config);
        }
        if let Some(profile) = &self.options.profile {
            command.arg("--profile").arg(profile);
        }
        if let Some(verify) = &self.options.verify {
            command.arg("--verify").arg(verify);
        }
        for program in &self.options.programs {
            command.arg("--program").arg(program);
        }
        command
            .spawn()
            .map_err(|error| format!("could not start a run: {error}"))
    }

    fn log_lines(&self, session_id: &str) -> usize {
        std::fs::read_to_string(
            self.options
                .state
                .join("sessions")
                .join(format!("{session_id}.jsonl")),
        )
        .map(|text| text.lines().count())
        .unwrap_or(0)
    }

    /// Read the session log while the run writes it and translate each new entry.
    fn stream(
        &self,
        session_id: &str,
        session: &Session,
        child: &mut Child,
        mut seen: usize,
    ) -> Option<Value> {
        let log = self
            .options
            .state
            .join("sessions")
            .join(format!("{session_id}.jsonl"));
        let mut question: Option<Value> = None;
        let mut last_shell: Option<String> = None;
        let started = Instant::now();
        loop {
            if session.stop.load(Ordering::SeqCst) {
                let _ = child.kill();
                return question;
            }
            let text = std::fs::read_to_string(&log).unwrap_or_default();
            let lines: Vec<&str> = text.lines().collect();
            while seen < lines.len() {
                if let Ok(entry) = serde_json::from_str::<Value>(lines[seen]) {
                    self.entry(session_id, &entry, &mut question, &mut last_shell);
                }
                seen += 1;
            }
            if child.try_wait().ok().flatten().is_some() {
                // The log is written before the process exits; one more read takes the tail.
                let text = std::fs::read_to_string(&log).unwrap_or_default();
                let lines: Vec<&str> = text.lines().collect();
                while seen < lines.len() {
                    if let Ok(entry) = serde_json::from_str::<Value>(lines[seen]) {
                        self.entry(session_id, &entry, &mut question, &mut last_shell);
                    }
                    seen += 1;
                }
                return question;
            }
            if started.elapsed() > Duration::from_secs(1800) {
                let _ = child.kill();
                return question;
            }
            std::thread::sleep(Duration::from_millis(80));
        }
    }

    fn entry(
        &self,
        session_id: &str,
        entry: &Value,
        question: &mut Option<Value>,
        last_shell: &mut Option<String>,
    ) {
        let kind = entry
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let payload = entry.get("payload").unwrap_or(&Value::Null);
        match kind {
            "tool_call" => {
                let tool = payload
                    .get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or("tool");
                let program = payload
                    .get("program")
                    .and_then(Value::as_str)
                    .or_else(|| payload.get("path").and_then(Value::as_str))
                    .unwrap_or("");
                let call_id = format!(
                    "{session_id}-{}",
                    entry.get("id").and_then(Value::as_u64).unwrap_or(0)
                );
                if tool == "shell" {
                    *last_shell = Some(call_id.clone());
                }
                self.send(&json!({
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": {
                        "sessionId": session_id,
                        "update": {
                            "sessionUpdate": "tool_call",
                            "toolCallId": call_id,
                            "title": format!("{tool}: {}", clip(program, 120)),
                            "kind": match tool { "shell" => "execute", "write_file" | "patch" => "edit", _ => "other" },
                            "status": "in_progress",
                            "content": [{ "type": "content", "content": { "type": "text", "text": program } }],
                            "locations": [],
                            "rawInput": { "tool": tool }
                        }
                    }
                }));
            }
            "tool_result" => {
                let tool = payload
                    .get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or("tool");
                let output = payload.get("output").and_then(Value::as_str).unwrap_or("");
                let call_id = if tool == "shell" {
                    last_shell
                        .clone()
                        .unwrap_or_else(|| format!("{session_id}-result"))
                } else {
                    format!(
                        "{session_id}-{}",
                        entry.get("id").and_then(Value::as_u64).unwrap_or(0)
                    )
                };
                self.send(&json!({
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": {
                        "sessionId": session_id,
                        "update": {
                            "sessionUpdate": "tool_call_update",
                            "toolCallId": call_id,
                            "status": "completed",
                            "rawOutput": { "tool": tool, "output": clip(output, 4000) }
                        }
                    }
                }));
            }
            "approval_request" if question.is_none() => {
                *question = Some(payload.clone());
            }
            _ => {}
        }
    }

    fn ask(&self, session_id: &str, question: &Value) -> String {
        let id = self.next() + 1000;
        let key = id.to_string();
        let (sender, receiver) = mpsc::channel();
        self.pending.lock().unwrap().insert(key, sender);
        let action = question
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("an action");
        let reason = question.get("reason").and_then(Value::as_str).unwrap_or("");
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "session/request_permission",
            "params": {
                "sessionId": session_id,
                "toolCall": {
                    "toolCallId": format!("q{id}"),
                    "title": format!("{action}: {reason}"),
                    "kind": "other",
                    "status": "pending",
                    "content": [{ "type": "content", "content": { "type": "text", "text": reason } }],
                    "locations": []
                },
                "options": [
                    { "optionId": "allow-once", "name": "Allow once", "kind": "allow_once" },
                    { "optionId": "allow-session", "name": "Allow for this session", "kind": "allow_always" },
                    { "optionId": "deny", "name": "Deny", "kind": "reject_once" }
                ]
            }
        }));
        let reply = receiver
            .recv_timeout(Duration::from_secs(600))
            .unwrap_or_else(|_| json!({ "result": { "outcome": { "outcome": "cancelled" } } }));
        reply
            .get("result")
            .and_then(|result| result.get("outcome"))
            .and_then(|outcome| {
                if outcome.get("outcome").and_then(Value::as_str) == Some("selected") {
                    outcome.get("optionId").and_then(Value::as_str)
                } else {
                    None
                }
            })
            .unwrap_or("deny")
            .to_string()
    }

    /// Record the answer with the same command a person uses. `run_with` is called with an empty input
    /// because this process owns stdin; `cli::run` would try to lock it again and deadlock.
    fn answer(&self, question: &Value, allow: bool, scope: &str) -> Result<(), String> {
        let id = question
            .get("id")
            .and_then(Value::as_u64)
            .ok_or_else(|| "the question has no id".to_string())?;
        let mut text: Vec<u8> = Vec::new();
        let mut empty: &[u8] = &[];
        let code = crate::cli::run_with(
            &[
                "answer".to_string(),
                id.to_string(),
                if allow { "allow" } else { "deny" }.to_string(),
                "--scope".to_string(),
                scope.to_string(),
                "--by".to_string(),
                "acp".to_string(),
                "--state".to_string(),
                self.options.state.display().to_string(),
            ],
            &mut text,
            &mut empty,
        );
        if code == 0 {
            Ok(())
        } else {
            Err(format!(
                "could not record the answer: {}",
                String::from_utf8_lossy(&text).trim()
            ))
        }
    }

    fn message(&self, session_id: &str, text: impl Into<String>) {
        self.send(&json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": session_id,
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": text.into() }
                }
            }
        }));
    }
}

/// The text parts of a prompt, in order.
fn prompt_text(params: &Value) -> String {
    params
        .get("prompt")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn clip(text: &str, cap: usize) -> String {
    if text.chars().count() <= cap {
        return text.to_string();
    }
    let mut out: String = text.chars().take(cap).collect();
    out.push('…');
    out
}
