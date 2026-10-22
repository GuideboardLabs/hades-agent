//! Running one program, and reading it back.
//!
//! One implementation, because the tool and the checker both need it and two copies of "run a command"
//! would end up disagreeing about exit codes, which is the kind of disagreement that shows up as a pass.
//!
//! A cage is a parameter rather than a second runner. The tool passes one; the checker does not, because the
//! check is the caller's own command rather than the model's program, and a check that cannot reach its own
//! toolchain is not a check. A cage that cannot be built stops the run: the program does not get to run
//! uncaged because the cage had a bad day.
//!
//! The output is truncated here rather than at the point of use: a model-written program that prints a
//! hundred megabytes must not be able to spend the context budget by accident, and the honest way to cut
//! it is to keep the head and the tail and *say* how much was left out.

use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::Command as Process;

use crate::cage::Cage;
use crate::fault::Fault;

/// What a program did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    /// The exit status, or `-1` when the process was ended by a signal and there is no status to read.
    pub status: i32,
    pub output: String,
    /// Whether the output was cut, so a reader never has to guess whether it is complete.
    pub truncated: bool,
    /// Whether the wall-clock deadline ended the program. A killed run has no verdict about the work.
    pub timed_out: bool,
}

impl Run {
    /// A run passed only if it exited zero *and* was not killed on the deadline. The status alone is not
    /// enough: a shell can exit 0 while a background child holds the output open, and the command that was
    /// asked for never finished. `status` is normalised to -1 in that case, but the second half of the test
    /// is what the contract says.
    pub fn passed(&self) -> bool {
        self.status == 0 && !self.timed_out
    }
}

/// Run `program` through `sh -c` in `root`, keeping at most `limit` bytes of output, confined by `cage`
/// when there is one.
pub fn run(program: &str, root: &Path, limit: usize, cage: Option<&Cage>) -> Result<Run, Fault> {
    run_inner(program, root, limit, cage, None, false, None)
}

/// Run a *model's* program with an environment it cannot read secrets out of.
///
/// The harness process holds provider keys, and a child inherits them unless something says otherwise.
/// A model that runs `env` would then read a key it was never given, which is a read primitive the read
/// boundary does not cover. The passlist is deliberate: a toolchain location is not a credential.
pub fn run_clean(
    program: &str,
    root: &Path,
    limit: usize,
    cage: Option<&Cage>,
) -> Result<Run, Fault> {
    run_clean_in(program, root, limit, cage, None)
}

/// [`run_clean`] with a temporary directory the program may actually write in.
///
/// A caged program that calls `tempfile` or writes `$TMPDIR/x` reads the variable the harness inherited,
/// which points into the live install and is denied by the cage; the error then reads like a broken
/// toolchain. The directory is made here, named in the environment, and expected to be granted writable by
/// the caller's cage, which is the only arrangement in which the three agree.
pub fn run_clean_in(
    program: &str,
    root: &Path,
    limit: usize,
    cage: Option<&Cage>,
    tmp: Option<&Path>,
) -> Result<Run, Fault> {
    run_inner(program, root, limit, cage, None, true, tmp)
}

/// What a model-run program may see. Paths and locale only; no `*_KEY`, `*_TOKEN`, `*_SECRET`, and no
/// `HADES_*`, because those name the harness' own configuration.
pub fn clean_environment() -> Vec<(String, String)> {
    let mut kept = Vec::new();
    for key in [
        "PATH",
        "HOME",
        "TMPDIR",
        "LANG",
        "LC_ALL",
        "TZ",
        "TERM",
        // The toolchain locations a build host needs. A path is not a credential.
        "CARGO_HOME",
        "RUSTUP_HOME",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
    ] {
        if let Some(value) = std::env::var_os(key) {
            kept.push((key.to_string(), value.to_string_lossy().into_owned()));
        }
    }
    kept
}

