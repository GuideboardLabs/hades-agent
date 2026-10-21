//! An MCP client: one configured server, spoken to over stdio.
//!
//! The protocol is JSON-RPC 2.0, one JSON object per line, on the child's stdin and stdout. That is the
//! whole transport MCP defines for stdio, and it is enough for a client with no dependency beyond
//! `serde_json`: the tree already pays for that crate, and adding an MCP SDK would bring a runtime the
//! rest of the workspace does not use.
//!
//! **The server is a process the config names, never the model.** A chat can choose a configured server
//! and one of its tools. It cannot choose a command, add a server, or name an environment variable. The
//! child gets a filtered environment and only the variables the config lists, so a bot cannot hand an
//! unrelated key to a server by asking nicely.
//!
//! **An MCP call is opaque, and the allowlist is the decision.** The hard floor reads a command, a path
//! and a browser control. It cannot see inside a remote tool, so an allowed tool can do anything its
//! server offers: delete a file, send a message, move money. That is why `allow` is required, why an
//! empty list means no calls, and why the refusal happens before the activity ledger says `allow`.
//! This is not a sandbox for remote code, and the module says so rather than implying otherwise.
//!
//! **A server is started on first use and kept for the life of the process.** A browser or database MCP
//! server is useless if every message restarts it. One registry is built by the gateway and shared by the
//! chats and workers it spawns; calls to the same server serialize on that server's lock, which is slower
//! than a protocol that would multiplex and is correct for one person.
//!
//! **Stated limits.** stdio only, no HTTP/StreamableHTTP transport, no resources, prompts or sampling.
//! One page of `tools/list` is followed up to ten times, not unbounded. A server that writes log lines to
//! stdout is out of spec and is reported as a protocol fault rather than parsed around. A call that times
//! out kills the child; the next call starts a new one.

use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The protocol version this client speaks. The server answers with the version it will use, so a newer
/// server can still serve an older client when it supports this revision.
pub const PROTOCOL: &str = "2024-11-05";

/// The most text one tool result may put into a prompt.
const RESULT_LIMIT: usize = 30_000;
/// How many pages of `tools/list` are followed before the listing stops.
const PAGE_LIMIT: usize = 10;

/// The programs an MCP server may be started with unless the config names others. A server is a process
/// that talks to the model, so the program is the one thing that must not be free-form: `sh -c` with an
/// argument is a shell the model reached, and the allowlist is where that stops.
pub const COMMANDS: &[&str] = &["npx", "bunx", "uvx", "python3", "node", "deno"];

/// The `mcp` section of the config. A missing section is an empty registry, not an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpConfig {
    pub servers: BTreeMap<String, McpServer>,
    /// The programs a server may name. Defaults to [`COMMANDS`]; `mcp.commands` replaces the list rather
    /// than adding to it, so an owner who writes it can state exactly what may run.
    pub commands: Vec<String>,
}

impl Default for McpConfig {
    fn default() -> Self {
        McpConfig {
            servers: BTreeMap::new(),
            commands: COMMANDS.iter().map(|command| command.to_string()).collect(),
        }
    }
}

/// One server. `allow` names the tools a person has decided may run; empty means none and `*` means all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServer {
    pub command: String,
    pub args: Vec<String>,
    /// The child's variable name mapped to a value. A value beginning `env:` or `$` names a variable in
    /// the environment or `~/.hades/.env`; anything else is a literal and must not be a secret.
    pub env: BTreeMap<String, String>,
    pub allow: Vec<String>,
    pub timeout_s: u64,
}

/// One tool a server advertises.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tool {
    pub name: String,
    pub description: String,
}

