use std::fs;
use std::path::{Path, PathBuf};

use harness::cases;
use harness::pack::{self, ExportOptions, Kind};

fn state(name: &str) -> PathBuf {
    let root = harness::scratch::unique(name);
    fs::create_dir_all(root.join("skills")).expect("skills");
    fs::create_dir_all(root.join("harness")).expect("harness");
    root
}

fn write_skill(root: &Path, name: &str, body: &str) {
    let dir = root.join("skills").join(name);
    fs::create_dir_all(&dir).expect("skill dir");
    fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ncontract: test\n---\n\n{body}\n"),
    )
    .expect("skill");
}

fn write_harness(root: &Path, profile: &str, body: &str) {
    let dir = root.join("harness").join(profile);
    fs::create_dir_all(&dir).expect("harness dir");
    fs::write(dir.join("system.md"), body).expect("system");
}

fn write_case(root: &Path, profile: &str, id: &str, goal: &str, check: &str) -> String {
    let tree = root.join("cases").join(profile).join(id).join("tree");
    fs::create_dir_all(&tree).expect("tree");
    fs::write(tree.join("run.sh"), "#!/bin/sh\nexit 0\n").expect("script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(tree.join("run.sh"), fs::Permissions::from_mode(0o775)).expect("mode");
    }
    let digest = cases::tree_digest(&tree).expect("tree digest");
    let value = serde_json::json!({
        "kind": "case",
        "id": id,
        "at": 1,
        "profile": profile,
        "goal": goal,
        "check": check,
        "model": "test",
        "policy": "guarded",
        "bounds": {"attempts": 1, "minutes": 1, "budget_tokens": 100, "width": 1},
        "attempts_used": 1,
        "tree": digest,
        "status": "open"
    });
    let ledger = cases::ledger(root, profile);
    fs::create_dir_all(ledger.parent().expect("ledger parent")).expect("cases dir");
    fs::write(ledger, format!("{value}\n")).expect("ledger");
    digest
}

fn options(from: &Path, out: &Path) -> ExportOptions {
    ExportOptions {
        id: "test/pack".to_string(),
        version: "1.0.0".to_string(),
        from: from.to_path_buf(),
        title: "Test pack".to_string(),
        summary: "A fixture".to_string(),
        license: "MIT".to_string(),
        derivatives: "forbidden".to_string(),
        network: true,
        writes: vec!["vault".to_string()],
        requires_roles: vec!["survey".to_string()],
        requires_tools: vec!["fetch".to_string()],
        skills: vec!["pack-skill".to_string()],
        profiles: vec!["research".to_string()],
        cases: vec!["research-abc123def456".to_string()],
        out: Some(out.to_path_buf()),
    }
}

#[test]
fn export_is_deterministic_and_inspect_lists_every_item() {
    let from = state("pack-export");
    write_skill(&from, "pack-skill", "Do the work in the stated order.");
    write_harness(&from, "research", "You are a research harness.");
    write_case(
        &from,
        "research",
        "research-abc123def456",
        "Find the value",
        "test -f run.sh",
    );
    let first = from.join("first.hadepack");
    let second = from.join("second.hadepack");
    pack::export(&options(&from, &first)).expect("first export");
    pack::export(&options(&from, &second)).expect("second export");
    assert_eq!(
        fs::read(&first).expect("first"),
        fs::read(&second).expect("second")
    );

    let opened = pack::Pack::open(&first).expect("open");
    let text = opened.inspect();
    assert!(text.contains("test/pack 1.0.0"), "{text}");
    assert!(text.contains("items: 1 skill, 1 harness, 1 case"), "{text}");
    assert!(text.contains("unsigned"), "{text}");
    assert!(text.contains("writes the vault"), "{text}");
    assert!(text.contains("asks for the network"), "{text}");
    assert_eq!(opened.manifest.provides.all().len(), 3);
}

