//! A standing goal, and the heartbeat that continues it.
//!
//! A goal is session state, not a cron job. It names the chat and the session it belongs to, so a
//! continuation lands where the person was talking instead of opening a new place to report. The file
//! under `<state>/goals` is the record a heartbeat reads after a restart; every state change is also a
//! Meta entry in the goal's session, with a `type` discriminator so a reader that has never heard of
//! goals ignores it and `SCHEMA_VERSION` stays 1.
//!
//! The heartbeat is bounded twice. It runs only when the session has no live job and the last entry is
//! older than the interval, and each continuation draws a fresh [`crate::agent::Allowance`] of the goal's
//! budget. A goal that spends that allowance is paused with `budget` recorded; it is never retried
//! quietly.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use session_tree::{FileStore, Kind, Session, SessionId, SystemClock};

use crate::gateway::{Settings, chat_path, load_chat, save_chat, system_prompt, world_for};
use crate::model::{OpenAi, from_preset, preset};
use crate::task::TaskSpec;
use crate::verify::{CommandVerifier, Verifier};

/// The model calls one continuation may spend when a goal does not name a budget. It is per continuation,
/// not per goal: a goal that keeps making progress is allowed to keep going, and a goal that burns the
/// whole allowance in one turn is paused rather than handed another.
pub const DEFAULT_BUDGET: u32 = 8;

/// The idle interval before the heartbeat re-enters a session, in seconds. Fifteen minutes is long
/// enough that a conversation is over and short enough that a person notices the goal is still alive.
pub const DEFAULT_HEARTBEAT_SECS: i64 = 900;

/// How often the heartbeat thread looks for due goals. The interval is measured in seconds; this is
/// only how promptly a due goal is noticed.
pub const TICK_SECS: u64 = 30;

/// Where a goal's life cycle stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Open,
    Paused,
    Done,
    Cleared,
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            State::Open => "open",
            State::Paused => "paused",
            State::Done => "done",
            State::Cleared => "cleared",
        }
    }

    /// `None` for anything unrecognised, so a file is reported rather than guessed at.
    pub fn parse(text: &str) -> Option<State> {
        Some(match text {
            "open" => State::Open,
            "paused" => State::Paused,
            "done" => State::Done,
            "cleared" => State::Cleared,
            _ => return None,
        })
    }
}

/// Who continues the goal. Only `chat` exists in 0.0.3; `graph` is refused by name rather than
/// half-implemented, because a goal that silently took the wrong executor is work nobody asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Executor {
    Chat,
}

impl Executor {
    pub fn name(self) -> &'static str {
        match self {
            Executor::Chat => "chat",
        }
    }

    pub fn parse(text: &str) -> Result<Executor, Refusal> {
        match text.trim() {
            "" | "chat" => Ok(Executor::Chat),
            other => Err(Refusal::Unsupported {
                executor: other.to_string(),
            }),
        }
    }
}

/// Why a goal could not be created or moved. The variants are the contract: a caller matches on the
/// refusal instead of parsing a sentence, and the sentence is only for a person to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The named executor does not exist in this release.
    Unsupported {
        executor: String,
    },
    /// The state machine has no edge from the current state to the requested one.
    Illegal {
        from: State,
        to: State,
    },
    /// A goal needs text, a chat it can re-enter, and a budget above zero.
    Empty,
    NoChat,
    Budget,
    /// The goal's own check did not pass, so `done` is not available.
    Check {
        result: String,
    },
    NoGoal {
        id: u64,
    },
    /// A file could not be read or written. Kept apart from the state machine because the distinction
    /// matters to a caller deciding whether to retry.
    Store(String),
}

impl Refusal {
    /// A short name a caller can branch on without reading the prose.
    pub fn name(&self) -> &'static str {
        match self {
            Refusal::Unsupported { .. } => "executor",
            Refusal::Illegal { .. } => "transition",
            Refusal::Empty => "text",
            Refusal::NoChat => "chat",
            Refusal::Budget => "budget",
            Refusal::Check { .. } => "check",
            Refusal::NoGoal { .. } => "missing",
            Refusal::Store(_) => "store",
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::Unsupported { executor } => {
                write!(
                    f,
                    "executor `{executor}` is not implemented; only `chat` is"
                )
            }
            Refusal::Illegal { from, to } => {
                write!(
                    f,
                    "a goal that is {} cannot become {}",
                    from.name(),
                    to.name()
                )
            }
            Refusal::Empty => f.write_str("a goal needs text"),
            Refusal::NoChat => {
                f.write_str("a goal needs a chat; a goal with no chat cannot be re-entered")
            }
            Refusal::Budget => f.write_str("a goal needs a budget of at least one model call"),
            Refusal::Check { result } => {
                write!(f, "the recorded check did not pass ({result})")
            }
            Refusal::NoGoal { id } => write!(f, "there is no goal {id}"),
            Refusal::Store(said) => f.write_str(said),
        }
    }
}

