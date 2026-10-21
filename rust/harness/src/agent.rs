//! A conversation with tools.
//!
//! This is the loop a chat surface runs: the model may answer in prose, or ask for tools, or both, and
//! the loop keeps going until it answers. There is no program, no check and no attempt budget here. Those
//! belong to a governed run, where the question is whether a piece of work is done; a chat asks what the
//! person said and what the agent knows.

use crate::fault::Fault;
use crate::gateway::children::{Admission, Event as ChildEvent, Refusal, Registry};
use crate::memory::{Memory, MnemeMemory};
use crate::model::{Feed, FeedFn, OpenAi};
use crate::policy::Decision;
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
    /// The session this surface speaks for: the parent a spawned child records, and the log its
    /// `child_admitted` and `child_result` events land in. The gateway supplies the chat's
    /// child-lifecycle session and the TUI supplies its own; a surface that never spawns with a
    /// gateway (a `hades turn`, a test) supplies a name whose log stays quiet.
    pub session: String,
    /// Messages a running loop reads between steps. `None` for a surface nobody can send to; a child
    /// admitted by the gateway gets one, and `tell` is the only thing that writes it.
    pub inbox: Option<std::sync::Arc<Inbox>>,
    /// The MCP servers this surface may reach, shared by the chats and workers a gateway spawns so a
    /// server is started once rather than per message.
    pub mcp: std::sync::Arc<crate::mcp::Mcp>,
    /// The model calls this conversation and its workers may spend, counted once for the whole tree.
    pub allowance: std::sync::Arc<Allowance>,
    /// The question surface, when a person can answer one. `None` on a surface with nobody to ask, which
    /// is what makes a headless job refuse at the floor instead of waiting forever.
    pub questions: Option<std::sync::Arc<crate::question::Desk>>,
}

/// A mailbox a caller can hand a running loop. The loop owns the type because the loop is what decides
/// when a message can be seen: at a step boundary, never inside a call already sent to the model. A
/// message that arrives too late for that is not retried and not faked; it waits for the next step, and
/// if there is none it is never read. That is the honest limit of a turn-based loop.
#[derive(Debug, Default)]
pub struct Inbox {
    messages: std::sync::Mutex<Vec<String>>,
}

impl Inbox {
    /// Hand one message to the loop. Never blocks and never fails: a mailbox with no reader is a
    /// message the child will not see, not an error in the sender.
    pub fn push(&self, text: impl Into<String>) {
        self.messages
            .lock()
            .unwrap_or_else(|held| held.into_inner())
            .push(text.into());
    }

    /// Everything waiting, oldest first. The loop calls this at the top of a step.
    pub fn drain(&self) -> Vec<String> {
        let mut held = self
            .messages
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::mem::take(&mut *held)
    }
}

impl World {
    fn memory(&self) -> Result<MnemeMemory, String> {
        MnemeMemory::open(&self.vault, &self.db)
    }

