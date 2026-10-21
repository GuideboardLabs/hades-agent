//! The daemon: one process that owns the sessions under a state directory.
//!
//! Why it exists, in one line: the seams a run holds are expensive to build, a run is long, and a run started
//! from a terminal dies with the terminal. The daemon holds them, and everything it knows is on disk.
//!
//! **It owns no state of its own.** On start there is nothing to load: a `status` reads the session logs, a
//! `questions` reads the question record, a `submit` opens the session it was told to open. A restart is
//! therefore a re-read rather than a migration, and there is no in-memory copy of anything that could go
//! stale or be lost. This is what makes the H3 proof a real `kill -9`: a second daemon on the same state
//! directory finds everything the first one had, because the first one was never holding any of it.
//!
//! **The limit, stated rather than implied.** The session store has no lock of its own: `Session::append` is
//! one `writeln!` to an append-mode file and nothing arbitrates between two writers. So the daemon is the
//! single writer for the sessions it owns, and it enforces that among *runs it is asked to make* with a
//! per-session lease. A second process appending to the same log is not detected, and this module does not
//! claim otherwise.
//!
//! **Stopping is a request, not an undo.** `stop` marks the run's flag and answers at once; the walk honors
//! it at its next boundary (before another attempt, or after a model call but before the program runs). A
//! model call or a shell command already in flight finishes first, so a stop can be preceded by the very
//! side effect it was meant to prevent. `shutdown` is the separate verb that stops every run and ends the
//! daemon; `stop` never ends the process and never deletes a job.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use session_tree::{Clock, FileStore, Kind, Session, SessionId, SystemClock};

use crate::approval::{ApprovalBroker, OnDisk, PendingId, Scope};
use crate::fault::Fault;
use crate::graph::{Bounds, Ended};
use crate::policy::Decision;

/// How long a reader waits between looks at a file it is following, and how long the accept loop waits
/// between polls. Small enough that a person watching sees an entry arrive as it is written, large enough
/// that a daemon with no clients is doing nothing.
const TICK: Duration = Duration::from_millis(50);

/// A run that holds a session.
#[derive(Debug, Clone)]
struct Lease {
    run: u64,
    since: String,
}

/// What the daemon was told on the way up.
pub struct Daemon {
    state: PathBuf,
    socket: PathBuf,
    started: Instant,
    runs: Arc<Mutex<BTreeMap<String, Lease>>>,
    /// The stop flag for the run holding each session. One flag per run, because stopping session `a` must
    /// not stop a run in session `b`.
    cancels: Arc<Mutex<BTreeMap<String, Arc<AtomicBool>>>>,
    next_run: Arc<AtomicU64>,
    stopping: Arc<AtomicBool>,
}

/// Where a running session leaves a marker. A marker that survives the process is the only evidence a
/// crash leaves: the log shows the last step, and this shows that the run never reached its end.
pub fn inflight_dir(state: &Path) -> PathBuf {
    state.join("inflight")
}

fn inflight_path(state: &Path, session: &str) -> PathBuf {
    // Session ids are already filename-safe, but a marker path is built from a string this function did not
    // validate, so anything else is folded to `-` rather than being trusted as a path component.
    let safe: String = session
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' { ch } else { '-' })
        .collect();
    inflight_dir(state).join(format!("{safe}.json"))
}

/// Record that this session is running. Best effort by the same contract as the ledger: a marker that
/// cannot be written must not stop the run.
fn mark_inflight(state: &Path, session: &str, run: u64, goal: &str) {
    if let Err(error) = crate::scratch::state_dir(&inflight_dir(state)) {
        eprintln!("inflight: could not make the marker directory: {error}");
        return;
    }
    let value = json!({
        "session": session,
        "run": run,
        "goal": goal.chars().take(160).collect::<String>(),
        "started": SystemClock.now(),
        "pid": std::process::id(),
    });
    if let Err(error) = fs::write(inflight_path(state, session), format!("{value}\n")) {
        eprintln!("inflight: could not write the marker for {session}: {error}");
    }
}

/// The run reached its end, so the marker goes. A marker that will not delete is reported, not swallowed:
/// it will be read as an interruption next time.
fn clear_inflight(state: &Path, session: &str) {
    let path = inflight_path(state, session);
    if path.exists()
        && let Err(error) = fs::remove_file(&path)
    {
        eprintln!("inflight: could not remove {}: {error}", path.display());
    }
}

