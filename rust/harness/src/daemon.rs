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

/// How often the runs this daemon holds are looked at for a stall. A stall is measured in steps, not in
/// milliseconds, so once a second is enough and costs a session read per running session. This is a
/// check inside the loop the daemon already runs, never a thread of its own.
const WATCHDOG_EVERY: Duration = Duration::from_secs(1);

/// A run that holds a session.
#[derive(Debug, Clone)]
struct Lease {
    run: u64,
    since: String,
    /// The byte offset in the session log where this run's entries begin: the length of the log at the
    /// moment the run was taken on. The log is cumulative and every run appends to the same file, so this
    /// is the only thing that separates this run's steps from every run before it.
    from: u64,
    /// The token allowance the submit declared, when it declared one. The watchdog asks whether that
    /// allowance was spent without a file written; a run that declared none has no such bound.
    allowance: Option<u64>,
}

/// A lease and its stop flag, held for the length of one submit and let go on every exit path.
///
/// The pre-work between the acknowledgement and the walk has five fallible steps, and each of them used to
/// return through `?` with the lease still in the table. A failure there left a run that `status` reported as
/// running for ever, that the watchdog could not judge (`since` failed on every tick), and that nothing could
/// take on again, because the lease refused a second writer. A drop runs on every path out of the function,
/// which is the one exit the code cannot forget to take.
struct Held<'a> {
    daemon: &'a Daemon,
    session: String,
    run: u64,
    /// Why the run ended before it started. Empty while it is still going, and filled by the step that
    /// failed, so the record says where it stopped and not only that it did.
    why: String,
    ended: bool,
}

impl<'a> Held<'a> {
    fn new(daemon: &'a Daemon, session: &str, run: u64) -> Held<'a> {
        Held {
            daemon,
            session: session.to_string(),
            run,
            why: String::new(),
            ended: false,
        }
    }

    /// One fallible step of the pre-work: the fault goes back to the client unchanged, and the reason it
    /// ended stays here for the record a drop writes.
    fn step<T>(&mut self, what: &str, result: Result<T, Fault>) -> Result<T, Fault> {
        result.inspect_err(|fault| {
            self.why = format!(
                "the run ended before it started, while {what}: {}",
                fault.detail()
            );
        })
    }

    /// The normal end: the marker and the lease go together, and the drop that follows finds nothing to do.
    fn end(mut self) -> Result<(), Fault> {
        self.ended = true;
        clear_inflight(&self.daemon.state, &self.session);
        self.daemon.release(&self.session, self.run)
    }
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        if self.ended {
            return;
        }
        clear_inflight(&self.daemon.state, &self.session);
        if self.why.is_empty() {
            self.why = "the run ended before it started".to_string();
        }
        let why = std::mem::take(&mut self.why);
        // A release that cannot reach the table has nothing left to say: the lock is poisoned, which means a
        // panic elsewhere already owns the record.
        match self.daemon.release(&self.session, self.run) {
            Ok(()) => crate::activity::lease_released(
                &self.daemon.state,
                &format!("run {} in {}", self.run, self.session),
                &why,
            ),
            Err(said) => eprintln!(
                "daemon: run {} in {} could not be released: {said}",
                self.run, self.session
            ),
        }
    }
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
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect();
    inflight_dir(state).join(format!("{safe}.json"))
}

/// Where this state records the last run number it handed out.
///
/// A run number names a run in the ledger (`stop: run {run} in {id}`, `watchdog_stopped: run {run} in {id}`),
/// so it has to outlive the process that issued it: a counter in memory made every restart start again at 1,
/// and two stops in two lifetimes then read the same number.
fn run_counter_path(state: &Path) -> PathBuf {
    state.join("run-counter")
}

/// The last run number this state has handed out, or zero when nothing has run here.
///
/// A record that cannot be read is zero: a state that cannot say has run nothing as far as numbering goes,
/// which is what a first run needs, and the next lease overwrites whatever was unreadable.
fn recorded_run(state: &Path) -> u64 {
    fs::read_to_string(run_counter_path(state))
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(0)
}