impl McpConfig {
    /// Read `mcp` from a config object. Unknown keys are refused: a typo in `allow` must not silently
    /// leave every call blocked, and a typo in `command` must not silently start something else.
    pub fn from_value(value: Option<&Value>, path: &Path) -> Result<McpConfig, String> {
        let Some(value) = value else {
            return Ok(McpConfig::default());
        };
        let object = value
            .as_object()
            .ok_or_else(|| format!("{}: `mcp` is not an object", path.display()))?;
        refuse_unknown(object, &["servers", "commands"], "mcp", path)?;
        let commands = match object.get("commands") {
            None => COMMANDS
                .iter()
                .map(|command| command.to_string())
                .collect::<Vec<String>>(),
            Some(value) => string_list(Some(value), "mcp.commands", path)?
                .into_iter()
                .map(|command| command.trim().to_string())
                .filter(|command| !command.is_empty())
                .collect(),
        };
        if commands.is_empty() {
            return Err(format!(
                "{}: `mcp.commands` is empty, so no server could ever start; remove the key for the \
                 default list",
                path.display()
            ));
        }
        let Some(servers) = object.get("servers") else {
            return Ok(McpConfig {
                servers: BTreeMap::new(),
                commands,
            });
        };
        let servers = servers
            .as_object()
            .ok_or_else(|| format!("{}: `mcp.servers` is not an object", path.display()))?;
        let mut out = BTreeMap::new();
        for (name, value) in servers {
            if name.is_empty()
                || !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                return Err(format!(
                    "{}: `{name}` is not a server name; use letters, digits, `-` and `_`",
                    path.display()
                ));
            }
            let server = value.as_object().ok_or_else(|| {
                format!("{}: `mcp.servers.{name}` is not an object", path.display())
            })?;
            let where_ = format!("mcp.servers.{name}");
            refuse_unknown(
                server,
                &["command", "args", "env", "allow", "timeout_s"],
                &where_,
                path,
            )?;
            let command = server
                .get("command")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    format!(
                        "{}: `{where_}.command` is missing; a server needs a program to start",
                        path.display()
                    )
                })?
                .to_string();
            let program = std::path::Path::new(&command)
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| command.clone());
            if !commands.iter().any(|allowed| allowed == &program) {
                return Err(format!(
                    "{}: `{where_}.command` is `{command}`, which is not on `mcp.commands`; add it there \
                     if it should be allowed, or use one of: {}",
                    path.display(),
                    commands.join(", ")
                ));
            }
            let args = string_list(server.get("args"), &format!("{where_}.args"), path)?;
            let env = string_map(server.get("env"), &format!("{where_}.env"), path)?;
            let allow = string_list(server.get("allow"), &format!("{where_}.allow"), path)?;
            let timeout_s = match server.get("timeout_s") {
                None => 30,
                Some(Value::Number(number)) => number.as_u64().ok_or_else(|| {
                    format!(
                        "{}: `{where_}.timeout_s` is not a whole number",
                        path.display()
                    )
                })?,
                Some(_) => {
                    return Err(format!(
                        "{}: `{where_}.timeout_s` is not a whole number",
                        path.display()
                    ));
                }
            }
            .clamp(1, 300);
            out.insert(
                name.clone(),
                McpServer {
                    command,
                    args,
                    env,
                    allow,
                    timeout_s,
                },
            );
        }
        Ok(McpConfig { servers: out, commands })
    }

    /// The shape `save` writes back.
    pub fn to_value(&self) -> Value {
        let mut servers = Map::new();
        for (name, server) in &self.servers {
            servers.insert(
                name.clone(),
                json!({
                    "command": server.command,
                    "args": server.args,
                    "env": server.env,
                    "allow": server.allow,
                    "timeout_s": server.timeout_s,
                }),
            );
        }
        json!({ "servers": servers, "commands": self.commands })
    }
}

fn refuse_unknown(
    object: &Map<String, Value>,
    known: &[&str],
    where_: &str,
    path: &Path,
) -> Result<(), String> {
    for key in object.keys() {
        if !known.contains(&key.as_str()) {
            return Err(format!(
                "{}: `{where_}.{key}` is not a key this build reads; the keys are {}",
                path.display(),
                known.join(", ")
            ));
        }
    }
    Ok(())
}

fn string_list(value: Option<&Value>, where_: &str, path: &Path) -> Result<Vec<String>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let items = value
        .as_array()
        .ok_or_else(|| format!("{}: `{where_}` is not a list", path.display()))?;
    let mut out = Vec::new();
    for item in items {
        let text = item.as_str().ok_or_else(|| {
            format!(
                "{}: `{where_}` has a value that is not a string",
                path.display()
            )
        })?;
        let text = text.trim();
        if !text.is_empty() {
            out.push(text.to_string());
        }
    }
    Ok(out)
}

fn string_map(
    value: Option<&Value>,
    where_: &str,
    path: &Path,
) -> Result<BTreeMap<String, String>, String> {
    let Some(value) = value else {
        return Ok(BTreeMap::new());
    };
    let object = value
        .as_object()
        .ok_or_else(|| format!("{}: `{where_}` is not an object", path.display()))?;
    let mut out = BTreeMap::new();
    for (key, value) in object {
        let text = value
            .as_str()
            .ok_or_else(|| format!("{}: `{where_}.{key}` is not a string", path.display()))?;
        out.insert(key.clone(), text.to_string());
    }
    Ok(out)
}

