use std::fs;
use std::path::{Path, PathBuf};

use harness::cases;
use harness::pack::{self, ExportOptions, Kind};
use harness::{pack_fit, skills};

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

fn fit_options(from: &Path, out: &Path) -> ExportOptions {
    let mut options = options(from, out);
    options.cases.clear();
    options
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
        cases: vec!["research-f3e86460f834".to_string()],
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
        "research-f3e86460f834",
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
        "research-f3e86460f834",
        "Find the value",
        "test -f run.sh",
    );
    let archive = from.join("pack.hadepack");
    pack::export(&options(&from, &archive)).expect("export");
    let opened = pack::Pack::open(&archive).expect("open");
    let tree = opened.root().join("files/cases/research-f3e86460f834/tree");
    let digest = cases::tree_digest(&tree).expect("tree digest");
    let case = cases::load(&from, "research")
        .into_iter()
        .find(|case| case.id == "research-f3e86460f834")
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

#[test]
fn a_pack_applies_a_skill_and_harness_and_uninstall_removes_both() {
    let source = state("pack-fit-source");
    write_skill(&source, "pack-skill", "A procedure with a support file.");
    write_harness(&source, "research", "You are a research harness.");
    let archive = source.join("fit.hadepack");
    pack::export(&fit_options(&source, &archive)).expect("export");
    let opened = pack::Pack::open(&archive).expect("open");

    let target = state("pack-fit-target");
    let decisions = pack_fit::parse_decisions(
        r#"{"items":[
            {"kind":"skill","identity":"pack-skill","action":"install"},
            {"kind":"harness","identity":"research","action":"install"}
        ]}"#,
    )
    .expect("decisions");
    let receipt = pack_fit::apply(&opened, &target, &target, &decisions, false).expect("apply");
    assert_eq!(receipt.items.len(), 2);
    assert!(target.join("skills/proposed/pack-skill/SKILL.md").is_file());
    assert!(target.join("harness/research/system.md").is_file());
    assert_eq!(pack_fit::read_catalog(&target).expect("catalog").len(), 2);
    assert!(target.join("packs/test/pack/1.0.0").is_dir());

    let receipt = pack_fit::uninstall(&target, &target, "test/pack", None).expect("uninstall");
    assert_eq!(receipt.removed.len(), 2, "{}", receipt.text());
    assert!(!target.join("skills/proposed/pack-skill").exists());
    assert!(!target.join("harness/research").exists());
    assert!(pack_fit::read_catalog(&target).expect("catalog").is_empty());
    assert!(!target.join("packs/test/pack/1.0.0").exists());
}

#[test]
fn activating_a_skill_installs_it_directly_and_uninstall_removes_it() {
    let source = state("pack-activate-source");
    write_skill(&source, "pack-skill", "An active procedure.");
    write_harness(&source, "research", "A harness.");
    let archive = source.join("active.hadepack");
    pack::export(&fit_options(&source, &archive)).expect("export");
    let opened = pack::Pack::open(&archive).expect("open");
    let target = state("pack-activate-target");
    let decisions = pack_fit::parse_decisions(
        r#"{"items":[{"kind":"skill","identity":"pack-skill","action":"install"}]}"#,
    )
    .expect("decisions");
    pack_fit::apply(&opened, &target, &target, &decisions, true).expect("apply");
    let text = fs::read_to_string(target.join("skills/pack-skill/SKILL.md")).expect("skill");
    assert!(text.contains("status: active"), "{text}");
    pack_fit::uninstall(&target, &target, "test/pack", None).expect("uninstall");
    assert!(!target.join("skills/pack-skill").exists());
}

#[test]
fn a_second_apply_refuses_before_installing_anything() {
    let source = state("pack-twice-source");
    write_skill(&source, "pack-skill", "A procedure.");
    write_harness(&source, "research", "A harness.");
    let archive = source.join("twice.hadepack");
    pack::export(&fit_options(&source, &archive)).expect("export");
    let opened = pack::Pack::open(&archive).expect("open");
    let target = state("pack-twice-target");
    let decisions = pack_fit::parse_decisions(
        r#"{"items":[{"kind":"skill","identity":"pack-skill","action":"install"}]}"#,
    )
    .expect("decisions");
    pack_fit::apply(&opened, &target, &target, &decisions, false).expect("first apply");
    let error = pack_fit::apply(&opened, &target, &target, &decisions, false)
        .expect_err("a second apply must refuse");
    assert!(error.contains("already exists"), "{error}");
}

#[test]
fn an_adapted_skill_is_accepted_only_as_a_proposal() {
    let source = state("pack-adapt-source");
    write_skill(&source, "pack-skill", "Use the old command.");
    write_harness(&source, "research", "A harness.");
    let archive = source.join("adapt.hadepack");
    let mut options = fit_options(&source, &archive);
    options.derivatives = "allowed".to_string();
    pack::export(&options).expect("export");
    let opened = pack::Pack::open(&archive).expect("open");
    let target = state("pack-adapt-target");
    let adapted = pack_fit::Adapted {
        text: "Use the new command.".to_string(),
        guidance: None,
        changes: vec!["new command".to_string()],
        conflicts: Vec::new(),
    };
    let path = pack_fit::accept_adapted(
        &opened,
        &target,
        &target,
        Kind::Skill,
        "pack-skill",
        &adapted,
    )
    .expect("accept");
    assert!(path.contains("skills/proposed/pack-skill"));
    let text =
        fs::read_to_string(target.join("skills/proposed/pack-skill/SKILL.md")).expect("proposal");
    assert!(text.contains("status: proposed"), "{text}");
    assert!(text.contains("contract: TODO"), "{text}");
    assert!(skills::installed(&target).is_empty(), "nothing active yet");
}

