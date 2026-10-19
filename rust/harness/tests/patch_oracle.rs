//! The oracle for the `patch` tool: the rule against real files, and the schema against the real binary.
//!
//! `agent::execute` is private, so the wiring is covered by the module's own tests; this file drives the
//! public `harness::patch` surface and the built CLI.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::json;

use harness::patch::{Edit, FileEdit, apply, plan_all, tool, write_all};
use harness::path::PathPolicy;

fn scratch(name: &str) -> PathBuf {
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/scratch");
    std::fs::create_dir_all(&base).ok();
    let base = std::fs::canonicalize(&base).unwrap_or(base);
    let dir = base.join(format!("hades-patch-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

/// The policy the oracle drives through: the scratch directory is the one root, as the workspace is in
/// the wired tool. Built after the directory exists, because the check canonicalizes its roots.
fn policy_at(dir: &Path) -> PathPolicy {
    PathPolicy::for_root(&std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf()))
}

fn edit(old: &str, new: &str) -> Edit {
    Edit {
        old: old.to_string(),
        new: new.to_string(),
    }
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).expect("the file reads")
}

#[test]
fn one_edit_changes_only_its_own_bytes() {
    let dir = scratch("one");
    let file = dir.join("note.txt");
    std::fs::write(&file, "alpha\nbeta\ngamma\n").expect("a file");

    let said = tool(
        &json!({"path": "note.txt", "edits": [{"old": "beta", "new": "BETA"}]}),
        &policy_at(&dir),
    );
    assert!(said.contains("patched"), "{said}");
    assert_eq!(read(&file), "alpha\nBETA\ngamma\n");
    assert!(said.contains("line 2"), "the hunk names its line: {said}");
}

#[test]
fn two_edits_apply_in_order() {
    let dir = scratch("two");
    let file = dir.join("note.txt");
    std::fs::write(&file, "one\ntwo\nthree\n").expect("a file");

    let said = tool(
        &json!({"path": "note.txt", "edits": [
            {"old": "one", "new": "ONE"},
            {"old": "three", "new": "THREE"}
        ]}),
        &policy_at(&dir),
    );
    assert!(said.contains("2 edit(s)"), "{said}");
    assert_eq!(read(&file), "ONE\ntwo\nTHREE\n");
}

#[test]
fn a_second_failure_leaves_the_first_edit_unwritten() {
    let dir = scratch("atomic");
    let file = dir.join("note.txt");
    std::fs::write(&file, "a\nb\n").expect("a file");

    let said = tool(
        &json!({"path": "note.txt", "edits": [
            {"old": "a", "new": "A"},
            {"old": "missing", "new": "x"}
        ]}),
        &policy_at(&dir),
    );
    assert!(said.contains("not found"), "{said}");
    assert_eq!(read(&file), "a\nb\n", "nothing was written");
}

#[test]
fn an_ambiguous_anchor_is_refused_and_says_how_many() {
    let dir = scratch("ambiguous");
    let file = dir.join("note.txt");
    std::fs::write(&file, "x\nx\n").expect("a file");

    let said = tool(
        &json!({"path": "note.txt", "edits": [{"old": "x", "new": "y"}]}),
        &policy_at(&dir),
    );
    assert!(said.contains("appears 2 times"), "{said}");
    assert_eq!(read(&file), "x\nx\n");
}

#[test]
fn an_empty_anchor_is_refused() {
    let dir = scratch("empty");
    std::fs::write(dir.join("note.txt"), "alpha\n").expect("a file");
    let said = tool(
        &json!({"path": "note.txt", "edits": [{"old": "", "new": "x"}]}),
        &policy_at(&dir),
    );
    assert!(said.contains("empty"), "{said}");
}

#[test]
fn a_missing_file_is_refused_and_nothing_is_created() {
    let dir = scratch("missing");
    let said = tool(
        &json!({"path": "new.txt", "edits": [{"old": "a", "new": "b"}]}),
        &policy_at(&dir),
    );
    assert!(said.contains("could not resolve"), "{said}");
    assert!(!dir.join("new.txt").exists());
}

#[test]
fn a_patch_of_a_path_outside_the_policy_is_refused_by_name() {
    let dir = scratch("outside");
    let away = dir
        .parent()
        .expect("a parent above the workspace")
        .join("away");
    std::fs::create_dir_all(&away).expect("a directory outside the workspace");
    let file = away.join("note.txt");
    std::fs::write(&file, "alpha\nbeta\nalpha\n").expect("a file outside the workspace");

    let said = tool(
        &json!({"path": file.display().to_string(), "edits": [{"old": "beta", "new": "BETA"}]}),
        &policy_at(&dir),
    );
    assert!(said.starts_with("Refused:"), "{said}");
    assert!(said.contains("outside"), "{said}");
    assert_eq!(
        std::fs::read_to_string(&file).expect("untouched"),
        "alpha\nbeta\nalpha\n"
    );
}

#[test]
fn a_crlf_file_asks_for_crlf_in_the_anchor() {
    let dir = scratch("crlf");
    let file = dir.join("note.txt");
    std::fs::write(&file, "alpha\r\nbeta\r\n").expect("a CRLF file");

    let missed = tool(
        &json!({"path": "note.txt", "edits": [{"old": "alpha\n", "new": "ALPHA\n"}]}),
        &policy_at(&dir),
    );
    assert!(missed.contains("CRLF"), "{missed}");

    let applied = tool(
        &json!({"path": "note.txt", "edits": [{"old": "alpha\r\n", "new": "ALPHA\r\n"}]}),
        &policy_at(&dir),
    );
    assert!(applied.contains("patched"), "{applied}");
    assert_eq!(read(&file), "ALPHA\r\nbeta\r\n");
}

#[test]
fn a_second_call_sees_the_first_result() {
    let dir = scratch("twice");
    let file = dir.join("note.txt");
    std::fs::write(&file, "alpha\n").expect("a file");

    let first = tool(
        &json!({"path": "note.txt", "edits": [{"old": "alpha", "new": "beta"}]}),
        &policy_at(&dir),
    );
    assert!(first.contains("patched"), "{first}");
    let second = tool(
        &json!({"path": "note.txt", "edits": [{"old": "beta", "new": "gamma"}]}),
        &policy_at(&dir),
    );
    assert!(second.contains("patched"), "{second}");
    assert_eq!(read(&file), "gamma\n");
}

#[test]
fn a_symlink_survives_and_its_target_is_edited() {
    let dir = scratch("symlink");
    let target = dir.join("real.txt");
    let link = dir.join("link.txt");
    std::fs::write(&target, "alpha\n").expect("a target");
    std::os::unix::fs::symlink(&target, &link).expect("a link");

    let said = tool(
        &json!({"path": "link.txt", "edits": [{"old": "alpha", "new": "beta"}]}),
        &policy_at(&dir),
    );
    assert!(said.contains("patched"), "{said}");
    assert!(link.is_symlink(), "the link is still a link");
    assert_eq!(read(&target), "beta\n");
}

#[test]
fn the_mode_survives_and_no_temp_file_is_left() {
    let dir = scratch("mode");
    let file = dir.join("run.sh");
    std::fs::write(&file, "#!/bin/sh\necho old\n").expect("a script");
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).expect("a mode");

    let said = tool(
        &json!({"path": "run.sh", "edits": [{"old": "echo old", "new": "echo new"}]}),
        &policy_at(&dir),
    );
    assert!(said.contains("patched"), "{said}");
    let mode = std::fs::metadata(&file)
        .expect("metadata")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o755, "the script stayed executable");

    let leftovers: Vec<String> = std::fs::read_dir(&dir)
        .expect("the directory")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name.contains("hades-patch"))
        .collect();
    assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
}

