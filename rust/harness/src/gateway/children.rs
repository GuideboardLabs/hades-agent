//! The gateway's child registry: the gateway owns every admitted child, and a parent holds only an id.
//!
//! Why here and not in the daemon: the daemon owns no state of its own, and its `submit` assembles a
//! graph runner rather than a tool-using conversation. Cron is the gateway's scheduler and has the wrong
//! shape for this: a job repeats on a schedule, a child runs once and reports one typed result. So the
//! gateway gets one small registry: `<state>/children/<id>.json` per admitted child, one executor thread
//! per child, and the existing [`crate::agent::answer`] loop inside it.
//!
//! The honest limit: a thread cannot outlive its process. A gateway restart marks every `admitted` or
//! `running` record `lost`, with a reason, instead of resuming work that no longer exists. Cutover has to
//! treat those children as in-flight work rather than as something this registry can recover.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use session_tree::{FileStore, Kind, Session, SystemClock};

use crate::agent::{Activity, Inbox, World, child_prompt, fits, parse_json_answer};
use crate::model::{Feed, OpenAi};

/// The most children one gateway process runs at once. The bound exists so a model that keeps spawning
/// cannot turn one chat into an unbounded number of threads; a refusal is a typed `busy`, not a queue.
pub const MAX_LIVE: usize = 16;

/// A child's record moves through these states. `admitted` is the handle's answer, `running` is the
/// executor's first write, and the last three are terminal.
pub const STATE_ADMITTED: &str = "admitted";
pub const STATE_RUNNING: &str = "running";
pub const STATE_DONE: &str = "done";
pub const STATE_FAILED: &str = "failed";
pub const STATE_LOST: &str = "lost";

/// The `type` discriminator on a child lifecycle [`session_tree::Kind::Meta`] event. A reader that does
/// not know a value skips it; the schema version does not move for a payload because the store already
/// keeps any JSON object in Meta.
pub const CHILD_ADMITTED: &str = "child_admitted";
pub const CHILD_MESSAGE: &str = "child_message";
pub const CHILD_RESULT: &str = "child_result";

type Log = Session<FileStore, SystemClock>;

/// One admitted child, as the registry keeps it. `parent` is the parent's session id, `session` is the
/// child's, and `result_path` is the artifact `result` will read. A finished child keeps both timestamps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub id: String,
    pub parent: String,
    pub session: String,
    pub chat: i64,
    pub state: String,
    pub result_path: String,
    pub started: i64,
    pub ended: Option<i64>,
    /// Why a child is `failed` or `lost`. Absent while it is running and after a clean finish.
    pub reason: Option<String>,
    /// The process that wrote this record, and that process's start token. Both are absent on a record
    /// written before ownership was recorded: such a record has no owner this build can find alive, so
    /// recovery treats it as a dead gateway's child.
    pub pid: Option<u32>,
    pub start: Option<u64>,
}

impl Record {
    pub fn to_value(&self) -> Value {
        json!({
            "id": self.id,
            "parent": self.parent,
            "session": self.session,
            "chat": self.chat,
            "state": self.state,
            "result_path": self.result_path,
            "started": self.started,
            "ended": self.ended,
            "reason": self.reason,
            "pid": self.pid,
            "start": self.start,
        })
    }

    /// Strict: a record missing a field an owner relies on is not a record. `ended` and `reason` are
    /// nullable because a live child has neither. `pid` and `start` are optional only for records this
    /// build did not write; a record that has them is checked against the live process table.
    pub fn from_value(value: &Value) -> Option<Record> {
        Some(Record {
            id: value.get("id")?.as_str()?.to_string(),
            parent: value.get("parent")?.as_str()?.to_string(),
            session: value.get("session")?.as_str()?.to_string(),
            chat: value.get("chat")?.as_i64()?,
            state: value.get("state")?.as_str()?.to_string(),
            result_path: value.get("result_path")?.as_str()?.to_string(),
            started: value.get("started")?.as_i64()?,
            ended: value.get("ended").and_then(Value::as_i64),
            reason: value
                .get("reason")
                .and_then(Value::as_str)
                .map(str::to_string),
            pid: value
                .get("pid")
                .and_then(Value::as_u64)
                .and_then(|pid| u32::try_from(pid).ok()),
            start: value.get("start").and_then(Value::as_u64),
        })
    }

    /// Read one child's record, if it exists and still parses.
    pub fn load(state: &Path, id: &str) -> Option<Record> {
        read_record(&path_of(state, id))
    }

    /// Write one record atomically, so a reader never sees half a file.
    pub fn write(state: &Path, record: &Record) -> Result<(), String> {
        write_atomic(&path_of(state, &record.id), &record.to_value())
    }
}

/// What a caller asks the registry to run. The goal and schema live here rather than in the record
/// because a restart does not resume a child: the record is the lifecycle, the request is the work.
#[derive(Debug, Clone, PartialEq)]
pub struct Admission {
    pub id: String,
    /// The parent session, so the child's Meta events can say where they came from.
    pub parent: String,
    pub session: String,
    pub chat: i64,
    pub goal: String,
    pub schema: Value,
    pub steps: usize,
}

impl Admission {
    /// The default contract: one prose field, still an object, so `result` never returns a bare string.
    pub fn report_schema() -> Value {
        json!({
            "type": "object",
            "properties": {"report": {"type": "string"}},
            "required": ["report"]
        })
    }
}

/// What the caller gets back as soon as the record exists and the executor is started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Admitted {
    pub child_id: String,
    pub session: String,
    pub state: String,
}

impl Admitted {
    pub fn to_value(&self) -> Value {
        json!({
            "child_id": self.child_id,
            "session": self.session,
            "state": self.state,
        })
    }
}

/// Why admission was refused, or why `tell`/`result` could not answer. `Busy` is the one a tool
/// reports as a typed busy fault; the rest name the case plainly so a caller can tell a retryable
/// refusal from a wrong id from a child that is simply not finished yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    Busy {
        live: usize,
        limit: usize,
    },
    Exists {
        id: String,
    },
    /// The session already belongs to a child. The child's log and `result.json` are derived from the
    /// session, so admitting a second child with the same one would make them share both.
    SessionInUse {
        session: String,
        id: String,
    },
    Io {
        path: String,
        detail: String,
    },
    /// No child with that id on this gateway.
    Missing {
        id: String,
    },
    /// The child reached a terminal state, so a message would be queued for nobody to read.
    Finished {
        id: String,
        state: String,
    },
    /// The wait ran out before the child finished. The child was not killed and may still finish.
    TimedOut {
        id: String,
        state: String,
        waited_ms: u128,
    },
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Busy { live, limit } => write!(
                f,
                "the gateway is already running {live} children (limit {limit}); try again when one finishes"
            ),
            Refusal::Exists { id } => write!(f, "child {id} already exists"),
            Refusal::SessionInUse { session, id } => {
                write!(f, "session {session} already belongs to child {id}")
            }
            Refusal::Io { path, detail } => write!(f, "{path}: {detail}"),
            Refusal::Missing { id } => write!(f, "there is no child {id} on this gateway"),
            Refusal::Finished { id, state } => write!(
                f,
                "child {id} is already {state}; it will not read another message"
            ),
            Refusal::TimedOut {
                id,
                state,
                waited_ms,
            } => write!(
                f,
                "child {id} was still {state} after {waited_ms} ms; it was not killed"
            ),
        }
    }
}

