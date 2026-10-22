//! Uninstall, at its two failure boundaries.
//!
//! Both need a failure staged on purpose, and both need the process environment, so this is an integration
//! test binary of its own: one failing catalog write here must not disturb an install running elsewhere.

use std::path::{Path, PathBuf};

use harness::pack::ExportOptions;
use harness::packs_install;

fn pack(work: &Path, name: &str) -> PathBuf {
    let source = work.join(format!("src-{name}"));
    std::fs::create_dir_all(source.join("skills").join(name)).expect("a source");
    std::fs::write(
        source.join("skills").join(name).join("SKILL.md"),
        format!(
            "---\\nname: {name}\\ndescription: Use when {name} is the one.\\n---\\n\\n# {name}\\n"
        ),
    )
    .expect("a skill");
    let out = work.join(format!("{name}.hadepack"));
    let options = ExportOptions {
        id: name.to_string(),
        version: "1.0.0".to_string(),
        from: source,
        title: name.to_string(),
        summary: format!("The {name} pack."),
        license: "MIT".to_string(),
        derivatives: "allowed".to_string(),
        network: false,
        writes: Vec::new(),
        requires_roles: Vec::new(),
        requires_tools: Vec::new(),
        skills: vec![name.to_string()],
        profiles: Vec::new(),
        cases: Vec::new(),
        keywords: Vec::new(),
        source: None,
        out: Some(out.clone()),
    };
    harness::pack::export(&options).expect("an archive");
    out
}

/// A catalog that cannot be committed must leave the files alone: a row naming a directory that is gone is a
/// worse state than a directory nobody has removed yet.
#[test]
fn a_failed_catalog_commit_removes_nothing() {
    let work = std::env::temp_dir().join(format!("hades-uninstall-catalog-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).expect("a work dir");
    let archive = pack(&work, "steady");
    let home = work.join("home");
    let state = work.join("state");
    std::fs::create_dir_all(&home).expect("a home");
    std::fs::create_dir_all(&state).expect("a state");
    packs_install::add_file(&archive, None, &home, &state, true).expect("an install");
    assert!(state.join("skills/steady/SKILL.md").is_file());

    // SAFETY: this binary sets this before any uninstall and runs the cases one at a time.
    unsafe { std::env::set_var("HADES_TEST_FAIL", "catalog-write") };
    let refused = harness::pack_fit::uninstall(&home, &state, "steady", None)
        .expect_err("a catalog that cannot be written");
    // SAFETY: as above.
    unsafe { std::env::remove_var("HADES_TEST_FAIL") };

    assert!(refused.contains("catalog"), "{refused}");
    assert!(
        state.join("skills/steady/SKILL.md").is_file(),
        "the skill was removed before the catalog commit failed"
    );
    let rows = harness::pack_fit::read_catalog(&state).expect("a catalog");
    assert_eq!(rows.len(), 1, "the catalog lost the row anyway");
    let _ = std::fs::remove_dir_all(&work);
}

/// An uninstall whose event cannot be written is committed, and says so, rather than reporting a failure for
/// something that is already done.
#[test]
fn an_uninstall_whose_event_failed_is_reported_as_committed() {
    let work = std::env::temp_dir().join(format!("hades-uninstall-event-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).expect("a work dir");
    let archive = pack(&work, "gone");
    let home = work.join("home");
    let state = work.join("state");
    std::fs::create_dir_all(&home).expect("a home");
    std::fs::create_dir_all(&state).expect("a state");
    packs_install::add_file(&archive, None, &home, &state, true).expect("an install");

    // A directory where the event file belongs: every append fails, and nothing else does.
    std::fs::remove_file(state.join("packs/events.jsonl")).expect("the event file");
    std::fs::create_dir_all(state.join("packs/events.jsonl")).expect("the obstruction");

    let receipt =
        harness::pack_fit::uninstall(&home, &state, "gone", None).expect("a committed uninstall");
    assert!(receipt.event_error.is_some(), "the failure went unreported");
    assert!(
        receipt.text().contains("the uninstall is committed"),
        "{}",
        receipt.text()
    );
    assert!(
        !state.join("skills/gone").exists(),
        "the skill was not removed"
    );
    assert!(
        harness::pack_fit::read_catalog(&state)
            .expect("a catalog")
            .is_empty()
    );
    let _ = std::fs::remove_dir_all(&work);
}
