//! The model, behind a seam.
//!
//! This is the seam that makes the loop testable, and it is worth stating plainly: **everything above
//! this line is exercised with no model call.** A loop whose first honest run needs a network round trip
//! has no offline proof that it works, and the offline proof is the only one available before the model
//! is wired in.
//!
//! [`Scripted`] is what a test drives and what the CLI's `turn --program` uses, where the person at the
//! keyboard takes the model's place. It is not a mock of a model: it is a harness that answers with a
//! program it was given, which is exactly what a harness does.
//!
//! [`OpenAi`] is the real one. It speaks the OpenAI chat-completions shape, because that is what a local
//! llama.cpp server serves and what the cloud account serves, so one implementation reaches both and the
//! difference between them is a URL and where a key lives. The seam stays one method: a turn asks once.
//!
//! What the model is asked for is a program, not a tool call, not JSON. The system prompt says so in as
//! few words as possible, and the parser accepts the two shapes a model actually produces: a fenced block
//! when it fences, and the whole answer when it does not. A model that answers with an apology instead of
//! a program produces a program that fails, and the log carries what it said, which is the honest outcome.

use serde_json::Value;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::context::Bundle;
use crate::fault::Fault;
use crate::http;
use crate::task::{Budget, ModelAnswer, TaskSpec, Usage};

/// The model. One method, because a turn asks once.
/// Which half of the model's answer a piece belongs to. A person watching a run wants the thinking while
/// it happens and the program when it is written, and the two arrive interleaved on one stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feed {
    Reasoning,
    Content,
}

/// The sink a caller hands a run: where each piece goes while the model writes it.
pub type FeedFn<'a> = &'a dyn Fn(Feed, &str);

impl Feed {
    pub fn name(self) -> &'static str {
        match self {
            Feed::Reasoning => "reasoning",
            Feed::Content => "content",
        }
    }
}

pub trait Harness {
    fn run(
        &self,
        task: &TaskSpec,
        bundle: &Bundle,
        budget: &Budget,
        feed: Option<FeedFn<'_>>,
    ) -> Result<ModelAnswer, Fault>;

    /// A plan, before any program. Separate from `run` because it is prose, not an answer, and because
    /// a plan-first turn has to record it before the call that can change the tree. A harness that cannot
    /// plan returns an empty string, which the turn treats as nothing written.
    fn plan(&self, task: &TaskSpec, bundle: &Bundle, budget: &Budget) -> Result<String, Fault>;

    /// A plain completion with the caller's own system prompt. Research uses this because its planner and
    /// extractor are not code turns: no program, no check, just one question and one answer. The default
    /// refuses, so a new harness cannot silently pretend it can answer one.
    fn complete(&self, _system: &str, _user: &str, _budget: &Budget) -> Result<Completion, Fault> {
        Err(Fault::model(
            "this harness cannot answer a plain completion".to_string(),
        ))
    }
}

/// One completion and what it cost, when the endpoint reports cost. Separate from `ModelAnswer` because a
/// research planner is prose, not a program, and pretending it is a draft would put it through the edit
/// parser.
#[derive(Debug, Clone)]
pub struct Completion {
    pub text: String,
    pub usage: Option<Usage>,
}

/// Answers from a queue, in order.
#[derive(Debug, Default)]
pub struct Scripted {
    answers: Mutex<VecDeque<ModelAnswer>>,
    /// Plans, in order. Empty means every plan is empty, which is how a test proves the refusal.
    plans: Mutex<VecDeque<String>>,
}

impl Scripted {
    pub fn new<I, S>(programs: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Scripted {
            answers: Mutex::new(programs.into_iter().map(ModelAnswer::new).collect()),
            plans: Mutex::new(VecDeque::new()),
        }
    }

    /// One answer, for a caller testing one turn.
    pub fn one(program: impl Into<String>) -> Self {
        Scripted::new([program])
    }

    /// Answers that are already built, so a test can hand the turn an edit or a refusal without going
    /// through a model endpoint. The parser lives in the endpoint client and is tested there.
    pub fn answers<I>(answers: I) -> Self
    where
        I: IntoIterator<Item = ModelAnswer>,
    {
        Scripted {
            answers: Mutex::new(answers.into_iter().collect()),
            plans: Mutex::new(VecDeque::new()),
        }
    }
}

impl Scripted {
    /// Plans for a plan-first turn, in order.
    pub fn planning<I, S>(mut self, plans: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.plans = Mutex::new(plans.into_iter().map(Into::into).collect());
        self
    }
}

impl Harness for Scripted {
    fn plan(&self, _task: &TaskSpec, _bundle: &Bundle, _budget: &Budget) -> Result<String, Fault> {
        Ok(self
            .plans
            .lock()
            .expect("the plan lock")
            .pop_front()
            .unwrap_or_default())
    }

    fn run(
        &self,
        _task: &TaskSpec,
        _bundle: &Bundle,
        _budget: &Budget,
        _feed: Option<FeedFn<'_>>,
    ) -> Result<ModelAnswer, Fault> {
        self.answers
            .lock()
            .expect("the script lock")
            .pop_front()
            .ok_or_else(|| Fault::model("the script had no answer left, so no program was written"))
    }

    fn complete(&self, _system: &str, _user: &str, _budget: &Budget) -> Result<Completion, Fault> {
        let answer = self
            .answers
            .lock()
            .expect("the script lock")
            .pop_front()
            .ok_or_else(|| {
                Fault::model("the script had no answer left, so no completion was made")
            })?;
        Ok(Completion {
            text: answer.program().to_string(),
            usage: answer.usage(),
        })
    }
}

/// What the model is told about its job, in the fewest words that have worked.
///
/// It asks for a program and says where the program runs, because a model that does not know its working
/// directory writes an absolute path into a tree it has never seen. It names the check, because a turn is
/// not done until the check passes and a model that does not know how it will be judged optimises for
/// looking finished.
pub const SYSTEM: &str = "You are the model inside a coding harness. You answer with one shell program \
and nothing else: no prose, no explanation, no markdown fences. The program runs with sh -c in the \
working directory you are given, and it is not saved anywhere: if the goal asks you to write a file, \
your program has to write it. Print whatever you need to see; your output is read back to you. You may be \
given more than one attempt: what your program printed is shown to you before the next one. Use them: do \
not spend attempt after attempt looking at the problem, because your own output already carries what you \
printed, so gather what you need and then change it. One exception to the program: when the step is a \
change to text already in a file, you may answer with one JSON object and nothing else: \
{\"edits\":[{\"path\":\"relative/path\",\"old\":\"the exact text now in the file\",\"new\":\"what replaces \
it\"}]}. Each old must appear exactly once and edits apply in order; an edit that does not is refused and \
you are shown why. An edit costs the edit, not the file. Use a program for a new file or for anything an \
edit cannot say. When the goal is met, say so by exiting 0.";

/// What a plan-first turn asks for. Not the program prompt: a plan names steps and the check, and a model
/// asked for a program writes one even when told not to.
pub const PLAN: &str = "You are the model inside a coding harness, writing a short plan before you act. \
Answer with a numbered plan of at most eight lines and nothing else: no code block, no preamble, no \
confidence. Each line names the file or command it touches, and the last line says how the check will \
show the work is done. You will be asked for one shell program after the plan is recorded.";

/// A model behind an OpenAI-compatible chat-completions endpoint: one POST, one program.
#[derive(Debug, Clone)]
pub struct OpenAi {
    target: String,
    model: String,
    key: Option<Key>,
    temperature: f32,
    max_tokens: u32,
    timeout: Duration,
    context: usize,
    /// Optional endpoint-reported accounting, shared with delegated client clones.
    usage_log: Option<std::sync::Arc<Mutex<Vec<Usage>>>>,
    /// Whether to ask the endpoint's template for thinking. `None` sends nothing, which is what a cloud
    /// endpoint wants; a local thinking model whose template understands `enable_thinking` is asked for
    /// none, because a model that thinks before it writes a one-line program spends the whole answer
    /// thinking and leaves the program empty.
    thinking: Option<bool>,
    /// The transport this endpoint speaks. Only the tool-calling loop requires chat completions; a plain
    /// call works in any of the three.
    mode: crate::provider::ApiMode,
    /// Headers the provider itself needs, from its config entry.
    extra_headers: Vec<(String, String)>,
    /// The providers to try, in order, when this one fails before it has produced an answer. A chain is
    /// built from `fallback_providers` and is empty unless the owner named one.
    fallbacks: Vec<OpenAi>,
    /// Whether a fault from this client names the model it asked. A strict request is one the caller
    /// named, so a failure has to say which model failed rather than leave the reader guessing whether
    /// something else was tried.
    strict: bool,
    /// The system prompt this client sends. The built-in constant unless a harness file replaced it; the
    /// harness is a property of the client, which is what it is, rather than a second thing every call has
    /// to carry.
    system: String,
}

/// Where a key comes from. The value is resolved at call time and never held in the request, the log, or
/// a fault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    /// An environment variable, which is where a key belongs when a shell starts the process.
    Env(String),
    /// A line in Hades' own dotenv file, `~/.hades/.env`. Hades does not read the live install's keys: a
    /// second system that borrows the first one's credentials is not a second system, and on the machine
    /// this gets tested on there is no Hermes to borrow from.
    Home(String),
}

/// A secret by name: the environment first, then Hades' own `.env`. The value is returned to the
/// caller that needs it and never printed; the error names the source, not the value.
pub fn secret(var: &str) -> Result<String, String> {
    Key::Home(var.to_string())
        .resolve()
        .map_err(|fault| fault.detail().to_string())
}

impl Key {
    /// The value, or a fault that names the *source* and never the value.
    fn resolve(&self) -> Result<String, Fault> {
        match self {
            Key::Env(var) => std::env::var(var).map_err(|_| {
                Fault::model(format!(
                    "the environment has no {var} in it, and this endpoint needs a key"
                ))
            }),
            Key::Home(var) => {
                if let Ok(value) = std::env::var(var) {
                    return Ok(value);
                }
                let path = hades_env_path();
                let text = std::fs::read_to_string(&path).map_err(|e| {
                    Fault::model(format!(
                        "no ${var} in the environment and {} could not be read: {e}",
                        path.display()
                    ))
                })?;
                key_from_text(&text, var).ok_or_else(|| {
                    Fault::model(format!(
                        "{} has no {var} line in it, so there is no key to send",
                        path.display()
                    ))
                })
            }
        }
    }
}

