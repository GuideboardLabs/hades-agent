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

    /// A plan, before any program. Separate from `run` because it is prose, not an answer, and because a
    /// plan-first turn has to record it before the call that can change the tree. A harness that cannot
    /// plan returns an empty string, which the turn treats as nothing written.
    fn plan(&self, task: &TaskSpec, bundle: &Bundle, budget: &Budget) -> Result<String, Fault>;
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
    /// Whether to ask the endpoint's template for thinking. `None` sends nothing, which is what a cloud
    /// endpoint wants; a local thinking model whose template understands `enable_thinking` is asked for
    /// none, because a model that thinks before it writes a one-line program spends the whole answer
    /// thinking and leaves the program empty.
    thinking: Option<bool>,
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
            // to read. Unused headroom costs nothing, because the model stops when it is done.
            max_tokens: 4096,
            timeout: Duration::from_secs(180),
            // Conservative by default, because the alternative is asking a model for more than its window
            // and having the server truncate silently, which is the worst outcome of the three: a context
            // that looks built and is not.
            context: 8192,
            thinking: None,
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

    /// What a person may see: where it goes and which model, never a key.
    pub fn display(&self) -> String {
        format!("{} at {}", self.model, self.target)
    }

    /// The request body. Separate from sending it, so a test can read exactly what would be sent.
    pub fn body(
        &self,
        task: &TaskSpec,
        bundle: &Bundle,
        budget: &Budget,
        streaming: bool,
    ) -> String {
        let user = format!(
            "Goal: {}\nWorking directory: {}\n\n{}\n\nAnswer with the JSON edit object when this step changes text already in a file, or with a shell \
             program for anything else (a new file, a build, an inspection). \
             Context budget for this turn: {} tokens.",
            task.goal(),
            task.root().display(),
            bundle.text(),
            budget.context_tokens()
        );
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

impl Harness for OpenAi {
    /// The plan is a chat call, not a program call: one system message and one user message, no tools, and
    /// the answer is prose. The client's own system prompt is not used, because a plan is not an answer and
    /// the harness text must not make the model write a program here.
    fn plan(&self, task: &TaskSpec, bundle: &Bundle, _budget: &Budget) -> Result<String, Fault> {
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
        // Nobody is watching: one POST, no stream, no second request when an endpoint ignores `stream`.
        let Some(feed) = feed else {
            return self.once(task, bundle, budget);
        };
        let mut content = String::new();
        let mut reasoning = String::new();
        let mut usage: Option<(u64, u64)> = None;
        let mut saw = false;
        let started = Instant::now();
        let body = self.body(task, bundle, budget, true);
        let mut headers: Vec<(&str, String)> =
            vec![("Content-Type", "application/json".to_string())];
        if let Some(key) = &self.key {
            headers.push(("Authorization", format!("Bearer {}", key.resolve()?)));
        }
        http::stream(&self.target, &body, &headers, self.timeout, &mut |line| {
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
                usage = Some((prompt, generated));
            }
        })?;
        let elapsed = started.elapsed();

        if !saw || content.trim().is_empty() {
            // No deltas at all: this endpoint does not stream, and one POST is the answer.
            return self.once(task, bundle, budget);
        }
        let program = extract(&content);
        let (prompt_tokens, generated_tokens) = usage.unwrap_or((0, 0));
        if let Some(edit_answer) = crate::patch::edits_from(&program) {
            let answer = match edit_answer {
                Ok(edits) => ModelAnswer::editing(edits),
                Err(reason) => ModelAnswer::refused(reason),
            };
            return Ok(answer
                .with_usage(Usage {
                    prompt_tokens,
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
                generated_tokens,
                millis: elapsed.as_millis(),
                reasoning_chars: reasoning.chars().count() as u64,
            })
            .with_reasoning(reasoning.clone());
        Ok(answer)
    }
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
        let body = serde_json::json!({
            "model": self.model,
            "messages": messages,
            "temperature": self.temperature,
            "max_tokens": self.max_tokens,
            "stream": true,
        });
        let mut headers: Vec<(&str, String)> =
            vec![("Content-Type", "application/json".to_string())];
        if let Some(key) = &self.key {
            headers.push(("Authorization", format!("Bearer {}", key.resolve()?)));
        }
        let mut content = String::new();
        http::stream(
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
        )?;
        if content.trim().is_empty() {
            return Err(Fault::model(
                "the endpoint streamed no answer at all".to_string(),
            ));
        }
        Ok(content)
    }

    fn once(
        &self,
        task: &TaskSpec,
        bundle: &Bundle,
        budget: &Budget,
    ) -> Result<ModelAnswer, Fault> {
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

/// The two paths, and the two strongest local models behind the agentic bench.
pub fn presets() -> Vec<Preset> {
    vec![
        Preset {
            id: "nanbeige-3b",
            thinking: Some(false),
            endpoint: "http://127.0.0.1:8110/v1/chat/completions",
            model: "nanbeige-3b",
            key: None,
            context: 8192,
            note: "the best Agon agentic result: 24/24, 44.9 t/s, Q4_K_M. Start a server on 8110 first.",
        },
        Preset {
            id: "bonsai-27b",
            thinking: Some(false),
            endpoint: "http://127.0.0.1:8111/v1/chat/completions",
            model: "bonsai-27b",
            key: None,
            context: 8192,
            note: "the other 24/24, a 27B ternary at 10.9 t/s. Needs the Prism fork; port 8111.",
        },
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
            note: "the account's coding model, for the agentic half of the test. The account has no \
                   qwen3-coder, which was the first guess here and was wrong",
        },
    ]
}

/// A preset by id, or nothing. Nothing is not an error here; the CLI decides what to say.
pub fn preset(id: &str) -> Option<Preset> {
    presets().into_iter().find(|p| p.id == id)
}

/// A preset as a harness.
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
        let model = OpenAi::new("http://127.0.0.1:8110/v1/chat/completions", "nanbeige-3b");
        let body = model.body(&task, &bundle, &Budget::new(4096), false);
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(parsed["model"], "nanbeige-3b");
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
        let model = OpenAi::new("http://127.0.0.1:8110/v1/chat/completions", "nanbeige-3b")
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
    fn every_preset_says_where_it_goes_and_two_of_them_are_the_cloud() {
        let all = presets();
        assert_eq!(all.len(), 4, "two local, two cloud");
        assert_eq!(all.iter().filter(|p| p.key.is_some()).count(), 2);
        for preset in &all {
            assert!(preset.endpoint.starts_with("http"), "{}", preset.id);
            assert!(parses(preset.endpoint), "{}", preset.id);
        }
        assert!(preset("nanbeige-3b").is_some());
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
        let mut body = serde_json::json!({
            "model": self.model,
            "messages": messages,
            "temperature": self.temperature,
            "max_tokens": self.max_tokens,
            "stream": true,
        });
        if let Some(tools) = tools {
            body["tools"] = tools.clone();
        }
        let mut headers: Vec<(&str, String)> =
            vec![("Content-Type", "application/json".to_string())];
        if let Some(key) = &self.key {
            headers.push(("Authorization", format!("Bearer {}", key.resolve()?)));
        }
        let mut reply = Reply::default();
        let mut partial: std::collections::BTreeMap<u64, ToolCall> =
            std::collections::BTreeMap::new();
        http::stream(
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
        )?;
        reply.calls = partial.into_values().collect();
        if reply.content.trim().is_empty() && reply.calls.is_empty() {
            return Err(Fault::model(
                "the endpoint streamed neither an answer nor a tool call".to_string(),
            ));
        }
        Ok(reply)
    }
}