    /// The path policy the file tools answer to: `root` is the only read root and the only write
    /// root, with one exception earned by use - the browser's screenshot directory under `state`
    /// is a read root, so the screenshot a browser tool just wrote is an image this surface can be
    /// told to look at. Writes stay `[root]` only: the shots directory is where the browser puts
    /// pictures, and no file tool writes there.
    ///
    /// The directory is created here, on the policy's first build per surface, because a default
    /// screenshot that lands in a directory the `image` tool may not read dead-ends the
    /// screenshot-then-look loop that is the reason the tool exists. A directory that cannot be
    /// made or resolved is skipped, not fatal: the tools refuse paths under an unresolvable root
    /// all the same, and a chat surface runs fine with no browser at all.
    ///
    /// `shots` is admitted only when it is a real directory whose canonical form is a direct child
    /// of the canonical state directory. A symlink at that name is refused: following it would
    /// hand a read root to wherever the link points, which is a place the state directory's owner
    /// did not choose for screenshots.
    pub fn policy(&self) -> crate::path::PathPolicy {
        let mut policy = crate::path::PathPolicy::for_root(&self.root);
        let shots = self.state.join("shots");
        if std::fs::create_dir_all(&shots).is_ok()
            && let Ok(metadata) = std::fs::symlink_metadata(&shots)
            && metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && let Ok(canonical) = std::fs::canonicalize(&shots)
            && let Ok(state) = std::fs::canonicalize(&self.state)
            && canonical.parent() == Some(state.as_path())
        {
            policy.read_roots.push(canonical);
        }
        policy
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
                "description": "Look at an image file on this machine and answer a question about it. Use it for screenshots the browser saved (in the state directory's shots folder), for a photo the person sent, or any picture whose content matters.",
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
                        "path": {"type": "string", "description": "For screenshot or upload; for cookies, the cookie path. A screenshot with no path is saved under the state directory's shots folder, which the image tool can read."},
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
                "description": "Fetch a URL and return it as text (HTML is stripped coarsely). HTTP and HTTPS only: other schemes, URLs with credentials in them, and private or loopback addresses are refused, as is any redirect that points somewhere this rule refuses.",
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
                "name": "library",
                "description": "Search or read Hades' offline library: PDFs, EPUB books, LaTeX, Markdown and text imported with `hades library add`. Use it when the network is unavailable, when the person names a book or paper, or when a claim should come from a local document. Search returns entry ids and snippets; read returns the canonical text around an offset.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["search", "read", "list"]},
                        "query": {"type": "string", "description": "For search: words to find."},
                        "id": {"type": "string", "description": "For read: the entry id from search or list."},
                        "offset": {"type": "integer", "description": "For read: character offset. Default 0."},
                        "limit": {"type": "integer", "description": "For search: hits. Default 5, max 25."}
                    },
                    "required": ["action"]
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
                "description": "List the skills available, read one by name, or resolve its folder with path. A skill is a procedure worth following; read it before doing a task it covers. Its scripts live under <path>/scripts.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["list", "read", "path"]},
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
                "name": "goal",
                "description": "The standing goal this chat is keeping. `status` reports it. `done` claims the goal is finished, and the goal's own recorded check decides: a failing check leaves the goal open and records the result, so the work continues. `pause` stops the heartbeat until a person resumes it. A chat with no goal has nothing to report.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["status", "done", "pause"]},
                        "reason": {"type": "string", "description": "For pause: why the goal should stop being continued."}
                    },
                    "required": ["action"]
                }
            }
        }));
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
                "description": "Create one child graph: a worker with its own loop and session that must return one JSON object matching a schema. Use it for a sub-goal that deserves its own investigation and a typed answer. On a gateway the child is admitted and you get a handle back at once: collect it with `result` and check on it with `children`. The child can spawn one level further; the depth and the model-call allowance are both shared. The child returns to you, not to the person's conversation.",
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
        list.as_array_mut().unwrap().push(json!({
            "type": "function",
            "function": {
                "name": "children",
                "description": "The children you spawned, with their state, session and result path. A child is one of admitted, running, done, failed or lost. Use it when you want to know what is still out.",
                "parameters": {"type": "object", "properties": {}}
            }
        }));
        list.as_array_mut().unwrap().push(json!({
            "type": "function",
            "function": {
                "name": "result",
                "description": "Collect one child's answer. Waits until the child finishes or until `timeout_s` runs out, then returns the checked JSON object for a child that succeeded, or its status and what went wrong for one that did not. A timeout is not a kill: the child may still finish.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "child_id": {"type": "string", "description": "The id `spawn` returned."},
                        "timeout_s": {"type": "integer", "description": "Seconds to wait for it, 1 to 600. Default 60."}
                    },
                    "required": ["child_id"]
                }
            }
        }));
        list.as_array_mut().unwrap().push(json!({
            "type": "function",
            "function": {
                "name": "tell",
                "description": "Send one message to a child that is still running. It reads it at its next step, so it can change what the child does next but not what it already did. A finished child refuses it: collect its result instead.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "child_id": {"type": "string", "description": "The id `spawn` returned."},
                        "text": {"type": "string", "description": "What the child should know or do next. It cannot see this conversation."}
                    },
                    "required": ["child_id", "text"]
                }
            }
        }));
    }
    // Asking changes nothing, so it survives the read filter and is the main tool in plan mode.
    list.as_array_mut().unwrap().push(json!({
        "type": "function",
        "function": {
            "name": "ask",
            "description": "Ask the person a question when the work genuinely forks: an engineering choice, a goal or scope change, or a preference where more than one way is valid. Give 2 to 4 options, mark exactly one recommended, and put what changes if it is picked in each detail. A blocking question stops until they answer; an advisory one takes the recommendation and continues. Do not ask about reversible details, and never ask for permission: the floor asks that.",
            "parameters": {
                "type": "object",
                "properties": {
                    "prompt": {"type": "string", "description": "The question, one line."},
                    "why": {"type": "string", "description": "One sentence on why this is a real fork."},
                    "blocking": {"type": "boolean", "description": "True only when the answer changes what may be touched or what the goal is. Default false."},
                    "options": {
                        "type": "array",
                        "minItems": 2,
                        "maxItems": 4,
                        "items": {
                            "type": "object",
                            "properties": {
                                "key": {"type": "string", "description": "stable lowercase key, e.g. sqlite"},
                                "label": {"type": "string", "description": "short label for the bar"},
                                "detail": {"type": "string", "description": "what changes if this is picked"},
                                "recommended": {"type": "boolean"}
                            },
                            "required": ["key", "label", "detail"]
                        }
                    }
                },
                "required": ["prompt", "why", "options"]
            }
        }
    }));
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
const TOOLS: [&str; 24] = [
    "shell",
    "read_file",
    "write_file",
    "patch",
    "list_dir",
    "search",
    "image",
    "browser",
    "fetch",
    "library",
    "recall",
    "remember",
    "session_search",
    "skills",
    "web_search",
    "cron",
    "goal",
    "mcp",
    "delegate",
    "spawn",
    "children",
    "result",
    "tell",
    "ask",
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
        "shell"
            | "write_file"
            | "patch"
            | "remember"
            | "cron"
            | "browser"
            | "mcp"
            | "spawn"
            | "tell"
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
        // A message from the parent is read at a step boundary, before the model is asked. It cannot
        // interrupt the call already in flight, and it does not count as the model's own turn: the
        // label says where it came from so a child cannot mistake an instruction for its own idea.
        if let Some(inbox) = &world.inbox {
            for said in inbox.drain() {
                messages.push(json!({
                    "role": "user",
                    "content": format!("[a message from your parent while you work] {said}")
                }));
            }
        }
        // An answer to a question the run already moved past. It arrives here, at a step boundary, with
        // the assumption it replaces. Nothing already done is rewound.
        if let Some(desk) = &world.questions {
            for late in desk.drain_late() {
                let chosen = late.chosen.unwrap_or_else(|| "no option".into());
                let note = late
                    .note
                    .map(|note| format!(" Their note: {note}"))
                    .unwrap_or_default();
                messages.push(json!({
                    "role": "user",
                    "content": format!(
                        "[the person answered question #{} after you had assumed `{}`. They chose `{}`.{} Continue from here; work already done is not rewound.]",
                        late.id.0, late.assumed, chosen, note
                    )
                }));
            }
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
            let result =
                execute_with_stop(&call.name, &call.arguments, world, client, activity, stop);
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

pub(crate) fn label(name: &str, arguments: &str) -> String {
    let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    let detail = match name {
        "shell" => args.get("command").and_then(Value::as_str).unwrap_or(""),
        "read_file" | "write_file" | "patch" | "list_dir" => {
            args.get("path").and_then(Value::as_str).unwrap_or("")
        }
        "search" => args.get("pattern").and_then(Value::as_str).unwrap_or(""),
        "fetch" => args.get("url").and_then(Value::as_str).unwrap_or(""),
        "web_search" => args.get("query").and_then(Value::as_str).unwrap_or(""),
        "library" => args
            .get("query")
            .or_else(|| args.get("id"))
            .and_then(Value::as_str)
            .unwrap_or("the shelf"),
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
        "children" => "children",
        "result" => args
            .get("child_id")
            .and_then(Value::as_str)
            .unwrap_or("a child"),
        "tell" => args
            .get("child_id")
            .and_then(Value::as_str)
            .unwrap_or("a child"),
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

/// How long a blocking question waits before the recommended option is taken. Tests use a short
/// deadline so a missed answer fails fast instead of stalling the suite.
#[cfg(not(test))]
const ASK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
#[cfg(test)]
const ASK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The test and library entry point for a tool call with no stop flag. The loop uses the stop-aware
/// variant, so a question wait can be interrupted by the same flag that stops the turn.
#[cfg(test)]
fn execute(
    name: &str,
    arguments: &str,
    world: &World,
    client: &OpenAi,
    activity: &dyn Fn(Activity, &str),
) -> String {
    execute_with_stop(name, arguments, world, client, activity, None)
}

fn execute_with_stop(
    name: &str,
    arguments: &str,
    world: &World,
    client: &OpenAi,
    activity: &dyn Fn(Activity, &str),
    stop: Option<&AtomicBool>,
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
    // The hard floor, before the tool does anything. A surface with a person attached raises a question
    // and waits; a surface with nobody to ask refuses into the model's result, as it always has.
    if let Some(floor) = crate::floor::check(name, &args, &world.root) {
        let Some(desk) = &world.questions else {
            decided(
                world,
                "floor",
                &what,
                "refuse",
                floor.category().phrase(),
                &artifact,
            );
            return floor.refusal();
        };
        let ask = crate::question::Ask::permission(
            format!("Allow {}?", clip(&what, 100)),
            floor.category().phrase(),
        );
        let request = match desk.permission(&world.session, floor.action(), &floor.reason(), ask) {
            Ok(request) => request,
            Err(said) => {
                decided(
                    world,
                    "floor",
                    &what,
                    "refuse",
                    &format!("the question could not be recorded: {said}"),
                    &artifact,
                );
                return floor.refusal();
            }
        };
        let answer = desk.wait(request.id, ASK_TIMEOUT, stop).ok().flatten();
        match answer {
            Some(answer) if answer.decision == Decision::Allow => {
                decided(
                    world,
                    "floor",
                    &what,
                    "allow",
                    &format!("answered by {}", answer.by),
                    &artifact,
                );
            }
            Some(answer) => {
                decided(
                    world,
                    "floor",
                    &what,
                    "deny",
                    &format!("answered by {}", answer.by),
                    &artifact,
                );
                let note = answer.note.unwrap_or_default();
                return if note.is_empty() {
                    floor.refusal()
                } else {
                    format!("{}\n\nThe person said: {note}", floor.refusal())
                };
            }
            None => {
                decided(
                    world,
                    "floor",
                    &what,
                    "deny",
                    "no answer before the deadline",
                    &artifact,
                );
                return format!(
                    "{}\n\nNo answer arrived, so the action was not taken. Continue without it, or ask again later.",
                    floor.refusal()
                );
            }
        }
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
        "library" => library_tool(&args, world),
        "delegate" => delegate(&args, world, client, activity),
        "spawn" => spawn(&args, world, client, activity),
        "children" => list_children(world),
        "result" => child_result(&args, world),
        "tell" => tell_child(&args, world),
        "cron" => crate::agent::cron(&args, world),
        "goal" => goal_tool(&args, world),
        "ask" => ask_tool(&args, world, stop),
        "mcp" => world
            .mcp
            .run(&args)
            .unwrap_or_else(|said| format!("MCP: {said}")),
        other => format!("there is no tool called {other}"),
    }
}

/// Ask the person a question with options. The model supplies the fork; the shape is validated here, so a
/// malformed question is refused with the reason rather than rendered as a broken bar.
fn ask_tool(args: &Value, world: &World, stop: Option<&AtomicBool>) -> String {
    let Some(desk) = &world.questions else {
        return "No person is attached to this surface, so there is nobody to ask. Decide with the best option and say which you assumed.".into();
    };
    let mut options = Vec::new();
    for option in args["options"].as_array().into_iter().flatten() {
        let mut choice = crate::question::Choice::new(
            option["key"].as_str().unwrap_or(""),
            option["label"].as_str().unwrap_or(""),
            option["detail"].as_str().unwrap_or(""),
            crate::question::Effect::Free,
        );
        choice.recommended = option["recommended"].as_bool().unwrap_or(false);
        options.push(choice);
    }
    let blocking = args["blocking"].as_bool().unwrap_or(false);
    let mut ask = crate::question::Ask::choice(
        args["prompt"].as_str().unwrap_or(""),
        args["why"].as_str().unwrap_or(""),
        blocking,
        options,
    );
    if blocking && ask.default.is_none() {
        ask.default = ask.recommended().map(|choice| choice.key.clone());
    }
    if let Err(said) = ask.validate() {
        return format!(
            "The question was refused: {said}. Fix it and ask again, or decide yourself and say which you assumed."
        );
    }
    ask.family = args["kind"].as_str().unwrap_or("").to_string();
    // What this root has already taught us about this family of question.
    let entries = crate::taste::read(&world.state).unwrap_or_default();
    let learned = crate::taste::learn(&entries, &world.root.to_string_lossy(), &ask.family());
    if learned.auto
        && let Some(key) = learned.recommendation.clone()
    {
        return json!({
            "assumed (learned)": key,
            "note": "the last five answers agreed, so this question was not asked; say so if this one should differ"
        })
        .to_string();
    }
    if let Some(key) = learned.recommendation.clone()
        && ask.option(&key).is_some()
    {
        for choice in &mut ask.options {
            choice.recommended = choice.key == key;
        }
        let position = ask
            .options
            .iter()
            .position(|choice| choice.key == key)
            .unwrap_or(0);
        let choice = ask.options.remove(position);
        ask.options.insert(0, choice);
        if ask.blocking {
            ask.default = Some(key);
        }
    }
    let request = match desk.choice(&world.session, "question", &ask.prompt, ask.clone()) {
        Ok(Some(request)) => request,
        Ok(None) => {
            return "The question budget for this turn is spent. Decide with the recommended option and say which you assumed.".into();
        }
        Err(said) => {
            return format!("The question could not be recorded: {said}. Decide yourself.");
        }
    };
    let answer = if blocking {
        desk.wait(request.id, ASK_TIMEOUT, stop).ok().flatten()
    } else {
        None
    };
    if answer.is_none() {
        // The run is moving on. A later answer is delivered at the next step boundary.
        desk.note_assumed(request.id, &ask.fallback());
    }
    let key = answer
        .as_ref()
        .and_then(|answer| answer.option.clone())
        .filter(|key| key == "other" || ask.option(key).is_some())
        .unwrap_or_else(|| ask.fallback());
    let label = ask
        .option(&key)
        .map(|choice| choice.label.clone())
        .unwrap_or_else(|| "something else".into());
    if blocking {
        let mut said = json!({"option": key, "label": label});
        if let Some(note) = answer
            .and_then(|answer| answer.note)
            .filter(|note| !note.is_empty())
        {
            said["note"] = json!(note);
        }
        said.to_string()
    } else {
        json!({"assumed": key, "label": label, "note": "the person can change this in the next turn"})
            .to_string()
    }
}

/// One worker per task, in parallel, each with its own conversation and the same tools. A worker cannot
/// delegate, so the tree is one level deep by construction rather than by a counter that might be missed.
const WORKER: &str = "You are one worker in a team. You have tools: shell, files, search, the web, memory, skills, \
sessions and images. Do the task with them instead of asking about it. You cannot see the conversation that \
sent you this task, so treat the task text as everything you know. Answer with a short report: what you did, \
what you found, and exact paths or numbers. Never ask a question back.";

/// The one system prompt a child sees, whether it runs in the blocking path or in the gateway's
/// executor. A second copy of this text is a second contract, and A4 compares the two paths' answers.
///
/// Two blocks: the fixed worker preamble, then the contract and its schema. Siblings share the first block
/// byte for byte, so a provider that caches a prefix can reuse it; the joined text is identical to the
/// pre-composer format, which a test pins.
pub(crate) fn child_prompt(schema: &Value) -> String {
    let schema = serde_json::to_string_pretty(schema).unwrap_or_else(|_| "{}".to_string());
    crate::harness_files::compose(vec![
        crate::harness_files::Block {
            name: "worker",
            text: WORKER.to_string(),
        },
        crate::harness_files::Block {
            name: "schema",
            text: format!(
                "Return one JSON object and nothing else. It must satisfy this schema:\n{schema}\n\
                 Do not put prose around it. Do not use Markdown fences. A missing or mistyped field fails \
                 the task."
            ),
        },
    ])
    .text
}

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
pub(crate) fn parse_json_answer(text: &str) -> Option<Value> {
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
pub(crate) fn fits(schema: &Value, value: &Value) -> Result<(), String> {
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
    parent: &str,
) -> Result<(String, Option<Value>, String), String> {
    let store = session_tree::FileStore::new(worker.state.join("sessions"));
    let id = session_tree::SessionId::new(name);
    let mut log = session_tree::Session::create(store, session_tree::SystemClock, id.clone())
        .map_err(|fault| format!("{fault:?}"))?;
    // `parent_session` is the same key the registry writes, so a reader walking either path finds the
    // same fact: this log belongs to a child of that session.
    let _ = log.append(
        session_tree::Kind::Meta,
        json!({"actor": kind, "parent": worker.actor, "parent_session": parent, "role": worker.role.name(), "goal": goal, "schema": schema}),
    );
    let _ = log.append(session_tree::Kind::User, json!({"text": goal}));
    let system = child_prompt(schema);
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
        // A child gets its own log and its own bundle. The parent's context gets a handle and a checked
        // result, not the worker's whole transcript.
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_secs())
            .unwrap_or(0);
        let child = format!("deleg-{stamp}-{index}-{}", mneme::slugify(&job.goal, 4));
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
            session: child.clone(),
            inbox: None,
            mcp: std::sync::Arc::clone(&world.mcp),
            allowance: std::sync::Arc::clone(&world.allowance),
            questions: world.questions.clone(),
        };
        let client = client.clone();
        activity(
            Activity::Start,
            &format!("delegate: {}", clip(&job.goal, 60)),
        );
        let started = job.goal.clone();
        let schema = job.schema.clone();
        let typed = job.typed;
        let parent = world.session.clone();
        note_child(
            world,
            ChildEvent::Admitted {
                child_id: child.clone(),
                parent_session: parent.clone(),
                session: child.clone(),
                chat: world.chat,
                goal: started.clone(),
            },
        );
        let name = child.clone();
        handles.push((
            started.clone(),
            child,
            std::thread::spawn(move || {
                run_child(
                    &name, &started, &schema, typed, &worker, &client, "delegate", 12, &parent,
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
        note_child(
            world,
            ChildEvent::Result {
                child_id: child.clone(),
                parent_session: world.session.clone(),
                status: match &report {
                    Ok((status, _, _)) => status.clone(),
                    Err(_) => "failed".to_string(),
                },
                result_path: artifact.display().to_string(),
            },
        );
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

/// A recursive node: one child graph with a depth, a slice of the shared allowance and a typed
/// contract. It returns to this parent only; it cannot reach the root, and the root learns of it
/// through this result.
///
/// Two shapes, one meaning. Inside a gateway the child is *admitted*: the record and the executor
/// exist before the first model call, and the caller gets `{child_id, session, state}` at once, with
/// `result` to collect. On a surface with no gateway (`hades turn`, a test) the child runs here and the
/// finished outcome is returned. The shapes differ on purpose: a handle is not a result, and pretending
/// otherwise would make a caller parse a field that may not be there yet.
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
    // One contract on both paths: the child returns a JSON object, and the parent checks it against the
    // schema (the report envelope when none was given). Letting the blocking path wrap prose into a
    // report would make one tool mean two things depending on which surface ran it.
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
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0);
    let name = format!("spawn-{stamp}-{}", mneme::slugify(&goal, 4));
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
        // The child speaks for its own session, so a grandchild records the right parent.
        session: name.clone(),
        // The registry hands the executor a mailbox at admission; this surface has none to give.
        inbox: None,
        mcp: std::sync::Arc::clone(&world.mcp),
        allowance: std::sync::Arc::clone(&world.allowance),
        questions: world.questions.clone(),
    };
    activity(Activity::Start, &format!("spawn: {}", clip(&goal, 60)));

    // Inside a gateway the child outlives this call — but only for the person's own conversation. A
    // delegated worker (depth > 0) takes the blocking path even when a registry exists: `delegate`
    // joins its workers, and an async spawn from inside one would outlive the join and orphan a
    // grandchild nobody is holding.
    if world.depth == 0
        && let Some(registry) = Registry::existing(&world.state)
    {
        let admission = Admission {
            id: name.clone(),
            parent: world.session.clone(),
            session: name,
            chat: world.chat,
            goal,
            schema,
            steps,
        };
        return match registry.admit(admission, worker, client.clone()) {
            Ok(admitted) => admitted.to_value().to_string(),
            Err(refusal) => refusal_text(&refusal),
        };
    }

    // No gateway, or a worker: run it here. `hades turn`, the tests and every delegated worker take this
    // path, and it blocks for the whole child, which is the honest behaviour when the caller joins it.
    let artifact = world
        .state
        .join("artifacts")
        .join(&name)
        .join("result.json");
    note_child(
        world,
        ChildEvent::Admitted {
            child_id: name.clone(),
            parent_session: world.session.clone(),
            session: name.clone(),
            chat: world.chat,
            goal: goal.clone(),
        },
    );
    let outcome = run_child(
        &name,
        &goal,
        &schema,
        true,
        &worker,
        client,
        "spawn",
        steps,
        &world.session,
    );
    let status = match &outcome {
        Ok((status, _, _)) => status.clone(),
        Err(_) => "failed".to_string(),
    };
    note_child(
        world,
        ChildEvent::Result {
            child_id: name.clone(),
            parent_session: world.session.clone(),
            status: status.clone(),
            result_path: artifact.display().to_string(),
        },
    );
    match outcome {
        Ok((_, Some(result), _)) => json!({
            "session": name,
            "artifact": artifact.display().to_string(),
            "status": status,
            "result": result,
        })
        .to_string(),
        Ok((_, None, raw)) => json!({
            "session": name,
            "artifact": artifact.display().to_string(),
            "status": status,
            "result": Value::Null,
            "raw": raw,
        })
        .to_string(),
        Err(error) => json!({
            "session": name,
            "artifact": artifact.display().to_string(),
            "status": status,
            "result": Value::Null,
            "raw": error,
        })
        .to_string(),
    }
}

/// One line to the parent's log. Best effort on a surface with no gateway: the child is real whether
/// or not the evidence landed, and refusing to spawn because a log could not be written would make a
/// history problem into a work problem. The registry path is stricter, because a child it admits will
/// outlive the call that admitted it.
fn note_child(world: &World, event: ChildEvent) {
    if let Err(said) = crate::gateway::children::record_event(&world.state, &world.session, &event)
    {
        eprintln!(
            "children: could not record {} for {}: {said}",
            event.type_name(),
            world.session
        );
    }
}

/// A refusal as the model reads it. The prefix is the class, because a model that cannot tell "try
/// again when one finishes" from "that id is wrong" from "it is still running" retries the wrong one.
fn refusal_text(refusal: &Refusal) -> String {
    match refusal {
        Refusal::Busy { .. } => format!("busy: {refusal}"),
        Refusal::TimedOut { .. } => format!("timeout: {refusal}"),
        Refusal::Finished { .. } => format!("finished: {refusal}"),
        Refusal::Missing { .. } => format!("missing: {refusal}"),
        Refusal::Exists { .. } | Refusal::SessionInUse { .. } | Refusal::Io { .. } => {
            format!("failed: {refusal}")
        }
    }
}

/// The registry for this surface, or the explanation that there is none. Read surfaces and a surface
/// with no gateway get the explanation rather than an empty list that looks like "no children".
fn gateway_registry(world: &World) -> Result<Registry, String> {
    Registry::existing(&world.state).ok_or_else(|| {
        "there are no gateway children on this surface: a child here runs to completion and hands \
         back its result, so there is nothing to list, collect or message"
            .to_string()
    })
}

/// `children`: what this parent has out. Scoped to `world.session`, because two chats in one gateway
/// must not see each other's work, and the cap is the only fact they share. A surface with no gateway
/// gets an empty list: it has no children, and a sentence is not a list.
pub(crate) fn list_children(world: &World) -> String {
    let Ok(registry) = gateway_registry(world) else {
        return "[]".to_string();
    };
    match registry.records() {
        Ok(records) => Value::Array(
            records
                .into_iter()
                .filter(|record| record.parent == world.session)
                .map(|record| {
                    json!({
                        "child_id": record.id,
                        "session": record.session,
                        "state": record.state,
                        "started": record.started,
                        "ended": record.ended,
                        "result_path": record.result_path,
                    })
                })
                .collect(),
        )
        .to_string(),
        Err(said) => format!("failed: {said}"),
    }
}

/// One child this parent owns. Another chat's child is `missing` rather than readable: the registry is
/// gateway-wide, but a parent's reach is not.
fn owned(registry: &Registry, id: &str, world: &World) -> Result<(), String> {
    match registry.record(id) {
        Some(record) if record.parent == world.session => Ok(()),
        _ => Err(format!("missing: there is no child {id} of this session")),
    }
}

/// `result`: the child's checked JSON, or its status and why it has none. The wait is this tool's own
/// clock (see `Registry::wait`); a timeout says so and never claims the child was killed.
pub(crate) fn child_result(args: &Value, world: &World) -> String {
    let Some(id) = argument(args, "child_id") else {
        return "result needs a child_id".to_string();
    };
    let seconds = args
        .get("timeout_s")
        .and_then(Value::as_u64)
        .unwrap_or(60)
        .clamp(1, 600);
    let registry = match gateway_registry(world) {
        Ok(registry) => registry,
        Err(said) => return said,
    };
    if let Err(said) = owned(&registry, &id, world) {
        return said;
    }
    let record = match registry.wait(&id, std::time::Duration::from_secs(seconds)) {
        Ok(record) => record,
        Err(refusal) => return refusal_text(&refusal),
    };
    let text = match std::fs::read_to_string(&record.result_path) {
        Ok(text) => text,
        Err(error) => {
            return format!(
                "failed: the child is {}, but its result at {} could not be read: {error}",
                record.state, record.result_path
            );
        }
    };
    let held: Value = match serde_json::from_str(&text) {
        Ok(held) => held,
        Err(error) => {
            return format!(
                "failed: the child's result at {} is not JSON: {error}",
                record.result_path
            );
        }
    };
    let status = held
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("failed")
        .to_string();
    let mut out = json!({
        "child_id": record.id,
        "session": record.session,
        "state": record.state,
        "status": status,
        "result": held.get("result").cloned().unwrap_or(Value::Null),
    });
    // A result that is not an object has no checked value to hand back. The first line of `raw` is the
    // reason, which is what the parent can act on; the whole raw stays on disk in the artifact.
    if status != "ok"
        && let Some(object) = out.as_object_mut()
    {
        let raw = held.get("raw").and_then(Value::as_str).unwrap_or("");
        object.insert(
            "detail".to_string(),
            json!(
                raw.lines()
                    .next()
                    .unwrap_or("the child failed without a reason")
            ),
        );
        if let Some(reason) = &record.reason {
            object.insert("reason".to_string(), json!(reason));
        }
    }
    out.to_string()
}

/// `tell`: one message to a child that is still running. It is refused, not queued, once the child is
/// finished: the answer is already written, and a message nobody will read is a lie about influence.
fn tell_child(args: &Value, world: &World) -> String {
    let Some(id) = argument(args, "child_id") else {
        return "tell needs a child_id".to_string();
    };
    let Some(text) = argument(args, "text") else {
        return "tell needs text".to_string();
    };
    let registry = match gateway_registry(world) {
        Ok(registry) => registry,
        Err(said) => return said,
    };
    if let Err(said) = owned(&registry, &id, world) {
        return said;
    }
    match registry.tell(&id, &text) {
        Ok(record) => json!({
            "child_id": record.id,
            "session": record.session,
            "state": record.state,
            "delivered": true,
        })
        .to_string(),
        Err(refusal) => refusal_text(&refusal),
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
                        crate::mode::Mode::Plan => "read-only, then a plan",
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

/// The standing goal this chat is keeping. The chat is where the goal lives, so the tool reads it from
/// `world.chat` rather than taking an id: a model cannot close somebody else's goal by naming it.
///
/// `done` is a claim, not a close. The goal's own recorded check runs in the workspace, and a failing or
/// missing check leaves the goal open with the verdict recorded, which is the only honest way to keep the
/// work going after a model says it has finished.
fn goal_tool(args: &Value, world: &World) -> String {
    let state = &world.state;
    let Some(goal) = crate::gateway::goal::active(state, world.chat) else {
        return "there is no open or paused goal in this chat".to_string();
    };
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("status");
    match action {
        "status" => crate::gateway::goal::describe(&goal),
        "pause" => {
            let reason =
                argument(args, "reason").unwrap_or_else(|| "paused by the agent".to_string());
            match crate::gateway::goal::pause(state, goal.id, &reason, epoch()) {
                Ok(paused) => format!("goal {} is paused: {reason}", paused.id),
                Err(refusal) => {
                    format!(
                        "Refused ({}): {refusal}. The goal is unchanged.",
                        refusal.name()
                    )
                }
            }
        }
        "done" => {
            if goal.state != crate::gateway::goal::State::Open {
                return format!(
                    "goal {} is {}, so there is no open work to claim done",
                    goal.id,
                    goal.state.name()
                );
            }
            match crate::gateway::goal::complete(state, goal.id, &world.root, epoch()) {
                Ok(done) => format!(
                    "goal {} is done: the recorded check passed ({})",
                    done.id,
                    if done.check.is_empty() {
                        "no check".to_string()
                    } else {
                        done.check.clone()
                    }
                ),
                Err(refusal) => format!(
                    "Refused ({}): {refusal}. The goal stays open; keep working or say what is missing.",
                    refusal.name()
                ),
            }
        }
        other => format!("there is no goal action called {other}: say status, done or pause"),
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
    match crate::command::run_clean_in(
        &program,
        &world.root,
        6000,
        Some(&cage),
        Some(&scratch),
    ) {
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
        .arg("--")
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

/// Fetch a URL and return its text. The policy lives in [`crate::fetch`], which judges the URL
/// before any process runs and judges every redirect hop the same way; this seam passes the tool's
/// arguments in and the clip back out. `HADES_FETCH_ALLOW_PRIVATE=1` in the surface's environment
/// is the one explicit hole for private targets, and it stays an environment read precisely so a
/// test can exercise both sides of the gate without touching process state.
fn fetch(args: &Value) -> String {
    let url = args.get("url").and_then(Value::as_str).unwrap_or("");
    if url.is_empty() {
        return "no url given".to_string();
    }
    let allow_private = std::env::var("HADES_FETCH_ALLOW_PRIVATE").as_deref() == Ok("1");
    crate::fetch::run(url, 12_000, allow_private)
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

/// Read the local shelf. The library path is the state's own shelf, so a process started with a different
/// `--state` cannot reach another install's books.
fn library_tool(args: &Value, world: &World) -> String {
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("search");
    let open = || crate::library::Library::open(world.state.join("library"));
    match action {
        "list" => {
            let library = match open() {
                Ok(library) => library,
                Err(said) => return said,
            };
            match library.list() {
                Ok(entries) if entries.is_empty() => {
                    "the offline library is empty; add a file with `hades library add <path>`"
                        .into()
                }
                Ok(entries) => {
                    let mut text = String::new();
                    for entry in entries.iter().take(100) {
                        text.push_str(&format!(
                            "{}  {}  {}{}\n",
                            entry.id,
                            entry.title,
                            entry.format.label(),
                            entry
                                .pages
                                .map(|pages| format!("  {pages} pages"))
                                .unwrap_or_default()
                        ));
                    }
                    clip(&text, 8_000)
                }
                Err(said) => said,
            }
        }
        "read" => {
            let Some(id) = args.get("id").and_then(Value::as_str) else {
                return "library read needs an id from search or list".into();
            };
            let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
            let library = match open() {
                Ok(library) => library,
                Err(said) => return said,
            };
            match library.text(id) {
                Ok((entry, text)) => {
                    let total = text.chars().count();
                    if offset >= total {
                        return format!(
                            "{}: offset {offset} is past the end ({} characters)",
                            entry.id, total
                        );
                    }
                    let rest: String = text.chars().skip(offset).collect();
                    let shown = clip(&rest, 12_000);
                    if offset + shown.chars().count() < total {
                        format!(
                            "{}\n\n[showing characters {offset}..{} of {total}; call again with a larger offset]",
                            shown,
                            offset + shown.chars().count()
                        )
                    } else {
                        shown
                    }
                }
                Err(said) => said,
            }
        }
        _ => {
            let query = args.get("query").and_then(Value::as_str).unwrap_or("");
            if query.trim().is_empty() {
                return "library search needs a query".into();
            }
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(5)
                .min(25) as usize;
            let library = match open() {
                Ok(library) => library,
                Err(said) => return said,
            };
            match library.search(query, limit) {
                Ok(hits) if hits.is_empty() => "no library hits".into(),
                Ok(hits) => {
                    let mut text = String::new();
                    for hit in hits {
                        text.push_str(&format!(
                            "{}  {}{}  {}\n  {}\n",
                            hit.entry.id,
                            hit.entry.title,
                            hit.page
                                .map(|page| format!("  page {page}"))
                                .unwrap_or_default(),
                            hit.entry.format.label(),
                            hit.snippet
                        ));
                    }
                    clip(&text, 10_000)
                }
                Err(said) => said,
            }
        }
    }
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
pub fn strip_html(text: &str) -> String {
    // A page's visible text is not its byte stream. Script and style bodies are code, not prose, and
    // an inline JSON island is data, not a sentence. Drop those blocks before removing tags; a
    // naive tag stripper turns a JavaScript-heavy page into a wall of code that looks like evidence.
    let bytes = text.as_bytes();
    let mut out = String::new();
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'<' {
            if starts_with_ci(bytes, index, b"<!--") {
                index = find_ci(bytes, index + 4, b"-->").map_or(bytes.len(), |at| at + 3);
                continue;
            }
            let mut skip_to = None;
            for (open, close) in [
                (b"<script".as_slice(), b"</script".as_slice()),
                (b"<style".as_slice(), b"</style".as_slice()),
                (b"<noscript".as_slice(), b"</noscript".as_slice()),
                (b"<template".as_slice(), b"</template".as_slice()),
                (b"<svg".as_slice(), b"</svg".as_slice()),
                (b"<iframe".as_slice(), b"</iframe".as_slice()),
            ] {
                if starts_with_ci(bytes, index, open) {
                    let after_open = index + open.len();
                    skip_to = Some(match find_ci(bytes, after_open, close) {
                        Some(at) => find_byte(bytes, at + close.len(), b'>')
                            .map_or(bytes.len(), |end| end + 1),
                        // A self-closing block (`<svg ... />`) has no closing tag. Drop the tag,
                        // not the rest of the page.
                        None => {
                            find_byte(bytes, after_open, b'>').map_or(bytes.len(), |end| end + 1)
                        }
                    });
                    break;
                }
            }
            if let Some(next) = skip_to {
                index = next;
                continue;
            }

            let name = tag_name(bytes, index);
            if matches!(
                name.as_str(),
                "p" | "div"
                    | "br"
                    | "li"
                    | "tr"
                    | "h1"
                    | "h2"
                    | "h3"
                    | "h4"
                    | "h5"
                    | "h6"
                    | "section"
                    | "article"
                    | "header"
                    | "footer"
                    | "pre"
                    | "blockquote"
            ) {
                out.push('\n');
            } else {
                out.push(' ');
            }
            index = find_byte(bytes, index + 1, b'>').map_or(bytes.len(), |end| end + 1);
            continue;
        }
        let ch = text[index..].chars().next().expect("a char boundary");
        out.push(ch);
        index += ch.len_utf8();
    }

    let decoded = decode_entities(&out);
    let mut cleaned = String::new();
    let mut blank = 0;
    for line in decoded.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        cleaned.push_str(&line);
        cleaned.push('\n');
    }
    cleaned.trim_start_matches('\n').to_string()
}

fn starts_with_ci(bytes: &[u8], at: usize, needle: &[u8]) -> bool {
    bytes.len() >= at + needle.len()
        && bytes[at..at + needle.len()]
            .iter()
            .zip(needle)
            .all(|(left, right)| left.eq_ignore_ascii_case(right))
}

fn find_ci(bytes: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || from >= bytes.len() {
        return None;
    }
    (from..=bytes.len().saturating_sub(needle.len())).find(|at| starts_with_ci(bytes, *at, needle))
}

fn find_byte(bytes: &[u8], from: usize, byte: u8) -> Option<usize> {
    bytes
        .iter()
        .enumerate()
        .skip(from)
        .find(|(_, found)| **found == byte)
        .map(|(at, _)| at)
}

fn tag_name(bytes: &[u8], open: usize) -> String {
    let mut at = open + 1;
    if bytes.get(at) == Some(&b'/') {
        at += 1;
    }
    let mut name = String::new();
    while let Some(byte) = bytes.get(at) {
        if byte.is_ascii_alphanumeric() {
            name.push(byte.to_ascii_lowercase() as char);
            at += 1;
        } else {
            break;
        }
    }
    name
}

fn decode_entities(text: &str) -> String {
    text.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
}

/// Look at a picture with the local vision pair. The path answers to the same `World::policy` the other
/// file tools do, so an absolute or symlinked path outside the workspace is refused before anything is
/// opened, and `describe` reads the path the policy returns, not the one the model named.
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
    let resolved = match world.policy().read(Path::new(path)) {
        Ok(path) => path,
        Err(reason) => return format!("Refused: {reason}."),
    };
    if let Some(reason) = crate::floor::protected(&resolved.to_string_lossy(), &world.root) {
        return format!("Refused: {reason}. A chat does not read the live install.");
    }
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
    if action == "path" {
        let name = args.get("name").and_then(Value::as_str).unwrap_or("");
        let Some((_, path)) = found.iter().find(|(skill, _)| skill == name) else {
            return format!("there is no skill called {name}");
        };
        return match path.parent() {
            Some(folder) => folder.display().to_string(),
            None => format!("{} has no folder", path.display()),
        };
    }
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

/// The head, a cut marker with the count, and the tail. The marker is `[... N characters cut ...]`,
/// which a reader reads as prose and a tally reads as one line.
pub fn clip(text: &str, max: usize) -> String {
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
    use crate::approval::ApprovalBroker as _;
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
    fn child_prompt_is_byte_identical_to_the_pre_composer_format() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {"answer": {"type": "string"}},
            "required": ["answer"]
        });
        let expected = format!(
            "{WORKER}\n\nReturn one JSON object and nothing else. It must satisfy this schema:\n{}\n\
             Do not put prose around it. Do not use Markdown fences. A missing or mistyped field fails \
             the task.",
            serde_json::to_string_pretty(&schema).unwrap()
        );
        assert_eq!(child_prompt(&schema), expected);
    }

    #[test]
    fn fan_out_siblings_share_the_worker_preamble_byte_for_byte() {
        let one = child_prompt(&serde_json::json!({"type": "object"}));
        let two = child_prompt(&serde_json::json!({"type": "array"}));
        let preamble = format!("{WORKER}\n\n");
        assert!(one.starts_with(&preamble), "{one}");
        assert!(two.starts_with(&preamble), "{two}");
        // Only the block after the preamble differs, so a cached prefix survives across siblings.
        assert_ne!(one, two);
        assert_ne!(one[preamble.len()..], two[preamble.len()..]);
    }

    /// Every object key, sorted, at every depth. serde_json's default map is a BTreeMap, which is why the
    /// serialization is already canonical; this pins that against a future `preserve_order` feature or a
    /// switch to a map that keeps insertion order.
    fn canonical(value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                let mut out = serde_json::Map::new();
                for key in keys {
                    out.insert(key.clone(), canonical(&map[key]));
                }
                Value::Object(out)
            }
            Value::Array(items) => Value::Array(items.iter().map(canonical).collect()),
            other => other.clone(),
        }
    }

    #[test]
    fn the_tool_surface_is_byte_stable_and_key_sorted() {
        for depth in [0u8, 1] {
            for mode in [Mode::Read, Mode::Act] {
                for role in [crate::profile::Role::Survey, crate::profile::Role::Patch] {
                    let one = schemas_for(depth, mode, role);
                    let two = schemas_for(depth, mode, role);
                    let first = serde_json::to_string(&one).expect("a schema");
                    let second = serde_json::to_string(&two).expect("a schema");
                    assert_eq!(first, second, "the tool surface changed between calls");
                    assert_eq!(
                        first,
                        serde_json::to_string(&canonical(&one)).expect("canonical"),
                        "the serialization is not key-sorted"
                    );
                    assert_eq!(names(&one), names(&two), "the tool order is not stable");
                }
            }
        }
    }

    #[test]
    fn changes_travel_as_appended_messages_not_a_rewritten_prefix() {
        let dir = admission_scratch("agent-prefix-stable");
        let (url, seen, server) = free_script(vec![
            json!({"tool_calls": [{"index": 0, "id": "c1", "function": {"name": "list_dir", "arguments": "{\"path\":\".\"}"}}]}),
            answer("done"),
        ]);
        let world = world_at(&dir, Mode::Act, "chat:7");
        let client = OpenAi::new(&url, "stub");
        let mut messages = vec![
            json!({"role": "system", "content": "You are terse."}),
            json!({"role": "user", "content": "go"}),
        ];
        let said = answer_stoppable(
            &client,
            &mut messages,
            &world,
            &|_, _| {},
            &|_, _| {},
            4,
            None,
        )
        .expect("an answer");
        assert_eq!(said.as_deref(), Some("{\"report\":\"done\"}"));
        let first = seen
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the first call");
        let second = seen
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the second call");
        assert_eq!(
            first[0], second[0],
            "the system message was rewritten mid-run"
        );
        assert_eq!(first[0]["content"], "You are terse.");
        // What changed travels behind the prefix: one assistant call and one tool result were appended.
        assert_eq!(second.len(), first.len() + 2, "{second:?}");
        assert_eq!(second[1..first.len()], first[1..], "{second:?}");
        server.join().expect("the endpoint");
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
            "library",
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
        let dir = crate::scratch::root().join("agent-read");
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
            session: "parent-session".to_string(),
            inbox: None,
            mcp: std::sync::Arc::new(crate::mcp::Mcp::empty()),
            allowance: crate::agent::Allowance::new(64),
            questions: None,
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
        let dir = crate::scratch::root().join("agent-patch");
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
        let base = crate::scratch::root().join("agent-cage");
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
            session: "parent-session".to_string(),
            inbox: None,
            mcp: std::sync::Arc::new(crate::mcp::Mcp::empty()),
            allowance: crate::agent::Allowance::new(64),
            questions: None,
        }
    }

    #[test]
    fn the_chat_seam_writes_one_line_for_every_decision() {
        let dir = crate::scratch::root().join("agent-activity");
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
        let dir = crate::scratch::root().join("agent-mcp-blocked");
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
            session: "parent-session".to_string(),
            inbox: None,
            mcp: mcp_with(&dir, &["echo"]),
            allowance: crate::agent::Allowance::new(64),
            questions: None,
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
        let dir = crate::scratch::root().join("agent-mcp-allowed");
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
            session: "parent-session".to_string(),
            inbox: None,
            mcp: mcp_with(&dir, &["echo"]),
            allowance: crate::agent::Allowance::new(64),
            questions: None,
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
        let dir = crate::scratch::root().join("agent-supersede");
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
            session: "parent-session".to_string(),
            inbox: None,
            mcp: std::sync::Arc::new(crate::mcp::Mcp::empty()),
            allowance: crate::agent::Allowance::new(64),
            questions: None,
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
        let dir = crate::scratch::root().join("agent-job");
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
            session: "parent-session".to_string(),
            inbox: None,
            mcp: std::sync::Arc::new(crate::mcp::Mcp::empty()),
            allowance: crate::agent::Allowance::new(64),
            questions: None,
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
        let dir = crate::scratch::root().join("agent-floor");
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
            session: "parent-session".to_string(),
            inbox: None,
            mcp: std::sync::Arc::new(crate::mcp::Mcp::empty()),
            allowance: crate::agent::Allowance::new(64),
            questions: None,
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
        let dir = crate::scratch::root().join("agent-hooks");
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
        let dir = crate::scratch::root().join("agent-delegate");
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
        // Two logs now: the child's, and the parent's child-lifecycle log that names it.
        assert_eq!(logs.len(), 2, "the child has its own log: {logs:?}");
        let text = logs
            .iter()
            .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
            .find(|text| text.contains("\"delegate\""))
            .expect("the child's log says what it is: a delegate");
        assert!(text.contains("parent_session"), "{text}");
    }

    #[test]
    fn archived_skills_are_not_offered_and_one_name_resolves_once() {
        let dir = crate::scratch::root().join("agent-skills");
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
        let dir = crate::scratch::root().join("agent-role");
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
        let dir = crate::scratch::root().join("agent-budget");
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
        let dir = crate::scratch::root().join("agent-spawn-depth");
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
        let dir = crate::scratch::root().join("agent-spawn-budget");
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
        let dir = crate::scratch::root().join("agent-spawn-fail");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a dir");
        let world = world_at(&dir, Mode::Act, "chat:7");
        let client = OpenAi::new("http://127.0.0.1:9/v1/chat/completions", "none");
        let said = handle_of(&spawn(
            &json!({"goal": "write down one fact", "steps": 2}),
            &world,
            &client,
            &|_, _| {},
        ));
        assert_eq!(said["status"], "failed", "{said}");
        assert!(
            said["artifact"]
                .as_str()
                .is_some_and(|path| path.ends_with("result.json")),
            "{said}"
        );
        assert!(
            said["raw"]
                .as_str()
                .is_some_and(|raw| raw.contains("Model")),
            "the failure keeps its class: {said}"
        );
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
        let base = crate::scratch::root().join(name);
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

    /// The shots directory is a read root only when it is the state directory's own child. A
    /// symlink at `<state>/shots` is not followed: the policy's second root must not be a door to
    /// wherever the link was pointed.
    #[test]
    #[cfg(unix)]
    fn a_symlinked_shots_directory_is_not_a_read_root() {
        let (base, world) = world_beside_outside("agent-policy-shots-link");
        let outside = base.join("away/shots");
        std::fs::create_dir_all(&outside).expect("a directory outside the state");
        std::fs::create_dir_all(&world.state).expect("a state directory");
        std::os::unix::fs::symlink(&outside, world.state.join("shots")).expect("the symlink");

        let policy = world.policy();
        let outside = std::fs::canonicalize(&outside).expect("the outside directory");
        assert!(
            !policy.read_roots.contains(&outside),
            "the policy followed the symlink: {:?}",
            policy.read_roots
        );
        assert_eq!(
            policy.read_roots.len(),
            1,
            "a symlinked shots adds no read root: {:?}",
            policy.read_roots
        );

        // The same name as a real directory is admitted, canonical, as a direct child of state.
        std::fs::remove_file(world.state.join("shots"))
            .expect("the link is removed, not its target");
        std::fs::create_dir_all(world.state.join("shots")).expect("a real shots directory");
        let policy = world.policy();
        let expected = std::fs::canonicalize(world.state.join("shots")).expect("the shots dir");
        assert!(
            policy.read_roots.contains(&expected),
            "a real shots directory is a read root: {:?}",
            policy.read_roots
        );
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

    /// The `image` tool's two gates without a vision server behind it: the probe runs the policy check
    /// and the floor check on the tool's own code path order and reports the path they would hand
    /// `describe`, or `None` when the call would be refused before vision is reached.
    fn image_probe_path(args: &Value, world: &World) -> Option<PathBuf> {
        let path = args.get("path").and_then(Value::as_str).unwrap_or("");
        if path.is_empty() {
            return None;
        }
        let _question = args
            .get("question")
            .and_then(Value::as_str)
            .unwrap_or("Describe this image precisely. Read any text it contains.")
            .to_string();
        match world.policy().read(Path::new(path)) {
            Err(_) => None,
            Ok(resolved)
                if crate::floor::protected(&resolved.to_string_lossy(), &world.root).is_some() =>
            {
                None
            }
            Ok(resolved) => Some(resolved),
        }
    }

    #[test]
    fn an_image_of_an_absolute_path_outside_the_root_is_refused_before_vision() {
        let (base, world) = world_beside_outside("agent-policy-image");
        let file = outside_of(&base, "picture.txt");
        let said = execute(
            "image",
            &json!({ "path": file.display().to_string() }).to_string(),
            &world,
            &OpenAi::new("http://127.0.0.1:9/v1/chat/completions", "none"),
            &|_, _| {},
        );
        assert!(said.starts_with("Refused:"), "{said}");
        assert!(said.contains("outside the permitted read roots"), "{said}");
        assert!(
            image_probe_path(&json!({ "path": file.display().to_string() }), &world).is_none(),
            "the policy gate must answer before the vision pair is reached"
        );
    }

    #[test]
    fn an_image_inside_the_root_resolves_the_way_describe_will_read_it() {
        let (_, world) = world_beside_outside("agent-policy-image-inside");
        std::fs::write(world.root.join("shot.png"), "not really pixels")
            .expect("a picture inside the workspace");
        let resolved = image_probe_path(&json!({ "path": "shot.png" }), &world)
            .expect("an in-root image passes both gates");
        assert_eq!(
            resolved,
            std::fs::canonicalize(world.root.join("shot.png")).expect("the file")
        );
        // No binary fixture: nothing is decoded here. Removing the file after the gates pass and
        // reading `describe`'s error tells the test exactly which path it was handed.
        std::fs::remove_file(&resolved).expect("the picture was removed");
        let said = crate::vision::describe(&resolved, "no server here").expect_err("no such file");
        assert!(
            said.starts_with(&format!("could not read {}", resolved.display())),
            "describe read the resolved path, not the model's word: {said}"
        );
        assert!(
            !said.contains("server"),
            "describe failed at the file read, not at the vision server: {said}"
        );
    }

    #[test]
    fn a_search_pattern_beginning_with_a_dash_is_a_pattern_not_a_flag() {
        let (_, world) = world_beside_outside("agent-policy-search-dash");
        std::fs::write(world.root.join("note.txt"), "the -v line stands here\n")
            .expect("a file to search");
        let said = execute(
            "search",
            &json!({ "pattern": "-v", "path": "." }).to_string(),
            &world,
            &OpenAi::new("http://127.0.0.1:9/v1/chat/completions", "none"),
            &|_, _| {},
        );
        assert!(
            said.contains("-v line stands"),
            "a leading-dash pattern must be searched as text: {said}"
        );
    }
    // ---- A2 to A5: the admission handle, its tools, and the session invariant --------------------

    /// Read one HTTP request and return its JSON body. The test endpoints below speak just enough HTTP
    /// for the harness's streaming client, and share this so two copies cannot drift.
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

    /// Answer one chat call as one streamed delta, then close.
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

    /// The children on disk at one moment, as `{id, state}`. A test can then say what the registry
    /// looked like *inside* a model call rather than after it.
    fn children_at(state: &Path) -> Vec<Value> {
        std::fs::read_dir(crate::gateway::children::children_dir(state))
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
                    .filter_map(|text| serde_json::from_str::<Value>(&text).ok())
                    .map(|record| json!({"id": record["id"], "state": record["state"]}))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// A scripted model endpoint: one connection and one answer per script entry. Each request reports
    /// the messages it was sent and the children on disk at that instant, then holds the answer until
    /// the test releases it. The barrier is the only clock these tests use: "the handle came back while
    /// the child was busy" is proven by the endpoint still holding the answer, not by a sleep.
    struct Stub {
        url: String,
        seen: std::sync::mpsc::Receiver<(Vec<Value>, Vec<Value>)>,
        release: std::sync::mpsc::Sender<()>,
        server: std::thread::JoinHandle<()>,
    }

    impl Stub {
        /// The next request, reported with what the registry looked like when it arrived.
        fn next(&self) -> (Vec<Value>, Vec<Value>) {
            self.seen
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("a model call")
        }

        fn go(&self) {
            self.release.send(()).expect("the endpoint is waiting");
        }
    }

    /// An endpoint that answers at once. A blocking surface waits for the answer inside `spawn`, so a
    /// test holding the answer would deadlock itself; this is the same child without the barrier.
    fn eager(answer: Value) -> (String, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a port");
        let url = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().expect("an address")
        );
        let server = std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let _ = read_request(&mut stream);
            write_answer(&mut stream, &answer);
        });
        (url, server)
    }

    /// An endpoint that answers each script entry without a barrier: for tests that drive one synchronous
    /// call through several steps and read the requests only after it returns.
    fn free_script(
        answers: Vec<Value>,
    ) -> (
        String,
        std::sync::mpsc::Receiver<Vec<Value>>,
        std::thread::JoinHandle<()>,
    ) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a port");
        let url = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().expect("an address")
        );
        let (report, requests) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            for answer in answers {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let request = read_request(&mut stream);
                let messages = request
                    .get("messages")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                if report.send(messages).is_err() {
                    return;
                }
                write_answer(&mut stream, &answer);
            }
        });
        (url, requests, server)
    }

    fn script(state: PathBuf, answers: Vec<Value>) -> Stub {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a port");
        let url = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().expect("an address")
        );
        let (report, requests) = std::sync::mpsc::channel();
        let (release, go) = std::sync::mpsc::channel::<()>();
        let server = std::thread::spawn(move || {
            for answer in answers {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let request = read_request(&mut stream);
                let messages = request
                    .get("messages")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                if report.send((messages, children_at(&state))).is_err() {
                    return;
                }
                if go.recv_timeout(std::time::Duration::from_secs(10)).is_err() {
                    return;
                }
                write_answer(&mut stream, &answer);
            }
        });
        Stub {
            url,
            seen: requests,
            release,
            server,
        }
    }

    /// A gate every held answer waits behind. A test opens it once and all the held children proceed
    /// together, which is the closest a test gets to "at the same instant" without a clock.
    #[derive(Default)]
    struct Gate {
        open: std::sync::Mutex<bool>,
        ready: std::sync::Condvar,
    }

    impl Gate {
        fn new() -> std::sync::Arc<Gate> {
            std::sync::Arc::new(Gate::default())
        }

        fn release(&self) {
            *self.open.lock().expect("the gate") = true;
            self.ready.notify_all();
        }

        fn wait(&self) {
            let mut open = self.open.lock().expect("the gate");
            while !*open {
                open = self.ready.wait(open).expect("the gate");
            }
        }
    }

    /// A model endpoint that accepts `count` connections at once, holds every answer behind the gate,
    /// and answers them all when it opens. Used for the cap and for the concurrent parent-log writes.
    fn farm(
        answer: Value,
        count: usize,
        gate: std::sync::Arc<Gate>,
    ) -> (
        String,
        std::sync::mpsc::Receiver<()>,
        std::thread::JoinHandle<()>,
    ) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a port");
        let url = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().expect("an address")
        );
        let (entered, arrivals) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let mut handlers = Vec::new();
            for _ in 0..count {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                let answer = answer.clone();
                let gate = std::sync::Arc::clone(&gate);
                let entered = entered.clone();
                handlers.push(std::thread::spawn(move || {
                    let _ = read_request(&mut stream);
                    let _ = entered.send(());
                    gate.wait();
                    write_answer(&mut stream, &answer);
                }));
            }
            for handler in handlers {
                let _ = handler.join();
            }
        });
        (url, arrivals, server)
    }

    /// A scratch directory under the repo's target, cleared first, so two runs cannot share a registry.
    fn admission_scratch(name: &str) -> PathBuf {
        let dir = crate::scratch::root().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("state")).expect("a scratch state");
        dir
    }

    fn answer(report: &str) -> Value {
        json!({"content": json!({"report": report}).to_string()})
    }

    fn handle_of(said: &str) -> Value {
        serde_json::from_str(said)
            .unwrap_or_else(|error| panic!("spawn did not return an object: {error}: {said}"))
    }

    /// A2: the handle is written before the first model call, and it comes back while the child is
    /// provably blocked. The endpoint reads the record while the call is in flight, so a record written
    /// later cannot pass; the endpoint holds the answer, so a child that finished before the handle
    /// cannot pass either.
    #[test]
    fn spawn_returns_a_handle_while_the_child_is_blocked_at_the_model() {
        let dir = admission_scratch("agent-spawn-admit");
        let state = dir.join("state");
        let registry = crate::gateway::children::Registry::new(&state);
        let stub = script(state.clone(), vec![answer("done")]);
        let world = world_at(&dir, Mode::Act, "chat:7");
        let said = spawn(
            &json!({"goal": "say done"}),
            &world,
            &OpenAi::new(&stub.url, "stub"),
            &|_, _| {},
        );

        let handle = handle_of(&said);
        let id = handle["child_id"].as_str().expect("a child id").to_string();
        assert_eq!(handle["state"], "admitted", "{handle}");
        assert_eq!(handle["session"], handle["child_id"]);
        let mut keys: Vec<String> = handle
            .as_object()
            .expect("an object")
            .keys()
            .cloned()
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            ["child_id", "session", "state"],
            "a handle is not a result: {handle}"
        );

        // The call has arrived and is being held. Its own report says the record already existed and
        // said `running` when it got there, which is the claim "before the first model call" made
        // observable from inside the call.
        let (messages, at_call) = stub.next();
        assert_eq!(at_call.len(), 1, "one child, one record: {at_call:?}");
        assert_eq!(at_call[0]["id"], id, "{at_call:?}");
        assert_eq!(at_call[0]["state"], "running", "{at_call:?}");
        assert!(
            messages.iter().any(|message| {
                message["content"]
                    .as_str()
                    .is_some_and(|text| text.contains("say done"))
            }),
            "the goal reaches the child: {messages:?}"
        );
        assert_eq!(
            registry.live(),
            1,
            "the child is still live while the model is blocked"
        );

        stub.go();
        let collected: Value = serde_json::from_str(&child_result(
            &json!({"child_id": id, "timeout_s": 10}),
            &world,
        ))
        .expect("a result object");
        assert_eq!(collected["status"], "ok", "{collected}");
        assert_eq!(collected["result"]["report"], "done", "{collected}");
        assert_eq!(registry.drain(), 1);
        stub.server.join().expect("the endpoint");
    }

    /// A2: the cap is the gateway's, and it refuses through the tool rather than starting a
    /// seventeenth thread. Every child is held inside its first call, so none can free a slot.
    #[test]
    fn a_seventeenth_live_child_is_refused_busy_by_the_spawn_tool() {
        let dir = admission_scratch("agent-spawn-cap");
        let state = dir.join("state");
        let registry = crate::gateway::children::Registry::new(&state);
        let limit = crate::gateway::children::MAX_LIVE;
        let gate = Gate::new();
        let (url, entered, farm) = farm(answer("done"), limit, std::sync::Arc::clone(&gate));
        for index in 0..limit {
            let id = format!("held-{index}");
            registry
                .admit(
                    Admission {
                        id: id.clone(),
                        parent: "parent-session".to_string(),
                        session: id,
                        chat: 7,
                        goal: "hold".to_string(),
                        schema: Admission::report_schema(),
                        steps: 2,
                    },
                    world_at(&dir, Mode::Act, "chat:7"),
                    OpenAi::new(&url, "stub"),
                )
                .expect("an admitted child");
        }
        for _ in 0..limit {
            entered
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("a held model call");
        }
        assert_eq!(registry.live(), limit);

        let world = world_at(&dir, Mode::Act, "chat:7");
        let refused = spawn(
            &json!({"goal": "one too many"}),
            &world,
            &OpenAi::new(&url, "stub"),
            &|_, _| {},
        );
        assert!(
            refused.starts_with("busy: "),
            "a typed busy fault: {refused}"
        );
        assert!(
            !refused.contains("child_id"),
            "a refused admission is not a handle: {refused}"
        );
        assert!(
            registry.records().expect("records").len() == limit,
            "no record for the refused child"
        );
        assert_eq!(registry.live(), limit, "the refusal took no slot");

        gate.release();
        assert_eq!(registry.drain(), limit);
        assert_eq!(registry.live(), 0);
        farm.join().expect("the endpoint");
    }

    /// A3: a message reaches a running child at its next step. The first call is held, the parent
    /// tells while it is in flight, and the second request carries the message and nothing before it.
    #[test]
    fn a_running_child_reads_a_told_message_on_its_next_turn() {
        let dir = admission_scratch("agent-tell");
        let state = dir.join("state");
        let registry = crate::gateway::children::Registry::new(&state);
        let stub = script(
            state.clone(),
            vec![
                json!({"tool_calls": [{"index": 0, "id": "c1", "function": {"name": "list_dir", "arguments": "{\"path\":\".\"}"}}]}),
                answer("done"),
            ],
        );
        let world = world_at(&dir, Mode::Act, "chat:7");
        let handle = handle_of(&spawn(
            &json!({"goal": "say done", "steps": 4}),
            &world,
            &OpenAi::new(&stub.url, "stub"),
            &|_, _| {},
        ));
        let id = handle["child_id"].as_str().expect("a child id").to_string();

        let (first, _) = stub.next();
        assert!(
            !first.iter().any(|message| {
                message["content"]
                    .as_str()
                    .is_some_and(|text| text.contains("keep going"))
            }),
            "the message is not in a request that went out before it was sent: {first:?}"
        );
        let told = tell_child(&json!({"child_id": id, "text": "keep going"}), &world);
        assert!(told.contains("\"delivered\":true"), "{told}");

        stub.go();
        let (second, _) = stub.next();
        assert!(
            second.iter().any(|message| {
                message["content"]
                    .as_str()
                    .is_some_and(|text| text.contains("keep going"))
            }),
            "the next turn carries the message: {second:?}"
        );
        stub.go();

        let collected: Value = serde_json::from_str(&child_result(
            &json!({"child_id": id, "timeout_s": 10}),
            &world,
        ))
        .expect("a result object");
        assert_eq!(collected["status"], "ok", "{collected}");
        assert_eq!(registry.drain(), 1);
        stub.server.join().expect("the endpoint");
    }

    /// A3: a finished child refuses a message instead of queueing one nobody reads.
    #[test]
    fn tell_on_a_finished_child_is_refused_rather_than_queued() {
        let dir = admission_scratch("agent-tell-finished");
        let state = dir.join("state");
        let registry = crate::gateway::children::Registry::new(&state);
        let stub = script(state.clone(), vec![answer("done")]);
        let world = world_at(&dir, Mode::Act, "chat:7");
        let handle = handle_of(&spawn(
            &json!({"goal": "say done"}),
            &world,
            &OpenAi::new(&stub.url, "stub"),
            &|_, _| {},
        ));
        let id = handle["child_id"].as_str().expect("a child id").to_string();
        stub.next();
        stub.go();
        let collected: Value = serde_json::from_str(&child_result(
            &json!({"child_id": id, "timeout_s": 10}),
            &world,
        ))
        .expect("a result object");
        assert_eq!(collected["state"], "done", "{collected}");

        let refused = tell_child(&json!({"child_id": id, "text": "one more thing"}), &world);
        assert!(
            refused.starts_with("finished: "),
            "a finished child refuses: {refused}"
        );
        // Not queued either: the child's log records the admission and the result, and no message.
        let store = session_tree::FileStore::new(state.join("sessions"));
        let log = session_tree::Session::load(
            store,
            session_tree::SystemClock,
            session_tree::SessionId::new(&id),
        )
        .expect("the child log");
        let kinds: Vec<&str> = crate::gateway::children::events(log.entries())
            .iter()
            .map(|event| event.type_name())
            .collect();
        assert_eq!(kinds, ["child_admitted", "child_result"]);
        assert_eq!(registry.drain(), 1);
        stub.server.join().expect("the endpoint");
    }

    /// A3: `result` waits on its own wall clock and reports a typed timeout at the parent's deadline,
    /// without killing the child. The Bounds are checked at graph edges and cannot interrupt a call in
    /// flight, so this deadline cannot be theirs.
    #[test]
    fn result_times_out_typed_at_the_parents_deadline_and_the_child_survives() {
        let dir = admission_scratch("agent-result-timeout");
        let state = dir.join("state");
        let registry = crate::gateway::children::Registry::new(&state);
        let stub = script(state.clone(), vec![answer("done")]);
        let world = world_at(&dir, Mode::Act, "chat:7");
        let handle = handle_of(&spawn(
            &json!({"goal": "say done"}),
            &world,
            &OpenAi::new(&stub.url, "stub"),
            &|_, _| {},
        ));
        let id = handle["child_id"].as_str().expect("a child id").to_string();
        stub.next();

        let started = std::time::Instant::now();
        let timed_out = child_result(&json!({"child_id": id, "timeout_s": 1}), &world);
        assert!(
            timed_out.starts_with("timeout: "),
            "a typed timeout: {timed_out}"
        );
        assert!(
            timed_out.contains("not killed"),
            "a timeout is not a kill: {timed_out}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "the deadline is the parent's, not the client's 180 s"
        );
        assert_eq!(
            registry.live(),
            1,
            "the child survives the parent's impatience"
        );

        stub.go();
        let collected: Value = serde_json::from_str(&child_result(
            &json!({"child_id": id, "timeout_s": 10}),
            &world,
        ))
        .expect("a result object");
        assert_eq!(collected["result"]["report"], "done", "{collected}");
        assert_eq!(registry.drain(), 1);
        stub.server.join().expect("the endpoint");
    }

    /// A4: the same scripted child, once with no gateway and once admitted. The typed value is the
    /// same object; the shapes differ because a handle is not a result.
    #[test]
    fn the_blocking_path_and_the_registry_agree_on_a_childs_typed_value() {
        let blocked = admission_scratch("agent-spawn-blocking");
        let (url, server) = eager(answer("same"));
        let world = world_at(&blocked, Mode::Act, "chat:7");
        let blocking = handle_of(&spawn(
            &json!({"goal": "say done"}),
            &world,
            &OpenAi::new(&url, "stub"),
            &|_, _| {},
        ));
        server.join().expect("the endpoint");
        assert_eq!(blocking["status"], "ok", "{blocking}");

        let admitted = admission_scratch("agent-spawn-registry");
        let state = admitted.join("state");
        let registry = crate::gateway::children::Registry::new(&state);
        let stub = script(state.clone(), vec![answer("same")]);
        let world = world_at(&admitted, Mode::Act, "chat:7");
        let handle = handle_of(&spawn(
            &json!({"goal": "say done"}),
            &world,
            &OpenAi::new(&stub.url, "stub"),
            &|_, _| {},
        ));
        let id = handle["child_id"].as_str().expect("a child id").to_string();
        stub.next();
        stub.go();
        let collected: Value = serde_json::from_str(&child_result(
            &json!({"child_id": id, "timeout_s": 10}),
            &world,
        ))
        .expect("a result object");

        assert_eq!(
            blocking["result"], collected["result"],
            "one child, one value"
        );
        assert_eq!(blocking["status"], collected["status"]);
        assert!(
            blocking.get("artifact").is_some() && blocking.get("child_id").is_none(),
            "the blocking path returns a finished outcome: {blocking}"
        );
        assert!(
            collected.get("child_id").is_some() && collected.get("artifact").is_none(),
            "the registry path returns a collected result: {collected}"
        );
        assert_eq!(registry.drain(), 1);
        stub.server.join().expect("the endpoint");
    }

    /// A5: walking the parent's log finds every child that was admitted, each event names its child
    /// session, each child's own log names the parent, and no event names a child with no record.
    #[test]
    fn a_parent_log_names_every_child_it_admitted_and_no_orphan() {
        let dir = admission_scratch("agent-parent-log");
        let state = dir.join("state");
        let parent = "parent-session";
        // A real parent session with history, so the walk is a walk of a log that already existed.
        {
            let store = session_tree::FileStore::new(state.join("sessions"));
            let mut log = session_tree::Session::create(
                store,
                session_tree::SystemClock,
                session_tree::SessionId::new(parent),
            )
            .expect("the parent log");
            log.append(session_tree::Kind::User, json!({"text": "spawn three"}))
                .expect("the parent's request");
        }
        let registry = crate::gateway::children::Registry::new(&state);
        let stub = script(
            state.clone(),
            vec![answer("one"), answer("two"), answer("three")],
        );
        let world = world_at(&dir, Mode::Act, "chat:7");
        let mut ids = Vec::new();
        for goal in ["first", "second", "third"] {
            let handle = handle_of(&spawn(
                &json!({"goal": goal}),
                &world,
                &OpenAi::new(&stub.url, "stub"),
                &|_, _| {},
            ));
            ids.push(handle["child_id"].as_str().expect("a child id").to_string());
        }
        for _ in 0..3 {
            stub.next();
            stub.go();
        }
        for id in &ids {
            let collected: Value = serde_json::from_str(&child_result(
                &json!({"child_id": id, "timeout_s": 10}),
                &world,
            ))
            .expect("a result object");
            assert_eq!(collected["status"], "ok", "{collected}");
        }
        assert_eq!(registry.drain(), 3);
        stub.server.join().expect("the endpoint");

        let store = session_tree::FileStore::new(state.join("sessions"));
        let log = session_tree::Session::load(
            store.clone(),
            session_tree::SystemClock,
            session_tree::SessionId::new(parent),
        )
        .expect("the parent log");
        let walked = crate::gateway::children::events(log.entries());
        let admitted: Vec<&crate::gateway::children::Event> = walked
            .iter()
            .filter(|event| event.type_name() == "child_admitted")
            .collect();
        let finished: Vec<&crate::gateway::children::Event> = walked
            .iter()
            .filter(|event| event.type_name() == "child_result")
            .collect();
        assert_eq!(admitted.len(), 3, "{walked:?}");
        assert_eq!(finished.len(), 3, "{walked:?}");
        for event in &admitted {
            let crate::gateway::children::Event::Admitted {
                child_id,
                parent_session,
                session,
                ..
            } = event
            else {
                panic!("a non-admission event where the walk expects one");
            };
            assert_eq!(parent_session, parent);
            assert!(
                ids.contains(child_id),
                "{child_id} is not a child we spawned"
            );
            let record = registry.record(child_id).expect("a record for every child");
            assert_eq!(
                &record.session, session,
                "the event names the child session"
            );
            assert_eq!(record.parent, parent);
            assert_eq!(record.state, "done");
            // And the child's own log says where it came from, so the walk works from either end.
            let child_log = session_tree::Session::load(
                store.clone(),
                session_tree::SystemClock,
                session_tree::SessionId::new(session),
            )
            .expect("the child log");
            let own = crate::gateway::children::events(child_log.entries());
            assert!(!own.is_empty(), "a child log with no events is an orphan");
            for event in own {
                match event {
                    crate::gateway::children::Event::Admitted { parent_session, .. }
                    | crate::gateway::children::Event::Message { parent_session, .. }
                    | crate::gateway::children::Event::Result { parent_session, .. } => {
                        assert_eq!(parent_session, parent)
                    }
                }
            }
        }
        // No orphans the other way either: every record names this parent and is named in the log.
        let records = registry.records().expect("records");
        assert_eq!(records.len(), 3, "{records:?}");
        for record in records {
            assert_eq!(record.parent, parent);
            assert!(
                admitted.iter().any(|event| matches!(
                    event,
                    crate::gateway::children::Event::Admitted { child_id, .. } if child_id == &record.id
                )),
                "{} has a record but no admission in the parent log",
                record.id
            );
        }
    }

    /// A5: several children finish at once and every one appends to the parent log from its own
    /// thread. The session tree numbers an entry from the handle's length, so without one writer per
    /// session at a time two children would write the same id and the log would refuse to load.
    #[test]
    fn children_finishing_at_once_append_to_one_parent_log_without_colliding() {
        let dir = admission_scratch("agent-parent-log-race");
        let state = dir.join("state");
        let registry = crate::gateway::children::Registry::new(&state);
        let count = 8;
        let gate = Gate::new();
        let (url, entered, farm) = farm(answer("done"), count, std::sync::Arc::clone(&gate));
        let mut ids = Vec::new();
        for index in 0..count {
            let id = format!("race-{index}");
            registry
                .admit(
                    Admission {
                        id: id.clone(),
                        parent: "parent-session".to_string(),
                        session: id.clone(),
                        chat: 7,
                        goal: "say done".to_string(),
                        schema: Admission::report_schema(),
                        steps: 2,
                    },
                    world_at(&dir, Mode::Act, "chat:7"),
                    OpenAi::new(&url, "stub"),
                )
                .expect("an admitted child");
            ids.push(id);
        }
        for _ in 0..count {
            entered
                .recv_timeout(std::time::Duration::from_secs(30))
                .expect("a held model call");
        }
        gate.release();
        assert_eq!(registry.drain(), count);
        farm.join().expect("the endpoint");

        let store = session_tree::FileStore::new(state.join("sessions"));
        let log = session_tree::Session::load(
            store,
            session_tree::SystemClock,
            session_tree::SessionId::new("parent-session"),
        )
        .expect("the parent log parses: no two children wrote the same entry id");
        let ids_on_disk: Vec<u64> = log.entries().iter().map(|entry| entry.id.0).collect();
        assert_eq!(
            ids_on_disk,
            (0..ids_on_disk.len() as u64).collect::<Vec<_>>(),
            "the log is one unbroken chain"
        );
        let walked = crate::gateway::children::events(log.entries());
        assert_eq!(
            walked.len(),
            count * 2,
            "every child is admitted and reported: {walked:?}"
        );
        for record in registry.records().expect("records") {
            assert_eq!(record.state, "done", "{} did not finish", record.id);
        }
        for id in &ids {
            assert!(
                walked.iter().any(|event| matches!(
                    event,
                    crate::gateway::children::Event::Result { child_id, .. } if child_id == id
                )),
                "{id} has no result in the parent log"
            );
        }
    }

    // ---- B3: a done claim is not a close until the recorded check passes --------------------------

    #[test]
    fn a_done_claim_with_a_failing_check_leaves_the_goal_open_and_records_it() {
        let dir = crate::scratch::root().join("agent-goal-done");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a dir");
        let world = world_at(&dir, Mode::Act, "chat:7");
        let new = crate::gateway::goal::NewGoal {
            text: "make the report".to_string(),
            session: String::new(),
            chat: 7,
            model: String::new(),
            check: "false".to_string(),
            budget: 4,
            executor: "chat".to_string(),
        };
        let goal = crate::gateway::goal::set(&world.state, new, 100).expect("a goal");
        let client = OpenAi::new("http://127.0.0.1:9/v1/chat/completions", "none");
        let said = execute("goal", r#"{"action":"done"}"#, &world, &client, &|_, _| {});
        assert!(said.contains("stays open"), "{said}");
        assert!(said.contains("check"), "{said}");
        let kept = crate::gateway::goal::load_one(&world.state, goal.id).expect("the goal");
        assert_eq!(kept.state, crate::gateway::goal::State::Open);
        assert!(kept.last_check.contains("failed"), "{:?}", kept.last_check);

        // The same claim closes the goal when the goal's own check passes.
        let passing = new_goal_for("true");
        let passing = crate::gateway::goal::set(&world.state, passing, 200).expect("a second goal");
        let said = execute("goal", r#"{"action":"done"}"#, &world, &client, &|_, _| {});
        assert!(said.contains("is done"), "{said}");
        let kept = crate::gateway::goal::load_one(&world.state, passing.id).expect("the goal");
        assert_eq!(kept.state, crate::gateway::goal::State::Done);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn new_goal_for(check: &str) -> crate::gateway::goal::NewGoal {
        crate::gateway::goal::NewGoal {
            text: "make the report".to_string(),
            session: String::new(),
            chat: 7,
            model: String::new(),
            check: check.to_string(),
            budget: 4,
            executor: "chat".to_string(),
        }
    }

    /// A model endpoint that answers by what it is asked. The grandchild's answer is delayed so the test
    /// can tell "delegate joined it" from "delegate returned while it was still running".
    fn routing_worker_stub() -> (
        String,
        std::thread::JoinHandle<()>,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a port");
        let url = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().expect("an address")
        );
        let served = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = std::sync::Arc::clone(&served);
        listener
            .set_nonblocking(true)
            .expect("a nonblocking listener");
        let server = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while counted.load(std::sync::atomic::Ordering::SeqCst) < 3
                && std::time::Instant::now() < deadline
            {
                let mut stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                        continue;
                    }
                    Err(_) => return,
                };
                stream.set_nonblocking(false).expect("a blocking stream");
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
                let messages = request["messages"].as_array().cloned().unwrap_or_default();
                let text: String = messages
                    .iter()
                    .filter_map(|message| message["content"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                let saw_tool = messages.iter().any(|message| message["role"] == "tool");
                let delta = if text.contains("check the inner file") {
                    std::thread::sleep(std::time::Duration::from_millis(250));
                    json!({"content": "{\"report\":\"deep\"}"})
                } else if text.contains("audit the tree") {
                    if saw_tool {
                        json!({"content": "{\"report\":\"worker\"}"})
                    } else {
                        json!({
                            "tool_calls": [{
                                "index": 0,
                                "id": "call-1",
                                "function": {
                                    "name": "spawn",
                                    "arguments": "{\"goal\":\"check the inner file\"}"
                                }
                            }]
                        })
                    }
                } else if saw_tool {
                    json!({"content": "finished"})
                } else {
                    json!({
                        "tool_calls": [{
                            "index": 0,
                            "id": "call-1",
                            "function": {
                                "name": "delegate",
                                "arguments": "{\"tasks\":[{\"goal\":\"audit the tree\"}]}"
                            }
                        }]
                    })
                };
                let frame = json!({"choices": [{"delta": delta}]}).to_string();
                let body = format!("data: {frame}\n\ndata: [DONE]\n\n");
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .expect("a response");
            }
        });
        (url, server, served)
    }

    /// A delegated worker's `spawn` used to be admitted asynchronously even though `delegate` joins its
    /// workers: the join returned and the grandchild kept running with nobody holding it. A worker now
    /// takes the blocking path, so when `delegate` returns the grandchild has finished and its result is
    /// on disk.
    #[test]
    fn a_grandchild_from_a_delegate_finishes_before_delegate_returns() {
        let dir = crate::scratch::root().join("agent-grandchild");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("state/sessions")).expect("a state");
        let world = world_at(&dir, Mode::Act, "chat:7");
        let registry = crate::gateway::children::Registry::new(&world.state);
        let (url, server, served) = routing_worker_stub();
        let client = OpenAi::new(&url, "stub");

        let said = execute(
            "delegate",
            r#"{"tasks":[{"goal":"audit the tree"}]}"#,
            &world,
            &client,
            &|_, _| {},
        );
        server.join().expect("the endpoint");
        assert_eq!(
            served.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "the parent, the worker and the grandchild each asked once"
        );
        assert!(said.contains("status: ok"), "the delegate's report: {said}");
        assert!(said.contains("worker"), "the delegate's report: {said}");

        // The registry is here and would have been used at depth zero. The worker's spawn still took the
        // blocking path: no record was handed to the gateway, and both children's artifacts exist
        // already, so the grandchild was finished when `delegate` returned and not still running.
        assert!(
            crate::gateway::children::Registry::existing(&world.state).is_some(),
            "the test's registry is the one spawn would see"
        );
        assert!(
            registry.records().expect("the records").is_empty(),
            "a delegated worker's child is not the gateway's to run"
        );
        assert_eq!(registry.live(), 0, "nothing is still running");

        let artifacts = world.state.join("artifacts");
        let mut names: Vec<String> = std::fs::read_dir(&artifacts)
            .expect("the artifact tree")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names.len(), 2, "the worker and its grandchild: {names:?}");
        let deep = names
            .iter()
            .find(|name| name.starts_with("spawn-"))
            .unwrap_or_else(|| panic!("no grandchild artifact in {names:?}"));
        let written =
            std::fs::read_to_string(artifacts.join(deep).join("result.json")).expect("a result");
        assert!(
            written.contains("deep"),
            "the grandchild finished and wrote its report: {written}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The floor asks the TUI, the TUI answers, and the action that was asked about actually runs.
    /// This is the whole loop across three independent readers of one append-only file.
    #[test]
    fn a_floor_action_asks_and_runs_only_after_the_person_allows_it() {
        let dir = crate::scratch::root().join("agent-floor-ask");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("build")).expect("a build directory");
        std::fs::write(dir.join("build/junk.txt"), "generated").expect("a file to delete");
        let mut world = world_at(&dir, Mode::Act, "chat:test");
        let record = dir.join("state/approvals.jsonl");
        let broker = crate::approval::OnDisk::open(&record).expect("the approval record");
        world.questions = Some(crate::question::Desk::new(std::sync::Arc::new(broker), 3));
        let worker = std::thread::spawn(move || {
            let client = OpenAi::new("http://127.0.0.1:9/v1/chat/completions", "none");
            execute(
                "shell",
                r#"{"command":"rm -rf build"}"#,
                &world,
                &client,
                &|_, _| {},
            )
        });
        let reader = crate::approval::OnDisk::open(&record).expect("a second reader");
        let mut asked = None;
        for _ in 0..200 {
            let questions = reader.questions().unwrap_or_default();
            if let Some(question) = questions.into_iter().find(|q| q.answer.is_none()) {
                assert!(
                    question.request.action.contains("delete"),
                    "{}",
                    question.request.action
                );
                asked = Some(question.request.id);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let id = asked.expect("the floor asked before it touched anything");
        assert!(dir.join("build").exists(), "nothing ran before the answer");
        reader
            .resolve_choice(id, "allow-once", None, "tui")
            .expect("the answer is recorded");
        let said = worker.join().expect("the tool thread");
        assert!(!dir.join("build").exists(), "an allowed delete ran: {said}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A model question with options reaches the same record and the answer comes back as the tool
    /// result, which is the path plan mode depends on.
    #[test]
    fn a_model_question_returns_the_option_the_person_picked() {
        let dir = crate::scratch::root().join("agent-ask-tool");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("state")).expect("a state directory");
        let mut world = world_at(&dir, Mode::Plan, "chat:test");
        let record = dir.join("state/approvals.jsonl");
        let broker = crate::approval::OnDisk::open(&record).expect("the approval record");
        world.questions = Some(crate::question::Desk::new(std::sync::Arc::new(broker), 6));
        let worker = std::thread::spawn(move || {
            let client = OpenAi::new("http://127.0.0.1:9/v1/chat/completions", "none");
            execute(
                "ask",
                r#"{"prompt":"Which store should this use?","why":"both fit the requirements","blocking":true,"options":[{"key":"sqlite","label":"SQLite","detail":"one file, no server","recommended":true},{"key":"postgres","label":"Postgres","detail":"a server to run"}]}"#,
                &world,
                &client,
                &|_, _| {},
            )
        });
        let reader = crate::approval::OnDisk::open(&record).expect("a second reader");
        let mut asked = None;
        for _ in 0..200 {
            let questions = reader.questions().unwrap_or_default();
            if let Some(question) = questions.into_iter().find(|q| q.answer.is_none()) {
                asked = Some(question.request.id);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let id = asked.expect("the model asked");
        reader
            .resolve_choice(id, "postgres", Some("we already run one"), "tui")
            .expect("the answer is recorded");
        let said = worker.join().expect("the tool thread");
        assert!(said.contains("\"option\":\"postgres\""), "{said}");
        assert!(said.contains("we already run one"), "{said}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Five agreeing answers turn a choice into an assumption: no bar, no budget spent, and the result
    /// says it was learned rather than guessed.
    #[test]
    fn a_learned_choice_is_not_asked_again_and_says_so() {
        let dir = crate::scratch::root().join("agent-taste-auto");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("state")).expect("a state directory");
        let mut world = world_at(&dir, Mode::Plan, "chat:test");
        world.questions = Some(crate::question::Desk::new(
            std::sync::Arc::new(crate::approval::InMemory::new()),
            6,
        ));
        let mut ask = crate::question::Ask::choice(
            "Which store?",
            "both fit",
            false,
            vec![
                crate::question::Choice::new(
                    "postgres",
                    "Postgres",
                    "a server",
                    crate::question::Effect::Free,
                )
                .recommended(),
                crate::question::Choice::new(
                    "sqlite",
                    "SQLite",
                    "one file",
                    crate::question::Effect::Free,
                ),
            ],
        );
        ask.family = "storage".into();
        for _ in 0..5 {
            let answer = crate::approval::Answer {
                decision: Decision::Allow,
                scope: crate::approval::Scope::Once,
                by: "test".into(),
                at: "2026-10-20T12:00:00Z".into(),
                option: Some("postgres".into()),
                note: None,
            };
            crate::taste::record(&dir.join("state"), &dir, &ask, &answer)
                .expect("a recorded answer");
        }
        let client = OpenAi::new("http://127.0.0.1:9/v1/chat/completions", "none");
        let said = execute(
            "ask",
            r#"{"kind":"storage","prompt":"Which store?","why":"both fit","blocking":true,"options":[{"key":"postgres","label":"Postgres","detail":"a server","recommended":true},{"key":"sqlite","label":"SQLite","detail":"one file"}]}"#,
            &world,
            &client,
            &|_, _| {},
        );
        assert!(said.contains("assumed (learned)"), "{said}");
        assert!(said.contains("postgres"), "{said}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
