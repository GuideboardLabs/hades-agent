//! The oracle for the CLI: driven through `run`, so no terminal is involved and every claim about the
//! output is checkable.

use std::path::{Path, PathBuf};

fn scratch(name: &str) -> PathBuf {
    let dir = scratch_base().join(format!("hades-cli-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

/// Runs the CLI the way the binary does, and hands back the exit code and everything it printed.
///
/// The config is read from the Hades home when no `--config` is given, and a test must not read the home
/// of the machine it runs on. `HADES_HOME` is set once, before the first call, for the whole binary.
fn hades(args: &[&str]) -> (i32, String) {
    isolate_home();
    let owned: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
    let mut out: Vec<u8> = Vec::new();
    let code = harness::cli::run(&owned, &mut out);
    (code, String::from_utf8_lossy(&out).into_owned())
}

fn isolate_home() {
    static HOME: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    HOME.get_or_init(|| {
        let dir = scratch_base().join(format!("hades-cli-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("an isolated Hades home");
        // SAFETY: set once, inside `OnceLock::get_or_init`, before any test thread reaches a CLI call,
        // and the CLI reads it only after that call. No other thread reads or writes this variable.
        unsafe { std::env::set_var("HADES_HOME", &dir) };
        dir
    });
}

/// A repository's `.hades-project.json` sets the check the run will use, and the context command has to
/// say so: a Python tree told to expect `cargo test --quiet` prepares the model for the wrong check. This
/// was true of `run` and false of `context` and `tune`, which is the kind of gap a suite catches only when
/// it looks at every path that reads a profile.
#[test]
fn a_project_recipe_reaches_the_context_and_tune_checks() {
    let root = scratch_base().join(format!("recipe-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("a root");
    std::fs::write(
        root.join(".hades-project.json"),
        r#"{"version":1,"verify":"python3 -m pytest -q","protect":["test_app.py"]}"#,
    )
    .expect("a recipe");
    std::fs::write(
        root.join("test_app.py"),
        "def test_ok():\n    assert True\n",
    )
    .expect("a test");

    let (code, output) = hades(&[
        "context",
        "make the test pass",
        "--root",
        root.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{output}");
    assert!(
        output.contains("python3 -m pytest -q"),
        "the context did not carry the recipe's check:\n{output}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

fn log_lines(state: &Path, session: &str) -> Vec<serde_json::Value> {
    let path = state.join("sessions").join(format!("{session}.jsonl"));
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    text.lines()
        .map(|line| serde_json::from_str(line).expect("every line is one entry of JSON"))
        .collect()
}

#[test]
fn the_help_lists_the_commands() {
    let (code, text) = hades(&["help"]);
    assert_eq!(code, 0);
    for command in [
        "profiles", "route", "context", "audit", "turn", "service", "version", "refine",
    ] {
        assert!(text.contains(command), "{command} is missing from the help");
    }
    assert!(text.contains("Exit codes"));
}

/// A delivery claim starts with the binary being able to say which build it is. The test reads the crate's
/// own version rather than a literal, so bumping the version does not break it.
#[test]
fn the_binary_says_which_version_it_is() {
    let (code, text) = hades(&["--version"]);
    assert_eq!(code, 0, "{text}");
    assert_eq!(text, format!("hades {}\n", env!("CARGO_PKG_VERSION")));

    let (code, text) = hades(&["version"]);
    assert_eq!(code, 0, "{text}");
    assert_eq!(text, format!("hades {}\n", env!("CARGO_PKG_VERSION")));

    let (code, text) = hades(&["-V"]);
    assert_eq!(code, 0, "{text}");
    assert_eq!(text, format!("hades {}\n", env!("CARGO_PKG_VERSION")));
}

#[test]
fn the_vault_can_be_built_from_the_cli() {
    let dir = scratch("weft-create");
    std::fs::create_dir_all(dir.join("work")).expect("work");
    let config = dir.join("config.json").display().to_string();
    let vault = dir.join("mneme").display().to_string();
    let work = dir.join("work").display().to_string();

    let (code, text) = hades(&[
        "init",
        "--config",
        &config,
        "--vault",
        &vault,
        "--root",
        &work,
        "--non-interactive",
        "--name",
        "Warden",
        "--goal",
        "keep the notes true",
    ]);
    assert_eq!(code, 0, "{text}");

    let (code, text) = hades(&[
        "topic",
        "technical",
        "AI Inference",
        "--summary",
        "How tokens are made.",
        "--config",
        &config,
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains("50 Topics/technical/ai-inference/index.md"),
        "{text}"
    );

    let (code, text) = hades(&[
        "note",
        "research",
        "technical/ai-inference",
        "KV cache at 32K",
        "--summary",
        "24.2 GiB at q4_0.",
        "--config",
        &config,
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains("20 Research/technical/ai-inference/kv-cache-at-32k.md"),
        "{text}"
    );

    let (code, text) = hades(&[
        "project",
        "Loom",
        "--topic",
        "technical/ai-inference",
        "--research",
        "kv-cache-at-32k",
        "--summary",
        "A serving stack.",
        "--config",
        &config,
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("30 Projects/loom/index.md"), "{text}");

    let (code, text) = hades(&[
        "product",
        "Loom 1.0",
        "--from",
        "loom",
        "--summary",
        "The closed release.",
        "--config",
        &config,
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("40 Products/loom-1-0/index.md"), "{text}");

    let (code, text) = hades(&["audit", "--config", &config]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("projects: 1"), "{text}");
    assert!(text.contains("products: 1"), "{text}");

    let index = std::fs::read_to_string(dir.join("mneme/INDEX.md")).expect("index");
    assert!(index.contains("AI Inference"), "{index}");
    assert!(index.contains("Loom"), "{index}");
    assert!(index.contains("Loom 1.0"), "{index}");

    let (code, text) = hades(&[
        "project",
        "Broken",
        "--topic",
        "technical/ai-inference",
        "--research",
        "no-such-note",
        "--summary",
        "A broken project.",
        "--config",
        &config,
    ]);
    assert_eq!(code, 2, "{text}");
    assert!(text.contains("does not name an existing"), "{text}");
}

#[test]
fn an_audit_reports_a_dangling_research_reference_instead_of_aborting() {
    let dir = scratch("weft-dangling");
    std::fs::create_dir_all(dir.join("work")).expect("work");
    let config = dir.join("config.json").display().to_string();
    let vault = dir.join("mneme").display().to_string();
    let work = dir.join("work").display().to_string();
    let (code, text) = hades(&[
        "init",
        "--config",
        &config,
        "--vault",
        &vault,
        "--root",
        &work,
        "--non-interactive",
    ]);
    assert_eq!(code, 0, "{text}");
    let project = dir.join("mneme/30 Projects/manual/index.md");
    std::fs::create_dir_all(project.parent().expect("parent")).expect("project dir");
    std::fs::write(
        &project,
        "---\ntitle: Manual\nsummary: A stack.\nstatus: durable\ntopic: technical/ai-inference\nresearch: [missing]\nprovenance: hand\n---\n\nbody\n",
    )
    .expect("project");

    let (code, text) = hades(&["audit", "--vault", &vault]);
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("dangling_research"), "{text}");
    assert!(text.contains("does not name an existing"), "{text}");
}

#[test]
fn mneme_upgrade_moves_the_old_topics_folder_out_of_the_vault() {
    let dir = scratch("weft-upgrade");
    std::fs::create_dir_all(dir.join("work")).expect("work");
    let config = dir.join("config.json").display().to_string();
    let vault = dir.join("mneme").display().to_string();
    let work = dir.join("work").display().to_string();
    let (code, text) = hades(&[
        "init",
        "--config",
        &config,
        "--vault",
        &vault,
        "--root",
        &work,
        "--non-interactive",
    ]);
    assert_eq!(code, 0, "{text}");

    let old = dir.join("mneme/40 Topics");
    std::fs::create_dir_all(&old).expect("old topics");
    std::fs::write(old.join("legacy.md"), "legacy\n").expect("old note");

    let (code, text) = hades(&["mneme", "upgrade", "--retire-old", "--config", &config]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("old: moved 1 file(s)"), "{text}");
    assert!(!old.exists(), "the old folder is still in the vault");
    let archive = dir.join("mneme-archive");
    let moved = std::fs::read_dir(&archive)
        .expect("archive")
        .filter_map(|entry| entry.ok())
        .any(|entry| entry.path().join("legacy.md").is_file());
    assert!(moved, "the old note was not archived");
    assert!(
        dir.join("mneme/50 Topics/START HERE.md").is_file(),
        "the new tree was not written"
    );
}

#[test]
fn an_audit_refresh_sees_a_hand_edited_note() {
    let dir = scratch("weft-audit-refresh");
    std::fs::create_dir_all(dir.join("work")).expect("work");
    let config = dir.join("config.json").display().to_string();
    let vault = dir.join("mneme").display().to_string();
    let work = dir.join("work").display().to_string();
    let (code, text) = hades(&[
        "init",
        "--config",
        &config,
        "--vault",
        &vault,
        "--root",
        &work,
        "--non-interactive",
    ]);
    assert_eq!(code, 0, "{text}");

    let project = dir.join("mneme/30 Projects/manual/index.md");
    std::fs::create_dir_all(project.parent().expect("parent")).expect("project dir");
    std::fs::write(
        &project,
        "---\ntitle: Manual\nsummary: A stack.\nstatus: durable\ntopic: technical/ai-inference\nresearch: [missing]\nprovenance: hand\n---\n\nbody\n",
    )
    .expect("project");

    let (code, text) = hades(&["audit", "--config", &config]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("projects: 0"), "{text}");

    let (code, text) = hades(&["audit", "--refresh", "--config", &config]);
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("projects: 1"), "{text}");
    assert!(text.contains("dangling_research"), "{text}");
    let index = std::fs::read_to_string(dir.join("mneme/INDEX.md")).expect("index");
    assert!(index.contains("Manual"), "{index}");

    let (code, text) = hades(&["audit", "--refresh", "--config", &config]);
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("projects: 1"), "{text}");

    let (code, text) = hades(&["audit", "--refresh", "--vault", &vault]);
    assert_eq!(code, 2, "{text}");
    assert!(text.contains("already imports the tree"), "{text}");
}

#[test]
fn profiles_lists_what_the_build_ships() {
    let (code, text) = hades(&["profiles"]);
    assert_eq!(code, 0);
    for id in ["fix", "sweep", "survey"] {
        assert!(text.contains(id), "{id} is missing");
    }
    assert!(text.contains("inherit"), "{text}");
    assert!(text.contains("cargo test --quiet"), "{text}");
}

#[test]
fn route_says_which_profile_and_why() {
    let (code, text) = hades(&["route", "fix the broken parser"]);
    assert_eq!(code, 0);
    assert!(text.contains("profile: fix"), "{text}");
    assert!(text.contains("model:   inherit"), "{text}");
    assert!(text.contains("reason:"), "{text}");

    let (code, text) = hades(&["route", "make it nicer somehow"]);
    assert_eq!(code, 0);
    assert!(text.contains("no rule matched"), "{text}");
    assert!(text.contains("profile: fix"), "the fallback is fix: {text}");
}

#[test]
fn an_unknown_flag_is_refused_rather_than_ignored() {
    let (code, text) = hades(&["route", "fix it", "--profil", "sweep"]);
    assert_eq!(code, 2);
    assert!(text.contains("--profil"), "{text}");
    assert!(text.contains("not a flag"), "{text}");
}

#[test]
fn a_flag_without_a_value_says_so() {
    let (code, text) = hades(&["turn", "do the thing", "--program"]);
    assert_eq!(code, 2);
    assert!(text.contains("needs a value"), "{text}");
}

#[test]
fn an_unknown_command_is_refused() {
    let (code, text) = hades(&["dance"]);
    assert_eq!(code, 2);
    assert!(text.contains("no command `dance`"), "{text}");
}

#[test]
fn a_turn_runs_a_program_and_writes_a_log_that_reads_back() {
    let root = scratch("turn");
    let state = root.join("state");

    let (code, text) = hades(&[
        "turn",
        "say hello",
        "--program",
        "echo hello",
        "--policy",
        "open",
        "--verify",
        "true",
        "--root",
        &root.display().to_string(),
        "--state",
        &state.display().to_string(),
        "--session",
        "one",
    ]);

    assert_eq!(code, 0, "{text}");
    assert!(text.contains("policy:  open"), "{text}");
    assert!(text.contains("check passed"), "{text}");
    assert!(text.contains("session: one"), "{text}");

    let entries = log_lines(&state, "one");
    let kinds: Vec<&str> = entries
        .iter()
        .map(|entry| entry["kind"].as_str().expect("a kind"))
        .collect();
    assert_eq!(
        kinds,
        vec![
            "meta",
            "meta",
            "user",
            "tool_call",
            "tool_result",
            "tool_call",
            "tool_result",
            "checkpoint",
        ]
    );

    // The second meta entry is the prompt that produced the run: one size and one digest, no text.
    assert_eq!(entries[1]["payload"]["type"], "harness");
    assert!(entries[1]["payload"]["prompt_bytes"].is_number());
    assert_eq!(
        entries[1]["payload"]["prompt_sha256"]
            .as_str()
            .expect("a digest")
            .len(),
        64
    );

    // The program that ran is in the log, and so is what it printed.
    assert_eq!(entries[3]["payload"]["program"], "echo hello");
    assert_eq!(entries[4]["payload"]["status"], 0);
    assert!(
        entries[4]["payload"]["output"]
            .as_str()
            .expect("output")
            .contains("hello")
    );
}

#[test]
fn a_second_cli_turn_resumes_the_same_session() {
    let root = scratch("resume");
    let state = root.join("state");
    let root_arg = root.display().to_string();
    let state_arg = state.display().to_string();

    for program in ["echo one", "echo two"] {
        let (code, text) = hades(&[
            "turn",
            "say hello",
            "--program",
            program,
            "--policy",
            "open",
            "--verify",
            "true",
            "--root",
            &root_arg,
            "--state",
            &state_arg,
            "--session",
            "again",
        ]);
        assert_eq!(code, 0, "{text}");
    }

    let entries = log_lines(&state, "again");
    assert_eq!(
        entries.len(),
        14,
        "two turns of six, after the root and the harness meta entries"
    );
    for (index, entry) in entries.iter().enumerate().skip(1) {
        assert_eq!(
            entry["parent"],
            serde_json::json!(index - 1),
            "every entry hangs off the one before it"
        );
    }
}

#[test]
fn a_failed_check_exits_one_and_says_what_happened() {
    let root = scratch("fail");
    let state = root.join("state");

    let (code, text) = hades(&[
        "turn",
        "fail the check",
        "--program",
        "echo worked",
        "--policy",
        "open",
        "--verify",
        "exit 4",
        "--root",
        &root.display().to_string(),
        "--state",
        &state.display().to_string(),
        "--session",
        "failed",
    ]);

    assert_eq!(code, 1, "{text}");
    assert!(text.contains("check failed"), "{text}");
    assert!(text.contains("exit 4"), "{text}");
}

#[test]
fn a_turn_with_no_program_says_where_the_program_has_to_come_from() {
    let root = scratch("noprogram");
    let (code, text) = hades(&[
        "turn",
        "do something",
        "--root",
        &root.display().to_string(),
        "--state",
        &root.join("state").display().to_string(),
    ]);
    assert_eq!(code, 2);
    // Both ways out are named, and so is the command that lists what a model could be.
    assert!(text.contains("--program"), "{text}");
    assert!(text.contains("--model"), "{text}");
    assert!(text.contains("hades models"), "{text}");
}

#[test]
fn the_context_carries_a_ranked_map_of_the_root() {
    // The map is the largest section and it goes in last, so this is also the test of the budget: the four
    // fixed sections survive and the map takes what is left.
    let root = scratch("mapctx");
    for (path, text) in [
        ("src/core.rs", "pub fn core() {}\n"),
        ("src/a.rs", "use crate::core;\npub fn a() {}\n"),
        ("src/b.rs", "use crate::core;\n"),
        ("target/junk.rs", "pub fn junk() {}\n"),
    ] {
        let full = root.join(path);
        std::fs::create_dir_all(full.parent().expect("a parent")).expect("a directory");
        std::fs::write(&full, text).expect("a file");
    }

    let (code, text) = hades(&[
        "context",
        "change core",
        "--root",
        &root.display().to_string(),
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("repo map"), "the map is a section: {text}");
    assert!(text.contains("core.rs"), "{text}");
    assert!(
        !text.contains("junk.rs"),
        "a build directory is not mapped: {text}"
    );
}

#[test]
fn a_question_raised_by_one_invocation_is_answered_by_another() {
    // Through the CLI, in separate processes: this is the design's claim at the surface a person touches.
    let root = scratch("approvals");
    let state = root.join("state");
    let state_arg = state.display().to_string();

    let asked = |program: &str| {
        hades(&[
            "turn",
            "say hello",
            "--program",
            program,
            "--verify",
            "true",
            "--root",
            &root.display().to_string(),
            "--state",
            &state.display().to_string(),
            "--session",
            "guarded",
        ])
    };

    // The default policy is guarded, so the first turn asks instead of running.
    let (code, text) = asked("echo hello");
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("waiting on question #0"), "{text}");
    assert!(
        text.contains("hades answer 0 allow --scope session"),
        "the way to answer is printed with the question: {text}"
    );
    assert!(!text.contains("check passed"), "nothing ran: {text}");

    // A separate invocation sees the question.
    let (code, text) = hades(&["approvals", "--state", &state_arg]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("waiting"), "{text}");
    assert!(text.contains("guarded"), "{text}");

    // And answers it, attributing the answer to whoever gave it.
    let (code, text) = hades(&[
        "answer", "0", "allow", "--scope", "session", "--by", "operator", "--state", &state_arg,
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains("answered allow for session by operator"),
        "{text}"
    );
    assert!(text.contains("will not ask again"), "{text}");

    let (code, text) = hades(&["approvals", "--state", &state_arg]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("allow session by operator"), "{text}");
    assert!(!text.contains("waiting"), "{text}");

    // Now the same turn runs, and the log records who let it through.
    let (code, text) = asked("echo hello");
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("check passed"), "{text}");
    assert!(text.contains("by=operator"), "{text}");

    let entries = log_lines(&state, "guarded");
    let kinds: Vec<&str> = entries
        .iter()
        .map(|entry| entry["kind"].as_str().expect("a kind"))
        .collect();
    assert_eq!(
        kinds,
        vec![
            "meta",
            "meta",
            "user",
            "approval_request",
            "user",
            "approval_decision",
            "tool_call",
            "tool_result",
            "tool_call",
            "tool_result",
            "checkpoint",
        ]
    );
    assert_eq!(entries[5]["payload"]["by"], "operator");
    assert_eq!(entries[5]["payload"]["question"], 0);

    // A second answer for the same question is refused rather than overwriting the first.
    let (code, text) = hades(&[
        "answer",
        "0",
        "deny",
        "--scope",
        "session",
        "--by",
        "someone else",
        "--state",
        &state_arg,
    ]);
    assert_eq!(code, 2, "{text}");
    assert!(text.contains("already answered"), "{text}");
}

#[test]
fn a_question_nobody_has_asked_is_not_answerable() {
    let root = scratch("noquestion");
    let (code, text) = hades(&[
        "approvals",
        "--state",
        &root.join("state").display().to_string(),
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("no questions have been asked"), "{text}");

    let (code, text) = hades(&[
        "answer",
        "4",
        "allow",
        "--state",
        &root.join("state").display().to_string(),
    ]);
    assert_eq!(code, 2, "{text}");
    assert!(text.contains("was ever asked"), "{text}");
}

#[test]
fn an_answer_has_to_be_an_answer() {
    use harness::approval::{ApprovalBroker as _, OnDisk};
    let root = scratch("badanswer");
    std::fs::create_dir_all(root.join("state")).expect("a state directory");
    let broker = OnDisk::open(root.join("state/approvals.jsonl")).expect("the record");
    let ask = harness::question::Ask::permission("Delete build?", "it cannot be undone");
    let prompt = ask.prompt.clone();
    broker
        .request_ask("chat", "permanently delete", &prompt, Some(ask))
        .expect("a question");
    let state = root.join("state").display().to_string();
    let (code, text) = hades(&["answer", "0", "maybe", "--state", &state]);
    assert_eq!(code, 2);
    assert!(text.contains("not an option"), "{text}");

    let (code, text) = hades(&[
        "answer", "0", "allow", "--scope", "forever", "--state", &state,
    ]);
    assert_eq!(code, 2);
    assert!(text.contains("say once or session"), "{text}");
}

#[test]
fn a_policy_that_does_not_exist_is_refused() {
    let root = scratch("badpolicy");
    let (code, text) = hades(&[
        "turn",
        "do the thing",
        "--program",
        "true",
        "--policy",
        "reckless",
        "--state",
        &root.join("state").display().to_string(),
    ]);
    assert_eq!(code, 2);
    assert!(text.contains("no policy named `reckless`"), "{text}");
    assert!(text.contains("open or guarded"), "{text}");
}

#[test]
fn context_shows_the_sections_and_what_was_dropped() {
    let root = scratch("context");
    std::fs::write(root.join("a-file.txt"), "x").expect("a file to list");

    let (code, text) = hades(&[
        "context",
        "fix the bug",
        "--root",
        &root.display().to_string(),
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("sections in:"), "{text}");
    assert!(text.contains("goal"), "{text}");
    assert!(text.contains("files"), "{text}");
    assert!(
        text.contains("a-file.txt"),
        "the listing is in the bundle: {text}"
    );
    assert!(text.contains("sections dropped:"), "{text}");
    assert!(text.contains("none"), "{text}");
}

#[test]
fn the_audit_runs_over_a_vault_and_reports_what_it_found() {
    let root = scratch("audit");
    let vault = mneme::Vault::new(root.clone(), mneme::FileVault::new(), mneme::SystemClock);
    vault.init(None).expect("a fresh vault");

    let (code, text) = hades(&["audit", "--vault", &root.display().to_string()]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("vault:"), "{text}");
    assert!(text.contains("inbox: 0"), "{text}");

    // A note behind a fence is reported and does not fail the audit. The fence is the boundary working: the
    // vault is a directory and the knowledge graph is the part of it outside the fences, so an audit that
    // failed for finding one would be asking the vault to stop holding what the boundary protects.
    std::fs::create_dir_all(root.join("2-Areas/Family")).expect("a fenced area");
    std::fs::write(
        root.join("2-Areas/Family/note.md"),
        "---\ntitle: Private\n---\n\nbody\n",
    )
    .expect("a note");
    let (code, text) = hades(&["audit", "--vault", &root.display().to_string()]);
    assert_eq!(code, 0, "a fenced path is a record, not a defect: {text}");
    assert!(
        text.contains("fenced_path"),
        "and it is still reported: {text}"
    );
    assert!(text.contains("2-Areas/Family/note.md"), "{text}");

    // Something actually wrong still exits non-zero, so the fence change did not turn the audit green.
    std::fs::write(
        root.join("20 Research/orphan.md"),
        "---\ntitle: Orphan\n---\n\nno summary, no topic\n",
    )
    .expect("a note");
    let (code, text) = hades(&["audit", "--vault", &root.display().to_string()]);
    assert_ne!(
        code, 0,
        "a research note with no topic is refused, not silently imported: {text}"
    );
    assert!(
        text.contains("needs a topic"),
        "the refusal names the rule: {text}"
    );
}

/// Hades is a second system. It refuses the live install's paths by name rather than trusting whoever typed
/// the command to remember, and the refusal says why.
#[test]
fn the_live_hermes_paths_are_refused() {
    let home = PathBuf::from(std::env::var("HOME").expect("a home"));
    let live_vault = home.join("Documents/ObsidianVault");
    if live_vault.exists() {
        let (code, text) = hades(&["audit", "--vault", &live_vault.display().to_string()]);
        assert_eq!(code, 2, "{text}");
        assert!(text.contains("live Hermes vault"), "{text}");
        assert!(text.contains("its own vault and state"), "{text}");
    }

    let live_home = home.join(".hermes");
    if live_home.exists() {
        let (code, text) = hades(&[
            "status",
            "--state",
            &live_home.join("scratch/state").display().to_string(),
        ]);
        assert_eq!(
            code, 2,
            "a state directory inside the live install is refused: {text}"
        );
        assert!(text.contains("live Hermes home"), "{text}");
    }
}

/// And its own paths are ordinary, so the guard is a boundary and not a wall.
#[test]
fn hades_own_directories_are_not_refused() {
    let root = scratch("boundary");
    let (code, text) = hades(&["audit", "--vault", &root.display().to_string()]);
    assert_eq!(code, 0, "{text}");
}

#[test]
fn audit_needs_a_vault_to_look_at() {
    let (code, text) = hades(&["audit"]);
    assert_eq!(code, 2);
    assert!(text.contains("--vault"), "{text}");
}

#[test]
fn run_tries_again_after_a_check_fails_and_says_so_in_its_walk() {
    let root = scratch("run");
    let state = root.join("state");
    let root_arg = root.display().to_string();
    let state_arg = state.display().to_string();

    // Two programs, one per attempt: the first does nothing, the second makes the file the check wants.
    let (code, text) = hades(&[
        "run",
        "make made.txt exist",
        "--program",
        "true",
        "--program",
        "touch made.txt",
        "--policy",
        "open",
        "--verify",
        "test -f made.txt",
        "--root",
        &root_arg,
        "--state",
        &state_arg,
        "--session",
        "one",
        "--attempts",
        "3",
    ]);

    assert_eq!(code, 0, "{text}");
    assert!(text.contains("accepted after 2 attempt(s)"), "{text}");
    assert!(text.contains("bounds:  3 attempts"), "{text}");
    assert!(root.join("made.txt").exists(), "the second attempt ran");

    // The walk is in the log as checkpoints, one per node entered, with the edge that led there.
    let edges: Vec<String> = log_lines(&state, "one")
        .iter()
        .filter_map(|entry| entry["payload"]["edge"].as_str().map(str::to_string))
        .collect();
    assert_eq!(
        edges,
        vec!["attempt -> attempt", "attempt -> accept", "accept -> done"],
        "{edges:?}"
    );
}

#[test]
fn a_token_budget_is_a_run_flag_and_a_budget_that_cannot_work_is_refused() {
    let dir = scratch("budget-tokens");
    let root = dir.join("root");
    std::fs::create_dir_all(&root).expect("a root");
    let root_arg = root.display().to_string();
    let state = dir.join("state");
    let state_arg = state.display().to_string();

    // Zero is not a budget: a run that may not spend is not a run. The refusal comes before the model is
    // asked, so a typo costs nothing.
    let (code, text) = hades(&[
        "run",
        "make a thing",
        "--budget-tokens",
        "0",
        "--program",
        "true",
        "--verify",
        "test -f never.txt",
        "--policy",
        "open",
        "--root",
        &root_arg,
        "--state",
        &state_arg,
    ]);
    assert_eq!(code, 2, "{text}");
    assert!(text.contains("`--budget-tokens 0`"), "{text}");

    // A budget smaller than one call's completion cap would let the first call overrun the whole run.
    let (code, text) = hades(&[
        "run",
        "make a thing",
        "--budget-tokens",
        "100",
        "--max-tokens",
        "200",
        "--program",
        "true",
        "--verify",
        "test -f never.txt",
        "--policy",
        "open",
        "--root",
        &root_arg,
        "--state",
        &state_arg,
    ]);
    assert_eq!(code, 2, "{text}");
    assert!(text.contains("`--max-tokens 200`"), "{text}");

    // The cap is resolved even when nobody typed one: the model client's default completion cap is 4096,
    // so a budget of 500 would let one call spend eight times the whole run's allowance.
    let (code, text) = hades(&[
        "run",
        "make a thing",
        "--budget-tokens",
        "500",
        "--program",
        "true",
        "--verify",
        "test -f never.txt",
        "--policy",
        "open",
        "--root",
        &root_arg,
        "--state",
        &state_arg,
    ]);
    assert_eq!(code, 2, "{text}");
    assert!(text.contains("`--max-tokens 4096`"), "{text}");

    // A real budget is a bound like any other: it is stated in the header, and the run still stops at the
    // attempt bound because a scripted answer reports no spend to count.
    let (code, text) = hades(&[
        "run",
        "make a thing",
        "--budget-tokens",
        "50000",
        "--program",
        "true",
        "--program",
        "true",
        "--verify",
        "test -f never.txt",
        "--policy",
        "open",
        "--root",
        &root_arg,
        "--state",
        &state_arg,
        "--attempts",
        "2",
    ]);
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("50000 tokens"), "{text}");
    assert!(text.contains("cause: attempts"), "{text}");

    // `turn` has no budget: --max-tokens there is the per-call completion cap, and a run budget under the
    // same name would be a second meaning for one flag.
    let (code, text) = hades(&["turn", "make a thing", "--budget-tokens", "100"]);
    assert_eq!(code, 2, "{text}");
    assert!(
        text.contains("`--budget-tokens` is not a flag this command takes"),
        "{text}"
    );
}

#[test]
fn run_gives_up_at_the_bound_and_exits_one() {
    let root = scratch("run-bounds");
    let state = root.join("state");
    let root_arg = root.display().to_string();
    let state_arg = state.display().to_string();

    // One program, which never makes the file, and a bound of one attempt: the graph has nothing left to
    // try and says so rather than looping.
    let (code, text) = hades(&[
        "run",
        "make made.txt exist",
        "--program",
        "true",
        "--policy",
        "open",
        "--verify",
        "test -f made.txt",
        "--root",
        &root_arg,
        "--state",
        &state_arg,
        "--session",
        "two",
        "--attempts",
        "1",
    ]);

    assert_eq!(code, 1, "{text}");
    assert!(text.contains("gave up after 1 attempt(s)"), "{text}");
    assert!(
        text.contains("the bounds were spent after 1 attempt(s)"),
        "the reason names the bounds: {text}"
    );
    // The bound is a condition on an edge, so both the attempt and the node it led to were left in it.
    assert!(text.contains("Exhausted"), "{text}");
}

#[test]
fn run_with_a_guarded_policy_ends_awaiting_an_answer_and_says_how_to_give_it() {
    let root = scratch("run-ask");
    let state = root.join("state");
    let root_arg = root.display().to_string();
    let state_arg = state.display().to_string();

    let (code, text) = hades(&[
        "run",
        "make made.txt exist",
        "--program",
        "touch made.txt",
        "--policy",
        "guarded",
        "--verify",
        "test -f made.txt",
        "--root",
        &root_arg,
        "--state",
        &state_arg,
        "--session",
        "three",
    ]);

    // Awaiting is not failing: the question was written down and a person can answer it from anywhere.
    assert_eq!(code, 2, "{text}");
    assert!(text.contains("awaiting an answer to question #0"), "{text}");
    assert!(text.contains("hades answer 0 allow"), "{text}");
    // Nothing ran, so there is nothing to check.
    assert!(!root.join("made.txt").exists());

    let (code, text) = hades(&[
        "answer", "0", "allow", "--scope", "session", "--state", &state_arg,
    ]);
    assert_eq!(code, 0, "{text}");

    // Answered, the same walk now runs the program and accepts it.
    let (code, text) = hades(&[
        "run",
        "make made.txt exist",
        "--program",
        "touch made.txt",
        "--policy",
        "guarded",
        "--verify",
        "test -f made.txt",
        "--root",
        &root_arg,
        "--state",
        &state_arg,
        "--session",
        "three",
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("accepted"), "{text}");
}

#[test]
fn run_needs_an_answerer_like_a_single_turn_does() {
    let (code, text) = hades(&["run", "do the thing", "--policy", "open"]);
    assert_eq!(code, 2);
    assert!(text.contains("--program"), "{text}");
    assert!(text.contains("--model"), "{text}");
}

/// The turn and the walk must build the same cage.
///
/// They used to assemble one each, inline, and the copies had already drifted: only the turn's asked each
/// language runtime where its standard library lives, so a program that started under `turn` died under
/// `run`. The sandbox a program ran in is recorded in the log, so the equality is checkable rather than a
/// claim about the source.
#[test]
fn the_walk_and_the_turn_build_the_same_cage() {
    let dir = scratch("one-cage");
    let state = dir.join("state");
    let state_arg = state.display().to_string();
    let root_arg = dir.display().to_string();

    let sandbox_of = |session: &str| -> String {
        log_lines(&state, session)
            .iter()
            .find_map(|entry| {
                (entry["kind"] == "tool_call" && entry["payload"]["tool"] == "shell").then(|| {
                    entry["payload"]["sandbox"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string()
                })
            })
            .unwrap_or_else(|| panic!("no caged program in session {session}"))
    };

    let (code, text) = hades(&[
        "turn",
        "say hello",
        "--root",
        &root_arg,
        "--state",
        &state_arg,
        "--session",
        "t-one",
        "--program",
        "true",
        "--policy",
        "open",
        "--verify",
        "true",
        "--cage",
        "on",
    ]);
    assert_eq!(code, 0, "{text}");
    let (code, text) = hades(&[
        "run",
        "say hello",
        "--root",
        &root_arg,
        "--state",
        &state_arg,
        "--session",
        "r-one",
        "--program",
        "true",
        "--policy",
        "open",
        "--verify",
        "true",
        "--attempts",
        "1",
        "--cage",
        "on",
        "--baseline",
        "off",
    ]);
    assert_eq!(code, 0, "{text}");

    let turn_cage = sandbox_of("t-one");
    let walk_cage = sandbox_of("r-one");
    assert!(!turn_cage.is_empty(), "the cage was not recorded at all");
    assert_eq!(
        turn_cage, walk_cage,
        "the walk and the turn confined the same program differently"
    );
    assert!(turn_cage.contains("readable:"), "{turn_cage}");
}

/// A cage has to let a program start the language runtime it needs.
///
/// `python3` on the test host resolves to a virtual environment under the home directory, and the cage refuses
/// the home directory. Unless the cage asks each runtime where its standard library lives, Python dies with
/// `ModuleNotFoundError: No module named 'encodings'` before it runs a line, and a model that finds a broken
/// interpreter concludes the box is broken and spends its attempts diagnosing it instead of the task.
#[test]
fn the_cage_lets_a_language_runtime_start() {
    let dir = scratch("cage-python");
    let state = dir.join("state");
    let state_arg = state.display().to_string();
    let root_arg = dir.display().to_string();

    let (code, text) = hades(&[
        "run",
        "print a number",
        "--root",
        &root_arg,
        "--state",
        &state_arg,
        "--session",
        "py",
        "--program",
        "python3 -c \"print(41 + 1)\"",
        "--policy",
        "open",
        "--verify",
        "python3 -c \"print(42)\"",
        "--attempts",
        "1",
        "--cage",
        "on",
        "--baseline",
        "off",
    ]);
    assert_eq!(code, 0, "{text}");

    let output = log_lines(&state, "py")
        .iter()
        .filter(|entry| entry["kind"] == "tool_result" && entry["payload"]["tool"] == "shell")
        .filter_map(|entry| entry["payload"]["output"].as_str().map(str::to_string))
        .next()
        .expect("the program's output is in the log");
    assert!(
        output.contains("42"),
        "python never started inside the cage; the program said {output:?}"
    );
}

/// A turn records the tree state its program started from, so a change can be attributed and undone.
#[test]
fn a_turn_records_which_snapshot_ran_the_program() {
    let dir = scratch("snapshot-recorded");
    let root = dir.join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let state = dir.join("state");
    std::fs::write(root.join("a.txt"), "before\n").unwrap();
    let (root_arg, state_arg) = (root.display().to_string(), state.display().to_string());

    let (code, text) = hades(&[
        "turn",
        "rewrite a.txt",
        "--root",
        &root_arg,
        "--state",
        &state_arg,
        "--session",
        "snap",
        "--program",
        "printf 'after\\n' > a.txt",
        "--policy",
        "open",
        "--verify",
        "test -f a.txt",
    ]);
    assert_eq!(code, 0, "{text}");

    // The check that runs afterwards is a check and not an effect, so it carries no snapshot of its own.
    let recorded: Vec<String> = log_lines(&state, "snap")
        .iter()
        .filter(|entry| entry["kind"] == "tool_call")
        .filter_map(|entry| entry["payload"]["snapshot"].as_str().map(str::to_string))
        .collect();
    assert_eq!(recorded, vec!["snap#1".to_string()], "{text}");

    let taken = std::fs::read_dir(state.join("shadows/snap"))
        .expect("the snapshot store")
        .count();
    assert!(
        taken >= 2,
        "the copy and its manifest are both there: {taken}"
    );
}

/// The whole point of the snapshot: the tree goes back, and a file the program invented goes away.
#[test]
fn undo_puts_back_what_a_turn_changed() {
    let dir = scratch("undo");
    let root = dir.join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let state = dir.join("state");
    std::fs::write(root.join("a.txt"), "before\n").unwrap();
    let (root_arg, state_arg) = (root.display().to_string(), state.display().to_string());

    let (code, text) = hades(&[
        "turn",
        "rewrite a.txt",
        "--root",
        &root_arg,
        "--state",
        &state_arg,
        "--session",
        "u",
        "--program",
        "printf 'after\\n' > a.txt; printf 'stray\\n' > stray.txt",
        "--policy",
        "open",
        "--verify",
        "test -f stray.txt",
    ]);
    assert_eq!(code, 0, "{text}");
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        "after\n"
    );

    let (code, text) = hades(&["undo", "u", "--root", &root_arg, "--state", &state_arg]);
    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains("put back 1, removed 1"),
        "the undo should account for what it did: {text}"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        "before\n",
        "the undo did not put the file back"
    );
    assert!(
        !root.join("stray.txt").exists(),
        "the undo did not remove the file the program invented"
    );
}

/// A second turn on a session id is a resume, and it says what the log carried into it.
#[test]
fn a_second_turn_on_a_session_reports_what_the_first_one_left_behind() {
    let dir = scratch("history");
    let root = dir.join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let state = dir.join("state");
    let (root_arg, state_arg) = (root.display().to_string(), state.display().to_string());

    let turn = |program: &str, verify: &str| {
        hades(&[
            "turn",
            "make stats.sh exist",
            "--root",
            &root_arg,
            "--state",
            &state_arg,
            "--session",
            "h",
            "--program",
            program,
            "--policy",
            "open",
            "--verify",
            verify,
        ])
    };

    let (code, text) = turn("printf 'a\\n' > a.txt", "test -f stats.sh");
    assert_eq!(code, 1, "{text}");
    assert!(
        text.contains("history: nothing carried over from the log"),
        "the first turn had no history to carry: {text}"
    );

    let (code, text) = turn("touch stats.sh", "test -f stats.sh");
    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains("history: 1 tried, 0 accepted"),
        "the second turn should be told about the failed check in front of it: {text}"
    );

    // And a third turn sees the second one's pass as an accepted constraint, which is what "the log is the
    // memory" means in practice.
    let (code, text) = turn("touch stats.sh", "test -f stats.sh");
    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains("history: 1 tried, 2 accepted"),
        "a pass is carried forward as a constraint the next attempt has to honour: {text}"
    );
}

/// An undo with nothing to undo says so rather than reporting a restore that did nothing.
#[test]
fn undo_on_a_session_that_never_ran_says_so() {
    let dir = scratch("undo-nothing");
    let root = dir.join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let state = dir.join("state");
    let (root_arg, state_arg) = (root.display().to_string(), state.display().to_string());

    let (code, text) = hades(&["undo", "never", "--root", &root_arg, "--state", &state_arg]);
    assert_ne!(code, 0, "an undo with no snapshot is not a success: {text}");
    assert!(text.contains("no snapshots"), "{text}");
}

/// A compaction shortens the view and not the log, and the next turn is told what was cut.
///
/// The output is large on purpose: a try that carries a program and three kilobytes of output is what makes
/// history expensive, and history is what a compaction is for. The check fails so every turn leaves one.
#[test]
fn a_history_too_big_to_carry_is_cut_and_the_log_keeps_it() {
    let dir = scratch("compact");
    let root = dir.join("repo");
    std::fs::create_dir_all(&root).expect("a repo");
    std::fs::write(root.join("a.txt"), "hi\n").expect("a file");
    let state = dir.join("state");
    std::fs::create_dir_all(&state).expect("a state directory");
    let root = root.display().to_string();
    let state = state.display().to_string();

    let turn = |session: &str| {
        hades(&[
            "turn",
            "make the check pass",
            "--root",
            &root,
            "--program",
            "seq 1 2000",
            "--verify",
            "false",
            "--policy",
            "open",
            "--state",
            &state,
            "--session",
            session,
            "--shadow",
            "off",
            "--context",
            "900",
        ])
    };

    let (code, first) = turn("s");
    assert_eq!(
        code, 1,
        "the check failed, so the turn's code is 1: {first}"
    );
    assert!(!first.contains("compacted:"), "{first}");

    let (code, second) = turn("s");
    assert_eq!(code, 1, "{second}");
    assert!(second.contains("compacted:"), "history was due: {second}");

    let log = log_lines(std::path::Path::new(&state), "s");
    let cut = log
        .iter()
        .position(|entry| entry["kind"] == "compaction")
        .expect("a compaction line");
    assert!(
        log.iter().any(|entry| entry["kind"] == "tool_call"),
        "the evidence before the cut is still there"
    );
    assert!(
        serde_json::to_string(&log[cut]["payload"])
            .expect("json")
            .contains("compacted by the harness"),
        "the summary says who wrote it"
    );

    let (code, third) = turn("s");
    assert_eq!(code, 1, "{third}");
    assert!(
        third.contains("and a compacted summary"),
        "the next turn reads the summary: {third}"
    );
}

/// A turn keeps the map's scan, and the second turn over the same state pays only the stat walk.
///
/// This is P3b's proof at small scale: before the cache was threaded into `context::sections`, every turn
/// built the map cold and the cache that `hades map` maintained was ignored by the only caller that runs
/// often enough to care.
#[test]
fn a_turn_over_a_known_tree_is_warm_the_second_time() {
    let dir = scratch("cache-warm");
    let root = dir.join("repo");
    std::fs::create_dir_all(root.join("src")).expect("a repo");
    std::fs::write(root.join("src/one.rs"), "fn one() {}\n").expect("a file");
    std::fs::write(root.join("src/two.rs"), "fn two() {}\n").expect("a file");
    std::fs::write(root.join("notes.md"), "# notes\n").expect("a file");
    let state = dir.join("state");
    std::fs::create_dir_all(&state).expect("a state directory");
    let root = root.display().to_string();
    let state = state.display().to_string();

    let (code, cold) = hades(&[
        "turn",
        "the cache",
        "--root",
        &root,
        "--program",
        "true",
        "--verify",
        "true",
        "--state",
        &state,
        "--session",
        "cold",
        "--shadow",
        "off",
    ]);
    assert_eq!(code, 0, "{cold}");
    assert!(cold.contains("0 hits, 3 misses"), "{cold}");

    let (code, warm) = hades(&[
        "turn",
        "the cache",
        "--root",
        &root,
        "--program",
        "true",
        "--verify",
        "true",
        "--state",
        &state,
        "--session",
        "warm",
        "--shadow",
        "off",
    ]);
    assert_eq!(code, 0, "{warm}");
    assert!(warm.contains("3 hits, 0 misses"), "{warm}");
    assert!(
        Path::new(&state).join("repomap.json").exists(),
        "the cache is written beside the state"
    );
}

/// A scratch base inside this crate's own `target/`, never the machine's temp directory.
///
/// The agent that runs these tests keeps its temp under the live install, and Hades refuses to write there:
/// that refusal is the boundary doing its job, so the tests must not ask it to make an exception.
fn scratch_base() -> std::path::PathBuf {
    // Created and then canonicalized: a path with `..` in it compares unequal to the same directory reached
    // by the store, and the snapshot's never-touch test is a prefix test. The first run of this helper put the
    // store inside the tree it was copying, and the copy recursed into itself until the kernel refused the
    // file name.
    let base = harness::scratch::root();
    std::fs::create_dir_all(&base).ok();
    std::fs::canonicalize(&base).unwrap_or(base)
}

// The hard floor, at the second door. A chat refuses (proven inside `agent::execute`); a governed turn
// has a person to ask, so the floor becomes a question even when the policy allowed the shell.

#[test]
fn a_read_surface_is_not_handed_a_tool_that_changes_anything() {
    let (code, text) = hades(&["tools", "--mode", "read"]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("no tool that changes anything"), "{text}");
    for kept in [
        "read_file",
        "list_dir",
        "search",
        "fetch",
        "web_search",
        "delegate",
    ] {
        assert!(
            text.contains(&format!("\n  {kept}\n")),
            "{kept} missing: {text}"
        );
    }
    for gone in [
        "shell",
        "write_file",
        "patch",
        "remember",
        "cron",
        "browser",
    ] {
        assert!(
            !text.contains(&format!("\n  {gone}\n")),
            "{gone} is in a read surface: {text}"
        );
    }

    let (code, text) = hades(&["tools", "--mode", "act"]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("\n  shell\n"), "{text}");
    assert!(text.contains("\n  cron\n"), "{text}");

    let (code, text) = hades(&["tools", "--mode", "write"]);
    assert_eq!(code, 2, "{text}");
    assert!(text.contains("say read or act"), "{text}");
}

// The standing goal file is the record a person corrects, so the listing shows the path and the fields,
// and a file that will not parse is named rather than silently absent.
#[test]
fn jobs_are_listed_with_their_fields_and_bad_files_are_named() {
    let state = scratch("jobs-listing");
    let folder = state.join("jobs");
    std::fs::create_dir_all(&folder).expect("the jobs folder");
    std::fs::write(
        folder.join("2-digest.md"),
        "---\nid: 2\nchat: 7\nschedule: every 6h\nmode: read\ndone_when: five items\nnever: do not post\n---\n\nWrite the digest.\n",
    )
    .expect("a job file");
    std::fs::write(
        folder.join("3-broken.md"),
        "---\nchat: 7\nschedule: every 6h\n---\n\nno id\n",
    )
    .expect("a file with no id");
    let (code, text) = hades(&["jobs", "--state", &state.display().to_string()]);
    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains("2 · every 6h · read · jobs/2-digest.md"),
        "{text}"
    );
    assert!(text.contains("done_when: five items"), "{text}");
    assert!(text.contains("never: do not post"), "{text}");
    assert!(text.contains("not run: jobs/3-broken.md"), "{text}");
    assert!(text.contains("no numeric `id`"), "{text}");
    let _ = std::fs::remove_dir_all(&state);
}

// Cancelling is a file verb: the job stops being due and a pending retry goes with it. The ledger names
// the surface that asked, the same way an allow does, and cancelling a job that is not there is an error.
#[test]
fn cancel_removes_the_job_and_the_pending_retry_and_says_so() {
    let state = scratch("cancel");
    std::fs::create_dir_all(state.join("jobs")).expect("jobs");
    std::fs::create_dir_all(state.join("retry")).expect("retry");
    std::fs::write(
        state.join("jobs/5-report.md"),
        "---\nid: 5\nchat: 7\nschedule: every 6h\nmode: read\n---\n\nWrite the report.\n",
    )
    .expect("a job file");
    std::fs::write(
        state.join("retry/5.json"),
        r#"{"window":"5@1760000000","attempt":1,"at":1760000100,"error":"timed out"}"#,
    )
    .expect("a pending retry");

    let state_arg = state.display().to_string();
    let (code, text) = hades(&["cancel", "5", "--state", &state_arg]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("cancelled job 5"), "{text}");
    assert!(!state.join("jobs/5-report.md").exists(), "{text}");
    assert!(!state.join("retry/5.json").exists(), "{text}");

    let ledger = harness::activity::read(&state);
    let line = ledger.last().expect("a cancel line");
    assert_eq!(line.decision, "stop", "{line:?}");
    assert!(line.actor.starts_with("cli:"), "{line:?}");
    assert_eq!(line.artifact, "jobs/5-report.md", "{line:?}");

    let (code, text) = hades(&["cancel", "99", "--state", &state_arg]);
    assert_eq!(code, 2, "{text}");
    assert!(text.contains("there is no job 99"), "{text}");
    let _ = std::fs::remove_dir_all(&state);
}

// The ledger answers "what did it decide" after the fact, so the command shows every field, oldest
// first, and honours the limit.
#[test]
fn the_activity_ledger_is_readable_in_order() {
    let state = scratch("activity-listing");
    let body = concat!(
        "{\"at\":1760000000,\"actor\":\"chat:7\",\"layer\":\"floor\",\"action\":\"shell: rm -f keep.txt\",\"decision\":\"refuse\",\"reason\":\"permanent delete: removing is forever\",\"artifact\":\"\"}\n",
        "{\"at\":1760000060,\"actor\":\"job:2\",\"layer\":\"surface\",\"action\":\"write_file: notes.md\",\"decision\":\"allow\",\"reason\":\"\",\"artifact\":\"notes.md\"}\n"
    );
    std::fs::write(state.join("activity.jsonl"), body).expect("a ledger");
    let state_arg = state.display().to_string();
    let (code, text) = hades(&["activity", "--state", &state_arg]);
    assert_eq!(code, 0, "{text}");
    let rows: Vec<&str> = text.lines().collect();
    assert_eq!(rows.len(), 2, "{text}");
    assert!(rows[0].contains("chat:7"), "{text}");
    assert!(rows[0].contains("floor"), "{text}");
    assert!(rows[0].contains("refuse"), "{text}");
    assert!(rows[0].contains("shell: rm -f keep.txt"), "{text}");
    assert!(rows[0].contains("permanent delete"), "{text}");
    assert!(rows[1].contains("job:2"), "{text}");
    assert!(rows[1].contains("allow"), "{text}");
    assert!(rows[1].contains("-> notes.md"), "{text}");
    let (code, last) = hades(&["activity", "--state", &state_arg, "--limit", "1"]);
    assert_eq!(code, 0, "{last}");
    assert_eq!(last.lines().count(), 1, "{last}");
    assert!(last.contains("job:2"), "{last}");
    let _ = std::fs::remove_dir_all(&state);
}

#[test]
fn a_floored_program_asks_even_under_an_open_policy() {
    let root = scratch("floor-open");
    let state = root.join("state");
    let root_arg = root.display().to_string();
    let state_arg = state.display().to_string();
    std::fs::write(root.join("keep.txt"), "still here").expect("a file to delete");

    let turn = |program: &str| {
        let (code, text) = hades(&[
            "turn",
            "remove the note",
            "--program",
            program,
            "--policy",
            "open",
            "--verify",
            "true",
            "--root",
            &root_arg,
            "--state",
            &state_arg,
            "--session",
            "floor-open",
        ]);
        (code, text)
    };

    let (_, text) = turn("rm -f keep.txt");
    assert!(
        text.contains("waiting on question #0"),
        "an open policy does not unlock the floor: {text}"
    );
    assert!(text.contains("permanent delete"), "{text}");
    assert!(
        root.join("keep.txt").exists(),
        "nothing ran while the question waits"
    );

    let (code, text) = hades(&[
        "answer", "0", "allow", "--scope", "session", "--state", &state_arg,
    ]);
    assert_eq!(code, 0, "{text}");

    let (code, text) = turn("rm -f keep.txt");
    assert_eq!(code, 0, "the answered category runs: {text}");
    assert!(
        !root.join("keep.txt").exists(),
        "the person's answer let it through"
    );
}

#[test]
fn a_standing_answer_to_the_shell_does_not_unlock_a_delete() {
    let root = scratch("floor-standing");
    let state = root.join("state");
    let root_arg = root.display().to_string();
    let state_arg = state.display().to_string();
    std::fs::write(root.join("keep.txt"), "still here").expect("a file to delete");

    let turn = |program: &str| {
        hades(&[
            "turn",
            "do the work",
            "--program",
            program,
            "--policy",
            "guarded",
            "--verify",
            "true",
            "--root",
            &root_arg,
            "--state",
            &state_arg,
            "--session",
            "floor-standing",
        ])
    };

    let (_, text) = turn("touch made.txt");
    assert!(text.contains("waiting on question #0"), "{text}");
    let (code, text) = hades(&[
        "answer", "0", "allow", "--scope", "session", "--state", &state_arg,
    ]);
    assert_eq!(code, 0, "{text}");

    let (_, text) = turn("rm -f keep.txt");
    assert!(
        text.contains("waiting on question #1"),
        "the shell answer is not a delete answer: {text}"
    );
    assert!(text.contains("permanent delete"), "{text}");
    assert!(root.join("keep.txt").exists(), "the delete waited");
}

#[test]
fn an_ordinary_program_is_not_stopped_by_the_floor() {
    let root = scratch("floor-plain");
    let state = root.join("state");
    let root_arg = root.display().to_string();
    let state_arg = state.display().to_string();

    let (code, text) = hades(&[
        "turn",
        "make a file",
        "--program",
        "touch fine.txt",
        "--policy",
        "open",
        "--verify",
        "true",
        "--root",
        &root_arg,
        "--state",
        &state_arg,
        "--session",
        "floor-plain",
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(!text.contains("waiting on question"), "{text}");
    assert!(root.join("fine.txt").exists());
}

// The harness is a file tree, so the command that reports it has to name the file, not just the profile.
#[test]
fn harness_names_the_files_a_run_would_load() {
    let dir = scratch("harness-files");
    std::fs::write(dir.join("system.md"), "You are terse.").expect("system");
    std::fs::write(dir.join("tool_guidance.md"), "Read before writing.").expect("guidance");
    let (code, text) = hades(&["harness", "--harness", dir.to_str().unwrap()]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("system.md"), "{text}");
    assert!(text.contains("tool_guidance.md"), "{text}");
    assert!(text.contains("effective:"), "{text}");

    let (code, text) = hades(&["harness", "--harness", "/no/such/harness"]);
    assert_eq!(code, 2, "{text}");
    assert!(text.contains("not a directory"), "{text}");
}

// Tune with given candidates is a probe loop, not a model call: two directories, one check, one table. It
// runs the real binary because its probes are new processes of that same binary.
#[test]
fn tune_probes_given_candidates_and_writes_the_library_note() {
    let exe = env!("CARGO_BIN_EXE_hades");
    let root = scratch("tune-root");
    let home = scratch("tune-home");
    let state = scratch("tune-state");
    let harnesses = scratch("tune-harnesses");
    let base = harnesses.join("base");
    let other = harnesses.join("other");
    std::fs::create_dir_all(&base).expect("base dir");
    std::fs::create_dir_all(&other).expect("other dir");
    std::fs::write(base.join("system.md"), "BASE").expect("base");
    std::fs::write(other.join("system.md"), "OTHER").expect("other");

    let output = std::process::Command::new(exe)
        .args([
            "tune",
            "create ok.txt with ok",
            "--root",
            root.to_str().unwrap(),
            "--verify",
            "test -f ok.txt",
            "--policy",
            "open",
            "--program",
            "printf ok > ok.txt",
            "--harness",
            base.to_str().unwrap(),
            "--candidate",
            other.to_str().unwrap(),
            "--state",
            state.to_str().unwrap(),
        ])
        .env("HADES_HOME", &home)
        .output()
        .expect("tune runs");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(text.contains("base       passed"), "{text}");
    assert!(text.contains("other      passed"), "{text}");
    assert!(text.contains("adopted:"), "{text}");

    let library = home.join("harness").join("library");
    let mut found = 0;
    for entry in std::fs::read_dir(&library).expect("the library") {
        if entry.expect("entry").path().join("EXPERIENCE.md").is_file() {
            found += 1;
        }
    }
    assert_eq!(found, 1, "one note for one tune");
}

// A scripted run has no judge, and the receipt must say which kind of "unjudged" this is: a profile that
// asks and has none attached is a different fact from a profile that never asked.
#[test]
fn a_run_receipt_names_the_judge_it_did_or_did_not_have() {
    let root = scratch("judge-receipt-root");
    let state = scratch("judge-receipt-state");
    let (code, text) = hades(&[
        "run",
        "say hello",
        "--profile",
        "fix",
        "--program",
        "true",
        "--policy",
        "open",
        "--verify",
        "true",
        "--root",
        root.to_str().unwrap(),
        "--state",
        state.to_str().unwrap(),
        "--session",
        "judge-receipt",
        "--baseline",
        "off",
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains("judge:   unjudged: this profile asks for a judge and none is attached"),
        "{text}"
    );

    let root = scratch("judge-receipt-root-2");
    let state = scratch("judge-receipt-state-2");
    let (code, text) = hades(&[
        "run",
        "say hello",
        "--profile",
        "survey",
        "--program",
        "true",
        "--policy",
        "open",
        "--verify",
        "true",
        "--root",
        root.to_str().unwrap(),
        "--state",
        state.to_str().unwrap(),
        "--session",
        "judge-receipt-2",
        "--baseline",
        "off",
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains("judge:   unjudged: this profile does not ask for a judge"),
        "{text}"
    );
}

#[test]
fn tune_says_when_a_probe_stopped_at_an_approval_question() {
    let dir = scratch("tune-parked");
    let root = dir.join("root");
    std::fs::create_dir_all(&root).expect("a root");
    std::fs::create_dir_all(root.join("keep")).expect("a directory to delete");
    let state = dir.join("state");
    let mut candidates = Vec::new();
    for name in ["cand-a", "cand-b"] {
        let path = dir.join(name);
        std::fs::create_dir_all(&path).expect("a candidate directory");
        std::fs::write(path.join("system.md"), "You are a test profile.").expect("a system prompt");
        candidates.push(path.to_string_lossy().to_string());
    }
    let program = format!("rm -rf {}", root.join("keep").display());
    // The real binary, not the in-process CLI: a tune probes by starting `hades run`, and in a test binary
    // that child would be the Rust test harness, which refuses `--root`.
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_hades"))
        .env("HADES_HOME", dir.join("home"))
        .args([
            "tune",
            "remove the keep directory",
            "--candidate",
            &candidates[0],
            "--candidate",
            &candidates[1],
            "--verify",
            "true",
            "--program",
            &program,
            "--root",
            root.to_str().unwrap(),
            "--state",
            state.to_str().unwrap(),
        ])
        .output()
        .expect("the binary starts");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.status.code(), Some(0), "{text}");
    assert!(text.contains("awaiting (approval)"), "{text}");
    assert!(
        text.contains("suggestion: 3 probe(s) stopped at an approval question"),
        "{text}"
    );
    assert!(text.contains("--policy open"), "{text}");
}

#[test]
fn fork_copies_the_chain_to_an_entry_and_leaves_the_source_alone() {
    let dir = scratch("fork");
    let root = dir.join("root");
    std::fs::create_dir_all(&root).expect("a root");
    let state = dir.join("state");
    let (code, text) = hades(&[
        "run",
        "write one",
        "--program",
        "printf one > one.txt",
        "--verify",
        "test -f one.txt",
        "--policy",
        "open",
        "--root",
        root.to_str().unwrap(),
        "--state",
        state.to_str().unwrap(),
        "--session",
        "src",
    ]);
    assert_eq!(code, 0, "{text}");
    let (code, text) = hades(&[
        "run",
        "write two",
        "--program",
        "printf two > two.txt",
        "--verify",
        "test -f two.txt",
        "--policy",
        "open",
        "--root",
        root.to_str().unwrap(),
        "--state",
        state.to_str().unwrap(),
        "--session",
        "src",
    ]);
    assert_eq!(code, 0, "{text}");

    let source = state.join("sessions").join("src.jsonl");
    let before = std::fs::read(&source).expect("the source log");
    let (code, text) = hades(&[
        "fork",
        "src",
        "--at",
        "1",
        "--state",
        state.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("-> src-fork-1"), "{text}");
    assert!(text.contains("2 entries"), "{text}");

    let after = std::fs::read(&source).expect("the source log");
    assert_eq!(before, after, "the source was rewritten by a fork");

    let child = log_lines(&state, "src-fork-1");
    assert_eq!(child.len(), 2, "{child:#?}");
    assert_eq!(child[1]["id"], 1);
    assert_eq!(child[0]["payload"]["forked_from"], "src");
    assert_eq!(child[0]["payload"]["forked_at"], 1);
    assert_eq!(child[0]["payload"]["session_id"], "src-fork-1");

    // A fork of a fork is a fork, not a special case.
    let (code, text) = hades(&["fork", "src-fork-1", "--state", state.to_str().unwrap()]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("src-fork-1-fork-1"), "{text}");
}

#[test]
fn a_finished_run_writes_one_scoreboard_row_and_the_table_reads_it() {
    let dir = scratch("scoreboard");
    let root = dir.join("root");
    std::fs::create_dir_all(&root).expect("a root");
    let state = dir.join("state");
    let (code, text) = hades(&[
        "run",
        "create score.txt",
        "--program",
        "printf scored > score.txt",
        "--verify",
        "test -f score.txt",
        "--policy",
        "open",
        "--root",
        root.to_str().unwrap(),
        "--state",
        state.to_str().unwrap(),
        "--profile",
        "fix",
    ]);
    assert_eq!(code, 0, "{text}");

    let text = std::fs::read_to_string(state.join("scoreboard.jsonl")).expect("a scoreboard row");
    let rows: Vec<serde_json::Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("one JSON value per row"))
        .collect();
    assert_eq!(rows.len(), 1, "{rows:#?}");
    assert_eq!(rows[0]["profile"], "fix");
    assert_eq!(rows[0]["passed"], true);
    assert!(rows[0]["millis"].as_u64().is_some());

    let (code, table) = hades(&["scoreboard", "--state", state.to_str().unwrap()]);
    assert_eq!(code, 0, "{table}");
    assert!(table.contains("fix"), "{table}");
    assert!(table.contains("1 run(s)"), "{table}");
    assert!(table.contains("too few to choose"), "{table}");
}

#[test]
fn the_router_consults_the_scoreboard_only_when_no_rule_matched() {
    let dir = scratch("scoreboard-route");
    let state = dir.join("state");
    std::fs::create_dir_all(&state).expect("a state");
    let mut rows = Vec::new();
    for (profile, passed, millis) in [
        ("fix", false, 10u64),
        ("fix", false, 10),
        ("fix", false, 10),
        ("sweep", true, 900),
        ("sweep", true, 900),
        ("sweep", true, 900),
    ] {
        rows.push(serde_json::json!({
            "profile": profile,
            "passed": passed,
            "attempts": 1,
            "millis": millis,
            "at": 1
        }));
    }
    let text: String = rows
        .iter()
        .map(|row| row.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(state.join("scoreboard.jsonl"), format!("{text}\n")).expect("a scoreboard");

    let (code, said) = hades(&[
        "route",
        "improve the thing",
        "--state",
        state.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{said}");
    assert!(said.contains("profile: sweep"), "{said}");
    assert!(
        said.contains("the scoreboard picked the best pass rate"),
        "{said}"
    );

    // A rule still wins: "fix" is in the goal, so the scoreboard is not consulted.
    let (code, said) = hades(&["route", "fix the thing", "--state", state.to_str().unwrap()]);
    assert_eq!(code, 0, "{said}");
    assert!(said.contains("profile: fix"), "{said}");
    assert!(!said.contains("scoreboard"), "{said}");
}

#[test]
fn profile_bounds_and_shadow_are_the_defaults_a_flag_does_not_have_to_repeat() {
    let dir = scratch("profile-defaults");
    let root = dir.join("root");
    std::fs::create_dir_all(&root).expect("a root");
    let state = dir.join("state");

    // survey ships one attempt and no shadow. The goal must not match a rule, so the profile is named.
    let (code, said) = hades(&[
        "run",
        "make a thing",
        "--profile",
        "survey",
        "--program",
        "false",
        "--verify",
        "false",
        "--policy",
        "open",
        "--root",
        root.to_str().unwrap(),
        "--state",
        state.to_str().unwrap(),
        "--session",
        "survey-bounds",
    ]);
    assert_eq!(code, 1, "{said}");
    assert!(said.contains("1 attempt(s)"), "{said}");
    assert!(
        !state.join("shadows").join("survey-bounds").exists(),
        "survey ships no shadow, so no snapshot directory should exist"
    );

    // fix ships three attempts and a shadow.
    let (code, said) = hades(&[
        "run",
        "make another thing",
        "--profile",
        "fix",
        "--program",
        "printf ok > ok.txt",
        "--verify",
        "test -f ok.txt",
        "--policy",
        "open",
        "--root",
        root.to_str().unwrap(),
        "--state",
        state.to_str().unwrap(),
        "--session",
        "fix-defaults",
    ]);
    assert_eq!(code, 0, "{said}");
    assert!(
        state.join("shadows").join("fix-defaults").exists(),
        "fix ships a shadow, so a snapshot directory should exist"
    );
}

#[test]
fn a_proposal_is_listed_promoted_and_then_installed() {
    let state = scratch("skills");
    harness::skills::propose(
        &state,
        "fix",
        "make the parser strict",
        "cargo test",
        "cargo fix --allow-dirty",
        "session-1",
        2,
    )
    .expect("a proposal")
    .expect("a path");
    let state_flag = state.to_str().expect("utf-8");
    let (code, text) = hades(&["skills", "list", "--state", state_flag]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("make-the-parser-strict"), "{text}");
    assert!(text.contains("(1)"), "{text}");

    let (code, text) = hades(&[
        "skills",
        "promote",
        "make-the-parser-strict",
        "--state",
        state_flag,
    ]);
    assert_ne!(code, 0, "a TODO contract must not promote: {text}");
    assert!(
        text.to_lowercase().contains("contract"),
        "the refusal has to name the missing contract: {text}"
    );
    let (code, text) = hades(&[
        "skills",
        "promote",
        "make-the-parser-strict",
        "--force",
        "--state",
        state_flag,
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("promoted"), "{text}");
    assert!(
        state
            .join("skills/make-the-parser-strict/SKILL.md")
            .exists()
    );
    assert!(
        !state
            .join("skills/proposed/make-the-parser-strict.md")
            .exists()
    );

    let (code, text) = hades(&["skills", "list", "--state", state_flag]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("(0)"), "{text}");
}

#[test]
fn doctor_and_session_reads_never_need_a_model() {
    let state = scratch("doctor");
    let flag = state.to_str().expect("utf-8");
    std::fs::create_dir_all(state.join("sessions")).expect("a sessions dir");
    let mut log = session_tree::Session::create(
        session_tree::FileStore::new(state.join("sessions")),
        session_tree::SystemClock,
        session_tree::SessionId::new("demo"),
    )
    .expect("a log");
    log.append(
        session_tree::Kind::User,
        serde_json::json!({"text": "hello"}),
    )
    .expect("an entry");

    let (code, text) = hades(&["doctor", "--state", flag]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("warn config"), "{text}");
    assert!(text.contains("1 log(s)"), "{text}");

    let (code, text) = hades(&["session", "list", "--state", flag]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("demo"), "{text}");

    let (code, text) = hades(&["session", "show", "demo", "--state", flag]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("hello"), "{text}");
}

#[test]
fn update_reports_the_running_binary_and_refuses_a_missing_source() {
    let (code, text) = hades(&["update"]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("binary:"), "{text}");
    assert!(text.contains("--from"), "{text}");

    let (code, _) = hades(&["update", "--from", "/nonexistent/hades"]);
    assert_eq!(
        code, 2,
        "a missing source is an error, not a silent success"
    );
}

/// A fresh install has to be useful before a person writes anything. The seven built-ins ship in the
/// binary, so `init` seeds them into the state with no network and no repository checkout present.
#[test]
fn init_seeds_the_eleven_builtin_skills() {
    isolate_home();
    let home = PathBuf::from(std::env::var("HADES_HOME").expect("the isolated home"));
    // The home is process-wide for the CLI tests, and another test may have run `init` first. Clear
    // the built-ins so this test still proves that a fresh install seeds them.
    let _ = std::fs::remove_dir_all(home.join("skills").join("builtin"));
    let base = scratch("init-skills");
    let vault = base.join("vault");
    let config = base.join("config.json");
    let (code, text) = hades(&[
        "init",
        "--non-interactive",
        "--config",
        config.to_str().expect("a config path"),
        "--vault",
        vault.to_str().expect("a vault path"),
    ]);
    // The count is "written this time", and another test in this binary may have seeded the shared
    // home first. The proof is the files below, not the number in the receipt.
    assert_eq!(code, 0, "{text}");

    let builtin = home.join("skills").join("builtin");
    let mut names: Vec<String> = std::fs::read_dir(&builtin)
        .expect("the built-in directory")
        .flatten()
        .filter(|entry| entry.path().join("SKILL.md").is_file())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "build-an-mcp-server",
            "create-a-skill",
            "keep-memory-useful",
            "land-a-change",
            "plan-a-change",
            "repair-a-failing-check",
            "research-with-sources",
            "review-a-diff",
            "run-and-verify",
            "safety-before-acting",
            "write-for-humans",
        ]
    );
}

#[test]
fn explain_reads_a_session_without_a_model() {
    let state = scratch("explain");
    std::fs::create_dir_all(state.join("sessions")).expect("a sessions dir");
    let log = concat!(
        r#"{"kind":"user","payload":{"goal":"make it pass","root":"/work","profile":"fix"}}"#,
        "\n",
        r#"{"kind":"tool_call","payload":{"tool":"shell","program":"echo hi > out.txt","model":{"generated_tokens":12}}}"#,
        "\n",
        r#"{"kind":"tool_result","payload":{"tool":"shell","status":0,"output":""}}"#,
        "\n",
        r#"{"kind":"tool_call","payload":{"tool":"verify","program":"test -f out.txt"}}"#,
        "\n",
        r#"{"kind":"tool_result","payload":{"tool":"verify","state":"passed","output":""}}"#,
        "\n",
        r#"{"kind":"checkpoint","payload":{"check":"passed","command":"test -f out.txt"}}"#,
        "\n",
        r#"{"kind":"checkpoint","payload":{"condition":"Passed","ended":true,"node":"done"}}"#,
        "\n"
    );
    std::fs::write(state.join("sessions/s1.jsonl"), log).expect("a session");
    let (code, text) = hades(&["explain", "s1", "--state", state.to_str().expect("utf-8")]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("verify"), "{text}");
    assert!(text.contains("the check"), "{text}");
    assert!(text.contains("accepted true"), "{text}");
}

#[test]
fn explain_names_the_budget_that_ran_out() {
    // Reading a log has to answer "which allowance stopped it" from the record, not from a guess. A run
    // that ran out of tokens must not be reported as one that ran out of attempts: the two mean different
    // things to whoever decides whether to raise the budget, retry, or leave it alone.
    //
    // The sentence is the one the walk actually writes: `Bounds::declared` says which allowances were
    // declared, so the fixture cannot drift into a sentence no run could produce.
    let state = scratch("explain-cause");
    std::fs::create_dir_all(state.join("sessions")).expect("a sessions dir");
    let bounds = harness::Bounds::new(2, 10).with_tokens(1200);
    let reason = format!(
        "the bounds were spent after 1 attempt(s) ({}), and the check still failed",
        bounds.declared()
    );
    let log = [
        serde_json::json!({
            "kind": "user",
            "payload": {"goal": "make it pass", "root": "/work", "profile": "fix"},
        }),
        serde_json::json!({
            "kind": "checkpoint",
            "payload": {
                "node": "attempt",
                "condition": "Failed",
                "edge": "attempt -> attempt",
                "attempt": 1,
            },
        }),
        serde_json::json!({
            "kind": "checkpoint",
            "payload": {
                "node": "gave_up",
                "condition": "Exhausted",
                "ended": true,
                "exhausted": "tokens",
                "reason": reason,
                "attempt": 1,
            },
        }),
    ]
    .iter()
    .map(|entry| entry.to_string())
    .collect::<Vec<_>>()
    .join("\n");
    std::fs::write(state.join("sessions/s1.jsonl"), format!("{log}\n")).expect("a session");
    let (code, text) = hades(&["explain", "s1", "--state", state.to_str().expect("utf-8")]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("cause: tokens"), "{text}");
    assert!(
        !text.contains("cause: attempts"),
        "a token-exhausted run was blamed on the attempt count: {text}"
    );
    // The runner's whole sentence survives the read, including the bounds it names: that is what a person
    // reading the end needs in order to know which allowance to raise.
    assert!(
        text.contains(&format!("why: {reason}")),
        "the human sentence is not in the log: {text}"
    );
    assert!(
        reason.contains("2 attempts, 10 minutes, 1200 tokens"),
        "the fixture no longer says what the runner says: {reason}"
    );
}

#[test]
fn distill_refuses_a_session_that_did_not_finish_accepted() {
    let state = scratch("distill-refuse");
    std::fs::create_dir_all(state.join("sessions")).expect("a sessions dir");
    std::fs::write(
        state.join("sessions/s1.jsonl"),
        concat!(
            r#"{"kind":"user","payload":{"goal":"make it pass","root":"/work","profile":"fix"}}"#,
            "\n",
            r#"{"kind":"checkpoint","payload":{"condition":"Failed","ended":true,"node":"gave_up"}}"#,
            "\n"
        ),
    )
    .expect("a session");
    let (code, text) = hades(&["distill", "s1", "--state", state.to_str().expect("utf-8")]);
    assert_eq!(code, 2, "{text}");
    assert!(text.contains("did not finish accepted"), "{text}");
}

#[test]
fn replay_asks_for_the_check_it_deliberately_does_not_store() {
    let root = scratch("replay");
    let book = root.join("book.md");
    std::fs::write(
        &book,
        "---\nname: x\n---\n## When to use\nx\n## Steps\n1. x\n## Check\nx\n",
    )
    .expect("a runbook");
    let (code, text) = hades(&[
        "replay",
        book.to_str().expect("utf-8"),
        "--root",
        root.to_str().expect("utf-8"),
    ]);
    assert_eq!(code, 2, "{text}");
    assert!(text.contains("--check"), "{text}");
}

#[test]
fn tune_refuses_a_candidate_that_fails_the_holdout() {
    let exe = env!("CARGO_BIN_EXE_hades");
    let root = scratch("tune-holdout-root");
    let home = scratch("tune-holdout-home");
    let state = scratch("tune-holdout-state");
    let harnesses = scratch("tune-holdout-harnesses");
    let base = harnesses.join("base");
    let other = harnesses.join("other");
    std::fs::create_dir_all(&base).expect("base dir");
    std::fs::create_dir_all(&other).expect("other dir");
    std::fs::write(base.join("system.md"), "BASE").expect("base");
    std::fs::write(other.join("system.md"), "OTHER").expect("other");

    let output = std::process::Command::new(exe)
        .args([
            "tune",
            "create ok.txt with ok",
            "--root",
            root.to_str().unwrap(),
            "--verify",
            "test -f ok.txt",
            "--holdout",
            "test -f hold.txt",
            "--policy",
            "open",
            "--program",
            "printf ok > ok.txt",
            "--harness",
            base.to_str().unwrap(),
            "--candidate",
            other.to_str().unwrap(),
            "--state",
            state.to_str().unwrap(),
        ])
        .env("HADES_HOME", &home)
        .output()
        .expect("tune runs");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(text.contains("hold-no"), "{text}");
    assert!(text.contains("adopted: none (base kept)"), "{text}");
}

#[test]
fn a_failed_branch_reports_the_best_partial_progress() {
    let root = scratch("branch-partial-root");
    let state = scratch("branch-partial-state");
    let check = "if [ -f b.txt ]; then printf 'HADES-CHECK {\"gate\":false,\"passed\":2,\"total\":3}\\n'; \
                 else printf 'HADES-CHECK {\"gate\":false,\"passed\":1,\"total\":3}\\n'; fi; exit 1";
    let (code, text) = hades(&[
        "run",
        "make the check pass",
        "--root",
        root.to_str().unwrap(),
        "--verify",
        check,
        "--program",
        "touch a.txt",
        "--program",
        "touch b.txt",
        "--branch",
        "2",
        "--attempts",
        "2",
        "--minutes",
        "2",
        "--policy",
        "open",
        "--state",
        state.to_str().unwrap(),
    ]);
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("2/3"), "{text}");
    assert!(
        !root.join("a.txt").exists() && !root.join("b.txt").exists(),
        "a failed branch still restores the tree"
    );
}

/// An endpoint that answers every request with a 500 and counts what arrived.
///
/// The count is the point: a strict request that fell through to a second model would leave a second
/// connection behind, and a test that only read the fault text could not tell the difference.
fn counting_failure() -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use std::io::{Read, Write};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a port");
    let port = listener.local_addr().expect("an address").port();
    let seen = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&seen);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            counter.fetch_add(1, Ordering::SeqCst);
            let mut raw: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let read = match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => read,
                };
                raw.extend_from_slice(&chunk[..read]);
                let Some(end) = raw.windows(4).position(|window| window == b"\r\n\r\n") else {
                    continue;
                };
                let head = String::from_utf8_lossy(&raw[..end]).to_ascii_lowercase();
                let length = head
                    .split("\r\n")
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if raw.len() >= end + 4 + length {
                    break;
                }
            }
            let body = "{\"error\":{\"message\":\"this model is not served here\"}}";
            let _ = write!(
                stream,
                "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\n\r\n{body}",
                body.len()
            );
        }
    });
    (format!("http://127.0.0.1:{port}/v1/chat/completions"), seen)
}

/// An endpoint that answers one chat-completions call with a scripted JSON body, in the SSE shape the
/// client reads. The test passes the JSON it wants the model to have said.
fn scripted_chat(content: &str) -> String {
    use std::io::{Read, Write};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a port");
    let port = listener.local_addr().expect("an address").port();
    let content = content.to_string();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut raw: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let read = match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => read,
                };
                raw.extend_from_slice(&chunk[..read]);
                let Some(end) = raw.windows(4).position(|window| window == b"\r\n\r\n") else {
                    continue;
                };
                let head = String::from_utf8_lossy(&raw[..end]).to_ascii_lowercase();
                let length = head
                    .split("\r\n")
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if raw.len() >= end + 4 + length {
                    break;
                }
            }
            let delta = serde_json::json!({"choices":[{"delta":{"content": content}}]});
            let body = format!("data: {delta}\n\ndata: [DONE]\n\n");
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
        }
    });
    format!("http://127.0.0.1:{port}/v1/chat/completions")
}

#[test]
fn pack_adapt_writes_a_reviewable_proposal_from_a_model_reply() {
    let dir = scratch("pack-adapt-cli");
    let source = dir.join("source");
    std::fs::create_dir_all(source.join("skills/pack-skill")).expect("skill");
    std::fs::create_dir_all(source.join("harness/research")).expect("harness");
    std::fs::write(
        source.join("skills/pack-skill/SKILL.md"),
        "---\nname: pack-skill\ncontract: test\n---\n\nUse the old command.\n",
    )
    .expect("skill text");
    std::fs::write(source.join("harness/research/system.md"), "A harness.").expect("harness text");
    let archive = dir.join("adapt.hadepack");
    let (code, text) = hades(&[
        "pack",
        "export",
        "test/pack",
        "--from",
        source.to_str().expect("source"),
        "--skill",
        "pack-skill",
        "--profile",
        "research",
        "--derivatives",
        "allowed",
        "--out",
        archive.to_str().expect("archive"),
    ]);
    assert_eq!(code, 0, "{text}");

    let target = dir.join("target");
    std::fs::create_dir_all(&target).expect("target");
    let endpoint = scripted_chat(
        r#"{"text":"Use the new command.","changes":["new command"],"conflicts":[]}"#,
    );
    let (code, text) = hades(&[
        "pack",
        "adapt",
        archive.to_str().expect("archive"),
        "--item",
        "skill:pack-skill",
        "--state",
        target.to_str().expect("target"),
        "--endpoint",
        &endpoint,
        "--model",
        "test",
        "--accept",
    ]);
    assert_eq!(code, 0, "{text}");
    let proposal = target.join("skills/proposed/pack-skill/SKILL.md");
    assert!(proposal.is_file(), "{text}");
    let body = std::fs::read_to_string(proposal).expect("proposal");
    assert!(body.contains("Use the new command."), "{body}");
    assert!(body.contains("status: proposed"), "{body}");
    assert!(body.contains("contract: TODO"), "{body}");
    assert!(!target.join("skills/pack-skill").exists(), "nothing active");
}

#[test]
fn pack_merge_never_overwrites_the_existing_skill() {
    let dir = scratch("pack-merge-cli");
    let source = dir.join("source");
    std::fs::create_dir_all(source.join("skills/pack-skill")).expect("skill");
    std::fs::create_dir_all(source.join("harness/research")).expect("harness");
    std::fs::write(
        source.join("skills/pack-skill/SKILL.md"),
        "---\nname: pack-skill\ncontract: test\n---\n\nIncoming procedure.\n",
    )
    .expect("skill text");
    std::fs::write(source.join("harness/research/system.md"), "A harness.").expect("harness text");
    let archive = dir.join("merge.hadepack");
    let (code, text) = hades(&[
        "pack",
        "export",
        "test/pack",
        "--from",
        source.to_str().expect("source"),
        "--skill",
        "pack-skill",
        "--profile",
        "research",
        "--derivatives",
        "allowed",
        "--out",
        archive.to_str().expect("archive"),
    ]);
    assert_eq!(code, 0, "{text}");

    let target = dir.join("target");
    std::fs::create_dir_all(&target).expect("target");
    std::fs::write(
        target.join("decisions.json"),
        r#"{"items":[{"kind":"skill","identity":"pack-skill","action":"install"}]}"#,
    )
    .expect("decisions");
    let (code, text) = hades(&[
        "pack",
        "apply",
        archive.to_str().expect("archive"),
        "--plan",
        target.join("decisions.json").to_str().expect("plan"),
        "--state",
        target.to_str().expect("target"),
        "--activate",
    ]);
    assert_eq!(code, 0, "{text}");
    let active = target.join("skills/pack-skill/SKILL.md");
    let before = std::fs::read_to_string(&active).expect("active");

    let endpoint = scripted_chat(
        r#"{"text":"Merged procedure.","changes":["Merged procedure."],"conflicts":[]}"#,
    );
    let (code, text) = hades(&[
        "pack",
        "merge",
        archive.to_str().expect("archive"),
        "--item",
        "skill:pack-skill",
        "--into",
        "pack-skill",
        "--name",
        "pack-skill-merged",
        "--state",
        target.to_str().expect("target"),
        "--endpoint",
        &endpoint,
        "--model",
        "test",
        "--accept",
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(
        target
            .join("skills/proposed/pack-skill-merged/SKILL.md")
            .is_file()
    );
    assert_eq!(
        std::fs::read_to_string(&active).expect("active"),
        before,
        "merge must not overwrite the installed skill"
    );
}

#[test]
fn a_strict_turn_refuses_a_missing_key_before_a_session_is_created() {
    let dir = scratch("strict-key");
    let root = dir.join("root");
    std::fs::create_dir_all(&root).expect("a root");
    let root_arg = root.display().to_string();
    let state = dir.join("state");
    let state_arg = state.display().to_string();

    let (code, text) = hades(&[
        "turn",
        "make a note",
        "--model",
        "strict-ghost",
        "--endpoint",
        "https://ollama.com/v1/chat/completions",
        "--key",
        "env:HADES_STRICT_KEY_NOT_IN_THE_ENVIRONMENT",
        "--strict-model",
        "--root",
        &root_arg,
        "--state",
        &state_arg,
    ]);
    assert_eq!(code, 2, "{text}");
    assert!(
        text.contains("strict-ghost"),
        "the refusal does not name the model: {text}"
    );
    assert!(
        text.contains("ollama-cloud"),
        "the refusal does not name the provider: {text}"
    );
    assert!(
        text.contains("HADES_STRICT_KEY_NOT_IN_THE_ENVIRONMENT"),
        "{text}"
    );
    // The refusal came before a session existed, so there is no log of a turn that never had a model.
    assert!(
        !state.join("sessions/cli.jsonl").exists(),
        "a strict refusal opened a session anyway"
    );
}

#[test]
fn a_strict_turn_refuses_a_program_that_stands_in_for_a_model() {
    // A person at the keyboard is not a model, so there is nothing for a strict request to hold to. Said
    // here rather than silently run, because the flag's whole promise is that a model was named.
    let (code, text) = hades(&["turn", "make a note", "--program", "true", "--strict-model"]);
    assert_eq!(code, 2, "{text}");
    assert!(text.contains("names no model"), "{text}");
}

#[test]
fn a_strict_run_names_the_model_on_its_first_call_and_asks_for_no_other() {
    use std::sync::atomic::Ordering;

    let (endpoint, calls) = counting_failure();
    let dir = scratch("strict-run");
    let root = dir.join("root");
    std::fs::create_dir_all(&root).expect("a root");
    let root_arg = root.display().to_string();
    let state = dir.join("state");
    let state_arg = state.display().to_string();

    let (code, text) = hades(&[
        "run",
        "make a thing",
        "--model",
        "strict-ghost",
        "--endpoint",
        &endpoint,
        "--strict-model",
        "--attempts",
        "3",
        "--minutes",
        "2",
        "--profile",
        "fix",
        "--policy",
        "open",
        "--root",
        &root_arg,
        "--state",
        &state_arg,
    ]);
    // A fault ends the walk rather than spending another attempt or another model.
    assert_eq!(code, 1, "{text}");
    assert!(
        text.contains("strict-ghost"),
        "the fault does not name the model: {text}"
    );
    assert!(text.contains("500"), "{text}");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the run asked again after the model it named failed: {text}"
    );
}

#[test]
fn run_help_lists_the_strict_model_flag() {
    let (code, text) = hades(&["run", "--help"]);
    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains("--strict-model"),
        "run --help does not document --strict-model: {text}"
    );

    for command in ["turn", "submit"] {
        let (code, text) = hades(&[command, "--help"]);
        assert_eq!(code, 0, "{text}");
        assert!(
            text.contains("--strict-model"),
            "{command} --help does not document --strict-model: {text}"
        );
    }
}

// A generation is the harness an adoption replaced. `refine` reads the number back, and `--rollback` puts
// those bytes where they belong: the prompt, and the skill file that was edited alongside it.
#[test]
fn refine_lists_generations_and_rolls_one_back() {
    let exe = env!("CARGO_BIN_EXE_hades");
    let home = scratch("refine-home");
    let harness = home.join("harness").join("fix");
    let skill = home.join("skills").join("alpha").join("SKILL.md");
    std::fs::create_dir_all(&harness).expect("the harness directory");
    std::fs::create_dir_all(skill.parent().expect("the skill directory's parent"))
        .expect("the skill directory");
    std::fs::write(harness.join("system.md"), "before the edit").expect("the prompt");
    let skill_before = "---\ndescription: alpha\n---\n\nDo it once.\n";
    std::fs::write(&skill, skill_before).expect("the skill");

    let generation = harness::tuning::capture(
        &home,
        "fix",
        &harness,
        &home,
        "tune_1_other",
        "passed",
        &harness::tuning::guard_digest(),
    )
    .expect("a capture")
    .expect("a generation");
    assert_eq!(generation.id, 1);
    // The adoption the generation makes undoable: a prompt edit and a skill description edit.
    std::fs::write(harness.join("system.md"), "after the edit").expect("the adopted prompt");
    std::fs::write(
        &skill,
        "---\ndescription: alpha, revised\n---\n\nDo it twice.\n",
    )
    .expect("the edited skill");

    let listed = std::process::Command::new(exe)
        .args(["refine", "--profile", "fix", "--state"])
        .arg(&home)
        .env("HADES_HOME", &home)
        .output()
        .expect("refine runs");
    let text = String::from_utf8_lossy(&listed.stdout);
    assert!(listed.status.success(), "{text}");
    assert!(text.contains("harness generations for `fix`"), "{text}");
    assert!(
        text.contains("tune_1_other"),
        "the listing names the evidence: {text}"
    );

    let rolled = std::process::Command::new(exe)
        .args(["refine", "--rollback", "1", "--profile", "fix", "--state"])
        .arg(&home)
        .env("HADES_HOME", &home)
        .output()
        .expect("refine runs");
    let text = String::from_utf8_lossy(&rolled.stdout);
    assert!(
        rolled.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&rolled.stderr)
    );
    assert!(text.contains("restored generation 1"), "{text}");
    assert_eq!(
        std::fs::read_to_string(harness.join("system.md")).expect("the restored prompt"),
        "before the edit"
    );
    assert_eq!(
        std::fs::read_to_string(&skill).expect("the restored skill"),
        skill_before
    );

    let missing = std::process::Command::new(exe)
        .args(["refine", "--rollback", "9", "--profile", "fix", "--state"])
        .arg(&home)
        .env("HADES_HOME", &home)
        .output()
        .expect("refine runs");
    assert_eq!(
        missing.status.code(),
        Some(2),
        "a missing generation is a refusal"
    );
    assert!(
        String::from_utf8_lossy(&missing.stdout).contains("there is no generation 9"),
        "{}",
        String::from_utf8_lossy(&missing.stdout)
    );
}

// A successful `tune --adopt` on the profile's own harness path must leave a generation behind: the
// default harness directory, not a scratch `--harness`, because that is the path a person adopts into.
#[test]
fn tune_adopt_writes_generation_one_on_the_profiles_harness_path() {
    let exe = env!("CARGO_BIN_EXE_hades");
    let dir = scratch("adopt-generation");
    let home = dir.join("home");
    let root = dir.join("root");
    let state = dir.join("state");
    let harness = home.join("harness").join("fix");
    let candidate = dir.join("candidate");
    std::fs::create_dir_all(&harness).expect("the profile harness");
    std::fs::create_dir_all(&root).expect("the root");
    std::fs::create_dir_all(&candidate).expect("the candidate");
    std::fs::write(harness.join("system.md"), "BASE").expect("the base prompt");
    std::fs::write(candidate.join("system.md"), "OTHER").expect("the candidate prompt");

    // The check passes only on the second program run. The base probe runs first and fails; the candidate
    // is measured after it and passes, so "the candidate won" is a fact rather than a race on milliseconds.
    let program = "n=$(ls -1 *.step 2>/dev/null | wc -l); : > \"run$n.step\"; test -f run1.step";
    let output = std::process::Command::new(exe)
        .args([
            "tune",
            "make the second run pass",
            "--root",
            root.to_str().unwrap(),
            "--verify",
            "test -f run1.step",
            "--policy",
            "open",
            "--program",
            program,
            "--profile",
            "fix",
            "--candidate",
            candidate.to_str().unwrap(),
            "--state",
            state.to_str().unwrap(),
            "--adopt",
        ])
        .env("HADES_HOME", &home)
        .output()
        .expect("tune runs");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "{text}");
    assert!(
        text.contains("generation 1 keeps what it replaced"),
        "{text}"
    );
    assert_eq!(
        std::fs::read_to_string(harness.join("system.md")).expect("the adopted prompt"),
        "OTHER"
    );

    let generation = home.join("control").join("harness").join("fix").join("1");
    let manifest_path = generation.join("manifest.json");
    assert!(
        manifest_path.is_file(),
        "an adoption on the real path wrote no generation: {text}"
    );
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&manifest_path).expect("the manifest"))
            .expect("the manifest is JSON");
    assert_eq!(manifest["id"], 1);
    assert_eq!(manifest["parent"], serde_json::Value::Null);
    assert_eq!(
        manifest["harness"].as_str(),
        Some(
            std::fs::canonicalize(&harness)
                .expect("the harness path")
                .to_str()
                .expect("a UTF-8 path")
        ),
        "the generation records the harness it belongs to"
    );
    let named: Vec<&str> = manifest["files"]
        .as_array()
        .expect("a file list")
        .iter()
        .map(|file| file["path"].as_str().expect("a path"))
        .collect();
    assert_eq!(
        named,
        vec!["harness/system.md"],
        "the manifest names every captured file: {manifest}"
    );
    assert_eq!(
        std::fs::read_to_string(generation.join("harness").join("system.md"))
            .expect("the captured prompt"),
        "BASE",
        "the generation holds the harness the adoption replaced"
    );
}