/// [`run`] with a wall-clock deadline. A program that outlives it is killed and the result says so, with
/// status `-1`: a caller that must not hang (a goal check on the only heartbeat thread) needs a bound
/// that ends, and a bound the child can outlive is not one.
///
/// The two pipes are drained on their own threads while this one polls. Reading them here would block
/// until the child closed them, which is exactly the wait the deadline exists to end.
pub fn run_within(
    program: &str,
    root: &Path,
    limit: usize,
    cage: Option<&Cage>,
    deadline: Option<std::time::Duration>,
) -> Result<Run, Fault> {
    run_inner(program, root, limit, cage, deadline, false, None)
}

fn run_inner(
    program: &str,
    root: &Path,
    limit: usize,
    cage: Option<&Cage>,
    deadline: Option<std::time::Duration>,
    clean: bool,
    tmp: Option<&Path>,
) -> Result<Run, Fault> {
    let plan = cage.map(|cage| cage.plan()).transpose()?;
    let mut command = Process::new("sh");
    command
        .arg("-c")
        .arg(program)
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // Its own process group, so a deadline can end everything the program started rather than only the
    // shell. `kill(-pid)` reaches a grandchild that a `kill(pid)` leaves running, and a runaway test
    // runner is exactly the case this exists for.
    command.process_group(0);
    if clean {
        command.env_clear();
        for (key, value) in clean_environment() {
            command.env(key, value);
        }
    }
    // After the passlist, so it is the last word: a model's program must find a temp directory it can
    // write in, and the one this process inherited is inside the live install that the cage denies.
    if let Some(tmp) = tmp {
        std::fs::create_dir_all(tmp).map_err(|error| {
            Fault::harness(format!("could not make {}: {error}", tmp.display()))
        })?;
        command.env("TMPDIR", tmp).env("TMP", tmp).env("TEMP", tmp);
    }

    if let Some(plan) = plan {
        // SAFETY: the hook runs in the child between fork and exec, so it must not allocate, lock or print,
        // and it does none of those: the plan was built in the parent, and the hook is system calls and an
        // error built from a number. A failure here fails the spawn instead of running the program
        // unconfined, which is the only safe direction for this to fail in.
        unsafe {
            command.pre_exec(move || plan.confine());
        }
    }

    let mut child = command
        .spawn()
        .map_err(|error| Fault::harness(format!("could not run the program: {error}")))?;
    let child_pid = child.id();
    let out_pipe = child.stdout.take();
    let err_pipe = child.stderr.take();
    // Half the budget per pipe, so the two together cannot exceed the limit and the final text needs no
    // second cut. The margin covers the two omission markers.
    let budget = (limit / 2).saturating_sub(64).max(64);
    // Each reader reports on a channel rather than through `join`, because `join` has no timeout: a
    // background child that inherited a pipe keeps it open after the shell exits, and a blocking join
    // would wait past the deadline the caller asked for.
    use std::os::unix::io::IntoRawFd as _;
    let out_fd = out_pipe.map(std::process::ChildStdout::into_raw_fd);
    let err_fd = err_pipe.map(std::process::ChildStderr::into_raw_fd);
    for descriptor in [out_fd, err_fd].into_iter().flatten() {
        set_nonblocking(descriptor);
    }

    let (sender, reader) = std::sync::mpsc::channel();
    // A reader that is blocked on a pipe cannot be cancelled, and a process that escaped the group can hold
    // that pipe open for as long as it likes. The descriptors are non-blocking and polled, so a cancellation
    // ends the thread within one poll interval instead of leaving it behind in a long-lived process.
    let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let out_sender = sender.clone();
    let out_cancel = std::sync::Arc::clone(&cancelled);
    let out_reader = std::thread::spawn(move || {
        let captured = read_fd(out_fd, budget, out_cancel);
        let _ = out_sender.send((false, captured));
    });
    let err_cancel = std::sync::Arc::clone(&cancelled);
    let err_reader = std::thread::spawn(move || {
        let captured = read_fd(err_fd, budget, err_cancel);
        let _ = sender.send((true, captured));
    });

    let started = std::time::Instant::now();
    let mut timed_out = false;
    let mut status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code().unwrap_or(-1),
            Ok(None) => {}
            Err(error) => {
                end_group(&mut child, child_pid);
                return Err(Fault::harness(format!(
                    "could not read the program's status: {error}"
                )));
            }
        }
        if deadline.is_some_and(|limit| started.elapsed() >= limit) {
            timed_out = true;
            cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
            end_group(&mut child, child_pid);
            break -1;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    };

    let mut out = Captured::default();
    let mut err = Captured::default();
    if !timed_out {
        // The shell exiting is not the end of the command's work: a background child may still hold
        // stdout or stderr. The deadline therefore covers the streams too, and when it passes the group
        // is killed and no output is claimed - what was written before the kill is not what it said.
        let mut received = 0;
        while received < 2 {
            let waited = match deadline {
                Some(limit) => {
                    let left = limit.saturating_sub(started.elapsed());
                    reader.recv_timeout(left)
                }
                None => reader
                    .recv()
                    .map_err(|_| std::sync::mpsc::RecvTimeoutError::Disconnected),
            };
            match waited {
                Ok((is_err, captured)) => {
                    if is_err {
                        err = captured;
                    } else {
                        out = captured;
                    }
                    received += 1;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    // The shell said it was done; the command was not. Recording its zero here would let a
                    // `Run` claim success and a deadline at the same time.
                    timed_out = true;
                    status = -1;
                    cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
                    end_group(&mut child, child_pid);
                    break;
                }
            }
        }
        if timed_out {
            drop(out_reader);
            drop(err_reader);
            out = Captured::default();
            err = Captured::default();
        }
    } else {
        // The group was killed, but a process that escaped it can still hold a pipe open; waiting for one
        // would undo the deadline. The readers are left to finish on their own and no output is claimed.
        drop(out_reader);
        drop(err_reader);
    }
    let mut text = render(&out);
    let stderr = render(&err);
    if timed_out {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&format!(
            "killed after {} ms: the check has a wall-clock deadline",
            started.elapsed().as_millis()
        ));
    }
    if !stderr.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&stderr);
    }

    let (output, truncated) = cap_to(text, limit, 0);
    Ok(Run {
        status,
        output,
        truncated: truncated || out.omitted > 0 || err.omitted > 0,
        timed_out,
    })
}

