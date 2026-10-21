//! The HTTP surface: one read-and-control API over the daemon.
//!
//! HTTP is another client of the daemon, exactly like the CLI and the Telegram gateway. It never builds a
//! runtime, never chooses a policy, never reads a goal from a query string into a shell, and never owns a
//! session. Every state-changing call becomes a daemon op over the existing unix socket, and every read
//! either asks the daemon or reads the same files the daemon answers from.
//!
//! `http.rs` stays the one-POST model client; this module is the server. Two halves, one grep.
//!
//! The shape is deliberately small and hand-rolled: the standard library is enough for one listener, one
//! thread per connection, and a line-oriented parser. Responses are JSON, every response closes the
//! connection, and nothing here streams. A browser `fetch` has a timeout measured in seconds while a
//! governed run is measured in minutes, so a submit answers `202` with the daemon's acknowledgement and the
//! client polls the session log.
//!
//! Stated limits, because a surface is only worth what it excludes:
//!
//! - No TLS and no CORS. A browser on another origin cannot read a response, so a command center proxies
//!   server-side or is served from this origin later. A non-loopback deployment belongs behind a
//!   TLS-terminating proxy, not in this process.
//! - On loopback with no token, any local process can call the API. That is the same trust the unix socket
//!   already has: the socket is a file only this machine can reach.
//! - A token is compared in constant time over equal-length byte strings; the length itself is observable.
//! - The API never returns a secret and never reads `~/.hades/.env`.
//! - `Connection: close` only: no keep-alive, no pipelining, no chunked bodies.
//! - A request past its cap gets an error and the connection closes; nothing is half-read and acted on.

use crate::activity;
use crate::cron;
use crate::daemon;
use crate::retry;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The request line and every header, together. Sixteen KiB is generous for an API with three headers and
/// small enough that a hostile client cannot make one connection cost a page of memory.
const HEADER_CAP: usize = 16 * 1024;
/// One megabyte of body: a goal is at most 20,000 bytes, and the rest is room to be wrong without being
/// dangerous.
const BODY_CAP: usize = 1024 * 1024;
/// How long a client has to send a complete request once the connection is accepted.
const REQUEST_DEADLINE: Duration = Duration::from_secs(10);
/// How long the daemon has to answer a request. A submit's acknowledgement is written before any work, so
/// this is a wedged-daemon deadline, not a run deadline.
const DAEMON_DEADLINE: Duration = Duration::from_secs(10);

const SESSION_LIMIT: usize = 50;
const SESSION_LIMIT_MAX: usize = 500;
const ACTIVITY_LIMIT: usize = 100;
const ACTIVITY_LIMIT_MAX: usize = 1000;

/// How many connections one server handles at once. A thread per connection is the whole shape, so the
/// count is also the thread count: a local process that opens a thousand idle sockets would otherwise cost
/// a thousand stacks and a thousand descriptors that never time out.
pub const DEFAULT_MAX_CONNECTIONS: usize = 32;

/// A count of connections in flight, with a way to wait for one.
struct Permits {
    free: std::sync::Mutex<usize>,
    ready: std::sync::Condvar,
}

/// One held permit. Dropping it is the only way to give it back, which is what makes a panic in the handler
/// release the slot instead of leaking it.
struct Permit(std::sync::Arc<Permits>);

impl Permits {
    fn new(count: usize) -> Permits {
        Permits {
            free: std::sync::Mutex::new(count.max(1)),
            ready: std::sync::Condvar::new(),
        }
    }

    /// Take a permit, or give up after `wait`. `None` is the server's answer to a client it cannot serve.
    fn acquire(permits: &std::sync::Arc<Permits>, wait: Duration) -> Option<Permit> {
        let mut free = match permits.free.lock() {
            Ok(free) => free,
            Err(poisoned) => poisoned.into_inner(),
        };
        let deadline = Instant::now() + wait;
        while *free == 0 {
            let left = deadline.checked_duration_since(Instant::now())?;
            let (held, timeout) = match permits.ready.wait_timeout(free, left) {
                Ok(pair) => pair,
                Err(poisoned) => poisoned.into_inner(),
            };
            free = held;
            if timeout.timed_out() && *free == 0 {
                return None;
            }
        }
        *free -= 1;
        Some(Permit(std::sync::Arc::clone(permits)))
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut free = match self.0.free.lock() {
            Ok(free) => free,
            Err(poisoned) => poisoned.into_inner(),
        };
        *free += 1;
        self.0.ready.notify_one();
    }
}