/// One standing goal. The fields are the whole contract a heartbeat needs to re-enter a session: where
/// to speak (`chat`), what session it continues (`session`), which model and check judge the work, and
/// how many calls a continuation may spend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Goal {
    pub id: u64,
    pub text: String,
    pub state: State,
    pub session: String,
    pub chat: i64,
    pub model: String,
    pub check: String,
    pub budget: u32,
    pub executor: Executor,
    pub created: i64,
    pub updated: i64,
    /// The epoch second of the last heartbeat continuation. Zero means the heartbeat never ran.
    /// Separate from `updated`, which is when the state last changed, because the listing shows one and
    /// the state machine cares about the other.
    pub heartbeat: i64,
    /// Why the goal is paused, or what the last continuation learned. Empty when there is nothing to say.
    pub reason: String,
    /// The last `done` claim's verdict, so a failing check is evidence rather than a silent retry.
    pub last_check: String,
}

/// A goal as a person asks for it, before it has an id or a creation time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewGoal {
    pub text: String,
    pub session: String,
    pub chat: i64,
    pub model: String,
    pub check: String,
    pub budget: u32,
    /// The name as the caller typed it, so `graph` reaches [`Executor::parse`] and is refused there.
    pub executor: String,
}

impl Goal {
    pub fn to_value(&self) -> Value {
        json!({
            "id": self.id,
            "text": self.text,
            "state": self.state.name(),
            "session": self.session,
            "chat": self.chat,
            "model": self.model,
            "check": self.check,
            "budget": self.budget,
            "executor": self.executor.name(),
            "created": self.created,
            "updated": self.updated,
            "heartbeat": self.heartbeat,
            "reason": self.reason,
            "last_check": self.last_check,
        })
    }

    pub fn from_value(value: &Value) -> Option<Goal> {
        let executor = value
            .get("executor")
            .and_then(Value::as_str)
            .unwrap_or("chat");
        Some(Goal {
            id: value.get("id")?.as_u64()?,
            text: value.get("text")?.as_str()?.to_string(),
            state: State::parse(value.get("state")?.as_str()?)?,
            session: value.get("session")?.as_str()?.to_string(),
            chat: value.get("chat")?.as_i64()?,
            model: value
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            check: value
                .get("check")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            budget: u32::try_from(
                value
                    .get("budget")
                    .and_then(Value::as_u64)
                    .unwrap_or(u64::from(DEFAULT_BUDGET)),
            )
            .ok()?,
            executor: Executor::parse(executor).ok()?,
            created: value.get("created").and_then(Value::as_i64).unwrap_or(0),
            updated: value.get("updated").and_then(Value::as_i64).unwrap_or(0),
            heartbeat: value.get("heartbeat").and_then(Value::as_i64).unwrap_or(0),
            reason: value
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            last_check: value
                .get("last_check")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        })
    }

    /// The one state machine. It is private because `done` has a second condition, the recorded check,
    /// and a public `transition` would be a way around it. `complete` is the only door to `done`.
    fn transition(&mut self, to: State, reason: &str, now: i64) -> Result<(), Refusal> {
        let legal = match (self.state, to) {
            (State::Open, State::Paused | State::Done) => true,
            (State::Paused, State::Open) => true,
            // Cleared is terminal. "Any to cleared" means any live state, not a cleared goal again.
            (_, State::Cleared) => self.state != State::Cleared,
            _ => false,
        };
        if !legal {
            return Err(Refusal::Illegal {
                from: self.state,
                to,
            });
        }
        self.state = to;
        self.updated = now;
        self.reason = reason.to_string();
        Ok(())
    }
}

/// Where the goal files live.
pub fn dir(state: &Path) -> PathBuf {
    state.join("goals")
}

/// Every goal file, and every file that would not parse. Sorted by id so two listings read the same way.
pub fn load(state: &Path) -> (Vec<Goal>, Vec<String>) {
    let mut goals = Vec::new();
    let mut problems = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir(state)) else {
        return (goals, problems);
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|kind| kind == "json"))
        .collect();
    paths.sort();
    for path in paths {
        let shown = path
            .strip_prefix(state)
            .unwrap_or(&path)
            .display()
            .to_string();
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) => {
                problems.push(format!("{shown}: {error}"));
                continue;
            }
        };
        match serde_json::from_str::<Value>(&text)
            .ok()
            .as_ref()
            .and_then(Goal::from_value)
        {
            Some(goal) => goals.push(goal),
            None => problems.push(format!("{shown}: not a goal record")),
        }
    }
    goals.sort_by_key(|goal| goal.id);
    (goals, problems)
}

pub fn load_one(state: &Path, id: u64) -> Option<Goal> {
    load(state).0.into_iter().find(|goal| goal.id == id)
}

/// The goal a chat is currently keeping: the newest one that is still open or paused. A done or cleared
/// goal is history and does not answer a `done` claim.
pub fn active(state: &Path, chat: i64) -> Option<Goal> {
    load(state)
        .0
        .into_iter()
        .filter(|goal| goal.chat == chat && matches!(goal.state, State::Open | State::Paused))
        .max_by_key(|goal| goal.id)
}

