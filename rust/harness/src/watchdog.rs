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
    /// It looked at something, or the log has no evidence that it changed a file and it is not one of the
    /// programs whose value is work on the tree. Reading is how a run rebuilds its working set after a trim,
    /// so this is the state the stall rule exempts: a false stop costs more than a slow run.
    Read,
    /// It created or changed a file: an edit that landed, or a program that sends its output into one.
    Write,
    /// A program that runs work whose writes the log cannot see, or a tool nobody classified. This is the
    /// only state that counts toward a stall, so it is the only one where silence is evidence.
    Nothing,
}

impl Did {
    /// The word the session log carries for it. The producer writes this beside the call, because it is the
    /// only place where the tool, its arguments and its result are all in hand at once.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Did::Read => "read",
            Did::Write => "write",
            Did::Nothing => "nothing",
        }
    }

    /// The judgement a log carries, when it carries one this build knows. A word from a later build is not
    /// guessed at: the text classifier answers instead, exactly as it does for a log written before the
    /// field existed.
    fn recorded(label: &str) -> Option<Did> {
        match label {
            "read" => Some(Did::Read),
            "write" => Some(Did::Write),
            "nothing" => Some(Did::Nothing),
            _ => None,
        }
    }
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

/// Whether a step is the harness's own check rather than a call the run made.
///
/// `turn` appends the check after every step it acts on, under its own tool. Counting it as one of the run's
/// steps makes every turn two steps and the trailing run of one call no longer than one, so `Repeating` could
/// not fire on a log a real run wrote, and a run that wrote nothing reached the twelve-step bound after six.
fn is_check(step: &Step) -> bool {
    step.tool == crate::VERIFY
}

/// The verdict on a step history, or nothing when nothing is wrong.
///
/// Only the tail is examined: the question is whether the run is stalled *now*, not whether it ever
/// repeated itself. A loop that a write interrupted and never resumed is a run that recovered.
pub fn inspect(steps: &[Step], limits: &Limits) -> Option<Stall> {
    // The run's own steps. The check is written by the harness, after the step it checked: it is evidence
    // about the tree, not a call the run chose, so neither the repeat rule nor the stall rule may count it.
    let run: Vec<&Step> = steps.iter().filter(|step| !is_check(step)).collect();
    let last = run.last()?;

    let key = crate::agent::call_key(&last.tool, &last.arguments);
    let times = run
        .iter()
        .rev()
        .take_while(|step| crate::agent::call_key(&step.tool, &step.arguments) == key)
        .count();
    if times >= limits.repeats {
        return Some(Stall::Repeating { tool: key, times });
    }

    // A loop that has also spent its allowance is a loop: the more specific finding is the useful one. The
    // sum is over every step, the check's model calls included: that is money, and money was spent.
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
    let still = run
        .iter()
        .rev()
        .take_while(|step| step.did == Did::Nothing)
        .count();
    if still >= limits.no_progress {
        return Some(Stall::NoProgress { steps: still });
    }

    None
}

/// What a step did, decided where the tool's name, its arguments and, for an edit, its result are all in
/// hand. `turn` calls this and writes the word into the log it produces; `steps` reads it back. This is the
/// seam the rule depends on: a reader that re-derived the judgement from text would be re-deriving it from
/// less than the writer had.
pub(crate) fn classify(tool: &str, arguments: &str) -> Did {
    match tool {
        crate::SHELL => program(arguments),
        _ => by_tool(tool),
    }
}

/// The judgement a log with no recorded classification gets: the text alone.
///
/// Narrower than `classify` by design, and kept only for logs written before the field existed. Its program
/// rule is a list of reporters, so a read-only program that list does not name is judged to have done
/// nothing; a read-only `git` step in an old log therefore counts toward the stall. That limit is the reason
/// the producer records the judgement at all.
fn classify_text(tool: &str, arguments: &str) -> Did {
    match tool {
        crate::SHELL => legacy_program(arguments),
        _ => by_tool(tool),
    }
}

/// The tools whose name is the whole claim. An edit is a write when it lands, and the result's own status is
/// what says so: a refused edit changed nothing, and `steps` corrects this for a log that does not carry the
/// classification.
fn by_tool(tool: &str) -> Did {
    match tool {
        "patch" | "write_file" | "edit" => Did::Write,
        "read_file" | "list_dir" | "search" | "grep" | "skills" | "recall" | "session_search"
        | "fetch" | "library" | "image" => Did::Read,
        _ => Did::Nothing,
    }
}

/// The programs whose value is work on the tree: builders, compilers and test runners, whose writes a log
/// cannot see. A step in one of these is the only shape where "nothing was written" is a fair reading, so it
/// is the only shape that counts toward a stall.
///
/// This list is deliberately short, and its failure is benign: a build tool nobody listed is exempted from
/// the stall rule rather than having a run stopped for it. The list it replaced named the *readers*, which
/// had to hold every program anyone runs and never could.
const WORK: &[&str] = &[
    "cargo", "rustc", "make", "cmake", "ninja", "gcc", "cc", "clang", "ld", "go", "npm", "pnpm",
    "yarn", "pytest", "mvn", "gradle", "zig", "tsc", "javac", "dotnet", "bazel",
];

