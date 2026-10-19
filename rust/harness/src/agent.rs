//! A conversation with tools.
//!
//! This is the loop a chat surface runs: the model may answer in prose, or ask for tools, or both, and
//! the loop keeps going until it answers. There is no program, no check and no attempt budget here. Those
//! belong to a governed run, where the question is whether a piece of work is done; a chat asks what the
//! person said and what the agent knows.

use crate::fault::Fault;
use crate::memory::{Memory, MnemeMemory};
use crate::model::{Feed, FeedFn, OpenAi};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// The model calls a whole conversation may spend, shared by the person's loop and every worker it
/// delegates to. A worker gets no fresh budget: it draws from the same counter, so four workers cannot
/// turn one message into an unbounded tree.
#[derive(Debug)]
pub struct Allowance {
    left: AtomicU32,
}

impl Allowance {
    pub fn new(calls: u32) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Allowance {
            left: AtomicU32::new(calls),
        })
    }

    /// One model call. False when the tree has spent the whole allowance; the caller returns a fault
    /// rather than asking again, because the bound exists to end the tree, not to slow it down.
    pub fn take(&self) -> bool {
        self.left
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok()
    }

    pub fn left(&self) -> u32 {
        self.left.load(Ordering::SeqCst)
    }
}

/// What a chat can reach: where its shell and files live, where its memory is, and where skills are read
/// from. Passed explicitly so a test never touches the machine's real vault.
pub struct World {
    pub root: PathBuf,
    pub vault: PathBuf,
    pub db: PathBuf,
    pub skills: Vec<PathBuf>,
    /// Where past sessions are read from: the daemon's jsonl logs and the gateway's chat records.
    pub sessions: Vec<PathBuf>,
    /// The state directory: the browser profile and screenshots live under it.
    pub state: PathBuf,
    /// The chat a scheduled job reports to. Zero when there is no chat, as in a command-line probe.
    pub chat: i64,
    /// 0 for the person's conversation, 1 for a worker it delegated to. A worker cannot delegate again,
    /// which keeps one message from becoming an unbounded tree of model calls.
    pub depth: u8,
    /// Read or act. A read surface is not handed the tools that change anything, so the rule is a missing
    /// tool rather than a sentence the model can ignore.
    pub mode: crate::mode::Mode,
    /// What this surface may do. A chat is a patch surface; a delegated verifier is read-only; a memory
    /// curator holds no code tool at all.
    pub role: crate::profile::Role,
    /// Who is speaking in the ledger: `chat:<id>` or `job:<id>`. A separate field from `chat`, because a
    /// job's report chat and the job's identity are different facts.
    pub actor: String,
    /// The MCP servers this surface may reach, shared by the chats and workers a gateway spawns so a
    /// server is started once rather than per message.
    pub mcp: std::sync::Arc<crate::mcp::Mcp>,
    /// The model calls this conversation and its workers may spend, counted once for the whole tree.
    pub allowance: std::sync::Arc<Allowance>,
}

impl World {
    fn memory(&self) -> Result<MnemeMemory, String> {
        MnemeMemory::open(&self.vault, &self.db)
    }

    /// The path policy the file tools answer to: `root` is the only read root and the only write root.
    ///
    /// The state directory is deliberately not a root of any kind: the browser and Mneme own their own
    /// paths under it, and no file tool reaches them.
    pub fn policy(&self) -> crate::path::PathPolicy {
        crate::path::PathPolicy::for_root(&self.root)
    }
}

/// When a tool starts and when its result is in. The surface shows the first and hides the second, the
/// way a person sees "running a command" and not the exit plumbing.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    Start,
    End,
}

/// The tools a chat session has. Deliberately the same verbs a Hermes session starts with: shell, files,
/// search, the web. More arrive as they are wired, not as they are wished for.
///
/// `mode` is the gate a scheduled job stands behind. A read surface is built without the tools that
/// change anything, so "do not write" is a missing tool rather than an instruction.
pub fn schemas(depth: u8, mode: crate::mode::Mode) -> Value {
    schemas_for(depth, mode, crate::profile::Role::Patch)
}

/// The tool list a role may hold. A read-only role loses every mutating tool, a record role loses code and
/// shell entirely, and `execute` refuses the same names again for a call that arrives anyway.
pub fn schemas_for(depth: u8, mode: crate::mode::Mode, role: crate::profile::Role) -> Value {
    let mut list = json!([
        {
            "type": "function",
            "function": {
                "name": "shell",
                "description": "Run a shell command in the workspace and return its combined output. Use it for anything the other tools do not cover.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": {"type": "string", "description": "The command to run with sh -c."},
                        "timeout_s": {"type": "integer", "description": "Seconds before the command is killed. Default 120."}
                    },
                    "required": ["command"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read a text file. Paths are relative to the workspace unless absolute.",
                "parameters": {
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "write_file",
                "description": "Write a text file, creating parent directories. Overwrites.",
                "parameters": {
                    "type": "object",
                    "properties": {"path": {"type": "string"}, "content": {"type": "string"}},
                    "required": ["path", "content"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "patch",
                "description": "Replace one exact occurrence of `old` with `new`. `edits` applies in order; each anchor must appear exactly once or the whole patch is refused and nothing is written. Cheaper and safer than rewriting the whole file with write_file.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "edits": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "old": {"type": "string", "description": "The exact text to find, once."},
                                    "new": {"type": "string", "description": "What replaces it."}
                                },
                                "required": ["old", "new"]
                            }
                        }
                    },
                    "required": ["path", "edits"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "list_dir",
                "description": "List a directory, one name per line, directories with a trailing slash.",
                "parameters": {
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": []
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "search",
                "description": "Search file contents with a regular expression, like ripgrep.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": {"type": "string"},
                        "path": {"type": "string", "description": "Where to search. Default the workspace."}
                    },
                    "required": ["pattern"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "image",
                "description": "Look at an image file on this machine and answer a question about it. Use it for screenshots the browser saved, for a photo the person sent, or any picture whose content matters.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "question": {"type": "string", "description": "What to look for or answer. Default: describe the image."}
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "browser",
                "description": "Drive a real browser as an observe-act loop. `observe` returns the page title, URL, headings, readable text, and interactive elements with refs from the accessibility tree. `click`, `type` and `press` return a fresh observation after the action, so you can see what changed. `wait` waits for text, a URL fragment or a selector with a deadline. `tabs` lists tabs; `new_tab`, `tab` and `close_tab` manage them, including popups the click opened. `frames` and `frame` enter same-origin or cross-origin frames. `network`, `cookies` and `storage` expose the session. Use `screenshot` when text is not enough, and `eval` only as an escape hatch.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["observe", "open", "click", "type", "press", "wait", "screenshot", "eval", "tabs", "new_tab", "tab", "close_tab", "contexts", "new_context", "close_context", "frames", "frame", "downloads", "upload", "network", "network_body", "block", "unblock", "cookies", "cookie_set", "cookie_delete", "cookie_clear", "storage", "storage_set", "storage_clear", "close"]},
                        "url": {"type": "string", "description": "For open, wait, cookie_set or cookie_delete."},
                        "ref": {"type": "string", "description": "A handle from the latest observation, for example `e3 search`. Prefer this over a CSS selector."},
                        "selector": {"type": "string", "description": "A CSS selector when no stable ref exists."},
                        "text": {"type": "string", "description": "For click: visible text. For type: the text to enter. For wait: text the page must contain."},
                        "key": {"type": "string", "description": "For press: Enter, Tab, Escape, Backspace, ArrowDown or ArrowUp."},
                        "script": {"type": "string", "description": "For eval: a JavaScript expression."},
                        "path": {"type": "string", "description": "For screenshot or upload; for cookies, the cookie path."},
                        "timeout_s": {"type": "integer", "description": "For wait: seconds before giving up. Default 20, max 60."},
                        "text_limit": {"type": "integer", "description": "For observe: characters of page text to return. Default 6000, max 50000."},
                        "item_limit": {"type": "integer", "description": "For observe: interactive items to return. Default 60, max 500."},
                        "full": {"type": "boolean", "description": "For eval or network_body: return up to 100000 characters instead of 6000."},
                        "index": {"type": "integer", "description": "For tab, close_tab or frame: the number from `tabs` or `frames`. Defaults to the current tab for close_tab."},
                        "frame": {"type": "string", "description": "For frame: a frame id from `frames`, or `main`."},
                        "context": {"type": "string", "description": "For new_tab: a browser context id. For close_context: the context id."},
                        "id": {"type": "string", "description": "For network_body: the request id from `network`."},
                        "name": {"type": "string", "description": "For cookie_set or cookie_delete: the cookie name."},
                        "value": {"type": "string", "description": "For cookie_set or storage_set: the value."},
                        "domain": {"type": "string", "description": "For cookie_set or cookie_delete: the cookie domain."},
                        "secure": {"type": "boolean", "description": "For cookie_set: mark the cookie secure."},
                        "http_only": {"type": "boolean", "description": "For cookie_set: mark the cookie httpOnly."},
                        "same_site": {"type": "string", "description": "For cookie_set: Strict, Lax or None."},
                        "key": {"type": "string", "description": "For storage_set: the key."},
                        "kind": {"type": "string", "description": "For storage actions: local, session or both."}
                    },
                    "required": ["action"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "fetch",
                "description": "Fetch a URL and return it as text (HTML is stripped coarsely).",
                "parameters": {
                    "type": "object",
                    "properties": {"url": {"type": "string"}},
                    "required": ["url"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "recall",
                "description": "Search Hades' own memory vault for notes about a topic. Use it before answering questions about past work, decisions or the person's projects.",
                "parameters": {
                    "type": "object",
                    "properties": {"query": {"type": "string"}},
                    "required": ["query"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "remember",
                "description": "Write a durable note into Hades' memory vault, in the inbox for review. Use it for decisions, findings and anything that will matter in a later session. When the note corrects an earlier one, name the old note's path in `supersedes` so the record keeps one current truth and the old note becomes history.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "text": {"type": "string", "description": "The note body."},
                        "title": {"type": "string", "description": "One short line."},
                        "supersedes": {"type": "string", "description": "For a correction: the path of the note this replaces, exactly as recall showed it (for example `00 Inbox/2026-10-18-port.md`). The old note is kept and marked as history."}
                    },
                    "required": ["text"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "session_search",
                "description": "Search earlier Hades sessions for what was said or decided. Use it when the person refers to a past conversation you do not have in context.",
                "parameters": {
                    "type": "object",
                    "properties": {"query": {"type": "string"}},
                    "required": ["query"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "skills",
                "description": "List the skills available, or read one by name. A skill is a procedure worth following; read it before doing a task it covers.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["list", "read"]},
                        "name": {"type": "string"}
                    },
                    "required": ["action"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "web_search",
                "description": "Search the web through the local SearXNG instance and return titles, links and snippets.",
                "parameters": {
                    "type": "object",
                    "properties": {"query": {"type": "string"}},
                    "required": ["query"]
                }
            }
        }
    ]);
    if mode.acts() {
        list.as_array_mut().unwrap().push(json!({
            "type": "function",
            "function": {
                "name": "mcp",
                "description": "Reach an MCP server this config names. `list` shows the servers, `tools` shows what one offers, and `call` runs one tool. Only tools on the person's allowlist can run. An MCP call is opaque to the hard floor, so a refusal is the allowlist deciding, not a guess about what the tool does.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["list", "tools", "call"]},
                        "server": {"type": "string", "description": "The configured server's name."},
                        "tool": {"type": "string", "description": "For call: the tool name the server advertises."},
                        "arguments": {"type": "object", "description": "For call: the arguments object the tool takes.", "additionalProperties": true}
                    },
                    "required": ["action"]
                }
            }
        }));
    }
    if depth == 0 && mode.acts() {
        list.as_array_mut().unwrap().push(json!({
            "type": "function",
            "function": {
                "name": "cron",
                "description": "Your scheduled jobs, each a standing goal file under the state directory. The body is the goal; the frontmatter says what it reads, what the answer is, when it is done, when to stop, and what it must never do. A job runs on its own and reports into the chat where it was created. Edit the file to correct a job that drifts. Schedules are `every 30m` / `every 6h` or `daily 07:30`. A job is `read` (the default: no shell, no writes) unless you ask for `act`.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["list", "add", "remove"]},
                        "schedule": {"type": "string", "description": "For add: `every 30m`, `every 6h`, `daily 07:30`."},
                        "goal": {"type": "string", "description": "For add: the standing goal, in full. The job cannot see this conversation."},
                        "inputs": {"type": "string", "description": "For add: what the job reads, one line."},
                        "outputs": {"type": "string", "description": "For add: what the answer should be, one line."},
                        "done_when": {"type": "string", "description": "For add: the condition that says the job did its work."},
                        "stop_when": {"type": "string", "description": "For add: when to stop looking, so it does not wander."},
                        "never": {"type": "string", "description": "For add: what it must not do, even in act mode."},
                        "mode": {"type": "string", "enum": ["read", "act"], "description": "For add: `read` (default) cannot change anything; `act` gets the full toolset and the hard floor still applies."},
                        "id": {"type": "integer", "description": "For remove: the job number."}
                    },
                    "required": ["action"]
                }
            }
        }));
    }
    if depth == 0 {
        list.as_array_mut().unwrap().push(json!({
            "type": "function",
            "function": {
                "name": "delegate",
                "description": "Hand one or more independent tasks to workers that each run their own loop with the same tools, in parallel, and report back. Use it when a job splits into parts that do not need each other, or when a search would flood this conversation. Give each task everything the worker needs: it cannot see this conversation.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "tasks": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "goal": {"type": "string", "description": "One self-contained instruction. The worker cannot see this conversation."},
                                    "result_schema": {"type": "object", "description": "A JSON Schema the worker's final answer must satisfy. Omit it and the result is {\"report\": \"<prose>\"}."}
                                },
                                "required": ["goal"]
                            },
                            "description": "One task per worker, at most four. A string is accepted as a goal with the default report envelope."
                        }
                    },
                    "required": ["tasks"]
                }
            }
        }));
    }
    if depth < MAX_SPAWN_DEPTH {
        list.as_array_mut().unwrap().push(json!({
            "type": "function",
            "function": {
                "name": "spawn",
                "description": "Create one child graph: a worker with its own loop and session that must return one JSON object matching a schema. Use it for a sub-goal that deserves its own investigation and a typed answer. The child can spawn one level further; the depth and the model-call allowance are both shared. The child returns to you, not to the person's conversation.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "goal": {"type": "string", "description": "One self-contained sub-goal. The child cannot see this conversation."},
                        "result_schema": {"type": "object", "description": "JSON Schema the child's answer must satisfy. Omit it and the result is {\"report\": \"<prose>\"}."},
                        "steps": {"type": "integer", "description": "Model calls this child may make, 2 to 12. Default 8."}
                    },
                    "required": ["goal"]
                }
            }
        }));
    }
    // A read surface is handed no tool that changes anything. Filtering here, after the list is built,
    // keeps one definition of "mutating" for the schema and the check in `execute`.
    if !mode.acts() {
        list.as_array_mut().unwrap().retain(|tool| {
            tool.pointer("/function/name")
                .and_then(Value::as_str)
                .is_some_and(|name| !mutating(name))
        });
    }
    list.as_array_mut().unwrap().retain(|tool| {
        tool.pointer("/function/name")
            .and_then(Value::as_str)
            .is_some_and(|name| role.allows(name))
    });
    list
}