/// The readable form of one pipe: the head, the tail, and the marker between them when bytes were dropped.
///
/// The marker is inserted here rather than at the end because joining a head to a tail with nothing
/// between them invents an adjacency that never existed: two halves of a number become one number.
fn render(captured: &Captured) -> String {
    let head = String::from_utf8_lossy(&captured.head);
    let tail = String::from_utf8_lossy(&captured.tail);
    if captured.omitted == 0 {
        format!("{head}{tail}")
    } else {
        format!("{head}\n… {} bytes omitted …\n{tail}", captured.omitted)
    }
}

/// End the child and everything it started.
///
/// A signal to the negated pid reaches the whole group, which is why the child was spawned into its own.
/// If the group signal cannot be sent (the child already reaped, a host without process groups), the
/// single-process kill is the fallback rather than nothing.
fn end_group(child: &mut std::process::Child, pid: u32) {
    let sent = unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    if sent != 0 {
        let _ = child.kill();
    }
    let _ = child.wait();
}

/// What one pipe contributed: the head, the tail, and how many bytes were dropped between them.
///
/// The point is the order of the cut. Collecting the stream and truncating it afterwards means a program
/// that prints two gigabytes has already allocated two gigabytes, so the cap has to run while the bytes
/// are being read, and the count has to be of the *stream* or the marker lies about what was lost.
#[derive(Debug, Default)]
struct Captured {
    head: Vec<u8>,
    tail: Vec<u8>,
    omitted: usize,
}

