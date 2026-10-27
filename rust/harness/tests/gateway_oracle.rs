//! The gateway without Telegram: scripted updates in, a scripted daemon socket out.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use harness::gateway::{Chat, 
    Button, Callback, Settings, Signals, Transport, Update, handle, handle_with, load_offset,
    serve, settings,
};
use serde_json::json;

#[derive(Default)]
struct Recorded {
    sent: Vec<(Chat, String, Vec<Button>)>,
    edits: Vec<(Chat, i64, String)>,
    typings: Vec<Chat>,
    acks: Vec<(String, String)>,
    batches: Vec<Vec<Update>>,
    taken: usize,
}

#[derive(Clone, Default)]
struct Fake {
    state: Arc<Mutex<Recorded>>,
}

impl Fake {
    fn with(batches: Vec<Vec<Update>>) -> Fake {
        let fake = Fake::default();
        fake.state.lock().unwrap().batches = batches;
        fake
    }

    fn sent(&self) -> Vec<(Chat, String, Vec<Button>)> {
        self.state.lock().unwrap().sent.clone()
    }

    fn acks(&self) -> Vec<(String, String)> {
        self.state.lock().unwrap().acks.clone()
    }

    /// Everything the chat saw, in send order and then edit order. The live status is edited, so a
    /// check that only looks at `sent` sees the first line and misses the run.
    fn texts(&self) -> Vec<String> {
        let state = self.state.lock().unwrap();
        let mut all: Vec<String> = state.sent.iter().map(|(_, text, _)| text.clone()).collect();
        all.extend(state.edits.iter().map(|(_, _, text)| text.clone()));
        all
    }

    fn typings(&self) -> usize {
        self.state.lock().unwrap().typings.len()
    }
}

impl Transport for Fake {
    // The oracle stands in for a surface that can ask for approval, as Telegram does.
    fn buttons(&self) -> bool {
        true
    }

    fn updates(&mut self, _offset: Option<i64>, _timeout: u64) -> Result<Vec<Update>, String> {
        let mut state = self.state.lock().unwrap();
        if state.taken >= state.batches.len() {
            return Ok(Vec::new());
        }
        let batch = state.batches[state.taken].clone();
        state.taken += 1;
        Ok(batch)
    }

    fn send(&mut self, chat: &Chat, text: &str, buttons: &[Button]) -> Result<Option<i64>, String> {
        self.state
            .lock()
            .unwrap()
            .sent
            .push((chat.clone(), text.to_string(), buttons.to_vec()));
        Ok(Some(1))
    }

    fn edit(
        &mut self,
        chat: &Chat,
        message: i64,
        text: &str,
        _buttons: &[Button],
    ) -> Result<(), String> {
        self.state
            .lock()
            .unwrap()
            .edits
            .push((chat.clone(), message, text.to_string()));
        Ok(())
    }

    fn typing(&mut self, chat: &Chat) -> Result<(), String> {
        self.state.lock().unwrap().typings.push(chat.clone());
        Ok(())
    }

    fn ack(&mut self, callback: &str, text: &str) -> Result<(), String> {
        self.state
            .lock()
            .unwrap()
            .acks
            .push((callback.to_string(), text.to_string()));
        Ok(())
    }
}

fn scratch(name: &str) -> PathBuf {
    let base = harness::scratch::root().join(format!("gateway-{name}"));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("scratch");
    base.canonicalize().expect("canonical")
}

fn socket_under(dir: &Path) -> PathBuf {
    dir.join("d.sock")
}

fn message(from: i64, chat: i64, text: &str) -> Update {
    Update {
        id: 1,
        from: from.to_string(),
        chat: Chat::telegram(chat),
        text: text.to_string(),
        callback: None,
        photo: None,
    }
}

fn config_with(dir: &Path, allowed: &[i64], policy: &str) -> PathBuf {
    let path = dir.join("config.json");
    std::fs::write(
        &path,
        json!({
            "version": 1,
            "provider": "llama.cpp",
            "model": "deepseek-v4.1-flash",
            "root": dir.join("work").display().to_string(),
            "vault": dir.join("mneme").display().to_string(),
            "gateway": {"telegram": {"allowed_users": allowed, "policy": policy,
                                    "verify": "test -f out.txt"}},
        })
        .to_string(),
    )
    .expect("config");
    path
}

