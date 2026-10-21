//! The cage, tested by trying to get out of it.
//!
//! Every test here has a twin: the same program, the same tree, once with a cage and once without. A test
//! that only shows a thing failing proves nothing about why it failed, so the pair is the evidence: the read
//! that is denied under a cage succeeds without one, which is what makes "the cage denied it" a finding
//! rather than an assumption about the machine.
//!
//! These run against the kernel, not a mock. Landlock is either there and working or the tests say so.

use std::path::{Path, PathBuf};

use harness::tool::{Call, Tool};
use harness::{Cage, Shell};

/// A tree with a root to work in and a directory outside it, which is where the secrets live.
fn scratch(name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let base = scratch_base().join(format!("hades-cage-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("root");
    let outside = base.join("outside");
    std::fs::create_dir_all(&root).expect("a root");
    std::fs::create_dir_all(&outside).expect("an outside");
    (base, root, outside)
}

fn write(path: &Path, text: &str) {
    std::fs::write(path, text).expect("a file");
}

fn shell(root: &Path, cage: Option<Cage>) -> Shell {
    match cage {
        Some(cage) => Shell::new(root, 8192).caged(cage),
        None => Shell::new(root, 8192),
    }
}

#[test]
fn the_cage_denies_a_read_outside_the_root() {
    let (_base, root, outside) = scratch("read");
    let secret = outside.join("secret.txt");
    write(&secret, "the keys to everything");

    let caged = shell(&root, Some(Cage::around(&root)));
    let run = caged
        .call(&Call::new(format!("cat {}", secret.display())))
        .expect("a run");
    assert!(!run.passed(), "the cage let it read: {:?}", run.output);
    assert!(
        run.output.contains("Permission denied"),
        "refused for the right reason: {}",
        run.output
    );

    // Told, not hidden: the file is still there and the program is told it may not.
    assert!(secret.exists());
}

#[test]
fn a_caged_program_can_write_to_dev_null() {
    // A shell redirects to /dev/null constantly. The first cut granted `/dev` as a directory, which is
    // readable and not writable, and every program that redirected died with
    // `sh: 1: cannot create /dev/null: Permission denied` on the way to doing anything.
    let (_base, root, _outside) = scratch("devnull");
    let caged = shell(&root, Some(Cage::around(&root)));
    let run = caged
        .call(&Call::new("echo nothing > /dev/null && echo wrote ok"))
        .expect("a run");
    assert!(run.passed(), "a shell could not redirect: {}", run.output);
    assert!(run.output.contains("wrote ok"), "{}", run.output);
}

#[test]
fn a_caged_program_can_start_the_language_runtime_it_needs() {
    // A program that cannot start an interpreter cannot do the work, and the failure is worse than a
    // refusal: against a frontier model, three attempts in a row went into diagnosing Python instead of
    // fixing the tests, because the interpreter inside the cage died with
    // `ModuleNotFoundError: No module named 'encodings'`. The model was right; the cage was wrong.
    let (_base, root, _outside) = scratch("runtime");
    let caged = shell(&root, Some(Cage::around(&root).with_runtimes()));
    let run = caged
        .call(&Call::new("python3 -c 'print(40 + 2)'"))
        .expect("a run");
    assert!(
        run.passed(),
        "python could not start inside the cage: {}",
        run.output
    );
    assert!(run.output.contains("42"), "{}", run.output);
}

#[test]
fn the_runtime_grant_is_read_only_and_does_not_reach_the_secrets() {
    // The grant exists to start an interpreter, not to open the box: it is read and execute, and the file a
    // cage is for reading is still refused.
    let (_base, root, outside) = scratch("runtime-narrow");
    let secret = outside.join("config.yaml");
    write(&secret, "provider: ollama-cloud");
    let caged = shell(&root, Some(Cage::around(&root).with_runtimes()));

    let write_attempt = caged
        .call(&Call::new(format!(
            "python3 -c \"open('{}/config.yaml','a').write('x')\"",
            outside.display()
        )))
        .expect("a run");
    assert!(
        !write_attempt.passed(),
        "python wrote outside the root: {}",
        write_attempt.output
    );
    assert!(
        write_attempt.output.contains("Permission denied")
            || write_attempt.output.contains("PermissionError"),
        "refused for the right reason: {}",
        write_attempt.output
    );
    assert_eq!(
        std::fs::read_to_string(&secret).expect("the file"),
        "provider: ollama-cloud",
        "the file changed"
    );
}

#[test]
fn without_the_cage_the_same_read_succeeds() {
    // The twin. Without this, a denied read could be a broken path, a missing file or a typo.
    let (_base, root, outside) = scratch("read-free");
    let secret = outside.join("secret.txt");
    write(&secret, "the keys to everything");

    let run = shell(&root, None)
        .call(&Call::new(format!("cat {}", secret.display())))
        .expect("a run");
    assert!(run.passed(), "{}", run.output);
    assert!(run.output.contains("the keys to everything"));
}

#[test]
fn the_cage_allows_a_write_inside_the_root() {
    let (_base, root, _outside) = scratch("write-in");
    let run = shell(&root, Some(Cage::around(&root)))
        .call(&Call::new("echo hello > inside.txt"))
        .expect("a run");
    assert!(run.passed(), "{}", run.output);
    assert_eq!(
        std::fs::read_to_string(root.join("inside.txt")).expect("the file"),
        "hello\n"
    );
}

#[test]
fn the_cage_denies_a_write_outside_the_root() {
    let (_base, root, outside) = scratch("write-out");
    let target = outside.join("planted.txt");
    let run = shell(&root, Some(Cage::around(&root)))
        .call(&Call::new(format!("echo planted > {}", target.display())))
        .expect("a run");
    assert!(!run.passed(), "the cage let it write: {:?}", run.output);
    assert!(run.output.contains("Permission denied"), "{}", run.output);
    assert!(!target.exists(), "nothing was planted");
}

#[test]
fn a_directory_the_cage_was_told_about_is_writable() {
    // A program that wants a scratch directory is not a program that needs `/tmp`: it needs somewhere it may
    // write, and this is how it is given one.
    let (_base, root, _outside) = scratch("scratch-dir");
    let scratch_dir = root.join("tmp");
    std::fs::create_dir_all(&scratch_dir).expect("a scratch");

    let run = shell(&root, Some(Cage::around(&root).also_writable(&scratch_dir)))
        .call(&Call::new("echo scratch > tmp/note.txt"))
        .expect("a run");
    assert!(run.passed(), "{}", run.output);
    assert_eq!(
        std::fs::read_to_string(scratch_dir.join("note.txt")).expect("the file"),
        "scratch\n"
    );
}

#[test]
fn the_cage_denies_the_network() {
    let (_base, root, _outside) = scratch("net");
    // `bash` and its `/dev/tcp`, rather than `python3`: the `python3` on this box's PATH is a virtual
    // environment under the home directory, so inside a cage that denies the home directory it cannot even
    // start, and a test that fails because its tool is missing proves nothing about the network.
    let program = "timeout 5 bash -c 'exec 3<>/dev/tcp/1.1.1.1/80'";
    let run = shell(&root, Some(Cage::around(&root)))
        .call(&Call::new(program))
        .expect("a run");
    assert!(!run.passed(), "the cage let it out: {:?}", run.output);
    assert!(
        run.output.contains("Permission denied"),
        "refused by the cage rather than by the network: {}",
        run.output
    );

    // The twin: without the cage the same attempt gets as far as the network, which is what makes the
    // refusal above the cage's doing rather than the box's.
    let open = shell(&root, None).call(&Call::new(program)).expect("a run");
    assert!(
        !open.output.contains("Permission denied"),
        "the same attempt is refused for a different reason without the cage: {}",
        open.output
    );
}

#[test]
fn the_cage_holds_for_what_the_program_starts() {
    // The program is a shell, and it starts another shell. What the first cannot read, the second cannot
    // read either, because the confinement belongs to the process and is inherited.
    let (_base, root, outside) = scratch("children");
    let secret = outside.join("secret.txt");
    write(&secret, "still the keys");

    let run = shell(&root, Some(Cage::around(&root)))
        .call(&Call::new(format!(
            "sh -c 'sh -c \"cat {}\"'",
            secret.display()
        )))
        .expect("a run");
    assert!(!run.passed(), "{:?}", run.output);
    assert!(run.output.contains("Permission denied"), "{}", run.output);
}

#[test]
fn the_cage_still_lets_a_program_run_and_read_the_system() {
    // The other half of the claim: a cage that stops everything is not a cage, it is a fault.
    let (_base, root, _outside) = scratch("system");
    let run = shell(&root, Some(Cage::around(&root)))
        .call(&Call::new("cat /etc/hostname && echo ran"))
        .expect("a run");
    assert!(run.passed(), "{}", run.output);
    assert!(run.output.contains("ran"));
}

#[test]
fn a_cage_that_cannot_be_built_stops_the_run() {
    // Refusing to run is the only safe way for this to fail: the program does not get the caller's rights
    // because the cage had a bad day.
    let (_base, root, _outside) = scratch("no-cage");
    let missing = root.join("not-here");
    let caged = shell(&root, Some(Cage::around(&root).also_writable(&missing)));

    let error = caged
        .call(&Call::new("echo should-not-run > ran.txt"))
        .expect_err("the cage cannot be built");
    assert!(
        error.detail().contains("not-here"),
        "the fault names what was wrong: {}",
        error.detail()
    );
    assert!(
        !root.join("ran.txt").exists(),
        "the program did not run with the caller's rights instead"
    );
}

#[test]
fn the_tool_says_what_it_confines() {
    let (_base, root, _outside) = scratch("describe");
    let caged = shell(&root, Some(Cage::around(&root)));
    let described = caged.describe();
    assert!(described.contains("network: closed"), "{described}");
    assert!(described.contains("writable:"), "{described}");

    let open = shell(&root, None);
    assert!(
        open.describe().contains("nothing is confined"),
        "{}",
        open.describe()
    );
}

#[test]
fn the_report_names_the_socket_limit() {
    // A Docker socket is a way to become root on the host, and Landlock's filesystem rules do not close a
    // pathname Unix socket before ABI 9, and never close an abstract one. A cage that implied otherwise
    // would be lying about what it contains; the report says the gap in words.
    let (_base, root, _outside) = scratch("report");
    let report = Cage::around(&root).report();
    assert!(report.contains("unix sockets"), "{report}");
    assert!(report.contains("landlock abi"), "{report}");
    assert!(report.contains("tcp:"), "{report}");
}

#[test]
fn the_landlock_syscalls_are_the_ones_the_kernel_table_names() {
    // The crate used to hard-code x86-64's numbers behind a `cfg`, which is why an ARM64 build did not
    // compile. They now come from libc, and this asserts the value on this architecture rather than
    // trusting that a table exists: 444, 445 and 446 on both the x86-64 and the generic tables.
    assert_eq!(libc::SYS_landlock_create_ruleset, 444);
    assert_eq!(libc::SYS_landlock_add_rule, 445);
    assert_eq!(libc::SYS_landlock_restrict_self, 446);
}

#[test]
fn the_abi_probe_answers_rather_than_panicking() {
    // On a kernel with Landlock this is the version; on one without it is ENOSYS, which `available()`
    // turns into `false`. Neither may panic, and a cross-check has to compile the same code.
    let abi = harness::cage::abi();
    // Either a version (>= 1) or a negative errno. Zero is neither, and a zero would silently look like
    // "no rights" rather than "no syscall".
    assert!(
        abi != 0,
        "the probe returned neither an abi nor an error: {abi}"
    );
    let _ = harness::cage::available();
}

#[test]
fn the_socket_right_is_handled_only_where_the_kernel_has_it() {
    // ABI 9 (Linux 7.1) added LANDLOCK_ACCESS_FS_RESOLVE_UNIX, bit 16 of the filesystem mask. Landlock
    // rejects a handled bit the running kernel does not know, so it is added only at ABI 9; below that the
    // report keeps saying the sockets are not contained rather than implying they are.
    assert_ne!(
        harness::cage::fs_rights_for_abi(9) & harness::cage::LANDLOCK_ACCESS_FS_RESOLVE_UNIX,
        0
    );
    assert_eq!(
        harness::cage::fs_rights_for_abi(8) & harness::cage::LANDLOCK_ACCESS_FS_RESOLVE_UNIX,
        0
    );
    assert_ne!(
        harness::cage::read_rights_for_abi(9) & harness::cage::LANDLOCK_ACCESS_FS_RESOLVE_UNIX,
        0,
        "a readable path is also a pathname socket a program may connect to"
    );
    assert_eq!(
        harness::cage::read_rights_for_abi(8) & harness::cage::LANDLOCK_ACCESS_FS_RESOLVE_UNIX,
        0
    );
    if harness::cage::abi() < 9 {
        assert!(
            Cage::around("/work")
                .socket_note()
                .contains("not contained"),
            "{}",
            Cage::around("/work").socket_note()
        );
    }
}

#[test]
fn strict_mode_does_not_grant_run() {
    let cage = Cage::strict("/work");
    assert!(
        !cage.grants_read(Path::new("/run")),
        "strict mode removes the broad /run grant"
    );
    assert!(
        cage.grants_read(Path::new("/usr")),
        "the system a program needs to start is still readable"
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