/// The registry a surface holds. It starts servers lazily and keeps them until the process ends.
pub struct Mcp {
    config: McpConfig,
    cwd: Option<PathBuf>,
    live: Mutex<BTreeMap<String, Arc<Mutex<Client>>>>,
}

impl Mcp {
    /// No servers. What a test or a surface with no `mcp` section holds.
    pub fn empty() -> Mcp {
        Mcp {
            config: McpConfig::default(),
            cwd: None,
            live: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn new(config: McpConfig, cwd: Option<PathBuf>) -> Mcp {
        Mcp {
            config,
            cwd,
            live: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn config(&self) -> &McpConfig {
        &self.config
    }

    /// Whether a call to `tool` on `server` is on the person's allowlist. This is the gate the activity
    /// ledger uses, and it runs before the call is attempted.
    pub fn allowed(&self, server: &str, tool: &str) -> Result<(), String> {
        let Some(config) = self.config.servers.get(server) else {
            let known: Vec<&str> = self.config.servers.keys().map(String::as_str).collect();
            return Err(if known.is_empty() {
                format!("there is no MCP server called `{server}`; none are configured")
            } else {
                format!(
                    "there is no MCP server called `{server}`; configured: {}",
                    known.join(", ")
                )
            });
        };
        if config.allow.iter().any(|name| name == "*" || name == tool) {
            return Ok(());
        }
        Err(format!(
            "`{tool}` is not on the allowed list for MCP server `{server}`, so it was not called. \
             Add it to `mcp.servers.{server}.allow`, or answer the person and let them decide."
        ))
    }

    /// The tool list a server advertises, following up to ten pages.
    pub fn tools(&self, server: &str) -> Result<Vec<Tool>, String> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..PAGE_LIMIT {
            let params = cursor
                .as_ref()
                .map(|cursor| json!({ "cursor": cursor }))
                .unwrap_or_else(|| json!({}));
            let result = self.request(server, "tools/list", params)?;
            let Some(items) = result.get("tools").and_then(Value::as_array) else {
                break;
            };
            for item in items {
                let Some(name) = item.get("name").and_then(Value::as_str) else {
                    continue;
                };
                out.push(Tool {
                    name: name.to_string(),
                    description: item
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                });
            }
            cursor = result
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }
        Ok(out)
    }

    /// Whether a tool would be allowed, without starting a server. Used by the listing.
    pub fn is_allowed(&self, server: &str, tool: &str) -> bool {
        self.config
            .servers
            .get(server)
            .is_some_and(|config| config.allow.iter().any(|name| name == "*" || name == tool))
    }

    /// Call one tool. The allowlist is checked here as well as at the surface, so no caller can route
    /// around it by building the arguments itself.
    pub fn call(&self, server: &str, tool: &str, arguments: Value) -> Result<String, String> {
        self.allowed(server, tool)?;
        let result = self.request(
            server,
            "tools/call",
            json!({ "name": tool, "arguments": arguments }),
        )?;
        Ok(render_result(&result))
    }

    /// The one function both the chat tool and `hades mcp` use, so the two surfaces cannot disagree.
    pub fn run(&self, args: &Value) -> Result<String, String> {
        let action = args.get("action").and_then(Value::as_str).unwrap_or("list");
        match action {
            "list" => Ok(self.listing()),
            "tools" => {
                let server = required(args, "server")?;
                let tools = self.tools(server)?;
                Ok(self.render_tools(server, &tools))
            }
            "call" => {
                let server = required(args, "server")?;
                let tool = required(args, "tool")?;
                let arguments = arguments_of(args)?;
                self.call(server, tool, arguments)
            }
            other => Err(format!(
                "`{other}` is not an MCP action: say `list`, `tools` or `call`"
            )),
        }
    }

    fn listing(&self) -> String {
        if self.config.servers.is_empty() {
            return "no MCP servers are configured. Add one under `mcp.servers` in the config; \
                    each server needs a `command`, and its tools need to be named in `allow` before \
                    they can be called."
                .to_string();
        }
        let mut text = format!(
            "{} MCP server(s). Run `hades mcp tools <server>` (or the `mcp` tool with action `tools`) \
             for what each one offers.\n",
            self.config.servers.len()
        );
        for (name, server) in &self.config.servers {
            let mut line = format!("{name} · {}", server.command);
            for arg in &server.args {
                line.push(' ');
                line.push_str(arg);
            }
            let allow = if server.allow.is_empty() {
                "none".to_string()
            } else {
                server.allow.join(", ")
            };
            let mut env: Vec<&str> = server.env.keys().map(String::as_str).collect();
            env.sort();
            text.push_str(&format!(
                "  {line}\n    allow: {allow} · timeout: {}s{}{}\n",
                server.timeout_s,
                if env.is_empty() { "" } else { " · env: " },
                env.join(", ")
            ));
        }
        text
    }

    fn render_tools(&self, server: &str, tools: &[Tool]) -> String {
        if tools.is_empty() {
            return format!("MCP server `{server}` advertises no tools.");
        }
        let mut text = format!("{server} · {} tool(s)\n", tools.len());
        for tool in tools {
            let state = if self.is_allowed(server, &tool.name) {
                "allowed"
            } else {
                "blocked"
            };
            let description = clip(tool.description.lines().next().unwrap_or(""), 100);
            if description.is_empty() {
                text.push_str(&format!("  {:<24} [{state}]\n", tool.name));
            } else {
                text.push_str(&format!("  {:<24} [{state}] {description}\n", tool.name));
            }
        }
        text
    }

    fn request(&self, server: &str, method: &str, params: Value) -> Result<Value, String> {
        let client = self.client(server)?;
        let mut held = client
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match held.request(method, params) {
            Ok(value) => Ok(value),
            Err(said) => {
                drop(held);
                self.evict(server);
                Err(said)
            }
        }
    }

    fn client(&self, name: &str) -> Result<Arc<Mutex<Client>>, String> {
        let Some(server) = self.config.servers.get(name) else {
            return Err(format!("there is no MCP server called `{name}`"));
        };
        let mut live = self
            .live
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(client) = live.get(name) {
            return Ok(Arc::clone(client));
        }
        let client = Arc::new(Mutex::new(Client::start(
            name,
            server,
            self.cwd.as_deref(),
        )?));
        live.insert(name.to_string(), Arc::clone(&client));
        Ok(client)
    }

    fn evict(&self, name: &str) {
        let mut live = self
            .live
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        live.remove(name);
    }
}

fn required<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("a call needs a `{key}`"))
}

/// The gate the activity ledger consults before it says `allow`. Only a `call` is checked: `list` and
/// `tools` start a server and read what it advertises, and neither runs a tool. A `call` with no server
/// or no tool is refused as a usage error rather than sent, because the ledger should record the decision
/// that actually stopped it.
pub fn gate(args: &Value, mcp: &Mcp) -> Result<(), String> {
    if args.get("action").and_then(Value::as_str) != Some("call") {
        return Ok(());
    }
    let server = required(args, "server")?;
    let tool = required(args, "tool")?;
    mcp.allowed(server, tool)
}

fn arguments_of(args: &Value) -> Result<Value, String> {
    match args.get("arguments") {
        None | Some(Value::Null) => Ok(json!({})),
        Some(Value::Object(object)) => Ok(Value::Object(object.clone())),
        Some(Value::String(text)) => {
            let value: Value = serde_json::from_str(text)
                .map_err(|error| format!("`arguments` is not JSON: {error}"))?;
            if value.is_object() {
                Ok(value)
            } else {
                Err("`arguments` must be a JSON object".to_string())
            }
        }
        Some(_) => Err("`arguments` must be a JSON object".to_string()),
    }
}

fn render_result(result: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(content) = result.get("content").and_then(Value::as_array) {
        for item in content {
            match item.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(text) = item.get("text").and_then(Value::as_str) {
                        parts.push(text.to_string());
                    }
                }
                _ => parts.push(item.to_string()),
            }
        }
    }
    if parts.is_empty()
        && let Some(value) = result.get("structuredContent")
    {
        parts.push(value.to_string());
    }
    let mut text = parts.join("\n");
    if text.is_empty() {
        text = "(the tool returned no content)".to_string();
    }
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        text = format!("the tool reported an error:\n{text}");
    }
    clip(&text, RESULT_LIMIT)
}