/// How deep a graph of loops may nest. Zero is the person's conversation, one is a delegated worker, two
/// is a worker a worker spawned. Three levels is the ceiling: enough for decomposition, not a fork bomb.
const MAX_SPAWN_DEPTH: u8 = 2;

/// The default result when a caller does not name a schema: one prose field, but still an object.
fn report_envelope() -> Value {
    json!({
        "type": "object",
        "properties": {"report": {"type": "string"}},
        "required": ["report"]
    })
}

/// The tools the act surface can call. Kept beside `mutating` and tied to the schema by a test, because
/// a ledger line that says "allowed" for a tool that does not exist is worse than no line.
const TOOLS: [&str; 18] = [
    "shell",
    "read_file",
    "write_file",
    "patch",
    "list_dir",
    "search",
    "image",
    "browser",
    "fetch",
    "recall",
    "remember",
    "session_search",
    "skills",
    "web_search",
    "cron",
    "mcp",
    "delegate",
    "spawn",
];

/// Record one decided action. Best effort: a ledger write that fails is reported, and the action it was
/// about proceeds. The ledger is evidence, not a gate.
fn decided(world: &World, layer: &str, action: &str, decision: &str, reason: &str, artifact: &str) {
    let line = crate::activity::Line {
        at: crate::agent::epoch(),
        actor: world.actor.clone(),
        layer: layer.to_string(),
        action: action.to_string(),
        decision: decision.to_string(),
        reason: reason.to_string(),
        artifact: artifact.to_string(),
    };
    if let Err(said) = crate::activity::record(&world.state, &line) {
        eprintln!("activity: could not record {action}: {said}");
    }
}

fn epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

/// The tools that change something on this machine or in the world. One list, because a schema that
/// omits a name and a check that refuses it must never disagree.
fn mutating(name: &str) -> bool {
    matches!(
        name,
        "shell" | "write_file" | "patch" | "remember" | "cron" | "browser" | "mcp" | "spawn"
    )
}

/// The loop. Returns the final prose. `steps` is a safety bound, not a budget the person ever sees.
pub fn answer(
    client: &OpenAi,
    messages: &mut Vec<Value>,
    world: &World,
    feed: FeedFn<'_>,
    activity: &dyn Fn(Activity, &str),
    steps: usize,
) -> Result<String, Fault> {
    answer_stoppable(client, messages, world, feed, activity, steps, None)?
        .ok_or_else(|| Fault::model("stopped by a person".to_string()))
}

/// The same loop, with a stop a surface can raise while it runs. `None` means a stop was asked for: the
/// answer was not finished, and no tool after the stop was executed. The boundary is between steps and
/// before each tool call, so a model call already in flight finishes first. That is a limit, not a promise.
pub fn answer_stoppable(
    client: &OpenAi,
    messages: &mut Vec<Value>,
    world: &World,
    feed: FeedFn<'_>,
    activity: &dyn Fn(Activity, &str),
    steps: usize,
    stop: Option<&AtomicBool>,
) -> Result<Option<String>, Fault> {
    let stopped = || stop.is_some_and(|flag| flag.load(Ordering::SeqCst));
    let tools = schemas_for(world.depth, world.mode, world.role);
    // The repetition guard: the third identical call in one answer is not a new attempt, it is a loop. The
    // model gets a refusal instead of a fourth result, and the ledger says why. Keyed on the exact name and
    // arguments, so two similar reads are not conflated.
    let mut repeats: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    for _ in 0..steps {
        if stopped() {
            return Ok(None);
        }
        if !world.allowance.take() {
            return Err(Fault::model(
                "the shared model-call allowance is spent; the tree is bounded".to_string(),
            ));
        }
        let reply = client.chat(messages, Some(&tools), feed)?;
        if reply.calls.is_empty() {
            let text = reply.content.trim().to_string();
            if text.is_empty() {
                return Err(Fault::model("the model answered with nothing".to_string()));
            }
            return Ok(Some(text));
        }
        let calls: Vec<Value> = reply
            .calls
            .iter()
            .map(|call| {
                json!({
                    "id": call.id,
                    "type": "function",
                    "function": {"name": call.name, "arguments": call.arguments}
                })
            })
            .collect();
        messages.push(json!({
            "role": "assistant",
            "content": reply.content,
            "tool_calls": calls
        }));
        for call in &reply.calls {
            // The side effect boundary. A stop that arrived while the model was writing this call means the
            // call is not made: the person asked for the work to end, not for one more step of it.
            if stopped() {
                return Ok(None);
            }
            let label = label(&call.name, &call.arguments);
            activity(Activity::Start, &label);
            let key = format!("{}:{}", call.name, call.arguments);
            let count = repeats.entry(key).and_modify(|n| *n += 1).or_insert(1);
            if repeats_refuse(*count) {
                let said = format!(
                    "Refused: `{}` with exactly these arguments has run {} times in this answer. Say what you learned, or change the call.",
                    call.name, *count
                );
                decided(
                    world,
                    "repeat",
                    &label,
                    "refuse",
                    "the same call repeated three times",
                    "",
                );
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call.id,
                    "content": said
                }));
                activity(Activity::End, &label);
                continue;
            }
            let result = execute(&call.name, &call.arguments, world, client, activity);
            messages.push(json!({
                "role": "tool",
                "tool_call_id": call.id,
                "content": result
            }));
            activity(Activity::End, &label);
        }
    }
    Err(Fault::model(format!(
        "stopped after {steps} steps without an answer"
    )))
}

fn label(name: &str, arguments: &str) -> String {
    let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    let detail = match name {
        "shell" => args.get("command").and_then(Value::as_str).unwrap_or(""),
        "read_file" | "write_file" | "patch" | "list_dir" => {
            args.get("path").and_then(Value::as_str).unwrap_or("")
        }
        "search" => args.get("pattern").and_then(Value::as_str).unwrap_or(""),
        "fetch" => args.get("url").and_then(Value::as_str).unwrap_or(""),
        "web_search" => args.get("query").and_then(Value::as_str).unwrap_or(""),
        "recall" => args.get("query").and_then(Value::as_str).unwrap_or(""),
        "remember" => args
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("a note"),
        "skills" => args.get("name").and_then(Value::as_str).unwrap_or(""),
        "session_search" => args.get("query").and_then(Value::as_str).unwrap_or(""),
        "cron" => args
            .get("task")
            .and_then(Value::as_str)
            .unwrap_or(args.get("action").and_then(Value::as_str).unwrap_or("jobs")),
        "delegate" => args
            .get("tasks")
            .and_then(Value::as_array)
            .and_then(|tasks| tasks.first())
            .and_then(Value::as_str)
            .unwrap_or("work"),
        "spawn" => args
            .get("goal")
            .and_then(Value::as_str)
            .unwrap_or("a sub-goal"),
        "browser" => ["url", "ref", "selector", "text", "action"]
            .iter()
            .find_map(|key| args.get(*key).and_then(Value::as_str))
            .unwrap_or(""),
        _ => "",
    };
    let one_line = detail.lines().next().unwrap_or("").trim();
    if one_line.is_empty() {
        name.to_string()
    } else {
        format!("{name}: {}", clip(one_line, 70))
    }
}

/// The same exact call is refused on its third occurrence in one answer.
const REPEATS: u32 = 3;

