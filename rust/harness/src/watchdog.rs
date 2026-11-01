//! The watchdog: a run that is still alive and no longer productive is stopped, and the reason is written
//! down. Report, never repair.
//!
//! Hades already refuses to replay a crash: a run that stopped between appending a call and appending its
//! result is reported as interrupted and a person decides. This is the other shape of the same problem.
//! A run can be perfectly alive, hold its lease, keep calling the model, and make no progress at all,
//! because it is issuing the same call over and over or reading without ever writing. The bound on
//! attempts does not see it, the token bound does not see it, and the person watching sees a status line
//! that moves.
//!
//! The judgement is a pure function over a step history, so it is testable without a daemon, a socket or a
//! clock, and so the same history always gives the same answer. The call it makes is `agent.rs`'s, reused
//! rather than copied: the repeat guard and the watchdog must agree on what "the same call" means, or one
//! of them is blind to a command rephrased only in its tail.

use session_tree::{Entry, Kind};

/// What a step did with the tree, as far as the log can tell.
///
/// The daemon records every step of a run under the tool it used, and `turn` uses one tool for a program:
/// `shell`. So the tool's *name* says nothing about what a step looked at or wrote, and a rule that judged
/// by name was blind to every shell read in a real run. This is the fact the rule needs, decided from the
/// tool and its own arguments together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Did {
    /// It looked at something: a file read, a search, a listing, a program that only reports.
    Read,
    /// It created or changed a file: an edit that landed, or a program that sends its output into one.
    Write,
    /// Neither, as far as the log can tell: a program that runs work, or a tool nobody classified.
    Nothing,
}

/// One step of a run, as the session log recorded it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// The tool that ran.
    pub tool: String,
    /// What it was asked to do: a program's text, an edit's arguments, the raw arguments of anything else.
    pub arguments: String,
    /// Whether this step created or modified a file, looked at something, or neither. A program is read for
    /// its own words: a shell program that writes through a redirect is counted, because the redirect is in
    /// the text, while a program that runs a build is not, because the log cannot see what it touched.
    pub did: Did,
    /// The tokens the step spent, prompt and generated together. Zero when the log did not record them.
    pub tokens: u64,
}

/// What the watchdog found, when it found something.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stall {
    /// The same normalised call, N times in a row. `tool` is the tool and the call as one key, so the
    /// sentence it renders reads `repeating shell: cargo test, 5 times`.
    Repeating { tool: String, times: usize },
    /// K steps in a row that neither wrote a file nor looked at anything.
    NoProgress { steps: usize },
    /// The allowance is gone and nothing was written.
    SpentWithoutOutput { tokens: u64 },
}

impl Stall {
    /// One sentence, for the ledger and for the person who reads it. A stop nobody can explain after the
    /// fact is indistinguishable from a crash.
    pub fn sentence(&self) -> String {
        match self {
            Stall::Repeating { tool, times } => format!("repeating {tool}, {times} times"),
            Stall::NoProgress { steps } => format!("{steps} steps with nothing written or read"),
            Stall::SpentWithoutOutput { tokens } => {
                format!("{tokens} tokens spent with nothing written")
            }
        }
    }
}

/// When a run has stopped being productive. The defaults are deliberately loose: a legitimate run does
/// read a dozen files in a row, and a false stop costs more than a slow run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// The same normalised call, this many times in a row.
    pub repeats: usize,
    /// This many steps in a row with no write and nothing read.
    pub no_progress: usize,
    /// The run's token allowance, when the caller declared one.
    pub allowance: Option<u64>,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            repeats: 5,
            no_progress: 12,
            allowance: None,
        }
    }
}

