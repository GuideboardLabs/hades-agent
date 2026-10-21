//! The oracle for `hades init`: one command that leaves a home a second machine can start from.
//!
//! Every path in these tests is a flag, so nothing here reads or writes the Hades home of the machine
//! the tests run on. The last test is the guard, and it only compares two paths by name.

use std::path::{Path, PathBuf};

use harness::cli;

fn scratch(name: &str) -> PathBuf {
    let dir = scratch_base().join(format!("hades-init-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

/// Drive the CLI with the answers already typed, so the interview is tested and not a pty.
fn hades(args: Vec<String>, answers: &str) -> (i32, String) {
    isolated_home();
    let mut out: Vec<u8> = Vec::new();
    let mut input = std::io::Cursor::new(answers.as_bytes().to_vec());
    let code = cli::run_with(&args, &mut out, &mut input);
    (code, String::from_utf8_lossy(&out).into_owned())
}

/// The flags for one full home under `dir`, with the three answers given.
fn init_under(dir: &Path) -> Vec<String> {
    [
        "init".to_string(),
        "--config".to_string(),
        dir.join("config.json").display().to_string(),
        "--vault".to_string(),
        dir.join("mneme").display().to_string(),
        "--root".to_string(),
        dir.join("work").display().to_string(),
    ]
    .to_vec()
}

#[test]
fn init_writes_a_config_a_vault_and_an_auditable_home() {
    let dir = scratch("home");
    std::fs::create_dir_all(dir.join("work")).expect("the tree");

    let mut args = init_under(&dir);
    args.extend(
        [
            "--non-interactive",
            "--name",
            "Warden",
            "--goal",
            "keep the notes true",
            "--topics",
            "memory, systems, memory",
        ]
        .map(str::to_string),
    );

    let (code, text) = hades(args, "");
    assert_eq!(code, 0, "{text}");
    assert!(
        !text.contains("What should the agent be called?"),
        "a non-interactive init asks nothing: {text}"
    );

    let vault = dir.join("mneme");
    for area in [
        "00 Inbox",
        "10 Outbox",
        "20 Research",
        "30 Projects",
        "40 Products",
        "50 Topics",
    ] {
        assert!(vault.join(area).is_dir(), "{area} is missing");
    }

    let charter = std::fs::read_to_string(vault.join("charter.md")).expect("a charter");
    assert!(charter.contains("- name: Warden"), "{charter}");
    assert!(charter.contains("keep the notes true"), "{charter}");
    assert!(charter.contains("- memory"), "{charter}");
    assert!(charter.contains("- systems"), "{charter}");
    assert_eq!(
        charter.matches("- memory").count(),
        1,
        "a topic named twice is one topic: {charter}"
    );

    let index = std::fs::read_to_string(vault.join("INDEX.md")).expect("an index");
    assert!(index.starts_with("# WEFT: index"), "{index}");
    assert!(index.contains("No topics yet"), "{index}");
    assert!(vault.join("TAXONOMY.md").is_file(), "the taxonomy ships");

    let config = harness::config::load(&dir.join("config.json"))
        .expect("the config reads")
        .expect("a config was written");
    assert_eq!(config.provider, "ollama-cloud", "{text}");
    assert_eq!(config.model, "deepseek-v4.1-flash");
    assert_eq!(config.root, dir.join("work"));
    assert_eq!(config.vault, vault);

    // The point of the file: an audit with no vault flag finds the one init made.
    let (code, text) = hades(
        vec![
            "audit".to_string(),
            "--config".to_string(),
            dir.join("config.json").display().to_string(),
        ],
        "",
    );
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("inbox: 0"), "{text}");
}

#[test]
fn a_second_init_refuses_before_it_asks_anything() {
    let dir = scratch("twice");
    std::fs::create_dir_all(dir.join("work")).expect("the tree");

    let (code, text) = hades(init_under(&dir), "Warden\nkeep the notes true\nmemory\n");
    assert_eq!(code, 0, "{text}");

    let charter = dir.join("mneme/charter.md");
    let before = std::fs::read_to_string(&charter).expect("the first charter");

    let (code, text) = hades(init_under(&dir), "Somebody else\na different goal\nother\n");
    assert_eq!(code, 2, "{text}");
    assert!(text.contains("already exists"), "{text}");
    assert!(
        text.contains(&dir.join("mneme").display().to_string()),
        "the refusal names the vault: {text}"
    );
    assert!(
        !text.contains("What should the agent be called?"),
        "the refusal comes before the questions: {text}"
    );
    assert_eq!(
        std::fs::read_to_string(&charter).expect("the charter"),
        before,
        "the first home is untouched"
    );
}

#[test]
fn an_empty_directory_is_written_into() {
    let dir = scratch("empty");
    std::fs::create_dir_all(dir.join("work")).expect("the tree");
    std::fs::create_dir_all(dir.join("mneme")).expect("an installer's mkdir");

    let (code, text) = hades(init_under(&dir), "Warden\n\n\n");
    assert_eq!(code, 0, "an empty directory is not a vault: {text}");
    assert!(dir.join("mneme/INDEX.md").is_file(), "{text}");
}

#[test]
fn the_interview_writes_the_answers_it_was_given() {
    let dir = scratch("interview");
    std::fs::create_dir_all(dir.join("work")).expect("the tree");

    let (code, text) = hades(
        init_under(&dir),
        "Warden\nkeep the notes true\nmemory, systems\n",
    );
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("What should the agent be called?"), "{text}");
    assert!(text.contains("What is the work for?"), "{text}");
    assert!(text.contains("Which topics matter?"), "{text}");

    let charter = std::fs::read_to_string(dir.join("mneme/charter.md")).expect("a charter");
    assert!(charter.contains("- name: Warden"), "{charter}");
    assert!(charter.contains("keep the notes true"), "{charter}");
    assert!(charter.contains("- memory"), "{charter}");
    assert!(charter.contains("- systems"), "{charter}");
}

#[test]
fn an_empty_stdin_says_it_used_the_defaults() {
    let dir = scratch("eof");
    std::fs::create_dir_all(dir.join("work")).expect("the tree");

    let (code, text) = hades(init_under(&dir), "");
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("no answer on stdin"), "{text}");
    assert!(text.contains("--non-interactive"), "{text}");

    let charter = std::fs::read_to_string(dir.join("mneme/charter.md")).expect("a charter");
    assert!(charter.contains("- name: Hades"), "{charter}");
    assert!(charter.contains("None named yet"), "{charter}");
}