fn repeats_refuse(count: u32) -> bool {
    count >= REPEATS
}

/// The person's pre-tool hooks, from `<state>/hooks.json`: `{"pre_tool": ["test -f .hades-allow"]}`.
///
/// A hook can only refuse. It runs before the tool, sees `HADES_TOOL` and `HADES_ACTION`, and any non-zero
/// exit blocks the call even when the policy allowed it. The file is read per call, so correcting a hook
/// takes effect on the next tool call rather than at the next restart. A hook that cannot start is a
/// refusal, not a silent pass.
fn pre_tool_hook(world: &World, name: &str, args: &Value) -> Option<String> {
    let text = std::fs::read_to_string(world.state.join("hooks.json")).ok()?;
    let config: Value = serde_json::from_str(&text).ok()?;
    let hooks = config.get("pre_tool")?.as_array()?;
    let action = crate::activity::action(name, args);
    for hook in hooks.iter().filter_map(Value::as_str) {
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(hook)
            .current_dir(&world.root)
            .env("HADES_TOOL", name)
            .env("HADES_ACTION", &action)
            .output();
        match output {
            Ok(output) if output.status.success() => {}
            Ok(output) => {
                let said = String::from_utf8_lossy(&output.stderr).trim().to_string();
                let code = output.status.code().unwrap_or(-1);
                return Some(if said.is_empty() {
                    format!("`{hook}` exited {code}")
                } else {
                    format!("`{hook}` exited {code}: {said}")
                });
            }
            Err(error) => return Some(format!("`{hook}` could not start: {error}")),
        }
    }
    None
}

fn execute(
    name: &str,
    arguments: &str,
    world: &World,
    client: &OpenAi,
    activity: &dyn Fn(Activity, &str),
) -> String {
    let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    let what = crate::activity::action(name, &args);
    let artifact = crate::activity::artifact(&args);
    // A read surface has no mutating tool in its schema; this is the second half of the same rule, for a
    // call that arrives anyway (a stale transcript, or a worker built before the mode was set).
    if !world.mode.acts() && mutating(name) {
        decided(
            world,
            "mode",
            &what,
            "refuse",
            "a read surface has no tool that changes anything",
            &artifact,
        );
        return format!(
            "this job is read-only, so it has no {name} tool. Read what you can and answer with what you found."
        );
    }
    // The role is the second gate: a verifier is read-only even in act mode, and a record profile holds
    // memory tools only. The schema already omits these; this catches a stale transcript or a direct call.
    if !world.role.allows(name) {
        decided(
            world,
            "role",
            &what,
            "refuse",
            &format!("the {} role does not hold this tool", world.role.name()),
            &artifact,
        );
        return format!(
            "Refused: a {} profile does not hold `{name}`. Use the tools it has, or hand the work to a profile that does.",
            world.role.name()
        );
    }
    // The hard floor, before the tool does anything. A chat has no approval surface, so the refusal
    // goes back to the model as the result: it says what was refused, and that a person decides.
    if let Some(floor) = crate::floor::check(name, &args, &world.root) {
        decided(
            world,
            "floor",
            &what,
            "refuse",
            floor.category().phrase(),
            &artifact,
        );
        return floor.refusal();
    }
    if !TOOLS.contains(&name) {
        decided(world, "surface", &what, "refuse", "no such tool", &artifact);
        return format!("there is no tool called {name}");
    }
    // The MCP allowlist is a fourth gate, before the ledger says allow. An MCP call is opaque to the
    // floor, so the config list is the decision, and a call it does not name is a refusal rather than a
    // line that claims the surface allowed something it did not.
    if name == "mcp"
        && let Err(said) = crate::mcp::gate(&args, &world.mcp)
    {
        decided(world, "allowlist", &what, "refuse", &said, &artifact);
        return said;
    }
    // A hook is the person's own command, run before the tool. It cannot widen a permission: its only power
    // is to say no, which is what makes it safe to add without re-reading the policy. A non-zero exit blocks
    // even a call the policy allowed.
    if let Some(said) = pre_tool_hook(world, name, &args) {
        decided(world, "hook", &what, "refuse", &said, &artifact);
        return format!("Refused: {said}. A pre-tool hook stopped this call.");
    }
    decided(world, "surface", &what, "allow", "", &artifact);
    match name {
        "shell" => shell(&args, world),
        "read_file" => read_file(&args, world),
        "write_file" => write_file(&args, world),
        "patch" => crate::patch::tool(&args, &world.policy()),
        "list_dir" => list_dir(&args, world),
        "search" => search(&args, world),
        "recall" => recall(&args, world),
        "remember" => remember(&args, world),
        "skills" => skills(&args, world),
        "session_search" => session_search(&args, world),
        "browser" => crate::browser::tool(&args, &world.state),
        "image" => image(&args, world),
        "fetch" => fetch(&args),
        "web_search" => web_search(&args),
        "delegate" => delegate(&args, world, client, activity),
        "spawn" => spawn(&args, world, client, activity),
        "cron" => crate::agent::cron(&args, world),
        "mcp" => world
            .mcp
            .run(&args)
            .unwrap_or_else(|said| format!("MCP: {said}")),
        other => format!("there is no tool called {other}"),
    }
}

/// One worker per task, in parallel, each with its own conversation and the same tools. A worker cannot
/// delegate, so the tree is one level deep by construction rather than by a counter that might be missed.
const WORKER: &str = "You are one worker in a team. You have tools: shell, files, search, the web, memory, skills, \
sessions and images. Do the task with them instead of asking about it. You cannot see the conversation that \
sent you this task, so treat the task text as everything you know. Answer with a short report: what you did, \
what you found, and exact paths or numbers. Never ask a question back.";

/// One delegated task: a goal and the schema its answer must satisfy. A string task gets the default
/// envelope, so every delegation returns a checked object rather than a paragraph the parent has to parse.
struct Job {
    goal: String,
    schema: Value,
    typed: bool,
}

fn jobs(args: &Value) -> Vec<Job> {
    let envelope = report_envelope();
    args.get("tasks")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| match item {
                    Value::String(text) if !text.trim().is_empty() => Some(Job {
                        goal: text.trim().to_string(),
                        schema: envelope.clone(),
                        typed: false,
                    }),
                    Value::Object(map) => {
                        let goal = map.get("goal").and_then(Value::as_str)?.trim();
                        if goal.is_empty() {
                            return None;
                        }
                        let given = map.get("result_schema").filter(|schema| schema.is_object());
                        Some(Job {
                            goal: goal.to_string(),
                            schema: given.cloned().unwrap_or_else(|| envelope.clone()),
                            typed: given.is_some(),
                        })
                    }
                    _ => None,
                })
                .take(4)
                .collect()
        })
        .unwrap_or_default()
}

/// The object a worker meant to return. A fenced block or a sentence before the JSON is tolerated,
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

/// A small structural check, not a JSON Schema engine: object, array, string, number, integer, boolean,
/// null, required keys and declared properties. That is the shape a delegation contract needs, and a
/// bigger engine would be a dependency for a claim this one can test.
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

/// One child graph, from the parent's point of view: its own session, its own tools, and one checked object
/// at the end. Delegation and spawn share this, so a child means the same thing wherever it was created.
#[allow(clippy::too_many_arguments)]
fn run_child(
    name: &str,
    goal: &str,
    schema: &Value,
    typed: bool,
    worker: &World,
    client: &OpenAi,
    kind: &str,
    steps: usize,
) -> Result<(String, Option<Value>, String), String> {
    let store = session_tree::FileStore::new(worker.state.join("sessions"));
    let id = session_tree::SessionId::new(name);
    let mut log = session_tree::Session::create(store, session_tree::SystemClock, id.clone())
        .map_err(|fault| format!("{fault:?}"))?;
    let _ = log.append(
        session_tree::Kind::Meta,
        json!({"actor": kind, "parent": worker.actor, "role": worker.role.name(), "goal": goal, "schema": schema}),
    );
    let _ = log.append(session_tree::Kind::User, json!({"text": goal}));
    let system = format!(
        "{WORKER}\n\nReturn one JSON object and nothing else. It must satisfy this schema:\n{}\n\
         Do not put prose around it. Do not use Markdown fences. A missing or mistyped field fails \
         the task.",
        serde_json::to_string_pretty(schema).unwrap_or_else(|_| "{}".to_string())
    );
    let mut messages = vec![
        json!({"role": "system", "content": system}),
        json!({"role": "user", "content": goal}),
    ];
    let silent = |_: Feed, _: &str| {};
    let nothing = |_: Activity, _: &str| {};
    let answer = answer(client, &mut messages, worker, &silent, &nothing, steps);
    let (status, result, raw) = match answer {
        Ok(text) => {
            let value = if typed {
                parse_json_answer(&text)
            } else {
                Some(json!({"report": text}))
            };
            match value {
                Some(value) => match fits(schema, &value) {
                    Ok(()) => ("ok".to_string(), Some(value), text),
                    Err(why) => ("invalid".to_string(), None, format!("{why}\n{text}")),
                },
                None => (
                    "invalid".to_string(),
                    None,
                    format!("the child did not return a JSON object\n{text}"),
                ),
            }
        }
        Err(fault) => ("failed".to_string(), None, format!("{fault:?}")),
    };
    let _ = log.append(
        session_tree::Kind::Assistant,
        json!({"text": raw, "status": status, "result": result}),
    );
    let dir = worker.state.join("artifacts").join(name);
    if std::fs::create_dir_all(&dir).is_ok() {
        let bundle = format!(
            "---\nid: {name}\nstatus: {status}\nrole: {}\nsession: {name}\nparent: {}\n---\n\n# {}\n\n{}\n",
            worker.role.name(),
            worker.actor,
            goal.trim(),
            match &result {
                Some(value) =>
                    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()),
                None => raw.clone(),
            }
        );
        let _ = std::fs::write(dir.join("report.md"), bundle);
        let record = json!({"status": status, "result": result, "raw": raw});
        let _ = std::fs::write(
            dir.join("result.json"),
            serde_json::to_string_pretty(&record).unwrap_or_default(),
        );
    }
    Ok((status, result, raw))
}