/// Every marker left behind. A marker whose process is still the current one is still running; a marker
/// from another pid belongs to a process that ended without reaching its end, which is what "interrupted"
/// means here. No replay is attempted: the report names the session and the last step, and a person
/// decides.
pub fn interrupted(state: &Path) -> Vec<Value> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(inflight_dir(state)) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if value.get("pid").and_then(Value::as_u64) == Some(std::process::id() as u64) {
            continue;
        }
        found.push(value);
    }
    found.sort_by_key(|value| value.get("started").and_then(Value::as_i64).unwrap_or(0));
    found
}

impl Daemon {
    /// Bind the socket and announce itself. Nothing is read here except the state directory's existence:
    /// a daemon that fails on start because some session file is malformed would be a daemon that cannot be
    /// asked to report it.
    pub fn bind(state: impl Into<PathBuf>, socket: impl Into<PathBuf>) -> Result<Daemon, Fault> {
        let state = state.into();
        let socket = socket.into();
        // Private before anything is written into it: the session transcript is the most sensitive file
        // the harness keeps, and `create_dir_all` would take its mode from the caller's umask.
        crate::scratch::state_dir(&state).map_err(|error| {
            Fault::harness(format!("could not protect {}: {error}", state.display()))
        })?;
        fs::create_dir_all(state.join("sessions")).map_err(|error| {
            Fault::harness(format!(
                "could not make {}: {error}",
                state.join("sessions").display()
            ))
        })?;
        if let Some(parent) = socket.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                Fault::harness(format!("could not make {}: {error}", parent.display()))
            })?;
        }
        // A socket file left behind by a dead daemon would make `bind` fail with "address in use" forever.
        // The pid file is the only thing that distinguishes a live one from a dead one, so it is checked
        // before anything is removed.
        if socket.exists() && live(&state).is_none() {
            let _ = fs::remove_file(&socket);
        }

        Ok(Daemon {
            state,
            socket,
            started: Instant::now(),
            runs: Arc::new(Mutex::new(BTreeMap::new())),
            cancels: Arc::new(Mutex::new(BTreeMap::new())),
            next_run: Arc::new(AtomicU64::new(0)),
            stopping: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    fn pid_file(&self) -> PathBuf {
        self.state.join("daemon.json")
    }

    /// Where a daemon's socket lives by default, so a client and a server agree without being told.
    pub fn socket_path(state: &Path) -> PathBuf {
        state.join("daemon.sock")
    }

    /// Run until asked to stop. One thread per connection, and a connection that is following a log holds
    /// its own thread for as long as it is watching.
    pub fn serve(self) -> Result<(), Fault> {
        let listener = UnixListener::bind(&self.socket).map_err(|error| {
            Fault::harness(format!(
                "could not bind {}: {error}{}",
                self.socket.display(),
                if live(&self.state).is_some() {
                    " (another daemon is running under this state directory)"
                } else {
                    ""
                }
            ))
        })?;
        listener.set_nonblocking(true).map_err(|error| {
            Fault::harness(format!(
                "could not set {} non-blocking: {error}",
                self.socket.display()
            ))
        })?;

        let announced = json!({
            "pid": std::process::id(),
            "socket": self.socket.display().to_string(),
            "state": self.state.display().to_string(),
            "started": SystemClock.now(),
        });
        fs::write(self.pid_file(), format!("{announced}\n")).map_err(|error| {
            Fault::harness(format!(
                "could not write {}: {error}",
                self.pid_file().display()
            ))
        })?;

        let (socket, pid_file) = (self.socket.clone(), self.pid_file());
        let shared = Arc::new(self);
        let mut workers = Vec::new();
        while !shared.stopping.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let daemon = Arc::clone(&shared);
                    workers.push(thread::spawn(move || {
                        let _ = daemon.handle(stream);
                    }));
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => thread::sleep(TICK),
                Err(error) => {
                    return Err(Fault::harness(format!(
                        "could not accept on {}: {error}",
                        socket.display()
                    )));
                }
            }
        }

        // A clean exit leaves nothing behind that a client could mistake for a running daemon.
        let _ = fs::remove_file(&socket);
        let _ = fs::remove_file(&pid_file);
        for worker in workers {
            let _ = worker.join();
        }
        Ok(())
    }

    /// One connection: read one request, do it, answer. The two ops that keep writing (`submit` and `watch`)
    /// write their own lines and answer with nothing.
    fn handle(self: Arc<Self>, mut stream: UnixStream) -> Result<(), Fault> {
        let mut line = String::new();
        let mut reader =
            BufReader::new(stream.try_clone().map_err(|error| {
                Fault::harness(format!("could not clone the connection: {error}"))
            })?);
        if reader
            .read_line(&mut line)
            .map_err(|error| Fault::harness(format!("could not read the request: {error}")))?
            == 0
        {
            return Ok(());
        }

        let request: Value = match serde_json::from_str(line.trim()) {
            Ok(request) => request,
            Err(error) => {
                writeln!(
                    stream,
                    "{}",
                    json!({"ok": false, "error": format!("that is not a request: {error}")})
                )
                .map_err(send_failed)?;
                return Ok(());
            }
        };

        match self.request(&request, &mut stream) {
            Ok(Value::Null) => Ok(()),
            Ok(response) => writeln!(stream, "{response}").map_err(send_failed),
            Err(fault) => writeln!(stream, "{}", json!({"ok": false, "error": fault.detail()}))
                .map_err(send_failed),
        }
    }

    fn request(&self, request: &Value, stream: &mut UnixStream) -> Result<Value, Fault> {
        match text(request, "op").as_str() {
            "ping" => Ok(json!({
                "ok": true,
                "pid": std::process::id(),
                "uptime_ms": self.started.elapsed().as_millis() as u64,
            })),
            "status" => self.status(),
            "questions" => self.questions(),
            "answer" => self.answer(request),
            "submit" => self.submit(request, stream),
            "watch" => {
                self.watch(request, stream)?;
                Ok(Value::Null)
            }
            "stop" => self.stop(request),
            "shutdown" => self.shutdown(request),
            other => Ok(json!({"ok": false, "error": format!("there is no op called `{other}`")})),
        }
    }

