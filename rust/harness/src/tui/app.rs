use super::{
    backend::{self, Event, Options, Overview, Row},
    editor::Editor,
    quota::Quota,
};
use crossterm::event::{
    Event as InputEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
use ratatui::layout::Rect;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc::{Receiver, SyncSender, sync_channel},
};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Chat,
    Sessions,
    Jobs,
    Approvals,
    Activity,
    Skills,
    Usage,
}
impl View {
    pub const ALL: [View; 7] = [
        Self::Chat,
        Self::Sessions,
        Self::Jobs,
        Self::Approvals,
        Self::Activity,
        Self::Skills,
        Self::Usage,
    ];
    pub fn name(self) -> &'static str {
        match self {
            Self::Chat => "Conversation",
            Self::Sessions => "Sessions",
            Self::Jobs => "Jobs",
            Self::Approvals => "Approvals",
            Self::Activity => "Activity",
            Self::Skills => "Skills",
            Self::Usage => "Usage",
        }
    }
}
#[derive(Clone, Debug)]
pub struct Message {
    pub role: String,
    pub text: String,
    pub detail: String,
    pub status: String,
}
impl Message {
    pub fn new(role: &str, text: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            text: text.into(),
            detail: String::new(),
            status: String::new(),
        }
    }
    pub fn json(&self) -> Value {
        json!({"role":self.role,"text":self.text,"detail":self.detail,"status":self.status})
    }
    pub fn from_json(value: &Value) -> Self {
        Self {
            role: value["role"].as_str().unwrap_or("system").into(),
            text: value["text"].as_str().unwrap_or("").into(),
            detail: value["detail"].as_str().unwrap_or("").into(),
            status: value["status"].as_str().unwrap_or("").into(),
        }
    }
}
#[derive(Clone, Debug)]
pub enum Overlay {
    Help,
    Palette,
    Models,
    Find,
    Detail(String, String),
    Approval(Row),
    CancelJob(Row),
}

pub const COMMANDS: &[(&str, &str)] = &[
    ("/new", "Start a new conversation"),
    ("/sessions", "Browse and resume conversations"),
    ("/model", "Choose a model preset"),
    ("/mode read", "Switch to read-only tools"),
    ("/mode act", "Enable action tools under the runtime's rules"),
    ("/run ", "Submit a checked run to the daemon"),
    ("/jobs", "Inspect scheduled work"),
    ("/approvals", "Review pending decisions"),
    ("/activity", "Inspect the action ledger"),
    ("/skills", "Browse installed procedures"),
    ("/usage", "View provider quota and session tokens"),
    ("/export", "Export this conversation as text"),
    ("/stop", "Request cancellation"),
    ("/motion", "Toggle flame animation"),
    ("/clear", "Clear the view; preserve the saved conversation"),
    ("/help", "Keyboard shortcuts and commands"),
    ("/quit", "Save and close"),
];

pub struct App {
    pub options: Options,
    pub view: View,
    pub messages: Vec<Message>,
    pub input: Editor,
    pub overlay: Option<Overlay>,
    pub query: Editor,
    pub selected: usize,
    pub scroll: usize,
    pub max_scroll: usize,
    pub follow: bool,
    pub inspector: bool,
    pub show_tools: bool,
    pub motion: bool,
    pub search: String,
    pub id: String,
    pub title: String,
    pub transcript: Vec<Value>,
    pub overview: Overview,
    pub quota: Quota,
    pub update: String,
    pub busy: bool,
    pub governed: Option<String>,
    pub mode: crate::mode::Mode,
    pub reasoning: String,
    pub current_tool: String,
    pub turn_started: Option<Instant>,
    pub status: String,
    pub quit: bool,
    pub exit_error: Option<String>,
    pub closing: bool,
    pub usage: Arc<Mutex<Vec<crate::task::Usage>>>,
    pub saved_input_tokens: u64,
    pub saved_output_tokens: u64,
    pub tabs_area: Rect,
    pub content_area: Rect,
    pub tab_hits: Vec<(Rect, View)>,
    pub visible_from: usize,
    pub chat_cache: Option<super::ui::ChatCache>,
    pub overlay_scroll: u16,
    sender: SyncSender<Event>,
    receiver: Receiver<Event>,
    stop_flag: Arc<AtomicBool>,
    refreshing: bool,
    quota_pending: bool,
    quota_checked: Option<Instant>,
    update_started: bool,
    active_assistant: Option<usize>,
    tool_start: usize,
    history: Vec<String>,
    history_at: Option<usize>,
    draft: String,
}