fn delegate(
    args: &Value,
    world: &World,
    client: &OpenAi,
    activity: &dyn Fn(Activity, &str),
) -> String {
    if world.depth > 0 {
        return "a worker cannot delegate again".to_string();
    }
    let jobs = jobs(args);
    if jobs.is_empty() {
        return "no tasks given".to_string();
    }
    let needed = jobs.len() as u32 * 2;
    if world.allowance.left() < needed {
        return format!(
            "the shared model-call allowance has {} call(s) left; {} worker(s) need at least {needed}",
            world.allowance.left(),
            jobs.len()
        );
    }

    let mut handles = Vec::new();
    for (index, job) in jobs.into_iter().enumerate() {
        let worker = World {
            root: world.root.clone(),
            vault: world.vault.clone(),
            db: world.db.clone(),
            skills: world.skills.clone(),
            sessions: world.sessions.clone(),
            state: world.state.clone(),
            chat: world.chat,
            depth: world.depth + 1,
            mode: world.mode,
            role: world.role,
            actor: world.actor.clone(),
            mcp: std::sync::Arc::clone(&world.mcp),
            allowance: std::sync::Arc::clone(&world.allowance),
        };
        let client = client.clone();
        activity(
            Activity::Start,
            &format!("delegate: {}", clip(&job.goal, 60)),
        );
        let started = job.goal.clone();
        let schema = job.schema.clone();
        let typed = job.typed;
        // A child gets its own log and its own bundle. The parent's context gets a handle and a checked
        // result, not the worker's whole transcript.
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_secs())
            .unwrap_or(0);
        let child = format!("deleg-{stamp}-{index}-{}", mneme::slugify(&started, 4));
        let name = child.clone();
        handles.push((
            started.clone(),
            child,
            std::thread::spawn(move || {
                run_child(
                    &name, &started, &schema, typed, &worker, &client, "delegate", 12,
                )
            }),
        ));
    }
    let mut out = String::new();
    for (task, child, handle) in handles {
        let report = handle
            .join()
            .unwrap_or_else(|_| Err("the worker stopped".to_string()));
        let artifact = world
            .state
            .join("artifacts")
            .join(&child)
            .join("result.json");
        out.push_str(&format!(
            "--- worker: {} ---\nsession: {child}\nartifact: {}\n",
            clip(&task, 60),
            artifact.display()
        ));
        match report {
            Ok((status, Some(result), _)) => out.push_str(&format!(
                "status: {status}\nresult: {}\n",
                serde_json::to_string(&result).unwrap_or_else(|_| "null".to_string())
            )),
            Ok((status, None, raw)) => out.push_str(&format!("status: {status}\n{raw}\n")),
            Err(error) => out.push_str(&format!("status: failed\n{error}\n")),
        }
    }
    out.trim().to_string()
}

/// A recursive node: one child graph, run to completion, returned to the caller that spawned it. The child
/// gets a depth, a slice of the shared allowance and a typed contract. It returns to this parent only; it
/// cannot reach the root, and the root learns of it through this result.
fn spawn(
    args: &Value,
    world: &World,
    client: &OpenAi,
    activity: &dyn Fn(Activity, &str),
) -> String {
    if world.depth >= MAX_SPAWN_DEPTH {
        return format!(
            "the spawn depth is spent: a graph at depth {} cannot replicate",
            world.depth
        );
    }
    let goal = match argument(args, "goal") {
        Some(goal) => goal,
        None => return "spawn needs a goal".to_string(),
    };
    let given = args
        .get("result_schema")
        .filter(|schema| schema.is_object());
    let typed = given.is_some();
    let schema = given.cloned().unwrap_or_else(report_envelope);
    let steps = args
        .get("steps")
        .and_then(Value::as_u64)
        .unwrap_or(8)
        .clamp(2, 12) as usize;
    if world.allowance.left() < 2 {
        return format!(
            "the shared model-call allowance has {} call(s) left; a child needs at least 2",
            world.allowance.left()
        );
    }
    let worker = World {
        root: world.root.clone(),
        vault: world.vault.clone(),
        db: world.db.clone(),
        skills: world.skills.clone(),
        sessions: world.sessions.clone(),
        state: world.state.clone(),
        chat: world.chat,
        depth: world.depth + 1,
        mode: world.mode,
        role: world.role,
        actor: world.actor.clone(),
        mcp: std::sync::Arc::clone(&world.mcp),
        allowance: std::sync::Arc::clone(&world.allowance),
    };
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0);
    let name = format!("spawn-{stamp}-{}", mneme::slugify(&goal, 4));
    activity(Activity::Start, &format!("spawn: {}", clip(&goal, 60)));
    let artifact = world
        .state
        .join("artifacts")
        .join(&name)
        .join("result.json");
    match run_child(
        &name, &goal, &schema, typed, &worker, client, "spawn", steps,
    ) {
        Ok((status, Some(result), _)) => format!(
            "session: {name}\nartifact: {}\nstatus: {status}\nresult: {}\n",
            artifact.display(),
            serde_json::to_string(&result).unwrap_or_else(|_| "null".to_string())
        ),
        Ok((status, None, raw)) => format!(
            "session: {name}\nartifact: {}\nstatus: {status}\n{raw}\n",
            artifact.display()
        ),
        Err(error) => format!("session: {name}\nstatus: failed\n{error}\n"),
    }
}

/// Jobs live in `<state>/jobs`, one file each, and are run by the gateway's scheduler, not here: this
/// function writes and removes those files, so a job created from a chat is the same file the scheduler
/// reads on its next tick and the same file a person corrects when the job drifts.
fn cron(args: &Value, world: &World) -> String {
    let state = &world.state;
    let (jobs, problems) = crate::cron::load(state);
    match args.get("action").and_then(Value::as_str).unwrap_or("list") {
        "add" => {
            let schedule = argument(args, "schedule")
                .or_else(|| argument(args, "when"))
                .unwrap_or_default();
            let goal = argument(args, "goal")
                .or_else(|| argument(args, "task"))
                .unwrap_or_default();
            if !crate::cron::valid(&schedule) {
                return format!(
                    "`{schedule}` is not a schedule I can keep. Use `every 30m`, `every 6h` or `daily 07:30`."
                );
            }
            if goal.is_empty() {
                return "a job needs a goal: what should it do?".to_string();
            }
            if world.chat == 0 {
                return "there is no chat to report to, so a job would run silently".to_string();
            }
            let mode = match args.get("mode").and_then(Value::as_str) {
                None => crate::mode::Mode::Read,
                Some(text) => match crate::mode::Mode::parse(text) {
                    Some(mode) => mode,
                    None => return format!("`{text}` is not a mode: say read or act."),
                },
            };
            let job = crate::cron::Job {
                id: crate::cron::next_id(&jobs),
                chat: world.chat,
                schedule: schedule.clone(),
                mode,
                goal,
                inputs: argument(args, "inputs").unwrap_or_default(),
                outputs: argument(args, "outputs").unwrap_or_default(),
                done_when: argument(args, "done_when").unwrap_or_default(),
                stop_when: argument(args, "stop_when").unwrap_or_default(),
                until: match crate::cron::check_until(&argument(args, "until").unwrap_or_default())
                {
                    Ok(value) => value,
                    Err(said) => return said,
                },
                never: argument(args, "never").unwrap_or_default(),
                last: 0,
            };
            let id = job.id;
            match crate::cron::write(state, &job) {
                Ok(path) => format!(
                    "job {id} is now {} · runs {schedule} · {}. Edit that file to correct it.",
                    crate::cron::relative(state, &path),
                    match mode {
                        crate::mode::Mode::Read => "read-only",
                        crate::mode::Mode::Act => "may act, under the hard floor",
                    }
                ),
                Err(said) => format!("could not write the job file: {said}"),
            }
        }
        "remove" => {
            let Some(id) = args.get("id").and_then(Value::as_u64) else {
                return "which job? give its number".to_string();
            };
            match crate::cron::remove(state, id) {
                Ok(path) => format!("job {id} removed ({path})"),
                Err(said) => said,
            }
        }
        _ => {
            let mut lines: Vec<String> = jobs
                .iter()
                .map(|job| {
                    let path = crate::cron::path_of(state, job.id)
                        .map(|path| crate::cron::relative(state, &path))
                        .unwrap_or_default();
                    format!(
                        "{} · {} · {} · {} · {}",
                        job.id,
                        job.schedule,
                        job.mode.name(),
                        clip(job.goal.lines().next().unwrap_or(""), 80),
                        path
                    )
                })
                .collect();
            // A file that will not parse is not a job that runs; saying so is the only way the person
            // who edited it learns that the edit took the job out.
            for problem in problems {
                lines.push(format!("not run: {problem}"));
            }
            if lines.is_empty() {
                "no jobs".to_string()
            } else {
                lines.join("\n")
            }
        }
    }
}

/// One optional string from the arguments, trimmed, empty means absent.
fn argument(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

/// Run one command for the chat.
///
/// The command runs under a cage (see `Cage::chat`): the workspace and the state's scratch directory are
/// writable, the system and its language runtimes are readable, the network is open, and the home directory
/// is not readable. A chat is where a fetched page can talk to the model, so the read boundary is the
/// thing that keeps a page from asking for a key and getting it.
///
/// The timeout is the `timeout` command rather than a thread that forgets the child, so a hung program dies
/// with its shell and cannot outlive the message.
fn shell(args: &Value, world: &World) -> String {
    let command = args.get("command").and_then(Value::as_str).unwrap_or("");
    if command.trim().is_empty() {
        return "no command given".to_string();
    }
    let seconds = args
        .get("timeout_s")
        .and_then(Value::as_u64)
        .unwrap_or(120)
        .clamp(1, 600);
    let scratch = world.state.join("tmp");
    if let Err(error) = std::fs::create_dir_all(&scratch) {
        return format!("could not make the scratch directory: {error}");
    }
    let program = format!("timeout {seconds} sh -c {}", quote(command));
    let cage = crate::cage::Cage::chat_with(&world.root, &scratch, world.role.network());
    match crate::command::run(&program, &world.root, 6000, Some(&cage)) {
        Ok(run) => {
            let mut text = run.output;
            if text.trim().is_empty() {
                text = format!("exit {}\n(no output)", run.status);
            } else if run.status != 0 {
                text.push_str(&format!("\n[exit {}]", run.status));
            }
            text
        }
        Err(fault) => format!("could not run the command: {fault}"),
    }
}

/// Quote one argument for `sh`, so the program reaches the shell as one word. A single quote inside is
/// closed, escaped and reopened; that is the whole of the POSIX rule.
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// Where the remaining non-policy caller resolves: `image` (the vision pair reads only what the model
/// named). Every file tool answers to `World::policy` instead; this is not a gate, so nothing writes
/// through it.
fn resolve(root: &Path, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

fn read_file(args: &Value, world: &World) -> String {
    let policy = world.policy();
    let root: &Path = &world.root;
    let path = args.get("path").and_then(Value::as_str).unwrap_or("");
    if path.is_empty() {
        return "no path given".to_string();
    }
    let path = match policy.read(Path::new(path)) {
        Ok(path) => path,
        Err(reason) => return format!("Refused: {reason}."),
    };
    if let Some(reason) = crate::floor::protected(&path.to_string_lossy(), root) {
        return format!("Refused: {reason}. A chat does not read the live install.");
    }
    match std::fs::read_to_string(&path) {
        Ok(text) => clip(&text, 30000),
        Err(error) => format!("could not read {}: {error}", path.display()),
    }
}

fn write_file(args: &Value, world: &World) -> String {
    let policy = world.policy();
    let path = args.get("path").and_then(Value::as_str).unwrap_or("");
    let content = args.get("content").and_then(Value::as_str).unwrap_or("");
    if path.is_empty() {
        return "no path given".to_string();
    }
    let path = match policy.create(Path::new(path)) {
        Ok(path) => path,
        Err(reason) => return format!("Refused: {reason}."),
    };
    if let Some(parent) = path.parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        return format!("could not create {}: {error}", parent.display());
    }
    match std::fs::write(&path, content) {
        Ok(()) => format!("wrote {} ({} bytes)", path.display(), content.len()),
        Err(error) => format!("could not write {}: {error}", path.display()),
    }
}

fn list_dir(args: &Value, world: &World) -> String {
    let policy = world.policy();
    let root: &Path = &world.root;
    let path = args.get("path").and_then(Value::as_str).unwrap_or(".");
    let path = match policy.read(Path::new(path)) {
        Ok(path) => path,
        Err(reason) => return format!("Refused: {reason}."),
    };
    if let Some(reason) = crate::floor::protected(&path.to_string_lossy(), root) {
        return format!("Refused: {reason}. A chat does not list the live install.");
    }
    let entries = match std::fs::read_dir(&path) {
        Ok(entries) => entries,
        Err(error) => return format!("could not list {}: {error}", path.display()),
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            if entry.path().is_dir() {
                format!("{name}/")
            } else {
                name
            }
        })
        .collect();
    names.sort();
    if names.is_empty() {
        "(empty)".to_string()
    } else {
        clip(&names.join("\n"), 8000)
    }
}