/// Every session under this state directory, as the logs have it. Read fresh every time, because the
/// logs are the only place this exists.
    fn status(&self) -> Result<Value, Fault> {
        let waiting = self.questions_raw()?;
        let runs = self.runs.lock().map_err(|_| {
            Fault::harness("the lease table was poisoned by a panic in another thread")
        })?;

        let mut reported = sessions_on_disk(&self.state)?;
        for entry in reported.iter_mut() {
            let Some(object) = entry.as_object_mut() else {
                continue;
            };
            let Some(id) = object
                .get("session")
                .and_then(Value::as_str)
                .map(str::to_string)
            else {
                continue;
            };
            if let Some(lease) = runs.get(&id) {
                object.insert("run".to_string(), json!(lease.run));
                object.insert("since".to_string(), json!(lease.since));
                // A lease outranks the derived state. "Interrupted" means a call with no result *and nobody
                // running it*; a session whose program is mid-flight is running, and calling that interrupted
                // would send a person looking for a corpse that is still standing.
                object.insert(
                    "state".to_string(),
                    json!(format!("running as run {}", lease.run)),
                );
            }
            object.insert(
                "waiting".to_string(),
                json!(waiting.iter().filter(|question| question.0 == id).count()),
            );
        }

        Ok(json!({
            "ok": true,
            "pid": std::process::id(),
            "uptime_ms": self.started.elapsed().as_millis() as u64,
            "socket": self.socket.display().to_string(),
            "state": self.state.display().to_string(),
            // Markers from a process that is not this one: a run that started and never reached its end.
            "interrupted": interrupted(&self.state),
            "sessions": reported,
        }))
    }

    /// The pending questions, the same record `hades approvals` reads.
    fn questions(&self) -> Result<Value, Fault> {
        let waiting = self.questions_raw()?;
        Ok(json!({
            "ok": true,
            "questions": waiting
                .iter()
                .map(|(session, id, action)| json!({"id": id, "session": session, "action": action}))
                .collect::<Vec<_>>(),
        }))
    }

    /// `(session, id, action)` for every question nobody has answered yet.
    fn questions_raw(&self) -> Result<Vec<(String, u64, String)>, Fault> {
        let broker = self.broker()?;
        let questions = broker.questions()?;
        Ok(questions
            .iter()
            .filter(|question| question.answer.is_none())
            .map(|question| {
                (
                    question.request.session.clone(),
                    question.request.id.0,
                    question.request.action.clone(),
                )
            })
            .collect())
    }

    fn broker(&self) -> Result<OnDisk, Fault> {
        OnDisk::open(self.state.join("approvals.jsonl"))
    }

    /// Record an answer. The daemon does this rather than the client so that a daemon and a stray `hades
    /// answer` are not two writers of the same append-only file at the same moment.
    fn answer(&self, request: &Value) -> Result<Value, Fault> {
        let id = request
            .get("id")
            .and_then(Value::as_u64)
            .ok_or_else(|| Fault::harness("an answer needs a numeric `id`".to_string()))?;
        let decision = match text(request, "decision").as_str() {
            "allow" => Decision::Allow,
            "deny" => Decision::Deny,
            other => {
                return Err(Fault::harness(format!(
                    "`{other}` is not an answer: say allow or deny"
                )));
            }
        };
        let scope = match text(request, "scope").as_str() {
            "once" => Scope::Once,
            "" | "session" => Scope::Session,
            other => {
                return Err(Fault::harness(format!(
                    "`{other}` is not a scope: say once or session"
                )));
            }
        };
        let by = match text(request, "by") {
            given if !given.is_empty() => given,
            _ => std::env::var("USER").unwrap_or_else(|_| "unknown".to_string()),
        };

        let option = text(request, "option");
        let note = text(request, "note");
        let broker = self.broker()?;
        let answer = if option.is_empty() {
            broker.resolve(PendingId(id), decision, scope, &by)?
        } else if option == "other" {
            broker.resolve_answer(
                PendingId(id),
                decision,
                scope,
                Some("other"),
                (!note.is_empty()).then_some(note.as_str()),
                &by,
            )?
        } else {
            broker.resolve_choice(
                PendingId(id),
                &option,
                (!note.is_empty()).then_some(note.as_str()),
                &by,
            )?
        };
        Ok(json!({
            "ok": true,
            "id": id,
            "decision": answer.decision.name(),
            "scope": answer.scope.name(),
            "by": answer.by,
        }))
    }

    /// Stop the run in one session, one numbered run, or every run. A request, not an undo: the flag is
    /// honored at the walk's next boundary, and this answers before that happens. The ledger line says who
    /// asked and what was in flight, so a stop is as auditable as an allow.
    fn stop(&self, request: &Value) -> Result<Value, Fault> {
        let session = text(request, "session");
        let wanted = number(request, "run");
        let by = actor(request);
        let mut stopping: Vec<(String, u64)> = Vec::new();
        {
            let runs = self.runs.lock().map_err(|_| {
                Fault::harness("the lease table was poisoned by a panic".to_string())
            })?;
            let cancels = self.cancels.lock().map_err(|_| {
                Fault::harness("the stop table was poisoned by a panic".to_string())
            })?;
            for (id, flag) in cancels.iter() {
                if !session.is_empty() && id != &session {
                    continue;
                }
                // A flag without a lease is a run that has already finished; its flag is only still here
                // because the cleanup has not reached it. That is nothing to stop, not run 0.
                let Some(lease) = runs.get(id) else {
                    continue;
                };
                if wanted.is_some_and(|run| lease.run != run) {
                    continue;
                }
                flag.store(true, Ordering::SeqCst);
                stopping.push((id.clone(), lease.run));
            }
        }

        let label = if !session.is_empty() {
            format!("session {session}")
        } else if let Some(run) = wanted {
            format!("run {run}")
        } else {
            "every run".to_string()
        };
        if stopping.is_empty() {
            crate::activity::stop(
                &self.state,
                &by,
                &format!("stop: {label}"),
                "nothing was running",
                "",
            );
        } else {
            for (id, run) in &stopping {
                crate::activity::stop(
                    &self.state,
                    &by,
                    &format!("stop: run {run} in {id}"),
                    "a person stopped it",
                    "",
                );
            }
        }
        Ok(json!({
            "ok": true,
            "count": stopping.len(),
            "stopping": stopping
                .iter()
                .map(|(session, run)| json!({"session": session, "run": run}))
                .collect::<Vec<_>>(),
        }))
    }

    /// Stop every run and end the daemon. Separate from `stop` so a chat command cannot mean two things: a
    /// surface that says "stopped" must not have also killed the process that answers the next message.
    fn shutdown(&self, request: &Value) -> Result<Value, Fault> {
        let by = actor(request);
        let count = {
            let cancels = self.cancels.lock().map_err(|_| {
                Fault::harness("the stop table was poisoned by a panic".to_string())
            })?;
            for flag in cancels.values() {
                flag.store(true, Ordering::SeqCst);
            }
            cancels.len()
        };
        self.stopping.store(true, Ordering::SeqCst);
        crate::activity::stop(
            &self.state,
            &by,
            &format!("shutdown: {count} run(s)"),
            "a person stopped everything",
            "",
        );
        Ok(json!({"ok": true, "shutdown": true, "stopping": count}))
    }

    /// Take a run on. The lease is the whole concurrency story: two runs on one session id would be two
    /// writers on one log, and the log has no lock to stop them.
    fn submit(&self, request: &Value, stream: &mut UnixStream) -> Result<Value, Fault> {
        // The bounds are read and refused before the lease and before any model call. A budget that cannot
        // mean anything is not a run to take on and then fail at.
        let attempts = count(request, "attempts").unwrap_or(3) as u32;
        let minutes = count(request, "minutes").unwrap_or(10);
        let mut bounds = Bounds::new(attempts, minutes);
        if let Some(tokens) = count(request, "budget-tokens") {
            let tokens = match crate::graph::budget_tokens(tokens, count(request, "max-tokens")) {
                Ok(tokens) => tokens,
                Err(said) => return Ok(json!({"ok": false, "error": said})),
            };
            bounds = bounds.with_tokens(tokens);
        }

        let flags = crate::cli::flags_from(request).map_err(Fault::harness)?;
        let attack = crate::cli::attack_of(&flags);
        let (mut engine, answerer, _, judge) =
            crate::cli::assemble(&flags, "daemon").map_err(Fault::harness)?;
        let session_id = engine.session_id().to_string();

        let cancel = Arc::new(AtomicBool::new(false));
        let run = {
            let mut runs = self.runs.lock().map_err(|_| {
                Fault::harness("the lease table was poisoned by a panic in another thread")
            })?;
            if let Some(lease) = runs.get(&session_id) {
                return Ok(json!({
                    "ok": false,
                    "error": format!(
                        "session {session_id} is already running as run {} (since {}); the log has no lock, \
                         so the daemon refuses a second writer rather than corrupting it",
                        lease.run, lease.since
                    ),
                }));
            }
            let run = self.next_run.fetch_add(1, Ordering::SeqCst) + 1;
            runs.insert(
                session_id.clone(),
                Lease {
                    run,
                    since: SystemClock.now(),
                },
            );
            // The flag goes in under the same lease, so a stop that arrives after this point finds a run and
            // a stop that arrives before it finds nothing rather than a lease with no way to reach it.
            self.cancels
                .lock()
                .map_err(|_| {
                    Fault::harness("the stop table was poisoned by a panic in another thread")
                })?
                .insert(session_id.clone(), Arc::clone(&cancel));
            run
        };

        // The acknowledgement goes out before any work, so a client knows the run is taken on and can follow
        // the log itself rather than waiting for a reply that arrives at the end.
        writeln!(
            stream,
            "{}",
            json!({"run": run, "session": session_id, "started": true})
        )
        .map_err(send_failed)?;

        // The log is followed by a thread of its own while the walk runs here, so an entry reaches a watching
        // client as soon as it is written rather than when the whole run is over.
        let log = engine.log_path();
        let done = Arc::new(AtomicBool::new(false));
        let (progress_tx, progress_rx) = std::sync::mpsc::channel::<Value>();
        let feed = move |kind: crate::model::Feed, piece: &str| {
            // A send that fails means the follower is gone, which means the client left; the model call
            // itself is unaffected and the run should finish.
            let _ = progress_tx.send(json!({"progress": kind.name(), "delta": piece}));
        };
        let follower = {
            let done = Arc::clone(&done);
            let stopping = Arc::clone(&self.stopping);
            let mut watcher = stream.try_clone().map_err(|error| {
                Fault::harness(format!("could not clone the connection: {error}"))
            })?;
            let from = fs::metadata(&log).map(|meta| meta.len()).unwrap_or(0);
            thread::spawn(move || {
                let _ = follow(&log, &mut watcher, from, &done, &stopping, &progress_rx);
            })
        };

        let mut session = engine.session().map_err(Fault::harness)?;
        let before = session.len();
        // The same memory a command-line run gets: one recall for the walk, filed once if it is accepted.
        // A daemon that walks without it is a second-class entry point, and the seam exists so it is not.
        let goal = crate::cli::goal(&flags).map_err(Fault::harness)?;
        // The marker lands once the goal is known, so an interrupted report can say what the run was doing.
        mark_inflight(&self.state, &session_id, run, &goal);
        let memo = crate::cli::remembered(
            &flags,
            &goal,
            (engine.profile().context_tokens / 10).min(2_000),
        );
        let protect = crate::cli::protect_of(&flags, engine.root()).map_err(Fault::harness)?;
        let policy_name = engine.policy_name().to_string();
        let model_name = answerer.describe();
        let check = engine.profile().verify.clone();
        let case_mode = crate::cli::case_mode(&flags).map_err(Fault::harness)?;
        let walked = {
            let mut runner = crate::graph::Runner::new(
                engine.parts(
                    answerer.harness(),
                    judge
                        .as_ref()
                        .map(|judge| judge as &dyn crate::judge::Evaluator),
                ),
                &mut session,
            )
            .with_bounds(bounds)
            .with_feed(&feed)
            .with_stop(cancel.as_ref())
            .with_baseline(crate::cli::baseline_of(&flags).map_err(Fault::harness)?)
            .with_protect(protect)
            .with_scoreboard(&self.state)
            .with_identity(model_name, policy_name)
            .with_call_tokens(crate::cli::call_tokens(&flags));
            if let Some(attack) = &attack {
                runner = runner.with_attack(attack.clone());
            }
            if let (Some(section), Some(writer)) = (memo.section.clone(), memo.memory.as_ref()) {
                runner = runner.with_memory(crate::graph::Remembering {
                    section: crate::context::Section::new(section.0, section.1),
                    writer,
                    topics: memo.topics.clone(),
                });
            }
            runner.run()
        };

        // The walker appended any compaction before it returned, and the store writes on append, so the line
        // is on disk before the lease below is released. That ordering is the contract: a crash between the
        // two leaves a compacted session and a free lease, never a released lease and an uncut history.
        // The cache is the reason the daemon exists for the map: save it once the run is over, not between
        // attempts, because the value is the next run.
        if let Err(said) = engine.save_cache() {
            eprintln!("repomap: the cache was not written, {said}");
        }
        let (hits, misses) = (engine.cache().hits(), engine.cache().misses());

        done.store(true, Ordering::SeqCst);
        let _ = follower.join();
        // The run reached its end, so the marker goes with the lease. A crash between here and the reply
        // leaves nothing behind to be read as an interruption of work that did finish.
        clear_inflight(&self.state, &session_id);
        self.runs
            .lock()
            .map_err(|_| Fault::harness("the lease table was poisoned".to_string()))?
            .remove(&session_id);
        if let Ok(mut cancels) = self.cancels.lock() {
            cancels.remove(&session_id);
        }

        match walked {
            Ok(walked) => {
                // The failure corpus, recorded before the reply: the client sees the case id in the same
                // message that says how the run ended. A write failure is a line, never a failed run.
                let case = crate::cases::record(
                    &self.state,
                    &walked,
                    &goal,
                    &session_id,
                    &check,
                    attack.as_deref(),
                    case_mode,
                );
                let (case_id, case_line, case_error) = match case {
                    Ok(crate::cases::Recorded::Case(case)) => (
                        Some(case.id.clone()),
                        Some(format!(
                            "{} recorded ({} files, tree {})",
                            case.id,
                            case.files,
                            case.tree.get(..12).unwrap_or(&case.tree)
                        )),
                        None,
                    ),
                    Ok(crate::cases::Recorded::Skipped(why)) => {
                        (None, Some(format!("not recorded ({why})")), None)
                    }
                    Err(said) => {
                        eprintln!("case: not recorded, {said}");
                        (None, Some(format!("not recorded ({said})")), Some(said))
                    }
                };
                let ended = ended_line(&walked.ended);
                writeln!(
                    stream,
                    "{}",
                    json!({
                        "run": run,
                        "session": session_id,
                        "finished": true,
                        "entries": (session.len() - before) as u64,
                        "attempts": walked.attempts(),
                        "ended": ended,
                        "exhausted": walked.ended.cause().map(|cause| cause.name()),
                        "code": walked.ended.code(),
                        "map_hits": hits,
                        "map_misses": misses,
                        "memory_recalled": memo.recalled,
                        "recorded": walked.recorded.as_ref().map(|path| path.display().to_string()),
                        "memory_error": walked.memory_error,
                        "case": case_id,
                        "case_line": case_line,
                        "case_error": case_error,
                        "compacted": session.entries()[before..]
                            .iter()
                            .any(|entry| entry.kind == Kind::Compaction),
                    })
                )
                .map_err(send_failed)?;
                Ok(Value::Null)
            }
            Err(fault) => {
                writeln!(
                    stream,
                    "{}",
                    json!({
                        "run": run,
                        "session": session_id,
                        "finished": true,
                        "fault": fault.detail(),
                        "class": fault.class().to_string(),
                        "code": 1,
                    })
                )
                .map_err(send_failed)?;
                Ok(Value::Null)
            }
        }
    }

    /// Follow a session's log until the client goes away. Nothing here is stored: a watcher that connects
    /// after the fact is reading the same file a watcher that connected first is reading.
    fn watch(&self, request: &Value, stream: &mut UnixStream) -> Result<(), Fault> {
        let session = text(request, "session");
        if session.is_empty() {
            return Err(Fault::harness("watch needs a `session`".to_string()));
        }
        let log = self.state.join("sessions").join(format!("{session}.jsonl"));
        if !log.exists() {
            return Err(Fault::harness(format!(
                "there is no session called `{session}` under {}",
                self.state.display()
            )));
        }
        // `from: "start"` replays what is already there, which is what `attach` wants; the default is to
        // follow only what happens next, which is what a `submit` client wants.
        let from = match text(request, "from").as_str() {
            "start" => 0,
            _ => fs::metadata(&log).map(|meta| meta.len()).unwrap_or(0),
        };
        let never = AtomicBool::new(false);
        // `watch` follows a log; progress frames belong to the run that produced them, not to a watcher.
        let (_quiet, quiet) = std::sync::mpsc::channel::<Value>();
        follow(&log, stream, from, &never, &self.stopping, &quiet)
    }
}