/// The verdict on a step history, or nothing when nothing is wrong.
///
/// Only the tail is examined: the question is whether the run is stalled *now*, not whether it ever
/// repeated itself. A loop that a write interrupted and never resumed is a run that recovered.
pub fn inspect(steps: &[Step], limits: &Limits) -> Option<Stall> {
    let last = steps.last()?;

    let key = crate::agent::call_key(&last.tool, &last.arguments);
    let times = steps
        .iter()
        .rev()
        .take_while(|step| crate::agent::call_key(&step.tool, &step.arguments) == key)
        .count();
    if times >= limits.repeats {
        return Some(Stall::Repeating { tool: key, times });
    }

    // A loop that has also spent its allowance is a loop: the more specific finding is the useful one.
    let spent: u64 = steps.iter().map(|step| step.tokens).sum();
    if let Some(allowance) = limits.allowance
        && spent >= allowance
        && steps.iter().all(|step| step.did != Did::Write)
    {
        return Some(Stall::SpentWithoutOutput { tokens: spent });
    }

    // A step stands still when it neither wrote nor looked at anything. Reading is how a run rebuilds its
    // working set after a trim, so counting reads as standing still would stop the runs that are recovering.
    // "Reading" is a fact about the step, not the name of its tool: a daemon run logs every program as
    // `shell`, and twelve `cat`/`grep` steps are one long look, not a stall.
    let still = steps
        .iter()
        .rev()
        .take_while(|step| step.did == Did::Nothing)
        .count();
    if still >= limits.no_progress {
        return Some(Stall::NoProgress { steps: still });
    }

    None
}

/// The step a tool call records: what ran, what it was asked, what it did, and what it cost.
///
/// Classification happens here, where the tool's name and its arguments are both in hand. The name alone is
/// not enough for the reason this module exists for: `turn` logs a program as `shell`, so the name of a
/// `grep` step and the name of a `cargo build` step are the same word.
fn classify(tool: &str, arguments: &str) -> Did {
    match tool {
        // The tools whose name is the whole claim. An edit is a write when it lands, and the result's own
        // status is what says so: a refused edit changed nothing, and `steps` corrects this.
        "patch" | "write_file" | "edit" => Did::Write,
        "read_file" | "list_dir" | "search" | "grep" | "skills" | "recall" | "session_search"
        | "fetch" | "library" | "image" => Did::Read,
        crate::SHELL => program(arguments),
        _ => Did::Nothing,
    }
}

/// The programs that only report: a command whose every segment is one of these looked at something and
/// changed nothing.
///
/// `cargo` and `rustc` are deliberately absent: they run work, and a run that builds twelve times without
/// writing is exactly the shape this rule exists to find. `git` is absent too, because the same word logs
/// and commits, and a name that cannot tell those apart is the defect this classifier replaced.
const REPORTERS: &[&str] = &[
    "cat",
    "bat",
    "tac",
    "head",
    "tail",
    "less",
    "more",
    "grep",
    "egrep",
    "fgrep",
    "rg",
    "ack",
    "ag",
    "ls",
    "tree",
    "stat",
    "wc",
    "file",
    "find",
    "fd",
    "jq",
    "yq",
    "sort",
    "uniq",
    "cut",
    "tr",
    "nl",
    "comm",
    "diff",
    "cmp",
    "column",
    "pwd",
    "echo",
    "printf",
    "which",
    "type",
    "env",
    "printenv",
    "md5sum",
    "sha256sum",
    "sha1sum",
    "realpath",
    "readlink",
    "basename",
    "dirname",
    "date",
    "id",
    "whoami",
    "ps",
    "ss",
    "df",
    "du",
    "uname",
    "nproc",
    "free",
    "uptime",
    "sed",
    "awk",
    "test",
];

/// What a program did, from its own words.
///
/// Three questions, cheapest first: did it send output into a file, did it edit one in place, and does every
/// command in it only report? A program that fails all three ran something, and the log cannot say what it
/// touched, so it counts as neither.
fn program(text: &str) -> Did {
    if !crate::floor::redirections(text).is_empty() || writes(text) {
        return Did::Write;
    }
    // Every segment has to be a reporter, and a program with no command in it at all is not a read: an
    // empty call is a step that looked at nothing.
    let segments = crate::floor::segments(text);
    if segments.is_empty() {
        return Did::Nothing;
    }
    for segment in segments {
        let words: Vec<&str> = segment.split_whitespace().collect();
        let Some(word) = crate::floor::command_word(&words) else {
            // A segment with no program in it (`FOO=1` alone) is neutral, not a look.
            continue;
        };
        if !REPORTERS.contains(&word.to_ascii_lowercase().as_str()) {
            return Did::Nothing;
        }
    }
    Did::Read
}