pub fn path_of(state: &Path, id: u64) -> Result<PathBuf, String> {
    let Ok(entries) = std::fs::read_dir(dir(state)) else {
        return Err(format!("there is no goal {id}"));
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|kind| kind != "json") {
            continue;
        }
        let parsed = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|value| Goal::from_value(&value));
        if parsed.is_some_and(|goal| goal.id == id) {
            return Ok(path);
        }
    }
    Err(format!("there is no goal {id}"))
}

/// Write one goal. The write goes through a temp name and a rename so a heartbeat reading the file at
/// the same moment sees the old record or the new one, never half of either.
pub fn save(state: &Path, goal: &Goal) -> Result<PathBuf, String> {
    let folder = dir(state);
    std::fs::create_dir_all(&folder).map_err(|error| error.to_string())?;
    let path = path_of(state, goal.id).unwrap_or_else(|_| {
        folder.join(format!(
            "{}-{}.json",
            goal.id,
            mneme::slugify(&goal.text, 4)
        ))
    });
    let temporary = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(&goal.to_value())
        .map_err(|error| format!("the goal could not be rendered: {error}"))?;
    std::fs::write(&temporary, text)
        .map_err(|error| format!("{}: {error}", temporary.display()))?;
    std::fs::rename(&temporary, &path).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(path)
}

pub fn next_id(goals: &[Goal]) -> u64 {
    goals.iter().map(|goal| goal.id).max().unwrap_or(0) + 1
}

/// Create a goal. Only the fields a heartbeat needs are required; everything else is either derived
/// (`session`) or bounded (`budget`, `executor`).
pub fn set(state: &Path, new: NewGoal, now: i64) -> Result<Goal, Refusal> {
    let text = new.text.trim().to_string();
    if text.is_empty() {
        return Err(Refusal::Empty);
    }
    if new.chat <= 0 {
        return Err(Refusal::NoChat);
    }
    if new.budget == 0 {
        return Err(Refusal::Budget);
    }
    let executor = Executor::parse(&new.executor)?;
    let session = if new.session.trim().is_empty() {
        format!("tg-{}", new.chat)
    } else {
        new.session.trim().to_string()
    };
    let (goals, _) = load(state);
    let goal = Goal {
        id: next_id(&goals),
        text,
        state: State::Open,
        session,
        chat: new.chat,
        model: new.model,
        check: new.check,
        budget: new.budget,
        executor,
        created: now,
        updated: now,
        heartbeat: 0,
        reason: String::new(),
        last_check: String::new(),
    };
    save(state, &goal).map_err(Refusal::Store)?;
    record(state, &goal, "open");
    Ok(goal)
}

/// A person pausing a goal. Only an open goal can be paused; pausing a paused goal is refused rather
/// than treated as a second, quieter pause.
pub fn pause(state: &Path, id: u64, reason: &str, now: i64) -> Result<Goal, Refusal> {
    let mut goal = load_one(state, id).ok_or(Refusal::NoGoal { id })?;
    goal.transition(State::Paused, reason, now)?;
    save(state, &goal).map_err(Refusal::Store)?;
    record(state, &goal, "paused");
    Ok(goal)
}

/// Resume a paused goal. The heartbeat then treats it as open on the next tick.
pub fn resume(state: &Path, id: u64, now: i64) -> Result<Goal, Refusal> {
    let mut goal = load_one(state, id).ok_or(Refusal::NoGoal { id })?;
    goal.transition(State::Open, "resumed", now)?;
    save(state, &goal).map_err(Refusal::Store)?;
    record(state, &goal, "resumed");
    Ok(goal)
}

/// A person closing a goal. This is the one close that needs no check: a person is allowed to decide the
/// work is no longer wanted.
pub fn clear(state: &Path, id: u64, now: i64) -> Result<Goal, Refusal> {
    let mut goal = load_one(state, id).ok_or(Refusal::NoGoal { id })?;
    goal.transition(State::Cleared, "a person cleared the goal", now)?;
    save(state, &goal).map_err(Refusal::Store)?;
    record(state, &goal, "cleared");
    Ok(goal)
}

/// The agent claiming done. The claim is not the close: the goal's own recorded check is run in the
/// goal's workspace, and `done` is written only when it passes. A failure is recorded on the goal and
/// returned as a typed refusal, so the goal stays open with the reason it is still open.
pub fn complete(state: &Path, id: u64, root: &Path, now: i64) -> Result<Goal, Refusal> {
    let mut goal = load_one(state, id).ok_or(Refusal::NoGoal { id })?;
    if goal.state != State::Open {
        return Err(Refusal::Illegal {
            from: goal.state,
            to: State::Done,
        });
    }
    let outcome = if goal.check.trim().is_empty() {
        "could_not_run: the goal records no check".to_string()
    } else {
        let task = TaskSpec::new(&goal.text, root);
        CommandVerifier::new(4_000)
            .verify(&task, &goal.check)
            .state_line()
    };
    goal.last_check = outcome.clone();
    if outcome.starts_with("passed") {
        goal.transition(State::Done, "the recorded check passed", now)?;
        save(state, &goal).map_err(Refusal::Store)?;
        record(state, &goal, "done");
        Ok(goal)
    } else {
        goal.updated = now;
        goal.reason = format!("the recorded check {outcome}");
        save(state, &goal).map_err(Refusal::Store)?;
        record(state, &goal, "check_failed");
        Err(Refusal::Check { result: outcome })
    }
}

