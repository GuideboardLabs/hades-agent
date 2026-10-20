//! The ruler's contract: the check and the declared files are frozen before the first write, and a
//! change after the check ran is a refused result rather than a pass.

use harness::profile::Protect;
use harness::ruler::Ruler;
use std::fs;
use std::path::{Path, PathBuf};

fn scratch(name: &str) -> PathBuf {
    let unique = format!(
        "hades-ruler-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or(0)
    );
    let path = std::env::temp_dir().join(unique);
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).expect("a scratch tree");
    path
}

fn write(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("the parent directory");
    }
    fs::write(path, text).expect("the file");
}

fn protected() -> Vec<Protect> {
    vec![
        Protect::dir("tests").expect("a directory rule"),
        Protect::suffix("_test.rs").expect("a suffix rule"),
    ]
}

#[test]
fn the_check_and_every_protected_file_are_hashed() {
    let root = scratch("same");
    write(&root, "tests/check.sh", "exit 0\n");
    write(&root, "tests/gate_test.rs", "#[test]\nfn a() {}\n");
    write(&root, "src/main.rs", "fn main() {}\n");

    let first = Ruler::take("cargo test", &root, &protected()).expect("a ruler");
    let second = Ruler::take("cargo test", &root, &protected()).expect("a ruler");
    assert_eq!(
        first.check_sha256, second.check_sha256,
        "the same check has the same digest"
    );
    assert_eq!(
        first.files, second.files,
        "the same tree has the same files"
    );
    assert_eq!(first.files.len(), 2, "{:?}", first.files);
    assert!(
        first.files.contains_key("tests/check.sh"),
        "{:?}",
        first.files
    );
}

#[test]
fn editing_a_protected_file_is_a_named_change() {
    let root = scratch("edit");
    write(&root, "tests/check.sh", "exit 0\n");
    let before = Ruler::take("cargo test", &root, &protected()).expect("a ruler");
    write(&root, "tests/check.sh", "exit 0 # weakened\n");
    let after = Ruler::take("cargo test", &root, &protected()).expect("a ruler");
    let detail = before.diff(&after).expect("a change");
    assert!(detail.contains("tests/check.sh"), "{detail}");
}

#[test]
fn a_new_or_removed_file_under_a_protected_directory_is_a_change() {
    let root = scratch("add-remove");
    write(&root, "tests/check.sh", "exit 0\n");
    write(&root, "src/lib.rs", "\n");
    let before = Ruler::take("cargo test", &root, &protected()).expect("a ruler");

    write(&root, "tests/extra.rs", "\n");
    let added = Ruler::take("cargo test", &root, &protected()).expect("a ruler");
    let detail = before.diff(&added).expect("a change");
    assert!(detail.contains("tests/extra.rs"), "{detail}");

    fs::remove_file(root.join("tests/extra.rs")).expect("the file");
    fs::remove_file(root.join("tests/check.sh")).expect("the file");
    let removed = Ruler::take("cargo test", &root, &protected()).expect("a ruler");
    let detail = before.diff(&removed).expect("a change");
    assert!(detail.contains("tests/check.sh"), "{detail}");
}

#[test]
fn an_edit_outside_the_protected_set_is_not_a_change() {
    let root = scratch("outside");
    write(&root, "tests/check.sh", "exit 0\n");
    write(&root, "src/lib.rs", "fn a() {}\n");
    let before = Ruler::take("cargo test", &root, &protected()).expect("a ruler");
    write(&root, "src/lib.rs", "fn b() {}\n");
    let after = Ruler::take("cargo test", &root, &protected()).expect("a ruler");
    assert_eq!(before.diff(&after), None);
}

#[test]
fn the_check_text_is_part_of_the_ruler() {
    let root = scratch("check-text");
    write(&root, "tests/check.sh", "exit 0\n");
    let before = Ruler::take("cargo test", &root, &protected()).expect("a ruler");
    let after = Ruler::take("cargo test -- --skip weak", &root, &protected()).expect("a ruler");
    let detail = before.diff(&after).expect("a change");
    assert!(detail.contains("check"), "{detail}");
}

