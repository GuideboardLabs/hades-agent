//! The oracle for the daemon, driven through the real binary over a real socket.
//!
//! The proof this file exists for is the third test: a `kill -9` while a run is in flight, and a second
//! daemon on the same state directory that finds everything the first one had. It is a real signal to a real
//! process for the same reason the claim is worth making: a daemon that held state in memory would pass a
//! graceful-shutdown test and fail this one.
//!
//! Nothing here needs a model or a network. Every run is scripted with `--program`, which is the seam the
//! design put there for exactly this.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

fn scratch(name: &str) -> PathBuf {
    let dir = scratch_base().join(format!("hades-daemon-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_hades")
}

fn repo(state: &Path) -> PathBuf {
    let root = state.join("repo");
    std::fs::create_dir_all(&root).expect("the tree");
    root
}

/// A daemon on a state directory, and the two ways a test takes it away.
struct Running {
    child: Child,
    state: PathBuf,
    socket: PathBuf,
}

impl Running {
    fn start(state: &Path) -> Running {
        let socket = state.join("daemon.sock");
        let child = Command::new(binary())
            .args(["daemon", "--state"])
            .arg(state)
            .env("HADES_HOME", isolated_home())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("the daemon starts");
        let running = Running {
            child,
            state: state.to_path_buf(),
            socket,
        };
        running.wait_until_listening();
        running
    }

    /// Readiness is the socket answering, not the file appearing: a bound socket that has not entered its
    /// loop yet would accept a connection and hang on it.
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
        panic!(
            "the daemon on {} never answered a ping",
            self.socket.display()
        );
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    /// SIGKILL, which is the point: nothing gets to run on the way out, so nothing gets a chance to save
    /// anything that only exists in memory.
    fn kill(mut self) -> PathBuf {
        let _ = Command::new("kill")
            .args(["-9", &self.child.id().to_string()])
            .status();
        let _ = self.child.wait();
        self.state.clone()
    }

    fn stop(mut self) {
        let _ = ask(&self.socket, json!({"op": "shutdown"}));
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                assert!(
                    !self.socket.exists(),
                    "a clean exit left {} behind",
                    self.socket.display()
                );
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("the daemon did not stop when it was asked to");
    }
}

/// One request, one answer.
fn ask(socket: &Path, request: Value) -> Result<Value, String> {
    let mut stream = UnixStream::connect(socket).map_err(|error| error.to_string())?;
    writeln!(stream, "{request}").map_err(|error| error.to_string())?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .map_err(|error| error.to_string())?;
    serde_json::from_str(line.trim()).map_err(|error| error.to_string())
}

/// A connection that keeps its lines coming.
struct Following {
    reader: BufReader<UnixStream>,
}

impl Following {
    fn open(socket: &Path, request: Value) -> Following {
        let mut stream = UnixStream::connect(socket).expect("a connection to the daemon");
        writeln!(stream, "{request}").expect("the request goes out");
        Following {
            reader: BufReader::new(stream),
        }
    }

    /// The next line that is not blank.
    fn line(&mut self) -> Value {
        loop {
            let mut line = String::new();
            let read = self
                .reader
                .read_line(&mut line)
                .expect("a line from the daemon");
            assert!(read > 0, "the daemon closed the connection");
            if line.trim().is_empty() {
                continue;
            }
            return serde_json::from_str(line.trim()).expect("the daemon speaks JSON");
        }
    }

    /// Everything up to the line that says the run finished.
    fn until_finished(&mut self) -> Value {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            assert!(Instant::now() < deadline, "the run never finished");
            let value = self.line();
            if value.get("finished").and_then(Value::as_bool) == Some(true) {
                return value;
            }
        }
    }
}

fn submit(run: &Running, session: &str, program: &str, verify: &str, policy: &str) -> Following {
    Following::open(
        &run.socket,
        json!({
            "op": "submit",
            "goal": "make stats.sh exist",
            "root": run.state.join("repo").display().to_string(),
            "state": run.state.display().to_string(),
            "session": session,
            "program": [program],
            "verify": verify,
            "policy": policy,
            "attempts": 1,
        }),
    )
}