/// Everything a child wrote to one pipe, bounded by `limit` while it is read.
/// Poll rather than block, so a cancellation is noticed even when no byte ever arrives.
fn set_nonblocking(descriptor: i32) {
    // SAFETY: `fcntl` on a descriptor this call owns the number of, with the flags it just reported.
    unsafe {
        let flags = libc::fcntl(descriptor, libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}

fn read_fd(
    descriptor: Option<i32>,
    limit: usize,
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Captured {
    use std::os::unix::io::FromRawFd as _;
    let mut captured = Captured::default();
    let Some(descriptor) = descriptor else {
        return captured;
    };
    // Own the descriptor, so it closes when this reader ends and a cancelled read cannot leak it.
    // SAFETY: the descriptor came from the child's pipe and is transferred to this `File` once.
    let mut pipe = unsafe { std::fs::File::from_raw_fd(descriptor) };
    let head_limit = limit / 2;
    let tail_limit = limit - head_limit;
    let mut chunk = [0u8; 8192];
    while !cancelled.load(std::sync::atomic::Ordering::Relaxed) {
        let mut polled = libc::pollfd {
            fd: descriptor,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one descriptor, no timeout pointer, a positive count.
        let ready = unsafe { libc::poll(&mut polled, 1, 50) };
        if ready < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if ready == 0 {
            continue;
        }
        match std::io::Read::read(&mut pipe, &mut chunk) {
            Ok(0) => break,
            Ok(read) => {
                let mut bytes = &chunk[..read];
                if captured.head.len() < head_limit {
                    let room = head_limit - captured.head.len();
                    let take = room.min(bytes.len());
                    captured.head.extend_from_slice(&bytes[..take]);
                    bytes = &bytes[take..];
                }
                if !bytes.is_empty() {
                    // Past the head, bytes go to a rolling tail: push, and drop from the front once the
                    // tail is full, counting each dropped byte as omitted.
                    captured.tail.extend_from_slice(bytes);
                    if captured.tail.len() > tail_limit {
                        let excess = captured.tail.len() - tail_limit;
                        captured.tail.drain(..excess);
                        captured.omitted += excess;
                    }
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
                ) =>
            {
                continue;
            }
            Err(_) => break,
        }
    }
    captured
}

/// Cut `text` to `limit` bytes, counting nothing as already dropped. Test-only: the running reader counts
/// what it drops, and this is the shape a caller with the whole string would have.
#[cfg(test)]
fn truncate(text: &str, limit: usize) -> (String, bool) {
    cap_to(text.to_string(), limit, 0)
}

/// Cut `text` to `limit` bytes, adding `seen_omitted` bytes that were already dropped while reading.
fn cap_to(text: String, limit: usize, seen_omitted: usize) -> (String, bool) {
    if text.len() <= limit && seen_omitted == 0 {
        return (text, false);
    }
    if text.len() <= limit {
        // Everything that survived fits; only the already-dropped bytes need saying.
        return (format!("{text}\n… {seen_omitted} bytes omitted …"), true);
    }

    let head = limit * 2 / 3;
    let tail = limit - head;
    let cut = floor_boundary(&text, head);
    let back = ceil_boundary(&text, text.len().saturating_sub(tail)).max(cut);
    let omitted = seen_omitted + text.len() - cut - (text.len() - back);

    (
        format!(
            "{}\n… {omitted} bytes omitted …\n{}",
            &text[..cut],
            &text[back..]
        ),
        true,
    )
}

fn floor_boundary(text: &str, index: usize) -> usize {
    let mut at = index.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

fn ceil_boundary(text: &str, index: usize) -> usize {
    let mut at = index.min(text.len());
    while !text.is_char_boundary(at) {
        at += 1;
    }
    at
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn here() -> PathBuf {
        std::env::current_dir().expect("a working directory")
    }

    #[test]
    fn a_program_that_prints_is_read() {
        let run = run("echo hello", &here(), 1024, None).expect("run");
        assert!(run.passed(), "{run:?}");
        assert_eq!(run.output.trim(), "hello");
        assert!(!run.truncated);
    }

    #[test]
    fn a_program_that_fails_keeps_its_status() {
        let run = run("exit 3", &here(), 1024, None).expect("run");
        assert_eq!(run.status, 3);
    }

    #[test]
    fn a_program_that_is_not_there_is_a_real_run_with_a_status() {
        // `sh` answered, and the shell said 127. That is a failed run, not a harness fault: the difference
        // matters, because one is the model's program and the other is our plumbing.
        let run = run("definitely-not-a-command-anywhere", &here(), 1024, None).expect("run");
        assert_eq!(run.status, 127);
        assert!(!run.output.is_empty());
    }

    /// The shell exits at once, but the background child keeps stdout open. The deadline has to cover
    /// the streams, or the call hangs until that child dies - the unattended-worker hang.
    #[test]
    fn a_background_child_cannot_hold_the_deadline_open() {
        let started = std::time::Instant::now();
        let run = run_within(
            "sleep 300 &",
            &here(),
            1024,
            None,
            Some(std::time::Duration::from_secs(2)),
        )
        .expect("run");
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(6),
            "the call waited {elapsed:?} for a child the shell left behind"
        );
        assert!(run.timed_out, "{run:?}");
        assert!(run.output.contains("wall-clock deadline"), "{}", run.output);
        // The shell exited 0; the command did not finish. A run cannot claim both.
        assert_eq!(
            run.status, -1,
            "a timed-out run kept the shell's exit status"
        );
        assert!(!run.passed(), "a timed-out run reported that it passed");
    }

    #[test]
    fn a_deadline_marks_the_run_timed_out() {
        // `sleep 5` cannot finish inside 200 ms. The run must say it was killed, and its status must not be
        // mistaken for the program's own answer.
        let run = run_within(
            "sleep 5",
            &here(),
            1024,
            None,
            Some(std::time::Duration::from_millis(200)),
        )
        .expect("run");
        assert!(run.timed_out, "{run:?}");
        assert_eq!(run.status, -1);
        assert!(run.output.contains("wall-clock deadline"), "{}", run.output);
    }

    #[test]
    fn a_huge_stream_is_capped_before_it_is_joined() {
        // Two megabytes through a 2000-byte limit. The cut has to happen while reading, and the omitted
        // count has to describe the stream rather than the bytes that survived the first cut.
        let root = crate::scratch::root().join("bounded-output");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("a root");
        let run = run_within(
            "yes x | head -c 2000000",
            &root,
            2000,
            None,
            Some(std::time::Duration::from_secs(20)),
        )
        .expect("a run");
        assert!(run.output.len() < 4000, "{}", run.output.len());
        assert!(
            run.output.contains("omitted"),
            "{}",
            &run.output[..200.min(run.output.len())]
        );
        assert!(run.truncated);
        let counted: usize = run
            .output
            .split('…')
            .nth(1)
            .and_then(|part| part.split_whitespace().next())
            .and_then(|number| number.parse().ok())
            .unwrap_or(0);
        assert!(
            counted > 1_900_000,
            "the marker undercounts the stream: {counted}"
        );
    }

    #[test]
    fn a_timeout_kills_the_grandchild_too() {
        // The program starts a child and waits. Killing only `sh` leaves `sleep 60` holding the pipes and
        // the work; the group signal is what makes "stopped" mean stopped.
        let root = crate::scratch::root().join("process-group");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("a root");
        let run = run_within(
            "sleep 60 & echo $! > child.pid; wait",
            &root,
            2000,
            None,
            Some(std::time::Duration::from_millis(300)),
        )
        .expect("a run");
        assert!(run.timed_out);
        let pid: i32 = std::fs::read_to_string(root.join("child.pid"))
            .expect("the child pid")
            .trim()
            .parse()
            .expect("a pid");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            let alive = std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .status()
                .map(|status| status.success())
                .unwrap_or(false);
            if !alive {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the grandchild still exists: {pid}"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    #[test]
    fn a_caged_program_has_a_writable_tmpdir() {
        // `tempfile` and every `$TMPDIR` write read the variable the harness inherited, which points into
        // the live install and is denied by the cage. Setting it is half the fix; the cage has to grant the
        // same directory, or the program gets a path it still cannot write.
        let state = crate::scratch::root().join("tmpdir-cage");
        let _ = std::fs::remove_dir_all(&state);
        let work = state.join("work");
        let scratch = state.join("tmp");
        std::fs::create_dir_all(&work).expect("a work root");
        std::fs::create_dir_all(&scratch).expect("a scratch");
        let cage = Cage::chat(&work, &scratch);
        let run = run_clean_in(
            "printf '%s\\n' \"$TMPDIR\"; touch \"$TMPDIR/probe\" && echo wrote",
            &work,
            4000,
            Some(&cage),
            Some(&scratch),
        )
        .expect("a run");
        assert!(
            run.output.contains(scratch.to_str().expect("utf8")),
            "{}",
            run.output
        );
        assert!(run.output.contains("wrote"), "{}", run.output);
        assert!(scratch.join("probe").exists(), "the file did not land");

        // The twin: without the variable the same write lands in a denied path, which is the failure this
        // exists to stop a model from diagnosing as a broken toolchain.
        let bare = run_clean_in(
            "touch \"$TMPDIR/probe2\" && echo wrote",
            &work,
            4000,
            Some(&cage),
            None,
        )
        .expect("a run");
        assert!(!bare.output.contains("wrote"), "{}", bare.output);
    }

    #[test]
    fn output_is_cut_at_both_ends_and_says_so() {
        let run = run("seq 1 4000", &here(), 200, None).expect("run");
        assert!(run.truncated);
        assert!(run.output.contains("bytes omitted"), "{}", run.output);
        assert!(run.output.starts_with('1'), "{}", run.output);
        assert!(run.output.trim_end().ends_with("4000"), "{}", run.output);
    }

    #[test]
    fn cutting_does_not_split_a_character() {
        // A byte limit landing inside a multi-byte character must not panic, and must not corrupt one into
        // mojibake. The only non-ASCII characters that may survive are the ones that went in.
        let text = "é".repeat(500);
        let (cut, truncated) = truncate(&text, 101);
        assert!(truncated);
        assert!(cut.contains("bytes omitted"));
        assert!(
            cut.chars().all(|c| c == 'é' || c == '…' || c.is_ascii()),
            "{cut:?}"
        );
        assert!(!cut.contains('\u{fffd}'), "{cut:?}");
    }

    // A model-run shell sees only the passlist, even when the harness holds a key.
    use std::sync::Mutex;

    static ENV: Mutex<()> = Mutex::new(());

    #[test]
    fn a_model_shell_cannot_read_a_key_the_harness_holds() {
        let _guard = ENV.lock().unwrap_or_else(|poison| poison.into_inner());
        // Planted in this process, exactly like a provider key the harness was started with.
        unsafe {
            std::env::set_var("HADES_TEST_SECRET", "sk-do-not-leak");
            std::env::set_var("OPENAI_API_KEY", "sk-do-not-leak");
            std::env::set_var("HADES_PROXY", "http://127.0.0.1:9");
        }
        let root = crate::scratch::root().join("clean-env");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("a scratch root");

        let clean = run_clean("env", &root, 100_000, None).expect("a clean run");
        assert!(!clean.output.contains("sk-do-not-leak"), "{}", clean.output);
        for name in ["HADES_TEST_SECRET", "OPENAI_API_KEY", "HADES_PROXY"] {
            assert!(
                !clean.output.contains(name),
                "{name} reached the model's shell"
            );
        }
        assert!(
            clean.output.contains("PATH="),
            "a shell still needs PATH: {}",
            clean.output
        );

        // The contrast is the point: the same command inherits the key without `run_clean`.
        let inherited = run("env", &root, 100_000, None).expect("an inherited run");
        assert!(inherited.output.contains("sk-do-not-leak"));

        unsafe {
            std::env::remove_var("HADES_TEST_SECRET");
            std::env::remove_var("OPENAI_API_KEY");
            std::env::remove_var("HADES_PROXY");
        }
    }
}