#[test]
fn an_audit_with_no_vault_says_how_to_get_one() {
    let dir = scratch("no-vault");
    let (code, text) = hades(
        vec![
            "audit".to_string(),
            "--config".to_string(),
            dir.join("no-config.json").display().to_string(),
        ],
        "",
    );
    assert_eq!(code, 2, "{text}");
    assert!(text.contains("audit needs a vault"), "{text}");
    assert!(text.contains("hades init"), "{text}");
}

/// The seam the gateway stands on: no `--model` and no `--program`, so the config's preset is what the
/// run would ask. A preset this build does not ship is refused by name, which proves the file was read
/// without making a network call.
#[test]
fn a_run_with_no_model_reads_the_preset_from_the_config() {
    let dir = scratch("config-model");
    std::fs::create_dir_all(dir.join("work")).expect("the tree");
    let config = dir.join("config.json");
    std::fs::write(
        &config,
        format!(
            r#"{{"version":1,"provider":"ollama-cloud","model":"not-a-preset","root":{},"vault":{}}}"#,
            serde_json::json!(dir.join("work").display().to_string()),
            serde_json::json!(dir.join("mneme").display().to_string())
        ),
    )
    .expect("a config");

    let (code, text) = hades(
        vec![
            "turn".to_string(),
            "do the thing".to_string(),
            "--config".to_string(),
            config.display().to_string(),
        ],
        "",
    );
    assert_eq!(code, 2, "{text}");
    assert!(
        text.contains("the model `not-a-preset` has no preset"),
        "{text}"
    );
}

#[test]
fn init_refuses_the_live_vault_and_writes_nothing() {
    let home = std::env::var("HOME").expect("a home");
    let live = PathBuf::from(home).join("Documents/ObsidianVault");
    let dir = scratch("guard");

    let args = vec![
        "init".to_string(),
        "--non-interactive".to_string(),
        "--config".to_string(),
        dir.join("config.json").display().to_string(),
        "--vault".to_string(),
        live.display().to_string(),
        "--root".to_string(),
        dir.join("work").display().to_string(),
    ];

    let (code, text) = hades(args, "");
    assert_eq!(code, 2, "{text}");
    assert!(text.contains("refusing"), "{text}");
    assert!(text.contains("the live Hermes vault"), "{text}");
    assert!(
        !dir.join("config.json").exists(),
        "a refused init writes nothing"
    );
}

fn scratch_base() -> PathBuf {
    let base = harness::scratch::root();
    std::fs::create_dir_all(&base).expect("the scratch base");
    base.canonicalize().unwrap_or(base)
}

/// A home for this process, so the temporary database `audit --vault` opens lands in scratch and
/// never in `~/.hades`. Set once, before any CLI call.
fn isolated_home() {
    use std::sync::OnceLock;
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let home = scratch_base().join(format!("hades-home-{}", std::process::id()));
        std::fs::create_dir_all(&home).expect("an isolated home");
        // SAFETY: no other thread reads the environment before the first CLI call, and the tests in
        // this binary all go through `hades`.
        unsafe { std::env::set_var("HADES_HOME", &home) };
    });
}
