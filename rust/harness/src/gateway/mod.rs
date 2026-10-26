//! The gateway: a chat message becomes a daemon op, never a second runtime.
//!
//! One daemon owns every session, and this module is one surface attached to it. The transport is a
//! trait because Telegram's network is not testable here and the loop is: the oracle drives scripted
//! updates through a fake and the real client is one implementation with no branches of its own.
//!
//! A run reports while it works. The status message is sent once and edited as entries arrive, the chat
//! shows a typing indicator for as long as the run is live, and the run's own steps (the program it
//! wrote, what the shell returned, the check's verdict) appear in the message a person is watching
//! rather than in a final summary that hides them. Telegram is the limit: an edit per second is safe,
//! and an edit that repeats the same text is refused, so the writer throttles and the last line always
//! wins.
//!
//! The trust boundary is the sender's numeric id, checked against `gateway.telegram.allowed_users` in
//! the config file. Message text never chooses a policy, a root, a profile, a model or a check: those
//! come from the config, because a stranger's sentence must not be able to set them.
//!
//! The token is read from `HADES_TELEGRAM_TOKEN` (the environment, then Hades' own `.env`) and appears
//! in the Bot API URL, which means it is visible in the curl process's arguments for the length of a
//! call. That is a limit of the Bot API's shape, recorded here rather than hidden.

pub mod chat;
pub mod children;
pub mod goal;
pub mod lease;
pub mod session;
pub mod telegram;

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

pub use chat::Chat;

/// The default per-message character budget, used by a surface that does not declare its own. Telegram
/// accepts 4096; the gateway keeps a margin for the tags it adds.
const DEFAULT_LIMIT: usize = 3_800;
/// How often the status message may be rewritten. An edit per entry turns a long attempt into a flood;
/// a second between edits keeps the stream readable and stays well inside Telegram's limit.
const EDIT_EVERY: Duration = Duration::from_millis(1_100);
/// Lines kept in the live status. Older steps scroll out; the message stays one screen and one edit.
const LIVE_LINES: usize = 14;

/// What the config file says about the gateway. Read from the same file the daemon reads, because a
/// second settings file is a second place to be out of date.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub allowed: Vec<String>,
    pub policy: String,
    /// The check a chat run is judged by. A message cannot name one for itself, and the profile's default
    /// (`cargo test --quiet`) is wrong outside a Rust tree: every run would fail a check about another
    /// project. The gateway therefore carries one check per surface, and a config without one refuses.
    pub verify: String,
    /// The preset a plain message is answered with. The harness profiles choose a model for work; a chat
    /// is not work and should not be routed through the fix profile's check and retries.
    pub model: String,
    /// Where a chat's tools run. Absent means the process's own directory, which a daemon should never
    /// rely on, so the config names it and the gateway refuses to guess.
    pub root: String,
    /// Hades' own memory, the same vault the governed runs use.
    pub vault: String,
    /// How long a session must be idle before the heartbeat continues an open goal, in seconds. From
    /// `gateway.heartbeat`, with the fifteen-minute default when the key is absent.
    pub heartbeat: i64,
    /// The MCP servers a chat may reach, parsed from the same file.
    pub mcp: crate::mcp::McpConfig,
}

/// The environment variable a surface's token is read from. A connector never chooses its own name: a
/// pack that could name `OPENAI_API_KEY` would be handed the model key as its "token".
pub fn token_env(surface: &str) -> String {
    if surface == "tg" {
        return "HADES_TELEGRAM_TOKEN".to_string();
    }
    format!(
        "HADES_CONNECTOR_{}_TOKEN",
        surface.to_uppercase().replace('-', "_")
    )
}

/// Read one surface's settings from a config file. Telegram keeps `gateway.telegram`; every connector
/// lives under `gateway.connectors.<id>`, so two surfaces cannot read each other's policy. A missing
/// section, an empty allowlist or an empty check is a refusal with the key named: an absent allowlist
/// must never mean everyone, and a run with no check can only end in `could_not_run`.
pub fn settings(path: &Path) -> Result<Settings, String> {
    settings_for(path, "tg")
}