// The guard runs before a probe, so a candidate that quotes the check costs nothing and leaves no evidence
// of an adoption that never happened.
#[test]
fn a_candidate_that_quotes_the_check_is_refused_before_anything_runs() {
    let exe = env!("CARGO_BIN_EXE_hades");
    let root = scratch("guard-root");
    let home = scratch("guard-home");
    let state = scratch("guard-state");
    let harnesses = scratch("guard-harnesses");
    let base = harnesses.join("base");
    let leaky = harnesses.join("leaky");
    std::fs::create_dir_all(&base).expect("the base harness");
    std::fs::create_dir_all(&leaky).expect("the candidate");
    std::fs::write(base.join("system.md"), "BASE").expect("the base prompt");
    std::fs::write(
        leaky.join("system.md"),
        "Make `test -f ok.txt` pass, then say you are done.",
    )
    .expect("a candidate that quotes the check");

    let output = std::process::Command::new(exe)
        .args([
            "tune",
            "create ok.txt with ok",
            "--root",
            root.to_str().unwrap(),
            "--verify",
            "test -f ok.txt",
            "--policy",
            "open",
            "--program",
            "printf ok > ok.txt",
            "--harness",
            base.to_str().unwrap(),
            "--candidate",
            leaky.to_str().unwrap(),
            "--state",
            state.to_str().unwrap(),
            "--adopt",
        ])
        .env("HADES_HOME", &home)
        .output()
        .expect("tune runs");
    let text = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(2), "{text}");
    assert!(text.contains("candidate `leaky` is refused"), "{text}");
    assert!(text.contains("quotes the check"), "{text}");
    assert!(
        !home.join("control").join("harness").exists(),
        "a refused candidate wrote a generation"
    );
    assert!(
        !state.join("sessions").exists(),
        "a refused candidate was probed: {}",
        text
    );
}