/// One `VAR=value` line out of a dotenv file, with the quotes a dotenv file usually has taken off.
pub fn key_from_text(text: &str, var: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix(var)
            && let Some(value) = rest.strip_prefix('=')
        {
            let value = value.trim().trim_matches('"').trim_matches('\'').trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

fn hades_env_path() -> std::path::PathBuf {
    match std::env::var("HADES_ENV_FILE") {
        Ok(path) if !path.is_empty() => std::path::PathBuf::from(path),
        _ => match std::env::var("HOME") {
            Ok(home) => std::path::PathBuf::from(home).join(".hades/.env"),
            Err(_) => std::path::PathBuf::from(".hades/.env"),
        },
    }
}

impl OpenAi {
    /// A model at an endpoint. The URL is the whole thing to POST to, path included, because guessing a
    /// path from a base URL is how a client ends up talking to the wrong route.
    pub fn new(target: impl Into<String>, model: impl Into<String>) -> Self {
        OpenAi {
            target: target.into(),
            model: model.into(),
            key: None,
            temperature: 0.2,
            // Room to think and then answer. A thinking model given 1024 tokens spends all of them
            // reasoning and returns nothing at all, which is worse than a wrong program: there is nothing
            // to read. Unused headroom costs nothing, because the model stops when it is done. Beam
            // spends a 4096-token cap on a hard turn and never starts the answer.
            max_tokens: 16_384,
            timeout: Duration::from_secs(180),
            // Conservative by default, because the alternative is asking a model for more than its window
            // and having the server truncate silently, which is the worst outcome of the three: a context
            // that looks built and is not.
            context: 8192,
            usage_log: None,
            thinking: None,
            mode: crate::provider::ApiMode::ChatCompletions,
            extra_headers: Vec::new(),
            fallbacks: Vec::new(),
            strict: false,
            system: SYSTEM.to_string(),
        }
    }

    /// The text around the model, replaced by a harness file. Trimmed, because a harness file that starts
    /// with three blank lines says nothing and costs tokens.
    pub fn with_system(mut self, system: impl Into<String>) -> Self {
        let text = system.into();
        if !text.trim().is_empty() {
            self.system = text.trim().to_string();
        }
        self
    }

    /// The window this model will be asked to work inside. A turn's budget is capped by it, so the bundle
    /// is built to fit rather than truncated on the way in.
    pub fn context(&self) -> usize {
        self.context
    }

    pub fn with_context(mut self, context: usize) -> Self {
        self.context = context.max(1024);
        self
    }

    /// Ask the endpoint's chat template for thinking, or for none. Only an endpoint whose template knows
    /// `enable_thinking` should be sent this; a cloud endpoint that does not will refuse the request.
    pub fn with_thinking(mut self, thinking: bool) -> Self {
        self.thinking = Some(thinking);
        self
    }

    /// Whether thinking is asked for, or nothing is said about it.
    pub fn thinking(&self) -> Option<bool> {
        self.thinking
    }

    /// Hold this client to the model it names: faults from it say which model failed, so a reader can
    /// tell a strict request from one that fell through to something else.
    /// The transport to speak, and the headers the provider needs. Both come from a provider entry.
    pub fn with_mode(mut self, mode: crate::provider::ApiMode) -> Self {
        self.mode = mode;
        self
    }

    pub fn with_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.extra_headers = headers;
        self
    }

    /// The clients to try when this one fails before answering. Order is the order they were named in.
    pub fn with_fallbacks(mut self, fallbacks: Vec<OpenAi>) -> Self {
        self.fallbacks = fallbacks;
        self
    }

    pub fn mode(&self) -> crate::provider::ApiMode {
        self.mode
    }

    pub fn with_strict(mut self) -> Self {
        self.strict = true;
        self
    }

    /// The check that costs nothing: is there an endpoint to call, and is the key this client would send
    /// with it. A request that cannot work fails here, before a session is opened and before a call is
    /// spent, and the fault names the model and the provider so the reader knows which one to fix.
    ///
    /// It dials nothing. Whether the endpoint is *reachable* is only knowable by asking it, and asking is
    /// the call this exists to avoid wasting on a configuration that was already wrong.
    pub fn preflight(&self) -> Result<(), Fault> {
        if self.target.trim().is_empty() {
            return Err(Fault::model(format!(
                "the model `{}` has no endpoint configured, so there is nothing to call; pass \
                 --endpoint or name a preset from `hades models`",
                self.model
            )));
        }
        if let Err(fault) = http::Target::parse(&self.target) {
            return Err(Fault::model(format!(
                "the model `{}` has an endpoint that cannot be called: {}",
                self.model,
                fault.detail()
            )));
        }
        if let Some(key) = &self.key {
            key.resolve().map_err(|fault| {
                Fault::model(format!(
                    "the model `{}` at {} (provider {}) needs a key, and it is not there: {}",
                    self.model,
                    self.target,
                    crate::config::provider_for(&self.target),
                    fault.detail()
                ))
            })?;
        }
        Ok(())
    }

    /// A fault with the model's name on it when this client is strict. A failed request that says only
    /// "connection refused" sends the reader looking at the network when the question was which model.
    fn named<T>(&self, result: Result<T, Fault>) -> Result<T, Fault> {
        result.map_err(|fault| {
            if self.strict && fault.class() == crate::fault::Class::Model {
                Fault::model(format!(
                    "model `{}` at {}: {}",
                    self.model,
                    self.target,
                    fault.detail()
                ))
            } else {
                fault
            }
        })
    }

    pub fn with_key(mut self, key: Key) -> Self {
        self.key = Some(key);
        self
    }

    pub fn with_temperature(mut self, temperature: f32) -> Self {
        self.temperature = temperature;
        self
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn with_usage_log(mut self, log: std::sync::Arc<Mutex<Vec<Usage>>>) -> Self {
        self.usage_log = Some(log);
        self
    }

    pub(crate) fn authorization(&self) -> Result<Option<String>, String> {
        self.key
            .as_ref()
            .map(Key::resolve)
            .transpose()
            .map_err(|e| e.to_string())
    }

    /// What a person may see: where it goes and which model, never a key.
    pub fn display(&self) -> String {
        format!("{} at {}", self.model, self.target)
    }

    /// The request body. Separate from sending it, so a test can read exactly what would be sent.
    /// The turn's user message. One place, so a transport that puts the system prompt somewhere else
    /// still asks the same question.
    fn user_text(&self, task: &TaskSpec, bundle: &Bundle, budget: &Budget) -> String {
        format!(
            "Goal: {}\nWorking directory: {}\n\n{}\n\nAnswer with the JSON edit object when this step changes text already in a file, or with a shell \
             program for anything else (a new file, a build, an inspection). \
             Context budget for this turn: {} tokens.",
            task.goal(),
            task.root().display(),
            bundle.text(),
            budget.context_tokens()
        )
    }

    pub fn body(
        &self,
        task: &TaskSpec,
        bundle: &Bundle,
        budget: &Budget,
        streaming: bool,
    ) -> String {
        let user = self.user_text(task, bundle, budget);
        let mut request = serde_json::json!({
            "model": self.model,
            "messages": [
                { "role": "system", "content": self.system },
                { "role": "user", "content": user },
            ],
            "temperature": self.temperature,
            "max_tokens": self.max_tokens,
            "stream": streaming,
        });
        if let Some(thinking) = self.thinking {
            request["chat_template_kwargs"] = serde_json::json!({ "enable_thinking": thinking });
        }
        request.to_string()
    }
}

impl OpenAi {
    /// The clients to try, this one first. A chain of one is the normal case.
    fn chain(&self) -> Vec<&OpenAi> {
        std::iter::once(self).chain(self.fallbacks.iter()).collect()
    }

    /// The headers a call carries: content type, the credential in whatever shape this transport wants,
    /// and the provider's own headers last so a gateway header is never overwritten by a default.
    fn wire(&self) -> Result<Vec<(&str, String)>, Fault> {
        let mut headers: Vec<(&str, String)> =
            vec![("Content-Type", "application/json".to_string())];
        match self.mode {
            crate::provider::ApiMode::AnthropicMessages => {
                headers.push(("anthropic-version", "2023-06-01".to_string()));
                if let Some(key) = &self.key {
                    headers.push(("x-api-key", key.resolve()?));
                }
            }
            _ => {
                if let Some(key) = &self.key {
                    headers.push(("Authorization", format!("Bearer {}", key.resolve()?)));
                }
            }
        }
        for (name, value) in &self.extra_headers {
            headers.push((name.as_str(), value.clone()));
        }
        Ok(headers)
    }

    /// The body for a call with no tools. A system message is the top-level `system` in Anthropic's
    /// shape and `instructions` in the responses API; chat completions carries it as a message.
    fn plain_body(
        &self,
        system: Option<&str>,
        messages: &[serde_json::Value],
        cap: u32,
    ) -> serde_json::Value {
        let content = |message: &serde_json::Value| -> String {
            message
                .get("content")
                .and_then(|value| value.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| message.get("content").cloned().unwrap_or_default().to_string())
        };
        match self.mode {
            crate::provider::ApiMode::ChatCompletions => {
                let mut all: Vec<serde_json::Value> = Vec::new();
                if let Some(system) = system {
                    all.push(serde_json::json!({ "role": "system", "content": system }));
                }
                all.extend(messages.iter().cloned());
                serde_json::json!({
                    "model": self.model,
                    "messages": all,
                    "temperature": self.temperature,
                    "max_tokens": cap,
                })
            }
            crate::provider::ApiMode::AnthropicMessages => {
                let turns: Vec<serde_json::Value> = messages
                    .iter()
                    .filter(|message| {
                        !matches!(
                            message.get("role").and_then(|role| role.as_str()),
                            Some("system")
                        )
                    })
                    .map(|message| {
                        serde_json::json!({
                            "role": if message.get("role").and_then(|r| r.as_str()) == Some("assistant") {
                                "assistant"
                            } else {
                                "user"
                            },
                            "content": content(message),
                        })
                    })
                    .collect();
                let mut body = serde_json::json!({
                    "model": self.model,
                    "messages": turns,
                    "max_tokens": cap,
                    "temperature": self.temperature,
                });
                if let Some(system) = system {
                    body["system"] = serde_json::json!(system);
                }
                body
            }
            crate::provider::ApiMode::Responses => {
                let input: Vec<serde_json::Value> = messages
                    .iter()
                    .filter(|message| {
                        !matches!(
                            message.get("role").and_then(|role| role.as_str()),
                            Some("system")
                        )
                    })
                    .map(|message| {
                        serde_json::json!({
                            "role": message.get("role").and_then(|r| r.as_str()).unwrap_or("user"),
                            "content": content(message),
                        })
                    })
                    .collect();
                let mut body = serde_json::json!({
                    "model": self.model,
                    "input": input,
                    "max_output_tokens": cap,
                    "temperature": self.temperature,
                });
                if let Some(system) = system {
                    body["instructions"] = serde_json::json!(system);
                }
                body
            }
        }
    }

    /// The answer text, in the shape the transport puts it in.
    fn plain_text(&self, parsed: &serde_json::Value) -> Option<String> {
        match self.mode {
            crate::provider::ApiMode::ChatCompletions => parsed
                .pointer("/choices/0/message/content")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .or_else(|| {
                    parsed
                        .pointer("/choices/0/text")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                }),
            crate::provider::ApiMode::AnthropicMessages => {
                let blocks = parsed.get("content")?.as_array()?;
                let text = blocks
                    .iter()
                    .filter(|block| block.get("type").and_then(|t| t.as_str()) == Some("text"))
                    .filter_map(|block| block.get("text").and_then(|t| t.as_str()))
                    .collect::<String>();
                Some(text)
            }
            crate::provider::ApiMode::Responses => parsed
                .get("output_text")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .or_else(|| {
                    let output = parsed.get("output")?.as_array()?;
                    Some(
                        output
                            .iter()
                            .filter_map(|item| item.get("content").and_then(|c| c.as_array()))
                            .flatten()
                            .filter(|part| {
                                part.get("type").and_then(|t| t.as_str()) == Some("output_text")
                            })
                            .filter_map(|part| part.get("text").and_then(|t| t.as_str()))
                            .collect::<String>(),
                    )
                }),
        }
    }

    /// The reasoning a transport reports, if it reports any.
    fn plain_reasoning(&self, parsed: &serde_json::Value) -> String {
        match self.mode {
            crate::provider::ApiMode::AnthropicMessages => parsed
                .get("content")
                .and_then(|c| c.as_array())
                .map(|blocks| {
                    blocks
                        .iter()
                        .filter(|block| {
                            block.get("type").and_then(|t| t.as_str()) == Some("thinking")
                        })
                        .filter_map(|block| block.get("thinking").and_then(|t| t.as_str()))
                        .collect::<String>()
                })
                .unwrap_or_default(),
            _ => ["reasoning_content", "reasoning", "thinking"]
                .iter()
                .find_map(|field| {
                    parsed
                        .pointer(&format!("/choices/0/message/{field}"))
                        .and_then(|v| v.as_str())
                })
                .unwrap_or("")
                .to_string(),
        }
    }

    /// Tokens and cache, named differently by each transport.
    fn plain_usage(&self, parsed: &serde_json::Value, millis: u128) -> Usage {
        let usage = parsed.get("usage").cloned().unwrap_or_default();
        let (prompt, generated) = match self.mode {
            crate::provider::ApiMode::ChatCompletions => (
                usage.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                usage
                    .get("completion_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
            ),
            _ => (
                usage.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                usage.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
            ),
        };
        Usage {
            prompt_tokens: prompt,
            cached_tokens: cached_of(&usage),
            generated_tokens: generated,
            millis,
            reasoning_chars: 0,
        }
    }

    /// One call with no tools and no stream, in whichever transport this client speaks.
    fn plain_call(
        &self,
        system: Option<&str>,
        messages: &[serde_json::Value],
        cap: u32,
    ) -> Result<(serde_json::Value, Usage), Fault> {
        let body = self.plain_body(system, messages, cap).to_string();
        let headers = self.wire()?;
        let started = Instant::now();
        let reply = http::post(&self.target, &body, &headers, self.timeout)?;
        let elapsed = started.elapsed();
        if !reply.ok() {
            return Err(Fault::model(format!(
                "the model endpoint answered {}: {}",
                reply.status,
                first_line(&reply.body, 300)
            )));
        }
        let parsed: serde_json::Value = serde_json::from_str(&reply.body).map_err(|error| {
            Fault::model(format!(
                "the model endpoint answered {} but not with JSON: {error} (it said: {})",
                reply.status,
                first_line(&reply.body, 200)
            ))
        })?;
        if let Some(message) = parsed.pointer("/error/message").and_then(|v| v.as_str()) {
            return Err(Fault::model(format!(
                "the model endpoint returned an error: {}",
                first_line(message, 300)
            )));
        }
        let usage = self.plain_usage(&parsed, elapsed.as_millis());
        Ok((parsed, usage))
    }
}

impl Harness for OpenAi {
    /// The plan is a chat call, not a program call: one system message and one user message, no tools, and
    /// the answer is prose. The client's own system prompt is not used, because a plan is not an answer and
    /// the harness text must not make the model write a program here.
    fn plan(&self, task: &TaskSpec, bundle: &Bundle, _budget: &Budget) -> Result<String, Fault> {
        self.named(self.write_plan(task, bundle))
    }

    fn complete(&self, system: &str, user: &str, _budget: &Budget) -> Result<Completion, Fault> {
        if self.mode != crate::provider::ApiMode::ChatCompletions {
            let messages = [serde_json::json!({ "role": "user", "content": user })];
            let mut errors: Vec<String> = Vec::new();
            for client in self.chain() {
                match client.plain_call(Some(system), &messages, client.max_tokens) {
                    Ok((parsed, usage)) => {
                        return Ok(Completion {
                            text: client
                                .plain_text(&parsed)
                                .unwrap_or_default()
                                .trim()
                                .to_string(),
                            usage: Some(usage),
                        });
                    }
                    Err(error) => errors.push(format!("{}: {error}", client.model)),
                }
            }
            return Err(Fault::model(match errors.len() {
                1 => errors.pop().unwrap_or_default(),
                _ => format!("every provider failed: {}", errors.join("; ")),
            }));
        }
        let messages = serde_json::json!([
            { "role": "system", "content": system },
            { "role": "user", "content": user },
        ]);
        // A plain completion still has a cost. A local clone with a usage log makes the request ask
        // for `stream_options.include_usage`; without the log the endpoint is allowed to omit `usage`,
        // and the research ledger would read zero for a call that was not free.
        let mut errors: Vec<String> = Vec::new();
        for candidate in self.chain() {
            let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let client = candidate.clone().with_usage_log(log.clone());
            match client.chat(
                messages.as_array().expect("an array of messages"),
                None,
                &|_, _| {},
            ) {
                Ok(reply) => {
                    let usage = reply
                        .usage
                        .or_else(|| log.lock().ok().and_then(|entries| entries.last().copied()));
                    return Ok(Completion {
                        text: reply.content.trim().to_string(),
                        usage,
                    });
                }
                Err(error) => errors.push(format!("{}: {error}", client.model)),
            }
        }
        Err(Fault::model(match errors.len() {
            1 => errors.pop().unwrap_or_default(),
            _ => format!("every provider failed: {}", errors.join("; ")),
        }))
    }

    /// Stream when the endpoint streams, and fall back to one POST when it does not.
    ///
    /// The stream is the reason a person can watch a run: the reasoning and the program arrive as they are
    /// written, and a caller that wants to show them edits one message instead of waiting for the end. A
    /// server that ignores `stream` answers one JSON object, which produces no deltas, so the same turn is
    /// asked once more without streaming. That is one wasted request on a non-streaming endpoint and the
    /// only way to keep both shapes working with one client.
    fn run(
        &self,
        task: &TaskSpec,
        bundle: &Bundle,
        budget: &Budget,
        feed: Option<FeedFn<'_>>,
    ) -> Result<ModelAnswer, Fault> {
        self.named(self.answer(task, bundle, budget, feed))
    }
}

impl OpenAi {
    /// The completion cap this client sends unless a caller says otherwise.
    pub fn call_cap(&self) -> u64 {
        self.max_tokens as u64
    }

    /// The same client with a smaller completion cap, for one call.
    pub fn with_cap(&self, cap: u64) -> OpenAi {
        let mut client = self.clone();
        client.max_tokens = cap.min(u32::MAX as u64) as u32;
        client
    }

    fn write_plan(&self, task: &TaskSpec, bundle: &Bundle) -> Result<String, Fault> {
        let user = format!(
            "Goal: {}\nWorking directory: {}\n\nWhat the harness already knows:\n{}\n\nWrite the plan.",
            task.goal(),
            task.root().display(),
            bundle.text()
        );
        let messages = serde_json::json!([
            { "role": "system", "content": PLAN },
            { "role": "user", "content": user },
        ]);
        let reply = self.chat(
            messages.as_array().expect("an array of messages"),
            None,
            &|_, _| {},
        )?;
        Ok(reply.content.trim().chars().take(4_000).collect())
    }

    fn answer(
        &self,
        task: &TaskSpec,
        bundle: &Bundle,
        budget: &Budget,
        feed: Option<FeedFn<'_>>,
    ) -> Result<ModelAnswer, Fault> {
        // Nobody is watching: one POST, no stream, no second request when an endpoint ignores `stream`.
        let Some(feed) = feed else {
            return self.once(task, bundle, budget);
        };
        let mut content = String::new();
        let mut reasoning = String::new();
        let mut usage: Option<(u64, u64, u64)> = None;
        let mut saw = false;
        let started = Instant::now();
        let body = self.body(task, bundle, budget, true);
        let mut headers: Vec<(&str, String)> =
            vec![("Content-Type", "application/json".to_string())];
        if let Some(key) = &self.key {
            headers.push(("Authorization", format!("Bearer {}", key.resolve()?)));
        }
        let streamed = http::stream(&self.target, &body, &headers, self.timeout, &mut |line| {
            let Some(data) = line.trim().strip_prefix("data:") else {
                return;
            };
            let data = data.trim();
            if data.is_empty() || data == "[DONE]" {
                return;
            }
            let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
                return;
            };
            saw = true;
            if let Some(delta) = value.pointer("/choices/0/delta") {
                if let Some(piece) = delta.get("content").and_then(|v| v.as_str())
                    && !piece.is_empty()
                {
                    content.push_str(piece);
                    feed(Feed::Content, piece);
                }
                for field in ["reasoning_content", "reasoning", "thinking"] {
                    if let Some(piece) = delta.get(field).and_then(|v| v.as_str())
                        && !piece.is_empty()
                    {
                        reasoning.push_str(piece);
                        feed(Feed::Reasoning, piece);
                    }
                }
            }
            if let Some(found) = value.get("usage") {
                let prompt = found
                    .get("prompt_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let generated = found
                    .get("completion_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                usage = Some((prompt, generated, cached_of(found)));
            }
        });
        if let Err(error) = streamed {
            // Nothing was written, so the next provider in the chain gets the turn; `once` walks it and
            // reports every failure if none of them answers. Content already on screen is not replayed.
            if content.trim().is_empty() && !saw {
                return self.once(task, bundle, budget);
            }
            return Err(error);
        }
        let elapsed = started.elapsed();

        if !saw || content.trim().is_empty() {
            // No deltas at all: this endpoint does not stream, and one POST is the answer.
            return self.once(task, bundle, budget);
        }
        let program = extract(&content);
        let (prompt_tokens, generated_tokens, cached_tokens) = usage.unwrap_or((0, 0, 0));
        if let Some(edit_answer) = crate::patch::edits_from(&program) {
            let answer = match edit_answer {
                Ok(edits) => ModelAnswer::editing(edits),
                Err(reason) => ModelAnswer::refused(reason),
            };
            return Ok(answer
                .with_usage(Usage {
                    prompt_tokens,
                    cached_tokens,
                    generated_tokens,
                    millis: elapsed.as_millis(),
                    reasoning_chars: reasoning.chars().count() as u64,
                })
                .with_reasoning(reasoning.clone()));
        }
        if program.trim().is_empty() {
            return self.once(task, bundle, budget);
        }
        let answer = ModelAnswer::new(program)
            .with_usage(Usage {
                prompt_tokens,
                cached_tokens,
                generated_tokens,
                millis: elapsed.as_millis(),
                reasoning_chars: reasoning.chars().count() as u64,
            })
            .with_reasoning(reasoning.clone());
        Ok(answer)
    }
}

/// Cached prompt tokens, in either shape a provider reports them.
fn cached_of(usage: &serde_json::Value) -> u64 {
    usage
        .pointer("/prompt_tokens_details/cached_tokens")
        .and_then(serde_json::Value::as_u64)
        .or_else(|| usage.get("prompt_cache_hit_tokens").and_then(serde_json::Value::as_u64))
        .unwrap_or(0)
}

impl OpenAi {
    /// A conversation, not a turn: the endpoint's chat template with whatever messages the caller built, no
    /// program extraction and no check. This is what a chat surface needs, and it is the same client, the
    /// same key resolution and the same stream as the harness uses, so there is one place a model is called.
    pub fn converse(
        &self,
        messages: &[serde_json::Value],
        feed: &dyn Fn(Feed, &str),
    ) -> Result<String, Fault> {
        if self.mode != crate::provider::ApiMode::ChatCompletions {
            // A transport without a stream: one call, then the whole answer is delivered at once so the
            // surface behaves the same way to the person watching it.
            let text_of = |message: &serde_json::Value| {
                message
                    .get("content")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_string()
            };
            let system = messages
                .iter()
                .filter(|message| message.get("role").and_then(|r| r.as_str()) == Some("system"))
                .map(&text_of)
                .collect::<Vec<_>>()
                .join("\n\n");
            let turns: Vec<serde_json::Value> = messages
                .iter()
                .filter(|message| message.get("role").and_then(|r| r.as_str()) != Some("system"))
                .cloned()
                .collect();
            let mut errors: Vec<String> = Vec::new();
            for client in self.chain() {
                match client.plain_call(
                    (!system.is_empty()).then_some(system.as_str()),
                    &turns,
                    client.max_tokens,
                ) {
                    Ok((parsed, _)) => {
                        let text = client.plain_text(&parsed).unwrap_or_default();
                        if text.trim().is_empty() {
                            errors.push(format!("{}: it answered nothing", client.model));
                            continue;
                        }
                        feed(Feed::Content, &text);
                        return Ok(text);
                    }
                    Err(error) => errors.push(format!("{}: {error}", client.model)),
                }
            }
            return Err(Fault::model(match errors.len() {
                1 => errors.pop().unwrap_or_default(),
                _ => format!("every provider failed: {}", errors.join("; ")),
            }));
        }
        // One attempt per provider in the chain. A stream that fails before producing anything is the
        // next provider's turn; one that fails after producing content is an error, not a stutter.
        let mut errors: Vec<String> = Vec::new();
        for candidate in self.chain() {
            let body = serde_json::json!({
                "model": candidate.model,
                "messages": messages,
                "temperature": candidate.temperature,
                "max_tokens": candidate.max_tokens,
                "stream": true,
            });
            let headers = candidate.wire()?;
            let mut content = String::new();
            let streamed = http::stream(
                &candidate.target,
                &body.to_string(),
                &headers,
                candidate.timeout,
                &mut |line| {
                    let Some(data) = line.trim().strip_prefix("data:") else {
                        return;
                    };
                    let data = data.trim();
                    if data.is_empty() || data == "[DONE]" {
                        return;
                    }
                    let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
                        return;
                    };
                    if let Some(delta) = value.pointer("/choices/0/delta") {
                        if let Some(piece) = delta.get("content").and_then(|v| v.as_str())
                            && !piece.is_empty()
                        {
                            content.push_str(piece);
                            feed(Feed::Content, piece);
                        }
                        for field in ["reasoning_content", "reasoning", "thinking"] {
                            if let Some(piece) = delta.get(field).and_then(|v| v.as_str())
                                && !piece.is_empty()
                            {
                                feed(Feed::Reasoning, piece);
                            }
                        }
                    }
                },
            );
            match streamed {
                Ok(()) if !content.trim().is_empty() => return Ok(content),
                Ok(()) => {
                    // A server that ignores `stream` answers one object and produces no delta; one plain
                    // call is the answer.
                    match candidate.plain_call(None, messages, candidate.max_tokens) {
                        Ok((parsed, _)) => {
                            if let Some(text) = candidate.plain_text(&parsed)
                                && !text.trim().is_empty()
                            {
                                feed(Feed::Content, &text);
                                return Ok(text);
                            }
                            errors.push(format!("{}: it answered nothing", candidate.model));
                        }
                        Err(error) => errors.push(format!("{}: {error}", candidate.model)),
                    }
                }
                Err(error) => {
                    if !content.trim().is_empty() {
                        return Err(error);
                    }
                    errors.push(format!("{}: {error}", candidate.model));
                }
            }
        }
        Err(Fault::model(match errors.len() {
            1 => errors.pop().unwrap_or_default(),
            _ => format!("every provider failed: {}", errors.join("; ")),
        }))
    }

    fn once(
        &self,
        task: &TaskSpec,
        bundle: &Bundle,
        budget: &Budget,
    ) -> Result<ModelAnswer, Fault> {
        let mut errors: Vec<String> = Vec::new();
        for client in self.chain() {
            match client.once_one(task, bundle, budget) {
                Ok(answer) => return Ok(answer),
                Err(error) => errors.push(format!("{}: {error}", client.model)),
            }
        }
        Err(Fault::model(match errors.len() {
            1 => errors.pop().unwrap_or_default(),
            _ => format!("every provider failed: {}", errors.join("; ")),
        }))
    }

    fn once_one(
        &self,
        task: &TaskSpec,
        bundle: &Bundle,
        budget: &Budget,
    ) -> Result<ModelAnswer, Fault> {
        // A transport that is not chat completions has no tool protocol here, so a turn is one plain
        // call: system prompt where that transport wants it, one user message, one program back.
        if self.mode != crate::provider::ApiMode::ChatCompletions {
            let user = self.user_text(task, bundle, budget);
            let messages = [serde_json::json!({ "role": "user", "content": user })];
            let (parsed, usage) = self.plain_call(Some(&self.system), &messages, self.max_tokens)?;
            let content = self.plain_text(&parsed).unwrap_or_default();
            let mut program = extract(&content);
            if program.trim().is_empty()
                && let Some((_, command)) = tool_call_program(&parsed)
            {
                program = command;
            }
            if program.trim().is_empty() {
                return Err(Fault::model(format!(
                    "the model answered with no program at all over {}: {} characters of content, \
                     the endpoint gave no reason for it",
                    self.mode.as_str(),
                    content.trim().chars().count()
                )));
            }
            return Ok(ModelAnswer::new(program)
                .with_usage(usage)
                .with_reasoning(self.plain_reasoning(&parsed)));
        }
        let body = self.body(task, bundle, budget, false);
        let mut headers: Vec<(&str, String)> =
            vec![("Content-Type", "application/json".to_string())];
        if let Some(key) = &self.key {
            // Resolved here and used here. It is not stored on the struct, not returned, and not logged.
            headers.push(("Authorization", format!("Bearer {}", key.resolve()?)));
        }

        let started = Instant::now();
        let reply = http::post(&self.target, &body, &headers, self.timeout)?;
        let elapsed = started.elapsed();

        if !reply.ok() {
            // The endpoint's own words, cut short, because an error page can be a whole web page and the
            // useful half is at the front. The key cannot be in here: it was sent as a header and never
            // appears in a body this client writes.
            return Err(Fault::model(format!(
                "the model endpoint answered {}: {}",
                reply.status,
                first_line(&reply.body, 300)
            )));
        }

        let parsed: serde_json::Value = serde_json::from_str(&reply.body).map_err(|e| {
            Fault::model(format!(
                "the model endpoint answered {} but not with JSON: {e} (it said: {})",
                reply.status,
                first_line(&reply.body, 200)
            ))
        })?;

        // An endpoint that answers 200 with an error object is still an error.
        if let Some(message) = parsed.pointer("/error/message").and_then(|v| v.as_str()) {
            return Err(Fault::model(format!(
                "the model endpoint returned an error: {}",
                first_line(message, 300)
            )));
        }

        let content = parsed
            .pointer("/choices/0/message/content")
            .and_then(|v| v.as_str())
            .or_else(|| parsed.pointer("/choices/0/text").and_then(|v| v.as_str()))
            .ok_or_else(|| {
                Fault::model(format!(
                    "the model endpoint answered {} with no program in it: {}",
                    reply.status,
                    first_line(&reply.body, 200)
                ))
            })?;

        let mut program = extract(content);
        // A model that answers through the endpoint's tool protocol has still answered. The program is in
        // the call's arguments, and refusing it because it arrived wrapped would blame the model for this
        // client's shape. The call's name and the field the program came out of are kept for the log.
        if program.trim().is_empty()
            && let Some((name, command)) = tool_call_program(&parsed)
        {
            program = command;
            let _ = &name;
        }
        if program.trim().is_empty() {
            // The empty answer has a cause and the cause is worth naming: a thinking model that spent the
            // whole answer thinking, or a model that answered in a field this client does not read, or a
            // model that said nothing. Three different problems, three different sentences.
            // Two names for the same thing in the wild: llama.cpp's template writes
            // `reasoning_content`, and the cloud endpoint writes `reasoning`. Reading only the first made
            // this fault blame the endpoint for a cause it had already stated.
            let reasoning = ["reasoning_content", "reasoning", "thinking"]
                .iter()
                .find_map(|field| {
                    parsed
                        .pointer(&format!("/choices/0/message/{field}"))
                        .and_then(|v| v.as_str())
                })
                .unwrap_or("");
            let finish = parsed
                .pointer("/choices/0/finish_reason")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            // A tool call this client could not read is a fourth cause, and the most confusing one to
            // guess at: the endpoint says `tool_calls` and there is no program because the program is in a
            // field nobody looked at.
            if finish == "tool_calls" {
                return Err(Fault::model(format!(
                    "the endpoint answered with a tool call rather than a program, and no argument of it \
                     carried a command: {}",
                    first_line(&reply.body, 300)
                )));
            }
            return Err(Fault::model(if !reasoning.trim().is_empty() {
                format!(
                    "the model spent its whole answer on reasoning and left no program: {} characters of \
                     thinking, finish reason {finish}. Its template takes `enable_thinking`, and this build \
                     asks for none with `--thinking off`.",
                    reasoning.chars().count()
                )
            } else {
                format!(
                    "the model answered with no program at all: {} characters of content, finish reason \
                     {finish}, and the endpoint gave no reason for it",
                    content.trim().chars().count()
                )
            }));
        }

        let reasoning_text = ["reasoning_content", "reasoning", "thinking"]
            .iter()
            .find_map(|field| {
                parsed
                    .pointer(&format!("/choices/0/message/{field}"))
                    .and_then(|v| v.as_str())
            })
            .unwrap_or("");
        let reasoning_chars = reasoning_text.chars().count() as u64;
        let usage = Usage {
            prompt_tokens: parsed
                .pointer("/usage/prompt_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            cached_tokens: cached_of(parsed.get("usage").unwrap_or(&serde_json::Value::Null)),
            generated_tokens: parsed
                .pointer("/usage/completion_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            millis: elapsed.as_millis(),
            reasoning_chars,
        };
        if let Some(edit_answer) = crate::patch::edits_from(&program) {
            let answer = match edit_answer {
                Ok(edits) => ModelAnswer::editing(edits),
                Err(reason) => ModelAnswer::refused(reason),
            };
            return Ok(answer.with_usage(usage).with_reasoning(reasoning_text));
        }
        Ok(ModelAnswer::new(program)
            .with_usage(usage)
            .with_reasoning(reasoning_text))
    }
}

/// The program out of a tool call, when the endpoint answered with one instead of content.
///
/// The cloud account's default model does exactly this: `content` empty, `finish_reason: tool_calls`, and
/// the work in `message.tool_calls[0].function.arguments`. The command is looked for under the names a
/// shell call uses, and the arguments are taken as the program when they are a bare string.
fn tool_call_program(parsed: &serde_json::Value) -> Option<(String, String)> {
    let call = parsed.pointer("/choices/0/message/tool_calls/0")?;
    let name = call
        .pointer("/function/name")
        .and_then(|v| v.as_str())
        .unwrap_or("unnamed")
        .to_string();
    let arguments = call.pointer("/function/arguments")?;
    // The arguments arrive as a JSON *string* holding JSON, which is the protocol's shape and not a mistake:
    // decode it once and look inside, and fall back to the string itself when it is not an object.
    if let Some(text) = arguments.as_str() {
        let inner: serde_json::Value =
            serde_json::from_str(text).unwrap_or(serde_json::Value::Null);
        for field in ["command", "cmd", "program", "script", "input"] {
            if let Some(command) = inner.get(field).and_then(|v| v.as_str()) {
                return Some((name, command.to_string()));
            }
        }
        return Some((name, text.to_string()));
    }
    for field in ["command", "cmd", "program", "script", "input"] {
        if let Some(text) = arguments.get(field).and_then(|v| v.as_str()) {
            return Some((name, text.to_string()));
        }
    }
    None
}

/// The program out of an answer. A fenced block when there is one, the whole answer otherwise.
///
/// The last fence wins: a model that thinks out loud and then says what it means puts the answer in the
/// last block, and a model that puts an example in the first block and nothing else has not answered.
pub fn extract(content: &str) -> String {
    let mut best: Option<String> = None;
    let mut rest = content;
    while let Some(start) = rest.find("```") {
        let after = &rest[start + 3..];
        // The info string, if any, ends at the first newline.
        let Some(newline) = after.find('\n') else {
            break;
        };
        let body = &after[newline + 1..];
        let Some(end) = body.find("```") else {
            // An unterminated fence: the model was cut off. What it did write is still the answer.
            best = Some(body.to_string());
            break;
        };
        best = Some(body[..end].to_string());
        rest = &body[end + 3..];
    }
    match best {
        Some(block) => block.trim().to_string(),
        None => content.trim().to_string(),
    }
}

/// The first line of a text, cut to a length, for a fault that has to say what happened without pasting a
/// web page into a log.
fn first_line(text: &str, limit: usize) -> String {
    let line = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    if line.chars().count() <= limit {
        return line.to_string();
    }
    let cut: String = line.chars().take(limit).collect();
    format!("{cut}...")
}

/// A named way to reach a model. Not a config file: two paths the operator asked to be able to test, and the CLI
/// can reach anything else with `--endpoint` and `--model`.
#[derive(Debug, Clone, PartialEq)]
pub struct Preset {
    pub id: &'static str,
    pub endpoint: &'static str,
    pub model: &'static str,
    pub key: Option<Key>,
    /// The window this build will ask for. A local preset takes the server's own `n_ctx`; a cloud preset
    /// asks for 32k of a much larger window, on the grounds that a turn needing more than 32k of context
    /// is a turn whose context should have been built better rather than built bigger.
    pub context: usize,
    /// Whether to ask the template for thinking. The local presets ask for none: both are thinking models
    /// and a one-line program does not need a paragraph of deliberation first. The cloud presets say
    /// nothing, because their endpoint is not known to read this field.
    pub thinking: Option<bool>,
    pub note: &'static str,
}

/// The cloud presets only. A local model is a fact about a machine, not about Hades: it belongs in
/// that machine's `endpoints` map, where the pack that ships has no opinion about it.
pub fn presets() -> Vec<Preset> {
    vec![
        Preset {
            id: "deepseek-v4.1-flash",
            thinking: None,
            endpoint: "https://ollama.com/v1/chat/completions",
            // The unversioned alias on purpose: pinning `:0731` produced a 410 the day that build retired,
            // and the pin looked like a working configuration until it was called.
            model: "deepseek-v4.1-flash",
            key: Some(Key::Home("OLLAMA_API_KEY".to_string())),
            context: 32768,
            note: "the cloud account's default: fast, cheap, and the one the account ships as its default",
        },
        Preset {
            id: "kimi-k2.7-code",
            thinking: None,
            endpoint: "https://ollama.com/v1/chat/completions",
            model: "kimi-k2.7-code",
            key: Some(Key::Home("OLLAMA_API_KEY".to_string())),
            context: 32768,
            note: "the account's coding model, for the agentic half of a test",
        },
    ]
}

/// A preset by id, or nothing. Nothing is not an error here; the CLI decides what to say.
pub fn preset(id: &str) -> Option<Preset> {
    presets().into_iter().find(|p| p.id == id)
}

/// A preset as a harness.
/// A provider is one URL, one key and any number of models. This is the route a config names when it
/// does not want a shipped preset: `groq/llama-3.3-70b`, or the bare name if only one provider lists it.
pub fn from_provider(provider: &crate::provider::Provider, model: &str) -> OpenAi {
    let url = provider.endpoint_url();
    let mut client = OpenAi::new(&url, model)
        .with_context(provider.context)
        .with_mode(provider.api_mode)
        .with_headers(
            provider
                .headers
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect(),
        );
    if let Some(thinking) = provider.thinking {
        client = client.with_thinking(thinking);
    }
    if let Some(var) = &provider.key_env {
        client = client.with_key(Key::Home(var.clone()));
    }
    client
}

/// Resolve a model name the way every config-driven path should: a provider first (it is the most
/// specific thing a person can write), then a named endpoint, then a shipped preset. The error lists
/// what does resolve, because "no such model" without the list is a puzzle.
pub fn client_for_config(config: &crate::config::Config, name: &str) -> Result<OpenAi, String> {
    let client = one_for_config(config, name)?;
    let mut chain = Vec::new();
    for fallback in &config.fallbacks {
        if fallback == name {
            continue;
        }
        chain.push(one_for_config(config, fallback)?);
    }
    Ok(client.with_fallbacks(chain))
}

/// One client, without the chain: the name resolves to a provider, an endpoint or a preset.
fn one_for_config(config: &crate::config::Config, name: &str) -> Result<OpenAi, String> {
    if let Some((provider, model)) = config.providers.resolve(name) {
        return Ok(from_provider(provider, &model));
    }
    if let Some(endpoint) = config.endpoints.get(name) {
        let mut client = OpenAi::new(&endpoint.url, &endpoint.model)
            .with_context(endpoint.context_tokens)
            .with_thinking(endpoint.thinking);
        if let Some(var) = &endpoint.key_env {
            client = client.with_key(Key::Home(var.clone()));
        }
        return Ok(client);
    }
    if let Some(preset) = preset(name) {
        return Ok(from_preset(&preset));
    }
    let mut known = config.providers.known();
    known.extend(config.endpoints.keys().cloned());
    known.extend(presets().iter().map(|preset| preset.id.to_string()));
    Err(format!(
        "no model named `{name}`: try {}",
        known.join(", ")
    ))
}

/// A provider named in a surface's config instead of a preset, so a connector can answer through a
/// different endpoint and model from the system it belongs to. The shape is deliberately the same as
/// a preset: endpoint, model, optional context, thinking, key.
///
/// ```json
/// "provider": {"endpoint": "http://127.0.0.1:8100/v1/chat/completions",
///              "model": "local-9b", "key_env": "MY_KEY", "context": 32768}
/// ```
pub fn from_config(value: &serde_json::Value) -> Result<OpenAi, String> {
    let endpoint = value
        .get("endpoint")
        .or_else(|| value.get("url"))
        .or_else(|| value.get("base_url"))
        .and_then(|text| text.as_str())
        .filter(|text| !text.trim().is_empty())
        .ok_or("a `provider` needs `endpoint`")?;
    let name = value
        .get("model")
        .and_then(|text| text.as_str())
        .filter(|text| !text.trim().is_empty())
        .ok_or("a `provider` needs `model`")?;
    let context = value
        .get("context")
        .or_else(|| value.get("context_tokens"))
        .and_then(|number| number.as_u64())
        .unwrap_or(32_768) as usize;
    let mut client = OpenAi::new(endpoint, name).with_context(context);
    if let Some(thinking) = value.get("thinking").and_then(|flag| flag.as_bool()) {
        client = client.with_thinking(thinking);
    }
    if let Some(var) = value
        .get("key_env")
        .and_then(|text| text.as_str())
        .filter(|text| !text.trim().is_empty())
    {
        // The name travels; the value is read when a call is made, so a config never holds a secret.
        client = client.with_key(Key::Home(var.to_string()));
    }
    Ok(client)
}

pub fn from_preset(preset: &Preset) -> OpenAi {
    let mut model = OpenAi::new(preset.endpoint, preset.model).with_context(preset.context);
    if let Some(thinking) = preset.thinking {
        model = model.with_thinking(thinking);
    }
    if let Some(key) = &preset.key {
        model = model.with_key(key.clone());
    }
    model
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-shot HTTP server: each entry is the response body, and every request it saw comes back so a
    /// test can assert what was actually sent.
    fn stub(codes: Vec<u16>, bodies: Vec<&'static str>) -> (String, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a port");
        let address = format!("http://{}", listener.local_addr().expect("an address"));
        let handle = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for (code, body) in codes.iter().zip(bodies.iter()) {
                let (mut stream, _) = listener.accept().expect("a connection");
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .ok();
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 4096];
                let mut headers_done = false;
                let mut content_length = 0usize;
                loop {
                    let read = stream.read(&mut chunk).unwrap_or(0);
                    if read == 0 {
                        break;
                    }
                    buffer.extend_from_slice(&chunk[..read]);
                    if !headers_done && buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                        headers_done = true;
                        let text = String::from_utf8_lossy(&buffer).to_string();
                        content_length = text
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|value| value.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                    }
                    if headers_done {
                        let split = buffer
                            .windows(4)
                            .position(|window| window == b"\r\n\r\n")
                            .map(|at| at + 4)
                            .unwrap_or(buffer.len());
                        if buffer.len() >= split + content_length {
                            break;
                        }
                    }
                }
                seen.push(String::from_utf8_lossy(&buffer).to_string());
                let reply = format!(
                    "HTTP/1.1 {code} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(reply.as_bytes()).ok();
                stream.flush().ok();
            }
            seen
        });
        (address, handle)
    }

    #[test]
    fn an_anthropic_call_puts_the_system_prompt_beside_the_turns_and_reads_content_blocks() {
        let (address, server) = stub(
            vec![200],
            vec![r#"{"content":[{"type":"text","text":"ok"}],"usage":{"input_tokens":5,"output_tokens":2}}"#],
        );
        let client = OpenAi::new(format!("{address}/v1/messages"), "claude-x")
            .with_mode(crate::provider::ApiMode::AnthropicMessages);
        let completion = client.complete("be brief", "hello", &Budget::new(8192)).expect("an answer");
        assert_eq!(completion.text, "ok");
        let request = server.join().expect("the server");
        let sent = &request[0];
        assert!(sent.contains("anthropic-version: 2023-06-01"), "{sent}");
        assert!(sent.contains("\"system\":\"be brief\""), "{sent}");
        assert!(sent.contains("\"role\":\"user\""), "{sent}");
        assert!(sent.contains("\"content\":\"hello\""), "{sent}");
        assert!(!sent.contains("\"choices\""), "{sent}");
    }

    #[test]
    fn a_responses_call_reads_output_text_and_still_puts_the_system_prompt_first() {
        let (address, server) = stub(
            vec![200],
            vec![r#"{"output":[{"content":[{"type":"output_text","text":"ok"}]}],"usage":{"input_tokens":3,"output_tokens":1}}"#],
        );
        let client = OpenAi::new(format!("{address}/v1/responses"), "gpt-x")
            .with_mode(crate::provider::ApiMode::Responses);
        let completion = client.complete("be brief", "hello", &Budget::new(8192)).expect("an answer");
        assert_eq!(completion.text, "ok");
        let request = server.join().expect("the server");
        let sent = &request[0];
        assert!(sent.contains("\"instructions\":\"be brief\""), "{sent}");
        assert!(sent.contains("\"input\":["), "{sent}");
        assert!(sent.contains("\"role\":\"user\""), "{sent}");
    }

    #[test]
    fn a_config_chain_tries_the_named_fallbacks_in_the_order_written() {
        let (first, first_server) = stub(vec![500], vec![r#"{"error":{"message":"down"}}"#]);
        let (second, second_server) = stub(
            vec![200],
            vec![r#"{"content":[{"type":"text","text":"answered by the backup"}],"usage":{"input_tokens":1,"output_tokens":1}}"#],
        );
        let dir = std::env::temp_dir().join(format!("hades-chain-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a dir");
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            format!(
                r#"{{"version":1,"provider":"primary","model":"primary/m","root":"/tmp","vault":"/tmp",
                    "providers":{{
                      "primary":{{"base_url":"{first}/v1","api_mode":"anthropic_messages"}},
                      "backup":{{"base_url":"{second}/v1","api_mode":"anthropic_messages"}}}},
                    "fallback_providers":["backup/m"]}}"#
            ),
        )
        .expect("write");
        let config = crate::config::load(&path)
            .expect("load")
            .expect("a config");
        assert_eq!(config.fallbacks, vec!["backup/m".to_string()]);
        let client = client_for_config(&config, "primary/m").expect("a client");
        let completion = client
            .complete("be brief", "hello", &Budget::new(8192))
            .expect("the backup answers");
        assert_eq!(completion.text, "answered by the backup");
        first_server.join().expect("first");
        second_server.join().expect("second");
    }

    #[test]
    fn a_failed_provider_is_followed_by_the_next_one_in_the_chain() {
        let (first, first_server) = stub(vec![500], vec![r#"{"error":{"message":"upstream is down"}}"#]);
        let (second, second_server) = stub(
            vec![200],
            vec![r#"{"content":[{"type":"text","text":"from the fallback"}],"usage":{"input_tokens":4,"output_tokens":2}}"#],
        );
        let primary = OpenAi::new(format!("{first}/v1/messages"), "primary")
            .with_mode(crate::provider::ApiMode::AnthropicMessages);
        let backup = OpenAi::new(format!("{second}/v1/messages"), "backup")
            .with_mode(crate::provider::ApiMode::AnthropicMessages);
        let client = primary.with_fallbacks(vec![backup]);
        let completion = client.complete("be brief", "hello", &Budget::new(8192)).expect("the second answers");
        assert_eq!(completion.text, "from the fallback");
        first_server.join().expect("first");
        second_server.join().expect("second");
    }



    #[test]
    fn a_fenced_answer_is_unwrapped() {
        assert_eq!(extract("```sh\nls -la\n```"), "ls -la");
        assert_eq!(extract("```\necho hi\n```"), "echo hi");
        assert_eq!(
            extract("here you go:\n```sh\ntrue\n```\nthat runs it"),
            "true"
        );
    }

    #[test]
    fn an_unfenced_answer_is_taken_as_it_stands() {
        assert_eq!(extract("  echo hi\n  "), "echo hi");
        assert_eq!(extract("echo one\necho two"), "echo one\necho two");
    }

    #[test]
    fn the_last_fence_wins() {
        // The first block is an example, the second is the answer. A model that thinks out loud and then
        // says what it means puts the answer last.
        let answer =
            "For example:\n```sh\necho not this\n```\nSo the program is:\n```sh\necho this\n```";
        assert_eq!(extract(answer), "echo this");
    }

    #[test]
    fn an_unterminated_fence_is_still_an_answer() {
        // The model was cut off mid-answer. What it wrote is what it meant, so it is used rather than
        // dropped for being unfinished.
        assert_eq!(extract("```sh\necho half an ans"), "echo half an ans");
    }

    #[test]
    fn prose_with_no_program_comes_back_empty_rather_than_as_prose() {
        assert_eq!(extract("   "), "");
        assert_eq!(extract("\n\n"), "");
    }

    #[test]
    fn the_request_names_the_goal_the_directory_and_the_model() {
        use crate::task::TaskSpec;
        let task = TaskSpec::new("write a file", "/tmp/here");
        let bundle = Bundle::new(
            vec![crate::context::Section::new("notes", "a note")],
            vec![],
            12,
        );
        let model = OpenAi::new("http://127.0.0.1:8110/v1/chat/completions", "local-9b");
        let body = model.body(&task, &bundle, &Budget::new(4096), false);
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(parsed["model"], "local-9b");
        assert_eq!(parsed["stream"], false);
        assert_eq!(parsed["messages"][0]["role"], "system");
        let user = parsed["messages"][1]["content"]
            .as_str()
            .expect("a user message");
        assert!(user.contains("write a file"), "{user}");
        assert!(user.contains("/tmp/here"), "{user}");
        assert!(user.contains("a note"), "{user}");
    }

    #[test]
    fn a_harness_replaces_the_system_message_and_leaves_the_rest_alone() {
        use crate::task::TaskSpec;
        let task = TaskSpec::new("write a file", "/tmp/here");
        let bundle = Bundle::new(
            vec![crate::context::Section::new("notes", "a note")],
            vec![],
            12,
        );
        let model = OpenAi::new("http://127.0.0.1:8110/v1/chat/completions", "local-9b")
            .with_system("You are terse.\n");
        let body = model.body(&task, &bundle, &Budget::new(4096), false);
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(parsed["messages"][0]["content"], "You are terse.");
        let user = parsed["messages"][1]["content"]
            .as_str()
            .expect("a user message");
        assert!(user.contains("write a file"), "{user}");
        assert!(user.contains("a note"), "{user}");
    }

    #[test]
    fn the_instruction_says_the_program_is_run_and_not_saved() {
        // A model asked for a script reads its answer as the script's contents, and a real run proved it:
        // the local 3B answered with a script and the words "save as stats.sh", then appended `chmod +x
        // stats.sh` to the program after being told the file was missing. The program is run, not written,
        // and the instruction has to say so or the seam is ambiguous in a way the check cannot explain.
        let says = SYSTEM.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(says.contains("not saved anywhere"), "{says}");
        assert!(says.contains("your program has to write it"), "{says}");
        // And that there may be another attempt: a model that thinks its one program is the whole
        // interaction spends it looking, and a model that knows otherwise spends it fixing.
        assert!(says.contains("more than one attempt"), "{says}");
        assert!(
            says.contains("gather what you need and then change it"),
            "{says}"
        );
    }

    #[test]
    fn a_program_that_arrives_as_a_tool_call_is_still_a_program() {
        // The account's default model answers this way: `content` empty, `finish_reason: tool_calls`, and
        // the command inside `message.tool_calls[0].function.arguments`, which is itself a JSON string.
        // Refusing that blamed the model for a shape this client did not read.
        let body = serde_json::json!({
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "content": "",
                    "tool_calls": [{
                        "type": "function",
                        "function": {
                            "name": "shell",
                            "arguments": "{\"command\": \"python3 -m pytest -q\"}"
                        }
                    }]
                }
            }]
        });
        let (name, program) = tool_call_program(&body).expect("a program");
        assert_eq!(name, "shell");
        assert_eq!(program, "python3 -m pytest -q");

        // And an arguments string that is not JSON is the program itself, not a parse failure.
        let plain = serde_json::json!({
            "choices": [{"message": {"tool_calls": [{"function": {"name": "shell", "arguments": "ls -la"}}]}}]
        });
        assert_eq!(
            tool_call_program(&plain).map(|(_, program)| program),
            Some("ls -la".to_string())
        );

        // Nothing to read is None rather than an empty program.
        let empty = serde_json::json!({"choices": [{"message": {"content": "hi"}}]});
        assert!(tool_call_program(&empty).is_none());
    }

    #[test]
    fn a_key_that_is_not_there_is_refused_by_name_and_never_by_value() {
        let fault = Key::Env("HADES_TEST_KEY_THAT_IS_NOT_SET".to_string())
            .resolve()
            .expect_err("refused");
        assert!(
            fault.detail().contains("HADES_TEST_KEY_THAT_IS_NOT_SET"),
            "{}",
            fault.detail()
        );
    }

    #[test]
    fn a_provider_error_frame_is_retried_and_not_hidden_as_an_empty_stream() {
        use std::io::{BufRead, BufReader, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("addr");
        let server = std::thread::spawn(move || {
            let answers = [
                "data: {\"error\":{\"message\":\"429 rate limit reached\"}}\n\ndata: [DONE]\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n",
            ];
            for answer in answers {
                let (mut socket, _) = listener.accept().expect("accept");
                let mut reader = BufReader::new(socket.try_clone().expect("clone"));
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).expect("header");
                    if line.trim().is_empty() {
                        break;
                    }
                }
                write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    answer.len(),
                    answer
                )
                .expect("write");
            }
        });

        let client = OpenAi::new(
            format!("http://{}/v1/chat/completions", address),
            "test-model",
        );
        let messages = vec![serde_json::json!({"role": "user", "content": "hi"})];
        let reply = client
            .chat(&messages, None, &mut |_, _| {})
            .expect("the second attempt answers");
        assert_eq!(reply.content, "ok");
        server.join().expect("server");
    }

    #[test]
    fn a_reasoning_only_answer_at_the_cap_is_retried_with_a_bigger_cap() {
        use std::io::{BufRead, BufReader, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("addr");
        let server = std::thread::spawn(move || {
            let answers = [
                "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"thinking\"},\"finish_reason\":\"length\"}]}\n\ndata: [DONE]\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n",
            ];
            for answer in answers {
                let (mut socket, _) = listener.accept().expect("accept");
                let mut reader = BufReader::new(socket.try_clone().expect("clone"));
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).expect("header");
                    if line.trim().is_empty() {
                        break;
                    }
                }
                write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    answer.len(),
                    answer
                )
                .expect("write");
            }
        });

        let client = OpenAi::new(
            format!("http://{}/v1/chat/completions", address),
            "test-model",
        );
        let messages = vec![serde_json::json!({"role": "user", "content": "hi"})];
        let reply = client
            .chat_with_cap(&messages, None, 4_096, &mut |_, _| {})
            .expect("the bigger cap answers");
        assert_eq!(reply.content, "ok");
        server.join().expect("server");
    }

    #[test]
    fn an_empty_turn_is_retried_with_one_nudge_and_no_repeat_of_the_same_silence() {
        use std::io::{BufRead, BufReader, Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("addr");
        let server = std::thread::spawn(move || {
            let answers = [
                "data: {\"choices\":[{\"delta\":{\"reasoning\":\"hmm\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
                "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"shell\",\"arguments\":\"{\\\"command\\\":\\\"true\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n",
            ];
            let mut bodies = Vec::new();
            for answer in answers {
                let (mut socket, _) = listener.accept().expect("accept");
                let mut reader = BufReader::new(socket.try_clone().expect("clone"));
                let mut length = 0usize;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).expect("header");
                    if line.trim().is_empty() {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().expect("length");
                    }
                }
                let mut body = vec![0u8; length];
                reader.read_exact(&mut body).expect("body");
                bodies.push(String::from_utf8_lossy(&body).to_string());
                write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    answer.len(),
                    answer
                )
                .expect("write");
            }
            bodies
        });

        let client = OpenAi::new(
            format!("http://{}/v1/chat/completions", address),
            "test-model",
        );
        let messages = vec![serde_json::json!({"role": "user", "content": "hi"})];
        let tools = serde_json::json!([
            {"type": "function", "function": {"name": "shell", "parameters": {"type": "object"}}}
        ]);
        let reply = client
            .chat(&messages, Some(&tools), &mut |_, _| {})
            .expect("the forced call answers");
        assert_eq!(reply.calls.len(), 1);
        assert_eq!(reply.calls[0].name, "shell");
        let bodies = server.join().expect("server");
        assert!(
            bodies[1].contains("Continue with the next step."),
            "the retry must carry a nudge: {}",
            bodies[1]
        );
        assert!(
            bodies[1].contains("\"tool_choice\":\"required\""),
            "the first retry must force a call: {}",
            bodies[1]
        );
        assert!(
            bodies[2].contains("\"tool_choice\":\"required\""),
            "the last attempt must force a call: {}",
            bodies[2]
        );
    }

    #[test]
    fn a_key_can_be_read_out_of_a_dotenv_file() {
        let text = "OTHER=1\n# a comment\nOLLAMA_API_KEY=abc123\nLATER=z\n";
        assert_eq!(
            key_from_text(text, "OLLAMA_API_KEY").as_deref(),
            Some("abc123")
        );
        assert_eq!(
            key_from_text("K=\"quoted\"\n", "K").as_deref(),
            Some("quoted")
        );
        assert_eq!(
            key_from_text("K='single'\n", "K").as_deref(),
            Some("single")
        );
        // A commented line is not a value, and an empty value is not a key.
        assert_eq!(key_from_text("# K=nope\n", "K"), None);
        assert_eq!(key_from_text("K=\n", "K"), None);
        assert_eq!(key_from_text("OTHERKEY=1\n", "K"), None);
    }

    #[test]
    fn every_preset_says_where_it_goes_and_only_the_cloud_ones_ship() {
        let all = presets();
        assert_eq!(all.len(), 2, "the cloud presets; local models live in `endpoints`");
        assert_eq!(all.iter().filter(|p| p.key.is_some()).count(), 2);
        for preset in &all {
            assert!(preset.endpoint.starts_with("http"), "{}", preset.id);
            assert!(parses(preset.endpoint), "{}", preset.id);
        }
        assert!(preset("deepseek-v4.1-flash").is_some());
        // A local model is a fact about a machine, not about Hades: it must not be compiled in.
        assert!(preset("local-9b").is_none());
        assert!(preset("nope").is_none());
    }

    /// Whether the URL would parse, so a preset with a typo fails here rather than at a call.
    fn parses(url: &str) -> bool {
        crate::http::Target::parse(url).is_ok()
    }
}