impl std::error::Error for Refusal {}

/// What one startup sweep did, with every file in the children directory in exactly one bucket.
/// `accounted` is the invariant: it must equal `files`, so a caller can tell a complete sweep from a
/// partial one without trusting a single count.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Recovery {
    /// How many `.json` files the sweep saw.
    pub files: usize,
    /// Admitted/running records whose owner is gone, now `lost`.
    pub marked: usize,
    /// Records not admitted or running: already terminal, or a state this build does not resume.
    pub terminal: usize,
    /// Live records this process owns (including a second `serve` sharing this registry).
    pub this_process: usize,
    /// Live records owned by another process that is still alive.
    pub elsewhere: usize,
    /// Files that did not parse. Left in place for a person; never guessed at.
    pub unreadable: Vec<String>,
    /// Records the sweep could not rewrite, with the failure. The sweep keeps going.
    pub failed: Vec<String>,
}

impl Recovery {
    pub fn accounted(&self) -> usize {
        self.marked
            + self.terminal
            + self.this_process
            + self.elsewhere
            + self.unreadable.len()
            + self.failed.len()
    }
}

/// A child lifecycle event. The `type` discriminator is the whole point: an older reader sees a Meta
/// payload it does not understand and ignores it, instead of failing the log or inventing a state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Admitted {
        child_id: String,
        parent_session: String,
        session: String,
        chat: i64,
        goal: String,
    },
    /// A message delivered to a running child. A3's `tell` writes this; the registry defines it now so
    /// the shape is one place.
    Message {
        child_id: String,
        parent_session: String,
        text: String,
    },
    Result {
        child_id: String,
        parent_session: String,
        status: String,
        result_path: String,
    },
}

impl Event {
    pub fn type_name(&self) -> &'static str {
        match self {
            Event::Admitted { .. } => CHILD_ADMITTED,
            Event::Message { .. } => CHILD_MESSAGE,
            Event::Result { .. } => CHILD_RESULT,
        }
    }

    pub fn payload(&self) -> Value {
        match self {
            Event::Admitted {
                child_id,
                parent_session,
                session,
                chat,
                goal,
            } => json!({
                "type": CHILD_ADMITTED,
                "child_id": child_id,
                "parent_session": parent_session,
                "session": session,
                "chat": chat,
                "goal": goal,
            }),
            Event::Message {
                child_id,
                parent_session,
                text,
            } => json!({
                "type": CHILD_MESSAGE,
                "child_id": child_id,
                "parent_session": parent_session,
                "text": text,
            }),
            Event::Result {
                child_id,
                parent_session,
                status,
                result_path,
            } => json!({
                "type": CHILD_RESULT,
                "child_id": child_id,
                "parent_session": parent_session,
                "status": status,
                "result_path": result_path,
            }),
        }
    }

    /// `None` for any entry that is not a child event, including a Meta entry whose `type` this build
    /// does not know. That is forward compatibility, not leniency: the entry stays in the log untouched.
    pub fn parse(entry: &session_tree::Entry) -> Option<Event> {
        if entry.kind != Kind::Meta {
            return None;
        }
        let payload = &entry.payload;
        match payload.get("type").and_then(Value::as_str)? {
            CHILD_ADMITTED => Some(Event::Admitted {
                child_id: payload.get("child_id")?.as_str()?.to_string(),
                parent_session: payload.get("parent_session")?.as_str()?.to_string(),
                session: payload.get("session")?.as_str()?.to_string(),
                chat: payload.get("chat")?.as_i64()?,
                goal: payload.get("goal")?.as_str()?.to_string(),
            }),
            CHILD_MESSAGE => Some(Event::Message {
                child_id: payload.get("child_id")?.as_str()?.to_string(),
                parent_session: payload.get("parent_session")?.as_str()?.to_string(),
                text: payload.get("text")?.as_str()?.to_string(),
            }),
            CHILD_RESULT => Some(Event::Result {
                child_id: payload.get("child_id")?.as_str()?.to_string(),
                parent_session: payload.get("parent_session")?.as_str()?.to_string(),
                status: payload.get("status")?.as_str()?.to_string(),
                result_path: payload.get("result_path")?.as_str()?.to_string(),
            }),
            _ => None,
        }
    }
}

/// Every child event in a log, in order. Unknown Meta types are skipped, which is what lets a newer
/// build write a type this reader has never heard of without breaking it.
pub fn events(entries: &[session_tree::Entry]) -> Vec<Event> {
    entries.iter().filter_map(Event::parse).collect()
}

/// Append one event to a session the caller already owns. Best effort at the call site: lifecycle
/// evidence never gates the work it describes.
pub fn append_event(log: &mut Log, event: &Event) -> Result<(), String> {
    log.append(Kind::Meta, event.payload())
        .map(|_| ())
        .map_err(|fault| fault.to_string())
}

/// Append an event to a session log by id, creating the log when it does not exist yet. Callers may
/// run on different threads and in different parts of the gateway, so the append goes through the one
/// per-session lock in [`crate::gateway::session`]: the session tree numbers an entry from the handle's
/// own length, and two handles that load at once compute the same next id.
pub fn record_event(state: &Path, session: &str, event: &Event) -> Result<(), String> {
    crate::gateway::session::append_meta(state, session, event.payload())
}

/// A child is finished when no executor will write to its record again.
fn terminal(state: &str) -> bool {
    matches!(state, STATE_DONE | STATE_FAILED | STATE_LOST)
}

/// The registry a gateway holds for its life. Cloning is another handle to the same registry, which is
/// what lets an executor thread update the record its admission created.
///
/// `new` is shared per state directory inside one process: the cap, the live count and the drain are
/// gateway-wide facts, and the `spawn` tool runs deep in `agent` where the gateway's handle cannot be
/// threaded through every call. Two `new` calls for the same `<state>` therefore name one registry.
#[derive(Clone)]
pub struct Registry {
    inner: Arc<Inner>,
}

struct Inner {
    state: PathBuf,
    /// Admitted ids whose executor has not finished. The cap counts this, so a child that exited is not
    /// still occupying a slot.
    live: Mutex<BTreeSet<String>>,
    /// The child that owns each session. A session names the child's log and artifact directory, so it
    /// belongs to one child for the life of the registry, even after that child ends. The on-disk
    /// records cover an earlier process; this map covers the window before the first record is written.
    sessions: Mutex<BTreeMap<String, String>>,
    /// Executors kept so a test or a shutdown can join them. Finished handles are pruned at the next
    /// admission; a thread cannot remove its own handle without joining itself.
    threads: Mutex<BTreeMap<String, std::thread::JoinHandle<()>>>,
    /// The mailbox a running child reads between steps. `tell` pushes here; the executor's loop drains
    /// it before its next model call. Kept beside the live set because both describe work in flight.
    mailboxes: Mutex<BTreeMap<String, Arc<Inbox>>>,
}