pub fn settings_for(path: &Path, surface: &str) -> Result<Settings, String> {
    let text =
        std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let value: Value =
        serde_json::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))?;
    let (section, name) = if surface == "tg" {
        (value.pointer("/gateway/telegram").cloned(), "gateway.telegram".to_string())
    } else {
        (
            value
                .pointer(&format!("/gateway/connectors/{surface}"))
                .cloned(),
            format!("gateway.connectors.{surface}"),
        )
    };
    let section = section.ok_or_else(|| {
        format!(
            "{}: there is no `{name}` section; the gateway will not start without one",
            path.display()
        )
    })?;
    let allowed: Vec<String> = section
        .get("allowed_users")
        .and_then(Value::as_array)
        .map(|ids| {
            ids.iter()
                .filter_map(|id| match id {
                    Value::String(text) => Some(text.clone()),
                    Value::Number(number) => Some(number.to_string()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    if allowed.is_empty() {
        return Err(format!(
            "{}: `{name}.allowed_users` is empty; an empty list is not everyone",
            path.display()
        ));
    }
    let verify = section
        .get("verify")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if verify.trim().is_empty() {
        return Err(format!(
            "{}: `{name}.verify` is empty; a chat run needs a check to be judged by",
            path.display()
        ));
    }
    let model = value
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if model.is_empty() {
        return Err(format!(
            "{}: no top-level `model`; a chat needs a preset to answer with",
            path.display()
        ));
    }
    let root = value
        .get("root")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if root.is_empty() {
        return Err(format!(
            "{}: no top-level `root`; a chat's tools need a workspace",
            path.display()
        ));
    }
    let vault = value
        .get("vault")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if vault.is_empty() {
        return Err(format!(
            "{}: no top-level `vault`; a chat's memory needs a vault",
            path.display()
        ));
    }
    let policy = section
        .get("policy")
        .and_then(Value::as_str)
        .unwrap_or("guarded")
        .to_string();
    if policy != "guarded" && policy != "open" {
        return Err(format!(
            "{}: `{name}.policy` is {policy:?}; say guarded or open",
            path.display()
        ));
    }
    // A heartbeat interval of zero would turn every tick into a continuation. Refusing is the honest
    // direction: a config that cannot mean what it says must not be guessed at.
    let heartbeat = match value.pointer("/gateway/heartbeat") {
        None => goal::DEFAULT_HEARTBEAT_SECS,
        Some(value) => value.as_i64().filter(|secs| *secs > 0).ok_or_else(|| {
            format!(
                "{}: `gateway.heartbeat` is {value}; it has to be a positive number of seconds",
                path.display()
            )
        })?,
    };
    Ok(Settings {
        allowed,
        policy,
        verify,
        model,
        root,
        vault,
        heartbeat,
        mcp: crate::mcp::McpConfig::from_value(value.get("mcp"), path)?,
    })
}

/// One message or one button press, as much of it as this surface reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Update {
    pub id: i64,
    pub from: String,
    pub chat: Chat,
    pub text: String,
    pub callback: Option<Callback>,
    /// The largest size of a photo the person sent, when they sent one. The gateway downloads it, looks at
    /// it with the vision pair, and gives the chat model the description: the chat model does not see pixels.
    pub photo: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Callback {
    pub id: String,
    pub data: String,
}

/// One inline button: what a person reads, and the short data Telegram sends back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Button {
    pub label: String,
    pub data: String,
}

/// The network, behind one seam. A clone is a second handle to the same account, which is what lets the
/// poller keep polling while a worker streams and a heartbeat types.
pub trait Transport {
    fn updates(&mut self, offset: Option<i64>, timeout: u64) -> Result<Vec<Update>, String>;
    /// Send a message and hand back its id when the surface has one, so it can be edited afterwards.
    fn send(&mut self, chat: &Chat, text: &str, buttons: &[Button]) -> Result<Option<i64>, String>;
    fn edit(
        &mut self,
        chat: &Chat,
        message: i64,
        text: &str,
        buttons: &[Button],
    ) -> Result<(), String>;
    fn typing(&mut self, chat: &Chat) -> Result<(), String>;
    fn ack(&mut self, callback: &str, text: &str) -> Result<(), String>;
    /// The most characters this surface accepts in one message. The gateway chunks an answer to fit
    /// rather than cutting it, because a surface with a smaller budget than Telegram's would otherwise
    /// lose the end of every long answer without saying so.
    fn limit(&self) -> usize {
        DEFAULT_LIMIT
    }
    /// Whether this surface can render inline buttons. An approval that arrives at a surface without them
    /// would park a run nobody can unblock, so a guarded policy needs this to be true.
    fn buttons(&self) -> bool {
        false
    }
    /// Fetch a file the platform holds, by its file id. Surfaces without files keep the default.
    fn file(&self, _file_id: &str) -> Result<Vec<u8>, String> {
        Err("this surface cannot fetch files".to_string())
    }
}

/// The in-memory half of a stop on this surface. The daemon's `stop` covers a `/run`, because that work is
/// a daemon session; a plain conversation runs in the gateway process, so it needs a flag the poller can
/// raise while the answer is streaming. One flag per chat, plus a lock per chat so two messages do not
/// interleave their histories.
#[derive(Default)]
pub struct Signals {
    flags: Mutex<BTreeMap<Chat, Arc<AtomicBool>>>,
    locks: Mutex<BTreeMap<Chat, Arc<Mutex<()>>>>,
    /// Set by `/shutdown`: the daemon is going, so the poller has no reason to ask it anything else.
    shutdown: AtomicBool,
}

impl Signals {
    /// The lock for one chat. A worker takes it before it registers, and holds it for its life, so two
    /// messages in one chat do not interleave their histories and the registered flag is always the flag of
    /// the answer a stop would actually reach.
    pub fn lock(&self, chat: &Chat) -> Arc<Mutex<()>> {
        let mut locks = self.locks.lock().unwrap_or_else(|held| held.into_inner());
        Arc::clone(locks.entry(chat.clone()).or_default())
    }

    /// Register the answer now running in this chat. The caller holds [`Signals::lock`] for this chat, so
    /// this cannot replace a worker that is still working.
    pub fn begin(&self, chat: &Chat) -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(false));
        let mut flags = self.flags.lock().unwrap_or_else(|held| held.into_inner());
        flags.insert(chat.clone(), Arc::clone(&flag));
        flag
    }

    /// Forget a worker, but only its own flag: a newer worker for the same chat must not lose its stop.
    pub fn leave(&self, chat: &Chat, flag: &Arc<AtomicBool>) {
        let mut flags = self.flags.lock().unwrap_or_else(|held| held.into_inner());
        if flags.get(chat).is_some_and(|held| Arc::ptr_eq(held, flag)) {
            flags.remove(&chat);
        }
    }

    /// Ask the answer in this chat to stop. False means no answer was registered, which is what a `/stop`
    /// with nothing running reports rather than pretending it did something.
    pub fn stop(&self, chat: &Chat) -> bool {
        let flags = self.flags.lock().unwrap_or_else(|held| held.into_inner());
        match flags.get(chat) {
            Some(flag) => {
                flag.store(true, Ordering::SeqCst);
                true
            }
            None => false,
        }
    }

    /// Whether a chat answer is running. The worker holds the chat's lock for its whole life, so a lock
    /// that cannot be taken is a live job. A poisoned lock is also work this surface cannot safely
    /// interleave with, so it counts as busy rather than free.
    pub fn busy(&self, chat: &Chat) -> bool {
        let lock = self.lock(chat);
        lock.try_lock().is_err()
    }

    fn flag(&self, chat: &Chat) -> Option<Arc<AtomicBool>> {
        self.flags
            .lock()
            .unwrap_or_else(|held| held.into_inner())
            .get(chat)
            .cloned()
    }

    /// Whether `/shutdown` ended the poller.
    pub fn stopped(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }
}

/// The offset file. One number: the next update id to ask for. Written after a batch is handled, so a
/// crash replays at most one batch and a replay is dropped by the offset rather than answered twice.
fn offset_path(state: &Path) -> PathBuf {
    state.join("gateway").join("telegram.json")
}

pub fn load_offset(state: &Path) -> Option<i64> {
    let text = std::fs::read_to_string(offset_path(state)).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value.get("offset").and_then(Value::as_i64)
}

fn save_offset(state: &Path, offset: i64) -> Result<(), String> {
    let path = offset_path(state);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    std::fs::write(&path, json!({ "offset": offset }).to_string())
        .map_err(|error| format!("{}: {error}", path.display()))
}

/// A question the gateway is holding: the goal it belongs to, and the session whose standing answer
/// will let the resumed attempt through.
#[derive(Debug, Clone)]
struct Pending {
    goal: String,
    session: String,
}

fn pending_path(state: &Path, id: u64) -> PathBuf {
    state
        .join("gateway")
        .join("pending")
        .join(format!("{id}.json"))
}

fn pending_save(state: &Path, id: u64, goal: &str, session: &str) -> Result<(), String> {
    let path = pending_path(state, id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    std::fs::write(&path, json!({"goal": goal, "session": session}).to_string())
        .map_err(|error| format!("{}: {error}", path.display()))
}

fn pending_load(state: &Path, id: u64) -> Option<Pending> {
    let text = std::fs::read_to_string(pending_path(state, id)).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    Some(Pending {
        goal: value.get("goal")?.as_str()?.to_string(),
        session: value.get("session")?.as_str()?.to_string(),
    })
}

fn pending_remove(state: &Path, id: u64) {
    let _ = std::fs::remove_file(pending_path(state, id));
}

/// One request whose answer is one line.
fn ask(socket: &Path, request: &Value) -> Result<Value, String> {
    let mut stream = UnixStream::connect(socket)
        .map_err(|error| format!("could not reach {}: {error}", socket.display()))?;
    writeln!(stream, "{request}")
        .map_err(|error| format!("could not send the request: {error}"))?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .map_err(|error| format!("the daemon went away: {error}"))?;
    serde_json::from_str(line.trim())
        .map_err(|error| format!("the daemon's answer would not read: {error}"))
}

/// The start of a long block, for a reasoning excerpt that a person reads rather than greps.
fn head(text: &str, limit: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= limit {
        return trimmed.to_string();
    }
    let kept: String = trimmed.chars().take(limit).collect();
    format!("{kept}...")
}

fn tail(text: &str, limit: usize) -> String {
    let trimmed = text.trim_end();
    if trimmed.chars().count() <= limit {
        return trimmed.to_string();
    }
    let skip = trimmed.chars().count() - limit;
    let kept: String = trimmed.chars().skip(skip).collect();
    format!("...{kept}")
}

/// Text that came from a model or a shell is untrusted as HTML: escape it before it goes in a message,
/// or one `<` from a build log changes the formatting of everything after it.
fn esc(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn pre(text: &str) -> String {
    format!("<pre>{}</pre>", esc(text.trim_end()))
}

/// A tool's name as a heading a person reads.
fn title(tool: &str) -> &'static str {
    match tool {
        "shell" => "Shell",
        "verify" => "Check",
        "vault" => "Vault",
        _ => "Work",
    }
}

/// One entry as a block a person can read. The point is not to mirror JSON; it is to show what the run
/// did while it did it, which is the part a final summary hides. Reasoning, when the endpoint returned
/// it, comes before the program that followed it, because that is the order it happened in.
fn describe(entry: &Value) -> Option<String> {
    let kind = entry.get("kind").and_then(Value::as_str).unwrap_or("");
    let payload = entry.get("payload").unwrap_or(&Value::Null);
    match kind {
        "tool_call" => {
            let tool = payload
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or("tool");
            let program = payload.get("program").and_then(Value::as_str).unwrap_or("");
            let mut block = String::new();
            if let Some(reasoning) = payload.get("reasoning").and_then(Value::as_str) {
                let said = head(reasoning, 500);
                if !said.trim().is_empty() {
                    block.push_str(&format!("<b>Thinking</b>\n<i>{}</i>\n\n", esc(&said)));
                }
            }
            if program.trim().is_empty() {
                block.push_str(&format!("<b>{}</b>", title(tool)));
            } else {
                block.push_str(&format!("<b>{}</b>\n{}", title(tool), pre(program)));
            }
            Some(block)
        }
        "tool_result" => {
            let status = payload
                .get("state")
                .and_then(Value::as_str)
                .or_else(|| payload.get("status").and_then(Value::as_str))
                .unwrap_or("");
            let output = payload.get("output").and_then(Value::as_str).unwrap_or("");
            let head = format!("<b>Result</b> · exit {}", esc(status));
            if output.trim().is_empty() {
                Some(head)
            } else {
                Some(format!("{head}\n{}", pre(&tail(output, 360))))
            }
        }
        "approval_request" => Some("<b>Permission</b> · waiting for you".to_string()),
        "approval_decision" => {
            let decision = payload
                .get("decision")
                .and_then(Value::as_str)
                .unwrap_or("decided");
            Some(format!("<b>Approval</b> · {}", esc(decision)))
        }
        "checkpoint" => payload
            .get("why")
            .and_then(Value::as_str)
            .map(|why| format!("<i>{}</i>", esc(why))),
        "judged" => payload
            .get("reason")
            .and_then(Value::as_str)
            .or_else(|| payload.get("verdict").and_then(Value::as_str))
            .map(|said| format!("<b>Judge</b> · {}", esc(said))),
        _ => None,
    }
}

/// The live message: one id, lines that grow, and the throttle that keeps the edits legal.
struct Status {
    chat: Chat,
    id: Option<i64>,
    /// The model's thinking, as it arrives. Only the tail is shown: what a person wants while it works is
    /// the current thought, not a transcript.
    reasoning: String,
    /// The answer being written, before the turn turns it into a program block.
    content: String,
    lines: Vec<String>,
    last: Instant,
}

impl Status {
    fn new(chat: Chat, id: Option<i64>) -> Status {
        Status {
            chat,
            id,
            reasoning: String::new(),
            content: String::new(),
            lines: Vec::new(),
            last: Instant::now() - EDIT_EVERY,
        }
    }

    fn push(&mut self, line: String) {
        if self.lines.last() == Some(&line) {
            return;
        }
        self.lines.push(line);
        if self.lines.len() > LIVE_LINES {
            let drop = self.lines.len() - LIVE_LINES;
            self.lines.drain(0..drop);
        }
    }

    fn feed(&mut self, kind: &str, delta: &str) {
        match kind {
            "reasoning" => self.reasoning.push_str(delta),
            "content" => self.content.push_str(delta),
            _ => {}
        }
    }

    fn text(&self, limit: usize) -> String {
        let mut parts: Vec<String> = Vec::new();
        if !self.reasoning.trim().is_empty() {
            parts.push(format!(
                "<b>Thinking</b>\n<i>{}</i>",
                esc(&tail(&self.reasoning, 900))
            ));
        }
        if !self.content.trim().is_empty() {
            parts.push(format!(
                "<b>Writing</b>\n{}",
                pre(&tail(&self.content, 700))
            ));
        }
        parts.extend(self.lines.iter().cloned());
        clip(&parts.join("\n"), limit)
    }

    fn draw(&mut self, transport: &mut dyn Transport, force: bool) -> Result<(), String> {
        let Some(id) = self.id else {
            return Ok(());
        };
        if !force && self.last.elapsed() < EDIT_EVERY {
            return Ok(());
        }
        self.last = Instant::now();
        let text = self.text(transport.limit());
        match transport.edit(&self.chat, id, &text, &[]) {
            // Telegram refuses an edit that changes nothing; that is not a failure to report.
            Err(said) if said.contains("message is not modified") => Ok(()),
            other => other,
        }
    }
}

/// Send a request whose answer is a stream, and narrate it into one edited message while it happens.
fn follow<T: Transport + Clone + Send + 'static>(
    socket: &Path,
    request: &Value,
    transport: &mut T,
    chat: Chat,
    state: &Path,
) -> Result<(), String> {
    let mut stream = UnixStream::connect(socket)
        .map_err(|error| format!("could not reach {}: {error}", socket.display()))?;
    writeln!(stream, "{request}")
        .map_err(|error| format!("could not send the request: {error}"))?;

    let started = transport.send(&chat, "<b>Hades</b> · working", &[])?;
    // The first typing claim is made here, not left to the ticker's thread: a run whose daemon answers
    // in a few milliseconds would otherwise finish before that thread was ever scheduled, and the chat
    // would never have been told the run was alive.
    let _ = transport.typing(&chat);
    let mut status = Status::new(chat.clone(), started);
    let mut asked = false;
    let stop = Arc::new(AtomicBool::new(false));
    let heartbeat = {
        // Typing is a five-second claim; a run that thinks for a minute has to keep making it, or the
        // chat looks dead exactly when the model is doing the most work.
        let mut beat = transport.clone();
        let stop = stop.clone();
        let chat = chat.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                let _ = beat.typing(&chat);
                for _ in 0..40 {
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        })
    };

    for line in BufReader::new(stream).lines() {
        let line = line.map_err(|error| format!("the daemon went away: {error}"))?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if value
            .get("started")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            let run = value.get("run").and_then(Value::as_u64).unwrap_or(0);
            let session = value.get("session").and_then(Value::as_str).unwrap_or("?");
            status.push(format!(
                "<b>Hades</b> · run {run} · session <code>{}</code>",
                esc(session)
            ));
            status.draw(transport, true)?;
        } else if let Some(kind) = value.get("progress").and_then(Value::as_str) {
            status.feed(
                kind,
                value.get("delta").and_then(Value::as_str).unwrap_or(""),
            );
            status.draw(transport, false)?;
        } else if let Some(entry) = value.get("entry") {
            let kind = entry.get("kind").and_then(Value::as_str).unwrap_or("");
            if let Some(described) = describe(entry) {
                if kind == "tool_call" {
                    // The streamed halves are now the program block; keeping them too would say it twice.
                    status.reasoning.clear();
                    status.content.clear();
                }
                status.push(described);
            }
            if kind == "approval_request" {
                let payload = entry.get("payload").unwrap_or(&Value::Null);
                let id = payload.get("id").and_then(Value::as_u64).unwrap_or(0);
                let action = payload
                    .get("action")
                    .and_then(Value::as_str)
                    .unwrap_or("an action");
                let reason = payload.get("reason").and_then(Value::as_str).unwrap_or("");
                let goal = request.get("goal").and_then(Value::as_str).unwrap_or("");
                let session = request.get("session").and_then(Value::as_str).unwrap_or("");
                asked = true;
                match pending_save(state, id, goal, session) {
                    Ok(()) => {
                        let text = clip(&format!(
                            "<b>Permission needed</b>\n{}\n<i>{}</i>",
                            pre(action),
                            esc(reason)
                        ), transport.limit());
                        if !transport.buttons() {
                            // A surface with no buttons cannot answer this question. Saying so and stopping
                            // is better than parking a run behind a prompt that can never be pressed; the
                            // question stays in the record, and another surface that can render it still can.
                            transport.send(&chat, &text, &[])?;
                            transport.send(
                                &chat,
                                &clip(
                                    "This surface cannot render Allow/Deny buttons, so this run is waiting for a decision it cannot ask for. Set `gateway.<id>.policy` to `open` or run it from a surface with buttons.",
                                    transport.limit(),
                                ),
                                &[],
                            )?;
                            stop.store(true, Ordering::SeqCst);
                            continue;
                        }
                        let buttons = [
                            Button {
                                label: "Allow".to_string(),
                                data: format!("a:{id}:1"),
                            },
                            Button {
                                label: "Deny".to_string(),
                                data: format!("a:{id}:0"),
                            },
                        ];
                        transport.send(&chat, &text, &buttons)?;
                        // While a person decides, the run is parked: typing would be a lie.
                        stop.store(true, Ordering::SeqCst);
                    }
                    Err(said) => {
                        let _ = transport.send(&chat,
                            &clip(&format!(
                                "this run wants to {action} but I could not remember the question ({said}), so Allow could not continue it. The question stays open in the record."
                            ), transport.limit()),
                            &[],
                        );
                    }
                }
            }
            status.draw(transport, false)?;
        } else if value
            .get("finished")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            stop.store(true, Ordering::SeqCst);
            let ended = value.get("ended").and_then(Value::as_str).unwrap_or("");
            if ended == "awaiting" && asked {
                status.push("<i>Waiting for your decision above.</i>".to_string());
                status.draw(transport, true)?;
                let _ = heartbeat.join();
                return Ok(());
            }
            let mut text = match value.get("ended").and_then(Value::as_str) {
                Some(ended) => {
                    let (head, rest) = ended.split_once(' ').unwrap_or((ended, ""));
                    let mut head = head.to_string();
                    if let Some(first) = head.get_mut(0..1) {
                        first.make_ascii_uppercase();
                    }
                    format!("<b>{}</b> {}", esc(&head), esc(rest))
                }
                None => match value.get("fault").and_then(Value::as_str) {
                    Some(fault) => format!("<b>Faulted</b>\n{}", pre(fault)),
                    None => "<b>Ended</b> · the daemon said nothing about how".to_string(),
                },
            };
            if let Some(recorded) = value.get("recorded").and_then(Value::as_str) {
                text.push_str(&format!("\nRecorded: <code>{}</code>", esc(recorded)));
            }
            status.push(text);
            status.draw(transport, true)?;
            let _ = heartbeat.join();
            return Ok(());
        }
    }
    stop.store(true, Ordering::SeqCst);
    let _ = heartbeat.join();
    // A stream that closes without `finished` is not a success; say so instead of leaving `working...`.
    status.push("the daemon closed the stream without saying how the run ended".to_string());
    status.draw(transport, true)
}