#[test]
fn a_suffix_rule_matches_by_file_name_only() {
    let root = scratch("suffix");
    write(&root, "tests/gate_test.rs", "\n");
    write(&root, "tests/notes.md", "\n");
    let ruler = Ruler::take(
        "cargo test",
        &root,
        &[Protect::suffix("_test.rs").expect("a suffix rule")],
    )
    .expect("a ruler");
    assert!(
        ruler.files.contains_key("tests/gate_test.rs"),
        "{:?}",
        ruler.files
    );
    assert!(
        !ruler.files.contains_key("tests/notes.md"),
        "{:?}",
        ruler.files
    );
}

#[test]
fn a_rule_that_escapes_the_root_is_refused() {
    for spec in ["../secrets", "/etc/passwd", "tests/../../x"] {
        assert!(
            Protect::parse(spec, Path::new("/tmp")).is_err(),
            "`{spec}` was accepted"
        );
    }
}

#[test]
fn a_cli_rule_is_a_directory_when_it_names_one_and_a_file_otherwise() {
    let root = scratch("parse");
    fs::create_dir_all(root.join("tests")).expect("the directory");
    write(&root, "check.sh", "#!/bin/sh\n");
    assert!(matches!(
        Protect::parse("tests", &root).expect("a rule"),
        Protect::Dir(_)
    ));
    assert!(matches!(
        Protect::parse("check.sh", &root).expect("a rule"),
        Protect::File(_)
    ));
    assert!(matches!(
        Protect::parse("benches/", &root).expect("a rule"),
        Protect::Dir(_)
    ));
}

#[test]
fn an_empty_protect_list_still_freezes_the_check() {
    let root = scratch("check-only");
    let before = Ruler::take("true", &root, &[]).expect("a ruler");
    assert!(before.files.is_empty());
    let after = Ruler::take("false", &root, &[]).expect("a ruler");
    assert!(before.diff(&after).expect("a change").contains("check"));
}

#[test]
fn a_file_rule_alone_does_not_see_inline_tests_and_the_shipped_rule_does() {
    // Two facts worth pinning together. A rule set that names files cannot see a region of a file, which is
    // why the shipped profiles carry `InlineTests`; and with that rule the same edit is a change. The
    // contrast is the point: without it the help text would promise coverage the profile does not ask for.
    let root = scratch("inline");
    write(&root, "src/lib.rs", "fn answer() -> u32 { 42 }\n");
    write(&root, "tests/gate.rs", "assert_eq!(answer(), 42);\n");
    let files_only = Ruler::take("cargo test", &root, &protected()).expect("a ruler");
    let shipped = Ruler::take(
        "cargo test",
        &root,
        &[protected(), vec![Protect::InlineTests]].concat(),
    )
    .expect("a ruler");

    write(
        &root,
        "src/lib.rs",
        "fn answer() -> u32 { 42 }\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn a() { assert_eq!(super::answer(), 40); }\n}\n",
    );
    let after_files_only = Ruler::take("cargo test", &root, &protected()).expect("a ruler");
    assert_eq!(
        after_files_only.diff(&files_only),
        None,
        "a file rule is blind to this"
    );
    let after_shipped = Ruler::take(
        "cargo test",
        &root,
        &[protected(), vec![Protect::InlineTests]].concat(),
    )
    .expect("a ruler");
    assert!(
        after_shipped.diff(&shipped).is_some(),
        "the shipped rule missed an added test module"
    );
}

#[test]
fn a_symlink_added_under_a_protected_directory_is_a_change() {
    // The verifier can be a link: `cargo test` and every shell follow it. A walk that skipped symlinks
    // let a model add a test file to `tests/` without the ruler seeing it, from a target the cage keeps
    // writable (`state/tmp`).
    let root = scratch("symlink-add");
    let outside = root.join("state/tmp");
    fs::create_dir_all(&outside).expect("a writable target");
    write(&root, "tests/check.sh", "exit 0\n");
    let before = Ruler::take("cargo test", &root, &protected()).expect("a ruler");

    let target = outside.join("extra.rs");
    fs::write(&target, "#[test]\nfn extra() {}\n").expect("the target");
    std::os::unix::fs::symlink(&target, root.join("tests/extra.rs")).expect("a link");

    let after = Ruler::take("cargo test", &root, &protected()).expect("a ruler");
    let change = before.diff(&after).expect("the added link is a change");
    assert!(
        change.contains("tests/extra.rs") || change.contains("added"),
        "{change}"
    );
}