/// The registry map: one weak handle per state directory. Weak, not strong, so a registry dies with
/// its last holder and a finished gateway leaves no live count behind in a long-lived process.
fn known() -> &'static Mutex<BTreeMap<PathBuf, std::sync::Weak<Inner>>> {
    static REGISTRIES: std::sync::OnceLock<Mutex<BTreeMap<PathBuf, std::sync::Weak<Inner>>>> =
        std::sync::OnceLock::new();
    REGISTRIES.get_or_init(|| Mutex::new(BTreeMap::new()))
}

impl Registry {
    pub fn new(state: impl Into<PathBuf>) -> Registry {
        let state = state.into();
        let mut known = known().lock().unwrap_or_else(|held| held.into_inner());
        if let Some(inner) = known.get(&state).and_then(std::sync::Weak::upgrade) {
            return Registry { inner };
        }
        let inner = Arc::new(Inner {
            state: state.clone(),
            live: Mutex::new(BTreeSet::new()),
            sessions: Mutex::new(BTreeMap::new()),
            threads: Mutex::new(BTreeMap::new()),
            mailboxes: Mutex::new(BTreeMap::new()),
        });
        known.insert(state, Arc::downgrade(&inner));
        Registry { inner }
    }

    /// The gateway's registry for this state directory, if this process has one, without creating one.
    ///
    /// `spawn` uses this as its seam: a surface inside a gateway admits a child and returns a handle,
    /// and a surface with no gateway (`hades turn`, or a test) keeps the blocking path. It deliberately
    /// does not create a registry, because a surface that never had a gateway must not start threads
    /// behind its parent's back and leave them for nobody to own.
    pub fn existing(state: &Path) -> Option<Registry> {
        let known = known().lock().unwrap_or_else(|held| held.into_inner());
        known
            .get(state)
            .and_then(std::sync::Weak::upgrade)
            .map(|inner| Registry { inner })
    }

    pub fn state(&self) -> &Path {
        &self.inner.state
    }