/// One answer from a chat call: the prose it wrote, and the tools it asked to run.
#[derive(Clone, Debug, Default)]
pub struct Reply {
    pub content: String,
    pub calls: Vec<ToolCall>,
    pub usage: Option<Usage>,
}

#[derive(Clone, Debug, Default)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

impl OpenAi {
    /// A chat step with tools: the endpoint may answer in prose, ask for tools, or both. Deltas stream
    /// straight to the feed, and fragmented tool calls are stitched by index because a name and its
    /// arguments arrive in separate chunks.
    pub fn chat(
        &self,
        messages: &[serde_json::Value],
        tools: Option<&serde_json::Value>,
        feed: FeedFn<'_>,
    ) -> Result<Reply, Fault> {
        self.chat_with_cap(messages, tools, self.max_tokens as u64, feed)
    }

    /// The same call under a cap the caller computed for this call. A budget is a reservation, and the
    /// completion cap is only half of what a call costs: the prompt is the other half, so the caller that
    /// knows what is left is the caller that must choose the cap.
    pub fn chat_with_cap(
        &self,
        messages: &[serde_json::Value],
        tools: Option<&serde_json::Value>,
        cap: u64,
        feed: FeedFn<'_>,
    ) -> Result<Reply, Fault> {
        let mut errors: Vec<String> = Vec::new();
        for candidate in self.chain() {
            match candidate.chat_one(messages, tools, cap, &feed) {
                Ok(reply) => return Ok(reply),
                Err(error) => errors.push(format!("{}: {error}", candidate.model)),
            }
        }
        Err(Fault::model(match errors.len() {
            1 => errors.pop().unwrap_or_default(),
            _ => format!("every provider failed: {}", errors.join("; ")),
        }))
    }