/// Every session under a state directory, as its log has it.
///
/// Free-standing because two surfaces ask this question: the daemon, and `hades status` when there is no
/// daemon. Two readings of one format would drift, and the drift would show up as a status that disagreed
/// with the log it was reading.
pub fn sessions_on_disk(state: &Path) -> Result<Vec<Value>, Fault> {
    let sessions = state.join("sessions");
    let mut listing = Vec::new();
    if let Ok(entries) = fs::read_dir(&sessions) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            listing.push(id.to_string());
        }
    }
    listing.sort();

    let mut reported = Vec::new();
    for id in listing {
        let session = Session::load(FileStore::new(&sessions), SystemClock, SessionId::new(&id))
            .map_err(|fault| Fault::harness(format!("could not read session {id}: {fault}")))?;

        // Interrupted is derived, never stored. A last entry that is a call with no result is a run that
        // stopped between appending the program and appending what it did, and nothing has to have written
        // that fact down for it to be true.
        let interrupted = session
            .entries()
            .last()
            .filter(|entry| entry.kind == Kind::ToolCall)
            .map(|entry| entry.id.0);

        reported.push(json!({
            "session": id,
            "entries": session.len(),
            "head": session.head().map(|head| head.0),
            "interrupted": interrupted,
            "state": match interrupted {
                Some(at) => format!("interrupted at #{at}"),
                None => "idle".to_string(),
            },
        }));
    }

    Ok(reported)
}