/// What to serve, and where. Owned by the CLI command; cloned once per connection.
#[derive(Clone, Debug)]
pub struct Server {
    pub state: PathBuf,
    pub bind: SocketAddr,
    /// Mandatory. Every bind needs one, including loopback, so there is no unauthenticated mode.
    pub token: String,
    /// Accept one request and return. A scripted probe should not leave a server behind.
    pub once: bool,
    /// How many connections may be in flight at once.
    pub max_connections: usize,
}

impl Server {
    /// Bind, announce, and accept. The listening line goes to stderr and is flushed before the first
    /// accept, matching `hades daemon`; the oracle spawns the real binary and reads it to learn a port that
    /// was asked for as `0`.
    pub fn serve(self) -> Result<(), String> {
        let listener = TcpListener::bind(self.bind)
            .map_err(|error| format!("could not bind {}: {error}", self.bind))?;
        let address = listener
            .local_addr()
            .map_err(|error| format!("could not read the bound address: {error}"))?;
        eprintln!("hades http listening on {address}");
        let _ = std::io::stderr().flush();

        if self.once {
            let (stream, _peer) = listener
                .accept()
                .map_err(|error| format!("could not accept a connection: {error}"))?;
            self.handle(stream);
            return Ok(());
        }

        let permits = std::sync::Arc::new(Permits::new(self.max_connections));
        for incoming in listener.incoming() {
            let Ok(stream) = incoming else {
                continue;
            };
            let Some(permit) = Permits::acquire(&permits, Duration::from_secs(1)) else {
                // No slot within a second: say so and close. Dropping the connection silently would look
                // like a crash to a client that cannot tell a refusal from a fault.
                let mut stream = stream;
                let refusal = Reply::new(
                    503,
                    json!({"ok": false, "error": "too many connections in flight"}),
                );
                let _ = write_reply(&mut stream, refusal);
                let _ = stream.flush();
                let _ = stream.shutdown(std::net::Shutdown::Write);
                continue;
            };
            let server = self.clone();
            std::thread::spawn(move || {
                server.handle(stream);
                drop(permit);
            });
        }
        Ok(())
    }

    /// One connection: read one request, answer it, close. A failed read is answered with the status that
    /// says what went wrong rather than dropped, because a client that gets nothing cannot tell a refusal
    /// from a crash.
    fn handle(&self, mut stream: TcpStream) {
        let reply = match read_request(&mut stream) {
            Ok(request) => self.route(request),
            Err(reply) => reply,
        };
        let _ = write_reply(&mut stream, reply);
        let _ = stream.flush();
        let _ = stream.shutdown(std::net::Shutdown::Write);
    }

    fn route(&self, request: Request) -> Reply {
        if let Err(reply) = self.authorise(&request) {
            return reply;
        }
        if let Err(reply) = self.same_origin(&request) {
            return reply;
        }

        let (path, query) = match request.target.split_once('?') {
            Some((path, query)) => (path, query),
            None => (request.target.as_str(), ""),
        };

        match path {
            "/api/health" => {
                if request.method != "GET" {
                    return wrong_method("GET");
                }
                self.health()
            }
            "/api/status" => {
                if request.method != "GET" {
                    return wrong_method("GET");
                }
                self.status()
            }
            "/api/sessions" => {
                if request.method != "GET" {
                    return wrong_method("GET");
                }
                self.sessions()
            }
            "/api/approvals" => {
                if request.method != "GET" {
                    return wrong_method("GET");
                }
                self.approvals()
            }
            "/api/jobs" => {
                if request.method != "GET" {
                    return wrong_method("GET");
                }
                self.jobs()
            }
            "/api/activity" => {
                if request.method != "GET" {
                    return wrong_method("GET");
                }
                match limit_of(query, ACTIVITY_LIMIT, ACTIVITY_LIMIT_MAX) {
                    Ok(limit) => self.activity(limit),
                    Err(reply) => reply,
                }
            }
            "/api/submit" => {
                if request.method != "POST" {
                    return wrong_method("POST");
                }
                self.submit(&request.body)
            }
            "/api/answer" => {
                if request.method != "POST" {
                    return wrong_method("POST");
                }
                self.answer(&request.body)
            }
            "/api/stop" => {
                if request.method != "POST" {
                    return wrong_method("POST");
                }
                self.stop(&request.body)
            }
            _ => {
                if let Some(id) = path.strip_prefix("/api/sessions/") {
                    if request.method != "GET" {
                        return wrong_method("GET");
                    }
                    return match limit_of(query, SESSION_LIMIT, SESSION_LIMIT_MAX) {
                        Ok(limit) => self.session_tail(id, limit),
                        Err(reply) => reply,
                    };
                }
                Reply::new(
                    404,
                    json!({"ok": false, "error": format!("there is no endpoint at {path}")}),
                )
            }
        }
    }