#[test]
fn a_missing_or_empty_allowlist_is_a_refusal() {
    let dir = scratch("settings");
    let missing = dir.join("missing.json");
    std::fs::write(
        &missing,
        json!({"version": 1, "provider": "p", "model": "m", "root": "r", "vault": "v"}).to_string(),
    )
    .expect("config");
    let error = settings(&missing).expect_err("no gateway section");
    assert!(error.contains("gateway.telegram"), "{error}");

    let empty = config_with(&dir, &[], "guarded");
    let error = settings(&empty).expect_err("an empty list is not everyone");
    assert!(error.contains("allowed_users"), "{error}");

    let no_check = dir.join("no-check.json");
    std::fs::write(
        &no_check,
        json!({"version": 1, "provider": "p", "model": "m", "root": "r", "vault": "v",
               "gateway": {"telegram": {"allowed_users": [7], "verify": " "}}})
        .to_string(),
    )
    .expect("config");
    // A check is only needed where work runs. The surface loads without one, and `/run` is where a
    // channel that does work is told it has nothing to be judged by.
    let loaded = settings(&no_check).expect("a surface may start without a check");
    assert!(loaded.verify.trim().is_empty());

    let good = config_with(&dir, &[7], "open");
    let read = settings(&good).expect("settings");
    assert_eq!(
        read,
        Settings {
            allowed: vec!["7".to_string()],
            policy: "open".to_string(),
            verify: "test -f out.txt".to_string(),
            model: "deepseek-v4.1-flash".to_string(),
            root: dir.join("work").display().to_string(),
            vault: dir.join("mneme").display().to_string(),
            heartbeat: harness::gateway::goal::DEFAULT_HEARTBEAT_SECS,
            mcp: harness::mcp::McpConfig::default(),
        mode: harness::mode::Mode::Act,
        channels: std::collections::HashMap::new(),
            prompt: None,
            loading: None,
            provider: None,
            endpoints: Default::default(),
        providers: Default::default(),
        }
    );
}

#[test]
fn an_unauthorized_sender_gets_nothing() {
    let dir = scratch("auth");
    let settings = Settings {
        allowed: vec!["1".to_string()],
        policy: "guarded".to_string(),
        verify: "test -f out.txt".to_string(),
        model: "deepseek-v4.1-flash".to_string(),
        root: "/tmp".to_string(),
        vault: "/tmp/v".to_string(),
        heartbeat: harness::gateway::goal::DEFAULT_HEARTBEAT_SECS,
        mcp: harness::mcp::McpConfig::default(),
        mode: harness::mode::Mode::Act,
        channels: std::collections::HashMap::new(),
        prompt: None,
        loading: None,
        provider: None,
        endpoints: Default::default(),
        providers: Default::default(),
    };
    let mut fake = Fake::default();
    // The socket does not exist: any attempt to reach the daemon would fail, which is the proof that
    // nothing tried. The update is from 2, not 1.
    handle(
        &message(2, 2, "/run do something"),
        &settings,
        &socket_under(&dir),
        &dir,
        &mut fake,
    )
    .expect("ignored");
    assert!(fake.sent().is_empty());
}

#[test]
fn a_channel_set_to_chat_cannot_start_work() {
    let dir = scratch("readonly");
    let mut settings = Settings {
        allowed: vec!["1".to_string()],
        policy: "open".to_string(),
        verify: "true".to_string(),
        model: "deepseek-v4.1-flash".to_string(),
        root: "/tmp".to_string(),
        vault: "/tmp/v".to_string(),
        heartbeat: harness::gateway::goal::DEFAULT_HEARTBEAT_SECS,
        mcp: harness::mcp::McpConfig::default(),
        mode: harness::mode::Mode::Act,
        channels: std::collections::HashMap::new(),
        prompt: None,
        loading: None,
        provider: None,
        endpoints: Default::default(),
        providers: Default::default(),
    };
    settings.mode = harness::mode::Mode::Read;
    let mut fake = Fake::default();
    // No daemon: if the gate let this through, the submit would fail instead of answering.
    handle(
        &message(1, 1, "/run make a file"),
        &settings,
        &socket_under(&dir),
        &dir,
        &mut fake,
    )
    .expect("refused");
    let sent = fake.sent();
    let text = &sent[0].1;
    assert!(text.contains("read-only"), "{text:?}");
    assert!(text.contains("work"), "{text:?}");
}

