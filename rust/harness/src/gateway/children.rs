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

use serde_json::{Value, json};
use session_tree::{FileStore, Kind, Session, SessionId, SystemClock};

use crate::agent::{Activity, World};
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
        })
    }

    /// Strict: a record missing a field an owner relies on is not a record. `ended` and `reason` are
    /// nullable because a live child has neither.
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

/// Why admission was refused. `Busy` is the one a tool reports as a typed busy fault; the other two are
/// refusals to overwrite or to invent a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    Busy { live: usize, limit: usize },
    Exists { id: String },
    Io { path: String, detail: String },
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Busy { live, limit } => write!(
                f,
                "the gateway is already running {live} children (limit {limit}); try again when one finishes"
            ),
            Refusal::Exists { id } => write!(f, "child {id} already exists"),
            Refusal::Io { path, detail } => write!(f, "{path}: {detail}"),
        }
    }
}

impl std::error::Error for Refusal {}

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

/// Append an event to a session log by id, creating the log when it does not exist yet. A caller
/// appending to a shared parent log must serialize its own writers; the session tree caches entry ids
/// per handle and two concurrent loads can compute the same next id.
pub fn record_event(state: &Path, session: &str, event: &Event) -> Result<(), String> {
    let mut log = open_log(state, session)?;
    append_event(&mut log, event)
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
    /// Executors kept so a test or a shutdown can join them. A thread cannot remove its own handle
    /// without joining itself, so finished ids stay here until `drain`.
    threads: Mutex<BTreeMap<String, std::thread::JoinHandle<()>>>,
}

impl Registry {
    pub fn new(state: impl Into<PathBuf>) -> Registry {
        let state = state.into();
        static REGISTRIES: std::sync::OnceLock<Mutex<BTreeMap<PathBuf, std::sync::Weak<Inner>>>> =
            std::sync::OnceLock::new();
        let known = REGISTRIES.get_or_init(|| Mutex::new(BTreeMap::new()));
        let mut known = known.lock().unwrap_or_else(|held| held.into_inner());
        if let Some(inner) = known.get(&state).and_then(std::sync::Weak::upgrade) {
            return Registry { inner };
        }
        let inner = Arc::new(Inner {
            state: state.clone(),
            live: Mutex::new(BTreeSet::new()),
            threads: Mutex::new(BTreeMap::new()),
        });
        known.insert(state, Arc::downgrade(&inner));
        Registry { inner }
    }

    pub fn state(&self) -> &Path {
        &self.inner.state
    }

