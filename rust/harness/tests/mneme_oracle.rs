//! The oracle for `hades mneme`: the record is the source, the tree is its export, and a hand edit is
//! never silently overwritten.
//!
//! Every path is a flag or a scratch directory. The home is isolated before the first call, so no
//! test can read or write the `~/.hades` of the machine running it.

use std::path::{Path, PathBuf};

use harness::cli;

fn scratch(name: &str) -> PathBuf {
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/scratch");
    std::fs::create_dir_all(&base).expect("the scratch base");
    let base = base.canonicalize().unwrap_or(base);
    let dir = base.join(format!("hades-mneme-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

fn isolated_home() {
    use std::sync::OnceLock;
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/scratch");
        std::fs::create_dir_all(&base).expect("the scratch base");
        let home = base
            .canonicalize()
            .unwrap_or(base)
            .join(format!("hades-home-{}", std::process::id()));
        std::fs::create_dir_all(&home).expect("an isolated home");
        // SAFETY: set before any CLI call, and every test in this binary goes through `hades`.
        unsafe { std::env::set_var("HADES_HOME", &home) };
    });
}

fn hades(args: &[String]) -> (i32, String) {
    isolated_home();
    let mut out: Vec<u8> = Vec::new();
    let mut input = std::io::Cursor::new(Vec::new());
    let code = cli::run_with(args, &mut out, &mut input);
    (code, String::from_utf8_lossy(&out).into_owned())
}

/// `hades init` for one home, with the three answers given so nothing is prompted. Returns the config.
fn init_home(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir.join("work")).expect("the tree");
    let config = dir.join("config.json");
    let args: Vec<String> = vec![
        "init".to_string(),
        "--non-interactive".to_string(),
        "--config".to_string(),
        config.display().to_string(),
        "--vault".to_string(),
        dir.join("mneme").display().to_string(),
        "--root".to_string(),
        dir.join("work").display().to_string(),
        "--name".to_string(),
        "Warden".to_string(),
        "--goal".to_string(),
        "keep the notes true".to_string(),
        "--topics".to_string(),
        "memory,systems".to_string(),
    ];
    let (code, text) = hades(&args);
    assert_eq!(code, 0, "{text}");
    config
}

#[test]
fn init_leaves_a_record_beside_the_config_and_audit_reads_it() {
    let dir = scratch("home");
    let config = init_home(&dir);

    assert!(
        dir.join("mneme.db").is_file(),
        "the record is not beside the config"
    );

    let (code, text) = hades(&[
        "audit".to_string(),
        "--config".to_string(),
        config.display().to_string(),
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("vault: "), "{text}");
    assert!(text.contains("inbox: 0"), "{text}");
}

#[test]
fn import_export_and_a_hand_edit_that_is_refused() {
    let dir = scratch("round");
    let config = init_home(&dir).display().to_string();

    // A canonical tree, made by the file store, to import.
    let source = dir.join("source");
    let file = mneme::Vault::new(source.clone(), mneme::FileVault::new(), mneme::SystemClock);
    file.init(None).expect("a source vault");
    file.capture("The body.", None, None).expect("a note");

    let (code, text) = hades(&[
        "mneme".to_string(),
        "import".to_string(),
        source.display().to_string(),
        "--config".to_string(),
        config.clone(),
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("imported: 1"), "{text}");

    let (code, text) = hades(&[
        "mneme".to_string(),
        "export".to_string(),
        "--config".to_string(),
        config.clone(),
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("written: 1"), "{text}");

    let inbox = dir.join("mneme").join("00 Inbox");
    let exported = std::fs::read_dir(&inbox)
        .expect("the inbox exists")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .find(|path| path.extension().and_then(|ext| ext.to_str()) == Some("md"))
        .expect("the note was exported");
    let canonical = std::fs::read_to_string(&exported).expect("read");
    assert!(canonical.contains("The body."), "{canonical}");

    std::fs::write(&exported, format!("{canonical}\nA hand edit.\n")).expect("edit");
    let (code, text) = hades(&[
        "mneme".to_string(),
        "export".to_string(),
        "--config".to_string(),
        config.clone(),
    ]);
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("refused: 1"), "{text}");
    assert!(
        std::fs::read_to_string(&exported)
            .expect("read")
            .contains("A hand edit."),
        "the hand edit was overwritten without --force"
    );

    let (code, text) = hades(&[
        "mneme".to_string(),
        "export".to_string(),
        "--force".to_string(),
        "--config".to_string(),
        config.clone(),
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("written: 1"), "{text}");
    assert!(
        !std::fs::read_to_string(&exported)
            .expect("read")
            .contains("A hand edit."),
        "--force did not write the record"
    );

    let (code, text) = hades(&["audit".to_string(), "--config".to_string(), config]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("inbox: 1"), "{text}");
}

#[test]
fn audit_over_a_tree_that_was_never_opened_and_no_temporary_database_is_left() {
    let dir = scratch("one-shot");
    let tree = dir.join("tree");
    let file = mneme::Vault::new(tree.clone(), mneme::FileVault::new(), mneme::SystemClock);
    file.init(None).expect("a vault");
    file.capture("A note with no database.", None, None)
        .expect("a note");

    let (code, text) = hades(&[
        "audit".to_string(),
        "--vault".to_string(),
        tree.display().to_string(),
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("inbox: 1"), "{text}");

    let home = std::env::var("HADES_HOME").expect("the isolated home");
    let leftovers: Vec<PathBuf> = std::fs::read_dir(Path::new(&home).join("tmp"))
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.path())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        leftovers.is_empty(),
        "temporary databases left: {leftovers:?}"
    );
}