impl App {
    pub fn new(options: Options) -> Self {
        let (sender, receiver) = sync_channel(256);
        let provider = options.provider();
        let motion = options.motion;
        let mut app = Self {
            options,
            view: View::Chat,
            messages: vec![],
            input: Editor::default(),
            overlay: None,
            query: Editor::default(),
            selected: 0,
            scroll: 0,
            max_scroll: 0,
            follow: true,
            inspector: true,
            show_tools: false,
            motion,
            search: String::new(),
            id: backend::new_id(),
            title: "Ready for your next task".into(),
            transcript: Vec::new(),
            overview: Overview::default(),
            quota: Quota::pending(provider),
            update: "main · checking…".into(),
            busy: false,
            governed: None,
            mode: crate::mode::Mode::Act,
            reasoning: String::new(),
            current_tool: String::new(),
            turn_started: None,
            status: "Ready · Enter to send · Ctrl+P for commands".into(),
            quit: false,
            exit_error: None,
            closing: false,
            usage: Arc::new(Mutex::new(Vec::new())),
            saved_input_tokens: 0,
            saved_output_tokens: 0,
            tabs_area: Rect::default(),
            content_area: Rect::default(),
            tab_hits: Vec::new(),
            visible_from: 0,
            chat_cache: None,
            overlay_scroll: 0,
            sender,
            receiver,
            stop_flag: Arc::new(AtomicBool::new(false)),
            refreshing: false,
            quota_pending: false,
            quota_checked: None,
            update_started: false,
            active_assistant: None,
            tool_start: 0,
            history: Vec::new(),
            history_at: None,
            draft: String::new(),
        };
        if app.options.demo {
            app.seed_demo();
        }
        app
    }
    fn seed_demo(&mut self) {
        self.options.user = "YOU".into();
        self.id = "demo-workspace".into();
        self.title = "Harden the job runner and verify restart recovery".into();
        self.update = "main · up to date [demo]".into();
        self.messages = vec![
            Message::new("user", "Make the job runner recover cleanly after a restart. Keep the change small, verify it, and tell me what changed."),
            Message::new("assistant", "I’ll trace the retry state, make the recovery change, and run the focused checks.\n\n1. Inspect the scheduler and persisted attempts.\n2. Preserve the pending retry across restarts.\n3. Verify recovery and report the result."),
            Message { role: "tool".into(), text: "shell · cargo test -p harness retry::tests".into(), detail: "running 8 tests\ntest retry::tests::pending_retry_survives_restart ... ok\n\ntest result: ok. 8 passed; 0 failed".into(), status: "complete".into() },
            Message::new("assistant", "The pending retry now survives a restart. The scheduler resumes the existing attempt budget.\n\n```rust\nlet pending = retry::read(&state, job.id);\nif let Some(step) = retry::step(&job, pending.as_ref(), now) {\n    run_attempt(step)?;\n}\n```\n\nChecks passed. The change is ready for review."),
        ];
        self.quota = super::quota::parse("Ollama Cloud", &json!({"included":{"balance_usd":45.6,"allowance_usd":60.0,"period":{"until":"2026-11-01T00:00:00Z"}},"purchased":{"balance_usd":0.0}})).expect("demo quota");
        self.quota.note.push_str(" · DEMO DATA");
        self.saved_input_tokens = 12480;
        self.saved_output_tokens = 1842;
        self.overview = Overview { daemon: true,
            sessions: vec![Row { id:"demo-workspace".into(), title:self.title.clone(), detail:"Ollama Cloud · deepseek-v4.1-flash".into(), status:"current".into() }],
            jobs: vec![Row { id:"1".into(), title:"Review overnight build results".into(), detail:"Read-only · report failures with links\nDone when: every failing check is accounted for".into(), status:"daily 07:30".into() }],
            skills: vec![Row { id:"run-and-verify".into(), title:"run-and-verify".into(), detail:"Execute the task, run the acceptance check, and leave a clear result.".into(), status:"built-in".into() }],
            activity: vec![Row { id:"1".into(), title:"shell: cargo test -p harness retry::tests".into(), detail:"The action passed the runtime's gates.".into(), status:"allow".into() }], ..Overview::default()
        };
        self.status = "DEMO · offline preview · no actions or model calls".into();
    }
    pub fn restore_initial(&mut self) -> Result<(), String> {
        if let Some(id) = self.options.resume.clone() {
            return self.resume(&id);
        }
        let overview = backend::overview(&self.options);
        if let Some(recent) = overview.sessions.first() {
            self.title = format!("Recent: {}", recent.title);
        }
        self.overview = overview;
        Ok(())
    }
    pub fn rows(&self) -> &[Row] {
        match self.view {
            View::Sessions => &self.overview.sessions,
            View::Jobs => &self.overview.jobs,
            View::Approvals => &self.overview.approvals,
            View::Activity => &self.overview.activity,
            View::Skills => &self.overview.skills,
            _ => &[],
        }
    }
    pub fn switch(&mut self, view: View) {
        self.view = view;
        self.selected = 0;
        self.scroll = 0;
        self.follow = true;
    }
    pub fn tokens(&self) -> (u64, u64, usize) {
        let log = self.usage.lock().unwrap_or_else(|e| e.into_inner());
        (
            self.saved_input_tokens + log.iter().map(|u| u.prompt_tokens).sum::<u64>(),
            self.saved_output_tokens + log.iter().map(|u| u.generated_tokens).sum::<u64>(),
            log.len(),
        )
    }
    pub fn refresh(&mut self) {
        if self.options.demo {
            return;
        }
        if !self.refreshing {
            self.refreshing = true;
            let options = self.options.clone();
            let tx = self.sender.clone();
            std::thread::spawn(move || {
                let _ = tx.send(Event::Overview(backend::overview(&options)));
            });
        }
        if !self.quota_pending
            && self
                .quota_checked
                .is_none_or(|t| t.elapsed() >= Duration::from_secs(60))
        {
            self.quota_pending = true;
            let options = self.options.clone();
            let tx = self.sender.clone();
            std::thread::spawn(move || {
                let _ = tx.send(Event::Quota(super::quota::poll(&options)));
            });
        }
        if !self.update_started {
            self.update_started = true;
            let tx = self.sender.clone();
            std::thread::spawn(move || {
                let _ = tx.send(Event::Update(backend::update_status()));
            });
        }
    }
    pub fn poll(&mut self) {
        for _ in 0..1024 {
            let Ok(event) = self.receiver.try_recv() else {
                break;
            };
            self.apply(event);
        }
    }
    pub fn apply(&mut self, event: Event) {
        if matches!(
            &event,
            Event::Delta(_) | Event::Tool(..) | Event::Finished { .. }
        ) {
            self.chat_cache = None;
        }
        match event {
            Event::Delta(text) => {
                let index = *self.active_assistant.get_or_insert_with(|| {
                    self.messages.push(Message::new("assistant", ""));
                    self.messages.len() - 1
                });
                if self.messages[index].text.len() < 2_000_000 {
                    self.messages[index].text.push_str(&backend::clean(&text));
                }
            }
            Event::Reasoning(text) => {
                if self.reasoning.len() < 100_000 {
                    self.reasoning.push_str(&backend::clean(&text));
                }
            }
            Event::Tool(start, label) => {
                self.active_assistant = None;
                if start {
                    self.current_tool = backend::clean(&label);
                    self.messages.push(Message {
                        role: "tool".into(),
                        text: backend::clean(&label),
                        detail: String::new(),
                        status: "running".into(),
                    });
                } else if let Some(message) = self.messages.iter_mut().rev().find(|m| {
                    m.role == "tool" && m.status == "running" && m.text == backend::clean(&label)
                }) {
                    message.status = "complete".into();
                    self.current_tool.clear();
                } else {
                    self.messages
                        .push(Message::new("tool", backend::clean(&label)));
                }
            }
            Event::Finished { answer, transcript } => {
                self.busy = false;
                self.current_tool.clear();
                if !transcript.is_empty() {
                    let start = transcript
                        .iter()
                        .rposition(|m| m["role"] == "user")
                        .unwrap_or(0);
                    let mut labels = std::collections::HashMap::new();
                    for message in &transcript[start..] {
                        if let Some(calls) = message["tool_calls"].as_array() {
                            for call in calls {
                                labels.insert(
                                    call["id"].as_str().unwrap_or(""),
                                    backend::clean(&crate::agent::label(
                                        call["function"]["name"].as_str().unwrap_or(""),
                                        call["function"]["arguments"].as_str().unwrap_or(""),
                                    )),
                                );
                            }
                        }
                    }
                    for result in transcript[start..].iter().filter(|m| m["role"] == "tool") {
                        if let Some(label) =
                            labels.get(result["tool_call_id"].as_str().unwrap_or(""))
                            && let Some(message) =
                                self.messages[self.tool_start..].iter_mut().find(|m| {
                                    m.role == "tool" && m.detail.is_empty() && &m.text == label
                                })
                        {
                            message.detail =
                                backend::clean(result["content"].as_str().unwrap_or(""));
                        }
                    }
                    self.transcript = transcript;
                }
                for message in self.messages[self.tool_start..]
                    .iter_mut()
                    .filter(|m| m.status == "running")
                {
                    message.status = if matches!(&answer, Ok(None)) {
                        "cancelled"
                    } else if answer.is_err() {
                        "interrupted"
                    } else {
                        "finished"
                    }
                    .into();
                }
                match answer {
                    Ok(Some(text)) => {
                        if let Some(index) = self.active_assistant.take() {
                            self.messages[index].text = backend::clean(&text);
                        } else {
                            self.messages
                                .push(Message::new("assistant", backend::clean(&text)));
                        }
                        self.status = "Turn complete · session saved".into();
                    }
                    Ok(None) => {
                        self.messages.push(Message::new(
                            "system",
                            "Turn cancelled at the runtime's next safe boundary.",
                        ));
                        self.status = "Cancelled".into();
                    }
                    Err(error) => {
                        self.messages
                            .push(Message::new("error", backend::clean(&error)));
                        self.status = "Turn failed · see the error above".into();
                    }
                }
                self.active_assistant = None;
                if let Err(error) = self.save() {
                    self.status = format!("Could not save session: {error}");
                }
                self.refresh();
                if self.closing {
                    self.quit = true;
                }
            }
            Event::Overview(value) => {
                self.overview = value;
                self.refreshing = false;
                self.selected = self.selected.min(self.rows().len().saturating_sub(1));
            }
            Event::Quota(quota) => {
                if quota.provider == self.options.provider() {
                    self.quota = quota;
                    self.quota_checked = Some(Instant::now());
                }
                self.quota_pending = false;
            }
            Event::Update(status) => self.update = status,
            Event::Notice(text) => self.status = backend::clean(&text),
        }
    }
    pub fn save(&self) -> Result<(), String> {
        if self.options.demo || (self.messages.is_empty() && self.input.text.is_empty()) {
            return Ok(());
        }
        let (input, output, _) = self.tokens();
        backend::save_session(
            &self.options.state,
            &self.id,
            &json!({"version":1,"id":self.id,"root":self.options.root,"model":self.options.model,"title":self.title,"messages":self.messages.iter().map(Message::json).collect::<Vec<_>>(),"transcript":self.transcript,"input_tokens":input,"output_tokens":output,"in_progress":self.busy,"draft":self.input.text}),
        )
    }
    pub fn resume(&mut self, id: &str) -> Result<(), String> {
        if self.busy {
            return Err("Wait for the active turn before switching sessions.".into());
        }
        if self.options.demo {
            self.status = "Demo session is already open".into();
            self.switch(View::Chat);
            return Ok(());
        }
        let value = backend::read_json(&backend::session_path(&self.options.state, id)?)?;
        if value["version"] != 1 {
            return Err("unsupported terminal session format".into());
        }
        let root = value["root"].as_str().ok_or("session has no workspace")?;
        if std::path::Path::new(root) != self.options.root {
            return Err(format!(
                "That session belongs to {root}. Reopen with `hades tui --root {root} --resume {id}`."
            ));
        }
        self.save()?;
        // Resume into a new branch so two terminals never overwrite the same session file.
        self.id = backend::new_id();
        self.visible_from = 0;
        self.chat_cache = None;
        self.governed = None;
        self.title = value["title"]
            .as_str()
            .unwrap_or("Resumed conversation")
            .into();
        self.messages = value["messages"]
            .as_array()
            .ok_or("session has no messages")?
            .iter()
            .map(Message::from_json)
            .collect();
        self.transcript = value["transcript"].as_array().cloned().unwrap_or_default();
        self.input
            .set(backend::clean(value["draft"].as_str().unwrap_or("")));
        if value["in_progress"] == true {
            self.messages.push(Message::new("system", "The previous process ended during a turn. Partial output is preserved. Inspect the activity ledger before retrying; an action may have completed."));
        }
        self.saved_input_tokens = value["input_tokens"].as_u64().unwrap_or(0);
        self.saved_output_tokens = value["output_tokens"].as_u64().unwrap_or(0);
        self.usage = Arc::new(Mutex::new(Vec::new()));
        self.switch(View::Chat);
        self.status = format!("Resumed {id} · saved as a new session branch");
        Ok(())
    }
    pub fn stop(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if self.busy {
            self.status = "Stop requested · waiting for the current operation to finish".into();
            if let Some(session) = self.governed.clone() {
                let state = self.options.state.clone();
                let tx = self.sender.clone();
                std::thread::spawn(move || {
                    let result = backend::daemon_request(
                        &state,
                        json!({"op":"stop","session":session,"by":"tui"}),
                    );
                    if let Err(error) = result {
                        let _ = tx.send(Event::Notice(error));
                    }
                });
            }
        }
    }
    fn close(&mut self) {
        if self.busy {
            self.closing = true;
            self.stop();
        } else {
            self.quit = true;
        }
    }
    pub fn submit(&mut self, prompt: String) {
        let prompt = prompt.trim().to_string();
        if prompt.is_empty() {
            return;
        }
        if prompt.starts_with('/') {
            self.command(&prompt);
            return;
        }
        if self.busy {
            self.status = "A turn is running. Your draft is kept; Esc requests a stop.".into();
            self.input.set(prompt);
            return;
        }
        if self.options.demo {
            self.messages.push(Message::new("user", prompt));
            self.messages.push(Message::new("system", "This is the offline demo. Reopen without --demo to work with your configured provider."));
            return;
        }
        if let Err(error) = self.options.client() {
            self.messages.push(Message::new("error", error));
            self.input.set(prompt);
            return;
        }
        self.history.push(prompt.clone());
        self.history_at = None;
        self.title = prompt
            .lines()
            .next()
            .unwrap_or("Current task")
            .chars()
            .take(160)
            .collect();
        self.messages.push(Message::new("user", prompt.clone()));
        self.begin();
        self.governed = None;
        backend::start_chat(
            self.options.clone(),
            backend::ChatRequest {
                id: self.id.clone(),
                prompt,
                transcript: self.transcript.clone(),
                mode: self.mode,
            },
            Arc::clone(&self.usage),
            Arc::clone(&self.stop_flag),
            self.sender.clone(),
        );
    }
    fn begin(&mut self) {
        self.busy = true;
        self.closing = false;
        self.stop_flag = Arc::new(AtomicBool::new(false));
        self.active_assistant = None;
        self.tool_start = self.messages.len();
        self.reasoning.clear();
        self.turn_started = Some(Instant::now());
        self.status = "Hades is working · Esc to request a stop".into();
        self.switch(View::Chat);
        if let Err(error) = self.save() {
            self.status = format!("Could not checkpoint the turn: {error}");
        }
    }
    pub fn command(&mut self, text: &str) {
        let (name, argument) = text.split_once(' ').unwrap_or((text, ""));
        let argument = argument.trim();
        match name {
            "/help" => self.overlay = Some(Overlay::Help),
            "/quit" | "/exit" => self.close(),
            "/stop" => self.stop(),
            "/new" => {
                if self.busy {
                    self.status = "Finish or stop the active turn first".into();
                    return;
                }
                if let Err(e) = self.save() {
                    self.status = e;
                    return;
                }
                self.id = backend::new_id();
                self.messages.clear();
                self.transcript.clear();
                self.visible_from = 0;
                self.chat_cache = None;
                self.title = "Ready for your next task".into();
                self.saved_input_tokens = 0;
                self.saved_output_tokens = 0;
                self.usage = Arc::new(Mutex::new(Vec::new()));
                self.governed = None;
                self.switch(View::Chat);
            }
            "/sessions" => self.switch(View::Sessions),
            "/jobs" => self.switch(View::Jobs),
            "/approvals" => self.switch(View::Approvals),
            "/activity" => self.switch(View::Activity),
            "/skills" => self.switch(View::Skills),
            "/usage" => self.switch(View::Usage),
            "/resume" => {
                if let Err(error) = self.resume(argument) {
                    self.status = error;
                }
            }
            "/model" => {
                if self.busy {
                    self.status = "Finish the active turn before changing models".into();
                    return;
                }
                if argument.is_empty() {
                    self.open_overlay(Overlay::Models);
                } else if crate::model::preset(argument).is_some() {
                    self.options.model = argument.into();
                    self.options.endpoint = None;
                    self.options.key = None;
                    self.quota = Quota::pending(self.options.provider());
                    self.quota_checked = None;
                    self.refresh();
                    self.status = format!("Model changed to {argument} for this session");
                } else {
                    self.status =
                        "Unknown preset. Use --model NAME --endpoint URL for a custom provider."
                            .into();
                }
            }
            "/mode" => {
                if self.busy {
                    self.status = "Finish the active turn before changing tool mode".into();
                } else if let Some(mode) = crate::mode::Mode::parse(argument) {
                    self.mode = mode;
                    self.status = format!("Tool mode: {}", mode.name());
                } else {
                    self.status = "Usage: /mode read or /mode act".into();
                }
            }
            "/run" => {
                if self.busy {
                    self.status = "A turn is already running".into();
                    return;
                }
                if argument.is_empty() {
                    self.input.set("/run ");
                    return;
                }
                if self.options.demo {
                    self.status = "Governed execution is disabled in the offline demo".into();
                    return;
                }
                self.title = argument.chars().take(160).collect();
                self.messages
                    .push(Message::new("user", format!("Governed task: {argument}")));
                let session = self
                    .governed
                    .clone()
                    .unwrap_or_else(|| format!("{}-run", self.id));
                self.governed = Some(session.clone());
                self.begin();
                backend::governed(
                    self.options.clone(),
                    session,
                    argument.into(),
                    self.sender.clone(),
                );
            }
            "/export" => match self.export() {
                Ok(path) => self.status = format!("Exported {}", path.display()),
                Err(error) => self.status = error,
            },
            "/clear" => {
                if self.busy {
                    self.status = "Finish the active turn before clearing the view".into();
                } else if let Err(error) = self.save() {
                    self.status = error;
                } else {
                    self.visible_from = self.messages.len();
                    self.chat_cache = None;
                    self.scroll = 0;
                    self.follow = true;
                    self.status =
                        "View cleared · saved messages and model context preserved".into();
                }
            }
            "/motion" => {
                self.motion = !self.motion;
                self.status = format!("Flame animation {}", if self.motion { "on" } else { "off" });
            }
            _ => self.status = format!("Unknown command {name}. Ctrl+P opens the command palette."),
        }
    }
    fn export(&self) -> Result<PathBuf, String> {
        if self.options.demo {
            return Err("Exports are disabled in the offline demo".into());
        }
        let dir = self.options.state.join("tui/exports");
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let path = dir.join(format!("{}-{}.txt", self.id, backend::new_id()));
        let mut text = format!(
            "HADES AGENT\n{}\nWorkspace: {}\nModel: {}\n\n",
            self.title,
            self.options.root.display(),
            self.options.model
        );
        for message in &self.messages {
            text.push_str(&format!(
                "[{}]\n{}\n{}\n\n",
                message.role.to_uppercase(),
                backend::clean(&message.text),
                backend::clean(&message.detail)
            ));
        }
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| e.to_string())?;
        file.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
        Ok(path)
    }
    fn open_overlay(&mut self, overlay: Overlay) {
        self.overlay = Some(overlay);
        self.query = Editor::default();
        self.selected = 0;
        self.overlay_scroll = 0;
    }
    pub fn palette_items(&self) -> Vec<(String, String)> {
        let query = self.query.text.to_lowercase();
        if matches!(self.overlay, Some(Overlay::Models)) {
            crate::model::presets()
                .into_iter()
                .filter(|p| p.id.to_lowercase().contains(&query))
                .map(|p| (p.id.into(), p.note.into()))
                .collect()
        } else {
            COMMANDS
                .iter()
                .filter(|(c, d)| format!("{c} {d}").to_lowercase().contains(&query))
                .map(|(c, d)| (c.to_string(), d.to_string()))
                .collect()
        }
    }
    pub fn event(&mut self, event: InputEvent) {
        match event {
            InputEvent::Key(key) if key.kind != KeyEventKind::Release => self.key(key),
            InputEvent::Paste(text) => {
                if matches!(
                    self.overlay,
                    Some(Overlay::Palette | Overlay::Models | Overlay::Find)
                ) {
                    self.query.insert(&text.replace('\n', " "));
                } else if self.overlay.is_none() {
                    self.input.insert(&text);
                }
            }
            InputEvent::Mouse(mouse) if self.overlay.is_none() => match mouse.kind {
                MouseEventKind::ScrollUp => {
                    self.follow = false;
                    self.scroll = self.scroll.saturating_sub(4);
                }
                MouseEventKind::ScrollDown => {
                    self.scroll = (self.scroll + 4).min(self.max_scroll);
                    self.follow = self.scroll == self.max_scroll;
                }
                MouseEventKind::Down(crossterm::event::MouseButton::Left)
                    if self.tabs_area.contains((mouse.column, mouse.row).into()) =>
                {
                    if let Some((_, view)) = self
                        .tab_hits
                        .iter()
                        .find(|(area, _)| area.contains((mouse.column, mouse.row).into()))
                    {
                        self.switch(*view);
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
    fn key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('q') {
            self.close();
            return;
        }
        if ctrl && key.code == KeyCode::Char('c') {
            if self.busy {
                self.stop();
            } else if self.overlay.is_some() {
                self.overlay = None;
            } else {
                self.close();
            }
            return;
        }
        if self.overlay.is_some() {
            self.overlay_key(key);
            return;
        }
        match key.code {
            KeyCode::F(1) => self.open_overlay(Overlay::Help),
            KeyCode::F(2) => self.open_overlay(Overlay::Models),
            KeyCode::F(3) => self.switch(View::Sessions),
            KeyCode::F(4) => self.switch(View::Approvals),
            KeyCode::F(5) => self.refresh(),
            KeyCode::Char('p') if ctrl => self.open_overlay(Overlay::Palette),
            KeyCode::Char('f') if ctrl => self.open_overlay(Overlay::Find),
            KeyCode::Char('n') if ctrl => self.command("/new"),
            KeyCode::Char('e') if ctrl => self.command("/export"),
            KeyCode::Char('b') if ctrl => self.inspector = !self.inspector,
            KeyCode::Char('t') if ctrl => self.show_tools = !self.show_tools,
            KeyCode::Esc => {
                if self.busy {
                    self.stop();
                } else {
                    self.search.clear();
                    self.overlay = None;
                    self.switch(View::Chat);
                }
            }
            KeyCode::Tab | KeyCode::BackTab => {
                let index = View::ALL.iter().position(|v| v == &self.view).unwrap_or(0);
                let previous =
                    key.code == KeyCode::BackTab || key.modifiers.contains(KeyModifiers::SHIFT);
                self.switch(View::ALL[(index + if previous { 6 } else { 1 }) % 7]);
            }
            KeyCode::PageUp => {
                self.follow = false;
                self.scroll = self.scroll.saturating_sub(10);
            }
            KeyCode::PageDown => {
                self.scroll = (self.scroll + 10).min(self.max_scroll);
                self.follow = self.scroll == self.max_scroll;
            }
            KeyCode::End if ctrl => {
                self.follow = true;
                self.scroll = self.max_scroll;
            }
            KeyCode::Home if ctrl => {
                self.follow = false;
                self.scroll = 0;
            }
            KeyCode::Up if self.view != View::Chat => {
                self.selected = self.selected.saturating_sub(1)
            }
            KeyCode::Down if self.view != View::Chat => {
                self.selected = (self.selected + 1).min(self.rows().len().saturating_sub(1))
            }
            KeyCode::Enter if self.view != View::Chat => self.activate(),
            KeyCode::Char('d') if self.view == View::Jobs => {
                if let Some(row) = self.rows().get(self.selected).cloned() {
                    self.open_overlay(Overlay::CancelJob(row));
                }
            }
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) =>
            {
                self.input.insert("\n")
            }
            KeyCode::Char('j') if ctrl => self.input.insert("\n"),
            KeyCode::Enter => {
                let text = std::mem::take(&mut self.input.text);
                self.input.cursor = 0;
                self.submit(text);
            }
            KeyCode::Up if !self.input.text.contains('\n') => self.history_move(true),
            KeyCode::Down if !self.input.text.contains('\n') => self.history_move(false),
            _ => edit(&mut self.input, key),
        }
    }
    fn history_move(&mut self, up: bool) {
        if self.history.is_empty() {
            return;
        }
        if up {
            if self.history_at.is_none() {
                self.draft = self.input.text.clone();
            }
            let at = self
                .history_at
                .unwrap_or(self.history.len())
                .saturating_sub(1);
            self.history_at = Some(at);
            self.input.set(self.history[at].clone());
        } else if let Some(at) = self.history_at {
            if at + 1 < self.history.len() {
                self.history_at = Some(at + 1);
                self.input.set(self.history[at + 1].clone());
            } else {
                self.history_at = None;
                self.input.set(self.draft.clone());
            }
        }
    }
    fn activate(&mut self) {
        let Some(row) = self.rows().get(self.selected).cloned() else {
            return;
        };
        match self.view {
            View::Sessions => {
                if let Err(error) = self.resume(&row.id) {
                    self.status = error;
                }
            }
            View::Approvals => self.open_overlay(Overlay::Approval(row)),
            _ => self.open_overlay(Overlay::Detail(row.title, row.detail)),
        }
    }
    fn overlay_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Esc {
            self.overlay = None;
            self.query = Editor::default();
            return;
        }
        if key.code == KeyCode::PageDown {
            self.overlay_scroll = self.overlay_scroll.saturating_add(8);
            return;
        }
        if key.code == KeyCode::PageUp {
            self.overlay_scroll = self.overlay_scroll.saturating_sub(8);
            return;
        }
        match self.overlay.clone().expect("overlay") {
            Overlay::Palette | Overlay::Models => match key.code {
                KeyCode::Up => self.selected = self.selected.saturating_sub(1),
                KeyCode::Down => {
                    self.selected =
                        (self.selected + 1).min(self.palette_items().len().saturating_sub(1))
                }
                KeyCode::Enter => {
                    if let Some((command, _)) = self.palette_items().get(self.selected).cloned() {
                        let models = matches!(self.overlay, Some(Overlay::Models));
                        self.overlay = None;
                        if models {
                            self.command(&format!("/model {command}"));
                        } else if command.ends_with(' ') {
                            self.input.set(command);
                            self.switch(View::Chat);
                        } else {
                            self.command(&command);
                        }
                    }
                }
                _ => {
                    edit(&mut self.query, key);
                    self.selected = 0;
                }
            },
            Overlay::Find => {
                if key.code == KeyCode::Enter {
                    self.search = self.query.text.clone();
                    self.overlay = None;
                    self.switch(View::Chat);
                    self.follow = false;
                    self.scroll = 0;
                    self.status = format!("Highlighted {:?} · Esc clears", self.search);
                } else {
                    edit(&mut self.query, key);
                }
            }
            Overlay::Approval(row) => {
                let decision = match key.code {
                    KeyCode::Char('y') => Some(("allow", "once")),
                    KeyCode::Char('s') => Some(("allow", "session")),
                    KeyCode::Char('n') => Some(("deny", "once")),
                    _ => None,
                };
                if let Some((decision, scope)) = decision {
                    self.overlay = None;
                    if self.options.demo {
                        self.status = "Approvals are disabled in the demo".into();
                        return;
                    }
                    let Ok(id) = row.id.parse::<u64>() else {
                        self.status = "Invalid approval identity".into();
                        return;
                    };
                    let state = self.options.state.clone();
                    let tx = self.sender.clone();
                    std::thread::spawn(move || {
                        let result = backend::daemon_request(
                            &state,
                            json!({"op":"answer","id":id,"decision":decision,"scope":scope,"by":"tui"}),
                        );
                        let _ = tx.send(Event::Notice(match result {
                            Ok(_) => format!("Approval #{id}: {decision} ({scope})"),
                            Err(e) => e,
                        }));
                    });
                }
            }
            Overlay::CancelJob(row) => {
                if key.code == KeyCode::Char('y') {
                    self.overlay = None;
                    if self.options.demo {
                        self.status = "Job cancellation is disabled in the demo".into();
                        return;
                    }
                    let state = self.options.state.clone();
                    let tx = self.sender.clone();
                    std::thread::spawn(move || {
                        let result = row
                            .id
                            .parse::<u64>()
                            .map_err(|e| e.to_string())
                            .and_then(|id| crate::cron::cancel(&state, id, "tui"));
                        let _ = tx.send(Event::Notice(match result {
                            Ok(_) => "Standing job cancelled".into(),
                            Err(e) => e,
                        }));
                    });
                } else if key.code == KeyCode::Char('n') {
                    self.overlay = None;
                }
            }
            Overlay::Help | Overlay::Detail(_, _) => {
                if key.code == KeyCode::Enter {
                    self.overlay = None;
                }
            }
        }
    }
}

fn edit(editor: &mut Editor, key: KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char('a') if ctrl => editor.home(),
        KeyCode::Char('e') if ctrl => editor.end(),
        KeyCode::Char('u') if ctrl => {
            editor.text.drain(..editor.cursor);
            editor.cursor = 0;
        }
        KeyCode::Char('k') if ctrl => {
            editor.text.truncate(editor.cursor);
        }
        KeyCode::Char('w') if ctrl => editor.word_back(),
        KeyCode::Char(c) if !ctrl => editor.insert(&c.to_string()),
        KeyCode::Backspace => editor.backspace(),
        KeyCode::Delete => editor.delete(),
        KeyCode::Left => editor.left(),
        KeyCode::Right => editor.right(),
        KeyCode::Home => editor.home(),
        KeyCode::End => editor.end(),
        KeyCode::Up => editor.line_up(),
        KeyCode::Down => editor.line_down(),
        _ => {}
    }
}