#[test]
fn a_daemon_answers_a_ping_lists_nothing_and_can_be_stopped() {
    let state = scratch("lifecycle");
    repo(&state);
    let run = Running::start(&state);

    let answer = ask(&run.socket, json!({"op": "ping"})).expect("a ping answer");
    assert_eq!(answer["ok"], json!(true));
    assert_eq!(answer["pid"], json!(run.pid()));

    let status = ask(&run.socket, json!({"op": "status"})).expect("a status answer");
    assert_eq!(status["ok"], json!(true));
    assert_eq!(
        status["sessions"].as_array().map(Vec::len),
        Some(0),
        "a daemon on a fresh state directory owns nothing: {status}"
    );

    // An op nobody wrote is refused rather than ignored.
    let unknown = ask(&run.socket, json!({"op": "levitate"})).expect("an answer");
    assert_eq!(unknown["ok"], json!(false));
    assert!(
        unknown["error"]
            .as_str()
            .unwrap_or_default()
            .contains("levitate"),
        "{unknown}"
    );

    let pid = run.pid();
    run.stop();
    assert!(
        !state.join("daemon.json").exists(),
        "a clean exit keeps no claim on the state directory"
    );
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "the process is gone too"
    );
}

#[test]
fn a_run_in_flight_is_reported_and_the_log_grows_while_it_runs() {
    let state = scratch("in-flight");
    repo(&state);
    let run = Running::start(&state);

    // Three seconds of sleeping is the window in which the run is a fact about the daemon.
    let mut following = submit(
        &run,
        "a",
        "sleep 3; touch stats.sh",
        "test -f stats.sh",
        "open",
    );
    let ack = following.line();
    assert_eq!(ack["run"], json!(1), "{ack}");
    assert_eq!(ack["session"], json!("a"));

    let status = ask(&run.socket, json!({"op": "status"})).expect("a status answer");
    let session = &status["sessions"][0];
    assert_eq!(session["session"], json!("a"));
    assert_eq!(session["run"], json!(1), "the lease is reported: {status}");
    assert_eq!(
        session["state"],
        json!("running as run 1"),
        "a run that is being run is running, and not interrupted: {status}"
    );

    let log = state.join("sessions/a.jsonl");
    let early = std::fs::metadata(&log).expect("the log exists").len();
    assert!(
        early > 0,
        "entries are already on disk while the run is in flight"
    );

    // A second run on the same session is refused, and the refusal names the holder: the log has no lock of
    // its own, so two writers is not a race that can be won.
    let refused = ask(
        &run.socket,
        json!({
            "op": "submit",
            "goal": "the same session",
            "root": state.join("repo").display().to_string(),
            "state": state.display().to_string(),
            "session": "a",
            "program": ["touch other.sh"],
            "verify": "test -f other.sh",
            "policy": "open",
        }),
    )
    .expect("an answer");
    assert_eq!(refused["ok"], json!(false), "{refused}");
    let why = refused["error"].as_str().unwrap_or_default();
    assert!(why.contains("already running"), "{why}");
    assert!(why.contains("run 1"), "the refusal names the holder: {why}");

    let finished = following.until_finished();
    assert_eq!(finished["code"], json!(0), "{finished}");
    assert_eq!(finished["run"], json!(1));
    assert_eq!(finished["ended"], json!("accepted after 1 attempt(s)"));

    let grew = std::fs::metadata(&log).expect("the log").len();
    assert!(grew > early, "the log grew: {early} then {grew}");

    // And the lease is gone, so the session can be run again.
    let after = ask(&run.socket, json!({"op": "status"})).expect("a status answer");
    assert!(
        after["sessions"][0].get("run").is_none(),
        "a finished run holds nothing: {after}"
    );

    run.stop();
}