fn clip(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let kept: String = text.chars().take(limit).collect();
    format!("{kept}\n[cut: the message was longer than {limit} characters]")
}

/// Split an answer into parts a surface will accept, without cutting a word in half when a boundary is
/// available. Paragraph first, then line, then space; a hard cut only when the text has no boundary at all
/// (a base64 blob, a long URL). The caller joins nothing back: each part is a message a person reads.
fn split(text: &str, limit: usize) -> Vec<String> {
    let mut parts = Vec::new();
    let mut rest = text.trim_start();
    while !rest.is_empty() {
        if rest.chars().count() <= limit {
            parts.push(rest.to_string());
            break;
        }
        let window: &str = &rest[..byte_at(rest, limit)];
        // Prefer the last blank line, then the last newline, then the last space, all inside the window and
        // past halfway so a part is never needlessly short.
        let floor = byte_at(window, limit / 2);
        let cut = window[floor..]
            .rfind("\n\n")
            .map(|at| floor + at + 2)
            .or_else(|| window[floor..].rfind('\n').map(|at| floor + at + 1))
            .or_else(|| window[floor..].rfind(' ').map(|at| floor + at + 1))
            .unwrap_or(window.len());
        let (head, tail) = rest.split_at(cut);
        let head = head.trim_end();
        if !head.is_empty() {
            parts.push(head.to_string());
        }
        rest = tail.trim_start();
    }
    if parts.is_empty() {
        parts.push(String::new());
    }
    parts
}