/// Read a log from `from` and write one JSON line per entry, until `done` says the writer has stopped and
/// there is nothing left to read. A partial line at the end is held back rather than emitted: a reader that
/// shows half an entry is a reader whose output cannot be parsed.
fn follow(
    path: &Path,
    stream: &mut UnixStream,
    mut offset: u64,
    done: &AtomicBool,
    stopping: &AtomicBool,
    progress: &std::sync::mpsc::Receiver<Value>,
) -> Result<(), Fault> {
    loop {
        // Deltas are the only thing on this socket that is not a log entry, and they are written here so
        // there is exactly one writer on the connection.
        while let Ok(frame) = progress.try_recv() {
            writeln!(stream, "{frame}").map_err(send_failed)?;
        }
        let mut read_something = false;
        let length = fs::metadata(path).map(|meta| meta.len()).unwrap_or(offset);
        if length > offset {
            let mut file = fs::File::open(path).map_err(|error| {
                Fault::harness(format!("could not open {}: {error}", path.display()))
            })?;
            file.seek(SeekFrom::Start(offset)).map_err(|error| {
                Fault::harness(format!("could not read {}: {error}", path.display()))
            })?;
            let mut text = String::new();
            file.read_to_string(&mut text).map_err(|error| {
                Fault::harness(format!("could not read {}: {error}", path.display()))
            })?;

            if let Some(last) = text.rfind('\n') {
                for line in text[..=last].lines() {
                    if line.trim().is_empty() {
                        continue;
                    }
                    let entry: Value = serde_json::from_str(line).unwrap_or(json!(line));
                    writeln!(stream, "{}", json!({"entry": entry})).map_err(send_failed)?;
                }
                offset += (last + 1) as u64;
                read_something = true;
            }
        }

        // A watcher outlives the run it is watching by design, so a stop has to end it: otherwise a clean
        // shutdown waits on a connection that has no reason to close.
        if (done.load(Ordering::SeqCst) || stopping.load(Ordering::SeqCst)) && !read_something {
            return Ok(());
        }
        thread::sleep(TICK);
    }
}