/// The programs that change a file without a redirect to show for it: `sed -i` edits in place, `tee` writes
/// the files it names, and the copy, move, remove and create commands write what they touch.
const WRITERS: &[&str] = &[
    "tee", "cp", "mv", "rm", "rmdir", "touch", "truncate", "dd", "ln", "mkdir", "install",
];

fn writes(text: &str) -> bool {
    crate::floor::segments(text).iter().any(|segment| {
        let words: Vec<&str> = segment.split_whitespace().collect();
        match crate::floor::command_word(&words) {
            Some(word) if matches!(word.to_ascii_lowercase().as_str(), "sed" | "perl") => words
                .iter()
                .skip(1)
                .any(|word| *word == "--in-place" || word.starts_with("-i")),
            // `tee` with no file writes to standard output, which changes nothing on disk.
            Some(word) if word.eq_ignore_ascii_case("tee") => {
                words.iter().skip(1).any(|word| !word.starts_with('-'))
            }
            Some(word) => WRITERS.contains(&word.to_ascii_lowercase().as_str()),
            None => false,
        }
    })
}

/// The steps a session log records, in order. One tool call is one step, and the result recorded beside it
/// says whether an edit landed: a refused edit is a step that changed nothing, which is what the log
/// itself would say if it were asked.
pub fn steps(entries: &[Entry]) -> Vec<Step> {
    let mut steps: Vec<Step> = Vec::new();
    for entry in entries {
        match entry.kind {
            Kind::ToolCall => {
                let tool = entry
                    .payload
                    .get("tool")
                    .and_then(|value| value.as_str())
                    .unwrap_or("")
                    .to_string();
                // A program is the work; an edit names the files it would touch; anything else carries its
                // arguments as they arrived.
                let arguments = entry
                    .payload
                    .get("program")
                    .and_then(|value| value.as_str())
                    .map(str::to_string)
                    .or_else(|| entry.payload.get("edits").map(|edits| edits.to_string()))
                    .or_else(|| {
                        entry
                            .payload
                            .get("arguments")
                            .map(|arguments| arguments.to_string())
                    })
                    .unwrap_or_default();
                let tokens = entry
                    .payload
                    .get("model")
                    .map(|model| {
                        ["prompt_tokens", "generated_tokens"]
                            .iter()
                            .filter_map(|key| model.get(*key).and_then(|value| value.as_u64()))
                            .sum()
                    })
                    .unwrap_or(0);
                let did = classify(&tool, &arguments);
                steps.push(Step {
                    tool,
                    arguments,
                    did,
                    tokens,
                });
            }
            Kind::ToolResult => {
                let Some(step) = steps.last_mut() else {
                    continue;
                };
                let status = entry
                    .payload
                    .get("status")
                    .and_then(|value| value.as_i64())
                    .unwrap_or(1);
                // Only an edit says whether it landed. A program's exit status is not evidence about what
                // it wrote: a `grep` with no match exits 1 and still wrote the file it was redirected into.
                if status != 0 && matches!(step.tool.as_str(), "patch" | "write_file" | "edit") {
                    step.did = Did::Nothing;
                }
            }
            _ => {}
        }
    }
    steps
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use session_tree::{FileStore, Session, SessionId, SystemClock};

    /// A program, classified the way the log classifies one.
    fn shell(command: &str) -> Step {
        Step {
            tool: crate::SHELL.to_string(),
            arguments: command.to_string(),
            did: program(command),
            tokens: 100,
        }
    }

    fn write(path: &str) -> Step {
        Step {
            tool: "write_file".to_string(),
            arguments: path.to_string(),
            did: Did::Write,
            tokens: 100,
        }
    }

    fn limits() -> Limits {
        Limits::default()
    }

    /// A session log with one call and one result per program, in the shape `turn` writes it: the whole
    /// history a daemon run leaves behind, read back the way the watchdog reads it.
    fn logged(programs: &[&str]) -> Vec<Entry> {
        let root = crate::scratch::unique("watchdog-log");
        let mut session =
            Session::create(FileStore::new(&root), SystemClock, SessionId::new("logged"))
                .expect("a session");
        for program in programs {
            session
                .append(Kind::ToolCall, json!({"tool": "shell", "program": program}))
                .expect("a call");
            session
                .append(
                    Kind::ToolResult,
                    json!({"tool": "shell", "status": 1, "output": ""}),
                )
                .expect("a result");
        }
        Session::load(FileStore::new(&root), SystemClock, SessionId::new("logged"))
            .expect("the log")
            .entries()
            .to_vec()
    }

    /// An edit that landed, as `turn` records one: the call names the file, and the result's status says it
    /// was written.
    fn applied(path: &str) -> Vec<Entry> {
        let root = crate::scratch::unique("watchdog-edit");
        let mut session = Session::create(
            FileStore::new(&root),
            SystemClock,
            SessionId::new("applied"),
        )
        .expect("a session");
        session
            .append(
                Kind::ToolCall,
                json!({"tool": "patch", "edits": [{"path": path}]}),
            )
            .expect("an edit");
        session
            .append(
                Kind::ToolResult,
                json!({"tool": "patch", "status": 0, "output": "applied"}),
            )
            .expect("applied");
        Session::load(
            FileStore::new(&root),
            SystemClock,
            SessionId::new("applied"),
        )
        .expect("the log")
        .entries()
        .to_vec()
    }

    /// The defect this exists for: `unittest ... | tail -30` and the same command with `| head -40` are
    /// one command rephrased in its tail, and a guard keyed on the exact text never sees the loop.
    #[test]
    fn a_command_rephrased_only_in_its_tail_five_times_is_repeating() {
        let steps = vec![
            shell("cargo test --quiet | tail -30"),
            shell("cargo test --quiet | tail -20"),
            shell("cargo test --quiet | head -40"),
            shell("cargo test --quiet   >  out.txt"),
            shell("cargo test --quiet"),
        ];
        let stall = inspect(&steps, &limits()).expect("five of the same call");
        match stall {
            Stall::Repeating { tool, times } => {
                assert_eq!(times, 5, "{tool}");
                assert_eq!(tool, "shell: cargo test --quiet");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn five_different_commands_are_not_a_stall() {
        let steps = vec![
            shell("cargo build"),
            shell("cargo test"),
            shell("cat notes.md"),
            shell("cargo run"),
            shell("git status"),
        ];
        assert!(inspect(&steps, &limits()).is_none());
    }

    /// A run that alternates a write with a look is working: the tail of it never holds K steps with
    /// nothing written.
    #[test]
    fn a_run_that_writes_every_other_step_is_not_no_progress() {
        let mut steps = Vec::new();
        for n in 0..20 {
            steps.push(write(&format!("src/file-{n}.rs")));
            steps.push(shell(&format!("cargo test --test case-{n}")));
        }
        assert!(inspect(&steps, &limits()).is_none());
    }

    #[test]
    fn a_run_that_spends_its_allowance_with_zero_writes_is_spent_without_output() {
        let steps: Vec<Step> = (0..4)
            .map(|n| Step {
                tool: "shell".to_string(),
                arguments: format!("cargo test --test case-{n}"),
                did: Did::Nothing,
                tokens: 1_000,
            })
            .collect();
        let mut limits = limits();
        limits.allowance = Some(4_000);
        match inspect(&steps, &limits).expect("an allowance spent with nothing written") {
            Stall::SpentWithoutOutput { tokens } => assert_eq!(tokens, 4_000),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_empty_history_is_not_a_stall() {
        assert!(inspect(&[], &limits()).is_none());
    }

    /// Reading is how a run rebuilds its working set after a trim. Counting reads as standing still would
    /// stop exactly the runs that are recovering.
    ///
    /// The history here is the one a daemon run writes: `turn` records a program under the tool `shell`, so
    /// twelve `cat`/`grep`/`sed` steps in a row all read as `shell` and never as `read_file`. The defect this
    /// exists for: the rule exempted a step by tool *name*, so every one of these was judged "nothing
    /// written" and a run was stopped for reading.
    #[test]
    fn a_run_of_shell_reads_is_not_no_progress() {
        let entries = logged(&[
            "cat src/watchdog.rs",
            "grep -n 'no_progress' src/watchdog.rs",
            "sed -n '100,140p' src/watchdog.rs",
            "head -40 src/daemon.rs",
            "ls -la src",
            "rg watchdog src",
            "wc -l src/watchdog.rs",
            "cat src/daemon.rs",
            "grep -rn lease src/daemon.rs",
            "awk '{print $2}' src/daemon.rs",
            "find src -name '*.rs'",
            "sort src/names.txt",
        ]);
        let steps = steps(&entries);
        assert!(
            inspect(&steps, &limits()).is_none(),
            "a run of reads was stopped for reading: {:?}",
            inspect(&steps, &limits())
        );
    }

    /// The other half of the rule: a step that looked at nothing and wrote nothing is still standing still,
    /// and twelve of them are a stall. Without this, a classifier that calls everything a read would pass the
    /// test above and stop measuring anything.
    #[test]
    fn twelve_shell_steps_that_look_at_nothing_are_no_progress() {
        let entries = logged(&[
            "cargo test --test case-1",
            "cargo test --test case-2",
            "cargo test --test case-3",
            "cargo test --test case-4",
            "cargo test --test case-5",
            "cargo test --test case-6",
            "cargo test --test case-7",
            "cargo test --test case-8",
            "cargo test --test case-9",
            "cargo test --test case-10",
            "cargo test --test case-11",
            "cargo test --test case-12",
        ]);
        let steps = steps(&entries);
        assert!(
            matches!(
                inspect(&steps, &limits()),
                Some(Stall::NoProgress { steps: 12 })
            ),
            "{:?}",
            inspect(&steps, &limits())
        );
    }

    /// A run that reads a file and then edits it is working, and the write is the one a daemon run really
    /// makes: `turn` records an applied edit as a result with status 0.
    #[test]
    fn a_run_that_alternates_a_read_with_an_applied_edit_is_not_no_progress() {
        let mut entries = Vec::new();
        for n in 0..20 {
            entries.extend(logged(&[&format!("grep -n helper-{n} src/lib.rs")]));
            entries.extend(applied(&format!("src/file-{n}.rs")));
        }
        let steps = steps(&entries);
        assert!(
            inspect(&steps, &limits()).is_none(),
            "{:?}",
            inspect(&steps, &limits())
        );
    }

    /// A program that sends its output into a file wrote that file, even though the log cannot say which
    /// paths it touched: the redirect is the evidence, and it is in the program's own words.
    #[test]
    fn a_program_that_writes_through_the_shell_is_a_write() {
        let programs: Vec<String> = (0..12)
            .map(|n| format!("echo line-{n} > notes-{n}.md"))
            .collect();
        let owned: Vec<&str> = programs.iter().map(String::as_str).collect();
        let entries = logged(&owned);
        let steps = steps(&entries);
        assert!(
            inspect(&steps, &limits()).is_none(),
            "{:?}",
            inspect(&steps, &limits())
        );
    }

    /// A loop that has also spent its allowance is a loop: the more specific finding wins, so the record
    /// says what the run was doing rather than only that it ran out of money.
    #[test]
    fn a_repeating_run_is_reported_as_repeating_before_it_is_reported_as_spent() {
        let steps: Vec<Step> = (0..5)
            .map(|_| Step {
                tool: "shell".to_string(),
                arguments: "cargo test | tail -5".to_string(),
                did: Did::Nothing,
                tokens: 2_000,
            })
            .collect();
        let mut limits = limits();
        limits.allowance = Some(1_000);
        assert!(matches!(
            inspect(&steps, &limits),
            Some(Stall::Repeating { .. })
        ));
    }

    /// The record is read by a person, so it is one sentence naming the tool, the call and the count.
    #[test]
    fn a_stall_says_what_it_saw_in_one_sentence() {
        let stall = Stall::Repeating {
            tool: "shell: cargo test".to_string(),
            times: 5,
        };
        assert_eq!(stall.sentence(), "repeating shell: cargo test, 5 times");
        assert_eq!(
            Stall::NoProgress { steps: 12 }.sentence(),
            "12 steps with nothing written or read"
        );
        assert_eq!(
            Stall::SpentWithoutOutput { tokens: 40_000 }.sentence(),
            "40000 tokens spent with nothing written"
        );
    }
}