fn search(args: &Value, world: &World) -> String {
    let policy = world.policy();
    let root: &Path = &world.root;
    let pattern = args.get("pattern").and_then(Value::as_str).unwrap_or("");
    if pattern.is_empty() {
        return "no pattern given".to_string();
    }
    let path = args
        .get("path")
        .and_then(Value::as_str)
        .map(|p| policy.read(Path::new(p)))
        .unwrap_or_else(|| policy.read(Path::new("")))
        .map_err(|reason| format!("Refused: {reason}."));
    let path = match path {
        Ok(path) => path,
        Err(said) => return said,
    };
    if let Some(reason) = crate::floor::protected(&path.to_string_lossy(), root) {
        return format!("Refused: {reason}. A chat does not search the live install.");
    }
    let rg = Command::new("rg")
        .arg("-n")
        .arg("--no-heading")
        .arg("-m")
        .arg("40")
        .arg(pattern)
        .arg(&path)
        .output();
    let output = match rg {
        Ok(output) if output.status.code() != Some(127) => output,
        _ => match Command::new("grep")
            .arg("-rn")
            .arg("-m")
            .arg("40")
            .arg("--")
            .arg(pattern)
            .arg(&path)
            .output()
        {
            Ok(output) => output,
            Err(error) => return format!("no search tool: {error}"),
        },
    };
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    if text.trim().is_empty() {
        "no matches".to_string()
    } else {
        clip(&text, 8000)
    }
}

fn fetch(args: &Value) -> String {
    let url = args.get("url").and_then(Value::as_str).unwrap_or("");
    if url.is_empty() {
        return "no url given".to_string();
    }
    match Command::new("curl")
        .arg("-sSL")
        .arg("--max-time")
        .arg("30")
        .arg("-A")
        .arg("Mozilla/5.0 (compatible; Hades/0.21)")
        .arg(url)
        .output()
    {
        Ok(output) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout).to_string();
            clip(&strip_html(&text), 12000)
        }
        Ok(output) => format!(
            "fetch failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Err(error) => format!("could not run curl: {error}"),
    }
}

fn web_search(args: &Value) -> String {
    let query = args.get("query").and_then(Value::as_str).unwrap_or("");
    if query.is_empty() {
        return "no query given".to_string();
    }
    let base = std::env::var("SEARXNG_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "http://127.0.0.1:8080".to_string());
    let url = format!(
        "{}/search?q={}&format=json",
        base.trim_end_matches('/'),
        urlencode(query)
    );
    let output = Command::new("curl")
        .arg("-sS")
        .arg("--max-time")
        .arg("20")
        .arg(&url)
        .output();
    let Ok(output) = output else {
        return "could not run curl".to_string();
    };
    let Ok(value) = serde_json::from_slice::<Value>(&output.stdout) else {
        return format!(
            "the search endpoint did not answer with JSON: {}",
            clip(String::from_utf8_lossy(&output.stdout).trim(), 300)
        );
    };
    let results = value
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if results.is_empty() {
        return "no results".to_string();
    }
    let mut text = String::new();
    for result in results.iter().take(6) {
        let title = result.get("title").and_then(Value::as_str).unwrap_or("");
        let link = result.get("url").and_then(Value::as_str).unwrap_or("");
        let snippet = result
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .replace('\n', " ");
        text.push_str(&format!("{title}\n{link}\n{}\n\n", clip(&snippet, 240)));
    }
    clip(&text, 6000)
}

fn urlencode(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Coarse: a person reading a page does not need the tag soup, and the model needs the words.
fn strip_html(text: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for ch in text.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    let mut cleaned = String::new();
    let mut blank = 0;
    for line in out.lines() {
        let line = line.trim();
        if line.is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        cleaned.push_str(line);
        cleaned.push('\n');
    }
    cleaned
}

/// Look at a picture with the local vision pair. Paths resolve against the workspace so a screenshot the
/// browser saved with a relative name is found without the model having to know where it landed.
fn image(args: &Value, world: &World) -> String {
    let path = args.get("path").and_then(Value::as_str).unwrap_or("");
    if path.is_empty() {
        return "no path given".to_string();
    }
    let question = args
        .get("question")
        .and_then(Value::as_str)
        .unwrap_or("Describe this image precisely. Read any text it contains.")
        .to_string();
    let resolved = resolve(&world.root, path);
    match crate::vision::describe(&resolved, &question) {
        Ok(said) => said,
        Err(said) => format!("could not look at {}: {said}", resolved.display()),
    }
}

fn recall(args: &Value, world: &World) -> String {
    let query = args.get("query").and_then(Value::as_str).unwrap_or("");
    if query.trim().is_empty() {
        return "no query given".to_string();
    }
    let memory = match world.memory() {
        Ok(memory) => memory,
        Err(said) => return format!("memory is not available: {said}"),
    };
    let topics = memory
        .vault()
        .read_charter()
        .map(|charter| charter.topics)
        .unwrap_or_default();
    match memory.recall(query, &topics, 1500) {
        Ok(found) if found.hits.is_empty() => "nothing in memory about that".to_string(),
        Ok(found) => clip(&found.text, 6000),
        Err(said) => format!("could not search memory: {said}"),
    }
}

fn remember(args: &Value, world: &World) -> String {
    let text = args.get("text").and_then(Value::as_str).unwrap_or("");
    if text.trim().is_empty() {
        return "no text given".to_string();
    }
    let title = args.get("title").and_then(Value::as_str);
    let supersedes = args
        .get("supersedes")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|given| !given.is_empty());
    match world.memory() {
        Ok(memory) => match memory.remember(text, title, supersedes) {
            Ok(path) => match supersedes {
                Some(old) => format!(
                    "remembered in {}; {old} is now history, kept and marked as replaced",
                    path.display()
                ),
                None => format!(
                    "remembered in {} (it sits in the inbox until it is filed)",
                    path.display()
                ),
            },
            Err(said) => format!("could not write the note: {said}"),
        },
        Err(said) => format!("memory is not available: {said}"),
    }
}

fn skills(args: &Value, world: &World) -> String {
    let action = args.get("action").and_then(Value::as_str).unwrap_or("list");
    let mut found = Vec::new();
    for dir in &world.skills {
        collect_skills(dir, 0, &mut found);
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    // One name, one skill. The directories are searched in the order the surface named them, so a
    // promoted skill in Hades' own state wins over a same-named one it inherited.
    found.dedup_by(|a, b| a.0 == b.0);
    if action == "read" {
        let name = args.get("name").and_then(Value::as_str).unwrap_or("");
        let Some((_, path)) = found.iter().find(|(skill, _)| skill == name) else {
            return format!("there is no skill called {name}");
        };
        return match std::fs::read_to_string(path) {
            Ok(text) => clip(&text, 12000),
            Err(said) => format!("could not read {}: {said}", path.display()),
        };
    }
    if found.is_empty() {
        return "no skills installed".to_string();
    }
    let mut text = String::new();
    for (name, path) in found.iter().take(400) {
        let said = std::fs::read_to_string(path)
            .ok()
            .map(|body| description(&body))
            .unwrap_or_default();
        if said.is_empty() {
            text.push_str(&format!("{name}\n"));
        } else {
            text.push_str(&format!("{name}: {said}\n"));
        }
    }
    clip(&text, 12_000)
}

fn collect_skills(dir: &Path, depth: usize, out: &mut Vec<(String, PathBuf)>) {
    if depth > 3 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        // A hidden directory is not part of the offered set. `.archive` is the real case: an archived
        // procedure is kept for the record, and offering it to the agent would put a retired instruction
        // back in front of a live task.
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with('.'))
        {
            continue;
        }
        if path.is_dir() {
            collect_skills(&path, depth + 1, out);
        } else if path.file_name().and_then(|name| name.to_str()) == Some("SKILL.md")
            && let Some(parent) = path.parent()
            && let Some(name) = parent.file_name().and_then(|name| name.to_str())
        {
            out.push((name.to_string(), path));
        }
    }
}

fn description(body: &str) -> String {
    for line in body.lines().take(20) {
        if let Some(rest) = line.trim().strip_prefix("description:") {
            return rest.trim().trim_matches('"').to_string();
        }
    }
    String::new()
}

/// Search the session logs. The daemon writes one JSON object per line; the gateway writes an array of
/// messages. Both are read the same way: find the text, then show where it came from.
fn session_search(args: &Value, world: &World) -> String {
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_lowercase();
    if query.is_empty() {
        return "no query given".to_string();
    }
    let mut hits = Vec::new();
    for dir in &world.sessions {
        collect_files(dir, 0, &mut hits);
    }
    let mut found = Vec::new();
    for path in hits.iter().take(400) {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        for line in text.lines() {
            if !line.to_lowercase().contains(&query) {
                continue;
            }
            if let Ok(value) = serde_json::from_str::<Value>(line) {
                let said = value
                    .get("text")
                    .or_else(|| value.get("goal"))
                    .or_else(|| value.get("content"))
                    .or_else(|| value.get("summary"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| line.to_string());
                let name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_default();
                found.push(format!("{name}: {}", clip(said.trim(), 300)));
            } else if !line.starts_with('[') && !line.starts_with('{') {
                let name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_default();
                found.push(format!("{name}: {}", clip(line.trim(), 300)));
            }
            if found.len() >= 10 {
                break;
            }
        }
        if found.len() >= 10 {
            break;
        }
    }
    if found.is_empty() {
        "nothing in the session record matches that".to_string()
    } else {
        found.join("\n")
    }
}

fn collect_files(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 3 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, depth + 1, out);
        } else if matches!(
            path.extension().and_then(|ext| ext.to_str()),
            Some("jsonl") | Some("json")
        ) {
            out.push(path);
        }
    }
}

/// Rough size of a message list. Four characters to a token is the usual English ratio and is enough to
/// decide when to compress; being wrong by 20 percent changes nothing at this scale.
pub fn estimate_tokens(messages: &[Value]) -> usize {
    messages
        .iter()
        .map(|message| {
            message
                .get("content")
                .and_then(Value::as_str)
                .map(|text| text.len() / 4)
                .unwrap_or(0)
                + 8
        })
        .sum()
}

