//! The oracle for a resume: what a session's own log carries into the next turn.
//!
//! This is the difference between a session that survives a kill and a run that can be picked up. The store
//! could reopen a log and still hand the next turn a prompt that knew nothing about it; before
//! `Learned::from_log` existed, that is exactly what the harness did. So these tests drive real turns through
//! the CLI, read the resulting log back, and assert on what the log says rather than on what it holds.

use std::path::PathBuf;

use harness::cli;
use harness::context::Learned;
use session_tree::{FileStore, Session, SessionId, SystemClock};

fn scratch(name: &str) -> PathBuf {
    let dir = scratch_base().join(format!("hades-resume-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

/// Drives the CLI the way the binary does, so what is read back is what a real run wrote.
///
/// `HADES_HOME` is set once, before the first call: the CLI reads a config from the Hades home when no
/// `--config` is given, and a test must not read the home of the machine it runs on.
fn hades(args: &[&str]) -> (i32, String) {
    isolate_home();
    let owned: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
    let mut out: Vec<u8> = Vec::new();
    let code = cli::run(&owned, &mut out);
    (code, String::from_utf8_lossy(&out).into_owned())
}

fn isolate_home() {
    static HOME: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    HOME.get_or_init(|| {
        let dir = scratch_base().join(format!("hades-resume-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("an isolated Hades home");
        // SAFETY: set once, inside `OnceLock::get_or_init`, before any test thread reaches a CLI call,
        // and the CLI reads it only after that call. No other thread reads or writes this variable.
        unsafe { std::env::set_var("HADES_HOME", &dir) };
        dir
    });
}

struct Bench {
    root: PathBuf,
    state: PathBuf,
}

impl Bench {
    fn new(name: &str) -> Self {
        let dir = scratch(name);
        let root = dir.join("repo");
        std::fs::create_dir_all(&root).expect("the tree");
        std::fs::write(root.join("a.txt"), "before\n").expect("a file");
        Bench {
            root,
            state: dir.join("state"),
        }
    }

    fn turn(&self, session: &str, program: &str, verify: &str) -> (i32, String) {
        hades(&[
            "turn",
            "make stats.sh exist",
            "--root",
            &self.root.display().to_string(),
            "--state",
            &self.state.display().to_string(),
            "--session",
            session,
            "--program",
            program,
            "--policy",
            "open",
            "--verify",
            verify,
        ])
    }

    /// The session as the store has it, which is what a daemon restarting would find.
    fn load(&self, session: &str) -> Session<FileStore, SystemClock> {
        Session::load(
            FileStore::new(self.state.join("sessions")),
            SystemClock,
            SessionId::new(session),
        )
        .expect("the log the turn just wrote")
    }
}

fn carried(learned: &Learned) -> String {
    learned
        .sections()
        .iter()
        .map(|section| section.text().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn a_failed_check_reads_back_as_a_try_carrying_the_program_and_the_output() {
    let bench = Bench::new("failed");
    let (code, text) = bench.turn(
        "s",
        "printf 'a\\n' > a.txt",
        "test -f stats.sh || { echo nothing wrote stats.sh; exit 1; }",
    );
    assert_eq!(code, 1, "the check was supposed to fail: {text}");

    let learned = Learned::from_log(&bench.load("s"));

    assert_eq!(learned.tries().len(), 1, "{}", learned.tries().len());
    let tried = &learned.tries()[0];
    assert_eq!(tried.program(), "printf 'a\\n' > a.txt");
    assert_eq!(tried.state(), "failed");
    assert!(
        tried.output().contains("nothing wrote stats.sh"),
        "the check's own output is what the next attempt needs: {:?}",
        tried.output()
    );
    assert!(
        learned.constraints().is_empty(),
        "a failed check established nothing to honour"
    );
}

#[test]
fn a_passing_run_reads_back_as_the_constraints_the_walk_recorded_live() {
    let bench = Bench::new("passed");
    let (code, text) = bench.turn("s", "touch stats.sh", "test -f stats.sh");
    assert_eq!(code, 0, "{text}");

    let learned = Learned::from_log(&bench.load("s"));

    assert!(learned.tries().is_empty(), "nothing failed");
    assert_eq!(
        learned.constraints().len(),
        2,
        "{:?}",
        learned.constraints()
    );
    let texts: Vec<&str> = learned
        .constraints()
        .iter()
        .map(|constraint| constraint.text())
        .collect();
    assert!(
        texts[0].contains("test -f stats.sh"),
        "the accepted constraint names the check that was satisfied: {texts:?}"
    );
    assert!(
        texts[1].contains("touch stats.sh"),
        "and the second names the program that satisfied it: {texts:?}"
    );
    assert_eq!(learned.constraints()[0].from(), "accept");
}

/// The claim in one test: a resumed run is told what the last one did, in the text of the prompt.
#[test]
fn a_resumed_run_is_told_what_the_last_one_did() {
    let bench = Bench::new("resumed");
    bench.turn(
        "s",
        "printf 'a\\n' > a.txt",
        "test -f stats.sh || { echo nothing wrote stats.sh; exit 1; }",
    );

    let session = bench.load("s");
    let resumed = Learned::from_log(&session);
    let fresh = Learned::new();

    assert!(
        fresh.sections().is_empty(),
        "a fresh run is told nothing about a history it does not have"
    );
    let text = carried(&resumed);
    assert!(
        text.contains("printf 'a\\n' > a.txt"),
        "the resumed prompt should carry the program that failed: {text:?}"
    );
    assert!(
        text.contains("nothing wrote stats.sh"),
        "and what the check said about it, which is the half that says what went wrong: {text:?}"
    );
    assert!(
        !carried(&fresh).contains("nothing wrote"),
        "a fresh run must not be handed the same text"
    );
}

/// A compaction is where the reading starts. The log keeps every entry; the view is what shortens.
#[test]
fn a_compaction_is_where_the_reading_starts() {
    let bench = Bench::new("compacted");
    bench.turn(
        "s",
        "printf 'a\\n' > a.txt",
        "test -f stats.sh || { echo nothing wrote stats.sh; exit 1; }",
    );

    let mut session = bench.load("s");
    assert!(
        !Learned::from_log(&session).is_empty(),
        "there is a failed check to read before the compaction"
    );

    session
        .compact(
            None,
            "the run got as far as rewriting a.txt and the check failed",
        )
        .expect("a compaction");

    let learned = Learned::from_log(&session);
    // Nothing before the cut is carried as a try or a constraint; the summary is, because a cut that leaves
    // no note leaves the next run blind to the work it is continuing.
    assert!(
        learned.tries().is_empty() && learned.constraints().is_empty(),
        "a compaction is where the reading starts, so no attempt before it is carried: {:?}",
        learned.tries()
    );
    assert_eq!(
        learned.summary(),
        Some("the run got as far as rewriting a.txt and the check failed"),
        "the cut says what it cut"
    );
    assert!(
        !carried(&learned).contains("printf"),
        "and the program itself is gone with it"
    );

    // The entries are still there. The log is the record; the view is what a reader is shown.
    assert!(
        session.len() > 1,
        "compaction is a view and never a deletion"
    );
}

#[test]
fn a_session_with_nothing_in_it_reads_back_as_nothing() {
    let bench = Bench::new("empty");
    let store = FileStore::new(bench.state.join("sessions"));
    let session = Session::create(store, SystemClock, SessionId::new("fresh")).expect("a session");

    assert!(
        Learned::from_log(&session).is_empty(),
        "an empty log carries nothing, and says so rather than inventing a history"
    );
}

/// The pair that keeps the reader honest: what it reads back has to be what the walk recorded, not a second
/// spelling of it. Both go through `context::accepted`, so this pins the strings rather than the plumbing.
#[test]
fn what_is_read_back_is_what_the_walk_writes() {
    let bench = Bench::new("pair");
    bench.turn("s", "touch stats.sh", "test -f stats.sh");
    let read_back = Learned::from_log(&bench.load("s"));

    let written = harness::context::accepted("test -f stats.sh", "touch stats.sh");

    assert_eq!(
        read_back.constraints(),
        written.as_slice(),
        "the log reader and the walk disagree about what an accepted result establishes"
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
    let base = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/scratch");
    std::fs::create_dir_all(&base).ok();
    std::fs::canonicalize(&base).unwrap_or(base)
}
