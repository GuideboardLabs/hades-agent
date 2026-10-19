import pathlib

p = pathlib.Path("harness/src/agent.rs")
s = p.read_text()

s = s.replace(
    "use std::sync::atomic::{AtomicBool, Ordering};",
    "use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};",
    1,
)

allowance = '''/// The model calls a whole conversation may spend, shared by the person's loop and every worker it
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
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| left.checked_sub(1))
            .is_ok()
    }

    pub fn left(&self) -> u32 {
        self.left.load(Ordering::SeqCst)
    }
}

'''
s = s.replace("/// What a chat can reach: where its shell and files live", allowance + "/// What a chat can reach: where its shell and files live", 1)

s = s.replace(
    '''    /// The MCP servers this surface may reach, shared by the chats and workers a gateway spawns so a
    /// server is started once rather than per message.
    pub mcp: std::sync::Arc<crate::mcp::Mcp>,
}''',
    '''    /// The MCP servers this surface may reach, shared by the chats and workers a gateway spawns so a
    /// server is started once rather than per message.
    pub mcp: std::sync::Arc<crate::mcp::Mcp>,
    /// The model calls this conversation and its workers may spend, counted once for the whole tree.
    pub allowance: std::sync::Arc<Allowance>,
}''',
    1,
)

# The loop spends one call before it asks.
s = s.replace(
    '''        if stopped() {
            return Ok(None);
        }
        let reply = client.chat(messages, Some(&tools), feed)?;''',
    '''        if stopped() {
            return Ok(None);
        }
        if !world.allowance.take() {
            return Err(Fault::model(
                "the shared model-call allowance is spent; the tree is bounded".to_string(),
            ));
        }
        let reply = client.chat(messages, Some(&tools), feed)?;''',
    1,
)

# Tool schema: tasks are objects with a goal and an optional result schema.
s = s.replace(
    '''                        "tasks": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "One self-contained instruction per worker, at most four."
                        }''',
    '''                        "tasks": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "goal": {"type": "string", "description": "One self-contained instruction. The worker cannot see this conversation."},
                                    "result_schema": {"type": "object", "description": "A JSON Schema the worker's final answer must satisfy. Omit it and the result is {\\"report\\": \\"<prose>\\"}."}
                                },
                                "required": ["goal"]
                            },
                            "description": "One task per worker, at most four. A string is accepted as a goal with the default report envelope."
                        }''',
    1,
)

start = s.index("fn delegate(\n")
end = s.index("/// Jobs live in `<state>/jobs`", start)

new_delegate = r'''/// One delegated task: a goal and the schema its answer must satisfy. A string task gets the default
/// envelope, so every delegation returns a checked object rather than a paragraph the parent has to parse.
struct Job {
    goal: String,
    schema: Value,
    typed: bool,
}

fn jobs(args: &Value) -> Vec<Job> {
    let envelope = json!({
        "type": "object",
        "properties": {"report": {"type": "string"}},
        "required": ["report"]
    });
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
        other if other.is_empty() => Ok(()),
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
        activity(Activity::Start, &format!("delegate: {}", clip(&job.goal, 60)));
        let started = job.goal.clone();
        let schema = job.schema.clone();
        let typed = job.typed;
        let system = format!(
            "{WORKER}\n\nReturn one JSON object and nothing else. It must satisfy this schema:\n{}\n\
             Do not put prose around it. Do not use Markdown fences. A missing or mistyped field fails \
             the task.",
            serde_json::to_string_pretty(&schema).unwrap_or_else(|_| "{}".to_string())
        );
        // A child gets its own log and its own bundle. The parent's context gets a handle and a checked
        // result, not the worker's whole transcript.
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_secs())
            .unwrap_or(0);
        let child = format!("deleg-{stamp}-{index}-{}", mneme::slugify(&started, 4));
        let name = child.clone();
        handles.push((
            started,
            child,
            std::thread::spawn(move || {
                let store = session_tree::FileStore::new(worker.state.join("sessions"));
                let id = session_tree::SessionId::new(&name);
                let mut log =
                    session_tree::Session::create(store, session_tree::SystemClock, id.clone())
                        .map_err(|fault| format!("{fault:?}"))?;
                let _ = log.append(
                    session_tree::Kind::Meta,
                    json!({"actor": "delegate", "parent": worker.actor, "role": worker.role.name(), "goal": started, "schema": schema}),
                );
                let _ = log.append(session_tree::Kind::User, json!({"text": started}));
                let mut messages = vec![
                    json!({"role": "system", "content": system}),
                    json!({"role": "user", "content": started}),
                ];
                let silent = |_: Feed, _: &str| {};
                let nothing = |_: Activity, _: &str| {};
                let answer = answer(&client, &mut messages, &worker, &silent, &nothing, 12);
                let (status, result, raw) = match answer {
                    Ok(text) => {
                        let value = if typed {
                            parse_json_answer(&text)
                        } else {
                            Some(json!({"report": text}))
                        };
                        match value {
                            Some(value) => match fits(&schema, &value) {
                                Ok(()) => ("ok".to_string(), Some(value), text),
                                Err(why) => ("invalid".to_string(), None, format!("{why}\n{text}")),
                            },
                            None => (
                                "invalid".to_string(),
                                None,
                                format!("the worker did not return a JSON object\n{text}"),
                            ),
                        }
                    }
                    Err(fault) => ("failed".to_string(), None, format!("{fault:?}")),
                };
                let _ = log.append(
                    session_tree::Kind::Assistant,
                    json!({"text": raw, "status": status, "result": result}),
                );
                let dir = worker.state.join("artifacts").join(&name);
                if std::fs::create_dir_all(&dir).is_ok() {
                    let bundle = format!(
                        "---\nid: {name}\nstatus: {status}\nrole: {}\nsession: {name}\nparent: {}\n---\n\n# {}\n\n{}\n",
                        worker.role.name(),
                        worker.actor,
                        started.trim(),
                        match &result {
                            Some(value) => serde_json::to_string_pretty(value)
                                .unwrap_or_else(|_| value.to_string()),
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
                Ok::<(String, Option<Value>, String), String>((status, result, raw))
            }),
        ));
    }
    let mut out = String::new();
    for (task, child, handle) in handles {
        let report = handle
            .join()
            .unwrap_or_else(|_| Err("the worker stopped".to_string()));
        let artifact = world.state.join("artifacts").join(&child).join("result.json");
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

'''
s = s[:start] + new_delegate + s[end:]

open(p, "w").write(s)
print("delegate rewritten", len(s))