/// The byte index of the `chars`-th character, or the length when the string is shorter.
fn byte_at(text: &str, chars: usize) -> usize {
    text.char_indices()
        .nth(chars)
        .map(|(at, _)| at)
        .unwrap_or(text.len())
}

fn command(text: &str) -> (&str, &str) {
    let text = text.trim();
    if !text.starts_with('/') {
        return ("/run", text);
    }
    match text.split_once(char::is_whitespace) {
        Some((name, rest)) => (name, rest.trim()),
        None => (text, ""),
    }
}

/// How much of a conversation is sent back to the model. Eight exchanges is enough for pronouns and not
/// enough to pay for yesterday.
const CHAT_KEEP: usize = 16;

fn chat_path(state: &Path, chat: &Chat) -> PathBuf {
    state
        .join("gateway")
        .join(chat.surface())
        .join("chat")
        .join(format!("{}.json", chat.id()))
}

fn load_chat(state: &Path, chat: &Chat) -> Vec<Value> {
    // Before surfaces existed, a chat's history was `gateway/chat/<number>.json`. The new path is
    // `gateway/<surface>/chat/<id>.json`; reading the old file once keeps an existing conversation
    // intact, and the next save writes it to the new place.
    let path = chat_path(state, chat);
    let legacy = state
        .join("gateway")
        .join("chat")
        .join(format!("{}.json", chat.id()));
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(_) if chat.surface() == "tg" => std::fs::read_to_string(legacy).unwrap_or_default(),
        Err(_) => String::new(),
    };
    let Ok(Value::Array(messages)) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    messages
}

fn save_chat(state: &Path, chat: &Chat, messages: &[Value]) {
    let path = chat_path(state, chat);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let kept: Vec<&Value> = messages
        .iter()
        .rev()
        .take(CHAT_KEEP)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let kept: Vec<Value> = kept.into_iter().cloned().collect();
    let _ = std::fs::write(&path, Value::Array(kept).to_string());
}

/// A chat card is prose, not a run card: the answer as it is written, and the thinking only until there is
/// an answer to read. The live view shows the tail of a long answer, because the newest words are the ones
/// a person is waiting for; the finished answer is split into messages instead of being cut.
fn render_chat(reasoning: &str, content: &str, activity: &[String], limit: usize) -> String {
    if !content.trim().is_empty() {
        return esc(&tail(content.trim(), limit));
    }
    let mut parts = Vec::new();
    if !activity.is_empty() {
        parts.push(
            activity
                .iter()
                .map(|line| format!("<i>· {}</i>", esc(line)))
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    let thought = tail(reasoning.trim(), 600);
    if !thought.trim().is_empty() {
        parts.push(format!("<i>{}</i>", esc(&thought)));
    }
    if parts.is_empty() {
        "<i>…</i>".to_string()
    } else {
        clip(&parts.join("\n\n"), limit)
    }
}

/// A plain message is a conversation. It is not a goal, it does not get a program, a check or attempts, and
/// the person sees the answer while it is written. `/run` is the door to the harness.
fn chat<T: Transport + Clone + Send + 'static>(
    update: &Update,
    settings: &Settings,
    state: &Path,
    transport: &mut T,
    halt: Option<&AtomicBool>,
    mcp: &Arc<crate::mcp::Mcp>,
) -> Result<(), String> {
    let _ = state;
    let preset = crate::model::preset(&settings.model)
        .ok_or_else(|| format!("no model preset named `{}`", settings.model))?;
    let client = crate::model::from_preset(&preset);
    let mut history = load_chat(state, &update.chat);
    let workspace = PathBuf::from(&settings.root);
    let world = world_for(
        settings,
        state,
        update.chat.clone(),
        crate::mode::Mode::Act,
        &format!("chat:{}", update.chat),
        mcp,
    );
    let mut messages = vec![json!({"role": "system", "content": system_prompt(&workspace)})];
    messages.append(&mut history);
    let mut asked = update.text.trim().to_string();
    if let Some(file_id) = &update.photo {
        let safe: String = file_id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let path = state.join("inbox").join(format!("{safe}.jpg"));
        let outcome = transport.file(file_id).and_then(|bytes| {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            }
            std::fs::write(&path, bytes).map_err(|error| error.to_string())
        });
        match outcome {
            Ok(()) => match crate::vision::describe(
                &path,
                "Describe this image precisely. Transcribe every word of text visible, verbatim.",
            ) {
                Ok(said) => {
                    asked = format!("[the person sent an image. What it shows: {said}]\n\n{asked}");
                }
                Err(said) => {
                    asked = format!(
                        "[the person sent an image saved at {} but it could not be read: {said}]\n\n{asked}",
                        path.display()
                    );
                }
            },
            Err(said) => {
                asked = format!(
                    "[the person sent an image that could not be downloaded: {said}]\n\n{asked}"
                );
            }
        }
        if asked.trim().is_empty() {
            asked = "[the person sent an image]".to_string();
        }
    }
    messages.push(json!({"role": "user", "content": asked}));

    // A long conversation becomes one summary at the top before it becomes a cost problem. Compression
    // is the one place the system prompt may change mid-conversation, which is why the summary rides
    // there instead of posing as a message the person never sent.
    if let Err(fault) = crate::agent::compress(&client, &mut messages, 8, 12000) {
        eprintln!("compression: not written, {fault}");
    }
    let id = transport.send(&update.chat, "<i>…</i>", &[])?;
    let limit = transport.limit();
    let shared = Arc::new(Mutex::new((
        String::new(),
        String::new(),
        Vec::<String>::new(),
    )));
    let stop = Arc::new(AtomicBool::new(false));
    // See `relay`: the claim is made once on this thread before the ticker owns it.
    let _ = transport.typing(&update.chat);
    let ticker = {
        let mut beat = transport.clone();
        let shared = Arc::clone(&shared);
        let stop = Arc::clone(&stop);
        let chat_id = update.chat.clone();
        std::thread::spawn(move || {
            let mut last = Instant::now() - EDIT_EVERY;
            while !stop.load(Ordering::SeqCst) {
                let _ = beat.typing(&chat_id);
                if last.elapsed() >= EDIT_EVERY
                    && let Some(message) = id
                {
                    let text = {
                        let held = shared.lock().expect("the chat buffer");
                        render_chat(&held.0, &held.1, &held.2, limit)
                    };
                    let _ = beat.edit(&chat_id, message, &text, &[]);
                    last = Instant::now();
                }
                std::thread::sleep(Duration::from_millis(300));
            }
        })
    };
    let feed = |kind: crate::model::Feed, piece: &str| {
        if let Ok(mut held) = shared.lock() {
            match kind {
                crate::model::Feed::Reasoning => held.0.push_str(piece),
                crate::model::Feed::Content => held.1.push_str(piece),
            }
        }
    };
    let activity = |event: crate::agent::Activity, label: &str| {
        if event == crate::agent::Activity::Start
            && let Ok(mut held) = shared.lock()
        {
            held.2.push(label.to_string());
            if held.2.len() > 8 {
                held.2.remove(0);
            }
        }
    };
    let answer =
        crate::agent::answer_stoppable(&client, &mut messages, &world, &feed, &activity, 20, halt);
    stop.store(true, Ordering::SeqCst);
    let _ = ticker.join();

    match answer {
        Ok(Some(text)) => {
            // An answer longer than the surface accepts becomes several messages instead of a cut. The
            // placeholder is the first part when the surface gave us an id to edit.
            let parts = split(&esc(&text), transport.limit());
            match id {
                Some(message) => {
                    transport.edit(&update.chat, message, &parts[0], &[])?;
                    for part in &parts[1..] {
                        transport.send(&update.chat, part, &[])?;
                    }
                }
                None => {
                    for part in &parts {
                        transport.send(&update.chat, part, &[])?;
                    }
                }
            }
            // The record is the conversation, not the tool traffic: the next message needs the exchange.
            let kept = vec![
                json!({"role": "user", "content": asked}),
                json!({"role": "assistant", "content": text}),
            ];
            let mut history = load_chat(state, &update.chat);
            history.extend(kept);
            save_chat(state, &update.chat, &history);
            Ok(())
        }
        Ok(None) => {
            // A stopped answer is not the assistant's turn: storing it would make the next prompt believe a
            // refusal was something it said. The person's message stays out too, for the same reason.
            if let Some(message) = id {
                transport.edit(
                    &update.chat,
                    message,
                    "<b>Stopped.</b> What already ran is not undone.",
                    &[],
                )?;
            }
            Ok(())
        }
        Err(fault) => {
            let said = fault.detail().to_string();
            if let Some(message) = id {
                transport.edit(
                    &update.chat,
                    message,
                    &format!("<b>Error</b> {}", esc(&said)),
                    &[],
                )?;
            }
            Err(said)
        }
    }
}

fn system_prompt(workspace: &Path) -> String {
    format!(
        "You are Hades, a single-user agent in a chat. Answer in prose that reads like a message: \
short unless the question needs depth, no tables, no headers. Your tools run in {}. Use them when the \
answer depends on the machine, a file or the web, and then report what matters rather than the tool \
traffic. For work that has to happen again later, use the cron tool and never the shell's crontab: a job \
made there reports back into this chat. When a job splits into independent parts, delegate runs them in \
parallel. Never mention checks, sessions, attempts or routing.",
        workspace.display()
    )
}

