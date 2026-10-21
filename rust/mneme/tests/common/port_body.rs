// The porting oracle: the behaviours of the Python suite in `tests/test_hades_vault.py`, restated
// against this crate.
//
// When these pass, the vault has been ported, because they pin the same contracts: six areas, a
// capture that lands in the Inbox and only there, filing that refuses an orphan and leaves it alone,
// durability as summary plus topic plus provenance, recall by topic and project, an index that is a
// view of the frontmatter, stale reporting, and the fence.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use mneme::{
    Area, Charter, Clock, Date, Fault, FileVault, Filing, Frontmatter, Holder, Note, NoteStore,
    SqliteStore, Vault,
};

const TODAY: &str = "2026-10-17";

#[derive(Debug, Clone, Copy)]
struct FixedClock(Date);

impl Clock for FixedClock {
    fn today(&self) -> Date {
        self.0
    }
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A scratch directory without pulling in a dependency for it.
fn scratch(name: &str) -> PathBuf {
    let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = scratch_base().join(format!("vault-{name}-{}-{unique}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn today() -> Date {
    Date::parse_iso(TODAY).expect("a fixed day")
}

type TestVault = Vault<Store, FixedClock>;

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

/// A vault with its six areas already made, which is where every test starts.
fn vault(name: &str) -> (TestVault, PathBuf) {
    let root = scratch(name);
    let vault = Vault::new(root.clone(), Store::open_at(&root), FixedClock(today()));
    vault.init(None).expect("init");
    (vault, root)
}

/// What is known about a note when it is filed, with the two defaults the Python tests use.
fn filing(topics: &[&str], projects: &[&str]) -> Filing {
    Filing {
        topics: topics.iter().map(|topic| topic.to_string()).collect(),
        projects: projects.iter().map(|project| project.to_string()).collect(),
        from_project: None,
        summary: Some("Short.".to_string()),
        source_url: Some("u".to_string()),
        provenance: None,
    }
}

fn texts(values: &[String]) -> Vec<&str> {
    values.iter().map(String::as_str).collect()
}

fn titles(notes: &[Note]) -> Vec<String> {
    notes.iter().map(Note::title).collect()
}

// ---------------- the six areas, and the charter ----------------

#[test]
fn init_creates_the_six_areas() {
    let root = scratch("init");
    let vault = Vault::new(root.clone(), FileVault::new(), FixedClock(today()));
    assert_eq!(vault.init(None).expect("init"), root);
    for name in [
        "00 Inbox",
        "10 Outbox",
        "20 Research",
        "30 Projects",
        "40 Products",
        "50 Topics",
    ] {
        assert!(root.join(name).is_dir(), "{name}");
        assert!(
            root.join(name).join("START HERE.md").is_file(),
            "{name} marker"
        );
    }
    assert!(root.join("INDEX.md").is_file());
    assert!(root.join("TAXONOMY.md").is_file());
    let taxonomy = std::fs::read_to_string(root.join("TAXONOMY.md")).expect("taxonomy");
    assert!(taxonomy.contains("## Technical (`technical`)"));
}

#[test]
fn init_writes_no_charter_without_answers() {
    let (vault, root) = vault("no-charter");
    assert!(!root.join("charter.md").exists());
    assert_eq!(vault.read_charter().expect("read"), Charter::default());
}

#[test]
fn charter_round_trips() {
    let root = scratch("charter");
    let vault = Vault::new(root, FileVault::new(), FixedClock(today()));
    let topics = vec!["memory".to_string(), "testing".to_string()];
    vault
        .init(Some(("Jac", "Ship Hades.", &topics)))
        .expect("init with answers");
    let charter = vault.read_charter().expect("read");
    assert_eq!(charter.name, "Jac");
    assert_eq!(charter.goal, "Ship Hades.");
    assert_eq!(charter.topics, topics);
    assert!(charter.is_set());
}

// ---------------- capture ----------------

#[test]
fn capture_lands_in_the_inbox_with_a_date_slug() {
    let (vault, root) = vault("capture");
    let path = vault
        .capture(
            "Body about cache invalidation.",
            Some("Cache invalidation"),
            None,
        )
        .expect("capture");
    assert_eq!(path.parent().expect("parent"), root.join("00 Inbox"));
    let name = path
        .file_name()
        .expect("name")
        .to_string_lossy()
        .into_owned();
    assert!(name.starts_with(TODAY), "{name}");
    assert!(name.ends_with("cache-invalidation.md"), "{name}");
    let note = vault.read_note(&path).expect("read");
    assert_eq!(note.frontmatter.captured.as_deref(), Some(TODAY));
    assert!(note.body.to_lowercase().contains("cache invalidation"));
}

#[test]
fn capture_survives_empty_text() {
    let (vault, _) = vault("empty-capture");
    let path = vault.capture("   ", None, None).expect("capture");
    assert!(vault.store().exists(&path));
    assert!(
        mneme::note::stem(&path).ends_with("capture"),
        "{path:?} falls back to a name of its own"
    );
}

// ---------------- filing, and the orphan rule ----------------

#[test]
fn file_refuses_an_orphan() {
    let (vault, _) = vault("orphan");
    let path = vault
        .capture("A thought with no home.", Some("Orphan"), None)
        .expect("capture");
    let before = vault.store().read(&path).expect("read");
    let refused = vault.file(
        &vault.permit(Holder::Curator),
        &path,
        Area::Research,
        &Filing::default(),
    );
    assert!(matches!(refused, Err(Fault::OrphanNote { .. })));
    assert!(vault.store().exists(&path), "an orphan stays in the Inbox");
    assert_eq!(
        vault.store().read(&path).expect("read"),
        before,
        "and untouched means untouched"
    );
}

#[test]
fn an_unknown_area_has_no_way_in() {
    // The Python checks the string and raises. Here the area is a type, so the refusal happens at the
    // boundary and there is nothing left to check downstream.
    assert!(Area::parse("elsewhere").is_none());
    assert!(Area::parse("Research").is_none(), "an area is lowercase");
    for area in Area::ALL {
        assert_eq!(Area::parse(area.key()), Some(area));
        assert!(!area.folder().is_empty());
    }
}

#[test]
fn file_moves_and_records_topics_and_projects() {
    let (vault, root) = vault("file");
    let path = vault
        .capture("Body of the finding.", Some("Agent memory"), None)
        .expect("capture");
    let filed = vault
        .file(
            &vault.permit(Holder::Curator),
            &path,
            Area::Research,
            &Filing {
                topics: vec!["agent-memory".to_string()],
                projects: vec!["hades-agent".to_string()],
                from_project: None,
                summary: Some("A one line summary.".to_string()),
                source_url: Some("https://example.com/a".to_string()),
                provenance: None,
            },
        )
        .expect("file");
    assert_eq!(filed.parent().expect("parent"), root.join("20 Research"));
    assert!(!path.exists(), "the Inbox copy is gone");
    assert!(filed.exists(), "the filed note is a file, not only a row");
    let note = vault.read_note(&filed).expect("read");
    assert_eq!(texts(note.topics()), ["agent-memory"]);
    assert_eq!(texts(note.projects()), ["hades-agent"]);
    assert_eq!(note.frontmatter.status.as_deref(), Some("durable"));
    assert!(note.is_durable());
}

#[test]
fn a_missing_note_is_named_rather_than_invented() {
    let (vault, _) = vault("missing");
    let refused = vault.file(
        &vault.permit(Holder::Curator),
        Path::new("00 Inbox/nothing-here.md"),
        Area::Research,
        &filing(&["vaults"], &[]),
    );
    assert!(matches!(refused, Err(Fault::MissingNote { .. })));
}

#[test]
fn durable_needs_summary_topic_and_provenance() {
    let base = Frontmatter {
        title: Some("x".to_string()),
        summary: Some("s".to_string()),
        topics: vec!["t".to_string()],
        source_url: Some("https://e.com".to_string()),
        ..Frontmatter::default()
    };
    let missing: [(&str, Frontmatter); 3] = [
        (
            "summary",
            Frontmatter {
                summary: None,
                ..base.clone()
            },
        ),
        (
            "topics",
            Frontmatter {
                topics: Vec::new(),
                ..base.clone()
            },
        ),
        (
            "source_url",
            Frontmatter {
                source_url: None,
                ..base.clone()
            },
        ),
    ];
    for (key, frontmatter) in missing {
        let note = Note::new("20 Research/x.md", frontmatter, "");
        assert!(!note.is_durable(), "without a {key}");
    }
    let whole = Note::new("20 Research/x.md", base.clone(), "");
    assert!(whole.is_durable(), "with all three");

    let by_provenance = Frontmatter {
        source_url: None,
        provenance: Some("a session on 2026-10-17".to_string()),
        ..base
    };
    assert!(Note::new("20 Research/x.md", by_provenance, "").is_durable());
}

// ---------------- recall, stale, and the index ----------------

#[test]
fn recall_by_topic_and_project() {
    let (vault, _) = vault("recall");
    let one = vault.capture("One.", Some("One"), None).expect("capture");
    let two = vault.capture("Two.", Some("Two"), None).expect("capture");
    vault
        .file(
            &vault.permit(Holder::Curator),
            &one,
            Area::Research,
            &filing(&["vaults"], &["hades-agent"]),
        )
        .expect("file one");
    vault
        .file(
            &vault.permit(Holder::Curator),
            &two,
            Area::Research,
            &filing(&["vaults"], &["other"]),
        )
        .expect("file two");

    let mut both = titles(&vault.recall(Some("vaults"), None).expect("recall"));
    both.sort();
    assert_eq!(both, ["One", "Two"]);
    assert_eq!(
        titles(&vault.recall(None, Some("hades-agent")).expect("recall")),
        ["One"]
    );
    assert_eq!(
        titles(&vault.recall(Some("vaults"), Some("other")).expect("recall")),
        ["Two"]
    );
    assert!(
        vault
            .recall(Some("nothing"), None)
            .expect("recall")
            .is_empty()
    );
}

#[test]
fn recall_without_a_filter_returns_nothing() {
    let (vault, _) = vault("recall-guard");
    let path = vault.capture("Body.", Some("One"), None).expect("capture");
    vault
        .file(
            &vault.permit(Holder::Curator),
            &path,
            Area::Research,
            &filing(&["vaults"], &[]),
        )
        .expect("file");
    assert!(
        vault.recall(None, None).expect("recall").is_empty(),
        "the vault never dumps itself"
    );
}

#[test]
fn index_lists_notes_under_their_topics() {
    let (vault, root) = vault("index");
    let path = vault
        .capture("Body.", Some("Finding"), None)
        .expect("capture");
    vault
        .file(
            &vault.permit(Holder::Curator),
            &path,
            Area::Research,
            &filing(&["agent-memory"], &["hades-agent"]),
        )
        .expect("file");

    let text = vault
        .build_index(Some("2026-10-16"), &[])
        .expect("build index");
    assert!(text.contains("### agent-memory (1)"), "{text}");
    assert!(text.contains("Finding") && text.contains("Short."));
    assert!(text.contains("## Projects"));
    assert!(text.contains("0 candidate(s)"));
    assert!(text.contains("20 Research/"));
    assert!(
        !text.contains(&root.to_string_lossy().to_string()),
        "index paths are vault relative:\n{text}"
    );
    let written = vault
        .write_index(Some("2026-10-16"), &[])
        .expect("write index");
    assert_eq!(std::fs::read_to_string(&written).expect("read"), text);
}

#[test]
fn index_on_disk_is_never_stale_after_a_write() {
    let (vault, root) = vault("fresh-index");
    let path = vault
        .capture("Body.", Some("Filed finding"), None)
        .expect("capture");
    vault
        .file(
            &vault.permit(Holder::Curator),
            &path,
            Area::Research,
            &filing(&["agent-memory"], &[]),
        )
        .expect("file");
    let on_disk = std::fs::read_to_string(root.join("INDEX.md")).expect("read");
    assert!(
        on_disk.contains("### agent-memory (1)"),
        "the index refreshes on write:\n{on_disk}"
    );

    let orphan = vault
        .capture("No home.", Some("Spark"), None)
        .expect("capture");
    let on_disk = std::fs::read_to_string(root.join("INDEX.md")).expect("read");
    assert!(on_disk.contains("1 candidate(s)"), "{on_disk}");
    assert!(vault.store().exists(&orphan));
}

#[test]
fn stale_reports_old_inbox_candidates() {
    let (vault, root) = vault("stale");
    let fresh = vault
        .capture("Fresh.", Some("Fresh"), None)
        .expect("capture");
    let inbox = root.join("00 Inbox");
    let old = inbox.join("2026-01-01-ancient.md");
    vault
        .store()
        .write(
            &old,
            "---\ntitle: Ancient\ncaptured: 2026-01-01\n---\n\nbody\n",
        )
        .expect("write");
    let undated = inbox.join("2026-01-02-undated.md");
    vault
        .store()
        .write(&undated, "---\ntitle: Undated\n---\n\nbody\n")
        .expect("write");
    let unreadable = inbox.join("whenever.md");
    vault
        .store()
        .write(
            &unreadable,
            "---\ntitle: Whenever\ncaptured: someday\n---\n\nbody\n",
        )
        .expect("write");

    let names: Vec<String> = vault
        .stale(mneme::INBOX_EXPIRY_DAYS)
        .expect("stale")
        .iter()
        .map(|note| {
            note.path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert!(
        names.contains(&"2026-01-01-ancient.md".to_string()),
        "{names:?}"
    );
    assert!(
        names.contains(&"2026-01-02-undated.md".to_string()),
        "the day comes off the file name when the note says nothing: {names:?}"
    );
    assert!(
        !names.contains(
            &unreadable
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        ),
        "a stamp no one can read is skipped, not guessed at"
    );
    assert!(!names.contains(&fresh.file_name().unwrap().to_string_lossy().into_owned()));
}

// ---------------- the frontmatter ----------------

#[test]
fn parse_frontmatter_without_a_header() {
    let (frontmatter, body) = Frontmatter::parse("just prose, no metadata\n");
    assert_eq!(frontmatter, Frontmatter::default());
    assert!(body.starts_with("just prose"));
}

#[test]
fn frontmatter_lists_and_scalars_round_trip() {
    let frontmatter = Frontmatter {
        title: Some("T".to_string()),
        topics: vec!["a".to_string(), "b".to_string()],
        summary: Some("S".to_string()),
        ..Frontmatter::default()
    };
    let text = frontmatter.render();
    assert_eq!(text, "---\ntitle: T\nsummary: S\ntopics: [a, b]\n---\n");
    let file = format!("{text}\nbody\n");
    let (parsed, body) = Frontmatter::parse(&file);
    assert_eq!(parsed.title.as_deref(), Some("T"));
    assert_eq!(texts(&parsed.topics), ["a", "b"]);
    assert_eq!(parsed.summary.as_deref(), Some("S"));
    assert_eq!(body.trim(), "body");
}

#[test]
fn a_topic_project_and_product_are_created_with_their_links() {
    let (vault, root) = vault("create-tree");
    let topic = vault
        .create_topic(
            "technical",
            "AI Inference",
            None,
            "How tokens are made.",
            "hand",
        )
        .expect("topic");
    assert_eq!(
        mneme::relative_posix(&topic, &root),
        "50 Topics/technical/ai-inference/index.md"
    );
    let subtopic = vault
        .create_topic(
            "technical",
            "KV Cache",
            Some("technical/ai-inference"),
            "The keys and values a decoder keeps.",
            "hand",
        )
        .expect("subtopic");
    assert_eq!(
        mneme::relative_posix(&subtopic, &root),
        "50 Topics/technical/ai-inference/kv-cache/index.md"
    );

    let research = vault
        .create_research(
            "technical/ai-inference/kv-cache",
            "KV cache at 32K",
            None,
            "24.2 GiB at q4_0.",
            "measured",
            None,
        )
        .expect("research");
    assert_eq!(
        mneme::relative_posix(&research, &root),
        "20 Research/technical/ai-inference/kv-cache/kv-cache-at-32k.md"
    );

    let project = vault
        .create_project(
            "Loom",
            "technical/ai-inference",
            &["kv-cache-at-32k".to_string()],
            "A serving stack.",
            "planning",
        )
        .expect("project");
    assert_eq!(
        mneme::relative_posix(&project, &root),
        "30 Projects/loom/index.md"
    );
    let note = vault.read_note(&project).expect("project note");
    assert_eq!(
        note.research(),
        &["technical/ai-inference/kv-cache/kv-cache-at-32k".to_string()]
    );

    let product = vault
        .create_product(
            "Loom 1.0",
            "loom",
            None,
            "frozen",
            "The closed release.",
            "release",
        )
        .expect("product");
    assert_eq!(
        mneme::relative_posix(&product, &root),
        "40 Products/loom-1-0/index.md"
    );
    let note = vault.read_note(&product).expect("product note");
    assert_eq!(note.from_project(), "loom");
    assert_eq!(note.topic(), "technical/ai-inference");
    assert_eq!(note.status(), "frozen");
}

#[test]
fn a_project_refuses_a_research_reference_that_does_not_exist() {
    let (vault, _) = vault("dangling-research");
    vault
        .create_topic("technical", "AI Inference", None, "How tokens are made.", "hand")
        .expect("topic");
    let error = vault
        .create_project(
            "Loom",
            "technical/ai-inference",
            &["missing-note".to_string()],
            "A serving stack.",
            "planning",
        )
        .unwrap_err();
    assert!(matches!(error, Fault::InvalidNote { .. }), "{error:?}");
    assert!(
        error.to_string().contains("does not name an existing"),
        "{error}"
    );

    let error = vault
        .create_product(
            "Loom 1.0",
            "missing-project",
            None,
            "frozen",
            "A release.",
            "release",
        )
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("does not name an existing project"),
        "{error}"
    );
}

#[test]
fn write_note_refuses_a_project_that_names_missing_research() {
    let (vault, root) = vault("dangling-write");
    let path = root.join("30 Projects/manual/index.md");
    let frontmatter = Frontmatter {
        title: Some("Manual".to_string()),
        summary: Some("A manual project.".to_string()),
        status: Some("durable".to_string()),
        topic: Some("technical/ai-inference".to_string()),
        topics: vec!["technical/ai-inference".to_string()],
        research: vec!["missing-note".to_string()],
        provenance: Some("hand".to_string()),
        ..Frontmatter::default()
    };
    let error = vault
        .write_note(&path, &frontmatter, "")
        .expect_err("the reference is checked");
    assert!(matches!(error, Fault::InvalidNote { .. }), "{error:?}");
    assert!(!vault.store().exists(&path));
}

#[test]
fn write_note_refuses_a_product_that_names_a_missing_project() {
    let (vault, root) = vault("dangling-product-write");
    let path = root.join("40 Products/thing/index.md");
    let frontmatter = Frontmatter {
        title: Some("Thing".to_string()),
        summary: Some("A release.".to_string()),
        status: Some("frozen".to_string()),
        topic: Some("technical/ai-inference".to_string()),
        topics: vec!["technical/ai-inference".to_string()],
        from_project: Some("missing".to_string()),
        provenance: Some("hand".to_string()),
        ..Frontmatter::default()
    };
    let error = vault
        .write_note(&path, &frontmatter, "")
        .expect_err("the project is checked");
    assert!(matches!(error, Fault::InvalidNote { .. }), "{error:?}");
    assert!(error.to_string().contains("does not name an existing project"));
    assert!(!vault.store().exists(&path));
}

#[test]
fn an_outbox_note_is_not_recalled() {
    let (vault, root) = vault("outbox-inert");
    let path = root.join("10 Outbox/hello.md");
    let frontmatter = Frontmatter {
        title: Some("Hello".to_string()),
        summary: Some("A message for another agent.".to_string()),
        status: Some("durable".to_string()),
        topics: vec!["agent-memory".to_string()],
        provenance: Some("hand".to_string()),
        ..Frontmatter::default()
    };
    vault.write_note(&path, &frontmatter, "body").expect("outbox note");
    assert!(vault.recall(Some("agent-memory"), None).expect("recall").is_empty());
}

#[test]
fn a_key_the_vault_does_not_name_survives_a_rewrite() {
    let (vault, _) = vault("extras");
    let path = vault
        .capture("Body.", Some("Hand written"), None)
        .expect("capture");
    let text = vault.store().read(&path).expect("read");
    vault
        .store()
        .write(
            &path,
            &text.replace("---\n", "---\ntags: [a, b]\nkept: yes\n"),
        )
        .expect("write");

    let filed = vault
        .file(
            &vault.permit(Holder::Curator),
            &path,
            Area::Research,
            &filing(&["vaults"], &[]),
        )
        .expect("file");
    let note = vault.read_note(&filed).expect("read");
    assert_eq!(
        note.frontmatter.extra.get("tags"),
        Some(&mneme::Field::List(vec!["a".to_string(), "b".to_string()]))
    );
    assert_eq!(
        note.frontmatter.extra.get("kept"),
        Some(&mneme::Field::Scalar("yes".to_string()))
    );
    let written = vault.store().read(&filed).expect("read");
    assert!(written.contains("tags: [a, b]") && written.contains("kept: yes"));
}

// ---------------- the fence ----------------

#[test]
fn fenced_matches_by_path_parts_not_by_substring() {
    let fence = |paths: &[&str]| {
        paths
            .iter()
            .map(|path| path.to_string())
            .collect::<Vec<String>>()
    };
    assert!(mneme::fenced(
        Path::new("2-Areas/Family/notes.md"),
        &fence(&["2-Areas/Family"])
    ));
    assert!(mneme::fenced(
        Path::new("2-Areas/Family/notes.md"),
        &fence(&["2-Areas", "Family"])
    ));
    assert!(!mneme::fenced(
        Path::new("2-Areas/Finance/notes.md"),
        &fence(&["2-Areas/Family"])
    ));
    assert!(
        !mneme::fenced(Path::new("2-Areas/Family/notes.md"), &[]),
        "no fences means nothing is fenced"
    );
    assert!(
        !mneme::fenced(
            Path::new("2-Areas/Family-history/notes.md"),
            &fence(&["2-Areas/Family"])
        ),
        "parts, not substrings"
    );
}

#[test]
fn a_fenced_note_is_never_walked_or_indexed() {
    let (vault, root) = vault("fence");
    let public = vault
        .file(
            &vault.permit(Holder::Curator),
            &vault
                .capture("Public body.", Some("Public"), None)
                .expect("capture"),
            Area::Research,
            &filing(&["vaults"], &[]),
        )
        .expect("file");
    let hidden = root.join("20 Research").join("2026-01-01-hidden.md");
    vault
        .store()
        .write(
            &hidden,
            "---\ntitle: Hidden\nsummary: secret\ntopic: vaults\ntopics: [vaults]\nsource_url: u\n---\n\nbody\n",
        )
        .expect("write");

    let fences = vec!["20 Research/2026-01-01-hidden.md".to_string()];
    let walked: Vec<String> = titles(&vault.iter_notes(None, &fences).expect("walk"))
        .into_iter()
        .filter(|title| !title.ends_with("start here"))
        .collect();
    assert_eq!(walked, ["Public"]);
    assert!(
        vault.store().exists(&public) && vault.store().exists(&hidden),
        "a fence skips a read, it never deletes"
    );
    let index = vault
        .build_index(Some("2026-10-16"), &fences)
        .expect("index");
    assert!(
        !index.contains("Hidden") && index.contains("Public"),
        "{index}"
    );
}

// ---------------- the permit ----------------

#[test]
fn a_move_needs_a_permit_and_the_permit_names_its_holder() {
    let (vault, _) = vault("permit");
    // Python cannot express this rule in its types, so it scans its own source for the write calls
    // instead. Here the only constructor of a permit is the vault's, it takes the holder, and the holder
    // is read back by the layer that writes the audit trail.
    let permit = vault.permit(Holder::Curator);
    assert_eq!(permit.holder(), Holder::Curator);
    assert_eq!(permit.holder().name(), "curator");

    let path = vault.capture("Body.", Some("Body"), None).expect("capture");
    let filed = vault
        .file(&permit, &path, Area::Research, &filing(&["vaults"], &[]))
        .expect("a permitted move");
    assert_eq!(
        filed.parent().expect("parent"),
        vault.area_path(Area::Research)
    );
}

// ---------------- end to end, with the real clock and the real files ----------------

/// The one thing the oracle cannot pin: the real clock and the real disk. Driven by hand, the index is
/// re-read after every step rather than trusted, because a state written once and never refreshed is
/// invisible to tests that build their own fixtures.
#[test]
fn end_to_end_on_disk_with_the_real_clock() {
    let root = scratch("end-to-end");
    let vault = Vault::new(root.clone(), FileVault::new(), mneme::SystemClock);
    assert_eq!(vault.clock().today().to_iso().len(), 10, "an ISO day");

    vault
        .init(Some(("Jac", "Ship Hades.", &[])))
        .expect("init with answers");
    assert_eq!(vault.read_charter().expect("read").name, "Jac");

    let captured = vault
        .capture("A finding worth keeping.", Some("End to end"), None)
        .expect("capture");
    let index = std::fs::read_to_string(root.join("INDEX.md")).expect("read");
    assert!(index.contains("1 candidate(s)"), "{index}");

    let filed = vault
        .file(
            &vault.permit(Holder::Curator),
            &captured,
            Area::Research,
            &filing(&["hades-agent"], &["hades-agent"]),
        )
        .expect("file");
    let on_disk = std::fs::read_to_string(root.join("INDEX.md")).expect("read");
    assert_eq!(
        on_disk,
        vault.build_index(None, &[]).expect("build"),
        "the index on disk is the index it builds"
    );
    assert!(!captured.exists() && filed.exists(), "moved, not copied");

    vault.write_index(None, &[]).expect("write again");
    assert_eq!(
        std::fs::read_to_string(root.join("INDEX.md")).expect("read"),
        on_disk,
        "writing it a second time refreshes rather than appends"
    );

    let found = vault.recall(Some("hades-agent"), None).expect("recall");
    assert_eq!(found.len(), 1);
    assert!(found[0].is_durable());
    assert!(
        vault
            .stale(mneme::INBOX_EXPIRY_DAYS)
            .expect("stale")
            .is_empty(),
        "the Inbox is empty again"
    );
}

fn scratch_base() -> std::path::PathBuf {
    // Off the source tree's disk: a full root filesystem must not be filled by a test run, and the build
    // directory is the natural place when Cargo names one. Without one, a directory under the Hades home
    // is used rather than the machine's temp directory, which on the test host lives under the live install and
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