    /// A token, when one is configured, is the whole gate. Missing and wrong are the same answer: a client
    /// that guesses wrong learns nothing except that it guessed wrong.
    fn authorise(&self, request: &Request) -> Result<(), Reply> {
        let expected = &self.token;
        let shown = request
            .headers
            .get("authorization")
            .cloned()
            .unwrap_or_default();
        let presented = shown
            .split_once(' ')
            .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
            .map(|(_, token)| token.trim());
        let allowed =
            presented.is_some_and(|token| constant_time_eq(token.as_bytes(), expected.as_bytes()));
        if allowed {
            Ok(())
        } else {
            Err(Reply::new(
                401,
                json!({"ok": false, "error": "this API needs `Authorization: Bearer <token>`"}),
            )
            .header("WWW-Authenticate", "Bearer"))
        }
    }

    /// Defense in depth behind the token: a page in a browser can POST to loopback, and a form or a
    /// `fetch` with a token a page should not have is still a request this API never wanted. An absent
    /// Origin is a command-line client; a present one must name the address the client connected to.
    fn same_origin(&self, request: &Request) -> Result<(), Reply> {
        if let Some(origin) = request.headers.get("origin") {
            let host = request.headers.get("host").cloned().unwrap_or_default();
            let port = host.rsplit_once(':').map(|(_, port)| port).unwrap_or("80");
            let allowed = [
                format!("http://{host}"),
                format!("http://127.0.0.1:{port}"),
                format!("http://localhost:{port}"),
            ];
            if !allowed.iter().any(|name| name.eq_ignore_ascii_case(origin)) {
                return Err(Reply::new(
                    403,
                    json!({"ok": false, "error": "this API does not accept cross-origin requests"}),
                ));
            }
        }
        if request.method == "POST" {
            let kind = request
                .headers
                .get("content-type")
                .map(|value| value.to_ascii_lowercase())
                .unwrap_or_default();
            if !kind.starts_with("application/json") {
                return Err(Reply::new(
                    415,
                    json!({"ok": false, "error": "a POST needs `Content-Type: application/json`"}),
                ));
            }
        }
        Ok(())
    }

    /// Health answers even when the daemon is down: a check that fails when a dependency is down cannot
    /// report the dependency.
    fn health(&self) -> Reply {
        let live = daemon::live(&self.state).is_some();
        Reply::ok(json!({
            "ok": true,
            "service": "hades-http",
            "daemon": live,
            "state": self.state.display().to_string(),
            "version": env!("CARGO_PKG_VERSION"),
        }))
    }

    /// The daemon's socket, or the 503 that says where it would have been.
    fn socket(&self) -> Result<PathBuf, Reply> {
        daemon::live(&self.state).ok_or_else(|| {
            Reply::new(
                503,
                json!({
                    "ok": false,
                    "error": format!(
                        "no daemon is running under {}; the socket would be {}",
                        self.state.display(),
                        daemon::Daemon::socket_path(&self.state).display()
                    ),
                }),
            )
        })
    }

    fn ask(&self, socket: &Path, request: Value) -> Result<Value, Reply> {
        daemon::ask_within(socket, request, DAEMON_DEADLINE)
            .map_err(|fault| Reply::new(504, json!({"ok": false, "error": fault.detail()})))
    }

