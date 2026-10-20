//! The oracle for the HTTP surface, driven through the real binary over a real socket.
//!
//! Every test gets its own scratch state and asks for port `0`; the server prints the address it actually
//! bound, and the test reads that line. Nothing here needs a model or a network: the read endpoints answer
//! from files, and the control endpoints are checked either against a real daemon or against a socket that
//! records what the API sent.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

fn scratch_base() -> PathBuf {
    let base = harness::scratch::root();
    std::fs::create_dir_all(&base).ok();
    std::fs::canonicalize(&base).unwrap_or(base)
}

fn scratch(name: &str) -> PathBuf {
    let dir = scratch_base().join(format!("hades-http-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

fn isolated_home() -> PathBuf {
    let dir = scratch_base().join(format!("hades-http-home-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("an isolated Hades home");
    dir
}

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_hades")
}

/// A running HTTP server, and the address it announced.
struct Http {
    child: Child,
    address: SocketAddr,
}

impl Http {
    fn start(state: &Path, extra: &[&str], envs: &[(&str, &str)]) -> Http {
        let mut command = Command::new(binary());
        command
            .args(["http", "--state"])
            .arg(state)
            .args(["--bind", "127.0.0.1:0"])
            .args(extra)
            .env("HADES_HOME", isolated_home())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        for (name, value) in envs {
            command.env(name, value);
        }
        let mut child = command.spawn().expect("the server starts");
        let stderr = child.stderr.take().expect("stderr is piped");
        let mut reader = BufReader::new(stderr);

        let deadline = Instant::now() + Duration::from_secs(20);
        let address = loop {
            assert!(
                Instant::now() < deadline,
                "the server never said where it listens"
            );
            let mut line = String::new();
            let read = reader.read_line(&mut line).expect("a line of stderr");
            assert!(
                read > 0,
                "the server exited before it said where it listens"
            );
            if let Some(rest) = line.trim().strip_prefix("hades http listening on ") {
                break rest.parse::<SocketAddr>().expect("the announced address");
            }
        };
        // Keep the pipe drained so a chatty server can never block on a full stderr buffer.
        std::thread::spawn(move || {
            let mut sink = String::new();
            let _ = reader.read_to_string(&mut sink);
        });

        Http { child, address }
    }

    fn get(&self, path: &str) -> Answer {
        self.raw(&format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n"))
    }

    fn post(&self, path: &str, body: &Value) -> Answer {
        let body = body.to_string();
        self.raw(&format!(
            "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ))
    }

    fn raw(&self, text: &str) -> Answer {
        request(self.address, text)
    }

    fn wait(&mut self) -> std::process::ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = self.child.try_wait().expect("the child may be asked") {
                return status;
            }
            assert!(Instant::now() < deadline, "the server did not exit");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Http {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Answer {
    status: u16,
    headers: BTreeMap<String, String>,
    body: Value,
}

fn request(address: SocketAddr, text: &str) -> Answer {
    let mut stream = TcpStream::connect(address).expect("a connection");
    stream.set_read_timeout(Some(Duration::from_secs(20))).ok();
    stream
        .write_all(text.as_bytes())
        .expect("the request goes out");
    stream.flush().ok();
    let mut bytes = Vec::new();
    stream
        .read_to_end(&mut bytes)
        .expect("the answer comes back");
    parse(&bytes)
}

fn parse(bytes: &[u8]) -> Answer {
    let split = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("the headers end");
    let head = String::from_utf8_lossy(&bytes[..split]);
    let body = &bytes[split + 4..];
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|status| status.parse().ok())
        .expect("a status line");
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    let body = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(body).expect("a JSON body")
    };
    Answer {
        status,
        headers,
        body,
    }
}

/// A daemon on a scratch state, taken away when the test ends.
struct Daemon {
    child: Child,
    socket: PathBuf,
}

impl Daemon {
    fn start(state: &Path) -> Daemon {
        let socket = state.join("daemon.sock");
        let child = Command::new(binary())
            .args(["daemon", "--state"])
            .arg(state)
            .env("HADES_HOME", isolated_home())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("the daemon starts");
        let daemon = Daemon { child, socket };
        daemon.wait_until_listening();
        daemon
    }

    fn wait_until_listening(&self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if let Ok(answer) = ask(&self.socket, json!({"op": "ping"}))
                && answer["ok"] == json!(true)
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("the daemon on {} never answered", self.socket.display());
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn ask(socket: &Path, request: Value) -> Result<Value, String> {
    let mut stream =
        std::os::unix::net::UnixStream::connect(socket).map_err(|error| error.to_string())?;
    writeln!(stream, "{request}").map_err(|error| error.to_string())?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .map_err(|error| error.to_string())?;
    serde_json::from_str(line.trim()).map_err(|error| error.to_string())
}

#[test]
fn health_answers_without_a_daemon() {
    let state = scratch("health");
    let server = Http::start(&state, &[], &[]);
    let answer = server.get("/api/health");
    assert_eq!(answer.status, 200);
    assert_eq!(answer.body["ok"], json!(true));
    assert_eq!(answer.body["daemon"], json!(false));
    assert_eq!(answer.body["service"], json!("hades-http"));
    assert_eq!(answer.body["state"], json!(state.display().to_string()));
}

#[test]
fn an_unknown_path_is_json_not_html() {
    let state = scratch("unknown");
    let server = Http::start(&state, &[], &[]);
    let answer = server.get("/api/nope");
    assert_eq!(answer.status, 404);
    assert_eq!(
        answer.headers.get("content-type").map(String::as_str),
        Some("application/json")
    );
    assert_eq!(answer.body["ok"], json!(false));
}

#[test]
fn a_wrong_method_says_which_one_is_right() {
    let state = scratch("method");
    let server = Http::start(&state, &[], &[]);

    let posted = server.post("/api/health", &json!({}));
    assert_eq!(posted.status, 405);
    assert_eq!(posted.headers.get("allow").map(String::as_str), Some("GET"));

    let deleted = server.raw("DELETE /api/health HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert_eq!(deleted.status, 405);
    // A method the parser does not know is refused before routing, so it names every method the API takes.
    let allow = deleted
        .headers
        .get("allow")
        .map(String::as_str)
        .unwrap_or_default();
    assert!(
        allow.contains("GET") && allow.contains("POST"),
        "Allow was {allow}"
    );
}

#[test]
fn a_body_over_one_mebibyte_is_refused_before_it_is_read() {
    let state = scratch("big");
    let server = Http::start(&state, &[], &[]);
    let answer = server
        .raw("POST /api/submit HTTP/1.1\r\nHost: localhost\r\nContent-Length: 1048577\r\n\r\n");
    assert_eq!(answer.status, 413);
    assert_eq!(answer.body["ok"], json!(false));
}

#[test]
fn status_comes_from_the_daemon() {
    let state = scratch("status");
    let daemon = Daemon::start(&state);
    let announced: Value = serde_json::from_str(
        &std::fs::read_to_string(state.join("daemon.json")).expect("daemon.json"),
    )
    .expect("the announcement is JSON");
    let pid = daemon.child.id() as u64;
    assert_eq!(announced["pid"], json!(pid));

    let server = Http::start(&state, &[], &[]);
    let answer = server.get("/api/status");
    assert_eq!(answer.status, 200);
    assert_eq!(answer.body["pid"], json!(pid));
    assert!(answer.body["sessions"].is_array());
    assert_eq!(
        answer.body["socket"],
        json!(daemon.socket.display().to_string())
    );
}

#[test]
fn approvals_show_a_seeded_question() {
    let state = scratch("approvals");
    std::fs::write(
        state.join("approvals.jsonl"),
        json!({
            "v": 1,
            "event": "asked",
            "id": 7,
            "session": "http-1",
            "action": "permanently delete",
            "reason": "the program removes a directory",
            "at": "1700000000",
        })
        .to_string()
            + "\n",
    )
    .expect("a seeded question");
    let _daemon = Daemon::start(&state);

    let server = Http::start(&state, &[], &[]);
    let answer = server.get("/api/approvals");
    assert_eq!(answer.status, 200);
    assert_eq!(answer.body["questions"][0]["id"], json!(7));
    assert_eq!(answer.body["questions"][0]["session"], json!("http-1"));
    assert_eq!(
        answer.body["questions"][0]["action"],
        json!("permanently delete")
    );
}

#[test]
fn a_session_tail_keeps_the_last_entries_and_counts_a_bad_line() {
    let state = scratch("tail");
    let sessions = state.join("sessions");
    std::fs::create_dir_all(&sessions).expect("the sessions directory");
    std::fs::write(
        sessions.join("http-1.jsonl"),
        [
            json!({"id": 1, "kind": "user"}).to_string(),
            json!({"id": 2, "kind": "tool_call"}).to_string(),
            "{not json".to_string(),
            json!({"id": 3, "kind": "tool_result"}).to_string(),
        ]
        .join("\n")
            + "\n",
    )
    .expect("a seeded log");

    let server = Http::start(&state, &[], &[]);
    let answer = server.get("/api/sessions/http-1?limit=2");
    assert_eq!(answer.status, 200);
    assert_eq!(answer.body["skipped"], json!(1));
    assert_eq!(answer.body["lines"], json!(4));
    assert_eq!(answer.body["entries"][0]["id"], json!(2));
    assert_eq!(answer.body["entries"][1]["id"], json!(3));
    assert_eq!(answer.body["entries"].as_array().map(Vec::len), Some(2));
}

#[test]
fn a_token_is_the_whole_gate() {
    let state = scratch("token");
    let server = Http::start(&state, &[], &[("HADES_HTTP_TOKEN", "sekret")]);

    let missing = server.get("/api/health");
    assert_eq!(missing.status, 401);
    assert_eq!(
        missing.headers.get("www-authenticate").map(String::as_str),
        Some("Bearer")
    );

    let wrong = server
        .raw("GET /api/health HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer nope\r\n\r\n");
    assert_eq!(wrong.status, 401);

    // Header names and the scheme are case-insensitive; the token itself is not.
    let right = server
        .raw("GET /api/health HTTP/1.1\r\nHost: localhost\r\nauthorization: bearer sekret\r\n\r\n");
    assert_eq!(right.status, 200);
}

#[test]
fn a_non_loopback_bind_without_a_token_is_refused() {
    let state = scratch("bind");
    let output = Command::new(binary())
        .args(["http", "--state"])
        .arg(&state)
        .args(["--bind", "0.0.0.0:0"])
        .env("HADES_HOME", isolated_home())
        .env_remove("HADES_HTTP_TOKEN")
        .output()
        .expect("the binary runs");
    assert_eq!(output.status.code(), Some(2));
    // The CLI writes its answer to stdout by design (`run_with` takes the sink), so read both streams.
    let shown = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        shown.contains("HADES_HTTP_TOKEN"),
        "the refusal has to name the environment variable: {shown}"
    );
}

#[test]
fn jobs_carry_their_machinery_and_name_a_file_that_will_not_parse() {
    let state = scratch("jobs");
    std::fs::create_dir_all(state.join("jobs")).expect("the jobs directory");
    std::fs::create_dir_all(state.join("retry")).expect("the retry directory");
    std::fs::create_dir_all(state.join("attempts")).expect("the attempts directory");
    std::fs::write(
        state.join("jobs/1-digest.md"),
        "---\nid: 1\nchat: 42\nschedule: daily 09:00\nmode: act\nlast: 1700000000\n---\nwrite the digest\n",
    )
    .expect("a job file");
    std::fs::write(
        state.join("jobs/2-broken.md"),
        "---\nschedule: hourly\n---\nthis file names no id\n",
    )
    .expect("a broken job file");
    std::fs::write(
        state.join("retry/1.json"),
        serde_json::to_string_pretty(&json!({
            "window": "1@1700000000",
            "attempt": 1,
            "at": 1700000300,
            "error": "the endpoint timed out",
        }))
        .expect("JSON")
            + "\n",
    )
    .expect("a pending retry");
    std::fs::write(
        state.join("attempts/1.jsonl"),
        json!({
            "window": "1@1700000000",
            "attempt": 1,
            "started": 1700000000,
            "finished": 1700000004,
            "outcome": "failed",
            "detail": "the endpoint timed out",
        })
        .to_string()
            + "\n",
    )
    .expect("an attempt");

    let server = Http::start(&state, &[], &[]);
    let answer = server.get("/api/jobs");
    assert_eq!(answer.status, 200);
    let job = &answer.body["jobs"][0];
    assert_eq!(job["id"], json!(1));
    assert_eq!(job["schedule"], json!("daily 09:00"));
    assert_eq!(job["mode"], json!("act"));
    assert_eq!(job["goal"], json!("write the digest"));
    assert_eq!(job["last"], json!(1700000000));
    assert_eq!(job["pending"]["attempt"], json!(1));
    assert_eq!(job["latest"]["outcome"], json!("failed"));

    let problems = answer.body["problems"].as_array().expect("problems");
    assert_eq!(problems.len(), 1);
    assert!(
        problems[0]
            .as_str()
            .unwrap_or_default()
            .contains("2-broken.md"),
        "the file that will not parse has to be named: {problems:?}"
    );
}

#[test]
fn an_activity_tail_keeps_the_last_lines() {
    let state = scratch("activity");
    let line = |action: &str| {
        json!({
            "at": 1700000000,
            "actor": "chat:42",
            "layer": "floor",
            "action": action,
            "decision": "allow",
            "reason": "nothing objected",
            "artifact": "",
        })
        .to_string()
    };
    std::fs::write(
        state.join("activity.jsonl"),
        [line("shell: one"), line("shell: two"), line("shell: three")].join("\n") + "\n",
    )
    .expect("a seeded ledger");

    let server = Http::start(&state, &[], &[]);
    let answer = server.get("/api/activity?limit=2");
    assert_eq!(answer.status, 200);
    assert_eq!(answer.body["lines"], json!(3));
    assert_eq!(answer.body["entries"][0]["action"], json!("shell: two"));
    assert_eq!(answer.body["entries"][1]["action"], json!("shell: three"));
}

#[test]
fn stop_without_a_daemon_names_the_socket() {
    let state = scratch("stop-down");
    let server = Http::start(&state, &[], &[]);
    let answer = server.post("/api/stop", &json!({"session": "http-1"}));
    assert_eq!(answer.status, 503);
    assert_eq!(answer.body["ok"], json!(false));
    assert!(
        answer.body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("daemon"),
        "the 503 has to say what is missing: {}",
        answer.body["error"]
    );
}

#[test]
fn submit_without_a_daemon_spawns_nothing() {
    let state = scratch("submit-down");
    let server = Http::start(&state, &[], &[]);
    let answer = server.post(
        "/api/submit",
        &json!({"goal": "say hello", "session": "http-1"}),
    );
    assert_eq!(answer.status, 503);
    assert!(!state.join("sessions/http-1.jsonl").exists());
}

#[test]
fn once_answers_one_request_and_exits_zero() {
    let state = scratch("once");
    let mut server = Http::start(&state, &["--once"], &[]);
    let answer = server.get("/api/health");
    assert_eq!(answer.status, 200);
    let status = server.wait();
    assert_eq!(status.code(), Some(0));

    // The listener is gone with the process.
    assert!(TcpStream::connect(server.address).is_err());
}

#[test]
fn a_traversal_attempt_never_reaches_a_file_outside_the_session_directory() {
    let state = scratch("traversal");
    std::fs::write(state.join("config.json"), "{\"secret\": true}").expect("a file to protect");
    let server = Http::start(&state, &[], &[]);
    let answer =
        server.raw("GET /api/sessions/../../config.json HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert_eq!(answer.status, 400);
    assert_eq!(answer.body["ok"], json!(false));
}

#[test]
fn stop_all_sends_a_request_with_no_session() {
    let state = scratch("stop-all");
    let socket = state.join("fake.sock");
    let listener = UnixListener::bind(&socket).expect("a fake daemon socket");
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut line = String::new();
            let mut reader = BufReader::new(stream.try_clone().expect("a copy"));
            if reader.read_line(&mut line).is_ok() {
                let _ = sender.send(line);
            }
            writeln!(
                stream,
                "{}",
                json!({"ok": true, "count": 0, "stopping": []})
            )
            .ok();
        }
    });
    std::fs::write(
        state.join("daemon.json"),
        json!({"pid": std::process::id(), "socket": socket.display().to_string()}).to_string(),
    )
    .expect("an announcement naming a live pid");

    let server = Http::start(&state, &[], &[]);
    let answer = server.post("/api/stop", &json!({"all": true}));
    assert_eq!(answer.status, 200);

    let sent: Value = serde_json::from_str(
        &receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("the request"),
    )
    .expect("the request is JSON");
    assert_eq!(sent["op"], json!("stop"));
    assert!(
        sent.get("session").is_none(),
        "`all` has to become an omitted session: {sent}"
    );
}

#[test]
fn a_submit_cannot_carry_a_mode_the_daemon_would_ignore() {
    let state = scratch("typo");
    let server = Http::start(&state, &[], &[]);
    let answer = server.post(
        "/api/submit",
        &json!({"goal": "say hello", "session": "http-1", "mode": "act"}),
    );
    assert_eq!(answer.status, 400);
    assert!(
        answer.body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("mode"),
        "the refused field has to be named: {}",
        answer.body["error"]
    );
}

#[test]
fn an_answer_can_name_question_zero() {
    // The broker numbers its first question 0, so a validator that insists on a positive integer refuses a
    // real question. The request shape is asserted against a socket, not a live answer.
    let state = scratch("answer-zero");
    let socket = state.join("fake.sock");
    let listener = UnixListener::bind(&socket).expect("a fake daemon socket");
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut line = String::new();
            let mut reader = BufReader::new(stream.try_clone().expect("a copy"));
            if reader.read_line(&mut line).is_ok() {
                let _ = sender.send(line);
            }
            writeln!(stream, "{}", json!({"ok": true, "id": 0})).ok();
        }
    });
    std::fs::write(
        state.join("daemon.json"),
        json!({"pid": std::process::id(), "socket": socket.display().to_string()}).to_string(),
    )
    .expect("an announcement naming a live pid");

    let server = Http::start(&state, &[], &[]);
    let answer = server.post("/api/answer", &json!({"id": 0, "decision": "deny"}));
    assert_eq!(answer.status, 200);

    let sent: Value = serde_json::from_str(
        &receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("the request"),
    )
    .expect("the request is JSON");
    assert_eq!(sent["op"], json!("answer"));
    assert_eq!(sent["id"], json!(0));
    assert_eq!(sent["scope"], json!("once"));
    assert_eq!(sent["by"], json!("http"));
}

#[test]
fn two_content_lengths_are_refused() {
    let state = scratch("lengths");
    let server = Http::start(&state, &[], &[]);
    let answer = server.raw(
        "POST /api/submit HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\nContent-Length: 2\r\n\r\n{}",
    );
    assert_eq!(answer.status, 400);
}
