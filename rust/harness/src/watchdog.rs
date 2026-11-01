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

/// One step of a run, as the session log recorded it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// The tool that ran.
    pub tool: String,
    /// What it was asked to do: a program's text, an edit's arguments, the raw arguments of anything else.
    pub arguments: String,
    /// Whether this step created or modified a file. A shell program that writes through the shell is not
    /// counted: the log does not say what a program touched, and inventing "it wrote" would hide a stall.
    pub wrote: bool,
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
            Stall::NoProgress { steps } => format!("{steps} steps with nothing written"),
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
        && steps.iter().all(|step| !step.wrote)
    {
        return Some(Stall::SpentWithoutOutput { tokens: spent });
    }

    // A step stands still when it neither wrote nor looked at anything. Reading is how a run rebuilds its
    // working set after a trim, so counting reads as standing still would stop the runs that are recovering.
    let still = steps
        .iter()
        .rev()
        .take_while(|step| !step.wrote && crate::agent::repeat_guarded(&step.tool))
        .count();
    if still >= limits.no_progress {
        return Some(Stall::NoProgress { steps: still });
    }

    None
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
                steps.push(Step {
                    tool,
                    arguments,
                    wrote: false,
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
                if step.tool == "patch" && status == 0 {
                    step.wrote = true;
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

    fn shell(command: &str) -> Step {
        Step {
            tool: "shell".to_string(),
            arguments: command.to_string(),
            wrote: false,
            tokens: 100,
        }
    }

    fn write(path: &str) -> Step {
        Step {
            tool: "write_file".to_string(),
            arguments: path.to_string(),
            wrote: true,
            tokens: 100,
        }
    }

    fn limits() -> Limits {
        Limits::default()
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
                wrote: false,
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
    #[test]
    fn a_run_of_reads_is_not_no_progress() {
        let steps: Vec<Step> = (0..40)
            .map(|n| Step {
                tool: "read_file".to_string(),
                arguments: format!("src/file-{n}.rs"),
                wrote: false,
                tokens: 100,
            })
            .collect();
        assert!(inspect(&steps, &limits()).is_none());
    }

    /// A loop that has also spent its allowance is a loop: the more specific finding wins, so the record
    /// says what the run was doing rather than only that it ran out of money.
    #[test]
    fn a_repeating_run_is_reported_as_repeating_before_it_is_reported_as_spent() {
        let steps: Vec<Step> = (0..5)
            .map(|_| Step {
                tool: "shell".to_string(),
                arguments: "cargo test | tail -5".to_string(),
                wrote: false,
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
            "12 steps with nothing written"
        );
        assert_eq!(
            Stall::SpentWithoutOutput { tokens: 40_000 }.sentence(),
            "40000 tokens spent with nothing written"
        );
    }
}
