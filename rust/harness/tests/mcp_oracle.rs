//! The MCP oracle: a real server process, real pipes, real JSON-RPC.
//!
//! The fixture is a Python stdio server, started by the client exactly as a configured one would be.
//! Nothing here mocks the transport: the point of the feature is that a process is spawned, initialised,
//! asked for its tools and told to run one, and a mock would prove none of that.
//!
//! Every path is a `--config` flag under `target/scratch`, and `HADES_HOME` is set once before the first
//! CLI call, so no test reads or writes the Hades home of the machine it runs on.

use std::path::{Path, PathBuf};
use std::time::Instant;

use harness::cli;

fn scratch(name: &str) -> PathBuf {
    let dir = scratch_base().join(format!("hades-mcp-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

fn scratch_base() -> PathBuf {
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/scratch");
    std::fs::create_dir_all(&base).expect("the scratch base");
    base.canonicalize().unwrap_or(base)
}

/// Set once, before any CLI call. The MCP command still gets an explicit `--config`; the home only
/// exists so a bug that falls back to the default config lands in scratch rather than in `~/.hades`.
fn isolated_home() {
    use std::sync::OnceLock;
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let home = scratch_base().join(format!("hades-mcp-home-{}", std::process::id()));
        std::fs::create_dir_all(&home).expect("an isolated home");
        // SAFETY: set before any CLI call, and every test in this binary goes through `hades`.
        unsafe { std::env::set_var("HADES_HOME", &home) };
    });
}

fn hades(args: Vec<String>) -> (i32, String) {
    isolated_home();
    let mut out: Vec<u8> = Vec::new();
    let mut input = std::io::Cursor::new(Vec::new());
    let code = cli::run_with(&args, &mut out, &mut input);
    (code, String::from_utf8_lossy(&out).into_owned())
}

fn fixture() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/mcp_echo.py")
        .display()
        .to_string()
}

/// A config that names the fixture server. `allow` is the person's decision, and the oracle passes it
/// explicitly so each test is about one rule.
fn config_with(dir: &Path, allow: &[&str], timeout_s: u64) -> String {
    std::fs::create_dir_all(dir.join("work")).expect("the tree");
    let path = dir.join("config.json");
    let value = serde_json::json!({
        "version": 1,
        "provider": "ollama-cloud",
        "model": "deepseek-v4.1-flash",
        "root": dir.join("work").display().to_string(),
        "vault": dir.join("mneme").display().to_string(),
        "mcp": {
            "servers": {
                "echo": {
                    "command": "python3",
                    "args": [fixture()],
                    "allow": allow,
                    "timeout_s": timeout_s
                }
            }
        }
    });
    std::fs::write(&path, value.to_string()).expect("the config");
    path.display().to_string()
}

fn command(config: &str, rest: &[&str]) -> Vec<String> {
    let mut args = vec!["mcp".to_string()];
    args.extend(rest.iter().map(|part| part.to_string()));
    args.push("--config".to_string());
    args.push(config.to_string());
    args
}

#[test]
fn mcp_list_shows_the_configured_server_without_starting_it() {
    let dir = scratch("list");
    let config = config_with(&dir, &["echo"], 10);
    let (code, text) = hades(command(&config, &["list"]));
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("1 MCP server(s)"), "{text}");
    assert!(text.contains("echo · python3"), "{text}");
    assert!(text.contains("allow: echo"), "{text}");
}

#[test]
fn mcp_tools_marks_allowed_and_blocked() {
    let dir = scratch("tools");
    let config = config_with(&dir, &["echo"], 10);
    let (code, text) = hades(command(&config, &["tools", "echo"]));
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("3 tool(s)"), "{text}");
    assert!(
        text.contains("echo") && text.contains("[allowed]"),
        "{text}"
    );
    assert!(
        text.contains("secret") && text.contains("[blocked]"),
        "{text}"
    );
}