    fn chat_one(
        &self,
        messages: &[serde_json::Value],
        tools: Option<&serde_json::Value>,
        cap: u64,
        feed: &FeedFn<'_>,
    ) -> Result<Reply, Fault> {
        if self.mode != crate::provider::ApiMode::ChatCompletions {
            return Err(Fault::model(format!(
                "`{}` has no tool protocol in this build: a turn with tools needs chat_completions, \
                 and this provider is configured as {}",
                self.model,
                self.mode.as_str()
            )));
        }
        let mut cap = cap;
        // A model can end a turn with nothing at all after a tool result: finish "stop", no
        // content, no call. Repeating the identical request buys the same silence, so the retry
        // carries one short user nudge. It is for this request only and never enters the session.
        let mut nudged: Option<Vec<serde_json::Value>> = None;
        let mut headers: Vec<(&str, String)> =
            vec![("Content-Type", "application/json".to_string())];
        if let Some(key) = &self.key {
            headers.push(("Authorization", format!("Bearer {}", key.resolve()?)));
        }
        let mut last_fault = String::from("the endpoint streamed neither an answer nor a tool call");
        for attempt in 0..3u32 {
            if attempt > 0 {
                let wait = if attempt == 1 { 1_000 } else { 3_000 };
                std::thread::sleep(std::time::Duration::from_millis(wait));
            }
            let mut body = serde_json::json!({
                "model": self.model,
                "messages": nudged.as_deref().unwrap_or(messages),
                "temperature": self.temperature,
                "max_tokens": cap.min(u32::MAX as u64),
                "stream": true,
            });
            if self.usage_log.is_some() {
                body["stream_options"] = serde_json::json!({"include_usage": true});
            }
            if let Some(tools) = tools {
                body["tools"] = tools.clone();
                // The first retry forces an action. A model that has decided the task is finished
                // answers an empty turn no matter how it is asked; a required tool call is the one
                // instruction it cannot satisfy with silence.
                if attempt >= 1 {
                    body["tool_choice"] = serde_json::json!("required");
                }
            }
            let mut reply = Reply::default();
            let started = std::time::Instant::now();
            let mut partial: std::collections::BTreeMap<u64, ToolCall> =
                std::collections::BTreeMap::new();
            let mut reasoning_chars = 0usize;
            let mut provider_error: Option<String> = None;
            let mut finish_reason = String::new();
            let streamed = http::stream(
                &self.target,
                &body.to_string(),
                &headers,
                self.timeout,
                &mut |line| {
                    let Some(data) = line.trim().strip_prefix("data:") else {
                        return;
                    };
                    let data = data.trim();
                    if data.is_empty() || data == "[DONE]" {
                        return;
                    }
                    let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
                        return;
                    };
                    // A provider-side failure arrives as data, not as an HTTP status. Read it,
                    // or the turn reports an empty stream and hides the reason.
                    if let Some(error) = value.get("error") {
                        provider_error = Some(
                            error
                                .get("message")
                                .and_then(Value::as_str)
                                .or_else(|| error.as_str())
                                .map(str::to_string)
                                .unwrap_or_else(|| error.to_string()),
                        );
                        return;
                    }
                    // Usage-only frames have an empty choices array and must be read before delta.
                    if let Some(usage) = value.get("usage").filter(|usage| {
                        usage.get("prompt_tokens").and_then(Value::as_u64).is_some()
                            || usage
                                .get("completion_tokens")
                                .and_then(Value::as_u64)
                                .is_some()
                    }) {
                        reply.usage = Some(Usage {
                            prompt_tokens: usage
                                .get("prompt_tokens")
                                .and_then(Value::as_u64)
                                .unwrap_or(0),
                            cached_tokens: cached_of(usage),
                            generated_tokens: usage
                                .get("completion_tokens")
                                .and_then(Value::as_u64)
                                .unwrap_or(0),
                            ..Usage::default()
                        });
                    }
                    if let Some(reason) = value
                        .pointer("/choices/0/finish_reason")
                        .and_then(Value::as_str)
                    {
                        finish_reason = reason.to_string();
                    }
                    let Some(delta) = value.pointer("/choices/0/delta") else {
                        return;
                    };
                    if let Some(piece) = delta.get("content").and_then(|v| v.as_str())
                        && !piece.is_empty()
                    {
                        reply.content.push_str(piece);
                        feed(Feed::Content, piece);
                    }
                    for field in ["reasoning_content", "reasoning", "thinking"] {
                        if let Some(piece) = delta.get(field).and_then(|v| v.as_str())
                            && !piece.is_empty()
                        {
                            reasoning_chars += piece.chars().count();
                            feed(Feed::Reasoning, piece);
                        }
                    }
                    if let Some(calls) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                        for call in calls {
                            let index = call.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
                            let slot = partial.entry(index).or_default();
                            if let Some(id) = call.get("id").and_then(|v| v.as_str())
                                && !id.is_empty()
                            {
                                slot.id = id.to_string();
                            }
                            if let Some(function) = call.get("function") {
                                if let Some(name) = function.get("name").and_then(|v| v.as_str())
                                    && !name.is_empty()
                                {
                                    slot.name = name.to_string();
                                }
                                if let Some(arguments) =
                                    function.get("arguments").and_then(|v| v.as_str())
                                {
                                    slot.arguments.push_str(arguments);
                                }
                            }
                        }
                    }
                },
            );
            if let Err(fault) = streamed {
                last_fault = fault.detail().to_string();
                if attempt < 2 {
                    continue;
                }
                return Err(Fault::model(last_fault));
            }
            if let Some(message) = provider_error {
                last_fault = format!("the endpoint reported: {message}");
                let transient = {
                    let text = last_fault.to_lowercase();
                    [
                        "429", "500", "502", "503", "504", "overload", "rate limit", "timeout",
                        "temporar", "try again",
                    ]
                    .iter()
                    .any(|needle| text.contains(needle))
                };
                if transient && attempt < 2 {
                    continue;
                }
                return Err(Fault::model(last_fault));
            }
            reply.calls = partial.into_values().collect();
            if let Some(usage) = &mut reply.usage {
                usage.millis = started.elapsed().as_millis();
                if let Some(log) = &self.usage_log
                    && let Ok(mut log) = log.lock()
                {
                    log.push(*usage);
                }
            }
            if reply.content.trim().is_empty() && reply.calls.is_empty() {
                // A thinking model that spent the whole cap gets one bigger cap. Anything else,
                // including a turn that reasoned for a few characters and then stopped, is a
                // stochastic empty answer: the same request can answer on the next try, and one
                // nudge makes the retry a different request rather than a repeat.
                if finish_reason == "length" && cap < 16_384 {
                    cap = (cap * 2).min(16_384);
                    continue;
                }
                last_fault = if reasoning_chars > 0 {
                    format!(
                        "the model reasoned {reasoning_chars} characters and produced no answer \
                         or tool call (finish: {})",
                        if finish_reason.is_empty() {
                            "none"
                        } else {
                            finish_reason.as_str()
                        }
                    )
                } else {
                    "the endpoint streamed neither an answer nor a tool call".to_string()
                };
                if attempt < 2 {
                    if nudged.is_none() {
                        let mut wire = messages.to_vec();
                        wire.push(serde_json::json!({
                            "role": "user",
                            "content": "Continue with the next step."
                        }));
                        nudged = Some(wire);
                    }
                    continue;
                }
                return Err(Fault::model(last_fault));
            }
            return Ok(reply);
        }
        Err(Fault::model(last_fault))
    }
}