/// Whether a goal is due for a continuation. The interval is measured from the later of the goal's last
/// heartbeat and the chat's last entry: a person who just spoke resets the clock, and so does a
/// continuation, which is what makes "one per interval" true even when a continuation fails.
pub fn due(goal: &Goal, now: i64, interval: i64, last_entry: i64) -> bool {
    goal.state == State::Open
        && goal.chat > 0
        && now.saturating_sub(goal.heartbeat.max(last_entry)) >= interval
}

/// The epoch second of the chat's most recent conversation entry. The chat history is the session's
/// last entry on this surface; its mtime is when that entry was written, and there is no timestamp in
/// the stored message to read instead. A chat that has never spoken is zero, so its first goal is due.
pub fn last_entry(state: &Path, chat: i64) -> i64 {
    std::fs::metadata(chat_path(state, chat))
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|since| since.as_secs() as i64)
        .unwrap_or(0)
}

/// Every goal the heartbeat may continue now, with the attempt stamped before it runs. The stamp is
/// written first on purpose: a continuation that crashes still counts as this interval's attempt, so a
/// broken goal is retried once an interval rather than once a tick.
///
/// `busy` is injected because "no live job" is three facts on this surface (a chat answer holding the
/// chat lock, a daemon run leasing the session, and a child executor still live), and a test should be
/// able to say "busy" without constructing all three.
pub fn due_goals<F>(state: &Path, now: i64, interval: i64, busy: F) -> Result<Vec<Goal>, String>
where
    F: Fn(&Goal) -> bool,
{
    let (goals, _) = load(state);
    let mut due_now = Vec::new();
    for mut goal in goals {
        if !due(&goal, now, interval, last_entry(state, goal.chat)) {
            continue;
        }
        if busy(&goal) {
            continue;
        }
        goal.heartbeat = now;
        save(state, &goal)?;
        record(state, &goal, "heartbeat");
        due_now.push(goal);
    }
    Ok(due_now)
}

/// What a continuation tells the model. It names the check because a model that does not know how it
/// will be judged optimises for looking finished, and it names the tool because the claim is not the
/// close.
fn continuation_prompt(goal: &Goal) -> String {
    let check = if goal.check.trim().is_empty() {
        "This goal records no check, so only a person can close it. Do not claim done.".to_string()
    } else {
        format!(
            "Done when this check passes: `{}`. When it is met, call the goal tool with action \
             \"done\"; the check decides, not your claim.",
            goal.check
        )
    };
    format!(
        "This is a heartbeat continuation of a standing goal you keep for this chat.\n\n\
         Goal {}: {}\n\
         {check}\n\n\
         Work on it with your tools. If it is not met yet, say what you did and what is still missing.",
        goal.id, goal.text
    )
}