    /// Startup recovery. A child that was `admitted` or `running` when its owning process died has no
    /// thread any more, so it is marked `lost` with a reason rather than left looking alive. A record
    /// owned by a live process is left alone: another gateway, or a second `serve` sharing this
    /// registry, still owns that child. Returns one bucket per file, whether or not it parsed.
    pub fn recover(&self) -> Result<Recovery, String> {
        let dir = children_dir(&self.inner.state);
        let mut report = Recovery::default();
        if !dir.exists() {
            return Ok(report);
        }
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
            .map_err(|error| format!("{}: {error}", dir.display()))?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "json")
            })
            .collect();
        paths.sort();
        for path in paths {
            report.files += 1;
            let Some(mut record) = read_record(&path) else {
                // One unreadable file is a fact to report, not a reason to abandon every other child.
                // It stays in place: guessing a state, or deleting the evidence, would be worse.
                report.unreadable.push(path.display().to_string());
                continue;
            };
            if record.state != STATE_ADMITTED && record.state != STATE_RUNNING {
                report.terminal += 1;
                continue;
            }
            match self.owner(&record) {
                Owner::ThisProcess => report.this_process += 1,
                Owner::Alive => report.elsewhere += 1,
                Owner::Gone => {
                    record.state = STATE_LOST.to_string();
                    record.ended = Some(epoch());
                    record.reason = Some(match record.pid {
                        Some(pid) => format!(
                            "the gateway restarted while this child was live (owner pid {pid} is gone)"
                        ),
                        None => "the gateway restarted while this child was live".to_string(),
                    });
                    match Record::write(&self.inner.state, &record) {
                        Ok(()) => report.marked += 1,
                        Err(said) => report.failed.push(format!("{}: {said}", path.display())),
                    }
                }
            }
        }
        Ok(report)
    }

    /// Who owns a record now. The live set is checked first so a second `serve` in one process can
    /// never mark its own children lost, even if the process table cannot be read.
    fn owner(&self, record: &Record) -> Owner {
        if self
            .inner
            .live
            .lock()
            .unwrap_or_else(|held| held.into_inner())
            .contains(&record.id)
        {
            return Owner::ThisProcess;
        }
        let Some(pid) = record.pid else {
            // A record from before ownership was recorded has no owner this build can find alive.
            return Owner::Gone;
        };
        let Some(start) = process_start(pid) else {
            return Owner::Gone;
        };
        // A pid can be reused; the start token is what says this is the process that wrote the record.
        if record.start.is_some_and(|recorded| recorded != start) {
            return Owner::Gone;
        }
        if pid == std::process::id() {
            Owner::ThisProcess
        } else {
            Owner::Alive
        }
    }

    /// The child that already owns a session, if any. The on-disk records cover every earlier child;
    /// the in-memory map covers an admission whose record is not written yet.
    fn session_owner(&self, session: &str) -> Result<Option<String>, String> {
        let dir = children_dir(&self.inner.state);
        if !dir.exists() {
            return Ok(None);
        }
        let entries =
            std::fs::read_dir(&dir).map_err(|error| format!("{}: {error}", dir.display()))?;
        for entry in entries.flatten() {
            let path = entry.path();
            if !path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                continue;
            }
            if let Some(record) = read_record(&path)
                && record.session == session
            {
                return Ok(Some(record.id));
            }
        }
        Ok(None)
    }

    /// Write the record, start the executor, and return the handle before the first model call. The
    /// order is the promise: a caller that gets a handle can read the record and the child's admitting
    /// event, and the cap counted this child before a thread existed.
    pub fn admit(
        &self,
        admission: Admission,
        mut world: World,
        client: OpenAi,
    ) -> Result<Admitted, Refusal> {
        // A handle whose executor is finished is not joinable work; drop it before it accumulates one
        // entry per child for the life of the process.
        self.prune_threads();
        if !valid_name(&admission.id) {
            return Err(Refusal::Io {
                path: path_of(&self.inner.state, &admission.id)
                    .display()
                    .to_string(),
                detail: "the child id is not a plain file name".to_string(),
            });
        }
        if !valid_name(&admission.session) {
            return Err(Refusal::Io {
                path: self.inner.state.join("sessions").display().to_string(),
                detail: format!(
                    "the session id {:?} is not a plain file name",
                    admission.session
                ),
            });
        }
        // The parent's session becomes a file name too, because the admission is recorded in the
        // parent's log. Refusing here keeps a bad id from writing outside the state directory, and
        // keeps the "walk the parent log" invariant from silently skipping a child.
        if !valid_name(&admission.parent) {
            return Err(Refusal::Io {
                path: self.inner.state.join("sessions").display().to_string(),
                detail: format!(
                    "the parent session {:?} is not a plain file name",
                    admission.parent
                ),
            });
        }
        let path = path_of(&self.inner.state, &admission.id);
        {
            let mut live = self
                .inner
                .live
                .lock()
                .unwrap_or_else(|held| held.into_inner());
            if live.len() >= MAX_LIVE {
                return Err(Refusal::Busy {
                    live: live.len(),
                    limit: MAX_LIVE,
                });
            }
            if live.contains(&admission.id) || path.exists() {
                return Err(Refusal::Exists { id: admission.id });
            }
            // A session names the child's log and its `result.json`, so it may belong to one child only.
            // The map covers this process's admissions; the records cover every earlier process.
            let mut sessions = self
                .inner
                .sessions
                .lock()
                .unwrap_or_else(|held| held.into_inner());
            if let Some(existing) = sessions.get(&admission.session) {
                return Err(Refusal::SessionInUse {
                    session: admission.session.clone(),
                    id: existing.clone(),
                });
            }
            let existing =
                self.session_owner(&admission.session)
                    .map_err(|detail| Refusal::Io {
                        path: children_dir(&self.inner.state).display().to_string(),
                        detail,
                    })?;
            if let Some(existing) = existing {
                return Err(Refusal::SessionInUse {
                    session: admission.session.clone(),
                    id: existing,
                });
            }
            sessions.insert(admission.session.clone(), admission.id.clone());
            live.insert(admission.id.clone());
        }

        let started = epoch();
        let result_path = result_path(&self.inner.state, &admission.session);
        let record = Record {
            id: admission.id.clone(),
            parent: admission.parent.clone(),
            session: admission.session.clone(),
            chat: admission.chat,
            state: STATE_ADMITTED.to_string(),
            result_path: result_path.display().to_string(),
            started,
            ended: None,
            reason: None,
            pid: Some(std::process::id()),
            start: process_start(std::process::id()),
        };
        let admitting = Event::Admitted {
            child_id: admission.id.clone(),
            parent_session: admission.parent.clone(),
            session: admission.session.clone(),
            chat: admission.chat,
            goal: admission.goal.clone(),
        };

        // The child's log and record exist before any model call. If either cannot be written, admission
        // is refused and no thread starts, so a handle never names a child with no evidence.
        if let Err(said) = record_event(&self.inner.state, &admission.session, &admitting)
            .and_then(|()| Record::write(&self.inner.state, &record))
        {
            self.unclaim_session(&admission.session);
            self.unslot(&admission.id);
            return Err(Refusal::Io {
                path: path.display().to_string(),
                detail: said,
            });
        }

        // The parent's log names the child before the child does anything. A failure here is fatal for
        // the admission: a child the parent's log does not name is exactly the orphan A5 forbids, so the
        // record is moved to `failed` with the reason rather than left looking live.
        if let Err(said) = record_event(&self.inner.state, &admission.parent, &admitting) {
            self.unslot(&admission.id);
            let _ = self.transition(
                &admission.id,
                STATE_FAILED,
                Some(epoch()),
                Some(said.clone()),
            );
            return Err(Refusal::Io {
                path: path.display().to_string(),
                detail: said,
            });
        }

        // A message can only reach a loop that is still running, so the mailbox belongs to the admission
        // and dies with the executor.
        let inbox = Arc::new(Inbox::default());
        world.inbox = Some(Arc::clone(&inbox));
        self.inner
            .mailboxes
            .lock()
            .unwrap_or_else(|held| held.into_inner())
            .insert(admission.id.clone(), inbox);

        let worker = self.clone();
        let id = admission.id.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("child-{id}"))
            .spawn(move || worker.execute(admission, world, client));
        match spawned {
            Ok(handle) => {
                self.inner
                    .threads
                    .lock()
                    .unwrap_or_else(|held| held.into_inner())
                    .insert(id.clone(), handle);
                Ok(Admitted {
                    child_id: id,
                    session: record.session,
                    state: STATE_ADMITTED.to_string(),
                })
            }
            Err(error) => {
                self.unslot(&id);
                let _ = self.transition(
                    &id,
                    STATE_FAILED,
                    Some(epoch()),
                    Some(format!("the executor thread could not start: {error}")),
                );
                Err(Refusal::Io {
                    path: path.display().to_string(),
                    detail: format!("the executor thread could not start: {error}"),
                })
            }
        }
    }

    pub fn record(&self, id: &str) -> Option<Record> {
        Record::load(&self.inner.state, id)
    }

    /// Every record, sorted by id. A record that will not parse is an error rather than a silent skip:
    /// `children` must not report a shorter list than the directory holds.
    pub fn records(&self) -> Result<Vec<Record>, String> {
        let dir = children_dir(&self.inner.state);
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
            .map_err(|error| format!("{}: {error}", dir.display()))?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "json")
            })
            .collect();
        paths.sort();
        paths
            .into_iter()
            .map(|path| {
                read_record(&path).ok_or_else(|| {
                    format!(
                        "{}: the child record would not parse; refusing to hide it",
                        path.display()
                    )
                })
            })
            .collect()
    }

    /// How many executors are admitted or running. This is the cutover number: a live child is in-flight
    /// work, not something a restart can finish.
    pub fn live(&self) -> usize {
        self.inner
            .live
            .lock()
            .unwrap_or_else(|held| held.into_inner())
            .len()
    }

    /// Hand one message to a child that is still running. The child reads it at its next step, not
    /// inside a call already in flight, so "delivered" means the mailbox has it and the loop has not
    /// yet ended. A terminal child refuses: a result already written cannot be influenced, and a
    /// message queued for nobody to read would be a promise this cannot keep.
    pub fn tell(&self, id: &str, text: &str) -> Result<Record, Refusal> {
        let record = self
            .record(id)
            .ok_or_else(|| Refusal::Missing { id: id.to_string() })?;
        if terminal(&record.state) {
            return Err(Refusal::Finished {
                id: id.to_string(),
                state: record.state,
            });
        }
        let inbox = self
            .inner
            .mailboxes
            .lock()
            .unwrap_or_else(|held| held.into_inner())
            .get(id)
            .map(Arc::clone);
        let Some(inbox) = inbox else {
            // The record says live but no executor holds a mailbox: this process did not admit it, or
            // its executor has just gone. Refusing is honest; the caller can read the record.
            return Err(Refusal::Missing { id: id.to_string() });
        };
        // The log says what was delivered before it is delivered, so a message the child acted on has
        // evidence even if the mailbox is drained at the same moment.
        record_event(
            &self.inner.state,
            &record.session,
            &Event::Message {
                child_id: id.to_string(),
                parent_session: record.parent.clone(),
                text: text.to_string(),
            },
        )
        .map_err(|detail| Refusal::Io {
            path: path_of(&self.inner.state, id).display().to_string(),
            detail,
        })?;
        inbox.push(text);
        Ok(record)
    }

    /// Wait for a child to reach a terminal state, up to `timeout`, and return its final record.
    ///
    /// This is its own clock on purpose. The run's `Bounds` are checked at graph edges and cannot
    /// interrupt a model call already in flight, so a deadline promised through them would be a
    /// promise this cannot keep; a wall-clock wait here ends when it says it will. The wait never
    /// kills the child: a timeout is a fact about the caller's patience, not about the work.
    pub fn wait(&self, id: &str, timeout: Duration) -> Result<Record, Refusal> {
        let started = Instant::now();
        let deadline = started + timeout;
        loop {
            let record = self
                .record(id)
                .ok_or_else(|| Refusal::Missing { id: id.to_string() })?;
            if terminal(&record.state) {
                return Ok(record);
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(Refusal::TimedOut {
                    id: id.to_string(),
                    state: record.state,
                    waited_ms: now.duration_since(started).as_millis(),
                });
            }
            // Polling, not a condition variable: the executor writes a record through the filesystem and
            // has no channel to wake a waiter, and a 25 ms tick is invisible next to a model call.
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// Free the slot and the mailbox for a child, whatever ended it. Idempotent, because an admission
    /// that fails after the slot is taken, an executor that finishes, and a thread that never starts
    /// all have to be able to say "this id is done" without stepping on each other.
    fn unslot(&self, id: &str) {
        self.inner
            .live
            .lock()
            .unwrap_or_else(|held| held.into_inner())
            .remove(id);
        self.inner
            .mailboxes
            .lock()
            .unwrap_or_else(|held| held.into_inner())
            .remove(id);
    }

    /// Release a session claim taken by an admission that never wrote its record, so a retry can reuse
    /// the session. A claim whose record exists is never released: the artifact path stays owned.
    fn unclaim_session(&self, session: &str) {
        self.inner
            .sessions
            .lock()
            .unwrap_or_else(|held| held.into_inner())
            .remove(session);
    }

    /// Drop handles whose executor has finished. The threads map is for joining live work, not an
    /// archive of every child this process ever ran.
    fn prune_threads(&self) {
        self.inner
            .threads
            .lock()
            .unwrap_or_else(|held| held.into_inner())
            .retain(|_, handle| !handle.is_finished());
    }

    /// Join every executor this gateway started and return how many were joined. Used by tests and by a
    /// shutdown that wants the threads gone; it is not a way to make a slow model call finish sooner.
    pub fn drain(&self) -> usize {
        let handles: Vec<std::thread::JoinHandle<()>> = {
            let mut threads = self
                .inner
                .threads
                .lock()
                .unwrap_or_else(|held| held.into_inner());
            std::mem::take(&mut *threads).into_values().collect()
        };
        let joined = handles.len();
        for handle in handles {
            let _ = handle.join();
        }
        joined
    }

    /// The executor body. A panic becomes a failed record rather than a record stuck in `running`
    /// forever, because a parent waiting on `result` needs a terminal state either way.
    fn execute(&self, admission: Admission, world: World, client: OpenAi) {
        let id = admission.id.clone();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.run(&admission, &world, &client)
        }));
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(said)) => {
                let _ = self.transition(&id, STATE_FAILED, Some(epoch()), Some(said));
            }
            Err(_) => {
                let _ = self.transition(
                    &id,
                    STATE_FAILED,
                    Some(epoch()),
                    Some("the child panicked".to_string()),
                );
            }
        }
        self.unslot(&id);
    }

    fn run(&self, admission: &Admission, world: &World, client: &OpenAi) -> Result<(), String> {
        self.transition(&admission.id, STATE_RUNNING, None, None)?;
        let mut messages = child_messages(&admission.goal, &admission.schema);
        let silent = |_: Feed, _: &str| {};
        let nothing = |_: Activity, _: &str| {};
        let answer = crate::agent::answer(
            client,
            &mut messages,
            world,
            &silent,
            &nothing,
            admission.steps,
        );
        let (status, result, raw) = match answer {
            Ok(text) => match parse_json_answer(&text) {
                Some(value) => match fits(&admission.schema, &value) {
                    Ok(()) => ("ok".to_string(), Some(value), text),
                    Err(why) => ("invalid".to_string(), None, format!("{why}\n{text}")),
                },
                None => (
                    "invalid".to_string(),
                    None,
                    format!("the child did not return a JSON object\n{text}"),
                ),
            },
            Err(fault) => ("failed".to_string(), None, format!("{fault:?}")),
        };

        let result_path = result_path(&self.inner.state, &admission.session);
        write_result(&result_path, &status, &result, &raw)?;
        let ended = Event::Result {
            child_id: admission.id.clone(),
            parent_session: admission.parent.clone(),
            status: status.clone(),
            result_path: result_path.display().to_string(),
        };
        // The parent's log learns the outcome from the child's own thread. That is the append A5 has to
        // serialize: two children finishing at once would otherwise load the same parent log and both
        // write entry id N.
        let _ = record_event(&self.inner.state, &admission.session, &ended);
        let _ = record_event(&self.inner.state, &admission.parent, &ended);

        let state = if status == "ok" {
            STATE_DONE
        } else {
            STATE_FAILED
        };
        let reason = (status != "ok").then(|| {
            raw.lines()
                .next()
                .unwrap_or("the child failed without a reason")
                .to_string()
        });
        self.transition(&admission.id, state, Some(epoch()), reason)
    }

    fn transition(
        &self,
        id: &str,
        state: &str,
        ended: Option<i64>,
        reason: Option<String>,
    ) -> Result<(), String> {
        let mut record = Record::load(&self.inner.state, id)
            .ok_or_else(|| format!("child {id} has no record to transition"))?;
        record.state = state.to_string();
        record.ended = ended;
        record.reason = reason;
        Record::write(&self.inner.state, &record)
    }
}