#[test]
fn a_submit_is_relayed_from_started_to_the_end() {
    let dir = scratch("relay");
    let socket = socket_under(&dir);
    let listener = UnixListener::bind(&socket).expect("bind");
    let daemon = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("one connection");
        let mut line = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut line)
            .expect("the request");
        let request: serde_json::Value = serde_json::from_str(line.trim()).expect("json");
        assert_eq!(request["op"], "submit");
        assert_eq!(request["policy"], "open");
        writeln!(
            stream,
            "{}",
            json!({"run": 3, "session": "walk", "started": true})
        )
        .expect("started");
        writeln!(
            stream,
            "{}",
            json!({"entry": {"id": 2, "kind": "tool_call", "payload": {}}})
        )
        .expect("entry");
        writeln!(
            stream,
            "{}",
            json!({"progress": "reasoning", "delta": "the script should be there"})
        )
        .expect("progress");
        writeln!(
            stream,
            "{}",
            json!({"finished": true, "ended": "accepted after 1 attempt(s)", "code": 0,
                   "recorded": "/v/20 Research/note.md"})
        )
        .expect("finished");
    });

    let settings = Settings {
        allowed: vec!["1".to_string()],
        policy: "open".to_string(),
        verify: "test -f out.txt".to_string(),
        model: "deepseek-v4.1-flash".to_string(),
        root: "/tmp".to_string(),
        vault: "/tmp/v".to_string(),
        heartbeat: harness::gateway::goal::DEFAULT_HEARTBEAT_SECS,
        mcp: harness::mcp::McpConfig::default(),
        mode: harness::mode::Mode::Act,
        channels: std::collections::HashMap::new(),
        prompt: None,
        loading: None,
        provider: None,
        endpoints: Default::default(),
        providers: Default::default(),
    };
    let mut fake = Fake::default();
    handle(
        &message(1, 1, "/run make the stats file"),
        &settings,
        &socket,
        &dir,
        &mut fake,
    )
    .expect("relayed");
    daemon.join().expect("daemon");

    let texts = fake.texts();
    let last = texts
        .last()
        .unwrap_or_else(|| panic!("no message in {texts:?}"));
    assert!(last.contains("run 3"), "{texts:?}");
    assert!(last.contains("after 1 attempt(s)"), "{texts:?}");
    assert!(
        last.contains("Recorded:") && last.contains("20 Research/note.md"),
        "{texts:?}"
    );
    assert_eq!(fake.sent()[0].1, "<b>Hades</b> · working");
    assert!(
        last.contains("the script should be there") && last.contains("Thinking"),
        "the live reasoning never reached the card: {texts:?}"
    );
    assert!(
        fake.typings() >= 1,
        "the chat was never told the run was alive"
    );
}

#[test]
fn a_question_is_a_button_and_an_allow_continues_the_same_goal() {
    let dir = scratch("approval");
    let socket = socket_under(&dir);
    let listener = UnixListener::bind(&socket).expect("bind");

    let daemon = std::thread::spawn(move || {
        // 1. The run starts, asks, and ends `awaiting`: a question outlives the run that asked it.
        let (mut run, _) = listener.accept().expect("the run connection");
        let mut line = String::new();
        BufReader::new(run.try_clone().unwrap())
            .read_line(&mut line)
            .expect("the request");
        let request: serde_json::Value = serde_json::from_str(line.trim()).expect("json");
        assert_eq!(request["op"], "submit");
        assert_eq!(request["session"], "tg-1");
        writeln!(
            run,
            "{}",
            json!({"run": 4, "session": "tg-1", "started": true})
        )
        .expect("started");
        writeln!(
            run,
            "{}",
            json!({"entry": {"id": 2, "kind": "approval_request",
                             "payload": {"id": 42, "action": "shell", "reason": "the policy asks"}}})
        )
        .expect("question");
        writeln!(
            run,
            "{}",
            json!({"finished": true, "ended": "awaiting", "code": 0})
        )
        .expect("awaiting");
        drop(run);

        // 2. The answer arrives on its own connection, session-scoped so it outlives the run.
        let (mut answer, _) = listener.accept().expect("the answer connection");
        let mut line = String::new();
        BufReader::new(answer.try_clone().unwrap())
            .read_line(&mut line)
            .expect("the answer");
        let request: serde_json::Value = serde_json::from_str(line.trim()).expect("json");
        assert_eq!(request["op"], "answer");
        assert_eq!(request["id"], 42);
        assert_eq!(request["decision"], "allow");
        assert_eq!(request["scope"], "session");
        assert_eq!(request["by"], "telegram:1");
        writeln!(answer, "{}", json!({"ok": true})).expect("ok");
        drop(answer);

        // 3. Allow resumes the same goal in the same session, and the standing answer lets it through.
        let (mut resumed, _) = listener.accept().expect("the resumed run");
        let mut line = String::new();
        BufReader::new(resumed.try_clone().unwrap())
            .read_line(&mut line)
            .expect("the resume");
        let request: serde_json::Value = serde_json::from_str(line.trim()).expect("json");
        assert_eq!(request["op"], "submit");
        assert_eq!(request["goal"], "make the stats file");
        assert_eq!(request["session"], "tg-1");
        writeln!(
            resumed,
            "{}",
            json!({"finished": true, "ended": "accepted after 1 attempt(s)", "code": 0})
        )
        .expect("finished");
    });

    let settings = Settings {
        allowed: vec!["1".to_string()],
        policy: "guarded".to_string(),
        verify: "test -f out.txt".to_string(),
        model: "deepseek-v4.1-flash".to_string(),
        root: "/tmp".to_string(),
        vault: "/tmp/v".to_string(),
        heartbeat: harness::gateway::goal::DEFAULT_HEARTBEAT_SECS,
        mcp: harness::mcp::McpConfig::default(),
        mode: harness::mode::Mode::Act,
        channels: std::collections::HashMap::new(),
        prompt: None,
        loading: None,
        provider: None,
        endpoints: Default::default(),
        providers: Default::default(),
    };
    let fake = Fake::default();
    let mut worker = fake.clone();
    handle(
        &message(1, 1, "/run make the stats file"),
        &settings,
        &socket,
        &dir,
        &mut worker,
    )
    .expect("first run");
    let sent = fake.sent();
    let question = sent
        .iter()
        .find(|(_, _text, buttons)| !buttons.is_empty())
        .unwrap_or_else(|| panic!("no question in {sent:?}"));
    assert_eq!(question.2[0].data, "a:42:1");
    let texts = fake.texts();
    assert!(
        texts
            .iter()
            .any(|text| text.contains("Waiting for your decision")),
        "{texts:?}"
    );

    let mut poller = fake.clone();
    handle(
        &Update {
            id: 2,
            from: 1.to_string(),
            chat: Chat::telegram(1),
            text: String::new(),
            callback: Some(Callback {
                id: "cb-1".to_string(),
                data: "a:42:1".to_string(),
            }),
            photo: None,
        },
        &settings,
        &socket,
        &dir,
        &mut poller,
    )
    .expect("the answer is sent and the run continues");
    assert_eq!(
        poller.acks(),
        vec![("cb-1".to_string(), "allowed, continuing".to_string())]
    );

    daemon.join().expect("daemon");
    let texts = fake.texts();
    assert!(
        texts.iter().any(|text| text.contains("after 1 attempt(s)")),
        "{texts:?}"
    );
}