#[test]
fn an_unknown_field_in_an_edit_is_named() {
    let dir = scratch("unknown");
    std::fs::write(dir.join("note.txt"), "alpha\n").expect("a file");
    let said = tool(
        &json!({"path": "note.txt", "edits": [{"old": "alpha", "new": "beta", "after": "x"}]}),
        &policy_at(&dir),
    );
    assert!(said.contains("unknown field `after`"), "{said}");
}

#[test]
fn a_file_over_the_cap_is_refused_by_name() {
    let dir = scratch("big");
    let file = dir.join("big.txt");
    std::fs::write(&file, "a".repeat(harness::patch::CAP + 1)).expect("a big file");
    let said = tool(
        &json!({"path": "big.txt", "edits": [{"old": "a", "new": "b"}]}),
        &policy_at(&dir),
    );
    assert!(said.contains("1 MiB"), "{said}");
}

#[test]
fn apply_refuses_an_empty_edit_list() {
    let dir = scratch("none");
    let file = dir.join("note.txt");
    std::fs::write(&file, "alpha\n").expect("a file");
    assert!(apply(&file, &[]).is_err());
    assert!(apply(&file, &[edit("alpha", "beta")]).is_ok());
}

#[test]
fn the_read_surface_has_no_patch_and_the_act_surface_does() {
    let home = scratch("home");
    let run = |mode: &str| {
        let output = Command::new(env!("CARGO_BIN_EXE_hades"))
            .args(["tools", "--mode", mode])
            .env("HADES_HOME", &home)
            .output()
            .expect("the binary runs");
        assert!(output.status.success(), "tools --mode {mode} failed");
        String::from_utf8_lossy(&output.stdout).to_string()
    };
    assert!(
        run("act").contains("\n  patch\n"),
        "act mode has no patch tool"
    );
    assert!(
        !run("read").contains("\n  patch\n"),
        "a read surface was handed a patch tool"
    );
}