/// The programs that only report, for the text fallback only.
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
/// Three questions, cheapest first: did it send output into a file or edit one in place, and does any part
/// of it run work whose writes the log cannot see? A writer wrote. A builder is the one thing counted as
/// standing still. Everything else is a look, including the programs this build has never heard of: they
/// did not write, and a run cannot be stopped for what the log merely does not recognise.
fn program(text: &str) -> Did {
    if !crate::floor::redirections(text).is_empty() || writes(text) {
        return Did::Write;
    }
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
        if WORK.contains(&word.to_ascii_lowercase().as_str()) {
            return Did::Nothing;
        }
    }
    Did::Read
}

/// What a program did, from its own words, for a log written before the classification was recorded.
fn legacy_program(text: &str) -> Did {
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
        let lower = |word: &str| word.to_ascii_lowercase();
        match crate::floor::command_word(&words) {
            // `--in-place=.bak` names a backup suffix and is the same flag as `--in-place`, just as `-i.bak`
            // is the same flag as `-i`. A word that merely starts with `-i` catches both, and `--` is
            // excluded because no long option writes in place.
            Some(word) if matches!(lower(word).as_str(), "sed" | "perl") => {
                words.iter().skip(1).any(|word| {
                    let word = lower(word);
                    word.starts_with("--in-place")
                        || (word.starts_with("-i") && !word.starts_with("--"))
                })
            }
            // `find ... -exec rm {} ;` removes what it matched, and `-delete` does the same without a
            // program to name. A `-exec` that runs a reporter (`-exec cat {} +`) only prints.
            Some(word) if lower(word) == "find" => {
                let mut arguments = words.iter().skip(1);
                while let Some(word) = arguments.next() {
                    let flag = lower(word);
                    if flag == "-delete" {
                        return true;
                    }
                    if matches!(flag.as_str(), "-exec" | "-execdir" | "-ok" | "-okdir")
                        && let Some(next) = arguments.next()
                    {
                        let next = lower(next);
                        let next = next.rsplit('/').next().unwrap_or(next.as_str());
                        if WRITERS.contains(&next)
                            || (matches!(next, "sed" | "perl")
                                && words.iter().any(|word| lower(word).starts_with("-i")))
                        {
                            return true;
                        }
                    }
                }
                false
            }
            // `sort -o out` writes the file it names; with no destination it only prints.
            Some(word) if lower(word) == "sort" => words.iter().skip(1).any(|word| {
                let word = lower(word);
                word == "-o"
                    || word.starts_with("--output")
                    || (word.starts_with("-o") && word.len() > 2)
            }),
            // `uniq in out` takes a second file and writes it; `uniq in` only prints.
            Some(word) if lower(word) == "uniq" => {
                words
                    .iter()
                    .skip(1)
                    .filter(|word| !word.starts_with('-'))
                    .count()
                    > 1
            }
            // `tee` with no file writes to standard output, which changes nothing on disk.
            Some(word) if word.eq_ignore_ascii_case("tee") => {
                words.iter().skip(1).any(|word| !word.starts_with('-'))
            }
            Some(word) => WRITERS.contains(&lower(word).as_str()),
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
                // The producer recorded what this step did, in the only place where the tool, its arguments
                // and its result were all in hand. A log written before that field existed is judged by its
                // text, and the text is read only as far as it can be trusted.
                let did = entry
                    .payload
                    .get("did")
                    .and_then(|value| value.as_str())
                    .and_then(Did::recorded)
                    .unwrap_or_else(|| classify_text(&tool, &arguments));
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
                // An edit's result is the only thing that says whether the write landed, and the producer
                // writes that judgement here. A log without it keeps the older reading: a refused edit
                // changed nothing, while a program's exit status is not evidence about what it wrote (a
                // `grep` with no match exits 1 and still wrote the file it was redirected into).
                if let Some(did) = entry
                    .payload
                    .get("did")
                    .and_then(|value| value.as_str())
                    .and_then(Did::recorded)
                {
                    step.did = did;
                    continue;
                }
                let status = entry
                    .payload
                    .get("status")
                    .and_then(|value| value.as_i64())
                    .unwrap_or(1);
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

    /// A session log in the shape `turn` writes *after* it records what each step did: the call carries the
    /// classification, so the reader never has to re-derive it from text.
    fn recorded(programs: &[&str], did: &str) -> Vec<Entry> {
        let root = crate::scratch::unique("watchdog-recorded");
        let mut session = Session::create(
            FileStore::new(&root),
            SystemClock,
            SessionId::new("recorded"),
        )
        .expect("a session");
        for program in programs {
            session
                .append(
                    Kind::ToolCall,
                    json!({"tool": "shell", "program": program, "did": did}),
                )
                .expect("a call");
            session
                .append(
                    Kind::ToolResult,
                    json!({"tool": "shell", "status": 1, "output": ""}),
                )
                .expect("a result");
        }
        Session::load(
            FileStore::new(&root),
            SystemClock,
            SessionId::new("recorded"),
        )
        .expect("the log")
        .entries()
        .to_vec()
    }

    /// The defect A was narrowed to, not closed: every read-only program the text classifier could not
    /// place was judged to have done nothing, so twelve *distinct* `git` reads stopped a run for looking.
    /// The producer records the judgement, and the rule reads the record rather than the text.
    #[test]
    fn twelve_distinct_read_only_programs_are_not_no_progress() {
        let entries = recorded(
            &[
                "git log --oneline -3",
                "git diff --stat",
                "git show HEAD",
                "git status --short",
                "git log --format=%H -1",
                "git diff HEAD~1",
                "git show HEAD:rust/harness/src/lib.rs",
                "git status",
                "git log --oneline -1",
                "git diff --name-only",
                "git show --stat HEAD",
                "git status --porcelain",
            ],
            "read",
        );
        let steps = steps(&entries);
        assert!(
            inspect(&steps, &limits()).is_none(),
            "a run of read-only git steps was stopped for looking: {:?}",
            inspect(&steps, &limits())
        );
    }

    /// The same history recorded as a look, but with the words the log writes: the classification read from
    /// the entry is what decides, not the text underneath it.
    #[test]
    fn the_classification_the_producer_recorded_is_what_the_rule_reads() {
        let entries = recorded(&["cargo test --test case-1"], "read");
        let steps = steps(&entries);
        assert_eq!(
            steps[0].did,
            Did::Read,
            "the recorded classification was discarded and the text judged instead"
        );
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

    /// A refused edit in a log that predates the recorded classification. The call says `patch`, which is a
    /// write by name; only the result's own status says it did not land. That correction is the whole of the
    /// older reading and no test built the shape before this one, so deleting it left the suite green and a
    /// refused edit counting as a write.
    #[test]
    fn an_old_log_reads_a_refused_edit_as_nothing() {
        let root = crate::scratch::unique("watchdog-refused");
        let mut session = Session::create(
            FileStore::new(&root),
            SystemClock,
            SessionId::new("refused"),
        )
        .expect("a session");
        session
            .append(
                Kind::ToolCall,
                json!({"tool": "patch", "edits": [{"path": "note.txt"}]}),
            )
            .expect("a call");
        session
            .append(
                Kind::ToolResult,
                json!({"tool": "patch", "status": 1, "output": "note.txt: the old text was not found"}),
            )
            .expect("a result");
        let entries = Session::load(
            FileStore::new(&root),
            SystemClock,
            SessionId::new("refused"),
        )
        .expect("the log")
        .entries()
        .to_vec();
        let steps = steps(&entries);
        assert_eq!(
            steps[0].did,
            Did::Nothing,
            "a refused edit was read as a write, so a run that only tried counts as one that wrote"
        );
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

    /// The allowance branch is guarded by "and nothing was written", and the only test of it used steps that
    /// had all done nothing. A write anywhere in the window is what makes the branch skip, so without this
    /// case the guard could be deleted and the suite would not know.
    #[test]
    fn a_run_that_spent_its_allowance_and_wrote_something_is_not_stopped() {
        let steps = vec![
            Step {
                tool: crate::SHELL.to_string(),
                arguments: "git log --oneline -1".to_string(),
                did: Did::Read,
                tokens: 3_000,
            },
            Step {
                tool: "write_file".to_string(),
                arguments: "src/lib.rs".to_string(),
                did: Did::Write,
                tokens: 0,
            },
            Step {
                tool: crate::SHELL.to_string(),
                arguments: "git status --short".to_string(),
                did: Did::Read,
                tokens: 3_000,
            },
        ];
        let mut limits = limits();
        limits.allowance = Some(5_000);
        assert!(
            inspect(&steps, &limits).is_none(),
            "a run that wrote was stopped for spending: {:?}",
            inspect(&steps, &limits)
        );
    }

    /// A write folded into an option or a program's argument is still a write. Each of these was judged a
    /// look, which is the direction that hides a stall instead of killing a run: `sed --in-place=.bak` edits
    /// the file, `find -exec rm` removes what it matched, and `sort -o` and `uniq in out` write the file they
    /// name. Same family as the reader list: the text decides, and these four shapes were read as nothing.
    #[test]
    fn a_write_folded_into_an_argument_is_a_write() {
        for text in [
            "sed --in-place=.bak 's/alpha/beta/' note.txt",
            "sed -i.bak 's/alpha/beta/' note.txt",
            "find . -name '*.tmp' -exec rm {} \\;",
            "find . -name '*.tmp' -delete",
            "sort -o sorted.txt names.txt",
            "uniq names.txt unique.txt",
        ] {
            assert_eq!(program(text), Did::Write, "{text}");
        }
    }

    /// The other side of the same rule: `sort` and `uniq` with no output file only print, and `find` with no
    /// destructive action only lists. A fix that called every one of them a write would stop measuring.
    #[test]
    fn a_reporter_with_no_destination_is_still_a_look() {
        for text in [
            "sort names.txt",
            "uniq names.txt",
            "find . -name '*.tmp'",
            "sed -n '1,20p' note.txt",
        ] {
            assert_ne!(program(text), Did::Write, "{text}");
        }
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