/// One continuation, with the client supplied. Production builds the client from the goal's recorded
/// model; a test supplies an endpoint it can script.
#[allow(clippy::too_many_arguments)]
pub fn continuation_with<T: crate::gateway::Transport>(
    client: &OpenAi,
    settings: &Settings,
    state: &Path,
    goal: &mut Goal,
    transport: &mut T,
    mcp: &Arc<crate::mcp::Mcp>,
    now: i64,
    stop: Option<&AtomicBool>,
) -> Result<String, String> {
    let root = PathBuf::from(&settings.root);
    let mut world = world_for(
        settings,
        state,
        goal.chat,
        crate::mode::Mode::Act,
        &format!("goal:{}", goal.id),
        mcp,
    );
    // The continuation runs in the conversation's session, not the child-lifecycle log `world_for`
    // supplies, so its Meta evidence lands where the goal says it does.
    world.session = goal.session.clone();
    world.allowance = crate::agent::Allowance::new(goal.budget.max(1));

    let mut messages = vec![json!({
        "role": "system",
        "content": system_prompt(&root)
    })];
    messages.extend(load_chat(state, goal.chat));
    let prompt = continuation_prompt(goal);
    messages.push(json!({"role": "user", "content": prompt}));

    let silent = |_: crate::model::Feed, _: &str| {};
    let nothing = |_: crate::agent::Activity, _: &str| {};
    let answer =
        crate::agent::answer_stoppable(client, &mut messages, &world, &silent, &nothing, 12, stop);

    match answer {
        Ok(Some(text)) => {
            transport.send(goal.chat, &crate::gateway::esc(&text), &[])?;
            // The stored exchange is the continuation and the reply, not the whole prompt: the prompt is
            // rebuilt from the goal next time, and storing it would teach the chat history to talk to
            // itself. The reply is kept so a person's next message sees what the heartbeat said.
            let mut history = load_chat(state, goal.chat);
            history.push(json!({
                "role": "user",
                "content": format!("[heartbeat] continue goal {}", goal.id)
            }));
            history.push(json!({"role": "assistant", "content": text}));
            save_chat(state, goal.chat, &history);
            refresh(state, goal);
            Ok(text)
        }
        Ok(None) => {
            // A person stopped it. The interval was already stamped, and a stopped turn is not the
            // assistant's turn, so neither the prompt nor a refusal is stored.
            transport.send(
                goal.chat,
                "<b>Goal continuation stopped.</b> What already ran is not undone.",
                &[],
            )?;
            refresh(state, goal);
            Ok(String::new())
        }
        Err(fault) => {
            // The agent's own `goal done` call writes the file while this loop is running. Reload before
            // deciding, or a goal that just finished is overwritten with a pause.
            refresh(state, goal);
            let out_of_budget = world.allowance.left() == 0 && fault.detail().contains("allowance");
            if goal.state != State::Open {
                return Ok(String::new());
            }
            if out_of_budget {
                goal.transition(State::Paused, "budget", now)
                    .map_err(|refusal| refusal.to_string())?;
                save(state, goal)?;
                record(state, goal, "paused");
                transport.send(
                    goal.chat,
                    &format!(
                        "<b>Goal {} paused: budget.</b> A continuation spent all {} model call(s) \
                         without finishing. It will not be retried until a person resumes it.",
                        goal.id, goal.budget
                    ),
                    &[],
                )?;
                return Err("budget".to_string());
            }
            goal.reason = format!("continuation failed: {}", fault.detail());
            save(state, goal)?;
            record(state, goal, "failed");
            transport.send(
                goal.chat,
                &format!(
                    "<b>Goal {} continuation failed.</b> {}",
                    goal.id,
                    crate::gateway::esc(fault.detail())
                ),
                &[],
            )?;
            Err(fault.to_string())
        }
    }
}

/// One continuation, with the model taken from the goal or the gateway's configured preset.
#[allow(clippy::too_many_arguments)]
pub fn continuation<T: crate::gateway::Transport>(
    settings: &Settings,
    state: &Path,
    goal: &mut Goal,
    transport: &mut T,
    mcp: &Arc<crate::mcp::Mcp>,
    now: i64,
    stop: Option<&AtomicBool>,
) -> Result<String, String> {
    let name = if goal.model.trim().is_empty() {
        settings.model.clone()
    } else {
        goal.model.clone()
    };
    let chosen = preset(&name).ok_or_else(|| format!("no model preset named `{name}`"))?;
    let client = from_preset(&chosen);
    continuation_with(&client, settings, state, goal, transport, mcp, now, stop)
}

/// Replace the in-memory goal with the file the continuation may have changed. A file that vanished is
/// left alone: the caller still needs a value to report, and inventing a state would be worse.
fn refresh(state: &Path, goal: &mut Goal) {
    if let Some(kept) = load_one(state, goal.id) {
        *goal = kept;
    }
}

/// One line for a person and for the model. The heartbeat timestamp is shown as `never` rather than
/// zero, because zero reads as an epoch date nobody can act on.
pub fn describe(goal: &Goal) -> String {
    let mut text = format!(
        "goal {} · {} · chat {} · session {}\n  {}",
        goal.id,
        goal.state.name(),
        goal.chat,
        goal.session,
        goal.text.lines().next().unwrap_or("")
    );
    text.push_str(&format!(
        "\n  check: {}\n  budget: {} call(s)\n  last heartbeat: {}",
        if goal.check.trim().is_empty() {
            "none".to_string()
        } else {
            goal.check.clone()
        },
        goal.budget,
        heartbeat_text(goal.heartbeat)
    ));
    if !goal.reason.is_empty() {
        text.push_str(&format!("\n  reason: {}", goal.reason));
    }
    if !goal.last_check.is_empty() {
        text.push_str(&format!("\n  last check: {}", goal.last_check));
    }
    text
}

pub fn heartbeat_text(at: i64) -> String {
    if at == 0 {
        "never".to_string()
    } else {
        at.to_string()
    }
}

/// The Meta evidence. A best-effort append: the goal file is the record work runs from, and a log that
/// cannot be appended does not undo a goal that was already created.
///
/// The daemon owns `tg-<chat>` while a run is leased there, and the heartbeat only appends when no run
/// is leased. A set or a pause issued from a terminal during a live run can still lose that race; the
/// failure is reported here and the goal file, which is what the continuation reads, is unaffected.
fn record(state: &Path, goal: &Goal, action: &str) {
    let lock = log_lock(state, &goal.session);
    let _held = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let outcome = open_log(state, &goal.session).and_then(|mut log| {
        log.append(
            Kind::Meta,
            json!({
                "type": "goal",
                "action": action,
                "goal": goal.to_value(),
            }),
        )
        .map(|_| ())
        .map_err(|fault| fault.to_string())
    });
    if let Err(said) = outcome {
        eprintln!("goal {}: the session log was not written: {said}", goal.id);
    }
}