    fn status(&self) -> Reply {
        let socket = match self.socket() {
            Ok(socket) => socket,
            Err(reply) => return reply,
        };
        match self.ask(&socket, json!({"op": "status"})) {
            Ok(reply) => Reply::ok(reply),
            Err(reply) => reply,
        }
    }

    fn sessions(&self) -> Reply {
        match self.status() {
            reply if reply.status != 200 => reply,
            reply => {
                let sessions = reply
                    .body
                    .get("sessions")
                    .cloned()
                    .unwrap_or_else(|| json!([]));
                Reply::ok(json!({"ok": true, "sessions": sessions}))
            }
        }
    }

    fn approvals(&self) -> Reply {
        let socket = match self.socket() {
            Ok(socket) => socket,
            Err(reply) => return reply,
        };
        match self.ask(&socket, json!({"op": "questions"})) {
            Ok(reply) => Reply::ok(reply),
            Err(reply) => reply,
        }
    }

    /// The tail of one session log. A malformed line is skipped and counted: one bad append must not hide
    /// the history around it, and a client that cannot see the count would read a short log as a short run.
    fn session_tail(&self, id: &str, limit: usize) -> Reply {
        if !valid_session(id) {
            return Reply::bad(
                "a session id is 1..64 ASCII letters, digits, `-` and `_`; a path is not an id",
            );
        }
        let path = self.state.join("sessions").join(format!("{id}.jsonl"));
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Reply::new(
                    404,
                    json!({
                        "ok": false,
                        "error": format!("there is no session called `{id}` under {}", self.state.display()),
                    }),
                );
            }
            Err(error) => {
                return Reply::new(
                    500,
                    json!({"ok": false, "error": format!("{}: {error}", path.display())}),
                );
            }
        };

        let mut entries: Vec<Value> = Vec::new();
        let mut skipped = 0usize;
        let mut lines = 0usize;
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            lines += 1;
            match serde_json::from_str::<Value>(line) {
                Ok(value) => entries.push(value),
                Err(_) => skipped += 1,
            }
        }
        let total = entries.len();
        let start = total.saturating_sub(limit);
        let tail = entries.split_off(start);
        Reply::ok(json!({
            "ok": true,
            "session": id,
            "lines": lines,
            "skipped": skipped,
            "entries": tail,
        }))
    }

    /// The standing goal files, with the machinery beside them. A file that will not parse is not dropped:
    /// the person who wrote it is the one who can fix it.
    fn jobs(&self) -> Reply {
        let (jobs, problems) = cron::load(&self.state);
        let listed = jobs
            .iter()
            .map(|job| {
                let pending = retry::read(&self.state, job.id).map(|pending| {
                    json!({
                        "window": pending.window,
                        "attempt": pending.attempt,
                        "at": pending.at,
                        "error": pending.error,
                    })
                });
                let latest = std::fs::read_to_string(retry::log_path(&self.state, job.id))
                    .ok()
                    .and_then(|text| {
                        text.lines()
                            .rfind(|line| !line.trim().is_empty())
                            .map(str::to_string)
                    })
                    .and_then(|line| serde_json::from_str::<Value>(&line).ok());
                json!({
                    "id": job.id,
                    "schedule": job.schedule,
                    "mode": job.mode.name(),
                    "goal": job.goal,
                    "last": job.last,
                    "pending": pending,
                    "latest": latest,
                })
            })
            .collect::<Vec<_>>();
        Reply::ok(json!({"ok": true, "jobs": listed, "problems": problems}))
    }

    fn activity(&self, limit: usize) -> Reply {
        let all = activity::read(&self.state);
        let start = all.len().saturating_sub(limit);
        let entries = all[start..]
            .iter()
            .map(|line| {
                json!({
                    "at": line.at,
                    "actor": line.actor,
                    "layer": line.layer,
                    "action": line.action,
                    "decision": line.decision,
                    "reason": line.reason,
                    "artifact": line.artifact,
                })
            })
            .collect::<Vec<_>>();
        Reply::ok(json!({"ok": true, "lines": all.len(), "entries": entries}))
    }

    /// A submit answers `202` with the daemon's acknowledgement. The daemon writes that line before it does
    /// any work, and `ask_within` reads exactly one line, so closing here leaves the walk running and the
    /// session log is the handle a client polls.
    ///
    /// No policy and no root are sent: those come from the daemon's config, exactly as they do for a
    /// command-line run. `state` is sent explicitly, because a daemon's default state and this server's
    /// `--state` are two different sentences and only one of them is where the client will look.
    fn submit(&self, body: &[u8]) -> Reply {
        let map = match object(body) {
            Ok(map) => map,
            Err(reply) => return reply,
        };
        if let Err(reply) = only(&map, &["goal", "session"]) {
            return reply;
        }
        let goal = match string(&map, "goal") {
            Ok(goal) => goal,
            Err(reply) => return reply,
        };
        if goal.trim().is_empty() {
            return Reply::bad("`goal` is empty; a submitted run needs something to do");
        }
        if goal.len() > 20_000 {
            return Reply::bad("`goal` is over 20,000 bytes");
        }
        let session = match string(&map, "session") {
            Ok(session) => session,
            Err(reply) => return reply,
        };
        if !valid_session(&session) {
            return Reply::bad(
                "`session` is 1..64 ASCII letters, digits, `-` and `_`; it becomes a file name",
            );
        }

        let socket = match self.socket() {
            Ok(socket) => socket,
            Err(reply) => return reply,
        };
        let request = json!({
            "op": "submit",
            "state": self.state.display().to_string(),
            "goal": goal,
            "session": session,
        });
        match self.ask(&socket, request) {
            Ok(reply) if reply.get("ok").and_then(Value::as_bool) == Some(false) => Reply::new(
                409,
                json!({
                    "ok": false,
                    "error": reply
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("the daemon refused the submit"),
                }),
            ),
            Ok(reply) => Reply::new(202, reply),
            Err(reply) => reply,
        }
    }

    /// An answer records the decision and starts nothing. The `by` names this surface, so the ledger
    /// answers "who allowed what" without the daemon guessing at an identity.
    fn answer(&self, body: &[u8]) -> Reply {
        let map = match object(body) {
            Ok(map) => map,
            Err(reply) => return reply,
        };
        if let Err(reply) = only(&map, &["id", "decision", "scope"]) {
            return reply;
        }
        let id = match identifier(&map, "id") {
            Ok(id) => id,
            Err(reply) => return reply,
        };
        let decision = match string(&map, "decision") {
            Ok(decision) => decision,
            Err(reply) => return reply,
        };
        if decision != "allow" && decision != "deny" {
            return Reply::bad("`decision` is `allow` or `deny`");
        }
        let scope = match map.get("scope") {
            Some(Value::String(scope)) if scope == "once" || scope == "session" => scope.clone(),
            Some(_) => return Reply::bad("`scope` is `once` or `session`"),
            None => "once".to_string(),
        };

        let socket = match self.socket() {
            Ok(socket) => socket,
            Err(reply) => return reply,
        };
        let request = json!({
            "op": "answer",
            "id": id,
            "decision": decision,
            "scope": scope,
            "by": "http",
        });
        match self.ask(&socket, request) {
            Ok(reply) if reply.get("ok").and_then(Value::as_bool) == Some(false) => Reply::new(
                409,
                json!({
                    "ok": false,
                    "error": reply
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("the daemon refused the answer"),
                }),
            ),
            Ok(reply) => Reply::ok(reply),
            Err(reply) => reply,
        }
    }

    /// Stop one session, one numbered run, or every run. The daemon has no `all` field: an absent or empty
    /// session already means every run, so `all: true` is translated here rather than invented there.
    fn stop(&self, body: &[u8]) -> Reply {
        let map = match object(body) {
            Ok(map) => map,
            Err(reply) => return reply,
        };
        if let Err(reply) = only(&map, &["session", "run", "all"]) {
            return reply;
        }
        let all = match map.get("all") {
            None => false,
            Some(Value::Bool(all)) => *all,
            Some(_) => return Reply::bad("`all` is a boolean"),
        };
        let session = match map.get("session") {
            None => None,
            Some(Value::String(session)) => {
                if !valid_session(session) {
                    return Reply::bad(
                        "`session` is 1..64 ASCII letters, digits, `-` and `_`; it names a log",
                    );
                }
                Some(session.clone())
            }
            Some(_) => return Reply::bad("`session` is a string"),
        };
        let run = match map.get("run") {
            None => None,
            Some(_) => match number(&map, "run") {
                Ok(run) => Some(run),
                Err(reply) => return reply,
            },
        };

        if all && (session.is_some() || run.is_some()) {
            return Reply::bad("`all` cannot be combined with `session` or `run`");
        }
        if session.is_none() && run.is_none() && !all {
            return Reply::bad("say `all: true`, or name a `session` or a `run`");
        }

        let mut request = json!({"op": "stop", "by": "http"});
        if let Some(session) = session {
            request["session"] = json!(session);
        }
        if let Some(run) = run {
            request["run"] = json!(run);
        }

        let socket = match self.socket() {
            Ok(socket) => socket,
            Err(reply) => return reply,
        };
        match self.ask(&socket, request) {
            Ok(reply) if reply.get("ok").and_then(Value::as_bool) == Some(false) => Reply::new(
                409,
                json!({
                    "ok": false,
                    "error": reply
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("the daemon refused the stop"),
                }),
            ),
            Ok(reply) => Reply::ok(reply),
            Err(reply) => reply,
        }
    }
}