// A goal is session state, and the listing is the surface a person uses to see what Hades is still
// working on. It must read the files without a config and without a model: the isolated home used by
// these tests has no config, no key and no endpoint, so a listing that needed one could not succeed.
#[test]
fn goals_are_listed_from_files_without_a_model() {
    let state = scratch("goal-list");
    let folder = state.join("goals");
    std::fs::create_dir_all(&folder).expect("the goals folder");
    std::fs::write(
        folder.join("2-ship.json"),
        r#"{"id":2,"text":"ship the report","state":"open","session":"tg-7","chat":7,
            "model":"","check":"test -f out.txt","budget":3,"executor":"chat",
            "created":100,"updated":100,"heartbeat":0,"reason":"","last_check":""}"#,
    )
    .expect("a goal file");
    std::fs::write(folder.join("3-broken.json"), "{not json").expect("a broken file");

    let (code, text) = hades(&["goal", "list", "--state", &state.display().to_string()]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("ship the report"), "{text}");
    assert!(text.contains("goal 2 · open"), "{text}");
    assert!(text.contains("last heartbeat: never"), "{text}");
    assert!(text.contains("not read: goals/3-broken.json"), "{text}");
    let _ = std::fs::remove_dir_all(&state);
}

#[test]
fn a_goal_is_set_then_paused_and_graph_is_refused() {
    let state = scratch("goal-set");
    let state_arg = state.display().to_string();
    let (code, text) = hades(&[
        "goal", "set", "write", "the", "report", "--chat", "7", "--check", "true", "--budget", "3",
        "--state", &state_arg,
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("goal 1 is open for chat 7"), "{text}");
    assert!(text.contains("last heartbeat: never"), "{text}");
    assert!(state.join("goals/1-write-the-report.json").exists());

    let (code, text) = hades(&[
        "goal", "pause", "1", "--reason", "waiting", "--state", &state_arg,
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("goal 1 is paused: waiting"), "{text}");

    let (code, text) = hades(&["goal", "list", "--state", &state_arg]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("goal 1 · paused"), "{text}");
    assert!(text.contains("reason: waiting"), "{text}");

    let (code, text) = hades(&["goal", "clear", "1", "--state", &state_arg]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("goal 1 is cleared"), "{text}");
    let (code, text) = hades(&["goal", "list", "--state", &state_arg]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("goal 1 · cleared"), "{text}");

    let (code, text) = hades(&[
        "goal",
        "set",
        "graph",
        "work",
        "--chat",
        "7",
        "--executor",
        "graph",
        "--state",
        &state_arg,
    ]);
    assert_eq!(code, 2, "{text}");
    assert!(
        text.contains("executor `graph` is not implemented"),
        "{text}"
    );
    assert!(!state.join("goals/2-graph-work.json").exists());
    let _ = std::fs::remove_dir_all(&state);
}

// A paused goal must be resumable, including one the heartbeat paused because its budget was spent.
// Without this the only way out of a pause was to clear the goal and write it again.
#[test]
fn a_paused_goal_can_be_resumed() {
    let state = scratch("goal-resume");
    let state_arg = state.display().to_string();
    let (code, text) = hades(&[
        "goal", "set", "write", "the", "report", "--chat", "7", "--check", "true", "--budget", "3",
        "--state", &state_arg,
    ]);
    assert_eq!(code, 0, "{text}");

    let (code, text) = hades(&[
        "goal", "pause", "1", "--reason", "budget", "--state", &state_arg,
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("goal 1 is paused: budget"), "{text}");

    let (code, text) = hades(&["goal", "resume", "1", "--state", &state_arg]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("goal 1 is open"), "{text}");

    let (code, text) = hades(&["goal", "list", "--state", &state_arg]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("goal 1 · open"), "{text}");
    assert!(text.contains("reason: resumed"), "{text}");

    // Resuming an open goal is a refusal, not a second, quieter open.
    let (code, text) = hades(&["goal", "resume", "1", "--state", &state_arg]);
    assert_eq!(code, 2, "{text}");
    assert!(text.contains("transition"), "{text}");
    let _ = std::fs::remove_dir_all(&state);
}

#[test]
fn approvals_shows_each_option_and_answer_takes_the_key() {
    use harness::approval::{ApprovalBroker as _, OnDisk};
    use harness::question::{Ask, Choice, Effect};

    let root = scratch("approvals-options");
    let state = root.join("state");
    std::fs::create_dir_all(&state).expect("a state directory");
    let broker = OnDisk::open(state.join("approvals.jsonl")).expect("the record");
    let mut ask = Ask::choice(
        "Which store should this use?",
        "both fit the requirements",
        true,
        vec![
            Choice::new("sqlite", "SQLite", "one file, no server", Effect::Free).recommended(),
            Choice::new("postgres", "Postgres", "a server to run", Effect::Free),
        ],
    );
    ask.family = "storage".into();
    let prompt = ask.prompt.clone();
    broker
        .request_ask("chat-1", "question", &prompt, Some(ask))
        .expect("a question");

    let state_arg = state.display().to_string();
    let (code, text) = hades(&["approvals", "--state", &state_arg]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("Which store should this use?"), "{text}");
    assert!(
        text.contains("▸ sqlite"),
        "the recommendation is marked: {text}"
    );
    assert!(text.contains("postgres"), "every option is shown: {text}");
    assert!(text.contains("waiting"), "{text}");

    let (code, text) = hades(&[
        "answer",
        "0",
        "sqlite",
        "--note",
        "same as the last one",
        "--state",
        &state_arg,
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("answered allow"), "{text}");

    let (code, text) = hades(&["log", "--asks", "chat-1", "--state", &state_arg]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("option sqlite"), "{text}");
    assert!(text.contains("same as the last one"), "{text}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn taste_prints_the_recommendation_it_learned_and_forget_clears_it() {
    use harness::approval::{Answer, Scope};
    use harness::policy::Decision;
    use harness::question::{Ask, Choice, Effect};

    let root = scratch("taste-cli");
    let state = root.join("state");
    std::fs::create_dir_all(&state).expect("a state directory");
    let mut ask = Ask::choice(
        "Which store should this use?",
        "both fit",
        true,
        vec![
            Choice::new("postgres", "Postgres", "a server", Effect::Free).recommended(),
            Choice::new("sqlite", "SQLite", "one file", Effect::Free),
        ],
    );
    ask.family = "storage".into();
    for _ in 0..3 {
        let answer = Answer {
            decision: Decision::Allow,
            scope: Scope::Once,
            by: "operator".into(),
            at: "2026-10-20T12:00:00Z".into(),
            option: Some("sqlite".into()),
            note: None,
        };
        harness::taste::record(&state, &root, &ask, &answer).expect("a recorded answer");
    }
    let state_arg = state.display().to_string();
    let (code, text) = hades(&["taste", "--state", &state_arg]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("recommendation sqlite"), "{text}");
    assert!(text.contains("3 answers"), "{text}");

    let (code, text) = hades(&["taste", "forget", "all", "--state", &state_arg]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("forgot 3"), "{text}");
    let (_, text) = hades(&["taste", "--state", &state_arg]);
    assert!(text.contains("no answers recorded"), "{text}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn baseline_has_to_be_red_or_off() {
    // The flag is read before the engine is assembled, so a typo costs nothing and cannot reach a model.
    let (code, text) = hades(&["run", "make it pass", "--baseline", "maybe"]);
    assert_eq!(code, 2);
    assert!(text.contains("say red or off"), "{text}");
}

#[test]
fn research_file_writes_the_report_body_and_the_run_link() {
    let dir = scratch("research-file");
    let state = dir.join("state");
    let vault = dir.join("vault");
    let db = dir.join("mneme.db");
    let store = harness::research::Store::new(&state);
    let mut run = store
        .create_named(
            "r-file",
            "What did the shelf say?",
            harness::research::QuestionKind::Fact,
            harness::research::ResearchBudget::default(),
        )
        .expect("a run");
    run.status = harness::research::RunStatus::Published;
    store.save(&run).expect("a saved run");
    std::fs::write(
        store.dir("r-file").join("report.md"),
        "# What did the shelf say?\n\nA body that must reach the note.\n",
    )
    .expect("a report");

    let (code, text) = hades(&[
        "topic",
        "technical",
        "GPU inference",
        "--summary",
        "Measured limits.",
        "--vault",
        vault.to_str().expect("utf8"),
        "--db",
        db.to_str().expect("utf8"),
    ]);
    assert_eq!(code, 0, "{text}");
    let (code, text) = hades(&[
        "research",
        "file",
        "r-file",
        "--topic",
        "technical/gpu-inference",
        "--state",
        state.to_str().expect("utf8"),
        "--vault",
        vault.to_str().expect("utf8"),
        "--db",
        db.to_str().expect("utf8"),
    ]);
    assert_eq!(code, 0, "{text}");
    let note = vault.join("20 Research/technical/gpu-inference/what-did-the-shelf-say.md");
    let body = std::fs::read_to_string(&note).expect("a filed note");
    assert!(body.contains("A body that must reach the note."), "{body}");
    assert!(
        body.contains("source_url: hades://research/r-file"),
        "{body}"
    );
}