/// The test this file exists for.
#[test]
fn a_state_with_an_inflight_run_reports_it_as_interrupted() {
    // A marker is written when a run starts and removed when it ends, so one that is still there belongs to
    // a process that is not running. The pid is deliberately not this process: a marker written by the
    // process reading it is a run in flight, not an interruption.
    let state = scratch("inflight");
    let dir = state.join("inflight");
    std::fs::create_dir_all(&dir).expect("a marker directory");
    std::fs::write(
        dir.join("abc.json"),
        json!({
            "session": "abc",
            "run": 3,
            "goal": "fix the parser",
            "started": 1,
            "pid": 999_999
        })
        .to_string(),
    )
    .expect("a marker");

    let output = Command::new(binary())
        .args(["status", "--state"])
        .arg(&state)
        .env("HADES_HOME", isolated_home())
        .output()
        .expect("status runs");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "{text}");
    assert!(text.contains("interrupted: 1"), "{text}");
    assert!(text.contains("abc"), "{text}");
    assert!(text.contains("fix the parser"), "{text}");
}

#[test]
fn a_kill_in_flight_is_survived_by_a_second_daemon_on_the_same_state_directory() {
    let state = scratch("kill");
    repo(&state);
    let run = Running::start(&state);

    let mut following = submit(
        &run,
        "a",
        // Long enough that a loaded machine cannot finish it between the log appearing and the kill.
        // The child the daemon spawned is orphaned by SIGKILL and exits on its own; the resumed run's
        // program is different, so the orphan cannot satisfy its check.
        "sleep 30; touch stats.sh",
        "test -f stats.sh",
        "open",
    );
    let ack = following.line();
    assert_eq!(ack["run"], json!(1), "{ack}");

    // Let the run get as far as writing the program it is about to try, then take the process away with a
    // signal it cannot handle. Waiting for any byte is not enough: the meta and user entries land before
    // the model is asked, so a loaded machine gets the kill before there is a call to lose.
    let log = state.join("sessions/a.jsonl");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        let called = text.lines().any(|line| {
            serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .and_then(|entry| {
                    entry
                        .get("kind")
                        .and_then(|kind| kind.as_str())
                        .map(str::to_string)
                })
                .is_some_and(|kind| kind == "tool_call")
        });
        if called {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the run never wrote a call to lose"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let entries_before = std::fs::read_to_string(&log)
        .expect("the log")
        .lines()
        .count();
    assert!(entries_before > 0, "there was something to lose");
    drop(following);
    let state = run.kill();

    // A second daemon on the same directory. Nothing is migrated and nothing is loaded at start: it answers
    // out of the files, which is the whole design.
    let second = Running::start(&state);
    let status = ask(&second.socket, json!({"op": "status"})).expect("a status answer");
    assert_eq!(
        status["sessions"].as_array().map(Vec::len),
        Some(1),
        "{status}"
    );

    let session = &status["sessions"][0];
    assert_eq!(session["session"], json!("a"));
    assert!(
        session["entries"].as_u64().unwrap_or(0) >= entries_before as u64,
        "every entry the killed daemon wrote is still there: {status}"
    );
    assert!(
        session["interrupted"].as_u64().is_some(),
        "the last entry is a call with no result: {status}"
    );
    assert_eq!(
        session["state"],
        json!(format!("interrupted at #{}", session["interrupted"])),
        "a run that stopped between a call and its result is reported as interrupted: {}",
        session["state"]
    );

    // And the session goes on. A new run on the same id appends to the same log, and the log it reads back is
    // the one the killed daemon was writing.
    let mut resumed = submit(&second, "a", "touch stats.sh", "test -f stats.sh", "open");
    let ack = resumed.line();
    assert_eq!(
        ack["run"],
        json!(1),
        "the second daemon counts its own runs: {ack}"
    );
    let finished = resumed.until_finished();
    assert_eq!(finished["code"], json!(0), "{finished}");

    let after = std::fs::read_to_string(&log).expect("the log");
    assert!(
        after.lines().count() > entries_before,
        "the resumed run appended to the log the killed one left"
    );
    assert!(after.contains("touch stats.sh"), "{after}");

    second.stop();
}

#[test]
fn a_question_raised_before_a_kill_is_still_pending_after_a_restart() {
    let state = scratch("question");
    repo(&state);
    let run = Running::start(&state);

    // The guarded policy asks before it runs anything the model wrote, so this run ends waiting.
    let mut following = submit(&run, "q", "touch stats.sh", "test -f stats.sh", "guarded");
    following.until_finished();

    let pending = ask(&run.socket, json!({"op": "questions"})).expect("an answer");
    let questions = pending["questions"].as_array().cloned().unwrap_or_default();
    assert_eq!(questions.len(), 1, "one question is waiting: {pending}");
    let id = questions[0]["id"].as_u64().expect("a question id");

    let state = run.kill();

    let second = Running::start(&state);
    let after = ask(&second.socket, json!({"op": "questions"})).expect("an answer");
    assert_eq!(
        after["questions"].as_array().map(Vec::len),
        Some(1),
        "the question is a record on disk, so it outlives the process that asked it: {after}"
    );

    let answered = ask(
        &second.socket,
        json!({"op": "answer", "id": id, "decision": "allow", "scope": "session", "by": "a test"}),
    )
    .expect("an answer");
    assert_eq!(answered["ok"], json!(true), "{answered}");
    assert_eq!(answered["by"], json!("a test"));

    let empty = ask(&second.socket, json!({"op": "questions"})).expect("an answer");
    assert_eq!(
        empty["questions"].as_array().map(Vec::len),
        Some(0),
        "and answering it settles it: {empty}"
    );

    second.stop();
}

#[test]
fn watch_replays_a_session_from_the_start_when_it_is_asked_to() {
    let state = scratch("watch");
    repo(&state);
    let run = Running::start(&state);

    let mut following = submit(&run, "w", "touch stats.sh", "test -f stats.sh", "open");
    following.until_finished();

    let mut watcher = Following::open(
        &run.socket,
        json!({"op": "watch", "session": "w", "from": "start"}),
    );
    let first = watcher.line();
    assert_eq!(
        first["entry"]["kind"],
        json!("meta"),
        "a replay starts where the log starts: {first}"
    );

    let kinds: Vec<String> = (0..5)
        .map(|_| {
            watcher.line()["entry"]["kind"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    assert!(
        kinds.contains(&"tool_call".to_string()),
        "and carries the whole turn: {kinds:?}"
    );

    // Watching a session that is not there is an error rather than a wait that never ends.
    let missing = ask(&run.socket, json!({"op": "watch", "session": "nobody"}));
    assert!(missing.is_err() || missing.unwrap()["ok"] == json!(false));

    drop(watcher);
    run.stop();
}

#[test]
fn a_log_line_that_will_not_read_is_reported_with_its_line_number() {
    let state = scratch("corrupt");
    let root = repo(&state);

    // A real session first, so the only thing wrong with the file is the line this test adds.
    let output = Command::new(binary())
        .env("HADES_HOME", isolated_home())
        .args(["turn", "make stats.sh exist", "--root"])
        .arg(&root)
        .args(["--state"])
        .arg(&state)
        .args([
            "--session",
            "alone",
            "--program",
            "touch stats.sh",
            "--policy",
            "open",
            "--verify",
            "test -f stats.sh",
        ])
        .output()
        .expect("the CLI runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let log = state.join("sessions/alone.jsonl");
    let whole = std::fs::read_to_string(&log).expect("the log");
    let torn = whole.lines().count() + 1;
    // JSON, and not an entry: what a writer appending from a bad state would leave behind.
    let mut damaged = whole.clone();
    damaged.push_str("{\"id\":99,\"parent\":0,\"kind\":\"user\",\"payload\":{}}\n");
    std::fs::write(&log, damaged).expect("a damaged log");

    let run = Running::start(&state);
    let answer = ask(&run.socket, json!({"op": "status"})).expect("an answer");
    assert_eq!(answer["ok"], json!(false), "{answer}");
    let why = answer["error"].as_str().unwrap_or_default();
    assert!(
        why.contains(&format!("line {torn}")),
        "the fault names the line, which is {torn}: {why}"
    );
    assert!(why.contains("alone.jsonl"), "and the file: {why}");

    run.stop();
}

/// The CLI is a client of the same socket, so `hades status` with no daemon is a read of the logs and not a
/// failure: the state is on disk either way.
#[test]
fn status_without_a_daemon_reads_the_logs_and_says_so() {
    let state = scratch("offline");
    let root = repo(&state);
    let output = Command::new(binary())
        .env("HADES_HOME", isolated_home())
        .args(["turn", "make stats.sh exist", "--root"])
        .arg(&root)
        .args(["--state"])
        .arg(&state)
        .args([
            "--session",
            "alone",
            "--program",
            "touch stats.sh",
            "--policy",
            "open",
            "--verify",
            "test -f stats.sh",
        ])
        .output()
        .expect("the CLI runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let status = Command::new(binary())
        .env("HADES_HOME", isolated_home())
        .args(["status", "--state"])
        .arg(&state)
        .output()
        .expect("the CLI runs");
    let text = String::from_utf8_lossy(&status.stdout);
    assert!(status.status.success(), "{text}");
    assert!(
        text.contains("none running"),
        "it says there is no daemon rather than pretending: {text}"
    );
    assert!(
        text.contains("alone"),
        "and still reports the session: {text}"
    );
}

/// A stop is a request: the attempt in flight finishes, the next one is never made, and the ledger names
/// who stopped what in the same shape an allow is recorded.
#[test]
fn a_stop_ends_the_run_before_the_next_attempt_and_the_ledger_names_it() {
    let state = scratch("stop");
    repo(&state);
    let run = Running::start(&state);

    let mut following = Following::open(
        &run.socket,
        json!({
            "op": "submit",
            "goal": "make done.sh exist",
            "root": state.join("repo").display().to_string(),
            "state": state.display().to_string(),
            "session": "s",
            // The first attempt sleeps so a stop can land while it is in flight and fails its check, so
            // the walk reaches the top of a second attempt. The second program would leave a mark.
            "program": ["sleep 2\nfalse", "touch second.sh"],
            "verify": "test -f done.sh",
            "policy": "open",
            "attempts": 5,
        }),
    );
    let ack = following.line();
    assert_eq!(ack["run"], json!(1), "{ack}");

    let log = state.join("sessions/s.jsonl");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        let called = text.lines().any(|line| {
            serde_json::from_str::<Value>(line)
                .ok()
                .and_then(|entry| {
                    entry
                        .get("kind")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .is_some_and(|kind| kind == "tool_call")
        });
        if called {
            break;
        }
        assert!(Instant::now() < deadline, "the run never wrote a call");
        std::thread::sleep(Duration::from_millis(20));
    }

    let answer = ask(
        &run.socket,
        json!({"op": "stop", "session": "s", "by": "a test"}),
    )
    .expect("an answer");
    assert_eq!(answer["ok"], json!(true), "{answer}");
    assert_eq!(answer["count"], json!(1), "{answer}");
    assert_eq!(answer["stopping"][0]["run"], json!(1), "{answer}");
    assert_eq!(answer["stopping"][0]["session"], json!("s"), "{answer}");

    let finished = following.until_finished();
    assert_eq!(
        finished["ended"],
        json!("stopped after 1 attempt(s)"),
        "an attempt after the stop ran, or the ending lied: {finished}"
    );
    assert_eq!(finished["code"], json!(1), "{finished}");
    assert!(
        !state.join("repo/second.sh").exists(),
        "the program of an attempt after the stop ran"
    );

    let ledger = harness::activity::read(&state);
    let stop = ledger.last().expect("a stop line");
    assert_eq!(stop.decision, "stop", "{stop:?}");
    assert_eq!(stop.actor, "a test", "{stop:?}");
    assert_eq!(stop.layer, "surface", "{stop:?}");
    assert!(stop.action.contains("run 1 in s"), "{}", stop.action);

    // Stopping again with nothing in flight is still a decision on the record, and says what it found.
    let quiet = ask(
        &run.socket,
        json!({"op": "stop", "session": "s", "by": "a test"}),
    )
    .expect("an answer");
    assert_eq!(quiet["count"], json!(0), "{quiet}");
    let ledger = harness::activity::read(&state);
    assert_eq!(
        ledger.last().map(|line| line.reason.as_str()),
        Some("nothing was running")
    );

    run.stop();
}

/// `shutdown` is the only verb that ends the daemon, and it asks the runs to stop first.
#[test]
fn a_shutdown_stops_the_run_and_ends_the_daemon() {
    let state = scratch("shutdown");
    repo(&state);
    let run = Running::start(&state);

    let mut following = Following::open(
        &run.socket,
        json!({
            "op": "submit",
            "goal": "make done.sh exist",
            "root": state.join("repo").display().to_string(),
            "state": state.display().to_string(),
            "session": "s",
            "program": ["sleep 2\nfalse", "touch second.sh"],
            "verify": "test -f done.sh",
            "policy": "open",
            "attempts": 5,
        }),
    );
    assert_eq!(following.line()["run"], json!(1));

    let log = state.join("sessions/s.jsonl");
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        if text.contains("\"tool_call\"") {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let answer = ask(&run.socket, json!({"op": "shutdown", "by": "a test"})).expect("an answer");
    assert_eq!(answer["ok"], json!(true), "{answer}");
    assert_eq!(answer["shutdown"], json!(true), "{answer}");
    assert_eq!(answer["stopping"], json!(1), "{answer}");

    let finished = following.until_finished();
    assert_eq!(
        finished["ended"],
        json!("stopped after 1 attempt(s)"),
        "{finished}"
    );

    let ledger = harness::activity::read(&state);
    let line = ledger.last().expect("a shutdown line");
    assert_eq!(line.decision, "stop", "{line:?}");
    assert!(line.action.contains("shutdown: 1 run(s)"), "{line:?}");
    assert_eq!(line.actor, "a test");

    // `stop` waits for the exit and asserts the socket and pid file are gone.
    run.stop();
}

/// A submitted run with a budget that cannot mean anything is refused before a lease or a model call.
#[test]
fn a_submitted_run_refuses_a_zero_token_budget() {
    let state = scratch("budget-zero");
    repo(&state);
    let run = Running::start(&state);

    let answer = ask(
        &run.socket,
        json!({
            "op": "submit",
            "goal": "make never.txt exist",
            "root": state.join("repo").display().to_string(),
            "state": state.display().to_string(),
            "session": "zero",
            "program": ["true"],
            "verify": "test -f never.txt",
            "policy": "open",
            "budget-tokens": 0,
        }),
    )
    .expect("an answer");
    assert_eq!(answer["ok"], json!(false), "{answer}");
    assert!(
        answer["error"]
            .as_str()
            .unwrap_or_default()
            .contains("`--budget-tokens 0`"),
        "{answer}"
    );

    // Nothing was taken on: a refused budget leaves no lease and no log behind.
    let status = ask(&run.socket, json!({"op": "status"})).expect("a status answer");
    assert_eq!(
        status["sessions"].as_array().map(Vec::len),
        Some(0),
        "{status}"
    );
    assert!(!state.join("sessions/zero.jsonl").exists());

    run.stop();
}

/// The CLI sends every named flag as a JSON string, so a daemon that read only numbers would silently
/// fall back to the defaults: three attempts, and a zero budget accepted. Both string forms are asserted.
#[test]
fn a_submitted_budget_reads_the_strings_the_cli_sends() {
    let state = scratch("budget-strings");
    repo(&state);
    let run = Running::start(&state);

    // "0" is the same zero budget as the numeric form, refused before a lease exists.
    let refused = ask(
        &run.socket,
        json!({
            "op": "submit",
            "goal": "make never.txt exist",
            "root": state.join("repo").display().to_string(),
            "state": state.display().to_string(),
            "session": "zero-string",
            "program": ["true"],
            "verify": "test -f never.txt",
            "policy": "open",
            "attempts": "1",
            "minutes": "2",
            "budget-tokens": "0",
        }),
    )
    .expect("an answer");
    assert_eq!(refused["ok"], json!(false), "{refused}");
    assert!(
        refused["error"]
            .as_str()
            .unwrap_or_default()
            .contains("`--budget-tokens 0`"),
        "{refused}"
    );
    assert!(!state.join("sessions/zero-string.jsonl").exists());

    // "1" is one attempt, not the default three: the check never passes, and the ending counts the
    // attempts the string asked for.
    let mut following = Following::open(
        &run.socket,
        json!({
            "op": "submit",
            "goal": "make never.txt exist",
            "root": state.join("repo").display().to_string(),
            "state": state.display().to_string(),
            "session": "one-string",
            "program": ["true"],
            "verify": "test -f never.txt",
            "policy": "open",
            "attempts": "1",
            "minutes": "2",
        }),
    );
    let finished = following.until_finished();
    assert_eq!(finished["code"], json!(1), "{finished}");
    let ended = finished["ended"].as_str().unwrap_or_default();
    assert!(
        ended.starts_with("gave up after 1 attempt(s)"),
        "the string attempts value was not read: {ended}"
    );

    run.stop();
}

#[test]
fn a_strict_submit_is_refused_before_a_session_exists() {
    let state = scratch("strict-model");
    repo(&state);
    let run = Running::start(&state);

    let answer = ask(
        &run.socket,
        json!({
            "op": "submit",
            "goal": "make never.txt exist",
            "root": state.join("repo").display().to_string(),
            "state": state.display().to_string(),
            "session": "strict",
            "model": "strict-ghost",
            "endpoint": "https://ollama.com/v1/chat/completions",
            "key": "env:HADES_DAEMON_STRICT_KEY_NOT_SET",
            "strict-model": true,
        }),
    )
    .expect("an answer");
    assert_eq!(answer["ok"], json!(false), "{answer}");
    let error = answer["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("strict-ghost"),
        "the refusal does not name the model: {answer}"
    );
    assert!(
        error.contains("ollama-cloud"),
        "the refusal does not name the provider: {answer}"
    );

    // A strict request that cannot reach its model leaves nothing behind: no session, no lease.
    let status = ask(&run.socket, json!({"op": "status"})).expect("a status answer");
    assert_eq!(
        status["sessions"].as_array().map(Vec::len),
        Some(0),
        "{status}"
    );
    assert!(!state.join("sessions/strict.jsonl").exists());

    run.stop();
}

/// A scratch base inside this crate's own `target/`, never the machine's temp directory.
///
/// The agent that runs these tests keeps its temp under the live install, and Hades refuses to write there:
/// that refusal is the boundary doing its job, so the tests must not ask it to make an exception.
fn scratch_base() -> std::path::PathBuf {
    // Created and then canonicalized: a path with `..` in it compares unequal to the same directory reached
    // by the store, and the snapshot's never-touch test is a prefix test. The first run of this helper put the
    // store inside the tree it was copying, and the copy recursed into itself until the kernel refused the
    // file name.
    let base = harness::scratch::root();
    std::fs::create_dir_all(&base).ok();
    std::fs::canonicalize(&base).unwrap_or(base)
}

/// A Hades home for the children, so a config on this machine cannot change what a spawned run does.
fn isolated_home() -> PathBuf {
    let dir = scratch_base().join(format!("hades-daemon-home-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("an isolated Hades home");
    dir
}
