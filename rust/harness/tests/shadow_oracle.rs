//! The oracle for the checkpointer.
//!
//! Every test here drives the real store over a real directory: a snapshot is a copy, and the only way to
//! know a copy is faithful is to change the original and put it back.
//!
//! The pair that matters most is the fourth: a restore must be able to *remove*, and it must not be able to
//! touch. A safety net that deletes something it was told to keep is worse than no safety net, because the
//! damage happens at the moment it is trusted.

use std::fs;
use std::path::{Path, PathBuf};

use harness::shadow::{Checkpointer, Nothing, Shadows, Snapshot, SnapshotId, Subject};

/// The id, or a loud failure saying the snapshot was declined and why.
fn taken(snapshot: Snapshot) -> SnapshotId {
    match snapshot {
        Snapshot::Taken(id) => id,
        Snapshot::Declined(why) => panic!("the snapshot was declined: {why}"),
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = scratch_base().join(format!("hades-shadow-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("a scratch directory");
    // Canonical, because the never-touch rule is a path-prefix test and a symlinked temp directory would
    // make every prefix miss.
    fs::canonicalize(&dir).expect("a canonical scratch directory")
}

fn write(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("a parent directory");
    }
    fs::write(path, text).expect("a file");
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

#[test]
fn a_restore_puts_every_file_back_and_brings_back_a_deleted_one() {
    let root = scratch("restore");
    let store = scratch("restore-store");
    write(&root.join("src/a.rs"), "fn a() {}\n");
    write(&root.join("README.md"), "hello\n");
    write(&root.join("gone.txt"), "gone\n");

    let shadows = Shadows::new(&store);
    let subject = Subject::new("s", &root);
    let id = taken(shadows.snapshot(&subject).expect("a snapshot"));

    write(&root.join("src/a.rs"), "fn a() { changed }\n");
    write(&root.join("README.md"), "goodbye\n");
    fs::remove_file(root.join("gone.txt")).expect("remove a file the program would have removed");

    let restored = shadows.restore(&subject, &id).expect("a restore");

    assert_eq!(read(&root.join("src/a.rs")), "fn a() {}\n");
    assert_eq!(read(&root.join("README.md")), "hello\n");
    assert_eq!(read(&root.join("gone.txt")), "gone\n");
    assert_eq!(restored.put_back, 3, "{restored:?}");
    assert_eq!(restored.removed, 0, "{restored:?}");
}

#[test]
fn a_restore_removes_what_the_program_made_and_leaves_the_ignored_paths_alone() {
    let root = scratch("removes");
    let store = scratch("removes-store");
    write(&root.join("kept.txt"), "kept\n");
    write(&root.join(".git/HEAD"), "ref: the repo's own history\n");

    let shadows = Shadows::new(&store);
    let subject = Subject::new("s", &root);
    let id = taken(shadows.snapshot(&subject).expect("a snapshot"));

    write(&root.join("made.txt"), "made by the program\n");
    write(&root.join(".git/HEAD"), "ref: rewritten by the program\n");

    let restored = shadows.restore(&subject, &id).expect("a restore");

    assert!(
        !root.join("made.txt").exists(),
        "the program's file is still there"
    );
    assert_eq!(
        read(&root.join(".git/HEAD")),
        "ref: rewritten by the program\n",
        "a restore wrote inside an ignored path"
    );
    assert_eq!(restored.removed, 1, "{restored:?}");
    assert!(restored.left_alone >= 1, "{restored:?}");
}

#[test]
fn a_build_directory_is_never_copied() {
    let root = scratch("ignore");
    let store = scratch("ignore-store");
    write(&root.join("src/a.rs"), "pub fn a() {}\n");
    write(&root.join("target/debug/junk.bin"), "a build artifact\n");
    write(&root.join("node_modules/dep/index.js"), "a dependency\n");

    let shadows = Shadows::new(&store);
    let subject = Subject::new("s", &root);
    let id = taken(shadows.snapshot(&subject).expect("a snapshot"));

    let names: Vec<String> = shadows
        .manifest(&id)
        .expect("a manifest")
        .files
        .iter()
        .map(|file| file.path.clone())
        .collect();
    assert_eq!(names, vec!["src/a.rs".to_string()], "{names:?}");
}

#[test]
fn a_restore_never_touches_a_path_it_was_told_to_keep_out_of() {
    let root = scratch("keepout");
    let store = scratch("keepout-store");
    let keep = root.join("keepout");
    write(&keep.join("precious.txt"), "original\n");
    write(&root.join("ordinary.txt"), "original\n");

    let shadows = Shadows::new(&store);
    // Built from the canonical root, so the prefix test is comparing like with like.
    let subject = Subject::new("s", &root).never_touching(&keep);
    let id = taken(shadows.snapshot(&subject).expect("a snapshot"));

    write(&keep.join("precious.txt"), "rewritten\n");
    write(&root.join("ordinary.txt"), "rewritten\n");
    write(&keep.join("appeared.txt"), "made\n");

    let restored = shadows.restore(&subject, &id).expect("a restore");

    assert_eq!(
        read(&keep.join("precious.txt")),
        "rewritten\n",
        "a restore wrote inside a never-touch path"
    );
    assert!(
        keep.join("appeared.txt").exists(),
        "a restore removed something inside a never-touch path"
    );
    assert_eq!(read(&root.join("ordinary.txt")), "original\n");
    assert!(restored.left_alone >= 1, "{restored:?}");
}

#[test]
fn a_store_inside_the_tree_does_not_copy_itself() {
    let root = scratch("recursion");
    write(&root.join("a.txt"), "a\n");
    let store = root.join("state/shadows");
    write(
        &store.join("already-here.txt"),
        "the store is inside the tree\n",
    );

    let shadows = Shadows::new(&store);
    let subject = Subject::new("s", &root);
    let id = taken(shadows.snapshot(&subject).expect("a snapshot"));

    let names: Vec<String> = shadows
        .manifest(&id)
        .expect("a manifest")
        .files
        .iter()
        .map(|file| file.path.clone())
        .collect();
    assert_eq!(
        names,
        vec!["a.txt".to_string()],
        "the snapshot copied its own output"
    );
}

#[test]
fn every_snapshot_records_what_it_cost_and_gets_its_own_number() {
    let root = scratch("cost");
    let store = scratch("cost-store");
    write(&root.join("a.txt"), "12345\n");

    let shadows = Shadows::new(&store);
    let subject = Subject::new("s", &root);
    let first = taken(shadows.snapshot(&subject).expect("a snapshot"));
    let second = taken(shadows.snapshot(&subject).expect("a second snapshot"));

    assert_eq!(first.n, 1);
    assert_eq!(second.n, 2);
    assert_eq!(shadows.latest("s").map(|id| id.n), Some(2));
    assert!(
        shadows.manifest(&first).is_ok(),
        "the first manifest is readable"
    );
    assert!(
        shadows.manifest(&second).is_ok(),
        "the second manifest is readable"
    );

    let log = read(&store.join("s/cost.jsonl"));
    let lines: Vec<serde_json::Value> = log
        .lines()
        .map(|line| serde_json::from_str(line).expect("one entry of JSON per line"))
        .collect();
    assert_eq!(lines.len(), 2, "{log}");
    assert_eq!(lines[0]["files"], 1, "{log}");
    assert_eq!(lines[0]["bytes"], 6, "{log}");
    assert!(lines[0]["millis"].is_u64(), "{log}");
}

/// A tree too big to copy is refused out loud, with its size in the reason and a way out of it. This is the
/// bound the module's own measurement set: the harness's dev tree is 659 MB over 32,451 files, and a copy at
/// that price is a copy that gets switched off without anybody being told.
#[test]
fn a_tree_past_the_bound_is_declined_with_its_size_in_the_reason() {
    let root = scratch("too-big");
    let store = scratch("too-big-store");
    write(&root.join("a.txt"), "a\n");
    write(&root.join("big.bin"), &"x".repeat(4096));

    let shadows = Shadows::new(&store).up_to(1024, 10);
    let subject = Subject::new("s", &root);
    let decided = shadows.snapshot(&subject).expect("a decision");

    let Snapshot::Declined(why) = &decided else {
        panic!("a tree past the bound was copied anyway");
    };
    assert!(
        why.contains("4 KB"),
        "the reason should carry the size: {why}"
    );
    assert!(
        why.contains("--root"),
        "the reason should say what to do about it: {why}"
    );
    assert!(
        decided.label().starts_with("declined"),
        "a decline reads as a decline in the log: {}",
        decided.label()
    );
    assert!(
        !store.join("s").exists(),
        "a declined snapshot still wrote something"
    );

    // And the same tree under a bound that fits is taken, so the test is about the bound and not about the
    // store failing at large sizes.
    let roomy = Shadows::new(&store).up_to(1024 * 1024, 100);
    assert!(matches!(
        roomy.snapshot(&subject).expect("a decision"),
        Snapshot::Taken(_)
    ));
}

#[test]
fn a_manifest_that_will_not_read_fails_loudly_and_names_the_file() {
    let root = scratch("bad-manifest");
    let store = scratch("bad-manifest-store");
    write(&root.join("a.txt"), "a\n");

    let shadows = Shadows::new(&store);
    let subject = Subject::new("s", &root);
    let id = taken(shadows.snapshot(&subject).expect("a snapshot"));

    let manifest = store.join("s").join(format!("{}.json", id.n));
    fs::write(&manifest, "{ this is not json").expect("a broken manifest");

    let fault = shadows
        .restore(&subject, &id)
        .expect_err("a broken manifest is a fault");
    assert!(
        fault.detail().contains("manifest"),
        "the fault should say what could not be read: {}",
        fault.detail()
    );
    assert!(
        fault.detail().contains(&manifest.display().to_string()),
        "the fault should name the file: {}",
        fault.detail()
    );
}

#[test]
fn a_manifest_missing_a_key_fails_loudly_rather_than_guessing() {
    let root = scratch("short-manifest");
    let store = scratch("short-manifest-store");
    write(&root.join("a.txt"), "a\n");

    let shadows = Shadows::new(&store);
    let subject = Subject::new("s", &root);
    let id = taken(shadows.snapshot(&subject).expect("a snapshot"));

    let manifest = store.join("s").join(format!("{}.json", id.n));
    fs::write(&manifest, r#"{"session":"s","n":1,"files":[]}"#).expect("a short manifest");

    let fault = shadows.manifest(&id).expect_err("a missing key is a fault");
    assert!(fault.detail().contains("bytes"), "{}", fault.detail());
}

#[test]
fn nothing_says_it_snapshots_nothing_and_writes_nothing() {
    let root = scratch("nothing");
    write(&root.join("a.txt"), "a\n");

    let nothing = Nothing;
    let subject = Subject::new("s", &root);
    let decided = nothing.snapshot(&subject).expect("a decision, for the log");
    assert!(
        matches!(decided, Snapshot::Declined(_)),
        "a checkpointer that copies nothing claimed to have taken a snapshot"
    );
    assert!(
        decided.label().contains("nothing is snapshotted"),
        "{}",
        decided.label()
    );

    assert!(
        nothing.describe().contains("nothing is snapshotted"),
        "{}",
        nothing.describe()
    );
    assert!(
        !root.join("shadows").exists(),
        "a checkpointer that copies nothing made something"
    );
    assert!(
        nothing.restore(&subject, &SnapshotId::new("s", 0)).is_err(),
        "a restore with no snapshot has to fail rather than pretend"
    );
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
    let base = harness::scratch::root();
    std::fs::create_dir_all(&base).ok();
    std::fs::canonicalize(&base).unwrap_or(base)
}
