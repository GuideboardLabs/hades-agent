// The porting oracle: the behaviours of the Python suite in `tests/test_hades_curator.py`, restated
// against this crate.
//
// When these pass, governance has been ported, because they pin the same contracts: one writer with a
// stamp to prove it, provenance that keeps its shape, a refusal for anything that would become an
// orphan or a dead end, fences that are never read, and an audit that reports rather than reassures.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use governance::{Curation, Curator, ProblemKind, Refusal, Timestamp};
use mneme::{Area, Clock, Date, Field, FileVault, Frontmatter, NoteStore, SqliteStore, Vault};

const FIXED: &str = "2026-10-17T00:00:00+00:00";
const TODAY: &str = "2026-10-17";

#[derive(Debug, Clone, Copy)]
struct FixedClock(Date);

impl Clock for FixedClock {
    fn today(&self) -> Date {
        self.0
    }
}

#[derive(Debug, Clone, Copy)]
struct FixedTimestamp;

impl Timestamp for FixedTimestamp {
    fn now(&self) -> String {
        FIXED.to_string()
    }
}

type TestCurator = Curator<Store, FixedClock, FixedTimestamp>;

/// The one line a store adapter has to provide: how a test opens it.
trait TestStore: NoteStore {
    fn open_at(root: &Path) -> Self;
}

impl TestStore for FileVault {
    fn open_at(_root: &Path) -> Self {
        FileVault::new()
    }
}

