import pathlib

p = pathlib.Path("harness/src/agent.rs")
s = p.read_text()

# Constants and tool lists
s = s.replace('const TOOLS: [&str; 17] = [', 'const TOOLS: [&str; 18] = [', 1)
s = s.replace('    "delegate",\n];', '    "delegate",\n    "spawn",\n];', 1)
s = s.replace('''        "shell" | "write_file" | "patch" | "remember" | "cron" | "browser" | "mcp"''', '''        "shell" | "write_file" | "patch" | "remember" | "cron" | "browser" | "mcp" | "spawn"''', 1)
s = s.replace('''/// The tools the act surface can call.''', '''/// How deep a graph of loops may nest. Zero is the person's conversation, one is a delegated worker, two
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

/// The tools the act surface can call.''', 1)

# Schema: spawn, offered until the depth is spent
s = s.replace('''    // A read surface is handed no tool that changes anything.''', '''    if depth < MAX_SPAWN_DEPTH {
        list.as_array_mut().unwrap().push(json!({
            "type": "function",
            "function": {
                "name": "spawn",
                "description": "Create one child graph: a worker with its own loop and session that must return one JSON object matching a schema. Use it for a sub-goal that deserves its own investigation and a typed answer. The child can spawn one level further; the depth and the model-call allowance are both shared. The child returns to you, not to the person's conversation.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "goal": {"type": "string", "description": "One self-contained sub-goal. The child cannot see this conversation."},
                        "result_schema": {"type": "object", "description": "JSON Schema the child's answer must satisfy. Omit it and the result is {\\"report\\": \\"<prose>\\"}."},
                        "steps": {"type": "integer", "description": "Model calls this child may make, 2 to 12. Default 8."}
                    },
                    "required": ["goal"]
                }
            }
        }));
    }
    // A read surface is handed no tool that changes anything.''', 1)

# Label and dispatch
s = s.replace('''        "browser" => args
            .get("url")''', '''        "spawn" => args
            .get("goal")
            .and_then(Value::as_str)
            .unwrap_or("a sub-goal"),
        "browser" => args
            .get("url")''', 1)
s = s.replace('''        "delegate" => delegate(&args, world, client, activity),''', '''        "delegate" => delegate(&args, world, client, activity),
        "spawn" => spawn(&args, world, client, activity),''', 1)

# jobs(): use the shared envelope
s = s.replace('''fn jobs(args: &Value) -> Vec<Job> {
    let envelope = json!({
        "type": "object",
        "properties": {"report": {"type": "string"}},
        "required": ["report"]
    });''', '''fn jobs(args: &Value) -> Vec<Job> {
    let envelope = report_envelope();''', 1)

# Remove the system prompt built in delegate; run_child owns it.
s = s.replace('''        let typed = job.typed;
        let system = format!(
            "{WORKER}\\n\\nReturn one JSON object and nothing else. It must satisfy this schema:\\n{}\\n\\
             Do not put prose around it. Do not use Markdown fences. A missing or mistyped field fails \\
             the task.",
            serde_json::to_string_pretty(&schema).unwrap_or_else(|_| "{}".to_string())
        );
''', '''        let typed = job.typed;
''', 1)

# Replace the worker closure body with a call to run_child.
start = s.index("            std::thread::spawn(move || {", s.index("fn delegate("))
end = s.index("            }),\n        ));", start)
s = s[:start] + "            std::thread::spawn(move || {\n                run_child(&name, &started, &schema, typed, &worker, &client, 12)\n" + s[end + len("            }),\n"):]

# Insert run_child before delegate
anchor = "fn delegate(\n"
run_child = r'''/// One child graph, from the parent's point of view: its own session, its own tools, and one checked object
/// at the end. Delegation and spawn share this, so a child means the same thing wherever it was created.
fn run_child(
    name: &str,
    goal: &str,
    schema: &Value,
    typed: bool,
    worker: &World,
    client: &OpenAi,
    steps: usize,
) -> Result<(String, Option<Value>, String), String> {
    let store = session_tree::FileStore::new(worker.state.join("sessions"));
    let id = session_tree::SessionId::new(name);
    let mut log = session_tree::Session::create(store, session_tree::SystemClock, id.clone())
        .map_err(|fault| format!("{fault:?}"))?;
    let _ = log.append(
        session_tree::Kind::Meta,
        json!({"actor": worker.actor, "parent": worker.actor, "role": worker.role.name(), "goal": goal, "schema": schema}),
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
    Ok((status, result, raw))
}

'''
s = s.replace(anchor, run_child + anchor, 1)

# Spawn after delegate, before cron
cron_anchor = "/// Jobs live in `<state>/jobs`"
spawn = r'''/// A recursive node: one child graph, run to completion, returned to the caller that spawned it. The child
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
    let given = args.get("result_schema").filter(|schema| schema.is_object());
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
    match run_child(&name, &goal, &schema, typed, &worker, client, steps) {
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

'''
s = s.replace(cron_anchor, spawn + cron_anchor, 1)

p.write_text(s)
print("spawn added", len(s))