#[test]
fn the_offset_advances_after_a_batch() {
    let dir = scratch("offset");
    let settings = Settings {
        allowed: vec!["1".to_string()],
        policy: "guarded".to_string(),
        verify: "test -f out.txt".to_string(),
        model: "deepseek-v4.1-flash".to_string(),
        root: "/tmp".to_string(),
        vault: "/tmp/v".to_string(),
        heartbeat: harness::gateway::goal::DEFAULT_HEARTBEAT_SECS,
        mcp: harness::mcp::McpConfig::default(),
        mode: harness::mode::Mode::Act,
        channels: std::collections::HashMap::new(),
        prompt: None,
        loading: None,
        provider: None,
        endpoints: Default::default(),
        providers: Default::default(),
    };
    // One batch with one update, then nothing. `--once` handles the batch and returns.
    let fake = Fake::with(vec![vec![Update {
        id: 41,
        from: 1.to_string(),
        chat: Chat::telegram(1),
        text: "/help".to_string(),
        callback: None,
        photo: None,
    }]]);
    serve(
        fake.clone(),
        socket_under(&dir),
        dir.clone(),
        settings.clone(),
        true,
    )
    .expect("one batch");
    assert_eq!(load_offset(&dir), Some(42));
    assert!(
        fake.sent()[0].1.contains("Send a message"),
        "{:?}",
        fake.sent()
    );

    // A second run starts at the recorded offset and sends nothing new.
    let quiet = Fake::default();
    serve(
        quiet.clone(),
        socket_under(&dir),
        dir.clone(),
        settings,
        true,
    )
    .expect("quiet");
    assert!(quiet.sent().is_empty());
    assert_eq!(load_offset(&dir), Some(42));
}

/// The stop verbs on the surface. `/stop` is scoped to this chat, `/cancel` is a file verb, and `/shutdown`
/// is the one that also ends the poller.
fn settings_for(chat: i64) -> Settings {
    Settings {
        allowed: vec![chat.to_string()],
        policy: "guarded".to_string(),
        verify: "true".to_string(),
        model: "none".to_string(),
        root: "/work".to_string(),
        vault: "/work/mneme".to_string(),
        heartbeat: harness::gateway::goal::DEFAULT_HEARTBEAT_SECS,
        mcp: harness::mcp::McpConfig::default(),
        mode: harness::mode::Mode::Act,
        channels: std::collections::HashMap::new(),
        prompt: None,
        loading: None,
        provider: None,
        endpoints: Default::default(),
        providers: Default::default(),
    }
}