/// Keep a conversation inside the window: the old turns become one summary at the top, the recent turns
/// stay verbatim. Returns true when it actually compressed.
pub fn compress(
    client: &OpenAi,
    messages: &mut Vec<Value>,
    keep: usize,
    limit_tokens: usize,
) -> Result<bool, Fault> {
    if estimate_tokens(messages) <= limit_tokens || messages.len() <= keep + 1 {
        return Ok(false);
    }
    let system = messages
        .first()
        .cloned()
        .unwrap_or_else(|| json!({"role": "system", "content": ""}));
    let split = messages.len().saturating_sub(keep);
    if split <= 1 {
        return Ok(false);
    }
    let mut transcript = String::new();
    for message in &messages[1..split] {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let content = message
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if !content.is_empty() {
            transcript.push_str(&format!("{role}: {content}\n"));
        }
    }
    let ask = vec![
        json!({
            "role": "system",
            "content": "Summarize the earlier part of a conversation for the agent that continues it. Keep decisions, names, file paths, numbers, and what was asked for. Write at most 400 words, plain prose, no preamble."
        }),
        json!({"role": "user", "content": clip(&transcript, 24000)}),
    ];
    let summary = client.converse(&ask, &|_, _| {})?;
    let base = system.get("content").and_then(Value::as_str).unwrap_or("");
    let rebuilt_system = json!({
        "role": "system",
        "content": format!("{base}\n\nEarlier in this conversation:\n{summary}")
    });
    let mut rebuilt = vec![rebuilt_system];
    rebuilt.extend_from_slice(&messages[split..]);
    *messages = rebuilt;
    Ok(true)
}

fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max * 2 / 3).collect();
    let tail: String = text
        .chars()
        .rev()
        .take(max / 3)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!(
        "{head}\n[... {} characters cut ...]\n{tail}",
        text.len() - max
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mode::Mode;

    fn names(list: &Value) -> Vec<String> {
        list.as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool.pointer("/function/name"))
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn only_the_persons_conversation_can_delegate() {
        let top = names(&schemas(0, Mode::Act));
        let worker = names(&schemas(1, Mode::Act));
        assert!(top.contains(&"delegate".to_string()));
        assert!(!worker.contains(&"delegate".to_string()));
        assert!(worker.contains(&"shell".to_string()));
    }

    #[test]
    fn a_read_job_is_not_handed_a_tool_that_changes_anything() {
        let read = names(&schemas(0, Mode::Read));
        for gone in [
            "shell",
            "write_file",
            "patch",
            "remember",
            "cron",
            "browser",
            "mcp",
        ] {
            assert!(
                !read.contains(&gone.to_string()),
                "a read job still has {gone}: {read:?}"
            );
        }
        for kept in [
            "read_file",
            "list_dir",
            "search",
            "fetch",
            "recall",
            "session_search",
            "skills",
            "web_search",
            "delegate",
            "image",
        ] {
            assert!(
                read.contains(&kept.to_string()),
                "a read job lost {kept}: {read:?}"
            );
        }
        // A worker inherits the mode, so delegation is not a way around the gate.
        let worker = names(&schemas(1, Mode::Read));
        assert!(!worker.contains(&"shell".to_string()), "{worker:?}");
    }

    #[test]
    fn a_read_job_refuses_a_mutating_call_that_arrives_anyway() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/scratch/agent-read");
        std::fs::create_dir_all(&dir).expect("a repo-local scratch directory");
        let world = World {
            root: dir.clone(),
            vault: dir.join("vault"),
            db: dir.join("mneme.db"),
            skills: Vec::new(),
            sessions: Vec::new(),
            state: dir.join("state"),
            chat: 7,
            depth: 0,
            mode: Mode::Read,
            role: crate::profile::Role::Patch,
            actor: "chat:test".to_string(),
            mcp: std::sync::Arc::new(crate::mcp::Mcp::empty()),
            allowance: crate::agent::Allowance::new(64),
        };
        let client = OpenAi::new("http://127.0.0.1:9/v1/chat/completions", "none");
        let said = execute(
            "shell",
            r#"{"command":"touch should-not-exist.txt"}"#,
            &world,
            &client,
            &|_, _| {},
        );
        assert!(said.contains("read-only"), "{said}");
        assert!(!dir.join("should-not-exist.txt").exists(), "nothing ran");
        let said = execute("mcp", r#"{"action":"list"}"#, &world, &client, &|_, _| {});
        assert!(said.contains("read-only"), "{said}");
    }

    #[test]
    fn a_patch_edits_one_anchor_and_a_read_surface_has_none() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/scratch/agent-patch");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("state")).expect("a repo-local scratch directory");
        std::fs::write(dir.join("note.txt"), "alpha\nbeta\ngamma\n").expect("a file to patch");

        let client = OpenAi::new("http://127.0.0.1:9/v1/chat/completions", "none");
        let act = world_at(&dir, Mode::Act, "chat:7");
        let said = execute(
            "patch",
            r#"{"path":"note.txt","edits":[{"old":"beta","new":"BETA"}]}"#,
            &act,
            &client,
            &|_, _| {},
        );
        assert!(said.contains("patched"), "{said}");
        assert_eq!(
            std::fs::read_to_string(dir.join("note.txt")).expect("the patched file"),
            "alpha\nBETA\ngamma\n"
        );

        let read = world_at(&dir, Mode::Read, "job:7");
        let said = execute(
            "patch",
            r#"{"path":"note.txt","edits":[{"old":"BETA","new":"beta"}]}"#,
            &read,
            &client,
            &|_, _| {},
        );
        assert!(said.contains("read-only"), "{said}");
        assert!(
            std::fs::read_to_string(dir.join("note.txt"))
                .expect("the untouched file")
                .contains("BETA")
        );
    }

    #[test]
    fn a_chat_shell_cannot_read_outside_its_root() {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/scratch/agent-cage");
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("root");
        std::fs::create_dir_all(root.join("state")).expect("a scratch home");
        std::fs::write(base.join("outside.txt"), "a secret the chat must not read")
            .expect("a file above the root");

        let client = OpenAi::new("http://127.0.0.1:9/v1/chat/completions", "none");
        let world = world_at(&root, Mode::Act, "chat:7");
        let command = format!("cat {}", base.join("outside.txt").display());
        let said = execute(
            "shell",
            &json!({ "command": command }).to_string(),
            &world,
            &client,
            &|_, _| {},
        );
        assert!(!said.contains("a secret"), "{said}");
        assert!(said.contains("Permission denied"), "{said}");

        // Landlock hooks `open`, not `access(2)`, so coreutils `test -r` can answer "yes" to a file the cage
        // will not open. Record which answer this kernel gives: the docs must not use `test -r` as evidence.
        let access = execute(
            "shell",
            &json!({ "command": format!("test -r {}; echo rc=$?", base.join("outside.txt").display()) })
                .to_string(),
            &world,
            &client,
            &|_, _| {},
        );
        println!("ACCESS_PROBE {access}");
        assert!(
            access.contains("rc=0"),
            "Landlock is expected not to mediate access(2); if this changed, the docs can use `test -r`: {access}"
        );

        // A rustup toolchain outside `$HOME` is granted through CARGO_HOME/RUSTUP_HOME. Without this the fix
        // profile's own check (`cargo test`) cannot run inside the same cage a model program runs in.
        if std::env::var_os("RUSTUP_HOME").is_some() {
            let cargo = execute(
                "shell",
                &json!({ "command": "cargo --version" }).to_string(),
                &world,
                &client,
                &|_, _| {},
            );
            assert!(cargo.contains("cargo "), "cargo inside the cage: {cargo}");
        }

        let inside = execute(
            "shell",
            r#"{"command":"printf ok > note.txt && cat note.txt"}"#,
            &world,
            &client,
            &|_, _| {},
        );
        assert!(inside.contains("ok"), "{inside}");
        assert!(root.join("note.txt").exists());

        let home = std::env::var("HOME").expect("a home");
        let read = execute(
            "read_file",
            &json!({ "path": format!("{home}/.hermes/config.yaml") }).to_string(),
            &world,
            &client,
            &|_, _| {},
        );
        assert!(read.starts_with("Refused"), "{read}");
        let listed = execute(
            "list_dir",
            &json!({ "path": format!("{home}/.hermes") }).to_string(),
            &world,
            &client,
            &|_, _| {},
        );
        assert!(listed.starts_with("Refused"), "{listed}");
    }

    #[test]
    fn the_known_list_matches_the_act_schema() {
        let mut schema = names(&schemas(0, Mode::Act));
        let mut known: Vec<String> = TOOLS.iter().map(|name| name.to_string()).collect();
        schema.sort();
        known.sort();
        assert_eq!(schema, known);
    }

    fn world_at(dir: &Path, mode: Mode, actor: &str) -> World {
        World {
            root: dir.to_path_buf(),
            vault: dir.join("vault"),
            db: dir.join("mneme.db"),
            skills: Vec::new(),
            sessions: Vec::new(),
            state: dir.join("state"),
            chat: 7,
            depth: 0,
            mode,
            role: crate::profile::Role::Patch,
            actor: actor.to_string(),
            mcp: std::sync::Arc::new(crate::mcp::Mcp::empty()),
            allowance: crate::agent::Allowance::new(64),
        }
    }

    #[test]
    fn the_chat_seam_writes_one_line_for_every_decision() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/scratch/agent-activity");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("state")).expect("a repo-local scratch directory");
        let client = OpenAi::new("http://127.0.0.1:9/v1/chat/completions", "none");
        let act = world_at(&dir, Mode::Act, "chat:7");
        let _ = execute(
            "shell",
            r#"{"command":"rm -f keep.txt"}"#,
            &act,
            &client,
            &|_, _| {},
        );
        let _ = execute(
            "read_file",
            r#"{"path":"keep.txt"}"#,
            &act,
            &client,
            &|_, _| {},
        );
        let read = world_at(&dir, Mode::Read, "job:9");
        let _ = execute(
            "shell",
            r#"{"command":"touch nope.txt"}"#,
            &read,
            &client,
            &|_, _| {},
        );
        let lines = crate::activity::read(&dir.join("state"));
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert_eq!(lines[0].actor, "chat:7");
        assert_eq!(lines[0].layer, "floor");
        assert_eq!(lines[0].decision, "refuse");
        assert!(
            lines[0].reason.contains("permanent delete"),
            "{}",
            lines[0].reason
        );
        assert_eq!(lines[0].action, "shell: rm -f keep.txt");
        assert_eq!(lines[1].layer, "surface");
        assert_eq!(lines[1].decision, "allow");
        assert_eq!(lines[1].artifact, "keep.txt");
        assert_eq!(lines[2].actor, "job:9");
        assert_eq!(lines[2].layer, "mode");
        assert_eq!(lines[2].decision, "refuse");
    }

    /// An MCP registry whose one server is the oracle fixture. The allowlist is the test's decision.
    fn mcp_with(dir: &Path, allow: &[&str]) -> std::sync::Arc<crate::mcp::Mcp> {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp_echo.py");
        let value = json!({
            "servers": {
                "echo": {
                    "command": "python3",
                    "args": [fixture.display().to_string()],
                    "allow": allow,
                    "timeout_s": 10
                }
            }
        });
        let config = crate::mcp::McpConfig::from_value(Some(&value), Path::new("test"))
            .expect("a readable test config");
        std::sync::Arc::new(crate::mcp::Mcp::new(config, Some(dir.to_path_buf())))
    }

    #[test]
    fn an_mcp_tool_not_on_the_allowlist_is_refused_and_recorded() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/scratch/agent-mcp-blocked");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("state")).expect("a repo-local scratch directory");
        let dir = std::fs::canonicalize(&dir).expect("a real path");
        let world = World {
            root: dir.clone(),
            vault: dir.join("vault"),
            db: dir.join("state/mneme.db"),
            skills: Vec::new(),
            sessions: Vec::new(),
            state: dir.join("state"),
            chat: 7,
            depth: 0,
            mode: Mode::Act,
            role: crate::profile::Role::Patch,
            actor: "chat:7".to_string(),
            mcp: mcp_with(&dir, &["echo"]),
            allowance: crate::agent::Allowance::new(64),
        };
        let client = crate::model::OpenAi::new("http://127.0.0.1:9/v1/chat/completions", "none");
        let said = execute(
            "mcp",
            r#"{"action":"call","server":"echo","tool":"secret"}"#,
            &world,
            &client,
            &|_, _| {},
        );
        assert!(said.contains("not on the allowed list"), "{said}");
        let lines = crate::activity::read(&dir.join("state"));
        let last = lines.last().expect("a refusal");
        assert_eq!(last.actor, "chat:7");
        assert_eq!(last.layer, "allowlist");
        assert_eq!(last.decision, "refuse");
        assert!(last.action.contains("mcp: echo/secret"), "{}", last.action);
    }

    #[test]
    fn an_allowed_mcp_tool_reaches_the_server() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/scratch/agent-mcp-allowed");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("state")).expect("a repo-local scratch directory");
        let dir = std::fs::canonicalize(&dir).expect("a real path");
        let world = World {
            root: dir.clone(),
            vault: dir.join("vault"),
            db: dir.join("state/mneme.db"),
            skills: Vec::new(),
            sessions: Vec::new(),
            state: dir.join("state"),
            chat: 7,
            depth: 0,
            mode: Mode::Act,
            role: crate::profile::Role::Patch,
            actor: "chat:7".to_string(),
            mcp: mcp_with(&dir, &["echo"]),
            allowance: crate::agent::Allowance::new(64),
        };
        let client = crate::model::OpenAi::new("http://127.0.0.1:9/v1/chat/completions", "none");
        let said = execute(
            "mcp",
            r#"{"action":"call","server":"echo","tool":"echo","arguments":{"text":"hello there"}}"#,
            &world,
            &client,
            &|_, _| {},
        );
        assert!(said.contains("hello there"), "{said}");
    }

    #[test]
    fn remember_can_retire_the_note_it_corrects() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/scratch/agent-supersede");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("state")).expect("a repo-local scratch directory");
        let dir = std::fs::canonicalize(&dir).expect("a real path");
        let world = World {
            root: dir.clone(),
            vault: dir.join("vault"),
            db: dir.join("state/mneme.db"),
            skills: Vec::new(),
            sessions: Vec::new(),
            state: dir.join("state"),
            chat: 0,
            depth: 0,
            mode: Mode::Act,
            role: crate::profile::Role::Patch,
            actor: "chat:test".to_string(),
            mcp: std::sync::Arc::new(crate::mcp::Mcp::empty()),
            allowance: crate::agent::Allowance::new(64),
        };
        let first = remember(
            &json!({"text": "the port is 8101", "title": "Port"}),
            &world,
        );
        assert!(first.contains("remembered in"), "{first}");
        let old = first
            .trim_start_matches("remembered in ")
            .split(" (")
            .next()
            .expect("the path in the reply");
        let old = Path::new(old)
            .strip_prefix(&world.vault)
            .expect("a path inside the vault")
            .to_string_lossy()
            .replace('\\', "/");

        let second = remember(
            &json!({
                "text": "the port is 8102",
                "title": "Port, corrected",
                "supersedes": old
            }),
            &world,
        );
        assert!(second.contains("is now history"), "{second}");
        let fresh = second
            .trim_start_matches("remembered in ")
            .split(';')
            .next()
            .expect("the path in the reply");
        let fresh = Path::new(fresh)
            .strip_prefix(&world.vault)
            .expect("a path inside the vault")
            .to_string_lossy()
            .replace('\\', "/");

        let said = recall(&json!({"query": "port"}), &world);
        assert!(said.contains("history, replaced by"), "{said}");
        let fresh_first =
            said.find(&fresh).expect("the correction") < said.find(&old).expect("history");
        assert!(fresh_first, "the correction comes first:\n{said}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_job_created_from_chat_is_a_goal_file_with_its_fields() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/scratch/agent-job");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a repo-local scratch directory");
        let world = World {
            root: dir.clone(),
            vault: dir.join("vault"),
            db: dir.join("mneme.db"),
            skills: Vec::new(),
            sessions: Vec::new(),
            state: dir.join("state"),
            chat: 7,
            depth: 0,
            mode: Mode::Act,
            role: crate::profile::Role::Patch,
            actor: "chat:test".to_string(),
            mcp: std::sync::Arc::new(crate::mcp::Mcp::empty()),
            allowance: crate::agent::Allowance::new(64),
        };
        let said = cron(
            &json!({
                "action": "add",
                "schedule": "daily 07:30",
                "goal": "Summarise the overnight X feed",
                "inputs": "the notes under 23 X",
                "outputs": "a digest with links",
                "done_when": "five items",
                "stop_when": "one pass",
                "never": "do not post"
            }),
            &world,
        );
        assert!(
            said.contains("jobs/1-summarise-the-overnight-x-feed.md"),
            "{said}"
        );
        let (jobs, problems) = crate::cron::load(&world.state);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].mode, Mode::Read, "read is the default");
        assert_eq!(jobs[0].never, "do not post");
        // The file is the record a person corrects, so the field is in it, not only in the struct.
        let text = std::fs::read_to_string(crate::cron::path_of(&world.state, 1).unwrap())
            .expect("the job file");
        assert!(text.contains("done_when: five items"), "{text}");
        assert!(
            text.trim_end().ends_with("Summarise the overnight X feed"),
            "{text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_estimate_grows_with_the_conversation() {
        let short = vec![json!({"role": "user", "content": "hi"})];
        let long = vec![json!({"role": "user", "content": "x".repeat(4000)})];
        assert!(estimate_tokens(&long) > estimate_tokens(&short) + 900);
    }

    #[test]
    fn a_description_is_read_from_the_frontmatter() {
        let body = "---\nname: x\ndescription: Use when showing a thing.\n---\n\n# X\n";
        assert_eq!(description(body), "Use when showing a thing.");
    }

    #[test]
    fn the_chat_loop_refuses_a_permanent_delete_before_the_shell_runs() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/scratch/agent-floor");
        std::fs::create_dir_all(&dir).expect("a repo-local scratch directory");
        let keep = dir.join("keep.txt");
        std::fs::write(&keep, "still here").expect("a file to delete");
        let world = World {
            root: dir.clone(),
            vault: dir.join("vault"),
            db: dir.join("mneme.db"),
            skills: Vec::new(),
            sessions: Vec::new(),
            state: dir.join("state"),
            chat: 0,
            depth: 0,
            mode: crate::mode::Mode::Act,
            role: crate::profile::Role::Patch,
            actor: "chat:test".to_string(),
            mcp: std::sync::Arc::new(crate::mcp::Mcp::empty()),
            allowance: crate::agent::Allowance::new(64),
        };
        let client = OpenAi::new("http://127.0.0.1:9/v1/chat/completions", "none");
        let said = execute(
            "shell",
            r#"{"command":"rm -rf keep.txt"}"#,
            &world,
            &client,
            &|_, _| {},
        );
        assert!(said.contains("hard floor"), "{said}");
        assert!(said.contains("permanent delete"), "{said}");
        assert!(
            keep.exists(),
            "the floor runs before the tool, so nothing was deleted"
        );
        let _ = std::fs::remove_file(&keep);
    }

    #[test]
    fn the_label_is_one_short_line() {
        let said = label("shell", r#"{"command":"echo one\necho two"}"#);
        assert!(said.starts_with("shell: echo one"));
        assert!(!said.contains("echo two"));
    }

    #[test]
    fn the_third_identical_call_is_the_limit() {
        assert!(!repeats_refuse(1));
        assert!(!repeats_refuse(2));
        assert!(repeats_refuse(3));
        assert!(repeats_refuse(4));
    }

    #[test]
    fn a_pre_tool_hook_can_refuse_a_call_the_policy_allowed() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/scratch/agent-hooks");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("state")).expect("a scratch state");
        std::fs::write(dir.join("inside.txt"), "readable").expect("a file");
        let client = OpenAi::new("http://127.0.0.1:9/v1/chat/completions", "none");
        let world = world_at(&dir, Mode::Act, "chat:7");

        std::fs::write(
            dir.join("state/hooks.json"),
            r#"{"pre_tool":["echo not today >&2; exit 7"]}"#,
        )
        .expect("a hook file");
        let said = execute(
            "read_file",
            r#"{"path":"inside.txt"}"#,
            &world,
            &client,
            &|_, _| {},
        );
        assert!(said.contains("hook"), "{said}");
        assert!(!said.contains("readable"), "{said}");

        std::fs::write(dir.join("state/hooks.json"), r#"{"pre_tool":["true"]}"#)
            .expect("a passing hook");
        let said = execute(
            "read_file",
            r#"{"path":"inside.txt"}"#,
            &world,
            &client,
            &|_, _| {},
        );
        assert!(said.contains("readable"), "{said}");
    }

    #[test]
    fn a_delegation_leaves_a_child_session_and_a_bundle() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/scratch/agent-delegate");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("state/sessions")).expect("a sessions dir");
        let world = world_at(&dir, Mode::Act, "chat:7");
        let client = OpenAi::new("http://127.0.0.1:9/v1/chat/completions", "none");
        let said = delegate(
            &json!({"tasks": ["write down one fact"]}),
            &world,
            &client,
            &|_, _| {},
        );
        assert!(said.contains("session: deleg-"), "{said}");
        assert!(said.contains("status: failed"), "{said}");
        let bundles: Vec<_> = std::fs::read_dir(dir.join("state/artifacts"))
            .expect("an artifacts dir")
            .flatten()
            .collect();
        assert_eq!(bundles.len(), 1, "one worker, one bundle");
        let report =
            std::fs::read_to_string(bundles[0].path().join("report.md")).expect("a report");
        assert!(report.contains("status: failed"), "{report}");
        assert!(report.contains("write down one fact"), "{report}");
        let result = std::fs::read_to_string(bundles[0].path().join("result.json"))
            .expect("a typed result, even when the worker failed");
        assert!(result.contains("\"status\": \"failed\""), "{result}");
        let logs: Vec<_> = std::fs::read_dir(dir.join("state/sessions"))
            .expect("a sessions dir")
            .flatten()
            .collect();
        assert_eq!(logs.len(), 1, "the child has its own log");
        let text = std::fs::read_to_string(logs[0].path()).expect("the log");
        assert!(text.contains("\"delegate\""), "{text}");
    }

    #[test]
    fn archived_skills_are_not_offered_and_one_name_resolves_once() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/scratch/agent-skills");
        let _ = std::fs::remove_dir_all(&dir);
        let own = dir.join("state/skills");
        let inherited = dir.join("inherited");
        for (base, name, said) in [
            (own.clone(), "shared", "own shared"),
            (own.clone(), "live", "own live"),
            (inherited.clone(), "shared", "inherited shared"),
            (inherited.join(".archive"), "shared", "archived shared"),
            (inherited.join(".archive"), "retired", "archived retired"),
        ] {
            let folder = base.join(name);
            std::fs::create_dir_all(&folder).expect("a skill folder");
            std::fs::write(
                folder.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: {said}\n---\n# {name}\n"),
            )
            .expect("a skill");
        }
        let mut world = world_at(&dir, Mode::Act, "chat:7");
        world.skills = vec![own, inherited];
        let listed = skills(&json!({"action": "list"}), &world);
        assert!(listed.contains("shared: own shared"), "{listed}");
        assert!(!listed.contains("inherited shared"), "{listed}");
        assert!(!listed.contains("archived shared"), "{listed}");
        assert!(!listed.contains("retired"), "{listed}");
        let read = skills(&json!({"action": "read", "name": "shared"}), &world);
        assert!(read.contains("own shared"), "{read}");
    }

    #[test]
    fn a_role_is_a_permission_not_a_prompt() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/scratch/agent-role");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("state")).expect("a scratch state");
        let client = OpenAi::new("http://127.0.0.1:9/v1/chat/completions", "none");

        let mut world = world_at(&dir, Mode::Act, "chat:7");
        world.role = crate::profile::Role::Record;
        let said = execute(
            "shell",
            r#"{"command":"printf x > x.txt"}"#,
            &world,
            &client,
            &|_, _| {},
        );
        assert!(said.starts_with("Refused"), "{said}");
        assert!(!dir.join("x.txt").exists(), "the tool must not run");

        let verify = names(&schemas_for(0, Mode::Act, crate::profile::Role::Verify));
        assert!(!verify.contains(&"shell".to_string()), "{verify:?}");
        assert!(!verify.contains(&"patch".to_string()), "{verify:?}");
        assert!(verify.contains(&"read_file".to_string()), "{verify:?}");

        let record = names(&schemas_for(0, Mode::Act, crate::profile::Role::Record));
        assert!(record.contains(&"remember".to_string()), "{record:?}");
        assert!(!record.contains(&"shell".to_string()), "{record:?}");
    }
    #[test]
    fn a_typed_result_must_carry_its_required_fields() {
        let schema = json!({
            "type": "object",
            "properties": {
                "answer": {"type": "string"},
                "count": {"type": "integer"}
            },
            "required": ["answer", "count"]
        });
        assert!(fits(&schema, &json!({"answer": "ok", "count": 2})).is_ok());
        assert!(
            fits(&schema, &json!({"answer": "ok"}))
                .unwrap_err()
                .contains("count")
        );
        assert!(
            fits(&schema, &json!({"answer": "ok", "count": "two"}))
                .unwrap_err()
                .contains("count")
        );
    }

    #[test]
    fn a_decorated_json_answer_is_still_the_object_it_meant() {
        let said = "Here it is:\n```json\n{\"report\": \"done\"}\n```\n";
        assert_eq!(parse_json_answer(said), Some(json!({"report": "done"})));
        assert_eq!(parse_json_answer("no object here"), None);
    }

    #[test]
    fn a_delegation_is_refused_when_the_shared_allowance_cannot_cover_it() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/scratch/agent-budget");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a dir");
        let mut world = world_at(&dir, Mode::Act, "chat:7");
        world.allowance = Allowance::new(1);
        let client = OpenAi::new("http://127.0.0.1:9/v1/chat/completions", "none");
        let said = delegate(
            &json!({"tasks": ["one task", "two tasks"]}),
            &world,
            &client,
            &|_, _| {},
        );
        assert!(said.contains("allowance"), "{said}");
        assert!(!said.contains("session: deleg-"), "{said}");
    }

    #[test]
    fn the_allowance_is_shared_and_finite() {
        let allowance = Allowance::new(2);
        assert!(allowance.take());
        assert!(allowance.take());
        assert!(!allowance.take(), "a third call exceeds the budget");
        assert_eq!(allowance.left(), 0);
    }
    #[test]
    fn spawn_is_offered_until_the_depth_is_spent() {
        assert!(names(&schemas(0, Mode::Act)).contains(&"spawn".to_string()));
        assert!(names(&schemas(1, Mode::Act)).contains(&"spawn".to_string()));
        assert!(!names(&schemas(2, Mode::Act)).contains(&"spawn".to_string()));
        assert!(!names(&schemas(0, Mode::Read)).contains(&"spawn".to_string()));
    }

    #[test]
    fn a_graph_at_the_depth_limit_cannot_spawn() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/scratch/agent-spawn-depth");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a dir");
        let mut world = world_at(&dir, Mode::Act, "chat:7");
        world.depth = MAX_SPAWN_DEPTH;
        let client = OpenAi::new("http://127.0.0.1:9/v1/chat/completions", "none");
        let said = spawn(&json!({"goal": "go deeper"}), &world, &client, &|_, _| {});
        assert!(said.contains("depth is spent"), "{said}");
    }

    #[test]
    fn a_spawn_without_allowance_is_refused_before_a_child_exists() {
        let dir =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/scratch/agent-spawn-budget");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a dir");
        let mut world = world_at(&dir, Mode::Act, "chat:7");
        world.allowance = Allowance::new(1);
        let client = OpenAi::new("http://127.0.0.1:9/v1/chat/completions", "none");
        let said = spawn(&json!({"goal": "go"}), &world, &client, &|_, _| {});
        assert!(said.contains("allowance"), "{said}");
        assert!(!said.contains("session: spawn-"), "{said}");
    }

    #[test]
    fn a_failed_spawn_still_returns_a_typed_artifact() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/scratch/agent-spawn-fail");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a dir");
        let world = world_at(&dir, Mode::Act, "chat:7");
        let client = OpenAi::new("http://127.0.0.1:9/v1/chat/completions", "none");
        let said = spawn(
            &json!({"goal": "write down one fact", "steps": 2}),
            &world,
            &client,
            &|_, _| {},
        );
        assert!(said.contains("status: failed"), "{said}");
        assert!(said.contains("artifact: "), "{said}");
        let bundles: Vec<_> = std::fs::read_dir(dir.join("state/artifacts"))
            .expect("an artifacts dir")
            .flatten()
            .collect();
        assert_eq!(bundles.len(), 1);
        let result =
            std::fs::read_to_string(bundles[0].path().join("result.json")).expect("a typed result");
        assert!(result.contains("\"status\": \"failed\""), "{result}");
    }

    /// An absolute path outside the workspace, next to a real file there so a refusal is about the
    /// boundary and not a missing file. The layout is the one `a_chat_shell_cannot_read_outside_its_root`
    /// uses: the workspace at `<base>/work`, the outside tree beside it at `<base>/away`.
    fn outside_of(base: &Path, name: &str) -> PathBuf {
        let away = base.join("away");
        std::fs::create_dir_all(&away).expect("a directory outside the workspace");
        let file = away.join(name);
        std::fs::write(&file, "alpha\nbeta\nalpha\n").expect("a file outside the workspace");
        file
    }

    /// A scratch tree with the workspace at `<base>/work`, so something can sit beside the root. Built
    /// on `world_at`, the module's one `World` helper.
    fn world_beside_outside(name: &str) -> (PathBuf, World) {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../target/scratch/{name}"));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("work");
        std::fs::create_dir_all(&root).expect("a scratch workspace");
        let world = world_at(&root, Mode::Act, "chat:7");
        (base, world)
    }

    #[test]
    fn a_read_file_of_an_absolute_path_outside_the_root_is_refused_by_the_policy() {
        let (base, world) = world_beside_outside("agent-policy-read");
        let file = outside_of(&base, "secret.txt");
        let said = execute(
            "read_file",
            &json!({ "path": file.display().to_string() }).to_string(),
            &world,
            &OpenAi::new("http://127.0.0.1:9/v1/chat/completions", "none"),
            &|_, _| {},
        );
        assert!(said.starts_with("Refused:"), "{said}");
        assert!(said.contains("outside the permitted read roots"), "{said}");
    }

    #[test]
    fn a_read_file_of_a_symlink_that_leaves_the_root_is_refused_by_the_policy() {
        let (base, world) = world_beside_outside("agent-policy-link");
        let file = outside_of(&base, "linked.txt");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&file, world.root.join("door.txt")).expect("the symlink");
        #[cfg(not(unix))]
        let _ = file;

        let said = execute(
            "read_file",
            &json!({ "path": "door.txt" }).to_string(),
            &world,
            &OpenAi::new("http://127.0.0.1:9/v1/chat/completions", "none"),
            &|_, _| {},
        );
        #[cfg(unix)]
        {
            assert!(said.starts_with("Refused:"), "{said}");
            assert!(said.contains("symlink"), "{said}");
        }
        #[cfg(not(unix))]
        assert!(!said.starts_with("Refused:"), "no unix, no link: {said}");
    }

    #[test]
    fn a_write_file_to_an_absolute_path_outside_the_root_is_refused_by_the_policy() {
        let (base, world) = world_beside_outside("agent-policy-write");
        let file = outside_of(&base, "made.txt");
        let said = execute(
            "write_file",
            &json!({ "path": file.display().to_string(), "content": "no" }).to_string(),
            &world,
            &OpenAi::new("http://127.0.0.1:9/v1/chat/completions", "none"),
            &|_, _| {},
        );
        assert!(said.starts_with("Refused:"), "{said}");
        assert!(said.contains("outside the permitted write roots"), "{said}");
        assert!(
            std::fs::read_to_string(&file).expect("the untouched file") == "alpha\nbeta\nalpha\n",
            "nothing was written outside the workspace"
        );
    }

    #[test]
    fn a_patch_of_an_absolute_path_outside_the_root_is_refused_by_the_policy() {
        let (base, world) = world_beside_outside("agent-policy-patch");
        let file = outside_of(&base, "note.txt");
        let said = execute(
            "patch",
            &json!({
                "path": file.display().to_string(),
                "edits": [{ "old": "alpha", "new": "ALPHA" }]
            })
            .to_string(),
            &world,
            &OpenAi::new("http://127.0.0.1:9/v1/chat/completions", "none"),
            &|_, _| {},
        );
        assert!(said.starts_with("Refused:"), "{said}");
        assert!(said.contains("outside the permitted write roots"), "{said}");
        assert_eq!(
            std::fs::read_to_string(&file).expect("the untouched file"),
            "alpha\nbeta\nalpha\n",
            "nothing was changed outside the workspace"
        );
    }

    #[test]
    fn a_list_dir_of_an_absolute_path_outside_the_root_is_refused_by_the_policy() {
        let (base, world) = world_beside_outside("agent-policy-list");
        let away = base.join("away");
        std::fs::create_dir_all(&away).expect("a directory outside the workspace");
        let said = execute(
            "list_dir",
            &json!({ "path": away.display().to_string() }).to_string(),
            &world,
            &OpenAi::new("http://127.0.0.1:9/v1/chat/completions", "none"),
            &|_, _| {},
        );
        assert!(said.starts_with("Refused:"), "{said}");
        assert!(said.contains("outside the permitted read roots"), "{said}");
    }

    #[test]
    fn a_search_of_an_absolute_path_outside_the_root_is_refused_by_the_policy() {
        let (base, world) = world_beside_outside("agent-policy-search");
        let away = base.join("away");
        std::fs::create_dir_all(&away).expect("a directory outside the workspace");
        std::fs::write(away.join("note.txt"), "a secret pattern here")
            .expect("a file outside the workspace");
        let said = execute(
            "search",
            &json!({ "pattern": "secret", "path": away.display().to_string() }).to_string(),
            &world,
            &OpenAi::new("http://127.0.0.1:9/v1/chat/completions", "none"),
            &|_, _| {},
        );
        assert!(said.starts_with("Refused:"), "{said}");
        assert!(said.contains("outside the permitted read roots"), "{said}");
        assert!(
            !said.contains("a secret pattern"),
            "nothing was read outside the root: {said}"
        );
    }
}
