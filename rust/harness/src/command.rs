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
}

impl Run {
    pub fn passed(&self) -> bool {
        self.status == 0
    }
}

/// Run `program` through `sh -c` in `root`, keeping at most `limit` bytes of output, confined by `cage`
/// when there is one.
pub fn run(program: &str, root: &Path, limit: usize, cage: Option<&Cage>) -> Result<Run, Fault> {
    run_within(program, root, limit, cage, None)
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
    let plan = cage.map(|cage| cage.plan()).transpose()?;
    let mut command = Process::new("sh");
    command
        .arg("-c")
        .arg(program)
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

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
    let out_pipe = child.stdout.take();
    let err_pipe = child.stderr.take();
    let out_reader = std::thread::spawn(move || read_pipe(out_pipe));
    let err_reader = std::thread::spawn(move || read_pipe(err_pipe));

    let started = std::time::Instant::now();
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code().unwrap_or(-1),
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Fault::harness(format!(
                    "could not read the program's status: {error}"
                )));
            }
        }
        if deadline.is_some_and(|limit| started.elapsed() >= limit) {
            timed_out = true;
            let _ = child.kill();
            let _ = child.wait();
            break -1;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    };

    let (out_bytes, err_bytes) = if timed_out {
        // The shell was killed, but a grandchild it started can hold the pipes open; waiting for them
        // would undo the deadline. The readers are left to finish on their own and no output is
        // claimed, which is the honest report: what was written before the kill is not what it said.
        drop(out_reader);
        drop(err_reader);
        (Vec::new(), Vec::new())
    } else {
        (
            out_reader.join().unwrap_or_default(),
            err_reader.join().unwrap_or_default(),
        )
    };
    let mut text = String::from_utf8_lossy(&out_bytes).into_owned();
    let stderr = String::from_utf8_lossy(&err_bytes);
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

    let (output, truncated) = truncate(&text, limit);
    Ok(Run {
        status,
        output,
        truncated,
    })
}

/// Everything a child wrote to one pipe, or nothing when it had none.
fn read_pipe<R: std::io::Read>(pipe: Option<R>) -> Vec<u8> {
    let mut bytes = Vec::new();
    if let Some(mut pipe) = pipe {
        let _ = pipe.read_to_end(&mut bytes);
    }
    bytes
}

/// Keep the head and the tail, and say what was left out. Both ends, because a failure usually announces
/// itself at one end and explains itself at the other.
fn truncate(text: &str, limit: usize) -> (String, bool) {
    if text.len() <= limit {
        return (text.to_string(), false);
    }

    let head = limit * 2 / 3;
    let tail = limit - head;
    let cut = floor_boundary(text, head);
    let back = ceil_boundary(text, text.len() - tail);
    let omitted = text.len() - cut - (text.len() - back);

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
}
