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
fn a_named_toolchain_is_readable_and_an_unnamed_home_path_is_not() {
    // Discovery cannot look under the home without granting it, so a rustup under `~/.cargo` is invisible
    // unless the config names it. Naming it grants that directory and nothing around it.
    let (base, _root, _outside) = scratch("toolchain-named");
    let toolchain = base.join("home/.cargo");
    std::fs::create_dir_all(toolchain.join("bin")).expect("a toolchain");
    let cage = Cage::around("/work").with_runtime_paths([toolchain.clone()]);
    assert!(cage.grants_read(&toolchain));
    assert!(cage.grants_read(toolchain.join("bin/cargo")));
    assert!(
        !cage.grants_read(base.join("home/.ssh")),
        "the rest of the home stays denied"
    );
}

#[test]
fn a_toolchain_path_that_is_the_home_or_above_it_is_refused() {
    // A grant that resolves to the home directory is the whole filesystem with extra steps, and a path that
    // does not exist is a typo; neither may become a silent grant.
    let (base, root, _outside) = scratch("toolchain-refused");
    let cage = Cage::around(&root);
    // The real home, not a synthetic one: a made-up directory under the scratch tree is somewhere this cage
    // may legitimately grant, and testing it would prove nothing about the home rule.
    let real_home = PathBuf::from(std::env::var_os("HOME").expect("a home directory"));
    assert!(
        !cage
            .clone()
            .with_runtime_paths([real_home.clone()])
            .grants_read(&real_home)
    );
    assert!(
        !cage
            .clone()
            .with_runtime_paths([PathBuf::from("/")])
            .grants_read(Path::new("/"))
    );
    let missing = base.join("home/does-not-exist");
    assert!(
        !cage
            .with_runtime_paths([missing.clone()])
            .grants_read(&missing)
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
    // `bash` and its `/dev/tcp`, rather than `python3`: the `python3` on the test host's PATH is a virtual
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
    // A Docker socket is a way to become root on the host. Landlock's filesystem rules do not close a
    // pathname Unix socket before ABI 9, and never close an abstract one, so the cage refuses
    // `socket(AF_UNIX)` at the syscall instead. The report says which mechanism is doing the work,
    // because "landlock" alone would imply more than it does.
    let (_base, root, _outside) = scratch("report");
    let report = Cage::around(&root).report();
    assert!(report.contains("unix sockets: closed"), "{report}");
    assert!(report.contains("seccomp"), "{report}");
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
        // Below ABI 9 Landlock has no socket rule at all; the cage's own note says which mechanism is
        // doing the work rather than implying Landlock does it.
        assert!(
            Cage::around("/work").socket_note().contains("seccomp"),
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

/// The chat shell's path list, without running anything.
///
/// The measurement that decided this: on Linux 7.0.0 with Landlock ABI 8, a caged child connected to
/// `/run/docker.sock`, to `/run/user/1000/bus`, and to a socket under a directory no rule named, in the
/// broad cage and in the strict one alike. The path list is not the boundary for sockets on an ABI-8
/// kernel, which is why the chat cage drops `/run` for the paths it can see and refuses `socket(AF_UNIX)`
/// for the ones it cannot.
#[test]
fn the_chat_cage_does_not_grant_run_and_keeps_the_resolver() {
    let (_base, root, _outside) = scratch("chat-paths");
    let scratch_dir = root.join("tmp");
    std::fs::create_dir_all(&scratch_dir).expect("a scratch");
    let cage = Cage::chat(&root, &scratch_dir).with_runtimes();
    assert!(
        !cage.grants_read(Path::new("/run")),
        "the chat shell still reads /run: {}",
        cage.describe()
    );
    assert!(
        !cage.grants_read(Path::new("/run/docker.sock")),
        "a host control socket is still on the readable list: {}",
        cage.describe()
    );
    // `/etc/resolv.conf` is a symlink into `/run/systemd/resolve` on a systemd host, so the named grants
    // are what keeps a name lookup working once the broad grant is gone.
    for resolver in ["/run/systemd/resolve", "/run/nscd"] {
        let path = Path::new(resolver);
        if path.exists() {
            assert!(
                cage.grants_read(path),
                "{resolver} is not granted: {}",
                cage.describe()
            );
        }
    }
}

/// A caged program cannot dial a Unix socket, whichever directory it lives in.
#[test]
fn a_caged_program_cannot_reach_a_unix_socket() {
    let (_base, root, outside) = scratch("sock");
    // The worst case for the claim: the socket sits inside the directory the cage was told to treat as
    // writable. A path rule cannot help there, so only the syscall-level refusal can.
    let granted = root.join("tmp");
    std::fs::create_dir_all(&granted).expect("a scratch");
    let inside = granted.join("ctl.sock");
    let unattached = outside.join("ctl.sock");
    for socket in [&inside, &unattached] {
        let listener = std::os::unix::net::UnixListener::bind(socket).expect("bind");
        std::mem::forget(listener);
    }

    let Some(python) = python() else {
        println!("skipping: no system python3 for the caged side of this test");
        return;
    };
    let program = format!(
        "{python} -c 'import socket,sys\n\
         for p in sys.argv[1:]:\n\
         \x20   try:\n\
         \x20       s=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM);s.settimeout(2);s.connect(p)\n\
         \x20       print(\"CONNECTED\",p);s.close()\n\
         \x20   except OSError as e:\n\
         \x20       print(\"REFUSED\",p,e.errno,e.strerror)' {} {}",
        inside.display(),
        unattached.display()
    );
    let run = |cage: Option<&Cage>| {
        harness::command::run_within(
            &program,
            &root,
            8192,
            cage,
            Some(std::time::Duration::from_secs(30)),
        )
        .expect("a run")
        .output
    };

    let chat = Cage::chat(&root, &granted).with_runtimes();
    let caged = run(Some(&chat));
    for socket in [&inside, &unattached] {
        assert!(
            caged.contains(&format!("REFUSED {} 13", socket.display())),
            "a caged program reached {}: {caged}",
            socket.display()
        );
    }

    // The governed run's shell is the same program under a different cage, so it is closed too.
    let governed = run(Some(&Cage::around(&root).also_writable(&granted)));
    assert!(
        governed.contains(&format!("REFUSED {} 13", inside.display())),
        "a governed run's program reached {}: {governed}",
        inside.display()
    );

    // The twin: the same program, the same socket, with the refusal lifted. Without this, "refused"
    // could be a missing interpreter or a broken path rather than the cage.
    let open = caged_open(&chat, &program, &root);
    assert!(
        open.contains(&format!("CONNECTED {}", inside.display())),
        "the same connect failed with the refusal lifted, so the test proves nothing: {open}"
    );

    // And a host control socket, when this host has one, is refused the same way.
    for control in harness::cage::SOCKETS.iter().map(Path::new) {
        if !control.exists() {
            continue;
        }
        let attempt = format!(
            "{python} -c 'import socket\ntry:\n\x20   s=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM);s.connect(\"{}\");print(\"CONNECTED\")\nexcept OSError as e:\n\x20   print(\"REFUSED\",e.errno)'",
            control.display()
        );
        let refused = harness::command::run_within(
            &attempt,
            &root,
            8192,
            Some(&chat),
            Some(std::time::Duration::from_secs(20)),
        )
        .expect("a run")
        .output;
        assert!(
            refused.contains("REFUSED 13"),
            "a caged program reached {}: {refused}",
            control.display()
        );
    }
}

/// The same program with the socket refusal lifted, as the twin for the test above.
fn caged_open(cage: &Cage, program: &str, root: &Path) -> String {
    harness::command::run_within(
        program,
        root,
        8192,
        Some(&cage.clone().allow_unix_sockets()),
        Some(std::time::Duration::from_secs(30)),
    )
    .expect("a run")
    .output
}

/// A name lookup still works in the chat cage, and so does the file it reads through.
///
/// Dropping `/run` from the readable set removes `/etc/resolv.conf` with it on a systemd host, because
/// that file is a symlink to `/run/systemd/resolve/stub-resolv.conf`. The named resolver grants are what
/// keeps a clone, an install or a fetch from failing with a DNS error that looks like a broken network.
#[test]
fn name_lookup_still_works_in_the_chat_cage() {
    let (_base, root, _outside) = scratch("dns");
    let scratch_dir = root.join("tmp");
    std::fs::create_dir_all(&scratch_dir).expect("a scratch");
    let Some(python) = python() else {
        println!("skipping: no system python3 for the caged side of this test");
        return;
    };
    let program = format!(
        "{python} -c 'import socket;print(\"ADDR\", socket.getaddrinfo(\"example.com\", 80)[0][4][0])'"
    );
    // The guard: if this host cannot resolve the name without a cage, the cage is not what failed.
    let open = harness::command::run_within(
        &program,
        &root,
        8192,
        None,
        Some(std::time::Duration::from_secs(20)),
    )
    .expect("a run");
    if !open.passed() || !open.output.contains("ADDR") {
        println!(
            "skipping: this host cannot resolve example.com without a cage: {}",
            open.output
        );
        return;
    }

    let cage = Cage::chat(&root, &scratch_dir).with_runtimes();
    let caged = harness::command::run_within(
        &program,
        &root,
        8192,
        Some(&cage),
        Some(std::time::Duration::from_secs(20)),
    )
    .expect("a run");
    assert!(
        caged.passed() && caged.output.contains("ADDR"),
        "name lookup broke inside the chat cage: {}",
        caged.output
    );
    let resolv = harness::command::run_within(
        "cat /etc/resolv.conf",
        &root,
        8192,
        Some(&cage),
        Some(std::time::Duration::from_secs(10)),
    )
    .expect("a run");
    assert!(
        resolv.passed(),
        "the resolver's own file is unreadable inside the chat cage: {}",
        resolv.output
    );
}

/// An interpreter a caged program can start.
///
/// The `python3` this machine's `PATH` carries is a virtual environment under the home directory, and the
/// cage denies the home by design, so a test that used it would fail with a missing interpreter and prove
/// nothing about the cage. The system interpreter under `/usr` is the one a caged program has.
fn python() -> Option<&'static str> {
    ["/usr/bin/python3", "/usr/bin/python"]
        .into_iter()
        .find(|path| Path::new(path).is_file())
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

/// A caged command gets a cleared environment, but `/proc/<parent>/environ` is a file the cage grants.
/// Sealing the process is what keeps that read from reaching the parent's keys.
///
/// Yama decides whether the read was ever possible: with `ptrace_scope` above 0 a child may not read its
/// parent's entry, so on such a host the seal is a second lock rather than the only one. The test asserts
/// the mechanism either way, and asserts the observable difference where the kernel allows it.
#[test]
fn a_caged_child_cannot_read_the_parents_environment() {
    // Putting the process back is not optional: a panic between seal and unseal would leave the rest of
    // the suite non-dumpable.
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = harness::cage::unseal_environment();
        }
    }

    let root = scratch_base().join(format!("seal-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("a root");
    let cage = Cage::around(&root);
    // `PATH` is in the environment the process was started with, and is not a variable this test sets:
    // `/proc/<pid>/environ` reports the environment at exec, so a value set later would not be there.
    let program = "tr '\\0' '\\n' < /proc/$PPID/environ | grep -c '^PATH='";
    let read = || {
        harness::command::run_within(program, &root, 8192, Some(&cage), None)
            .expect("run")
            .output
    };

    assert!(
        !harness::cage::is_sealed(),
        "the test process was already sealed before the test could prove the change"
    );
    let before = read();
    let readable_before = before.lines().any(|line| line.trim() == "1");

    let _restore = Restore;
    harness::cage::seal_environment().expect("seal");
    assert!(harness::cage::is_sealed(), "the seal did not take");

    let after = read();
    assert!(
        !after.lines().any(|line| line.trim() == "1"),
        "a caged child read the parent's environment after sealing: {}",
        after
    );

    if readable_before {
        // This host allows the read, so the difference above is the proof.
    } else {
        // This host's Yama setting already denied it; the seal is the portable half.
        assert!(
            before.to_ascii_lowercase().contains("denied")
                || before.lines().any(|line| line.trim() == "0"),
            "the read failed for a reason this test does not understand: {}",
            before
        );
    }

    let _ = std::fs::remove_dir_all(&root);
}