/// One parsed request. The body is bytes rather than text because a body is not required to be UTF-8, and a
/// parser that assumes it is has already decided what a client may send.
struct Request {
    method: String,
    target: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

/// What to answer. Status and body are separate so a test can assert the status without reading prose.
struct Reply {
    status: u16,
    body: Value,
    headers: Vec<(&'static str, &'static str)>,
}

impl Reply {
    fn new(status: u16, body: Value) -> Reply {
        Reply {
            status,
            body,
            headers: Vec::new(),
        }
    }

    fn ok(body: Value) -> Reply {
        Reply::new(200, body)
    }

    fn bad(message: impl Into<String>) -> Reply {
        Reply::new(400, json!({"ok": false, "error": message.into()}))
    }

    fn header(mut self, name: &'static str, value: &'static str) -> Reply {
        self.headers.push((name, value));
        self
    }
}

/// Read one request with three caps: 16 KiB of line and headers, 1 MiB of body, 10 s to say it all.
fn read_request(stream: &mut TcpStream) -> Result<Request, Reply> {
    let deadline = Instant::now() + REQUEST_DEADLINE;
    let mut reader = BufReader::new(stream);
    let mut used = 0usize;

    let mut line = String::new();
    read_line(&mut reader, deadline, &mut used, &mut line)?;
    let line = line.trim_end_matches(['\r', '\n']);
    let mut parts = line.split(' ');
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    let version = parts.next().unwrap_or_default();
    if method.is_empty() || target.is_empty() || version.is_empty() || parts.next().is_some() {
        return Err(Reply::bad(
            "the request line is not `METHOD TARGET HTTP/1.1`",
        ));
    }
    if !version.starts_with("HTTP/1.") {
        return Err(Reply::bad("this server speaks HTTP/1.0 and HTTP/1.1"));
    }
    // A target must be an origin-form path. `http://host/path` is proxy-shaped and `*` is meaningless
    // here; both are refused rather than guessed at.
    if !target.starts_with('/') {
        return Err(Reply::bad(
            "the request target has to be a path beginning with `/`",
        ));
    }
    if target.chars().any(|c| c.is_control()) {
        return Err(Reply::bad("the request target has a control character"));
    }
    if method != "GET" && method != "POST" {
        return Err(Reply::new(
            405,
            json!({"ok": false, "error": "this API takes GET and POST"}),
        )
        .header("Allow", "GET, POST"));
    }

    let mut headers: BTreeMap<String, String> = BTreeMap::new();
    loop {
        let mut raw = String::new();
        read_line(&mut reader, deadline, &mut used, &mut raw)?;
        if raw == "\r\n" || raw == "\n" {
            break;
        }
        let trimmed = raw.trim_end_matches(['\r', '\n']);
        let Some((name, value)) = trimmed.split_once(':') else {
            return Err(Reply::bad("a header line has no colon"));
        };
        let name = name.trim().to_ascii_lowercase();
        if name.is_empty() || name.chars().any(|c| c.is_whitespace()) {
            return Err(Reply::bad("a header name is empty or has a space"));
        }
        let value = value.trim().to_string();
        if name == "transfer-encoding" {
            return Err(Reply::new(
                411,
                json!({"ok": false, "error": "chunked bodies are not read; send a Content-Length"}),
            ));
        }
        if name == "expect" && value.eq_ignore_ascii_case("100-continue") {
            return Err(Reply::new(
                417,
                json!({"ok": false, "error": "this server does not continue an expectation"}),
            ));
        }
        if name == "content-length" && headers.contains_key("content-length") {
            return Err(Reply::bad("the request has two Content-Length headers"));
        }
        headers.insert(name, value);
    }

    let length = match headers.get("content-length") {
        Some(value) => value
            .parse::<usize>()
            .map_err(|_| Reply::bad("`Content-Length` is not a number"))?,
        None => 0,
    };
    if method == "GET" && length > 0 {
        return Err(Reply::bad("a GET does not carry a body"));
    }
    if method == "POST" && !headers.contains_key("content-length") {
        return Err(Reply::new(
            411,
            json!({"ok": false, "error": "a POST needs a Content-Length"}),
        ));
    }
    if length > BODY_CAP {
        return Err(Reply::new(
            413,
            json!({"ok": false, "error": "the body is over 1 MiB"}),
        ));
    }

    let mut body = vec![0u8; length];
    if length > 0 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(timeout());
        }
        reader
            .get_ref()
            .set_read_timeout(Some(remaining))
            .map_err(|error| Reply::bad(format!("the connection could not be timed: {error}")))?;
        reader
            .read_exact(&mut body)
            .map_err(|error| read_failure(error, "the body"))?;
    }

    Ok(Request {
        method,
        target,
        headers,
        body,
    })
}

