//! The porting oracle: the behaviours of the 24 Python tests, restated against this crate.
//!
//! When these pass, the session tree has been ported, because they pin the same contracts:
//! append-only with parent pointers, fork reusing ids, compaction as a view, and loud failure on a
//! truncated or corrupt line.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::json;
use session_tree::{Clock, EntryId, Fault, FileStore, Kind, Session, SessionId};

#[derive(Debug, Clone)]
struct FixedClock(&'static str);

impl Clock for FixedClock {
    fn now(&self) -> String {
        self.0.to_string()
    }
}

const AT: &str = "2026-10-17T00:00:00+00:00";

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A scratch directory without pulling in a dependency for it.
fn scratch(name: &str) -> PathBuf {
    let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = scratch_base().join(format!(
        "session-tree-{name}-{}-{unique}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn session(name: &str) -> (Session<FileStore, FixedClock>, FileStore, PathBuf) {
    let dir = scratch(name);
    let store = FileStore::new(&dir);
    let session =
        Session::create(store.clone(), FixedClock(AT), SessionId::new("s1")).expect("create");
    (session, store, dir)
}

fn payload_text(entry: &session_tree::Entry) -> Option<&str> {
    entry.payload.get("text").and_then(|value| value.as_str())
}

// ---------------- creation and appending ----------------

#[test]
fn create_writes_a_meta_root() {
    let (session, _, _) = session("meta-root");
    assert_eq!(session.len(), 1);
    let root = &session.entries()[0];
    assert_eq!(root.id, EntryId(0));
    assert_eq!(root.parent, None);
    assert_eq!(root.kind, Kind::Meta);
    assert_eq!(root.payload["session_id"], json!("s1"));
    assert_eq!(root.at, AT, "the injected clock is the one that is used");
}

#[test]
fn append_assigns_ids_and_links_parents() {
    let (mut session, _, _) = session("append");
    let first = session
        .append(Kind::User, json!({"text": "hello"}))
        .unwrap();
    let second = session
        .append(Kind::Assistant, json!({"text": "hi"}))
        .unwrap();
    assert_eq!(first, EntryId(1));
    assert_eq!(second, EntryId(2));
    assert_eq!(session.get(first).unwrap().parent, Some(EntryId(0)));
    assert_eq!(session.get(second).unwrap().parent, Some(EntryId(1)));
    assert_eq!(session.head(), Some(EntryId(2)));
    assert_eq!(session.len(), 3);
}

#[test]
fn kind_parsing_is_exact_and_never_coerces() {
    assert_eq!(Kind::parse("tool_call"), Some(Kind::ToolCall));
    assert_eq!(Kind::parse("toolcall"), None);
    assert_eq!(Kind::parse("Tool_Call"), None);
    assert_eq!(Kind::parse(""), None);
}

#[test]
fn one_json_object_per_line() {
    let (mut session, store, _) = session("one-per-line");
    session.append(Kind::User, json!({"text": "one"})).unwrap();
    session.append(Kind::User, json!({"text": "two"})).unwrap();
    let text = std::fs::read_to_string(store.path_of(session.id())).unwrap();
    let lines: Vec<&str> = text.trim().lines().collect();
    assert_eq!(lines.len(), session.len());
    for line in lines {
        let value: serde_json::Value = serde_json::from_str(line).expect("each line is JSON");
        assert!(value.is_object());
    }
}

// ---------------- loading ----------------

#[test]
fn load_round_trips_through_disk() {
    let (mut session, store, _) = session("round-trip");
    session
        .append(Kind::User, json!({"text": "hello"}))
        .unwrap();
    session
        .append(Kind::ToolResult, json!({"ok": true, "items": [1, 2, 3]}))
        .unwrap();
    let again = Session::load(store, FixedClock(AT), session.id().clone()).unwrap();
    assert_eq!(again.entries(), session.entries());
    assert_eq!(again.head(), session.head());
}

#[test]
fn load_of_a_missing_file_is_explicit() {
    let dir = scratch("missing");
    let store = FileStore::new(&dir);
    let fault = Session::load(store, FixedClock(AT), SessionId::new("nope")).unwrap_err();
    assert!(matches!(fault, Fault::Io { .. }), "got {fault:?}");
}

#[test]
fn a_truncated_last_line_is_loud_and_names_the_line() {
    let (mut session, store, _) = session("truncated");
    session
        .append(Kind::User, json!({"text": "hello"}))
        .unwrap();
    let path = store.path_of(session.id());
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("{\"v\":1,\"id\":2,\"parent\":1,\"kind\":\"assis");
    std::fs::write(&path, text).unwrap();

    let fault = Session::load(store, FixedClock(AT), session.id().clone()).unwrap_err();
    match fault {
        Fault::CorruptLog { line, .. } => assert_eq!(line, 3, "the offending line is named"),
        other => panic!("expected CorruptLog, got {other:?}"),
    }
}

fn write_lines(name: &str, lines: &[&str]) -> (FileStore, SessionId) {
    let dir = scratch(name);
    let store = FileStore::new(&dir);
    let id = SessionId::new("s1");
    std::fs::write(store.path_of(&id), format!("{}\n", lines.join("\n"))).unwrap();
    (store, id)
}

#[test]
fn corrupt_cases_each_fail_loudly() {
    let root = r#"{"v":1,"id":0,"parent":null,"kind":"meta","payload":{},"at":"t"}"#;
    let cases: Vec<(&str, Vec<&str>)> = vec![
        ("not-json", vec![root, "}{"]),
        (
            "missing-key",
            vec![root, r#"{"v":1,"id":1,"parent":0,"kind":"user","at":"t"}"#],
        ),
        (
            "unknown-kind",
            vec![
                root,
                r#"{"v":1,"id":1,"parent":0,"kind":"sneaky","payload":{},"at":"t"}"#,
            ],
        ),
        (
            "future-schema",
            vec![
                root,
                r#"{"v":99,"id":1,"parent":0,"kind":"user","payload":{},"at":"t"}"#,
            ],
        ),
        (
            "dangling-parent",
            vec![
                root,
                r#"{"v":1,"id":1,"parent":7,"kind":"user","payload":{},"at":"t"}"#,
            ],
        ),
        (
            "bad-parent-type",
            vec![
                root,
                r#"{"v":1,"id":1,"parent":"0","kind":"user","payload":{},"at":"t"}"#,
            ],
        ),
        ("duplicate-id", vec![root, root]),
    ];
    for (name, lines) in cases {
        let (store, id) = write_lines(&format!("corrupt-{name}"), &lines);
        let fault = Session::load(store, FixedClock(AT), id).unwrap_err();
        assert!(
            matches!(fault, Fault::CorruptLog { .. }),
            "{name}: expected CorruptLog, got {fault:?}"
        );
    }
}

// ---------------- chains and views ----------------

#[test]
fn path_to_walks_from_the_root() {
    let (mut session, _, _) = session("path-to");
    session.append(Kind::User, json!({"text": "one"})).unwrap();
    session
        .append(Kind::Assistant, json!({"text": "two"}))
        .unwrap();
    session
        .append(Kind::User, json!({"text": "three"}))
        .unwrap();

    let ids = |entries: Vec<&session_tree::Entry>| -> Vec<u64> {
        entries.into_iter().map(|entry| entry.id.0).collect()
    };
    assert_eq!(
        ids(session.path_to(Some(EntryId(2))).unwrap()),
        vec![0, 1, 2]
    );
    assert_eq!(ids(session.path_to(None).unwrap()), vec![0, 1, 2, 3]);
    assert_eq!(ids(session.path_to(Some(EntryId(0))).unwrap()), vec![0]);
}

#[test]
fn unknown_entry_fails() {
    let (session, _, _) = session("unknown");
    assert!(matches!(
        session.get(EntryId(9)),
        Err(Fault::UnknownEntry { .. })
    ));
    assert!(matches!(
        session.path_to(Some(EntryId(9))),
        Err(Fault::UnknownEntry { .. })
    ));
    assert!(matches!(
        session.fork(Some(EntryId(9))),
        Err(Fault::UnknownEntry { .. })
    ));
}

#[test]
fn view_equals_the_chain_without_compaction() {
    let (mut session, _, _) = session("view-plain");
    session.append(Kind::User, json!({"text": "one"})).unwrap();
    let view: Vec<u64> = session.view().into_iter().map(|entry| entry.id.0).collect();
    assert_eq!(view, vec![0, 1]);
}

#[test]
fn compaction_is_lossless() {
    let (mut session, store, _) = session("compaction");
    for turn in 0..6 {
        session
            .append(Kind::User, json!({"text": format!("turn {turn}")}))
            .unwrap();
    }
    let before = session.len();
    session
        .compact(Some(EntryId(5)), "Six turns about a schema.")
        .unwrap();
    session
        .append(Kind::User, json!({"text": "and one after it"}))
        .unwrap();

    // nothing was discarded
    assert_eq!(session.len(), before + 2);
    let ids: Vec<u64> = session.entries().iter().map(|entry| entry.id.0).collect();
    assert_eq!(ids, (0..(before as u64 + 2)).collect::<Vec<u64>>());
    assert_eq!(
        std::fs::read_to_string(store.path_of(session.id()))
            .unwrap()
            .trim()
            .lines()
            .count(),
        before + 2
    );

    // but the view is shorter and carries the summary
    let view = session.view();
    assert_eq!(
        view[0].payload["summary"],
        json!("Six turns about a schema.")
    );
    assert_eq!(view[0].payload["covers"], json!([0, 5]));
    assert_eq!(view.len(), 2);
    assert_eq!(view[1].id, EntryId(before as u64 + 1));
}

#[test]
fn a_second_compaction_supersedes_the_first_in_the_view() {
    let (mut session, _, _) = session("compaction-twice");
    for turn in 0..6 {
        session
            .append(Kind::User, json!({"text": format!("turn {turn}")}))
            .unwrap();
    }
    session.compact(Some(EntryId(2)), "first").unwrap();
    session.compact(Some(EntryId(6)), "second").unwrap();
    let view = session.view();
    assert_eq!(view[0].payload["summary"], json!("second"));
    assert_eq!(view.len(), 1, "nothing follows the second compaction yet");
}

#[test]
fn compaction_survives_a_reload() {
    let (mut session, store, _) = session("compaction-reload");
    session.append(Kind::User, json!({"text": "one"})).unwrap();
    session
        .append(Kind::Assistant, json!({"text": "two"}))
        .unwrap();
    session.compact(None, "short").unwrap();

    let again = Session::load(store, FixedClock(AT), session.id().clone()).unwrap();
    assert_eq!(again.view()[0].payload["summary"], json!("short"));
    assert_eq!(again.len(), 4);
}

#[test]
fn a_plan_survives_a_cut_while_the_history_before_it_does_not() {
    let (mut session, _, _) = session("plan-across-cut");
    session.append(Kind::User, json!({"text": "one"})).unwrap();
    session
        .append(Kind::Plan, json!({"plan": "1. do the thing"}))
        .unwrap();
    session
        .append(Kind::Assistant, json!({"text": "working"}))
        .unwrap();
    session.compact(None, "short").unwrap();

    let view = session.view();
    assert_eq!(view[0].kind, Kind::Plan);
    assert_eq!(view[1].kind, Kind::Compaction);
    assert!(!view.iter().any(|entry| entry.kind == Kind::User));
    assert_eq!(session.len(), 5);
}

// ---------------- forking ----------------

#[test]
fn fork_copies_the_chain_with_the_same_ids() {
    let (mut session, _, _) = session("fork-ids");
    session.append(Kind::User, json!({"text": "one"})).unwrap();
    session
        .append(Kind::Assistant, json!({"text": "two"}))
        .unwrap();

    let child = session.fork(None).unwrap();
    let ids: Vec<u64> = child.entries().iter().map(|entry| entry.id.0).collect();
    let parents: Vec<Option<u64>> = child
        .entries()
        .iter()
        .map(|entry| entry.parent.map(|parent| parent.0))
        .collect();
    assert_eq!(ids, vec![0, 1, 2]);
    assert_eq!(parents, vec![None, Some(0), Some(1)]);
    assert_eq!(payload_text(&child.entries()[1]), Some("one"));
    assert_eq!(payload_text(&child.entries()[2]), Some("two"));
}

#[test]
fn create_refuses_to_overwrite_an_existing_session() {
    let dir = scratch("no-clobber");
    let store = FileStore::new(&dir);
    let id = SessionId::new("s1");
    Session::create(store.clone(), FixedClock(AT), id.clone()).unwrap();
    let fault = Session::create(store, FixedClock(AT), id).unwrap_err();
    assert!(
        matches!(fault, Fault::SessionExists { .. }),
        "got {fault:?}"
    );
}

#[test]
fn fork_records_where_it_came_from() {
    let (mut session, _, _) = session("fork-meta");
    session.append(Kind::User, json!({"text": "one"})).unwrap();
    let child = session.fork(Some(EntryId(1))).unwrap();
    let meta = &child.entries()[0].payload;
    assert_eq!(meta["forked_from"], json!("s1"));
    assert_eq!(meta["forked_at"], json!(1));
    assert_eq!(meta["source_head"], json!(1));
}

#[test]
fn fork_leaves_the_original_untouched() {
    let (mut session, store, _) = session("fork-untouched");
    session.append(Kind::User, json!({"text": "one"})).unwrap();
    let before = std::fs::read_to_string(store.path_of(session.id())).unwrap();
    session.fork(None).unwrap();
    assert_eq!(
        std::fs::read_to_string(store.path_of(session.id())).unwrap(),
        before
    );
    assert_eq!(session.len(), 2);
}

#[test]
fn fork_at_an_early_entry_is_shorter() {
    let (mut session, _, _) = session("fork-early");
    for turn in 0..4 {
        session
            .append(Kind::User, json!({"text": format!("turn {turn}")}))
            .unwrap();
    }
    let child = session.fork(Some(EntryId(2))).unwrap();
    let ids: Vec<u64> = child.entries().iter().map(|entry| entry.id.0).collect();
    assert_eq!(ids, vec![0, 1, 2]);
    assert_eq!(child.head(), Some(EntryId(2)));
}

#[test]
fn appending_after_a_fork_continues_from_the_fork_head() {
    let (mut session, _, _) = session("fork-append");
    session.append(Kind::User, json!({"text": "one"})).unwrap();
    session
        .append(Kind::Assistant, json!({"text": "two"}))
        .unwrap();

    let mut child = session.fork(Some(EntryId(1))).unwrap();
    let id = child
        .append(Kind::Assistant, json!({"text": "divergent"}))
        .unwrap();
    assert_eq!(id, EntryId(2));
    assert_eq!(child.get(id).unwrap().parent, Some(EntryId(1)));
    // the parent's own entry 2 still says what it always said
    assert_eq!(payload_text(session.get(EntryId(2)).unwrap()), Some("two"));
    assert_eq!(
        payload_text(child.get(EntryId(2)).unwrap()),
        Some("divergent")
    );
}

#[test]
fn two_forks_of_the_same_point_are_independent() {
    let (mut session, store, _) = session("fork-independent");
    session.append(Kind::User, json!({"text": "one"})).unwrap();

    let mut left = session.fork(None).unwrap();
    let mut right = session.fork(None).unwrap();
    left.append(Kind::Assistant, json!({"text": "from left"}))
        .unwrap();
    right
        .append(Kind::Assistant, json!({"text": "from right"}))
        .unwrap();

    assert_ne!(
        left.id(),
        right.id(),
        "a second fork must not clobber the first"
    );
    assert!(left.id().as_str().starts_with("s1-fork-1"));
    assert_eq!(
        payload_text(left.get(EntryId(2)).unwrap()),
        Some("from left")
    );
    assert_eq!(
        payload_text(right.get(EntryId(2)).unwrap()),
        Some("from right")
    );
    assert_eq!(session.head(), Some(EntryId(1)));
    assert!(store.path_of(left.id()).exists() && store.path_of(right.id()).exists());
}

#[test]
fn a_fork_inherits_the_clock_it_was_given() {
    let dir = scratch("fork-clock");
    let store = FileStore::new(&dir);
    let mut session = Session::create(
        store.clone(),
        FixedClock("2026-05-06T07:08:09+00:00"),
        SessionId::new("s1"),
    )
    .unwrap();
    session.append(Kind::User, json!({"text": "one"})).unwrap();
    let child = session.fork(None).unwrap();
    assert_eq!(child.entries()[0].at, "2026-05-06T07:08:09+00:00");
}

#[test]
fn the_system_clock_renders_the_python_shape() {
    let text = session_tree::SystemClock.now();
    assert_eq!(text.len(), 25, "got {text:?}");
    assert_eq!(&text[10..11], "T");
    assert_eq!(&text[19..], "+00:00");
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