fn open_log(state: &Path, session: &str) -> Result<Session<FileStore, SystemClock>, String> {
    let store = FileStore::new(state.join("sessions"));
    match Session::load(store.clone(), SystemClock, SessionId::new(session)) {
        Ok(log) => Ok(log),
        // A missing log is a new session. Any other fault is real and must not be papered over.
        Err(session_tree::Fault::Io { .. }) => {
            Session::create(store, SystemClock, SessionId::new(session))
                .map_err(|fault| fault.to_string())
        }
        Err(fault) => Err(fault.to_string()),
    }
}

/// One lock per session log, for the life of the process. Deliberately the same shape as the child
/// registry's lock: goals and children write different logs, and each writer needs the load and the
/// append to be one step so two handles do not compute the same next entry id.
type Locks = Mutex<BTreeMap<(PathBuf, String), Arc<Mutex<()>>>>;

fn log_lock(state: &Path, session: &str) -> Arc<Mutex<()>> {
    static LOCKS: std::sync::OnceLock<Locks> = std::sync::OnceLock::new();
    let locks = LOCKS.get_or_init(|| Mutex::new(BTreeMap::new()));
    let mut locks = locks.lock().unwrap_or_else(|held| held.into_inner());
    Arc::clone(
        locks
            .entry((state.to_path_buf(), session.to_string()))
            .or_default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::{Button, Transport, Update};

    fn scratch(name: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or(0);
        let base =
            std::env::temp_dir().join(format!("hades-goal-{name}-{}-{stamp}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("scratch");
        base
    }

    fn settings(root: &Path) -> Settings {
        Settings {
            allowed: vec![7],
            policy: "guarded".to_string(),
            verify: "true".to_string(),
            model: "nanbeige-3b".to_string(),
            root: root.display().to_string(),
            vault: root.join("mneme").display().to_string(),
            heartbeat: DEFAULT_HEARTBEAT_SECS,
            mcp: crate::mcp::McpConfig::default(),
        }
    }

    fn new_goal(chat: i64, check: &str, budget: u32) -> NewGoal {
        NewGoal {
            text: "ship the report".to_string(),
            session: String::new(),
            chat,
            model: String::new(),
            check: check.to_string(),
            budget,
            executor: "chat".to_string(),
        }
    }

    fn fixture(id: u64) -> Goal {
        Goal {
            id,
            text: "ship the report".to_string(),
            state: State::Open,
            session: "tg-7".to_string(),
            chat: 7,
            model: String::new(),
            check: "true".to_string(),
            budget: 3,
            executor: Executor::Chat,
            created: 100,
            updated: 100,
            heartbeat: 0,
            reason: String::new(),
            last_check: String::new(),
        }
    }

    fn base_time() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_secs() as i64)
            .unwrap_or(0)
    }

    #[test]
    fn a_goal_is_session_state_recorded_as_a_meta_event() {
        let state = scratch("meta");
        let goal = set(&state, new_goal(7, "true", 3), 100).expect("a goal");
        assert_eq!(goal.session, "tg-7", "the session is derived from the chat");
        assert_eq!(goal.state, State::Open);
        assert_eq!(goal.heartbeat, 0);

        let (goals, problems) = load(&state);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(goals, vec![goal.clone()]);

        let text =
            std::fs::read_to_string(state.join("sessions/tg-7.jsonl")).expect("the session log");
        let entries: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).expect("one entry per line"))
            .collect();
        let event = entries
            .iter()
            .find(|entry| {
                entry["kind"] == "meta"
                    && entry["payload"]["type"] == "goal"
                    && entry["payload"]["action"] == "open"
            })
            .expect("the goal event");
        assert_eq!(event["payload"]["goal"]["state"], "open");
        assert_eq!(event["payload"]["goal"]["chat"], 7);
        assert_eq!(
            session_tree::SCHEMA_VERSION,
            1,
            "a Meta-typed goal does not bump the log schema"
        );
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn only_the_declared_transitions_are_legal() {
        let mut goal = fixture(1);
        assert_eq!(
            goal.transition(State::Open, "", 1),
            Err(Refusal::Illegal {
                from: State::Open,
                to: State::Open
            })
        );
        goal.transition(State::Paused, "waiting", 2)
            .expect("open to paused");
        assert_eq!(
            goal.transition(State::Done, "", 3),
            Err(Refusal::Illegal {
                from: State::Paused,
                to: State::Done
            }),
            "a paused goal is not done without being resumed"
        );
        goal.transition(State::Open, "", 4).expect("paused to open");
        goal.transition(State::Done, "", 5).expect("open to done");
        assert_eq!(
            goal.transition(State::Paused, "", 6),
            Err(Refusal::Illegal {
                from: State::Done,
                to: State::Paused
            })
        );
        goal.transition(State::Cleared, "", 7)
            .expect("done to cleared");
        assert_eq!(
            goal.transition(State::Open, "", 8),
            Err(Refusal::Illegal {
                from: State::Cleared,
                to: State::Open
            }),
            "cleared is terminal"
        );
        assert_eq!(
            goal.transition(State::Cleared, "", 9),
            Err(Refusal::Illegal {
                from: State::Cleared,
                to: State::Cleared
            })
        );
        assert_eq!(goal.reason, "");
        assert_eq!(goal.updated, 7, "a refused transition writes nothing");
    }

    #[test]
    fn only_the_chat_executor_exists_and_graph_is_a_typed_refusal() {
        assert_eq!(Executor::parse("chat"), Ok(Executor::Chat));
        assert_eq!(Executor::parse(""), Ok(Executor::Chat));
        let refused = Executor::parse("graph").expect_err("graph is not implemented");
        assert_eq!(
            refused,
            Refusal::Unsupported {
                executor: "graph".to_string()
            }
        );
        assert_eq!(refused.name(), "executor");
        let state = scratch("graph");
        let mut new = new_goal(7, "true", 3);
        new.executor = "graph".to_string();
        assert_eq!(
            set(&state, new, 100),
            Err(Refusal::Unsupported {
                executor: "graph".to_string()
            })
        );
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn done_requires_the_goal_recorded_check_to_pass() {
        let state = scratch("check");
        let root = state.join("work");
        std::fs::create_dir_all(&root).expect("a workspace");
        let goal = set(&state, new_goal(7, "false", 3), 100).expect("a goal");
        let refused = complete(&state, goal.id, &root, 200).expect_err("the check fails");
        assert!(matches!(refused, Refusal::Check { .. }), "{refused:?}");
        assert_eq!(refused.name(), "check");
        let kept = load_one(&state, goal.id).expect("the goal");
        assert_eq!(kept.state, State::Open, "a failed claim leaves it open");
        assert!(kept.last_check.contains("failed"), "{:?}", kept.last_check);
        assert!(kept.reason.contains("failed"), "{:?}", kept.reason);

        let passing = set(&state, new_goal(7, "true", 3), 300).expect("a second goal");
        let done = complete(&state, passing.id, &root, 400).expect("the check passes");
        assert_eq!(done.state, State::Done);
        assert!(
            done.last_check.starts_with("passed"),
            "{:?}",
            done.last_check
        );
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn a_person_pauses_resumes_and_clears_with_the_reason_recorded() {
        let state = scratch("person");
        let goal = set(&state, new_goal(7, "true", 3), 100).expect("a goal");
        let paused = pause(&state, goal.id, "waiting for an answer", 200).expect("a pause");
        assert_eq!(paused.state, State::Paused);
        assert_eq!(paused.reason, "waiting for an answer");
        assert_eq!(
            pause(&state, goal.id, "again", 300),
            Err(Refusal::Illegal {
                from: State::Paused,
                to: State::Paused
            }),
            "pausing twice is not a transition"
        );
        let resumed = resume(&state, goal.id, 400).expect("a resume");
        assert_eq!(resumed.state, State::Open);
        let cleared = clear(&state, goal.id, 500).expect("a clear");
        assert_eq!(cleared.state, State::Cleared);
        assert_eq!(cleared.reason, "a person cleared the goal");
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn a_goal_file_survives_a_reload_and_keeps_its_fields() {
        let state = scratch("reload");
        let mut new = new_goal(9, "test -f out.txt", 5);
        new.session = "tg-9".to_string();
        new.model = "nanbeige-3b".to_string();
        let goal = set(&state, new, 100).expect("a goal");
        let (goals, problems) = load(&state);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(goals, vec![goal], "the file is the whole record");
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn the_heartbeat_is_one_attempt_per_interval_and_none_while_busy() {
        let state = scratch("due");
        let base = base_time();
        let goal = set(&state, new_goal(7, "true", 3), base).expect("a goal");

        let busy = due_goals(&state, base, DEFAULT_HEARTBEAT_SECS, |_| true).expect("a tick");
        assert!(busy.is_empty(), "a live job blocks the continuation");
        assert_eq!(
            load_one(&state, goal.id).expect("the goal").heartbeat,
            0,
            "a blocked tick does not spend the interval"
        );

        let first = due_goals(&state, base, DEFAULT_HEARTBEAT_SECS, |_| false).expect("a tick");
        assert_eq!(first.len(), 1, "one continuation");
        assert_eq!(first[0].heartbeat, base);
        assert!(
            due_goals(&state, base + 899, DEFAULT_HEARTBEAT_SECS, |_| false)
                .expect("a tick")
                .is_empty(),
            "the interval is not over yet"
        );
        let second =
            due_goals(&state, base + 900, DEFAULT_HEARTBEAT_SECS, |_| false).expect("a tick");
        assert_eq!(second.len(), 1, "exactly one more at the next interval");
        assert!(
            due_goals(&state, base + 1_799, DEFAULT_HEARTBEAT_SECS, |_| false)
                .expect("a tick")
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(&state);
    }

    /// A model endpoint that answers each request with one streamed delta, in order. The count is the
    /// proof for the budget test: the fourth call must never happen.
    struct Stub {
        url: String,
        calls: Arc<std::sync::atomic::AtomicUsize>,
        server: std::thread::JoinHandle<()>,
    }

    fn read_request(stream: &mut std::net::TcpStream) -> Value {
        use std::io::{BufRead, BufReader, Read};
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
        serde_json::from_slice(&body).expect("a request")
    }

    fn write_answer(stream: &mut std::net::TcpStream, delta: &Value) {
        use std::io::Write;
        let frame = json!({"choices": [{"delta": delta}]}).to_string();
        let body = format!("data: {frame}\n\ndata: [DONE]\n\n");
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .expect("a response");
    }

    fn stub(answers: Vec<Value>) -> Stub {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a port");
        let url = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().expect("an address")
        );
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&calls);
        let server = std::thread::spawn(move || {
            for answer in answers {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let _ = read_request(&mut stream);
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                write_answer(&mut stream, &answer);
            }
        });
        Stub { url, calls, server }
    }

    #[derive(Default)]
    struct Fake {
        sent: Vec<(i64, String)>,
    }

    impl Transport for Fake {
        fn updates(&mut self, _offset: Option<i64>, _timeout: u64) -> Result<Vec<Update>, String> {
            Ok(Vec::new())
        }

        fn send(
            &mut self,
            chat: i64,
            text: &str,
            _buttons: &[Button],
        ) -> Result<Option<i64>, String> {
            self.sent.push((chat, text.to_string()));
            Ok(Some(1))
        }

        fn edit(
            &mut self,
            _chat: i64,
            _message: i64,
            _text: &str,
            _buttons: &[Button],
        ) -> Result<(), String> {
            Ok(())
        }

        fn typing(&mut self, _chat: i64) -> Result<(), String> {
            Ok(())
        }

        fn ack(&mut self, _callback: &str, _text: &str) -> Result<(), String> {
            Ok(())
        }
    }

    #[test]
    fn a_continuation_lands_in_the_recorded_chat() {
        let state = scratch("continuation");
        let root = state.join("work");
        std::fs::create_dir_all(&root).expect("a workspace");
        let base = base_time();
        let goal = set(&state, new_goal(7, "true", 4), base).expect("a goal");
        let stub = stub(vec![json!({"content": "still working"})]);
        let mut fake = Fake::default();
        let mut continuing = load_one(&state, goal.id).expect("the goal");
        continuation_with(
            &OpenAi::new(&stub.url, "stub"),
            &settings(&root),
            &state,
            &mut continuing,
            &mut fake,
            &Arc::new(crate::mcp::Mcp::empty()),
            base,
            None,
        )
        .expect("a continuation");

        assert_eq!(fake.sent.len(), 1);
        assert_eq!(fake.sent[0].0, 7, "the recorded chat, not another one");
        assert!(fake.sent[0].1.contains("still working"));
        let history = load_chat(&state, 7);
        let last = history.last().expect("the reply in the history");
        assert_eq!(last["role"], "assistant");
        assert_eq!(last["content"], "still working");
        stub.server.join().expect("the endpoint");
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn a_goal_with_a_three_call_budget_pauses_and_says_budget() {
        let state = scratch("budget");
        let root = state.join("work");
        std::fs::create_dir_all(&root).expect("a workspace");
        let goal = set(&state, new_goal(7, "true", 3), base_time()).expect("a goal");
        let call = json!({
            "tool_calls": [{
                "index": 0,
                "id": "call-1",
                "function": {"name": "read_file", "arguments": "{\"path\":\"missing.txt\"}"}
            }]
        });
        let stub = stub(vec![call.clone(), call.clone(), call]);
        let mut fake = Fake::default();
        let mut continuing = load_one(&state, goal.id).expect("the goal");
        let outcome = continuation_with(
            &OpenAi::new(&stub.url, "stub"),
            &settings(&root),
            &state,
            &mut continuing,
            &mut fake,
            &Arc::new(crate::mcp::Mcp::empty()),
            base_time(),
            None,
        );
        assert_eq!(outcome.expect_err("the allowance is spent"), "budget");
        assert_eq!(
            stub.calls.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "three calls, then the fourth is refused before the endpoint"
        );
        let kept = load_one(&state, goal.id).expect("the goal");
        assert_eq!(kept.state, State::Paused);
        assert_eq!(kept.reason, "budget");
        assert!(
            fake.sent
                .iter()
                .any(|(_, text)| text.to_lowercase().contains("budget")),
            "{:?}",
            fake.sent
        );
        stub.server.join().expect("the endpoint");
        let _ = std::fs::remove_dir_all(&state);
    }
}