/// One line, inside the header cap and the request deadline. `used` is the running count of the request
/// line plus every header, so the cap is on the whole preamble rather than on each line separately.
fn read_line(
    reader: &mut BufReader<&mut TcpStream>,
    deadline: Instant,
    used: &mut usize,
    line: &mut String,
) -> Result<(), Reply> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(timeout());
    }
    reader
        .get_ref()
        .set_read_timeout(Some(remaining))
        .map_err(|error| Reply::bad(format!("the connection could not be timed: {error}")))?;
    let read = reader
        .read_line(line)
        .map_err(|error| read_failure(error, "the request"))?;
    if read == 0 {
        return Err(Reply::bad("the request ended before it was complete"));
    }
    *used += read;
    if *used > HEADER_CAP {
        return Err(Reply::new(
            413,
            json!({"ok": false, "error": "the request line and headers are over 16 KiB"}),
        ));
    }
    Ok(())
}

/// A method that does not belong on this path, with the one it does belong to.
fn wrong_method(allowed: &'static str) -> Reply {
    Reply::new(
        405,
        json!({"ok": false, "error": format!("this path takes {allowed}")}),
    )
    .header("Allow", allowed)
}

fn timeout() -> Reply {
    Reply::new(
        408,
        json!({"ok": false, "error": "the request was not complete within 10 s"}),
    )
}