/// One update, handled to the end. A submit blocks until its run finishes, which is why `serve` gives
/// each one a thread: the poller has to stay free to receive the approval that the run is waiting for.
pub fn handle<T: Transport + Clone + Send + 'static>(
    update: &Update,
    settings: &Settings,
    socket: &Path,
    state: &Path,
    transport: &mut T,
) -> Result<(), String> {
    handle_with(
        update,
        settings,
        socket,
        state,
        transport,
        &Signals::default(),
        &Arc::new(crate::mcp::Mcp::empty()),
    )
}

/// The same, with the surface's stop flags. A plain message needs this: it runs a model loop in this
/// process, and a `/stop` that cannot reach that loop is a command that only works when nothing is running.
pub fn handle_with<T: Transport + Clone + Send + 'static>(
    update: &Update,
    settings: &Settings,
    socket: &Path,
    state: &Path,
    transport: &mut T,
    signals: &Signals,
    mcp: &Arc<crate::mcp::Mcp>,
) -> Result<(), String> {
    if !settings.allowed.contains(&update.from) {
        return Ok(());
    }
    if let Some(callback) = &update.callback {
        let by = format!("{}:{}", update.chat.surface(), update.from);
        let parts: Vec<&str> = callback.data.split(':').collect();
        let (id, allow) = match parts.as_slice() {
            ["a", id, allow] if *allow == "1" || *allow == "0" => match id.parse::<u64>() {
                Ok(id) => (id, *allow == "1"),
                Err(_) => return transport.ack(&callback.id, "that question id does not read"),
            },
            _ => return transport.ack(&callback.id, "that button is not an answer"),
        };
        let Some(held) = pending_load(state, id) else {
            return transport.ack(&callback.id, "that question is not waiting on this machine");
        };
        let decision = if allow { "allow" } else { "deny" };
        // Session scope, not once: the answer has to survive the run that asked ending in `awaiting`, or
        // the next attempt asks the same question and no answer ever lets work through.
        let response = ask(
            socket,
            &json!({
                "op": "answer",
                "id": id,
                "decision": decision,
                "scope": "session",
                "by": by,
            }),
        )?;
        if !response.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            let said = response
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("the daemon refused the answer");
            return transport.ack(&callback.id, said);
        }
        pending_remove(state, id);
        if !allow {
            transport.ack(&callback.id, "denied")?;
            transport.send(&update.chat, "<b>Denied.</b> The run stays stopped.", &[])?;
            return Ok(());
        }
        transport.ack(&callback.id, "allowed, continuing")?;
        let request = json!({
            "op": "submit",
            "state": state.display().to_string(),
            "goal": held.goal,
            "session": held.session,
            "policy": settings.policy,
            "verify": settings.verify,
        });
        return follow(socket, &request, transport, update.chat.clone(), state);
    }

    let text = update.text.trim();
    if !text.starts_with('/') {
        let halt = signals.flag(&update.chat);
        return chat(update, settings, state, transport, halt.as_deref(), mcp);
    }
    match command(text) {
        ("/help", _) => {
            transport.send(&update.chat,
                "Send a message to talk to Hades.\n<code>/run &lt;goal&gt;</code> starts work on the machine, with a check and an approval when the policy asks.\n<code>/status</code> shows the sessions.\n<code>/stop</code> stops this chat's work; <code>/stop all</code> stops every run.\n<code>/cancel &lt;job&gt;</code> removes a scheduled job and its pending retry.\n<code>/shutdown</code> stops everything and ends the daemon.",
                &[],
            )?;
            Ok(())
        }
        ("/status", _) => {
            let response = ask(socket, &json!({"op": "status"}))?;
            let sessions = response
                .get("sessions")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            let waiting = response
                .get("questions")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            transport.send(&update.chat,
                &format!("<b>{sessions}</b> session(s), <b>{waiting}</b> question(s) waiting"),
                &[],
            )?;
            Ok(())
        }
        ("/stop", what) => {
            let by = format!("{}:{}", update.chat.surface(), update.from);
            let all = what.trim() == "all";
            let request = if all {
                json!({"op": "stop", "by": by})
            } else {
                json!({
                    "op": "stop",
                    "session": update.chat.key(),
                    "by": by,
                })
            };
            let response = match ask(socket, &request) {
                Ok(response) => response,
                Err(said) => {
                    // A stop with no daemon to stop is not a stop. Say that, not "the answer failed".
                    transport.send(&update.chat,
                        &format!("<b>Could not stop.</b> {}", esc(&said)),
                        &[],
                    )?;
                    return Ok(());
                }
            };
            if response.get("ok").and_then(Value::as_bool) != Some(true) {
                let said = response
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("the daemon refused the stop");
                transport.send(&update.chat,
                    &format!("<b>Could not stop.</b> {}", esc(said)),
                    &[],
                )?;
                return Ok(());
            }
            // Only after the daemon accepted the request: a failed stop must not kill the answer in this
            // chat and then tell the person it could not stop anything.
            let answered = signals.stop(&update.chat);
            let runs = response
                .get("stopping")
                .and_then(Value::as_array)
                .map(|runs| {
                    runs.iter()
                        .map(|run| {
                            format!(
                                "run {} in {}",
                                run.get("run").and_then(Value::as_u64).unwrap_or(0),
                                run.get("session").and_then(Value::as_str).unwrap_or("?")
                            )
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let said = match (answered, runs.is_empty()) {
                (false, true) => {
                    "<b>Nothing is running.</b> Stopping is not undo: what already ran stays run."
                        .to_string()
                }
                (true, true) => {
                    "<b>Stopping.</b> The answer in this chat ends at its next step. What already ran is \
                     not undone."
                        .to_string()
                }
                (_, false) => format!(
                    "<b>Stopping.</b> {}. What already ran is not undone.",
                    esc(&runs.join(", "))
                ),
            };
            transport.send(&update.chat, &said, &[])?;
            Ok(())
        }
        ("/cancel", job) => {
            let id = match job.trim().parse::<u64>() {
                Ok(id) => id,
                Err(_) => {
                    transport.send(&update.chat,
                        "That command needs a job number, as in <code>/cancel 3</code>.",
                        &[],
                    )?;
                    return Ok(());
                }
            };
            let by = format!("{}:{}", update.chat.surface(), update.from);
            match crate::cron::cancel(state, id, &by) {
                Ok(path) => transport.send(&update.chat,
                    &format!(
                        "<b>Cancelled job {id}.</b> Its file <code>{}</code> is gone and a pending retry is \
                         cleared. What it already ran is not undone.",
                        esc(&path)
                    ),
                    &[],
                )?,
                Err(said) => transport.send(&update.chat,
                    &format!("<b>Not cancelled.</b> {}", esc(&said)),
                    &[],
                )?,
            };
            Ok(())
        }
        ("/shutdown", _) => {
            let by = format!("{}:{}", update.chat.surface(), update.from);
            let response = match ask(socket, &json!({"op": "shutdown", "by": by})) {
                Ok(response) => response,
                Err(said) => {
                    transport.send(&update.chat,
                        &format!("<b>Could not shut down.</b> {}", esc(&said)),
                        &[],
                    )?;
                    return Ok(());
                }
            };
            if response.get("ok").and_then(Value::as_bool) != Some(true) {
                let said = response
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("the daemon refused the shutdown");
                transport.send(&update.chat,
                    &format!("<b>Could not shut down.</b> {}", esc(said)),
                    &[],
                )?;
                return Ok(());
            }
            signals.shutdown.store(true, Ordering::SeqCst);
            let count = response
                .get("stopping")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            transport.send(&update.chat,
                &format!(
                    "<b>Stopping everything.</b> {count} run(s) asked to stop and the daemon is exiting. \
                     A command already running finishes first."
                ),
                &[],
            )?;
            Ok(())
        }
        ("/run", goal) if !goal.is_empty() => {
            let request = json!({
                "op": "submit",
                "state": state.display().to_string(),
                "goal": goal,
                "session": update.chat.key(),
                "policy": settings.policy,
                "verify": settings.verify,
            });
            follow(socket, &request, transport, update.chat.clone(), state)
        }
        ("/run", _) => {
            transport.send(&update.chat, "That command needs a goal after it.", &[])?;
            Ok(())
        }
        (unknown, _) => {
            transport.send(&update.chat,
                &format!("There is no command called {}. Send /help.", esc(unknown)),
                &[],
            )?;
            Ok(())
        }
    }
}

/// The poll loop. Quick updates are handled here; a run gets a thread so the poller can receive the
/// approval that run is blocked on. `once` handles one batch and returns, which is what the oracle drives.
/// Scheduled work lives beside the gateway rather than inside the daemon: the result is a message, and the
/// gateway is what has a chat to put it in. The last-run stamp is written before the work starts, so a crash
/// delays a job instead of repeating it forever.
fn scheduler<T: Transport + Clone + Send + 'static>(
    mut transport: T,
    settings: Settings,
    state: PathBuf,
    mcp: Arc<crate::mcp::Mcp>,
) {
    loop {
        let now = epoch();
        let (jobs, _) = crate::cron::load(&state);
        for job in jobs {
            let pending = crate::retry::read(&state, job.id);
            let Some(step) = crate::retry::step(&job, pending.as_ref(), now) else {
                continue;
            };
            // A cancel between the load and this tick removes the file. The first attempt would notice in
            // `stamp`, but a retry does not stamp, so it is checked here or a cancelled job runs once more.
            if crate::cron::path_of(&state, job.id).is_err() {
                crate::retry::done(&state, job.id);
                continue;
            }
            if step.attempt == 1 {
                // Re-read and stamp that one file before anything runs, so a hand edit made a moment ago
                // is kept and a crash delays the job instead of repeating it every twenty seconds. If the
                // stamp cannot be written the job does not run: an unstamped window would run again on
                // the next tick.
                if let Err(said) = crate::cron::stamp(&state, job.id, now) {
                    eprintln!("cron: job {}: not running, {said}", job.id);
                    continue;
                }
            }
            let headline = esc(&job
                .goal
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(70)
                .collect::<String>());
            let started = now;
            let result = scheduled(&job, &settings, &state, &step, &mcp);
            let finished = epoch();
            let (outcome, detail) = match &result {
                Ok(_) => ("ok", String::new()),
                Err(said) => ("failed", said.clone()),
            };
            let entry = crate::retry::Entry {
                window: step.window.clone(),
                attempt: step.attempt,
                started,
                finished,
                outcome: outcome.to_string(),
                detail,
            };
            if let Err(said) = crate::retry::record(&state, job.id, &entry) {
                eprintln!("cron: job {}: could not record the attempt: {said}", job.id);
            }
            match result {
                Ok(text) => {
                    if let Err(said) = transport.send(&job.chat,
                        &format!("<b>{headline}</b>\n\n{}", esc(&text)),
                        &[],
                    ) {
                        eprintln!("cron: job {}: {said}", job.id);
                    }
                    crate::retry::done(&state, job.id);
                }
                Err(said) => {
                    if step.attempt < crate::retry::ATTEMPTS {
                        // Quiet until the last try: a transient failure is not news, and three messages
                        // about one occurrence is worse than one.
                        let at = finished + crate::retry::backoff(step.attempt);
                        if let Err(error) = crate::retry::again(&state, job.id, &step, at, &said) {
                            eprintln!("cron: job {}: could not schedule a retry: {error}", job.id);
                        }
                    } else {
                        if let Err(error) = transport.send(&job.chat,
                            &format!(
                                "<b>{headline}</b>\n\nAfter {} attempts this job failed: {}",
                                step.attempt,
                                esc(&said)
                            ),
                            &[],
                        ) {
                            eprintln!("cron: job {}: {error}", job.id);
                        }
                        crate::retry::done(&state, job.id);
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_secs(20));
    }
}

/// The epoch second, the one clock the scheduler reads.
fn epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

/// The heartbeat, beside the scheduler and not inside it. Every tick it asks the goal store which open
/// goals are due, then runs one continuation for each in the chat the goal records. The stamp is written
/// before the continuation runs, so a crash or a failure costs one interval rather than one tick.
fn heartbeat<T: Transport + Clone + Send + 'static>(
    mut transport: T,
    settings: Settings,
    state: PathBuf,
    socket: PathBuf,
    mcp: Arc<crate::mcp::Mcp>,
    signals: Arc<Signals>,
) {
    loop {
        let now = epoch();
        let busy = |goal: &goal::Goal| {
            signals.busy(&goal.chat)
                || daemon_busy(&socket, &goal.session)
                || child_busy(&state, &goal.chat)
        };
        match goal::due_goals(&state, now, settings.heartbeat, busy) {
            Ok(goals) => {
                for mut continuing in goals {
                    // The chat's own lock serialises a heartbeat with the person's answer. The busy check
                    // ran a moment ago; taking the lock is what makes the gap harmless.
                    let lock = signals.lock(&continuing.chat);
                    let _held = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    let flag = signals.begin(&continuing.chat);
                    let outcome = goal::continuation(
                        &settings,
                        &state,
                        &mut continuing,
                        &mut transport,
                        &mcp,
                        now,
                        Some(&flag),
                    );
                    signals.leave(&continuing.chat, &flag);
                    if let Err(said) = outcome {
                        eprintln!("goal {}: {said}", continuing.id);
                    }
                }
            }
            Err(said) => eprintln!("heartbeat: {said}"),
        }
        std::thread::sleep(Duration::from_secs(goal::TICK_SECS));
    }
}

/// Whether a daemon run holds a lease on this session. The gateway can ask the daemon the same question a
/// person's `/status` asks; a daemon that is not running is not busy, and the goal waits for it.
fn daemon_busy(socket: &Path, session: &str) -> bool {
    let Ok(response) = ask(socket, &json!({"op": "status"})) else {
        return false;
    };
    response
        .get("sessions")
        .and_then(Value::as_array)
        .is_some_and(|sessions| {
            sessions.iter().any(|entry| {
                entry.get("session").and_then(Value::as_str) == Some(session)
                    && entry.get("run").is_some()
            })
        })
}

/// Whether a child executor for this chat is still live. The record is written before the first model
/// call, so this covers the whole life of the child, not only the part after its first reply.
fn child_busy(state: &Path, chat: &Chat) -> bool {
    children::Registry::existing(state).is_some_and(|registry| {
        registry.records().is_ok_and(|records| {
            records.iter().any(|record| {
                record.chat == *chat
                    && (record.state == children::STATE_ADMITTED
                        || record.state == children::STATE_RUNNING)
            })
        })
    })
}

/// The child-lifecycle log a chat's surface writes to. Kept apart from `tg-<chat>`, which the daemon
/// appends to from another process for a run, and from `tg-<chat>-goals`, which the goal store owns.
/// The key is one function so the prompt, the registry and the continuation cannot drift apart: a
/// heartbeat-spawned child that named any other session would be invisible to the chat that owns it.
pub fn child_session(chat: &Chat) -> String {
    format!("{chat}-children")
}

/// The one place this surface builds a `World`. The person's chat runs as `act`; a scheduled job carries
/// the mode it was created with. Two literals drifted once already, and the mode is exactly the kind of
/// field that goes missing from one of them.
fn world_for(
    settings: &Settings,
    state: &Path,
    chat: Chat,
    mode: crate::mode::Mode,
    actor: &str,
    mcp: &Arc<crate::mcp::Mcp>,
) -> crate::agent::World {
    let skills_home = state.join("skills");
    crate::agent::World {
        root: PathBuf::from(&settings.root),
        vault: PathBuf::from(&settings.vault),
        db: state.join("mneme.db"),
        skills: vec![skills_home],
        sessions: vec![
            state.join("sessions"),
            state.join("gateway").join("tg").join("chat"),
            state.join("gateway").join("chat"),
        ],
        state: state.to_path_buf(),
        chat: chat.clone(),
        depth: 0,
        mode,
        role: crate::profile::Role::Patch,
        actor: actor.to_string(),
        // The chat's child-lifecycle log. Kept apart from `tg-<chat>`, which the daemon appends to from
        // another process for a run: two processes writing one log can compute the same next id, and
        // the session tree refuses a log with a duplicate id outright. The goal store has its own log
        // (`tg-<chat>-goals`) for the same reason.
        session: child_session(&chat),
        inbox: None,
        mcp: Arc::clone(mcp),
        allowance: crate::agent::Allowance::new(64),
        // A gateway chat has a person behind it, but no ask bar yet: the floor refuses rather than waits.
        questions: None,
    }
}

fn scheduled(
    job: &crate::cron::Job,
    settings: &Settings,
    state: &Path,
    step: &crate::retry::Step,
    mcp: &Arc<crate::mcp::Mcp>,
) -> Result<String, String> {
    let preset = crate::model::preset(&settings.model)
        .ok_or_else(|| format!("no model preset named `{}`", settings.model))?;
    let client = crate::model::from_preset(&preset);
    let workspace = PathBuf::from(&settings.root);
    let world = world_for(
        settings,
        state,
        job.chat.clone(),
        job.mode,
        &format!("job:{}", job.id),
        mcp,
    );
    let mut messages = job_messages(job, &workspace, step);
    let silent = |_: crate::model::Feed, _: &str| {};
    let nothing = |_: crate::agent::Activity, _: &str| {};
    crate::agent::answer(&client, &mut messages, &world, &silent, &nothing, 16)
        .map_err(|fault| format!("{fault:?}"))
}

/// What a scheduled run is told. Separate from the call so the retry line can be asserted without a
/// model, and because this is the only place the occurrence key reaches the model.
fn job_messages(job: &crate::cron::Job, workspace: &Path, step: &crate::retry::Step) -> Vec<Value> {
    let mode_line = match job.mode {
        crate::mode::Mode::Read => {
            "This job is read-only: it has no shell, no file writes, no browser and no scheduling. Read and answer with what you found."
        }
        crate::mode::Mode::Plan => {
            "This job is read-only and produces a plan: no shell, no file writes, no browser and no scheduling. Investigate, ask at real forks, then answer with the plan and the checks it would run."
        }
        crate::mode::Mode::Act => {
            "This job may act on the machine. The hard floor still applies: no permanent delete, money movement, credential or permission change, new security access, or message to a person without a person deciding."
        }
    };
    let mut brief = String::new();
    for (label, value) in [
        ("Inputs", &job.inputs),
        ("Outputs", &job.outputs),
        ("Done when", &job.done_when),
        ("Stop when", &job.stop_when),
        ("Never", &job.never),
    ] {
        if !value.is_empty() {
            brief.push_str(&format!("{label}: {value}\n"));
        }
    }
    let brief = if brief.is_empty() {
        String::new()
    } else {
        format!("The standing goal file says:\n{brief}\n")
    };
    // A retry is the same occurrence, not a new job. Say so, and carry the failure that caused it: a
    // model told only "try again" repeats the step that already ran.
    let retry_line = if step.attempt > 1 {
        format!(
            "This is attempt {} of {} for the same scheduled occurrence ({}). The previous attempt failed: {}\nDo not repeat an action that already succeeded; finish what is missing.\n\n",
            step.attempt,
            crate::retry::ATTEMPTS,
            step.window,
            step.previous
        )
    } else {
        String::new()
    };
    vec![
        json!({"role": "system", "content": system_prompt(workspace)}),
        json!({
            "role": "user",
            "content": format!(
                "This is a scheduled job you set up earlier. Do it now and answer with the result, not with a plan.\n\n{mode_line}\n\n{retry_line}{brief}{}",
                job.goal
            )
        }),
    ]
}

/// The stop path's one job: join every executor this gateway started and report the count. `drain`
/// waits for the children rather than killing a model call, so this can take as long as the work does.
fn stop_gateway(children: &children::Registry) -> usize {
    let live = children.live();
    let drained = children.drain();
    eprintln!("children: drained {drained} executor(s) at shutdown; {live} were live");
    drained
}

/// The gateway's long-lived half: one recovery pass, one lease, one scheduler and one heartbeat. A
/// surface builds it once and may call [`Runtime::run`] again after a transport failure without starting a
/// second set of background threads or replaying recovery against a live process.
pub struct Runtime {
    socket: PathBuf,
    state: PathBuf,
    settings: Settings,
    children: children::Registry,
    signals: Arc<Signals>,
    mcp: Arc<crate::mcp::Mcp>,
    /// Held for the life of the runtime. Dropping it releases the state directory for the next process.
    _lease: lease::Lease,
}

impl Runtime {
    /// Take the state directory and, when `background` is given, start the scheduler and the heartbeat
    /// against that transport. The transport is a clone of the surface's own handle: background work posts
    /// into the same chats, so it has to outlive a reconnect. A surface that reconnects keeps one handle and
    /// reconnects inside it, rather than handing the runtime a fresh one.
    pub fn new<T: Transport + Clone + Send + 'static>(
        surface: &str,
        socket: PathBuf,
        state: PathBuf,
        settings: Settings,
        background: Option<T>,
    ) -> Result<Runtime, String> {
        let lease = lease::Lease::take(&state, surface)?;
        // A child is a thread in this process, so records a dead process left behind name work that ended
        // with it. Recovery marks them lost before the gateway admits anything new; nothing is resumed.
        let children = children::Registry::new(&state);
        match children.recover() {
            Ok(report) if report.files == 0 => {}
            Ok(report) => {
                if report.marked > 0 {
                    eprintln!(
                        "children: {} live child(ren) from a previous gateway are marked lost",
                        report.marked
                    );
                }
                for path in &report.unreadable {
                    eprintln!("children: {path}: the record would not parse; left in place");
                }
                for said in &report.failed {
                    eprintln!("children: could not mark a lost child: {said}");
                }
                debug_assert_eq!(report.accounted(), report.files);
            }
            Err(said) => eprintln!("children: could not recover the registry: {said}"),
        }
        // The old single-list shape becomes one file per job before anything schedules it.
        match crate::cron::migrate(&state) {
            Ok(0) => {}
            Ok(count) => eprintln!(
                "cron: imported {count} job(s) into {}",
                crate::cron::dir(&state).display()
            ),
            Err(said) => eprintln!("cron: could not import the old job list: {said}"),
        }
        // One registry for the life of the gateway: a server started for one message is the server the next
        // message reaches, which is what makes an MCP server with state worth configuring.
        let mcp = Arc::new(crate::mcp::Mcp::new(
            settings.mcp.clone(),
            Some(PathBuf::from(&settings.root)),
        ));
        let signals = Arc::new(Signals::default());
        if let Some(transport) = background {
            {
                let worker = transport.clone();
                let settings = settings.clone();
                let state = state.clone();
                let mcp = Arc::clone(&mcp);
                std::thread::spawn(move || scheduler(worker, settings, state, mcp));
            }
            {
                // The heartbeat is a second thread beside the scheduler, not a cron schedule: it continues
                // a conversation that already exists, and cron's shape is a new run that reports into a chat.
                let worker = transport.clone();
                let settings = settings.clone();
                let state = state.clone();
                let socket = socket.clone();
                let mcp = Arc::clone(&mcp);
                let signals = Arc::clone(&signals);
                std::thread::spawn(move || heartbeat(worker, settings, state, socket, mcp, signals));
            }
        }
        Ok(Runtime {
            socket,
            state,
            settings,
            children,
            signals,
            mcp,
            _lease: lease,
        })
    }

    /// Poll one transport until it fails, `/shutdown` ends the surface, or `--once` has drained its batch.
    /// A transport error is returned to the caller; calling `run` again on the same runtime resumes without
    /// duplicating background work.
    pub fn run<T: Transport + Clone + Send + 'static>(
        &mut self,
        mut transport: T,
        once: bool,
    ) -> Result<(), String> {
        let socket = self.socket.clone();
        let state = self.state.clone();
        let settings = self.settings.clone();
        let signals = Arc::clone(&self.signals);
        let mcp = Arc::clone(&self.mcp);
        let mut offset = load_offset(&state);
        let mut pending: Vec<std::thread::JoinHandle<()>> = Vec::new();
        loop {
            if signals.stopped() {
                // `/shutdown` is the end of this gateway's ownership. Join the children it admitted and
                // say how many, instead of returning while their threads are still attached to a process
                // that is about to exit. `drain` waits for them; it does not kill a model call.
                stop_gateway(&self.children);
                return Ok(());
            }
            let updates = transport.updates(offset, 50)?;
            for update in &updates {
                offset = Some(offset.map_or(update.id + 1, |held| held.max(update.id + 1)));
                if !settings.allowed.contains(&update.from) {
                    continue;
                }
                let (name, goal) = command(&update.text);
                if update.callback.is_none() && name == "/run" && !goal.is_empty() {
                    let mut worker = transport.clone();
                    let socket = socket.clone();
                    let state = state.clone();
                    let settings = settings.clone();
                    let update = update.clone();
                    let signals = Arc::clone(&signals);
                    let mcp = Arc::clone(&mcp);
                    let handle = std::thread::spawn(move || {
                        if let Err(said) = handle_with(
                            &update,
                            &settings,
                            &socket,
                            &state,
                            &mut worker,
                            &signals,
                            &mcp,
                        ) {
                            let _ = worker.send(
                                &update.chat,
                                &clip(&format!("the run failed: {said}"), worker.limit()),
                                &[],
                            );
                        }
                    });
                    if once {
                        pending.push(handle);
                    }
                } else if update.callback.is_some() {
                    let mut worker = transport.clone();
                    let socket = socket.clone();
                    let state = state.clone();
                    let settings = settings.clone();
                    let update = update.clone();
                    let signals = Arc::clone(&signals);
                    let mcp = Arc::clone(&mcp);
                    let handle = std::thread::spawn(move || {
                        if let Err(said) = handle_with(
                            &update,
                            &settings,
                            &socket,
                            &state,
                            &mut worker,
                            &signals,
                            &mcp,
                        ) {
                            eprintln!("gateway: {said}");
                        }
                    });
                    if once {
                        pending.push(handle);
                    }
                } else if update.callback.is_none() && !update.text.trim().starts_with('/') {
                    // A conversation is work: it can stream for a minute, and a poller parked inside it
                    // cannot receive the `/stop` that would end it. One thread per chat, serialized by the
                    // chat's lock so two messages do not interleave their history.
                    let mut worker = transport.clone();
                    let socket = socket.clone();
                    let state = state.clone();
                    let settings = settings.clone();
                    let update = update.clone();
                    let signals = Arc::clone(&signals);
                    let mcp = Arc::clone(&mcp);
                    let handle = std::thread::spawn(move || {
                        let lock = signals.lock(&update.chat);
                        let _held = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                        let flag = signals.begin(&update.chat);
                        if let Err(said) = handle_with(
                            &update,
                            &settings,
                            &socket,
                            &state,
                            &mut worker,
                            &signals,
                            &mcp,
                        ) {
                            let _ = worker.send(
                                &update.chat,
                                &clip(&format!("the answer failed: {said}"), worker.limit()),
                                &[],
                            );
                        }
                        signals.leave(&update.chat, &flag);
                    });
                    if once {
                        pending.push(handle);
                    }
                } else if let Err(said) = handle_with(
                    update,
                    &settings,
                    &socket,
                    &state,
                    &mut transport,
                    &signals,
                    &mcp,
                ) {
                    eprintln!("gateway: {said}");
                }
            }
            if let Some(id) = offset {
                save_offset(&state, id)?;
            }
            if once {
                // `--once` means the batch is handled to the end, not merely accepted. The workers hold
                // their own transport clones, so joining them here is what makes a scripted test
                // deterministic.
                for handle in pending.drain(..) {
                    let _ = handle.join();
                }
                return Ok(());
            }
            if updates.is_empty() {
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
}

/// Telegram, served the way it always was: one runtime, one transport, one loop.
pub fn serve<T: Transport + Clone + Send + 'static>(
    transport: T,
    socket: PathBuf,
    state: PathBuf,
    settings: Settings,
    once: bool,
) -> Result<(), String> {
    let mut runtime = Runtime::new("telegram", socket, state, settings, Some(transport.clone()))?;
    runtime.run(transport, once)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Settings {
        Settings {
            allowed: vec!["7".to_string()],
            policy: "guarded".to_string(),
            verify: "true".to_string(),
            model: "none".to_string(),
            root: "/work".to_string(),
            vault: "/work/mneme".to_string(),
            heartbeat: goal::DEFAULT_HEARTBEAT_SECS,
            mcp: crate::mcp::McpConfig::default(),
        }
    }

    #[test]
    fn a_scheduled_job_carries_its_mode_into_the_world() {
        let state = Path::new("/work/state");
        let mcp = Arc::new(crate::mcp::Mcp::empty());
        let read = world_for(
            &settings(),
            state,
            Chat::telegram(7),
            crate::mode::Mode::Read,
            "chat:7",
            &mcp,
        );
        assert_eq!(read.mode, crate::mode::Mode::Read);
        assert_eq!(read.chat, Chat::telegram(7));
        let act = world_for(&settings(), state, Chat::telegram(8), crate::mode::Mode::Act, "job:2", &mcp);
        assert_eq!(act.mode, crate::mode::Mode::Act);
        assert_eq!(act.actor, "job:2");
        assert!(act.mode.acts());
    }

    fn job() -> crate::cron::Job {
        crate::cron::Job {
            id: 2,
            chat: Chat::telegram(7),
            schedule: "every 6h".to_string(),
            mode: crate::mode::Mode::Read,
            goal: "Summarise the feed".to_string(),
            inputs: "the notes under 23".to_string(),
            outputs: "a digest".to_string(),
            done_when: "five items".to_string(),
            stop_when: "one pass".to_string(),
            until: String::new(),
            never: "do not post".to_string(),
            last: 0,
        }
    }

    fn prompt(step: &crate::retry::Step) -> String {
        let messages = job_messages(&job(), Path::new("/work"), step);
        messages[1]["content"]
            .as_str()
            .expect("the user message")
            .to_string()
    }

    #[test]
    fn a_first_attempt_is_not_told_it_is_a_retry() {
        let said = prompt(&crate::retry::Step {
            window: "2@1000".to_string(),
            attempt: 1,
            previous: String::new(),
        });
        assert!(said.contains("Summarise the feed"), "{said}");
        assert!(said.contains("Inputs: the notes under 23"), "{said}");
        assert!(said.contains("Never: do not post"), "{said}");
        assert!(!said.contains("previous attempt failed"), "{said}");
    }

    #[test]
    fn a_retry_carries_the_occurrence_and_the_failure() {
        let said = prompt(&crate::retry::Step {
            window: "2@1000".to_string(),
            attempt: 2,
            previous: "the endpoint refused the request".to_string(),
        });
        assert!(said.contains("attempt 2 of 3"), "{said}");
        assert!(said.contains("2@1000"), "the key reaches the model: {said}");
        assert!(said.contains("the endpoint refused the request"), "{said}");
        assert!(
            said.contains("Do not repeat an action that already succeeded"),
            "{said}"
        );
        assert!(said.contains("Summarise the feed"), "{said}");
    }

    fn scratch(name: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or(0);
        let base = std::env::temp_dir().join(format!(
            "hades-gateway-{name}-{}-{stamp}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("scratch");
        base
    }

    /// A0 fix: `/shutdown` must join the children this gateway admitted and say how many it drained,
    /// rather than returning while their threads are still attached.
    #[test]
    fn the_stop_path_drains_and_reports_the_children() {
        let state = scratch("stop-drain");
        let mcp = Arc::new(crate::mcp::Mcp::empty());
        let registry = children::Registry::new(&state);
        registry
            .admit(
                children::Admission {
                    id: "spawn-stop".to_string(),
                    parent: "tg-7-children".to_string(),
                    session: "spawn-stop".to_string(),
                    chat: Chat::telegram(7),
                    goal: "say done".to_string(),
                    schema: children::Admission::report_schema(),
                    steps: 2,
                },
                world_for(
                    &settings(),
                    &state,
                    Chat::telegram(7),
                    crate::mode::Mode::Act,
                    "chat:7",
                    &mcp,
                ),
                crate::model::OpenAi::new("http://127.0.0.1:9/v1/chat/completions", "stub"),
            )
            .expect("a child to drain");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while registry.live() > 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "the child did not finish"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        assert_eq!(
            stop_gateway(&registry),
            1,
            "the stop path joins the child it admitted"
        );
        assert_eq!(registry.live(), 0);
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn a_chat_answer_makes_the_heartbeat_see_the_session_as_busy() {
        let signals = Signals::default();
        assert!(!signals.busy(&Chat::telegram(7)), "an idle chat is not busy");
        let lock = signals.lock(&Chat::telegram(7));
        let _held = lock.lock().expect("the chat lock");
        assert!(
            signals.busy(&Chat::telegram(7)),
            "a worker holds the chat lock for its whole life"
        );
    }

    #[test]
    fn the_heartbeat_interval_is_configurable_and_defaults_to_fifteen_minutes() {
        let dir = scratch("settings-heartbeat");
        let path = dir.join("config.json");
        let base = r#"{
            "gateway": {"telegram": {"allowed_users": [7], "verify": "true"}},
            "model": "nanbeige-3b",
            "root": "/work",
            "vault": "/work/mneme"
        }"#;
        std::fs::write(&path, base).expect("a config");
        assert_eq!(
            super::settings(&path).expect("settings").heartbeat,
            goal::DEFAULT_HEARTBEAT_SECS,
            "the default is fifteen minutes"
        );

        let configured = base.replace(
            r#""gateway": {"telegram""#,
            r#""gateway": {"heartbeat": 60, "telegram""#,
        );
        std::fs::write(&path, configured).expect("a configured interval");
        assert_eq!(super::settings(&path).expect("settings").heartbeat, 60);

        let zero = base.replace(
            r#""gateway": {"telegram""#,
            r#""gateway": {"heartbeat": 0, "telegram""#,
        );
        std::fs::write(&path, zero).expect("an impossible interval");
        let refused = super::settings(&path).expect_err("zero is refused");
        assert!(refused.contains("heartbeat"), "{refused}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_answer_is_split_on_boundaries_not_mid_word() {
        let text = "alpha beta gamma\n\ndelta epsilon zeta";
        let parts = super::split(text, 24);
        assert!(parts.iter().all(|p| p.chars().count() <= 24), "{parts:?}");
        let before: Vec<&str> = text.split_whitespace().collect();
        let after: Vec<&str> = parts.iter().flat_map(|p| p.split_whitespace()).collect();
        assert_eq!(before, after, "{parts:?}");
    }

    #[test]
    fn a_wall_of_text_still_splits_at_the_limit() {
        let wall = "x".repeat(50);
        let parts = super::split(&wall, 10);
        assert_eq!(parts.len(), 5);
        assert!(parts.iter().all(|p| p.chars().count() <= 10));
    }

    #[test]
    fn a_split_never_cuts_a_character_in_half() {
        let text = "é".repeat(30);
        let parts = super::split(&text, 7);
        assert!(parts.iter().all(|p| p.chars().count() <= 7), "{parts:?}");
        assert_eq!(
            parts.iter().map(|p| p.chars().count()).sum::<usize>(),
            30,
            "{parts:?}"
        );
    }

    #[test]
    fn a_chat_written_before_surfaces_is_still_read_and_saved_to_the_new_path() {
        let dir = std::env::temp_dir().join(format!(
            "hades-chat-legacy-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(dir.join("gateway").join("chat")).expect("an old chat dir");
        std::fs::write(
            dir.join("gateway").join("chat").join("7.json"),
            r#"[{"role":"user","content":"hello from before"}]"#,
        )
        .expect("an old history");

        let chat = Chat::telegram(7);
        let history = load_chat(&dir, &chat);
        assert_eq!(history.len(), 1, "the old file is read once");
        assert_eq!(history[0]["content"], "hello from before");

        save_chat(&dir, &chat, &history);
        assert!(
            dir.join("gateway").join("tg").join("chat").join("7.json").exists(),
            "a save goes to the surface path"
        );
        assert!(
            dir.join("gateway").join("chat").join("7.json").exists(),
            "the old file is copied, never moved"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