fn clip(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let kept: String = text.chars().take(limit).collect();
    format!("{kept}\n[truncated at {limit} characters]")
}

/// One live child. Every request is a line on stdin and a line on stdout; the reader thread exists so a
/// request can time out without blocking the caller on a pipe read.
struct Client {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    next: u64,
    timeout: Duration,
    dead: bool,
    reaped: bool,
}

impl Client {
    fn start(name: &str, server: &McpServer, cwd: Option<&Path>) -> Result<Client, String> {
        let mut command = Command::new(&server.command);
        command.args(&server.args);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        filtered_env(&mut command, &server.env)?;
        let mut child = command.spawn().map_err(|error| {
            format!(
                "could not start MCP server `{name}` ({}): {error}",
                server.command
            )
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| format!("MCP server `{name}` did not give the client a stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| format!("MCP server `{name}` did not give the client a stdout"))?;
        let (sender, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else {
                    break;
                };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        let mut client = Client {
            child,
            stdin,
            lines,
            next: 1,
            timeout: Duration::from_secs(server.timeout_s),
            dead: false,
            reaped: false,
        };
        client.initialize(name)?;
        Ok(client)
    }

    fn initialize(&mut self, name: &str) -> Result<(), String> {
        let result = self.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL,
                "capabilities": {},
                "clientInfo": { "name": "hades", "version": env!("CARGO_PKG_VERSION") }
            }),
        )?;
        if !result.is_object() {
            self.stop();
            return Err(format!(
                "MCP server `{name}` answered `initialize` with something that is not an object"
            ));
        }
        self.notify("notifications/initialized")?;
        Ok(())
    }

    fn notify(&mut self, method: &str) -> Result<(), String> {
        self.write(&json!({ "jsonrpc": "2.0", "method": method }))
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        if self.dead {
            return Err("the MCP server is no longer running".to_string());
        }
        let id = self.next;
        self.next += 1;
        self.write(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))?;
        let deadline = Instant::now() + self.timeout;
        loop {
            let left = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or_default();
            if left.is_zero() {
                self.stop();
                return Err(format!(
                    "the MCP server did not answer `{method}` within {}s",
                    self.timeout.as_secs()
                ));
            }
            match self.lines.recv_timeout(left) {
                Ok(line) => {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    let value: Value = match serde_json::from_str(line) {
                        Ok(value) => value,
                        Err(error) => {
                            self.stop();
                            return Err(format!(
                                "the MCP server wrote a line that is not JSON ({error}): {}",
                                clip(line, 160)
                            ));
                        }
                    };
                    if value.get("id").and_then(Value::as_u64) != Some(id) {
                        continue;
                    }
                    if let Some(error) = value.get("error") {
                        let message = error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("no message");
                        return Err(format!("the MCP server refused `{method}`: {message}"));
                    }
                    return Ok(value.get("result").cloned().unwrap_or(Value::Null));
                }
                Err(RecvTimeoutError::Timeout) => {
                    self.stop();
                    return Err(format!(
                        "the MCP server did not answer `{method}` within {}s",
                        self.timeout.as_secs()
                    ));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    self.dead = true;
                    return Err("the MCP server closed its output".to_string());
                }
            }
        }
    }