fn read_failure(error: std::io::Error, what: &str) -> Reply {
    match error.kind() {
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => timeout(),
        std::io::ErrorKind::InvalidData => Reply::bad(format!("{what} is not UTF-8")),
        _ => Reply::bad(format!("{what} could not be read: {error}")),
    }
}

fn write_reply(stream: &mut TcpStream, reply: Reply) -> std::io::Result<()> {
    let body = reply.body.to_string();
    let mut head = format!("HTTP/1.1 {} {}\r\n", reply.status, reason(reply.status));
    head.push_str("Content-Type: application/json\r\n");
    head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    head.push_str("Cache-Control: no-store\r\n");
    head.push_str("X-Content-Type-Options: nosniff\r\n");
    head.push_str("Connection: close\r\n");
    for (name, value) in &reply.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Payload Too Large",
        417 => "Expectation Failed",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Unknown",
    }
}

/// A JSON object, or the 400 that says why not. Unknown fields are refused by name: a typo in a field the
/// API used to accept must not silently become a different request.
fn object(body: &[u8]) -> Result<Map<String, Value>, Reply> {
    if body.is_empty() {
        return Err(Reply::bad("the body has to be a JSON object"));
    }
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| Reply::bad(format!("the body is not JSON: {error}")))?;
    match value {
        Value::Object(map) => Ok(map),
        _ => Err(Reply::bad("the body has to be a JSON object")),
    }
}