#[test]
fn a_change_to_the_file_a_frozen_symlink_points_at_is_a_change() {
    let root = scratch("symlink-target");
    let outside = root.join("outside");
    fs::create_dir_all(&outside).expect("a target directory");
    let target = outside.join("gate.rs");
    fs::write(&target, "#[test]\nfn a() { assert!(true); }\n").expect("the target");
    fs::create_dir_all(root.join("tests")).expect("the test directory");
    std::os::unix::fs::symlink(&target, root.join("tests/gate.rs")).expect("a link");

    let before = Ruler::take("cargo test", &root, &protected()).expect("a ruler");
    fs::write(&target, "#[test]\nfn a() {}\n").expect("a weaker target");
    let after = Ruler::take("cargo test", &root, &protected()).expect("a ruler");

    let change = before
        .diff(&after)
        .expect("the target's contents are frozen");
    assert!(change.contains("tests/gate.rs"), "{change}");
}

#[test]
fn a_weakened_assertion_inside_a_cfg_test_module_is_a_change() {
    // The limit this test closes: `cargo test` runs the modules inside `src/`, and a file ruler over
    // `tests/` could not see them. The assertion is weakened, every `tests/` file is untouched, and the
    // check goes green.
    let root = scratch("inline-weakened");
    write(
        &root,
        "src/lib.rs",
        "pub fn answer() -> u32 { 42 }\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn it_adds() { assert_eq!(answer() + 1, 43); }\n}\n",
    );
    write(&root, "tests/gate.rs", "#[test]\nfn gate() {}\n");
    let rules = vec![Protect::dir("tests").expect("a rule"), Protect::InlineTests];
    let before = Ruler::take("cargo test", &root, &rules).expect("a ruler");

    write(
        &root,
        "src/lib.rs",
        "pub fn answer() -> u32 { 42 }\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn it_adds() { assert_eq!(answer() + 1, 43 + 1); }\n}\n",
    );
    let after = Ruler::take("cargo test", &root, &rules).expect("a ruler");
    let change = before.diff(&after).expect("the inline assertion is frozen");
    assert!(change.contains("#cfg(test)::tests"), "{change}");
}

#[test]
fn an_edit_above_a_cfg_test_module_is_not_a_change() {
    // The region is keyed by name, not by byte offset, so fixing the code above the tests must not read as
    // a moved verifier. A ruler that refused this would be unusable on a repo whose tests live at the
    // bottom of every file, which is most of them.
    let root = scratch("inline-above");
    let body = "pub fn answer() -> u32 { %s }\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn it_is_42() { assert_eq!(super::answer(), 42); }\n}\n";
    write(
        &root,
        "src/lib.rs",
        &format!("// a header\n{}", body.replace("%s", "42")),
    );
    let rules = vec![Protect::InlineTests];
    let before = Ruler::take("cargo test", &root, &rules).expect("a ruler");
    let moved = format!(
        "// a header\n// one more line of it\n{}",
        body.replace("%s", "42")
    );
    write(&root, "src/lib.rs", &moved);
    let after = Ruler::take("cargo test", &root, &rules).expect("a ruler");
    assert_eq!(before.diff(&after), None, "{:?}", after.files);
}