/// One request over the socket, for a client with nothing to follow.
pub fn ask(socket: &Path, request: Value) -> Result<Value, Fault> {
    let mut stream = UnixStream::connect(socket).map_err(|error| {
        Fault::harness(format!("could not reach {}: {error}", socket.display()))
    })?;
    writeln!(stream, "{request}")
        .map_err(|error| Fault::harness(format!("could not send the request: {error}")))?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|error| Fault::harness(format!("could not read the answer: {error}")))?;
    serde_json::from_str(line.trim())
        .map_err(|error| Fault::harness(format!("the answer would not read: {error}")))
}

/// One request over the socket, with a deadline on the answer.
///
/// [`ask`] has no read deadline by design: a CLI client watching a model thinks for as long as the model
/// thinks. A surface that answers an HTTP request cannot park a thread forever, so it names a bound. The
/// bound applies to the first line only, which is all a non-following client reads; a submit's
/// acknowledgement is written before any work, so a healthy daemon answers well inside it.
pub fn ask_within(
    socket: &Path,
    request: Value,
    timeout: std::time::Duration,
) -> Result<Value, Fault> {
    let mut stream = UnixStream::connect(socket).map_err(|error| {
        Fault::harness(format!("could not reach {}: {error}", socket.display()))
    })?;
    stream
        .set_write_timeout(Some(timeout))
        .and_then(|()| stream.set_read_timeout(Some(timeout)))
        .map_err(|error| {
            Fault::harness(format!(
                "could not time the connection to {}: {error}",
                socket.display()
            ))
        })?;
    writeln!(stream, "{request}")
        .map_err(|error| Fault::harness(format!("could not send the request: {error}")))?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let read = reader
        .read_line(&mut line)
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
                Fault::harness(format!(
                    "the daemon at {} did not answer within {} ms",
                    socket.display(),
                    timeout.as_millis()
                ))
            }
            _ => Fault::harness(format!("could not read the answer: {error}")),
        })?;
    if read == 0 {
        return Err(Fault::harness(format!(
            "the daemon at {} closed without answering",
            socket.display()
        )));
    }
    serde_json::from_str(line.trim())
        .map_err(|error| Fault::harness(format!("the answer would not read: {error}")))
}