fn only(map: &Map<String, Value>, allowed: &[&str]) -> Result<(), Reply> {
    for key in map.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(Reply::bad(format!("unknown field `{key}`")));
        }
    }
    Ok(())
}

fn string(map: &Map<String, Value>, key: &str) -> Result<String, Reply> {
    match map.get(key) {
        Some(Value::String(value)) => Ok(value.clone()),
        Some(_) => Err(Reply::bad(format!("`{key}` has to be a string"))),
        None => Err(Reply::bad(format!("`{key}` is required"))),
    }
}

/// A positive integer. Zero is not a run number, so it is refused rather than passed on.
fn number(map: &Map<String, Value>, key: &str) -> Result<u64, Reply> {
    match map.get(key) {
        Some(Value::Number(number)) => match number.as_u64() {
            Some(0) | None => Err(Reply::bad(format!("`{key}` is a positive integer"))),
            Some(number) => Ok(number),
        },
        Some(_) => Err(Reply::bad(format!("`{key}` is a number"))),
        None => Err(Reply::bad(format!("`{key}` is required"))),
    }
}

/// A record id. The broker numbers its first question 0, so zero is a real identity here even though zero
/// is not a run number.
fn identifier(map: &Map<String, Value>, key: &str) -> Result<u64, Reply> {
    match map.get(key) {
        Some(Value::Number(number)) => number
            .as_u64()
            .ok_or_else(|| Reply::bad(format!("`{key}` is a whole number"))),
        Some(_) => Err(Reply::bad(format!("`{key}` is a number"))),
        None => Err(Reply::bad(format!("`{key}` is required"))),
    }
}

/// `limit` defaults and is clamped; anything else in the query is refused rather than ignored.
fn limit_of(query: &str, default: usize, max: usize) -> Result<usize, Reply> {
    if query.is_empty() {
        return Ok(default);
    }
    let mut limit = None;
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key.is_empty() {
            continue;
        }
        if key != "limit" {
            return Err(Reply::bad(format!(
                "`{key}` is not a query this endpoint takes"
            )));
        }
        if limit.is_some() {
            return Err(Reply::bad("`limit` is given twice"));
        }
        let parsed = value
            .parse::<usize>()
            .map_err(|_| Reply::bad(format!("`limit={value}` is not a number")))?;
        limit = Some(parsed.clamp(1, max));
    }
    Ok(limit.unwrap_or(default))
}

/// A session id becomes a file name under `<state>/sessions`, so it is ours to check: `SessionId::new`
/// accepts any string and a `/` or a `..` would name a file outside the directory.
fn valid_session(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        })
}

/// Compare without an early exit. The length is observable; the bytes are not.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (left, right) in left.iter().zip(right.iter()) {
        difference |= left ^ right;
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_id_is_a_plain_file_name() {
        assert!(valid_session("http-1"));
        assert!(valid_session("tg-123456"));
        assert!(valid_session("daemon"));
        assert!(!valid_session(""));
        assert!(!valid_session(".."));
        assert!(!valid_session("a/b"));
        assert!(!valid_session("a.b"));
        assert!(!valid_session(&"a".repeat(65)));
    }

    #[test]
    fn tokens_compare_by_value() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }

    #[test]
    fn a_limit_defaults_and_clamps() {
        assert_eq!(limit_of("", 50, 500).ok(), Some(50));
        assert_eq!(limit_of("limit=2", 50, 500).ok(), Some(2));
        assert_eq!(limit_of("limit=0", 50, 500).ok(), Some(1));
        assert_eq!(limit_of("limit=9999", 50, 500).ok(), Some(500));
        assert!(limit_of("before=3", 50, 500).is_err());
        assert!(limit_of("limit=two", 50, 500).is_err());
    }
}