    /// Startup recovery. A child that was `admitted` or `running` when the process died has no thread
    /// any more, so it is marked `lost` with a reason rather than left looking alive. Returns how many
    /// records changed.
    pub fn recover(&self) -> Result<usize, String> {
        let dir = children_dir(&self.inner.state);
        if !dir.exists() {
            return Ok(0);
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
        let mut marked = 0;
        for path in paths {
            let mut record = read_record(&path).ok_or_else(|| {
                format!(
                    "{}: the child record would not parse; refusing to guess its state",
                    path.display()
                )
            })?;
            if record.state == STATE_ADMITTED || record.state == STATE_RUNNING {
                record.state = STATE_LOST.to_string();
                record.ended = Some(epoch());
                record.reason = Some("the gateway restarted while this child was live".to_string());
                Record::write(&self.inner.state, &record)?;
                marked += 1;
            }
        }
        Ok(marked)
    }

    /// Write the record, start the executor, and return the handle before the first model call. The
    /// order is the promise: a caller that gets a handle can read the record and the child's admitting
    /// event, and the cap counted this child before a thread existed.
    pub fn admit(
        &self,
        admission: Admission,
        world: World,
        client: OpenAi,
    ) -> Result<Admitted, Refusal> {
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
            self.inner
                .live
                .lock()
                .unwrap_or_else(|held| held.into_inner())
                .remove(&admission.id);
            return Err(Refusal::Io {
                path: path.display().to_string(),
                detail: said,
            });
        }

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
                self.inner
                    .live
                    .lock()
                    .unwrap_or_else(|held| held.into_inner())
                    .remove(&id);
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
        self.inner
            .live
            .lock()
            .unwrap_or_else(|held| held.into_inner())
            .remove(&id);
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
        if let Ok(mut log) = open_log(&self.inner.state, &admission.session) {
            let _ = append_event(
                &mut log,
                &Event::Result {
                    child_id: admission.id.clone(),
                    parent_session: admission.parent.clone(),
                    status: status.clone(),
                    result_path: result_path.display().to_string(),
                },
            );
        }

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

/// Open a session log, creating it when the file does not exist. A corrupt log is an error, not a
/// reason to overwrite it: append-only history is never clobbered.
fn open_log(state: &Path, session: &str) -> Result<Log, String> {
    let store = FileStore::new(state.join("sessions"));
    match Session::load(store.clone(), SystemClock, SessionId::new(session)) {
        Ok(log) => Ok(log),
        Err(session_tree::Fault::Io { .. }) => {
            Session::create(store, SystemClock, SessionId::new(session))
                .map_err(|fault| fault.to_string())
        }
        Err(fault) => Err(fault.to_string()),
    }
}

/// What a child is told. Close to the blocking spawn path's prompt, because A4 compares the two paths'
/// results for the same scripted child.
fn child_messages(goal: &str, schema: &Value) -> Vec<Value> {
    let system = format!(
        "You are one child agent in a team. You have tools: shell, files, search, the web, memory, \
         skills, sessions and images. Do the task with them instead of asking about it. You cannot see \
         the conversation that sent you this task, so treat the task text as everything you know. \
         Return one JSON object and nothing else. It must satisfy this schema:\n{}\nDo not put prose \
         around it. Do not use Markdown fences. A missing or mistyped field fails the task.",
        serde_json::to_string_pretty(schema).unwrap_or_else(|_| "{}".to_string())
    );
    vec![
        json!({"role": "system", "content": system}),
        json!({"role": "user", "content": goal}),
    ]
}

/// The object a child meant to return. A fenced block or a sentence before the JSON is tolerated,
/// because models decorate; the first `{` to the last `}` is the last honest attempt.
fn parse_json_answer(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    if let Ok(value) = serde_json::from_str(trimmed) {
        return Some(value);
    }
    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    if end <= start {
        return None;
    }
    serde_json::from_str(&trimmed[start..=end]).ok()
}

/// A small structural check, the same contract the blocking path enforces: object, array, string,
/// number, integer, boolean, null, required keys and declared properties. A bigger engine would be a
/// dependency for a claim this one can test. Kept here because the gateway executor and the blocking
/// path must agree on what a valid child result is.
fn fits(schema: &Value, value: &Value) -> Result<(), String> {
    let kind = schema.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        "object" => {
            let object = value
                .as_object()
                .ok_or_else(|| "expected an object".to_string())?;
            if let Some(required) = schema.get("required").and_then(Value::as_array) {
                for key in required.iter().filter_map(Value::as_str) {
                    if !object.contains_key(key) {
                        return Err(format!("missing required field `{key}`"));
                    }
                }
            }
            if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
                for (key, sub) in properties {
                    if let Some(field) = object.get(key) {
                        fits(sub, field).map_err(|why| format!("{key}: {why}"))?;
                    }
                }
            }
            Ok(())
        }
        "array" => {
            let items = value
                .as_array()
                .ok_or_else(|| "expected an array".to_string())?;
            if let Some(item) = schema.get("items") {
                for (index, entry) in items.iter().enumerate() {
                    fits(item, entry).map_err(|why| format!("[{index}]: {why}"))?;
                }
            }
            Ok(())
        }
        "string" if value.is_string() => Ok(()),
        "number" if value.is_number() => Ok(()),
        "integer" if value.as_i64().is_some() || value.as_u64().is_some() => Ok(()),
        "boolean" if value.is_boolean() => Ok(()),
        "null" if value.is_null() => Ok(()),
        "" => Ok(()),
        other => Err(format!("expected {other}, got {}", kind_of(value))),
    }
}

fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
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
        }
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
        assert_eq!(registry.recover().expect("recovery"), 2, "two were live");

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
}