#[test]
fn a_pack_case_is_installed_with_a_source_and_retired_on_uninstall() {
    let source = state("pack-case-source");
    write_skill(&source, "pack-skill", "A procedure.");
    write_harness(&source, "research", "A harness.");
    write_case(
        &source,
        "research",
        "research-f3e86460f834",
        "Find the value",
        "test -f run.sh",
    );
    let archive = source.join("case.hadepack");
    let mut options = fit_options(&source, &archive);
    options.cases = vec!["research-f3e86460f834".to_string()];
    pack::export(&options).expect("export");
    let opened = pack::Pack::open(&archive).expect("open");
    let target = state("pack-case-target");
    let decisions = pack_fit::parse_decisions(
        r#"{"items":[{"kind":"case","identity":"research-f3e86460f834","action":"install"}]}"#,
    )
    .expect("decisions");
    pack_fit::apply(&opened, &target, &target, &decisions, false).expect("apply");
    let case = cases::find(&target, "research", "research-f3e86460f834")
        .expect("find")
        .expect("a case");
    assert_eq!(case.source, "pack:test/pack@1.0.0");
    assert!(
        target
            .join("cases/research/research-f3e86460f834/tree/run.sh")
            .is_file()
    );

    pack_fit::uninstall(&target, &target, "test/pack", None).expect("uninstall");
    assert!(!target.join("cases/research/research-f3e86460f834").exists());
    let retired = cases::load(&target, "research")
        .into_iter()
        .find(|case| case.id == "research-f3e86460f834")
        .expect("the retired ledger line");
    assert_eq!(retired.status, "retired");
}

#[test]
fn replacing_a_conflicting_skill_backs_up_the_old_one() {
    let first = state("pack-replace-first");
    write_skill(&first, "pack-skill", "The old procedure.");
    write_harness(&first, "research", "A harness.");
    let one = first.join("one.hadepack");
    pack::export(&fit_options(&first, &one)).expect("first export");
    let second = state("pack-replace-second");
    write_skill(&second, "pack-skill", "The replacement procedure.");
    write_harness(&second, "research", "A harness.");
    let two = second.join("two.hadepack");
    let mut options = fit_options(&second, &two);
    options.id = "test/other".to_string();
    options.version = "1.0.1".to_string();
    pack::export(&options).expect("second export");

    let target = state("pack-replace-target");
    let first_pack = pack::Pack::open(&one).expect("open first");
    let install = pack_fit::parse_decisions(
        r#"{"items":[{"kind":"skill","identity":"pack-skill","action":"install"}]}"#,
    )
    .expect("decisions");
    pack_fit::apply(&first_pack, &target, &target, &install, true).expect("first apply");
    let second_pack = pack::Pack::open(&two).expect("open second");
    let replace = pack_fit::parse_decisions(
        r#"{"items":[{"kind":"skill","identity":"pack-skill","action":"replace"}]}"#,
    )
    .expect("decisions");
    pack_fit::apply(&second_pack, &target, &target, &replace, false).expect("replace");
    let text = fs::read_to_string(target.join("skills/pack-skill/SKILL.md")).expect("active");
    assert!(text.contains("The replacement procedure."), "{text}");
    let catalog = pack_fit::read_catalog(&target).expect("catalog");
    assert_eq!(catalog.len(), 1);
    assert_eq!(catalog[0].pack, "test/other");
    assert!(target.join("packs/replaced/test/other").is_dir());
}

#[test]
fn uninstall_keeps_a_skill_a_person_edited_after_install() {
    let source = state("pack-edited-source");
    write_skill(&source, "pack-skill", "Original text.");
    write_harness(&source, "research", "A harness.");
    let archive = source.join("edited.hadepack");
    pack::export(&fit_options(&source, &archive)).expect("export");
    let target = state("pack-edited-target");
    let opened = pack::Pack::open(&archive).expect("open");
    let decisions = pack_fit::parse_decisions(
        r#"{"items":[{"kind":"skill","identity":"pack-skill","action":"install"}]}"#,
    )
    .expect("decisions");
    pack_fit::apply(&opened, &target, &target, &decisions, false).expect("apply");
    let path = target.join("skills/proposed/pack-skill/SKILL.md");
    let mut text = fs::read_to_string(&path).expect("proposal");
    text.push_str("\nA local edit.\n");
    fs::write(&path, text).expect("edit");
    let receipt = pack_fit::uninstall(&target, &target, "test/pack", None).expect("uninstall");
    assert_eq!(receipt.kept, vec!["skills/proposed/pack-skill"]);
    assert!(path.is_file(), "an edited live copy is not deleted");
    assert!(pack_fit::read_catalog(&target).expect("catalog").is_empty());
}