#[test]
fn mcp_call_runs_an_allowed_tool() {
    let dir = scratch("call");
    let config = config_with(&dir, &["echo"], 10);
    let (code, text) = hades(command(
        &config,
        &[
            "call",
            "echo",
            "echo",
            "--args",
            r#"{"text":"hello from mcp"}"#,
        ],
    ));
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("hello from mcp"), "{text}");
}

#[test]
fn mcp_call_refuses_a_tool_not_on_the_allowlist() {
    let dir = scratch("blocked");
    let config = config_with(&dir, &["echo"], 10);
    let (code, text) = hades(command(&config, &["call", "echo", "secret"]));
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("not on the allowed list"), "{text}");
    assert!(text.contains("mcp.servers.echo.allow"), "{text}");
}

#[test]
fn mcp_call_to_a_missing_server_is_named() {
    let dir = scratch("missing-server");
    let config = config_with(&dir, &["echo"], 10);
    let (code, text) = hades(command(&config, &["call", "nowhere", "echo"]));
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("no MCP server called `nowhere`"), "{text}");
    assert!(text.contains("configured: echo"), "{text}");
}

#[test]
fn a_server_that_cannot_start_names_the_command() {
    let dir = scratch("bad-command");
    std::fs::create_dir_all(dir.join("work")).expect("the tree");
    let path = dir.join("config.json");
    let value = serde_json::json!({
        "version": 1,
        "provider": "ollama-cloud",
        "model": "deepseek-v4.1-flash",
        "root": dir.join("work").display().to_string(),
        "vault": dir.join("mneme").display().to_string(),
        "mcp": { "servers": { "echo": {
            "command": "/definitely/not/a/binary",
            "allow": ["echo"],
            "timeout_s": 5
        }}}
    });
    std::fs::write(&path, value.to_string()).expect("the config");
    let (code, text) = hades(command(&path.display().to_string(), &["tools", "echo"]));
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("could not start MCP server `echo`"), "{text}");
    assert!(text.contains("/definitely/not/a/binary"), "{text}");
}

#[test]
fn a_slow_server_is_killed_at_its_deadline() {
    let dir = scratch("timeout");
    let config = config_with(&dir, &["slow"], 1);
    let started = Instant::now();
    let (code, text) = hades(command(&config, &["call", "echo", "slow"]));
    let elapsed = started.elapsed();
    assert_eq!(code, 1, "{text}");
    assert!(
        text.contains("did not answer `tools/call` within 1s"),
        "{text}"
    );
    assert!(
        elapsed.as_secs() < 4,
        "the deadline was not honoured: {elapsed:?}"
    );
}

#[test]
fn an_empty_registry_says_how_to_add_one() {
    let dir = scratch("empty");
    std::fs::create_dir_all(dir.join("work")).expect("the tree");
    let path = dir.join("config.json");
    let value = serde_json::json!({
        "version": 1,
        "provider": "ollama-cloud",
        "model": "deepseek-v4.1-flash",
        "root": dir.join("work").display().to_string(),
        "vault": dir.join("mneme").display().to_string()
    });
    std::fs::write(&path, value.to_string()).expect("the config");
    let (code, text) = hades(command(&path.display().to_string(), &["list"]));
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("no MCP servers are configured"), "{text}");
    assert!(text.contains("mcp.servers"), "{text}");
}

#[test]
fn a_read_surface_has_no_mcp_tool() {
    let (code, act) = hades(vec![
        "tools".to_string(),
        "--mode".to_string(),
        "act".to_string(),
    ]);
    assert_eq!(code, 0, "{act}");
    assert!(act.contains("  mcp\n"), "{act}");
    let (code, read) = hades(vec![
        "tools".to_string(),
        "--mode".to_string(),
        "read".to_string(),
    ]);
    assert_eq!(code, 0, "{read}");
    assert!(
        !read.contains("  mcp\n"),
        "a read surface still has mcp: {read}"
    );
}
