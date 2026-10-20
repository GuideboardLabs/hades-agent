//! The oracle for the Memory seam: Lachesis recalls within a budget, Clotho files a durable note,
//! and neither can quietly do the other's job.

use std::path::{Path, PathBuf};

use governance::{Curator, SystemTimestamp};
use harness::cli;
use harness::memory::{Memory, MnemeMemory, OutcomeNote, RecallReason};
use mneme::{Clock, Frontmatter, NoteStore, SqliteStore, SystemClock, Vault};

fn scratch(name: &str) -> PathBuf {
    let base = harness::scratch::root();
    std::fs::create_dir_all(&base).expect("the scratch base");
    let base = base.canonicalize().unwrap_or(base);
    let dir = base.join(format!("hades-memory-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

fn note(title: &str, summary: &str, topics: &[&str], body: &str) -> String {
    let frontmatter = Frontmatter {
        title: Some(title.to_string()),
        summary: Some(summary.to_string()),
        status: Some("durable".to_string()),
        topics: topics.iter().map(|topic| topic.to_string()).collect(),
        provenance: Some("oracle".to_string()),
        ..Frontmatter::default()
    };
    format!("{}\n{}\n", frontmatter.render(), body.trim())
}

fn seeded(name: &str) -> (MnemeMemory, PathBuf) {
    let dir = scratch(name);
    let root = dir.join("vault");
    let memory = MnemeMemory::open(&root, dir.join("mneme.db")).expect("the record");
    memory
        .vault()
        .store()
        .write(
            &root.join("20 Research").join("lantern.md"),
            &note(
                "Lantern",
                "the lantern glows green",
                &["Memory"],
                "A body about the lantern.",
            ),
        )
        .expect("a note");
    memory
        .vault()
        .store()
        .write(
            &root.join("20 Research").join("systems.md"),
            &note(
                "Systems",
                "the systems note",
                &["systems"],
                "Nothing about the lamp.",
            ),
        )
        .expect("another note");
    (memory, dir)
}

#[test]
fn recall_finds_text_and_topics_within_the_budget() {
    let (memory, _dir) = seeded("recall");

    let text = memory
        .recall("where is the lantern", &[], 10_000)
        .expect("recall");
    assert_eq!(text.hits.len(), 1);
    assert!(
        text.text.contains("20 Research/lantern.md"),
        "{}",
        text.text
    );
    assert!(
        text.text.contains("the lantern glows green"),
        "{}",
        text.text
    );
    assert_eq!(text.reason, None);
    assert!(text.tokens > 0);

    // "green" is text, "systems" is a topic: the topic-only hit comes first even though its bm25 is
    // absent, and the tags are lowercase-matched ("Memory" versus "memory").
    let tagged = memory
        .recall("green", &["systems".to_string()], 10_000)
        .expect("recall");
    let names: Vec<String> = tagged
        .hits
        .iter()
        .map(|hit| hit.path.display().to_string())
        .collect();
    assert_eq!(
        names,
        vec!["20 Research/systems.md", "20 Research/lantern.md"]
    );
    assert_eq!(tagged.hits[0].topics, vec!["systems".to_string()]);

    let memory_tag = memory
        .recall("green", &["memory".to_string()], 10_000)
        .expect("recall");
    assert_eq!(memory_tag.hits.len(), 1);
    assert_eq!(memory_tag.hits[0].topics, vec!["Memory".to_string()]);
}

#[test]
fn an_empty_answer_says_which_kind_of_empty_it_is() {
    let (memory, _dir) = seeded("reasons");

    let nothing = memory.recall("zebra quantum", &[], 10_000).unwrap();
    assert!(nothing.is_empty());
    assert_eq!(nothing.reason, Some(RecallReason::NoMatch));
    assert!(nothing.text.contains("Nothing in the record"));

    let tiny = memory.recall("lantern", &[], 1).unwrap();
    assert!(tiny.is_empty());
    assert_eq!(tiny.reason, Some(RecallReason::BudgetTooSmall));
}

#[test]
fn a_recorded_outcome_is_a_durable_file_the_audit_accepts() {
    let (memory, dir) = seeded("record");
    let outcome = OutcomeNote {
        goal: "fix the lantern".to_string(),
        session: "sess-1".to_string(),
        run: 2,
        attempt: 1,
        title: "The lantern is fixed".to_string(),
        summary: "the lantern is fixed".to_string(),
        constraints: vec!["the check passes".to_string()],
        check: "test -f stats.sh".to_string(),
        files: vec!["stats.sh".to_string()],
        topics: vec!["Memory".to_string()],
    };

    let day = SystemClock.today().to_iso();
    let first = memory.record(&outcome).expect("record");
    let expected = format!("20 Research/{day}-sess-1-r2.md");
    assert!(
        first.path.display().to_string().ends_with(&expected),
        "{}",
        first.path.display()
    );
    let written = memory.vault().read_note(&first.path).expect("the note");
    assert_eq!(written.summary(), "the lantern is fixed");
    assert_eq!(written.topics(), ["Memory".to_string()].as_slice());
    assert!(written.body.contains("test -f stats.sh"));

    // The audit agrees: summary, topic and provenance are all on the note.
    let store = SqliteStore::open(dir.join("mneme.db"), dir.join("vault")).expect("reopen");
    let vault = Vault::new(dir.join("vault"), store, mneme::SystemClock);
    let report = Curator::new(vault, SystemTimestamp).audit().expect("audit");
    assert!(report.ok(), "{:?}", report.problems);

    // A caller that records twice is filing two outcomes; the record does not silently drop the second
    // one because the caller's run number repeats. Filing once per accepted outcome is the caller's job.
    let second = memory.record(&outcome).expect("record again");
    assert_ne!(second.path, first.path);
    assert!(
        second
            .path
            .display()
            .to_string()
            .ends_with(&format!("20 Research/{day}-sess-1-r3.md")),
        "{}",
        second.path.display()
    );
    let notes = memory
        .vault()
        .iter_notes(Some(mneme::Area::Research), &[])
        .expect("the research notes")
        .iter()
        .filter(|note| {
            note.path
                .file_name()
                .map(|name| name.to_string_lossy().starts_with(&format!("{day}-sess")))
                .unwrap_or(false)
        })
        .count();
    assert_eq!(notes, 2);
}

#[test]
fn a_record_without_a_topic_or_a_summary_is_refused_and_writes_nothing() {
    let (memory, _dir) = seeded("refuse");
    let base = OutcomeNote {
        goal: "fix it".to_string(),
        session: "sess-2".to_string(),
        run: 1,
        attempt: 1,
        title: "Done".to_string(),
        summary: "done".to_string(),
        constraints: vec![],
        check: String::new(),
        files: vec![],
        topics: vec!["memory".to_string()],
    };

    let mut no_topic = base.clone();
    no_topic.topics.clear();
    let error = memory.record(&no_topic).expect_err("a refusal");
    assert!(error.contains("topic"), "{error}");

    let mut no_summary = base.clone();
    no_summary.summary.clear();
    let error = memory.record(&no_summary).expect_err("a refusal");
    assert!(error.contains("summary"), "{error}");

    let recorded = memory
        .vault()
        .iter_notes(Some(mneme::Area::Research), &[])
        .expect("the research notes")
        .iter()
        .filter(|note| note.path.to_string_lossy().contains("sess-2-r1"))
        .count();
    assert_eq!(recorded, 0);
}

fn isolated_home() {
    use std::sync::OnceLock;
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let home = scratch("home-isolated");
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

/// The config a turn and a context share; `hades init` writes it, and the record beside it.
fn init_home(dir: &Path) -> String {
    std::fs::create_dir_all(dir.join("work")).expect("the tree");
    let config = dir.join("config.json");
    let args: Vec<String> = [
        "init",
        "--non-interactive",
        "--config",
        &config.display().to_string(),
        "--vault",
        &dir.join("mneme").display().to_string(),
        "--root",
        &dir.join("work").display().to_string(),
        "--name",
        "Warden",
        "--goal",
        "keep the notes true",
        "--topics",
        "memory,systems",
    ]
    .iter()
    .map(|value| value.to_string())
    .collect();
    let (code, text) = hades(&args);
    assert_eq!(code, 0, "{text}");
    config.display().to_string()
}

#[test]
fn a_turn_recalls_before_it_acts_and_records_after_it_passes() {
    let dir = scratch("turn");
    let config = init_home(&dir);
    let state = dir.join("state");
    let work = dir.join("work");
    let vault = dir.join("mneme");
    let memory = MnemeMemory::open(&vault, dir.join("mneme.db")).expect("the record");
    memory
        .vault()
        .store()
        .write(
            &vault.join("20 Research").join("lantern.md"),
            &note(
                "Lantern",
                "the lantern glows green",
                &["memory"],
                "The lantern is green.",
            ),
        )
        .expect("a note");

    let (code, text) = hades(&[
        "context".to_string(),
        "where is the lantern".to_string(),
        "--config".to_string(),
        config.clone(),
        "--root".to_string(),
        work.display().to_string(),
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("what Mneme remembers"), "{text}");
    assert!(text.contains("20 Research/lantern.md"), "{text}");
    assert!(text.contains("memory: recalled 1"), "{text}");

    let (code, text) = hades(&[
        "turn".to_string(),
        "make the stats file".to_string(),
        "--program".to_string(),
        "touch stats.sh".to_string(),
        "--verify".to_string(),
        "test -f stats.sh".to_string(),
        "--policy".to_string(),
        "open".to_string(),
        "--root".to_string(),
        work.display().to_string(),
        "--state".to_string(),
        state.display().to_string(),
        "--config".to_string(),
        config.clone(),
        "--session".to_string(),
        "demo1".to_string(),
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("recorded "), "{text}");
    assert!(text.contains("check passed"), "{text}");
    // The path the output names is a file a person can open, not a row that a later export would make
    // readable. The curator writes the export as part of filing.
    let recorded = text
        .lines()
        .find_map(|line| {
            line.split_once(", recorded ")
                .map(|(_, rest)| rest.trim().to_string())
        })
        .expect("the output names the note");
    let on_disk = std::fs::read_to_string(&recorded).expect("the recorded note is on disk");
    assert!(on_disk.contains("Goal: make the stats file"), "{on_disk}");

    // A second accepted turn on the same session and day is a second outcome, so it files r2 rather than
    // being discarded as a repeat of the first.
    let (code, text) = hades(&[
        "turn".to_string(),
        "make the stats file".to_string(),
        "--program".to_string(),
        "touch stats.sh".to_string(),
        "--verify".to_string(),
        "test -f stats.sh".to_string(),
        "--policy".to_string(),
        "open".to_string(),
        "--root".to_string(),
        work.display().to_string(),
        "--state".to_string(),
        state.display().to_string(),
        "--config".to_string(),
        config.clone(),
        "--session".to_string(),
        "demo1".to_string(),
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("-demo1-r2.md"), "{text}");
    let second = text
        .lines()
        .find_map(|line| {
            line.split_once(", recorded ")
                .map(|(_, rest)| rest.trim().to_string())
        })
        .expect("the output names the second note");
    assert_ne!(recorded, second);
    assert!(std::fs::read_to_string(&second).is_ok(), "{second}");

    let (code, text) = hades(&[
        "context".to_string(),
        "make the stats file".to_string(),
        "--config".to_string(),
        config.clone(),
        "--root".to_string(),
        work.display().to_string(),
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("-demo1-r1.md"), "{text}");

    let (code, text) = hades(&["audit".to_string(), "--config".to_string(), config]);
    assert_eq!(code, 0, "{text}");
    assert!(!text.contains("missing_"), "{text}");
}

#[test]
fn a_run_files_one_note_only_when_the_walk_accepts() {
    let dir = scratch("run");
    let config = init_home(&dir);
    let state = dir.join("state");
    let work = dir.join("work");
    let vault = dir.join("mneme");
    let memory = MnemeMemory::open(&vault, dir.join("mneme.db")).expect("the record");
    memory
        .vault()
        .store()
        .write(
            &vault.join("20 Research").join("lantern.md"),
            &note(
                "Lantern",
                "the lantern glows green",
                &["memory"],
                "The lantern is green.",
            ),
        )
        .expect("a note");

    let (code, text) = hades(&[
        "run".to_string(),
        "make the stats file".to_string(),
        "--program".to_string(),
        "touch stats.sh".to_string(),
        "--verify".to_string(),
        "test -f stats.sh".to_string(),
        "--policy".to_string(),
        "open".to_string(),
        "--attempts".to_string(),
        "1".to_string(),
        "--minutes".to_string(),
        "2".to_string(),
        "--root".to_string(),
        work.display().to_string(),
        "--state".to_string(),
        state.display().to_string(),
        "--config".to_string(),
        config.clone(),
        "--session".to_string(),
        "walk1".to_string(),
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("accepted after 1 attempt"), "{text}");
    assert!(text.contains("memory:  recalled 1, recorded "), "{text}");
    let filed = text
        .lines()
        .find_map(|line| {
            line.split_once(", recorded ")
                .map(|(_, rest)| rest.trim().to_string())
        })
        .expect("the run names its note");
    let body = std::fs::read_to_string(&filed).expect("the note is a file");
    assert!(body.contains("Goal: make the stats file"), "{body}");
    let walked: Vec<_> = std::fs::read_dir(vault.join("20 Research"))
        .expect("the research folder")
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().contains("walk1"))
        .collect();
    assert_eq!(walked.len(), 1, "one accepted run, one note");

    // A walk that gives up is not an outcome worth remembering: exit 1, nothing filed.
    let (code, text) = hades(&[
        "run".to_string(),
        "make nothing".to_string(),
        "--program".to_string(),
        "true".to_string(),
        "--verify".to_string(),
        "test -f never".to_string(),
        "--policy".to_string(),
        "open".to_string(),
        "--attempts".to_string(),
        "1".to_string(),
        "--minutes".to_string(),
        "2".to_string(),
        "--root".to_string(),
        work.display().to_string(),
        "--state".to_string(),
        state.display().to_string(),
        "--config".to_string(),
        config,
        "--session".to_string(),
        "walk2".to_string(),
    ]);
    assert_eq!(code, 1, "{text}");
    assert!(
        text.contains("recorded none (the run was not accepted)"),
        "{text}"
    );
    let refused = std::fs::read_dir(vault.join("20 Research"))
        .expect("the research folder")
        .filter_map(|entry| entry.ok())
        .any(|entry| entry.file_name().to_string_lossy().contains("walk2"));
    assert!(!refused, "a refused run files nothing");
}
