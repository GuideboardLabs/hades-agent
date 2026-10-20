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
fn an_inline_test_module_is_outside_a_file_ruler() {
    // The limit the help text states: `cargo test` also runs `#[cfg(test)]` modules inside `src/*.rs`,
    // and a file ruler cannot name a region of a file the model must edit. This test pins the limit
    // rather than pretending it away: the edit is real and the ruler is unchanged.
    let root = scratch("inline");
    write(&root, "src/lib.rs", "fn answer() -> u32 { 42 }\n");
    write(&root, "tests/gate.rs", "assert_eq!(answer(), 42);\n");
    let ruler = Ruler::take("cargo test", &root, &protected()).expect("a ruler");
    write(&root, "src/lib.rs", "fn answer() -> u32 { 41 }\n");
    let after = Ruler::take("cargo test", &root, &protected()).expect("a ruler");
    assert_eq!(
        ruler.diff(&after),
        None,
        "a file ruler must report unchanged, which is exactly the hole the help text names"
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