/// Where the registry lives.
pub fn children_dir(state: &Path) -> PathBuf {
    state.join("children")
}

/// One child's record. The id is validated before this is called; the path is still the single place
/// that decides the shape.
pub fn path_of(state: &Path, id: &str) -> PathBuf {
    children_dir(state).join(format!("{id}.json"))
}

fn result_path(state: &Path, session: &str) -> PathBuf {
    state.join("artifacts").join(session).join("result.json")
}

/// An id becomes a file name in two places (the record and the session log), so it may only be one
/// path segment. Nothing generates a slash here; refusing one keeps a bad id from escaping the state
/// directory.
fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

/// Who owns a child record: this gateway process, another live process, or a process that is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Owner {
    ThisProcess,
    Alive,
    Gone,
}

/// A process's start time from `/proc/<pid>/stat`: the token that tells a live pid from a reused one.
/// `None` means there is no such process (or it has no readable stat entry). Linux is the platform this
/// gateway runs on; other platforms can only vouch for their own pid.
#[cfg(target_os = "linux")]
fn process_start(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_name = &stat[stat.rfind(')')? + 1..];
    // After the parenthesised name, fields start at field 3 (state); starttime is field 22.
    after_name.split_whitespace().nth(19)?.parse().ok()
}

#[cfg(not(target_os = "linux"))]
fn process_start(pid: u32) -> Option<u64> {
    (pid == std::process::id()).then_some(0)
}