fn file(path: &str, old: &str, new: &str) -> FileEdit {
    FileEdit {
        path: path.to_string(),
        old: old.to_string(),
        new: new.to_string(),
    }
}

#[test]
fn one_bad_anchor_writes_none_of_the_files() {
    let dir = scratch("all-or-nothing");
    std::fs::write(dir.join("a.txt"), "alpha\n").expect("a");
    std::fs::write(dir.join("b.txt"), "beta\n").expect("b");

    let said = plan_all(
        &policy_at(&dir),
        &[
            file("a.txt", "alpha", "ALPHA"),
            file("b.txt", "missing", "X"),
        ],
    )
    .expect_err("the second anchor does not exist");
    assert!(said.contains("nothing was written"), "{said}");
    assert_eq!(read(&dir.join("a.txt")), "alpha\n");
    assert_eq!(read(&dir.join("b.txt")), "beta\n");
}

#[test]
fn two_files_are_planned_then_written_once_each() {
    let dir = scratch("two-files");
    std::fs::write(dir.join("a.txt"), "alpha\n").expect("a");
    std::fs::write(dir.join("b.txt"), "beta\n").expect("b");

    let plans = plan_all(
        &policy_at(&dir),
        &[
            file("a.txt", "alpha", "ALPHA"),
            file("b.txt", "beta", "BETA"),
        ],
    )
    .expect("both plan");
    assert_eq!(plans.len(), 2);
    let report = write_all(&plans).expect("both write");
    assert!(report.contains("2 file(s)"), "{report}");
    assert_eq!(read(&dir.join("a.txt")), "ALPHA\n");
    assert_eq!(read(&dir.join("b.txt")), "BETA\n");
}

#[test]
fn one_path_spelled_two_ways_is_one_file_with_both_edits_in_order() {
    let dir = scratch("two-spellings");
    std::fs::write(dir.join("a.txt"), "one two\n").expect("a");

    let plans = plan_all(
        &policy_at(&dir),
        &[file("a.txt", "one", "ONE"), file("./a.txt", "two", "TWO")],
    )
    .expect("one file");
    assert_eq!(plans.len(), 1, "the two spellings resolve to one file");
    assert_eq!(plans[0].hunks.len(), 2);
    write_all(&plans).expect("written");
    assert_eq!(read(&dir.join("a.txt")), "ONE TWO\n");
}
