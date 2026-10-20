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