#[test]
fn a_stop_is_scoped_to_this_chat_and_names_what_it_stopped() {
    let dir = scratch("stop-command");
    let socket = socket_under(&dir);
    let listener = UnixListener::bind(&socket).expect("bind");
    let daemon = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("one connection");
        let mut line = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut line)
            .expect("the request");
        let request: serde_json::Value = serde_json::from_str(line.trim()).expect("json");
        assert_eq!(request["op"], "stop", "{request}");
        assert_eq!(
            request["session"], "tg-7",
            "a chat stop must not stop every chat: {request}"
        );
        assert_eq!(request["by"], "telegram:7", "{request}");
        writeln!(
            stream,
            "{}",
            json!({"ok": true, "count": 1, "stopping": [{"session": "tg-7", "run": 3}]})
        )
        .expect("answer");
    });

    let mut fake = Fake::default();
    handle(
        &message(7, 7, "/stop"),
        &settings_for(7),
        &socket,
        &dir,
        &mut fake,
    )
    .expect("stopped");
    daemon.join().expect("daemon");

    let text = &fake.sent()[0].1;
    assert!(text.contains("Stopping"), "{text}");
    assert!(text.contains("run 3 in tg-7"), "{text}");
    assert!(
        text.contains("not undone"),
        "a stop that implies undo is lying: {text}"
    );
}

#[test]
fn a_cancel_removes_the_job_its_pending_retry_and_records_who_asked() {
    let dir = scratch("cancel-command");
    std::fs::create_dir_all(dir.join("jobs")).expect("jobs");
    std::fs::create_dir_all(dir.join("retry")).expect("retry");
    std::fs::write(
        dir.join("jobs/4-digest.md"),
        "---\nid: 4\nchat: 7\nschedule: every 6h\nmode: read\n---\n\nWrite the digest.\n",
    )
    .expect("a job file");
    std::fs::write(
        dir.join("retry/4.json"),
        r#"{"window":"4@1760000000","attempt":1,"at":1760000100,"error":"timed out"}"#,
    )
    .expect("a pending retry");

    let mut fake = Fake::default();
    // No socket: cancelling is a file verb and must not need a daemon to be up.
    handle(
        &message(7, 7, "/cancel 4"),
        &settings_for(7),
        &socket_under(&dir),
        &dir,
        &mut fake,
    )
    .expect("cancelled");

    assert!(
        !dir.join("jobs/4-digest.md").exists(),
        "the job is still there, so it is still due"
    );
    assert!(
        !dir.join("retry/4.json").exists(),
        "a pending retry outlived the job it belongs to"
    );
    let text = &fake.sent()[0].1;
    assert!(text.contains("Cancelled job 4"), "{text}");
    assert!(text.contains("not undone"), "{text}");

    let ledger = harness::activity::read(&dir);
    let line = ledger.last().expect("a cancel line");
    assert_eq!(line.decision, "stop", "{line:?}");
    assert_eq!(line.actor, "telegram:7", "{line:?}");
    assert!(line.action.contains("cancel: job 4"), "{line:?}");
    assert_eq!(line.artifact, "jobs/4-digest.md", "{line:?}");
}

#[test]
fn a_shutdown_stops_every_run_and_ends_the_poller() {
    let dir = scratch("shutdown-command");
    let socket = socket_under(&dir);
    let listener = UnixListener::bind(&socket).expect("bind");
    let daemon = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("one connection");
        let mut line = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut line)
            .expect("the request");
        let request: serde_json::Value = serde_json::from_str(line.trim()).expect("json");
        assert_eq!(request["op"], "shutdown", "{request}");
        assert_eq!(request["by"], "telegram:7", "{request}");
        writeln!(
            stream,
            "{}",
            json!({"ok": true, "shutdown": true, "stopping": 2})
        )
        .expect("answer");
    });

    let signals = Signals::default();
    let mut fake = Fake::default();
    let mcp = std::sync::Arc::new(harness::mcp::Mcp::empty());
    handle_with(
        &message(7, 7, "/shutdown"),
        &settings_for(7),
        &socket,
        &dir,
        &mut fake,
        &signals,
        &mcp,
    )
    .expect("shutdown");
    daemon.join().expect("daemon");

    assert!(
        signals.stopped(),
        "the daemon exited but the poller kept asking it for work"
    );
    let text = &fake.sent()[0].1;
    assert!(text.contains("Stopping everything"), "{text}");
    assert!(text.contains("2 run(s)"), "{text}");
}