    fn write(&mut self, value: &Value) -> Result<(), String> {
        let mut line = value.to_string();
        line.push('\n');
        let written = self
            .stdin
            .write_all(line.as_bytes())
            .and_then(|_| self.stdin.flush());
        if let Err(error) = written {
            self.dead = true;
            return Err(format!("the MCP server is not taking input: {error}"));
        }
        Ok(())
    }

    fn stop(&mut self) {
        self.dead = true;
        if self.reaped {
            return;
        }
        self.reaped = true;
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A child gets the ordinary shell environment and nothing else, plus the variables the config names.
/// A value starting `env:` or `$` is a secret name resolved from the environment or `~/.hades/.env`; a
/// literal is allowed for a non-secret setting and is never printed by the listing.
fn filtered_env(command: &mut Command, extra: &BTreeMap<String, String>) -> Result<(), String> {
    command.env_clear();
    for key in [
        "PATH", "HOME", "USER", "LANG", "LC_ALL", "TERM", "SHELL", "TMPDIR",
    ] {
        if let Ok(value) = std::env::var(key) {
            command.env(key, value);
        }
    }
    for (key, value) in std::env::vars() {
        if key.starts_with("XDG_") {
            command.env(key, value);
        }
    }
    if std::env::var_os("PATH").is_none() {
        command.env("PATH", "/usr/local/bin:/usr/bin:/bin");
    }
    for (key, value) in extra {
        let value = match value
            .strip_prefix("env:")
            .or_else(|| value.strip_prefix('$'))
        {
            Some(name) => crate::model::secret(name.trim()).map_err(|said| {
                format!("MCP env `{key}` names a secret that could not be read: {said}")
            })?,
            None => value.clone(),
        };
        command.env(key, value);
    }
    Ok(())
}