/// Write the number before the run it names starts, so a crash between the two leaves a gap in the numbering
/// rather than a name used twice.
fn record_run(state: &Path, run: u64) -> Result<(), String> {
    let path = run_counter_path(state);
    fs::write(&path, format!("{run}\n"))
        .map_err(|error| format!("could not record run {run} in {}: {error}", path.display()))
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

        // A restart continues the numbering rather than starting it again: the ledger's run numbers are read
        // back by a person, and a number that names two different runs names neither.
        let next_run = recorded_run(&state);
        Ok(Daemon {
            state,
            socket,
            started: Instant::now(),
            runs: Arc::new(Mutex::new(BTreeMap::new())),
            cancels: Arc::new(Mutex::new(BTreeMap::new())),
            next_run: Arc::new(AtomicU64::new(next_run)),
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
        self.serve_every(WATCHDOG_EVERY)
    }

    /// The same loop with the watchdog's interval named, so a test can watch one tick without waiting a
    /// second for it. The interval is the only thing a caller can change about the loop.
    fn serve_every(self, every: Duration) -> Result<(), Fault> {
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
        let mut watched = Instant::now();
        while !shared.stopping.load(Ordering::SeqCst) {
            // The one periodic check this loop does: a run that is alive and going nowhere is stopped
            // here, beside the connections, rather than in a thread of its own.
            if watched.elapsed() >= every {
                watched = Instant::now();
                shared.watchdog();
            }
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

    /// One pass over the runs this daemon holds. Returns how many it stopped, which is what the test
    /// asserts on and what a future status line could report. The lease is taken as a snapshot and handed
    /// with the session, so the judgement is made about one numbered run and not about whatever the table
    /// says a moment later. A finished run has released its lease and is never inspected: the session log
    /// it left behind is history, not work in progress.
    fn watchdog(&self) -> usize {
        let held: Vec<(String, Lease)> = match self.runs.lock() {
            Ok(runs) => runs
                .iter()
                .map(|(session, lease)| (session.clone(), lease.clone()))
                .collect(),
            Err(_) => return 0,
        };
        held.iter()
            .filter(|(session, lease)| self.watch_session(session, lease))
            .count()
    }

    /// One run, judged on the steps it wrote and on nothing before them. `held` is the lease the tick
    /// snapshotted: the run number and the offset where that run's entries begin. Judging the whole log
    /// would let a brand-new run inherit the last four calls of the previous one and be stopped by them.
    fn watch_session(&self, session: &str, held: &Lease) -> bool {
        let Ok(entries) =
            FileStore::new(self.state.join("sessions")).since(&SessionId::new(session), held.from)
        else {
            // A log that will not read is `status`'s to report; stopping the run would be a guess. A torn
            // tail is the usual cause and the next tick reads it whole.
            return false;
        };
        let steps = crate::watchdog::steps(&entries);
        let limits = crate::watchdog::Limits {
            allowance: held.allowance,
            ..crate::watchdog::Limits::default()
        };
        let Some(stall) = crate::watchdog::inspect(&steps, &limits) else {
            return false;
        };
        // The lease is read again under the lock `submit` takes to release it. A run whose lease is gone has
        // finished, and the stop flag outliving it by a moment is cleanup, not work in progress: swapping
        // that flag would record `watchdog_stopped` for a run that ended before the judgement landed. The
        // same lock also keeps the one-record guarantee, since a run that is already stopping leaves its
        // flag set.
        let Ok(runs) = self.runs.lock() else {
            return false;
        };
        let Some(current) = runs.get(session) else {
            return false;
        };
        if current.run != held.run {
            // The session was released and taken on again while this tick was reading it: the judgement is
            // about a run that no longer exists, and the flag now belongs to a different one.
            return false;
        }
        let Ok(cancels) = self.cancels.lock() else {
            return false;
        };
        let Some(flag) = cancels.get(session).cloned() else {
            return false;
        };
        let asked = flag.swap(true, Ordering::SeqCst);
        drop(cancels);
        drop(runs);
        if asked {
            return false;
        }
        crate::activity::watchdog_stopped(&self.state, held.run, session, &stall.sentence());
        true
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

    /// Take a session on for one run, or refuse because another run holds it.
    ///
    /// The window this run is judged on is read here, under the lease and after the refusal, and handed back
    /// with the run number: the same two numbers the follower and the watchdog use. A run that appends its
    /// last entries and lets the session go while this thread waits for the table has moved the end of the
    /// log; a length read before the lock opened the new run's window over that run's tail, and the watchdog
    /// judged it on steps it did not take.
    fn lease(
        &self,
        session: &str,
        log: &Path,
        cancel: &Arc<AtomicBool>,
        allowance: Option<u64>,
    ) -> Result<(u64, u64), String> {
        let mut runs = self
            .runs
            .lock()
            .map_err(|_| "the lease table was poisoned by a panic in another thread".to_string())?;
        if let Some(lease) = runs.get(session) {
            return Err(format!(
                "session {session} is already running as run {} (since {}); the log has no lock, so the \
                 daemon refuses a second writer rather than corrupting it",
                lease.run, lease.since
            ));
        }
        let from = fs::metadata(log).map(|meta| meta.len()).unwrap_or(0);
        let run = self.next_run.fetch_add(1, Ordering::SeqCst) + 1;
        // The number is written before the lease exists, so a crash here leaves a gap rather than a name the
        // next lifetime will hand out again.
        record_run(&self.state, run)?;
        runs.insert(
            session.to_string(),
            Lease {
                run,
                since: SystemClock.now(),
                from,
                allowance,
            },
        );
        // The flag goes in under the same lease, so a stop that arrives after this point finds a run and a
        // stop that arrives before it finds nothing rather than a lease with no way to reach it.
        self.cancels
            .lock()
            .map_err(|_| "the stop table was poisoned by a panic in another thread".to_string())?
            .insert(session.to_string(), Arc::clone(cancel));
        Ok((run, from))
    }

    /// Let go of a session after its run reached its end.
    ///
    /// The lease and the stop flag go together, under the lease lock, and only for the run that still holds
    /// the session. The earlier shape removed the lease, released the table and then removed the flag by
    /// session id: a submit that took the session on in that gap lost its flag, a stop then found no live run
    /// while one was running, and the watchdog had no way to reach it.
    ///
    /// A submit calls this from `Held::end` on the normal path and from `Held`'s drop on every other one, so
    /// a step that fails between the acknowledgement and the walk cannot leave a run that `status` reports as
    /// running for ever.
    ///
    /// **A poisoned stop table is a note, not a shape to recover from.** The lease goes first and the flag
    /// after it, so a panic in another thread holding the stop table leaves a flag with no lease. `stop`
    /// skips exactly that case on purpose (a flag without a lease is a run that has already finished), the
    /// next lease for the session overwrites the flag, and every path that touches the poisoned table fails
    /// with a named error rather than acting on half of it. No recovery is written for it: a state nothing
    /// can reach is documented here, beside the release semantics, and not repaired by code no test can
    /// drive.
    fn release(&self, session: &str, run: u64) -> Result<(), Fault> {
        let mut runs = self
            .runs
            .lock()
            .map_err(|_| Fault::harness("the lease table was poisoned".to_string()))?;
        let Some(held) = runs.get(session) else {
            // The session is already free: a release that arrives twice has nothing to remove, and must not
            // reach for a flag that now belongs to whoever took the session on next.
            return Ok(());
        };
        if held.run != run {
            // The session was released and taken on again before this release landed: that lease and that
            // flag belong to the new run and are not this one's to remove.
            return Ok(());
        }
        runs.remove(session);
        if let Ok(mut cancels) = self.cancels.lock() {
            cancels.remove(session);
        }
        Ok(())
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
        let log = engine.log_path();
        let (run, from) = match self.lease(&session_id, &log, &cancel, bounds.tokens()) {
            Ok(taken) => taken,
            Err(said) => return Ok(json!({"ok": false, "error": said})),
        };

        // From here to the walk every step can fail, and every one of them used to leave the lease and its
        // stop flag in the table. The guard is taken the moment the lease is and let go on every path out.
        let mut held = Held::new(self, &session_id, run);

        // The acknowledgement goes out before any work, so a client knows the run is taken on and can follow
        // the log itself rather than waiting for a reply that arrives at the end.
        held.step(
            "sending the acknowledgement",
            writeln!(
                stream,
                "{}",
                json!({"run": run, "session": session_id, "started": true})
            )
            .map_err(send_failed),
        )?;

        // The log is followed by a thread of its own while the walk runs here, so an entry reaches a watching
        // client as soon as it is written rather than when the whole run is over. It follows from the same
        // offset the watchdog's window opens at.
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
            let mut watcher = held.step(
                "cloning the connection",
                stream.try_clone().map_err(|error| {
                    Fault::harness(format!("could not clone the connection: {error}"))
                }),
            )?;
            thread::spawn(move || {
                let _ = follow(&log, &mut watcher, from, &done, &stopping, &progress_rx);
            })
        };

        let mut session = held.step(
            "opening the session",
            engine.session().map_err(Fault::harness),
        )?;
        let before = session.len();
        // The same memory a command-line run gets: one recall for the walk, filed once if it is accepted.
        // A daemon that walks without it is a second-class entry point, and the seam exists so it is not.
        let goal = held.step(
            "reading the goal",
            crate::cli::goal(&flags).map_err(Fault::harness),
        )?;
        // The marker lands once the goal is known, so an interrupted report can say what the run was doing.
        mark_inflight(&self.state, &session_id, run, &goal);
        let memo = crate::cli::remembered(
            &flags,
            &goal,
            (engine.profile().context_tokens / 10).min(2_000),
        );
        let protect = held.step(
            "reading the protected paths",
            crate::cli::protect_of(&flags, engine.root()).map_err(Fault::harness),
        )?;
        let policy_name = engine.policy_name().to_string();
        let model_name = answerer.describe();
        let check = engine.profile().verify.clone();
        let case_mode = held.step(
            "reading the case mode",
            crate::cli::case_mode(&flags).map_err(Fault::harness),
        )?;
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
            .with_baseline(held.step(
                "reading the baseline",
                crate::cli::baseline_of(&flags).map_err(Fault::harness),
            )?)
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
        held.end()?;

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

#[cfg(test)]
mod watchdog_tests {
    use super::*;

    use crate::context::Section;
    use crate::{
        Budgeted, CommandVerifier, Effort, Outcome, Policy, Profile, ProfileId, Role, Rule,
        ScriptedModel, Seams, Shell, TaskSpec,
    };

    fn scratch(name: &str) -> PathBuf {
        let base = crate::scratch::root().join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("scratch");
        base
    }

    /// The same command once more, exactly as the run before it issued it. A second run on a session the
    /// first one left behind begins by repeating what was last said here.
    fn more_of_the_same(state: &Path, id: &str, calls: usize) {
        let mut session = Session::load(
            FileStore::new(state.join("sessions")),
            SystemClock,
            SessionId::new(id),
        )
        .expect("the session");
        for _ in 0..calls {
            session
                .append(
                    Kind::ToolCall,
                    json!({"tool": "shell", "program": "cargo test --quiet"}),
                )
                .expect("a call");
            session
                .append(
                    Kind::ToolResult,
                    json!({"tool": "shell", "status": 1, "output": "test failed"}),
                )
                .expect("a result");
        }
    }

    /// A run that reads nothing and writes nothing: `calls` distinct calls, each recording the tokens the
    /// model spent beside it.
    fn spending_run(state: &Path, calls: usize, tokens: u64) -> String {
        let id = "spending";
        let mut session = Session::create(
            FileStore::new(state.join("sessions")),
            SystemClock,
            SessionId::new(id),
        )
        .expect("a session");
        for n in 0..calls {
            session
                .append(
                    Kind::ToolCall,
                    json!({
                        "tool": "shell",
                        "program": format!("cargo test --test case-{n}"),
                        "model": {
                            "prompt_tokens": tokens / 2,
                            "generated_tokens": tokens - tokens / 2,
                        },
                    }),
                )
                .expect("a call");
            session
                .append(
                    Kind::ToolResult,
                    json!({"tool": "shell", "status": 1, "output": "test failed"}),
                )
                .expect("a result");
        }
        id.to_string()
    }

    /// One more call of a run's own, with its own spend.
    fn one_more_spending(state: &Path, id: &str, program: &str, tokens: u64) {
        let mut session = Session::load(
            FileStore::new(state.join("sessions")),
            SystemClock,
            SessionId::new(id),
        )
        .expect("the session");
        session
            .append(
                Kind::ToolCall,
                json!({
                    "tool": "shell",
                    "program": program,
                    "model": {
                        "prompt_tokens": tokens / 2,
                        "generated_tokens": tokens - tokens / 2,
                    },
                }),
            )
            .expect("a call");
        session
            .append(
                Kind::ToolResult,
                json!({"tool": "shell", "status": 1, "output": "test failed"}),
            )
            .expect("a result");
    }

    /// A run that keeps issuing one command: five calls, one after another, each differing only in the
    /// pipe at its end the way a model's retries do.
    fn looping_run(state: &Path, calls: usize) -> String {
        let id = "looping";
        let mut session = Session::create(
            FileStore::new(state.join("sessions")),
            SystemClock,
            SessionId::new(id),
        )
        .expect("a session");
        for n in 0..calls {
            session
                .append(
                    Kind::ToolCall,
                    json!({"tool": "shell", "program": format!("cargo test --quiet | tail -{}", n + 5)}),
                )
                .expect("a call");
            session
                .append(
                    Kind::ToolResult,
                    json!({"tool": "shell", "status": 1, "output": "test failed"}),
                )
                .expect("a result");
        }
        id.to_string()
    }

    /// A run that edits a file and then checks it, over and over: every look is paid for by a write.
    fn healthy_run(state: &Path) -> String {
        let id = "healthy";
        let mut session = Session::create(
            FileStore::new(state.join("sessions")),
            SystemClock,
            SessionId::new(id),
        )
        .expect("a session");
        for n in 0..6 {
            session
                .append(
                    Kind::ToolCall,
                    json!({"tool": "patch", "edits": [{"path": format!("src/file-{n}.rs")}]}),
                )
                .expect("an edit");
            session
                .append(
                    Kind::ToolResult,
                    json!({"tool": "patch", "status": 0, "output": "applied"}),
                )
                .expect("applied");
            session
                .append(
                    Kind::ToolCall,
                    json!({"tool": "shell", "program": format!("cargo test --test case-{n}")}),
                )
                .expect("a check");
            session
                .append(
                    Kind::ToolResult,
                    json!({"tool": "shell", "status": 0, "output": "ok"}),
                )
                .expect("ok");
        }
        id.to_string()
    }

    /// Where a session log currently ends: the offset a run taken on now would write from.
    fn log_end(state: &Path, id: &str) -> u64 {
        std::fs::metadata(store(state).path_of(&SessionId::new(id)))
            .expect("a log")
            .len()
    }

    fn store(state: &Path) -> FileStore {
        FileStore::new(state.join("sessions"))
    }

    /// A daemon holding one run, with the stop flag a submit installs. No socket is served and no thread
    /// is started: the tick is a method precisely so a test can call it twice. `from` opens the window the
    /// run is judged on, as the submit of a run does.
    fn holding(
        state: &Path,
        id: &str,
        from: u64,
        allowance: Option<u64>,
    ) -> (Daemon, Arc<AtomicBool>) {
        holding_as(state, id, 1, from, allowance)
    }

    /// The same, with the number the lease table would carry: a session taken on a second time is run 2.
    fn holding_as(
        state: &Path,
        id: &str,
        run: u64,
        from: u64,
        allowance: Option<u64>,
    ) -> (Daemon, Arc<AtomicBool>) {
        let daemon = Daemon::bind(state, state.join("daemon.sock")).expect("bind");
        let flag = Arc::new(AtomicBool::new(false));
        daemon.runs.lock().expect("runs").insert(
            id.to_string(),
            Lease {
                run,
                since: SystemClock.now(),
                from,
                allowance,
            },
        );
        daemon
            .cancels
            .lock()
            .expect("cancels")
            .insert(id.to_string(), Arc::clone(&flag));
        (daemon, flag)
    }

    /// The defect this exists for: the run number lived in memory only, so a restart began again at 1. Two
    /// stops in two lifetimes then both read `run 1 in <session>` in the ledger, and the record that exists to
    /// tell two runs apart told them nothing.
    #[test]
    fn a_restart_does_not_restart_the_run_numbers() {
        let state = scratch("run-counter");
        let id = looping_run(&state, 1);
        let log = store(&state).path_of(&SessionId::new(&id));
        let cancel = Arc::new(AtomicBool::new(false));

        let first = Daemon::bind(&state, state.join("first.sock")).expect("bind");
        let (run, _) = first.lease(&id, &log, &cancel, None).expect("run 1");
        assert_eq!(run, 1, "a fresh state starts at run 1");
        first.release(&id, 1).expect("release");
        drop(first);

        let second = Daemon::bind(&state, state.join("second.sock")).expect("bind again");
        let (run, _) = second
            .lease(&id, &log, &cancel, None)
            .expect("the run after a restart");
        assert_eq!(
            run, 2,
            "a restart restarted the numbering, so two stops in two lifetimes both read run 1"
        );
        let _ = std::fs::remove_dir_all(&state);
    }

    /// The defect this exists for: `submit` read the log length before taking the runs lock. A run that
    /// appended its last entries and released in that gap left the new lease recording an offset older than
    /// the boundary, so run 2 was judged on run 1's tail. The interleaving is forced: the table is held
    /// while run 1 appends and lets the session go, and only then may run 2 take it.
    #[test]
    fn a_run_that_finishes_while_the_submit_waits_for_the_lease_is_not_in_the_new_runs_window() {
        let state = scratch("lease-window");
        let id = looping_run(&state, 3);
        let log = store(&state).path_of(&SessionId::new(&id));
        let start = std::fs::metadata(&log).expect("a log").len();
        let (daemon, _) = holding(&state, &id, start, None);
        // Run 1 was taken on through the same path, so the counter says the next lease is run 2.
        daemon.next_run.store(1, Ordering::SeqCst);
        let cancel = Arc::new(AtomicBool::new(false));

        let mut runs = daemon.runs.lock().expect("runs");
        let run = std::thread::scope(|scope| {
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let session = id.clone();
            let daemon = &daemon;
            let taker = scope.spawn(move || {
                let _ = ready_tx.send(());
                daemon.lease(&session, &log, &cancel, None)
            });
            ready_rx.recv().expect("the submit started");
            // Run 1 is holding the session while run 2 waits for the table: it appends its last steps and
            // lets go, which is exactly the gap this test is about.
            std::thread::sleep(Duration::from_millis(200));
            more_of_the_same(&state, &id, 2);
            runs.remove(&id);
            daemon.cancels.lock().expect("cancels").remove(&id);
            drop(runs);
            taker.join().expect("the submit thread")
        });
        let (run, from) = run.expect("run 2 is taken on");

        assert_eq!(run, 2);
        let end = log_end(&state, &id);
        assert!(
            end > start,
            "run 1's tail was never appended, so the window proves nothing"
        );
        assert_eq!(
            from, end,
            "run 2's window opened before run 1's tail, so it is judged on steps it did not take"
        );
        let held = daemon
            .runs
            .lock()
            .expect("runs")
            .get(&id)
            .cloned()
            .expect("a lease");
        assert_eq!(held.from, from, "the lease records the window it was given");
        let _ = std::fs::remove_dir_all(&state);
    }

    /// The defect this exists for: the release removed the lease and then, by session id and with no run
    /// number, removed the stop flag. A submit that took the session on in that gap had its flag deleted,
    /// so a stop found no live run while one was running and the watchdog had no way to reach it: a
    /// five-call loop ran on unstopped.
    #[test]
    fn a_late_release_does_not_disarm_the_run_that_took_the_session() {
        let state = scratch("lease-release-race");
        let id = looping_run(&state, 5);
        let (daemon, _finished) = holding(&state, &id, 0, None);
        // Run 2 took the session on between the lease removal and the flag removal.
        let live = Arc::new(AtomicBool::new(false));
        let from = log_end(&state, &id);
        daemon.runs.lock().expect("runs").insert(
            id.clone(),
            Lease {
                run: 2,
                since: SystemClock.now(),
                from,
                allowance: None,
            },
        );
        daemon
            .cancels
            .lock()
            .expect("cancels")
            .insert(id.clone(), Arc::clone(&live));

        // Run 1's release arrives late.
        daemon.release(&id, 1).expect("release");

        assert_eq!(
            daemon
                .runs
                .lock()
                .expect("runs")
                .get(&id)
                .map(|lease| lease.run),
            Some(2),
            "the live run's lease was deleted by a finished run's release"
        );
        assert!(
            daemon.cancels.lock().expect("cancels").contains_key(&id),
            "the live run's stop flag was deleted by a finished run's release"
        );
        let stopped = daemon
            .stop(&json!({"session": id, "run": 2}))
            .expect("a stop");
        assert_eq!(
            stopped["count"], 1,
            "a stop found no live run while one was running: {stopped}"
        );
        assert!(
            live.load(Ordering::SeqCst),
            "the live run was not asked to stop"
        );
        let _ = std::fs::remove_dir_all(&state);
    }

    /// The other direction: a stop that reaches a run still holding the session finds it and asks it to
    /// stop, and the release that follows leaves neither a lease nor a flag behind.
    #[test]
    fn a_stop_while_the_run_holds_the_session_finds_it_and_the_release_leaves_nothing() {
        let state = scratch("lease-stop-live");
        let id = looping_run(&state, 5);
        let (daemon, flag) = holding(&state, &id, 0, None);

        let stopped = daemon
            .stop(&json!({"session": id, "run": 1}))
            .expect("a stop");
        assert_eq!(stopped["count"], 1, "{stopped}");
        assert_eq!(stopped["stopping"][0]["run"], 1);
        assert!(flag.load(Ordering::SeqCst), "the run was not asked to stop");

        daemon.release(&id, 1).expect("release");
        assert!(
            daemon.runs.lock().expect("runs").is_empty(),
            "the lease outlived the run"
        );
        assert!(
            daemon.cancels.lock().expect("cancels").is_empty(),
            "the stop flag outlived the run"
        );
        let _ = std::fs::remove_dir_all(&state);
    }

    fn stopped(state: &Path) -> Vec<crate::activity::Line> {
        crate::activity::read(state)
            .into_iter()
            .filter(|line| line.action.starts_with("watchdog_stopped"))
            .collect()
    }

    /// The defect this exists for: `watchdog_stopped` named the session and the finding but not the run,
    /// while `stop` writes `run {run} in {id}`. Two stops across runs, or across a restart where the
    /// numbering starts again at 1, were two copies of one indistinguishable record.
    #[test]
    fn a_watchdog_stop_carries_the_run_it_stopped() {
        let state = scratch("watchdog-run-number");
        let id = looping_run(&state, 5);
        let (daemon, _first) = holding(&state, &id, 0, None);
        assert_eq!(daemon.watchdog(), 1, "the first run stalls");
        daemon.release(&id, 1).expect("release");

        // The session is taken on again and repeats the same loop: run 2, judged on its own window.
        let from = log_end(&state, &id);
        more_of_the_same(&state, &id, 5);
        let (daemon, _second) = holding_as(&state, &id, 2, from, None);
        assert_eq!(daemon.watchdog(), 1, "the second run stalls");

        let lines = stopped(&state);
        assert_eq!(lines.len(), 2, "{lines:?}");
        for (line, run) in lines.iter().zip([1, 2]) {
            assert!(
                line.action.contains(&format!("run {run}")),
                "a stop does not name its run: {:?}",
                line.action
            );
            assert!(
                line.action.contains(&id),
                "a stop does not name its session: {:?}",
                line.action
            );
        }
        assert_ne!(
            lines[0].action, lines[1].action,
            "two stops across two runs are one record"
        );
    }

    #[test]
    fn a_looping_run_is_stopped_once_and_recorded_once() {
        let state = scratch("watchdog-loop");
        let id = looping_run(&state, 5);
        let (daemon, flag) = holding(&state, &id, 0, None);

        assert_eq!(daemon.watchdog(), 1, "the loop was found");
        assert!(flag.load(Ordering::SeqCst), "the run was asked to stop");
        let lines = stopped(&state);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0].decision, "stop");
        assert_eq!(lines[0].layer, "watchdog");
        assert!(
            lines[0]
                .reason
                .contains("repeating shell: cargo test --quiet"),
            "{:?}",
            lines[0].reason
        );
        assert!(
            lines[0].action.contains("run 1") && lines[0].action.contains(&id),
            "the run and the session are named: {:?}",
            lines[0].action
        );
        let _ = std::fs::remove_dir_all(&state);
    }

    /// The next tick sees the same run and must not ask again: one record per stop is what makes the line
    /// answerable, and a run winding down is still alive for a moment after the request.
    #[test]
    fn a_second_tick_over_a_run_already_stopping_records_nothing_more() {
        let state = scratch("watchdog-twice");
        let id = looping_run(&state, 5);
        let (daemon, _flag) = holding(&state, &id, 0, None);
        assert_eq!(daemon.watchdog(), 1);
        for _ in 0..3 {
            assert_eq!(daemon.watchdog(), 0);
        }
        assert_eq!(stopped(&state).len(), 1, "{:?}", stopped(&state));
        let _ = std::fs::remove_dir_all(&state);
    }

    /// A run that has finished released its lease, and a tick that inspected it anyway would stop a run
    /// that is not there and stop the next run to use that session id.
    #[test]
    fn a_finished_run_is_not_inspected() {
        let state = scratch("watchdog-finished");
        let id = looping_run(&state, 5);
        let (daemon, flag) = holding(&state, &id, 0, None);
        daemon.runs.lock().expect("runs").remove(&id);
        assert_eq!(daemon.watchdog(), 0);
        assert!(!flag.load(Ordering::SeqCst), "a finished run was stopped");
        assert!(stopped(&state).is_empty(), "{:?}", stopped(&state));
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn a_healthy_run_is_left_alone() {
        let state = scratch("watchdog-healthy");
        let id = healthy_run(&state);
        let (daemon, flag) = holding(&state, &id, 0, None);
        for _ in 0..3 {
            assert_eq!(daemon.watchdog(), 0);
        }
        assert!(!flag.load(Ordering::SeqCst), "a working run was stopped");
        assert!(stopped(&state).is_empty(), "{:?}", stopped(&state));
        let _ = std::fs::remove_dir_all(&state);
    }

    /// The cost of a tick on a long session. The tick used to load and parse the whole log once a second
    /// for every running session; the audit measured 3.06 s for 2.9 MB and 20,001 entries in a debug build,
    /// which is time the accept loop spends not accepting. It now reads from the offset the run was taken
    /// on, so the numbers printed here are the difference between reading a history and reading a run.
    /// They are printed and not asserted: a wall clock on a shared machine is not evidence a build can
    /// keep, and a flaky ceiling would teach a person to ignore this test.
    #[test]
    fn a_tick_over_a_long_history_reads_only_this_runs_window() {
        let state = scratch("watchdog-long-log");
        let id = looping_run(&state, 1);
        let mut session =
            Session::load(store(&state), SystemClock, SessionId::new(&id)).expect("the session");
        for n in 0..10_000 {
            session
                .append(
                    Kind::ToolCall,
                    json!({
                        "tool": "shell",
                        "program": format!("cargo test --test case-{n}"),
                        "model": {"prompt_tokens": 100, "generated_tokens": 100},
                    }),
                )
                .expect("a call");
            session
                .append(
                    Kind::ToolResult,
                    json!({"tool": "shell", "status": 1, "output": "test failed"}),
                )
                .expect("a result");
        }
        let from = log_end(&state, &id);
        for n in 0..3 {
            one_more_spending(&state, &id, &format!("cargo test --test fresh-{n}"), 100);
        }

        let (daemon, flag) = holding(&state, &id, from, None);
        let window = store(&state)
            .since(&SessionId::new(&id), from)
            .expect("the window");
        let whole = std::time::Instant::now();
        let loaded =
            Session::load(store(&state), SystemClock, SessionId::new(&id)).expect("the whole log");
        let load_ms = whole.elapsed().as_secs_f64() * 1_000.0;
        let ticked = std::time::Instant::now();
        assert_eq!(daemon.watchdog(), 0, "a healthy run was stopped");
        let tick_ms = ticked.elapsed().as_secs_f64() * 1_000.0;

        assert_eq!(
            window.len(),
            6,
            "the window holds this run's entries and nothing else"
        );
        assert_eq!(
            loaded.len(),
            20_009,
            "a twenty-thousand-entry history, the size the audit measured"
        );
        assert!(!flag.load(Ordering::SeqCst), "a working run was stopped");
        println!(
            "{} entries, {} bytes: a whole log {} ms, one tick {tick_ms} ms over {} entries",
            loaded.len(),
            from,
            load_ms,
            window.len()
        );
        let _ = std::fs::remove_dir_all(&state);
    }

    /// The defect this exists for: the call that runs the tick inside the accept loop had no test, so
    /// deleting it left the whole suite green and the watchdog stopped watching. The interval is a parameter
    /// so that this costs a millisecond rather than a second per look.
    #[test]
    fn the_accept_loop_runs_the_tick_while_it_serves() {
        use std::io::Write;
        use std::os::unix::net::UnixStream;

        let state = scratch("watchdog-loop-wired");
        let id = looping_run(&state, 5);
        let (daemon, flag) = holding(&state, &id, 0, None);
        let socket = daemon.socket().to_path_buf();
        let served = std::thread::spawn(move || daemon.serve_every(Duration::from_millis(1)));

        let deadline = Instant::now() + Duration::from_secs(5);
        while stopped(&state).is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            !stopped(&state).is_empty(),
            "the loop served connections without ever running the tick"
        );
        assert!(flag.load(Ordering::SeqCst), "the run was not asked to stop");

        // End the loop the way a person does, so the thread joins and the socket is cleaned up.
        let mut stream = UnixStream::connect(&socket).expect("connect to the daemon");
        writeln!(stream, "{}", json!({"op": "shutdown", "by": "test"})).expect("shutdown");
        drop(stream);
        served
            .join()
            .expect("the serving thread")
            .expect("a clean exit");
        assert_eq!(stopped(&state).len(), 1, "{:?}", stopped(&state));
        let _ = std::fs::remove_dir_all(&state);
    }

    /// The defect this exists for: the tick snapshotted the leases and then swapped the stop flag without
    /// looking at the lease again. `submit` releases the run before it removes the flag, so a tick landing in
    /// that window found a finished run's flag and recorded `watchdog_stopped` for work that had already
    /// ended. The flag outliving the lease is cleanup, and cleanup is not a run in progress.
    #[test]
    fn a_run_that_finished_between_the_snapshot_and_the_record_is_not_stopped() {
        let state = scratch("watchdog-finished-mid-tick");
        let id = looping_run(&state, 5);
        let (daemon, flag) = holding(&state, &id, 0, None);
        let held = daemon
            .runs
            .lock()
            .expect("runs")
            .get(&id)
            .cloned()
            .expect("a lease");
        // The run reaches its end between the snapshot and the record, in the order `submit` uses: the lease
        // goes first and the flag a moment later.
        daemon.runs.lock().expect("runs").remove(&id);

        assert!(
            !daemon.watch_session(&id, &held),
            "a finished run was recorded as stopped"
        );
        assert!(
            !flag.load(Ordering::SeqCst),
            "a finished run was asked to stop"
        );
        assert!(stopped(&state).is_empty(), "{:?}", stopped(&state));
        let _ = std::fs::remove_dir_all(&state);
    }

    /// The defect this exists for: the tick judged a run on the whole session log, so a brand-new run that
    /// repeated the previous run's last call was stopped inside its first second. Four identical calls in
    /// run 1 and one of its own in run 2 is a fresh run, not a loop.
    #[test]
    fn a_new_run_is_not_stopped_for_the_calls_the_run_before_it_made() {
        let state = scratch("watchdog-second-run");
        let id = looping_run(&state, 4);
        let from = log_end(&state, &id);
        more_of_the_same(&state, &id, 1);
        let (daemon, flag) = holding(&state, &id, from, None);
        assert_eq!(
            daemon.watchdog(),
            0,
            "a fresh run was stopped on its predecessor's calls: {:?}",
            stopped(&state)
        );
        assert!(
            !flag.load(Ordering::SeqCst),
            "the new run was asked to stop"
        );
        assert!(stopped(&state).is_empty(), "{:?}", stopped(&state));
        let _ = std::fs::remove_dir_all(&state);
    }

    /// The same flaw spent the allowance twice: the budget belongs to the run that declared it, so a second
    /// run's spend is measured from where it started. Run 1 spent 4,000 and wrote nothing; run 2 has spent
    /// 200 of its own against 4,000.
    #[test]
    fn a_new_run_spends_its_own_allowance_and_not_the_run_before_its() {
        let state = scratch("watchdog-second-run-spend");
        let id = spending_run(&state, 4, 1_000);
        let from = log_end(&state, &id);
        one_more_spending(&state, &id, "cargo test --test one", 200);
        let (daemon, flag) = holding(&state, &id, from, Some(4_000));
        assert_eq!(
            daemon.watchdog(),
            0,
            "a fresh run was stopped on its predecessor's spend: {:?}",
            stopped(&state)
        );
        assert!(
            !flag.load(Ordering::SeqCst),
            "the new run was asked to stop"
        );
        assert!(stopped(&state).is_empty(), "{:?}", stopped(&state));
        let _ = std::fs::remove_dir_all(&state);
    }

    /// The pieces a real turn runs on, so a test that drives the writer does not rebuild six seams inline.
    #[allow(clippy::type_complexity)]
    fn bench(
        state: &Path,
    ) -> (
        PathBuf,
        TaskSpec,
        Profile,
        Policy,
        Budgeted,
        Shell,
        CommandVerifier,
    ) {
        let root = state.join("work");
        std::fs::create_dir_all(&root).expect("a work root");
        let task = TaskSpec::new("build the tree and read what it wrote", &root);
        let profile = Profile {
            id: ProfileId::new("test"),
            role: Role::Patch,
            model: Some("scripted".to_string()),
            effort: Effort::Medium,
            context_tokens: 4096,
            verify: "true".to_string(),
            judge: false,
            plan_first: false,
            edits: true,
            shadow: true,
            attempts: 3,
            minutes: 10,
            protect: Vec::new(),
            network: true,
        };
        let policy = Policy::new(vec![Rule::new(
            crate::SHELL,
            Decision::Allow,
            "the test allows the shell",
        )]);
        let context = Budgeted::new(vec![Section::new("goal", task.goal())]);
        let shell = Shell::new(&root, 64 * 1024);
        let verifier = CommandVerifier::new(64 * 1024);
        (root, task, profile, policy, context, shell, verifier)
    }

    /// The chain the headline claim rests on, assembled once instead of argued in halves.
    ///
    /// Every other stop in this module is judged on a history this module wrote by hand, and every one of
    /// those `ToolCall`s carries no `did`: the daemon has therefore only ever been proven against the text
    /// fallback, while a real run's history is the one the writer produces. So this test drives the real
    /// seams: `Seams::turn` writes five identical builds, each with the classification the producer decided,
    /// under the lease a submit takes. The same tick then has to stop that run, name it in the ledger, and
    /// leave the next run alone: the same session taken on again, which reads the file the first one wrote
    /// and then builds once.
    #[test]
    fn the_tick_stops_a_run_real_turns_wrote_and_leaves_the_next_run_alone() {
        // A build whose output is the file the second run reads. The redirect is what puts the artifact on
        // disk, and it is also what makes the step a write: `cargo build` alone is a build the log cannot see
        // into, which is the shape the repeat rule is for.
        const BUILD: &str = "cargo build --quiet > build.log";

        let state = scratch("watchdog-assembled");
        let (root, task, profile, policy, context, shell, verifier) = bench(&state);
        let daemon = Daemon::bind(&state, state.join("daemon.sock")).expect("bind");

        // Run 1, taken on the way a submit takes it on: the lease first, then the turns that write while it
        // is held.
        let building = "building";
        let mut log = Session::create(store(&state), SystemClock, SessionId::new(building))
            .expect("a session");
        let path = store(&state).path_of(&SessionId::new(building));
        let first = Arc::new(AtomicBool::new(false));
        let (run, from) = daemon
            .lease(building, &path, &first, None)
            .expect("run 1 is taken on");
        assert_eq!(run, 1, "a fresh state names its first run 1");

        let model = ScriptedModel::new([BUILD; 5]);
        for n in 0..5 {
            let turn = Seams::new(
                &task, &profile, &policy, &context, &model, &shell, &verifier,
            )
            .turn(&mut log)
            .unwrap_or_else(|fault| panic!("turn {}: {}", n + 1, fault.detail()));
            assert!(
                matches!(turn.outcome, Outcome::Ran { .. }),
                "turn {} did not run the program: {:?}",
                n + 1,
                turn.outcome
            );
        }
        drop(log);
        assert!(
            root.join("build.log").is_file(),
            "the build wrote no file, so the second run has nothing of the first one's to read"
        );

        // The verdict itself, on the history this run wrote and nothing before it: five identical calls is
        // `Repeating` at exactly five. Without this, "run 1 reached Repeating" rides on the ledger sentence,
        // and that sentence is written by the same tick: a tick that stopped the run on some other ground and
        // named a repeat would read identically. The step count is asserted too, because a verdict reached
        // over the wrong window would still be a verdict.
        let history = store(&state)
            .since(&SessionId::new(building), from)
            .expect("run 1's history");
        let steps = crate::watchdog::steps(&history);
        assert_eq!(
            crate::watchdog::inspect(&steps, &crate::watchdog::Limits::default()),
            Some(crate::watchdog::Stall::Repeating {
                tool: "shell: cargo build --quiet".to_string(),
                times: 5,
            }),
            "run 1's own history is not the stall the tick is supposed to find: {} steps, {:?}",
            steps.len(),
            steps
        );

        // The tick with only run 1 held: five identical calls is the repeat rule's five, so this is a stop.
        let count = daemon.watchdog();
        assert_eq!(
            count,
            1,
            "the tick stopped {count} runs: {:?}",
            stopped(&state)
        );
        assert!(
            first.load(Ordering::SeqCst),
            "the run that wrote the repeated history was never asked to stop"
        );

        // Run 2 is the same session taken on again, which is how a session is resumed: a submit releases the
        // lease when its run exits, and the next run on that log is a new run with a window of its own. It
        // makes the one call it is entitled to make, the same build once more.
        //
        // One build, and not three looks followed by a build, because that is the shape that discriminates.
        // The repeat rule counts the trailing run of identical calls, so a look between the two builds would
        // break the run and the assertion would hold whichever window the tick read. A look is no better:
        // its first occurrence is progress in both windows, so the stall count is the same in both. If the
        // tick judged the whole log instead of this run's own steps, run 1's five builds and this one would
        // be six identical calls, the repeat rule stops a run at five, and run 2 would be stopped for the
        // calls the run before it made. The shape this replaced (a separate session, three looks, a
        // twelve-step bound) could not fail for the reason it claimed: a separate session has no earlier
        // prefix to ignore, and nothing in it could have reached a bound of any size.
        daemon
            .release(building, run)
            .expect("run 1's lease goes when its run exits");
        let mut reread = Session::load(store(&state), SystemClock, SessionId::new(building))
            .expect("the session run 1 left");
        let second = Arc::new(AtomicBool::new(false));
        let (again, at) = daemon
            .lease(building, &path, &second, None)
            .expect("run 2 is taken on");
        assert_eq!(again, 2, "the second lease is run 2");
        assert!(
            at > from,
            "run 2's window starts at {at}, which is not past the {from} where run 1's entries begin"
        );
        let reader = ScriptedModel::new([BUILD]);
        let turn = Seams::new(
            &task, &profile, &policy, &context, &reader, &shell, &verifier,
        )
        .turn(&mut reread)
        .expect("a turn of run 2");
        assert!(
            matches!(turn.outcome, Outcome::Ran { .. }),
            "run 2 did not run the program: {:?}",
            turn.outcome
        );
        drop(reread);

        let count = daemon.watchdog();
        assert_eq!(
            count,
            0,
            "the tick stopped {count} runs on the second pass: {:?}",
            stopped(&state)
        );
        assert!(
            !second.load(Ordering::SeqCst),
            "the second run was stopped for the calls the run before it made"
        );

        let lines = stopped(&state);
        assert_eq!(lines.len(), 1, "one stop, one record: {lines:?}");
        assert_eq!(lines[0].layer, "watchdog");
        assert_eq!(lines[0].decision, "stop");
        assert!(
            lines[0]
                .reason
                .contains("repeating shell: cargo build --quiet"),
            "the record does not name the call the run repeated: {:?}",
            lines[0].reason
        );
        assert!(
            lines[0].action.contains("run 1") && lines[0].action.contains(building),
            "the record does not name the run it stopped: {:?}",
            lines[0].action
        );
        assert!(
            !lines[0].action.contains("run 2"),
            "the record names the run that was left alone: {:?}",
            lines[0].action
        );
        println!("assembled: {:?}", lines[0].action);
        let _ = std::fs::remove_dir_all(&state);
    }

    /// The other half of the same claim, and the one the hand-written fixtures could never reach: the tick
    /// reads the judgement the producer recorded, not the program's text.
    ///
    /// `git log` is a look. The record says so, because the producer had the tool and its arguments in hand.
    /// The text fallback, which exists only for logs written before the field existed, names a fixed list of
    /// reporters and `git` is not on it, so it reads the same twelve steps as twelve that looked at nothing
    /// and stops the run. Twelve *distinct* reads are progress under the record and a stall under the text.
    /// Nothing here is hand-built: the calls are the ones `Seams::turn` wrote, and the only thing this test
    /// changes is which run the daemon holds.
    #[test]
    fn the_tick_reads_the_record_the_writer_made_rather_than_the_program_text() {
        let state = scratch("watchdog-record");
        let (_root, task, profile, policy, context, shell, verifier) = bench(&state);
        let daemon = Daemon::bind(&state, state.join("daemon.sock")).expect("bind");

        let looking = "looking";
        let mut log = Session::create(store(&state), SystemClock, SessionId::new(looking))
            .expect("a session");
        let path = store(&state).path_of(&SessionId::new(looking));
        let flag = Arc::new(AtomicBool::new(false));
        daemon
            .lease(looking, &path, &flag, None)
            .expect("the run is taken on");

        let programs: Vec<String> = (0..12)
            .map(|n| format!("git log --oneline case-{n}"))
            .collect();
        let model = ScriptedModel::new(programs);
        for _ in 0..12 {
            let turn = Seams::new(
                &task, &profile, &policy, &context, &model, &shell, &verifier,
            )
            .turn(&mut log)
            .expect("a turn that looks");
            assert!(
                matches!(turn.outcome, Outcome::Ran { .. }),
                "{:?}",
                turn.outcome
            );
        }
        drop(log);

        let count = daemon.watchdog();
        assert_eq!(
            count,
            0,
            "the tick stopped a run of twelve distinct looks: {:?}",
            stopped(&state)
        );
        assert!(
            !flag.load(Ordering::SeqCst),
            "the run was asked to stop for looking"
        );
        assert!(stopped(&state).is_empty(), "{:?}", stopped(&state));
        let _ = std::fs::remove_dir_all(&state);
    }
}