#[test]
fn plan_classifies_a_reexport_as_duplicate_and_a_changed_skill_as_conflict() {
    let from = state("pack-plan");
    write_skill(
        &from,
        "pack-skill",
        "The measured value is four hundred ten tokens per second.",
    );
    write_harness(&from, "research", "You are a research harness.");
    let archive = from.join("pack.hadepack");
    let mut opts = options(&from, &archive);
    opts.cases.clear();
    pack::export(&opts).expect("export");
    let opened = pack::Pack::open(&archive).expect("open");
    let rows = pack::plan(&opened, &from).expect("plan");
    assert!(
        rows.iter().all(|row| row.class == "duplicate"),
        "{:?}",
        rows
    );

    write_skill(
        &from,
        "pack-skill",
        "The measured value is a different result entirely.",
    );
    let rows = pack::plan(&opened, &from).expect("plan again");
    let skill = rows
        .iter()
        .find(|row| row.kind == Kind::Skill)
        .expect("skill row");
    assert_eq!(skill.class, "conflict", "{skill:?}");
    assert_eq!(skill.default_action, "ask");
}

#[test]
fn plan_marks_a_near_duplicate_with_another_name_as_redundant() {
    let pack_state = state("pack-redundant-source");
    write_skill(
        &pack_state,
        "pack-skill",
        "The measured value is four hundred ten tokens per second on this driver in a fixed test run with the same batch. ",
    );
    write_harness(&pack_state, "research", "You are a research harness.");
    let archive = pack_state.join("pack.hadepack");
    let mut opts = options(&pack_state, &archive);
    opts.cases.clear();
    pack::export(&opts).expect("export");

    let live = state("pack-redundant-live");
    write_skill(
        &live,
        "other-skill",
        "The measured value is four hundred ten tokens per second on this driver in a fixed test run with the same batch. ",
    );
    let opened = pack::Pack::open(&archive).expect("open");
    let rows = pack::plan(&opened, &live).expect("plan");
    let row = rows
        .iter()
        .find(|row| row.kind == Kind::Skill)
        .expect("skill row");
    assert_eq!(row.class, "redundant", "{row:?}");
    assert!(row.score >= 0.60, "{row:?}");
    assert!(!row.shared.is_empty(), "{row:?}");
}

#[test]
fn plan_marks_an_unrelated_pack_item_new() {
    let pack_state = state("pack-new-source");
    write_skill(
        &pack_state,
        "pack-skill",
        "Use the telescope and record the coordinates.",
    );
    let archive = pack_state.join("pack.hadepack");
    let mut opts = options(&pack_state, &archive);
    opts.profiles.clear();
    opts.cases.clear();
    pack::export(&opts).expect("export");

    let live = state("pack-new-live");
    write_skill(
        &live,
        "other-skill",
        "Knead the bread and let it rise for an hour.",
    );
    let opened = pack::Pack::open(&archive).expect("open");
    let rows = pack::plan(&opened, &live).expect("plan");
    let row = rows
        .iter()
        .find(|row| row.kind == Kind::Skill)
        .expect("skill row");
    assert_eq!(row.class, "new", "{row:?}");
}

#[test]
fn a_pack_case_tree_keeps_its_executable_mode() {
    let from = state("pack-modes");
    write_skill(&from, "pack-skill", "A skill.");
    write_harness(&from, "research", "A harness.");
    write_case(
        &from,
        "research",
        "research-abc123def456",
        "Find the value",
        "test -f run.sh",
    );
    let archive = from.join("pack.hadepack");
    pack::export(&options(&from, &archive)).expect("export");
    let opened = pack::Pack::open(&archive).expect("open");
    let tree = opened.root().join("files/cases/research-abc123def456/tree");
    let digest = cases::tree_digest(&tree).expect("tree digest");
    let case = cases::load(&from, "research")
        .into_iter()
        .find(|case| case.id == "research-abc123def456")
        .expect("case");
    assert_eq!(digest, case.tree);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(tree.join("run.sh"))
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o775);
    }
}