impl TestStore for SqliteStore {
    fn open_at(root: &Path) -> Self {
        SqliteStore::open(root.join(".mneme.db"), root).expect("sqlite store")
    }
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn scratch(name: &str) -> PathBuf {
    let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = scratch_base().join(format!("governance-{name}-{}-{unique}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn today() -> Date {
    Date::parse_iso(TODAY).expect("a fixed day")
}

/// A vault with its five areas made, under a curator with the default fences.
fn fresh(name: &str) -> (TestCurator, PathBuf) {
    let root = scratch(name);
    let vault = Vault::new(root.clone(), Store::open_at(&root), FixedClock(today()));
    vault.init(None).expect("init");
    (Curator::new(vault, FixedTimestamp), root)
}

/// The same, with fences of the test's choosing.
fn fresh_with(name: &str, fences: &[&str]) -> (TestCurator, PathBuf) {
    let root = scratch(name);
    let vault = Vault::new(root.clone(), Store::open_at(&root), FixedClock(today()));
    vault.init(None).expect("init");
    let fences = fences.iter().map(|fence| fence.to_string()).collect();
    (Curator::with_fences(vault, FixedTimestamp, fences), root)
}

fn capture(curator: &TestCurator, title: &str, provenance: &str) -> PathBuf {
    curator
        .capture("Body of the finding.", Some(title), Some(provenance))
        .expect("capture")
}

fn curation(topics: &[&str], projects: &[&str], summary: &str, provenance: &str) -> Curation {
    Curation {
        topics: topics.iter().map(|topic| topic.to_string()).collect(),
        projects: projects.iter().map(|project| project.to_string()).collect(),
        summary: summary.to_string(),
        provenance: provenance.to_string(),
    }
}

fn scalar(note: &mneme::Note, key: &str) -> Option<String> {
    match note.frontmatter.extra.get(key) {
        Some(Field::Scalar(text)) => Some(text.clone()),
        _ => None,
    }
}

// ---------------- writing ----------------

#[test]
fn file_accepts_a_complete_note() {
    let (curator, root) = fresh("accept");
    let filed = curator
        .file(
            &capture(&curator, "Finding", "https://example.com/a"),
            Area::Research,
            &curation(
                &["agent-harness"],
                &["hades-agent"],
                "A summary.",
                "https://example.com/a",
            ),
        )
        .expect("a complete note");
    assert_eq!(filed.parent().expect("parent"), root.join("20 Research"));
    assert!(
        curator
            .vault()
            .read_note(&filed)
            .expect("read")
            .is_durable()
    );
}

#[test]
fn file_refuses_without_a_topic() {
    let (curator, _) = fresh("no-topic");
    let note = capture(&curator, "Finding", "https://example.com/a");
    let refused = curator.file(
        &note,
        Area::Research,
        &curation(&[], &[], "s", "https://e.com"),
    );
    assert!(matches!(refused, Err(Refusal::Unfiled { .. })));
    assert!(
        curator.vault().store().exists(&note),
        "a refused note stays in the Inbox"
    );
}

#[test]
fn file_refuses_without_a_summary() {
    let (curator, _) = fresh("no-summary");
    let refused = curator.file(
        &capture(&curator, "Finding", "https://example.com/a"),
        Area::Research,
        &curation(&["t"], &[], "", "https://e.com"),
    );
    assert!(matches!(refused, Err(Refusal::Unfiled { .. })));
}

#[test]
fn file_refuses_without_provenance() {
    let (curator, _) = fresh("no-provenance");
    let refused = curator.file(
        &capture(&curator, "Finding", "https://example.com/a"),
        Area::Research,
        &curation(&["t"], &[], "s", "   "),
    );
    assert!(matches!(refused, Err(Refusal::MissingProvenance { .. })));
}

#[test]
fn file_refuses_an_area_that_is_not_durable() {
    let (curator, _) = fresh("not-durable");
    let refused = curator.file(
        &capture(&curator, "Finding", "https://example.com/a"),
        Area::Topics,
        &curation(&["t"], &[], "s", "p"),
    );
    assert!(matches!(refused, Err(Refusal::Unfiled { .. })));
}

#[test]
fn file_refuses_a_fenced_destination() {
    let (curator, _) = fresh_with("fenced-destination", &["20 Research"]);
    let refused = curator.file(
        &capture(&curator, "Finding", "https://example.com/a"),
        Area::Research,
        &curation(&["t"], &[], "s", "p"),
    );
    assert!(matches!(refused, Err(Refusal::Fenced { .. })));
}

#[test]
fn provenance_keeps_its_shape() {
    let (curator, _) = fresh("provenance");
    let url = curator
        .file(
            &capture(&curator, "From a url", "https://example.com/a"),
            Area::Research,
            &curation(&["t"], &[], "s", "https://example.com/a"),
        )
        .expect("file a url");
    let prose = curator
        .file(
            &capture(&curator, "From a conversation", "https://example.com/a"),
            Area::Research,
            &curation(
                &["t"],
                &[],
                "s",
                "a conversation with the owner on 2026-10-17",
            ),
        )
        .expect("file prose");
    assert_eq!(
        curator
            .vault()
            .read_note(&url)
            .expect("read")
            .frontmatter
            .source_url
            .as_deref(),
        Some("https://example.com/a")
    );
    let prose_note = curator.vault().read_note(&prose).expect("read");
    assert!(
        prose_note
            .frontmatter
            .provenance
            .as_deref()
            .expect("provenance")
            .starts_with("a conversation")
    );
}

#[test]
fn the_curator_stamps_what_it_wrote() {
    let (curator, _) = fresh("stamp");
    let filed = curator
        .file(
            &capture(&curator, "Finding", "https://example.com/a"),
            Area::Research,
            &curation(&["t"], &[], "s", "p"),
        )
        .expect("file");
    let note = curator.vault().read_note(&filed).expect("read");
    assert_eq!(scalar(&note, "curator").as_deref(), Some("curator"));
    assert_eq!(scalar(&note, "curated_at").as_deref(), Some(FIXED));
}

#[test]
fn filing_refreshes_the_index() {
    let (curator, root) = fresh("index");
    curator
        .file(
            &capture(&curator, "Finding", "https://example.com/a"),
            Area::Research,
            &curation(&["agent-harness"], &[], "S.", "p"),
        )
        .expect("file");
    let text = std::fs::read_to_string(root.join("INDEX.md")).expect("read");
    assert!(text.contains("### agent-harness (1)"), "{text}");
}

// ---------------- fences ----------------

#[test]
fn is_fenced_matches_on_path_parts() {
    let (curator, _) = fresh("fence-parts");
    assert!(curator.is_fenced(Path::new("2-Areas/Family/note.md")));
    assert!(curator.is_fenced(Path::new("People/alice.md")));
    assert!(!curator.is_fenced(Path::new("20 Research/finding.md")));
    assert!(
        !curator.is_fenced(Path::new("20 Research/2-Areas.md")),
        "a filename is not a directory"
    );
}

#[test]
fn a_fenced_area_is_skipped_by_readers() {
    let (curator, _) = fresh("fenced-reader");
    curator
        .file(
            &capture(&curator, "Finding", "https://example.com/a"),
            Area::Research,
            &curation(&["t"], &[], "s", "p"),
        )
        .expect("file");
    let fences = vec!["20 Research".to_string()];
    assert!(
        curator
            .vault()
            .iter_notes(Some(Area::Research), &fences)
            .expect("walk")
            .is_empty()
    );
    assert_eq!(
        curator
            .vault()
            .iter_notes(Some(Area::Research), &[])
            .expect("walk")
            .len(),
        1
    );
}

// ---------------- audit ----------------

#[test]
fn a_fresh_vault_audits_clean() {
    let (curator, _) = fresh("clean");
    let report = curator.audit().expect("audit");
    assert!(report.ok(), "{:?}", report.lines());
    assert_eq!(report.count("inbox"), 0);
    assert_eq!(report.count("research"), 0);
}

#[test]
fn audit_is_clean_after_two_complete_notes() {
    let (curator, _) = fresh("two-notes");
    for index in 0..2 {
        curator
            .file(
                &capture(
                    &curator,
                    &format!("Finding {index}"),
                    "https://example.com/a",
                ),
                Area::Research,
                &curation(
                    &["agent-harness"],
                    &["hades-agent"],
                    &format!("Summary {index}."),
                    "https://example.com/a",
                ),
            )
            .expect("file");
    }
    let report = curator.audit().expect("audit");
    assert!(report.ok(), "{:?}", report.lines());
    assert_eq!(report.count("research"), 2);
    assert_eq!(report.count("inbox"), 0);
}

#[test]
fn audit_flags_a_hand_written_note_with_no_provenance() {
    let (curator, root) = fresh("smuggled");
    curator
        .vault()
        .write_note(
            &root.join("20 Research").join("smuggled.md"),
            &Frontmatter {
                title: Some("Smuggled".to_string()),
                summary: Some("s".to_string()),
                topics: vec!["t".to_string()],
                ..Frontmatter::default()
            },
            "body",
        )
        .expect("write");
    let report = curator.audit().expect("audit");
    assert!(!report.ok());
    assert_eq!(report.problems_of(ProblemKind::MissingProvenance).len(), 1);
    assert_eq!(
        report.problems_of(ProblemKind::MissingProvenance)[0].detail,
        "no source_url or provenance"
    );
}

#[test]
fn audit_flags_a_durable_note_missing_a_summary_and_a_topic() {
    let (curator, root) = fresh("thin");
    curator
        .vault()
        .write_note(
            &root.join("20 Research").join("thin.md"),
            &Frontmatter {
                title: Some("Thin".to_string()),
                source_url: Some("https://e.com".to_string()),
                ..Frontmatter::default()
            },
            "body",
        )
        .expect("write");
    let report = curator.audit().expect("audit");
    let details: Vec<String> = report
        .problems
        .iter()
        .map(|problem| format!("{} {}", problem.kind, problem.detail))
        .collect();
    assert!(
        details.contains(&"missing_summary no summary".to_string()),
        "{details:?}"
    );
    assert!(
        details.contains(&"missing_topic no topic".to_string()),
        "{details:?}"
    );
    assert!(!report.ok());
}

#[test]
fn audit_flags_a_note_in_a_fenced_subtree() {
    let (curator, root) = fresh("fenced-subtree");
    let private = root.join("2-Areas").join("Family");
    std::fs::create_dir_all(&private).expect("mkdir");
    std::fs::write(
        private.join("note.md"),
        "---\ntitle: Private\n---\n\nno summary, no topic\n",
    )
    .expect("write");

    let report = curator.audit().expect("audit");
    let fenced = report
        .fenced
        .iter()
        .filter(|problem| problem.kind == ProblemKind::FencedPath)
        .collect::<Vec<_>>();
    assert_eq!(fenced.len(), 1);
    assert_eq!(fenced[0].path, "2-Areas/Family/note.md");
    assert_eq!(fenced[0].detail, "a path check, not a read");
    assert!(
        report
            .problems
            .iter()
            .all(|problem| !problem.path.contains("2-Areas")),
        "the audit did not read its contents: {:?}",
        report.lines()
    );
    // A fence is the boundary working. A vault that fails its audit for having one would be a vault that is
    // being asked to stop holding the things the boundary protects.
    assert!(
        report.ok(),
        "a fenced note alone is not a defect: {:?}",
        report.lines()
    );
    assert!(
        report
            .lines()
            .iter()
            .any(|line| line.contains("fenced_path")),
        "and it is still reported: {:?}",
        report.lines()
    );
}

#[test]
fn audit_flags_an_inbox_over_the_cap() {
    let (curator, _) = fresh("over-cap");
    for index in 0..=governance::INBOX_CAP {
        let spark = format!("spark {index}");
        curator
            .capture(&spark, Some(&spark), None)
            .expect("capture");
    }
    let report = curator.audit().expect("audit");
    let over = report.problems_of(ProblemKind::InboxOverCap);
    assert_eq!(over.len(), 1);
    assert!(
        over[0]
            .detail
            .contains(&format!("cap is {}", governance::INBOX_CAP)),
        "{}",
        over[0].detail
    );
}

#[test]
fn audit_reports_stale_candidates() {
    let (curator, root) = fresh("stale");
    curator
        .vault()
        .store()
        .write(
            &root.join("00 Inbox").join("2026-01-01-ancient.md"),
            "---\ntitle: Ancient\ncaptured: 2026-01-01\n---\n\nbody\n",
        )
        .expect("write");
    let report = curator.audit().expect("audit");
    assert_eq!(report.count("stale"), 1);
    assert_eq!(report.problems_of(ProblemKind::StaleInbox).len(), 1);
}

#[test]
fn stale_detail_names_at_most_five_candidates() {
    let (curator, root) = fresh("stale-detail");
    for day in 1..=7 {
        curator
            .vault()
            .store()
            .write(
                &root
                    .join("00 Inbox")
                    .join(format!("2026-01-{day:02}-old-{day}.md")),
                &format!("---\ntitle: Old {day}\ncaptured: 2026-01-{day:02}\n---\n\nbody\n"),
            )
            .expect("write");
    }
    let report = curator.audit().expect("audit");
    assert_eq!(report.count("stale"), 7);
    let stale = report.problems_of(ProblemKind::StaleInbox);
    assert_eq!(stale.len(), 1);
    let detail = &stale[0].detail;
    assert!(detail.starts_with("7 older than 14 days: "), "{detail}");
    let named = detail.split_once(": ").expect("a prefix").1;
    assert_eq!(named.split(", ").count(), 5, "{detail}");
}

#[test]
fn report_lines_are_readable() {
    let (curator, _) = fresh("lines");
    let lines = curator.audit().expect("audit").lines();
    assert!(
        lines.iter().any(|line| line.starts_with("inbox:")),
        "{lines:?}"
    );
}

// ---------------- the one writer ----------------

#[test]
fn the_one_writer_is_a_type_and_the_note_says_who_wrote_it() {
    // The Python cannot say this in its type system, so it scans its own source for the write calls
    // instead. Here the move takes a permit whose only constructor is the vault's, and the holder it
    // carries is what the stamp records, so there is nothing to scan for.
    let (curator, _) = fresh("one-writer");
    let filed = curator
        .file(
            &capture(&curator, "Finding", "https://example.com/a"),
            Area::Research,
            &curation(&["t"], &[], "s", "https://example.com/a"),
        )
        .expect("file");
    let note = curator.vault().read_note(&filed).expect("read");
    assert_eq!(scalar(&note, "curator").as_deref(), Some("curator"));
}

// ---------------- end to end, with the real clock and the real timestamp ----------------

/// What the oracle cannot pin: the real clock, the real timestamp and the real disk. Driven by hand, and
/// the report re-read after the work rather than trusted.
#[test]
fn end_to_end_on_disk_with_the_real_timestamp() {
    let root = scratch("end-to-end");
    let vault = Vault::new(root.clone(), FileVault::new(), mneme::SystemClock);
    vault.init(None).expect("init");
    let curator = Curator::new(vault, governance::SystemTimestamp);

    let captured = curator
        .capture("A finding worth keeping.", Some("End to end"), None)
        .expect("capture");
    let filed = curator
        .file(
            &captured,
            Area::Research,
            &curation(
                &["hades-agent"],
                &["hades-agent"],
                "A summary.",
                "a conversation with the owner",
            ),
        )
        .expect("file");

    let on_disk = std::fs::read_to_string(root.join("INDEX.md")).expect("read");
    assert!(on_disk.contains("### hades-agent (1)"), "{on_disk}");
    assert!(
        !curator.vault().store().exists(&captured) && curator.vault().store().exists(&filed),
        "moved, not copied"
    );

    let note = curator.vault().read_note(&filed).expect("read");
    assert_eq!(
        note.frontmatter.provenance.as_deref(),
        Some("a conversation with the owner"),
        "prose lands in the provenance key"
    );
    assert!(note.is_durable());
    assert_eq!(scalar(&note, "curator").as_deref(), Some("curator"));
    let stamp = scalar(&note, "curated_at").expect("a stamp");
    assert_eq!(stamp.len(), 25, "{stamp}");
    assert!(stamp.contains('T') && stamp.ends_with("+00:00"), "{stamp}");

    let report = curator.audit().expect("audit");
    assert!(report.ok(), "{:?}", report.lines());
    assert_eq!(report.count("research"), 1);
    assert_eq!(report.count("inbox"), 0);
    assert_eq!(report.count("stale"), 0);
}

fn scratch_base() -> std::path::PathBuf {
    // Off the source tree's disk: a full root filesystem must not be filled by a test run, and the build
    // directory is the natural place when Cargo names one. Without one, a directory under the Hades home
    // is used rather than the machine's temp directory, which on this box lives under the live install and
    // is refused by Hades' boundary.
    let named = std::env::var("CARGO_TARGET_DIR")
        .ok()
        .filter(|dir| !dir.trim().is_empty());
    if let Some(dir) = named {
        return std::path::PathBuf::from(dir).join("scratch");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let base = std::path::PathBuf::from(home).join(".hades/test-scratch");
    std::fs::create_dir_all(&base).ok();
    std::fs::canonicalize(&base).unwrap_or(base)
}