fn read_record(path: &Path) -> Option<Record> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    Record::from_value(&value)
}

/// Write through a temporary file in the same directory so a reader never sees half a record.
fn write_atomic(path: &Path, value: &Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(format!(".tmp-{}", std::process::id()));
    let temporary = PathBuf::from(temporary);
    std::fs::write(&temporary, value.to_string())
        .map_err(|error| format!("{}: {error}", temporary.display()))?;
    std::fs::rename(&temporary, path).map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        format!("{}: {error}", path.display())
    })
}

fn write_result(
    path: &Path,
    status: &str,
    result: &Option<Value>,
    raw: &str,
) -> Result<(), String> {
    write_atomic(
        path,
        &json!({"status": status, "result": result, "raw": raw}),
    )
}

/// What a child is told. The blocking path's prompt is the contract; this function exists so the
/// gateway executor and `spawn`/`delegate` cannot drift into two prompts for one child.
fn child_messages(goal: &str, schema: &Value) -> Vec<Value> {
    vec![
        json!({"role": "system", "content": child_prompt(schema)}),
        json!({"role": "user", "content": goal}),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use session_tree::SessionId;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::process::Command;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    fn scratch(name: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or(0);
        let base = std::env::temp_dir().join(format!(
            "hades-children-{name}-{}-{stamp}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("scratch");
        base
    }

    fn cleanup(state: &Path) {
        let _ = std::fs::remove_dir_all(state);
    }

    fn world(state: &Path) -> crate::agent::World {
        crate::agent::World {
            root: state.join("work"),
            vault: state.join("vault"),
            db: state.join("mneme.db"),
            skills: Vec::new(),
            sessions: vec![state.join("sessions")],
            state: state.to_path_buf(),
            chat: 7,
            depth: 1,
            mode: crate::mode::Mode::Read,
            role: crate::profile::Role::Patch,
            actor: "chat:7".to_string(),
            session: "parent-session".to_string(),
            inbox: None,
            mcp: Arc::new(crate::mcp::Mcp::empty()),
            allowance: crate::agent::Allowance::new(8),
        }
    }

    fn admission(id: &str) -> Admission {
        Admission {
            id: id.to_string(),
            parent: "parent-session".to_string(),
            session: id.to_string(),
            chat: 7,
            goal: "say done".to_string(),
            schema: Admission::report_schema(),
            steps: 4,
        }
    }

    fn record(id: &str, state: &str) -> Record {
        Record {
            id: id.to_string(),
            parent: "parent-session".to_string(),
            session: id.to_string(),
            chat: 7,
            state: state.to_string(),
            result_path: format!("/nonexistent/{id}/result.json"),
            started: 10,
            ended: None,
            reason: None,
            pid: None,
            start: None,
        }
    }

    /// A record an identified process wrote: the shape this build writes, and the shape recovery has to
    /// judge by ownership rather than by its own opinion about whether a thread exists.
    fn owned(id: &str, state: &str, pid: u32, start: Option<u64>) -> Record {
        Record {
            pid: Some(pid),
            start,
            ..record(id, state)
        }
    }

    fn unreachable() -> crate::model::OpenAi {
        crate::model::OpenAi::new("http://127.0.0.1:9/v1/chat/completions", "stub")
    }

    type Stub = (
        String,
        mpsc::Receiver<(Option<String>, bool)>,
        mpsc::Sender<()>,
        std::thread::JoinHandle<()>,
    );

    /// A model endpoint that reports what the registry looked like at the moment it was called, then
    /// waits for the test before answering. That ordering is the thing under test: a record written
    /// after the call cannot be observed from inside the call.
    fn model_server(state: PathBuf, id: String, content: String) -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
        let url = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().expect("an address")
        );
        let (entered, observed) = mpsc::channel();
        let (go, wait) = mpsc::channel::<()>();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("a connection");
            let mut reader = BufReader::new(stream.try_clone().expect("a clone"));
            let mut length = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("a header");
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; length];
            reader.read_exact(&mut body).expect("a body");
            let request: Value = serde_json::from_slice(&body).expect("a request");
            let goal = request["messages"].as_array().is_some_and(|messages| {
                messages.iter().any(|message| {
                    message["content"]
                        .as_str()
                        .is_some_and(|text| text.contains("say done"))
                })
            });
            let at_call = Record::load(&state, &id).map(|record| record.state);
            entered
                .send((at_call, goal))
                .expect("the test is listening");
            wait.recv_timeout(Duration::from_secs(10)).expect("the go");
            let frame = json!({"choices": [{"delta": {"content": content}}]}).to_string();
            let body = format!("data: {frame}\n\ndata: [DONE]\n\n");
            let mut out = stream;
            write!(
                out,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .expect("a response");
        });
        (url, observed, go, server)
    }

    fn wait_for(registry: &Registry, id: &str, state: &str) -> Record {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let record = registry.record(id).expect("the child record");
            if record.state == state {
                return record;
            }
            assert!(
                Instant::now() < deadline,
                "the child stayed {:?}, not {state}",
                record.state
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A0: the registry file is written and the executor is in its first model call before the handle
    /// would have any answer to return. The endpoint itself is the witness: it reads the record while
    /// the call is in flight.
    #[test]
    fn a_child_record_exists_before_the_model_is_called() {
        let state = scratch("before-call");
        let (url, observed, go, server) = model_server(
            state.clone(),
            "spawn-one".to_string(),
            json!({"report": "done"}).to_string(),
        );
        let registry = Registry::new(&state);
        let admitted = registry
            .admit(
                admission("spawn-one"),
                world(&state),
                crate::model::OpenAi::new(&url, "stub"),
            )
            .expect("an admitted child");
        assert_eq!(admitted.child_id, "spawn-one");
        assert_eq!(admitted.session, "spawn-one");
        assert_eq!(admitted.state, "admitted");
        assert!(
            registry.record("spawn-one").is_some(),
            "the handle has a record"
        );

        let (at_call, goal_reached) = observed
            .recv_timeout(Duration::from_secs(10))
            .expect("the first model call");
        assert_eq!(
            at_call.as_deref(),
            Some("running"),
            "the record said running before the endpoint was called"
        );
        assert!(goal_reached, "the goal reaches the child's prompt");
        go.send(()).expect("the endpoint is waiting");

        let done = wait_for(&registry, "spawn-one", "done");
        assert_eq!(registry.drain(), 1);
        server.join().expect("the endpoint");
        assert!(done.ended.is_some(), "a finished child has an end");

        let result: Value =
            serde_json::from_str(&std::fs::read_to_string(&done.result_path).expect("a result"))
                .expect("result JSON");
        assert_eq!(result["status"], "ok");
        assert_eq!(result["result"]["report"], "done");

        let store = FileStore::new(state.join("sessions"));
        let log = Session::load(store, SystemClock, SessionId::new("spawn-one")).expect("the log");
        let names: Vec<&str> = events(log.entries())
            .iter()
            .map(|event| event.type_name())
            .collect();
        assert_eq!(names, ["child_admitted", "child_result"]);
        cleanup(&state);
    }

    /// A0: a process that dies mid-child leaves records behind. The next start marks them lost, with a
    /// reason and an end, and does not start an executor for them.
    #[test]
    fn a_gateway_restart_marks_live_children_lost_and_does_not_resume_them() {
        let state = scratch("recovery");
        Record::write(&state, &record("spawn-admitted", "admitted")).expect("a record");
        Record::write(&state, &record("spawn-running", "running")).expect("a record");
        let mut finished = record("spawn-done", "done");
        finished.ended = Some(20);
        Record::write(&state, &finished).expect("a record");

        let registry = Registry::new(&state);
        let report = registry.recover().expect("recovery");
        assert_eq!(report.marked, 2, "two were live");
        assert_eq!(report.files, 3, "every file is accounted for");
        assert_eq!(report.accounted(), report.files);

        for id in ["spawn-admitted", "spawn-running"] {
            let lost = registry.record(id).expect("the record");
            assert_eq!(lost.state, "lost");
            assert!(lost.ended.is_some());
            assert!(
                lost.reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("gateway restarted")),
                "a lost child says why: {:?}",
                lost.reason
            );
            assert!(
                !Path::new(&lost.result_path).exists(),
                "nothing was resumed"
            );
        }
        assert_eq!(registry.record("spawn-done"), Some(finished));
        assert_eq!(registry.live(), 0);
        assert_eq!(registry.drain(), 0);
        cleanup(&state);
    }

    /// A0 fix: one file that will not parse is reported, not fatal. Every other record is still
    /// recovered, and the report accounts for every file in the directory.
    #[test]
    fn recovery_reports_a_bad_record_and_keeps_going() {
        let state = scratch("recovery-unreadable");
        Record::write(&state, &record("spawn-lost", "running")).expect("a record");
        let mut finished = record("spawn-done", "done");
        finished.ended = Some(20);
        Record::write(&state, &finished).expect("a record");
        std::fs::write(children_dir(&state).join("spawn-bad.json"), "{not json")
            .expect("a bad record");

        let registry = Registry::new(&state);
        let report = registry.recover().expect("recovery keeps going");

        assert_eq!(report.files, 3);
        assert_eq!(
            report.marked, 1,
            "the record after the bad file is recovered"
        );
        assert_eq!(report.terminal, 1);
        assert_eq!(report.unreadable.len(), 1);
        assert!(
            report.unreadable[0].ends_with("spawn-bad.json"),
            "the report names the file it could not read: {:?}",
            report.unreadable
        );
        assert_eq!(report.accounted(), report.files);
        assert_eq!(
            registry.record("spawn-lost").expect("the record").state,
            "lost"
        );
        assert!(
            children_dir(&state).join("spawn-bad.json").exists(),
            "an unreadable record is left for a person, not deleted"
        );
        cleanup(&state);
    }

    /// A0 fix: a child whose gateway process is still alive is not lost. Recovery is about dead
    /// owners, not about this process's inability to find a thread for another process's child.
    #[test]
    fn recovery_keeps_a_child_whose_owner_is_alive() {
        let state = scratch("recovery-owner-alive");
        let mut owner = Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("a live process");
        let pid = owner.id();
        let start = process_start(pid).expect("a start token for a live process");
        Record::write(&state, &owned("spawn-alive", "running", pid, Some(start)))
            .expect("a record");

        let registry = Registry::new(&state);
        let report = registry.recover().expect("recovery");

        assert_eq!(report.marked, 0);
        assert_eq!(report.elsewhere, 1);
        assert_eq!(
            registry.record("spawn-alive").expect("the record").state,
            "running",
            "a live gateway still owns this child"
        );

        let _ = owner.kill();
        let _ = owner.wait();
        cleanup(&state);
    }

    /// A0 fix: a second `serve` in one process shares the same registry. Recovery must never mark that
    /// registry's own live children lost, even though it has no idea which thread they belong to.
    #[test]
    fn recovery_keeps_this_registrys_own_live_children() {
        let state = scratch("recovery-own");
        let registry = Registry::new(&state);
        let start = process_start(std::process::id()).expect("a start token for this process");
        Record::write(
            &state,
            &owned("spawn-own", "running", std::process::id(), Some(start)),
        )
        .expect("a record");
        registry
            .inner
            .live
            .lock()
            .expect("the live set")
            .insert("spawn-own".to_string());

        let second = Registry::new(&state);
        let report = second.recover().expect("recovery");

        assert_eq!(report.marked, 0);
        assert_eq!(report.this_process, 1);
        assert_eq!(
            second.record("spawn-own").expect("the record").state,
            "running"
        );
        registry
            .inner
            .live
            .lock()
            .expect("the live set")
            .remove("spawn-own");
        cleanup(&state);
    }

    /// A0 fix: the owner's pid is not enough, because pids are reused. The start token has to match,
    /// and a record whose owner is gone becomes lost with a reason.
    #[test]
    fn recovery_marks_a_child_whose_owner_pid_is_gone_lost() {
        let state = scratch("recovery-owner-gone");
        let mut dead = Command::new("true").spawn().expect("a short process");
        let pid = dead.id();
        dead.wait().expect("the short process ends");
        Record::write(
            &state,
            &owned("spawn-dead", "admitted", pid, Some(u64::MAX)),
        )
        .expect("a record");

        let registry = Registry::new(&state);
        let report = registry.recover().expect("recovery");

        assert_eq!(report.marked, 1);
        let lost = registry.record("spawn-dead").expect("the record");
        assert_eq!(lost.state, "lost");
        assert!(
            lost.reason
                .as_deref()
                .is_some_and(|reason| reason.contains("gateway restarted")),
            "{:?}",
            lost.reason
        );
        cleanup(&state);
    }

    /// A0 fix: the artifact path is derived from the session, so two children may not share one. A
    /// session already named by a record is refused even when that child is finished.
    #[test]
    fn an_admission_whose_session_is_already_owned_is_refused() {
        let state = scratch("session-reuse");
        let mut first = record("spawn-first", "done");
        first.session = "shared-session".to_string();
        first.ended = Some(20);
        Record::write(&state, &first).expect("a record");
        let registry = Registry::new(&state);

        let mut second = admission("spawn-second");
        second.session = "shared-session".to_string();
        let refused = registry.admit(second, world(&state), unreachable());

        assert_eq!(
            refused,
            Err(Refusal::SessionInUse {
                session: "shared-session".to_string(),
                id: "spawn-first".to_string(),
            })
        );
        assert!(
            registry.record("spawn-second").is_none(),
            "a refused admission writes no record"
        );
        assert_eq!(registry.drain(), 0);
        cleanup(&state);
    }

    /// A0 fix: a live child holds its session too, including before its executor writes anything else.
    #[test]
    fn a_live_child_holds_its_session() {
        let state = scratch("session-live");
        let registry = Registry::new(&state);
        let mut first = admission("spawn-first");
        first.session = "shared-session".to_string();
        registry
            .admit(first, world(&state), unreachable())
            .expect("the first child");

        let mut second = admission("spawn-second");
        second.session = "shared-session".to_string();
        let refused = registry.admit(second, world(&state), unreachable());

        assert_eq!(
            refused,
            Err(Refusal::SessionInUse {
                session: "shared-session".to_string(),
                id: "spawn-first".to_string(),
            })
        );
        assert_eq!(registry.drain(), 1);
        cleanup(&state);
    }

    /// A0: the cap is gateway-wide and refuses before a thread exists, rather than starting an
    /// unbounded number of them.
    #[test]
    fn the_cap_refuses_admission_before_a_seventeenth_thread_starts() {
        let state = scratch("cap");
        let registry = Registry::new(&state);
        {
            let mut live = registry.inner.live.lock().expect("the live set");
            for index in 0..MAX_LIVE {
                live.insert(format!("held-{index}"));
            }
        }
        let refused = registry.admit(
            admission("spawn-one"),
            world(&state),
            crate::model::OpenAi::new("http://127.0.0.1:9/v1/chat/completions", "stub"),
        );
        assert_eq!(
            refused,
            Err(Refusal::Busy {
                live: MAX_LIVE,
                limit: MAX_LIVE
            })
        );
        assert!(
            registry.record("spawn-one").is_none(),
            "a refused admission writes no record"
        );
        assert_eq!(registry.drain(), 0);
        cleanup(&state);
    }

    /// A0: the cap is gateway-wide, so a second handle for the same state directory sees the same live
    /// children rather than starting a second count.
    #[test]
    fn two_handles_for_one_state_directory_are_one_gateway_registry() {
        let state = scratch("shared");
        let first = Registry::new(&state);
        let second = Registry::new(&state);
        {
            let mut live = first.inner.live.lock().expect("the live set");
            live.insert("held".to_string());
        }
        assert_eq!(second.live(), 1, "the second handle sees the live child");
        first
            .inner
            .live
            .lock()
            .expect("the live set")
            .remove("held");
        assert_eq!(second.live(), 0);
        cleanup(&state);
    }

    /// A1: a Meta type written by a newer build survives a load and is skipped by a reader that only
    /// knows today's child events. The schema version does not move for this.
    #[test]
    fn a_meta_type_this_build_does_not_know_round_trips_and_is_ignored() {
        let state = scratch("unknown-type");
        let store = FileStore::new(state.join("sessions"));
        let mut log = Session::create(store.clone(), SystemClock, SessionId::new("spawn-meta"))
            .expect("a log");
        let unknown = json!({"type": "child_teleported", "child_id": "c1", "extra": {"x": 1}});
        log.append(Kind::Meta, unknown.clone()).expect("an entry");
        let admitted = Event::Admitted {
            child_id: "c1".to_string(),
            parent_session: "parent-session".to_string(),
            session: "spawn-meta".to_string(),
            chat: 7,
            goal: "say done".to_string(),
        };
        log.append(Kind::Meta, admitted.payload())
            .expect("an entry");

        let reloaded =
            Session::load(store, SystemClock, SessionId::new("spawn-meta")).expect("the log");
        assert_eq!(
            reloaded.entries()[1].payload,
            unknown,
            "the store keeps a type it does not understand"
        );
        assert_eq!(
            Event::parse(&reloaded.entries()[1]),
            None,
            "an unknown type is not a child event"
        );
        assert_eq!(events(reloaded.entries()), vec![admitted]);
        assert_eq!(session_tree::SCHEMA_VERSION, 1, "no schema bump for Meta");
        cleanup(&state);
    }

    /// A1: the three lifecycle records name themselves with one discriminator, and the message event
    /// A3 needs is already representable.
    #[test]
    fn every_child_event_names_its_type() {
        let events = [
            Event::Admitted {
                child_id: "c1".to_string(),
                parent_session: "p".to_string(),
                session: "c1".to_string(),
                chat: 7,
                goal: "g".to_string(),
            },
            Event::Message {
                child_id: "c1".to_string(),
                parent_session: "p".to_string(),
                text: "keep going".to_string(),
            },
            Event::Result {
                child_id: "c1".to_string(),
                parent_session: "p".to_string(),
                status: "ok".to_string(),
                result_path: "/state/artifacts/c1/result.json".to_string(),
            },
        ];
        assert_eq!(events[0].payload()["type"], "child_admitted");
        assert_eq!(events[1].payload()["type"], "child_message");
        assert_eq!(events[2].payload()["type"], "child_result");
        assert_eq!(events[1].payload()["parent_session"], "p");
    }

    /// A0 fix: the gateway executor and the blocking path must hand the model one contract. The prompt
    /// is the blocking path's, not a second paraphrase that can drift from it.
    #[test]
    fn the_gateway_child_uses_the_blocking_paths_worker_prompt() {
        let schema = Admission::report_schema();
        let messages = child_messages("say done", &schema);

        assert_eq!(
            messages[0]["content"],
            crate::agent::child_prompt(&schema),
            "a child sees the same system prompt wherever it was created"
        );
        let system = messages[0]["content"].as_str().expect("a system message");
        assert!(system.contains("You are one worker in a team."), "{system}");
        assert!(system.contains("Return one JSON object"), "{system}");
        assert_eq!(messages[1]["content"], "say done");
    }

    /// A0 fix: the threads map is for joining live work, not an archive. A finished handle is pruned
    /// when the next child is admitted, so a long-lived gateway does not keep one handle per child.
    #[test]
    fn a_finished_handle_is_pruned_before_the_next_child() {
        let state = scratch("thread-prune");
        let (url, observed, go, server) = model_server(
            state.clone(),
            "spawn-first".to_string(),
            json!({"report": "done"}).to_string(),
        );
        let registry = Registry::new(&state);
        registry
            .admit(
                admission("spawn-first"),
                world(&state),
                crate::model::OpenAi::new(&url, "stub"),
            )
            .expect("the first child");
        let (_, _) = observed
            .recv_timeout(Duration::from_secs(10))
            .expect("the first model call");
        go.send(()).expect("the endpoint is waiting");
        wait_for(&registry, "spawn-first", "done");
        server.join().expect("the endpoint");
        assert_eq!(
            registry.inner.threads.lock().expect("the threads").len(),
            1,
            "the finished handle is still there to be joined"
        );

        registry
            .admit(admission("spawn-second"), world(&state), unreachable())
            .expect("the second child");
        assert_eq!(
            registry.inner.threads.lock().expect("the threads").len(),
            1,
            "the finished handle was pruned at admission"
        );
        assert_eq!(registry.drain(), 1);
        cleanup(&state);
    }
}