/// The socket of a daemon that is running under this state directory, or `None`.
///
/// A pid file is a claim and not a fact, so the claim is checked: a file naming a pid that is gone is a
/// daemon that died without cleaning up, and treating it as alive would make every client wait on a socket
/// nothing is listening to.
pub fn live(state: &Path) -> Option<PathBuf> {
    let text = fs::read_to_string(state.join("daemon.json")).ok()?;
    let announced: Value = serde_json::from_str(&text).ok()?;
    let pid = announced.get("pid")?.as_u64()?;
    let socket = PathBuf::from(announced.get("socket")?.as_str()?);
    Path::new(&format!("/proc/{pid}"))
        .exists()
        .then_some(socket)
}

/// How a run ended, in one line, for a client that will not read the walk.
fn ended_line(ended: &Ended) -> String {
    match ended {
        Ended::Accepted { attempts, .. } => format!("accepted after {attempts} attempt(s)"),
        Ended::GaveUp {
            attempts, reason, ..
        } => {
            format!("gave up after {attempts} attempt(s): {reason}")
        }
        Ended::Awaiting { attempts, question } => format!(
            "waiting after {attempts} attempt(s) for #{}, on {}",
            question.id.0, question.action
        ),
        Ended::Stopped { attempts } => format!("stopped after {attempts} attempt(s)"),
        Ended::Satisfied => "the check already passed before any change; nothing to do".to_string(),
        Ended::RulerChanged { detail, .. } => {
            format!("the check changed during the run; the result is not accepted ({detail})")
        }
    }
}

/// Who asked for a stop. The caller names itself (`telegram:<id>`, `cli:<user>`) so the ledger answers "who
/// stopped what" without the daemon guessing at an identity from a message.
fn actor(request: &Value) -> String {
    let given = text(request, "by");
    if !given.is_empty() {
        return given;
    }
    format!(
        "cli:{}",
        std::env::var("USER").unwrap_or_else(|_| "unknown".to_string())
    )
}

fn text(payload: &Value, key: &str) -> String {
    payload
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn number(payload: &Value, key: &str) -> Option<u64> {
    payload.get(key).and_then(Value::as_u64)
}

/// A count from a request. The command line sends its flags as strings and a socket client may send a
/// number; both describe the same budget, so both are read here.
fn count(payload: &Value, key: &str) -> Option<u64> {
    match payload.get(key)? {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

fn send_failed(error: std::io::Error) -> Fault {
    // A client that went away is not a fault worth reporting to anybody: there is nobody left to hear it.
    Fault::harness(format!("the client went away: {error}"))
}