#[test]
fn a_new_cfg_test_module_is_a_change() {
    let root = scratch("inline-added");
    write(&root, "src/lib.rs", "pub fn answer() -> u32 { 42 }\n");
    let rules = vec![Protect::InlineTests];
    let before = Ruler::take("cargo test", &root, &rules).expect("a ruler");
    write(
        &root,
        "src/lib.rs",
        "pub fn answer() -> u32 { 42 }\n\n#[cfg(test)]\nmod extra {\n    #[test]\n    fn always_passes() {}\n}\n",
    );
    let after = Ruler::take("cargo test", &root, &rules).expect("a ruler");
    let change = before
        .diff(&after)
        .expect("an added test module is a change");
    assert!(change.contains("#cfg(test)::extra"), "{change}");
}

#[test]
fn a_dependency_may_be_added_but_a_test_target_may_not() {
    // The cost this removes: freezing the whole manifest made `cargo add` a refusal. The structure that
    // decides what the check runs stays frozen.
    let root = scratch("manifest");
    write(
        &root,
        "Cargo.toml",
        "[package]\nname = \"gated\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde = \"1\"\n",
    );
    let rules = vec![Protect::Manifest("Cargo.toml".to_string())];
    let before = Ruler::take("cargo test", &root, &rules).expect("a ruler");

    write(
        &root,
        "Cargo.toml",
        "[package]\nname = \"gated\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde = \"1\"\nanyhow = \"1\"\n",
    );
    let added = Ruler::take("cargo test", &root, &rules).expect("a ruler");
    assert_eq!(
        before.diff(&added),
        None,
        "a dependency is not a moved verifier"
    );

    write(
        &root,
        "Cargo.toml",
        "[package]\nname = \"gated\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde = \"1\"\n\n[[test]]\nname = \"gate\"\npath = \"src/lib.rs\"\n",
    );
    let redirected = Ruler::take("cargo test", &root, &rules).expect("a ruler");
    assert!(
        before.diff(&redirected).is_some(),
        "a test target pointed at another file is a moved verifier"
    );
}

#[test]
fn a_change_under_a_symlinked_protected_directory_is_a_change() {
    // A pre-existing `tests -> ../shared` used to be recorded as a link and not entered, so editing the
    // target left every entry identical. The check follows the link, so the ruler has to.
    let root = scratch("symlink-dir");
    let shared = scratch("symlink-dir-shared");
    write(
        &shared,
        "gate.rs",
        "#[test]\nfn gate() { assert!(true); }\n",
    );
    std::os::unix::fs::symlink(&shared, root.join("tests")).expect("a directory link");

    let rules = vec![Protect::dir("tests").expect("a rule")];
    let before = Ruler::take("cargo test", &root, &rules).expect("a ruler");
    assert!(
        before.files.keys().any(|key| key.ends_with("gate.rs")),
        "the contents of the linked directory were not hashed: {:?}",
        before.files
    );

    write(&shared, "gate.rs", "#[test]\nfn gate() {}\n");
    let after = Ruler::take("cargo test", &root, &rules).expect("a ruler");
    let change = before
        .diff(&after)
        .expect("the target's contents are frozen");
    assert!(change.contains("gate.rs"), "{change}");
}

#[test]
fn a_source_file_with_multibyte_text_does_not_panic_the_scan() {
    // The scanner advances one byte at a time, so a comment with an accent used to slice through a
    // character and abort the whole run. A crash on a normal file is worse than a missed region.
    let root = scratch("inline-utf8");
    write(
        &root,
        "src/lib.rs",
        "// café: naïve résumé — the tests below are frozen\npub fn answer() -> u32 { 42 }\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn it_is_42() { assert_eq!(super::answer(), 42); }\n}\n",
    );
    let rules = vec![Protect::InlineTests];
    let before = Ruler::take("cargo test", &root, &rules).expect("a ruler");
    assert!(
        before
            .files
            .keys()
            .any(|key| key.ends_with("#cfg(test)::tests")),
        "{:?}",
        before.files
    );
    write(
        &root,
        "src/lib.rs",
        "// café: naïve résumé — the tests below are frozen\npub fn answer() -> u32 { 42 }\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn it_is_42() { assert_eq!(super::answer(), 41); }\n}\n",
    );
    let after = Ruler::take("cargo test", &root, &rules).expect("a ruler");
    assert!(
        before.diff(&after).is_some(),
        "the weakened assertion was missed"
    );
}
