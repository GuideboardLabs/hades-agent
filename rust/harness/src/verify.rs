//! The check: a command, and what it found.
//!
//! Three states, and the third is the one that keeps the others honest. `CouldNotRun` is not a failure of
//! the answer and it is not a pass: it is the case where the harness has nothing to say, and reporting it
//! as either is how a run ends up claiming a check that never happened.
//!
//! It is a command rather than a language server, and that is deliberate. A language server gives better
//! diagnostics and better diagnostics change the answer, so it is opt-in and off unless the repo has one
//! configured, which most have not.

use serde_json::Value;

use crate::command;
use crate::task::TaskSpec;

/// What the check found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Passed,
    Failed,
    /// The check never ran: no command configured, or the harness could not start it. Not a result.
    CouldNotRun,
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            State::Passed => "passed",
            State::Failed => "failed",
            State::CouldNotRun => "could_not_run",
        }
    }

    pub fn is_a_result(self) -> bool {
        !matches!(self, State::CouldNotRun)
    }
}

/// The optional progress a check reports. A strict gate always outranks any amount of partial progress:
/// `gate` is the terminal verification, and `passed`/`total` only describe how far a failing run got.
///
/// The contract is one final line:
/// `HADES-CHECK {"gate":false,"passed":3,"total":5,"family":"parser"}`
/// A check that prints nothing keeps the binary behavior.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Progress {
    pub gate: bool,
    pub passed: u32,
    pub total: u32,
    pub family: Option<String>,
}

impl Progress {
    pub fn parse(output: &str) -> Option<Progress> {
        output
            .lines()
            .rev()
            .filter_map(|line| line.trim().strip_prefix("HADES-CHECK "))
            .find_map(|rest| {
                serde_json::from_str::<Value>(rest)
                    .ok()
                    .and_then(|value| Progress::from_value(&value))
            })
    }

    pub fn from_value(value: &Value) -> Option<Progress> {
        let passed = value.get("passed")?.as_u64()?;
        let total = value.get("total")?.as_u64()?;
        if total == 0 || passed > total {
            return None;
        }
        Some(Progress {
            gate: value.get("gate").and_then(Value::as_bool).unwrap_or(false),
            passed: passed as u32,
            total: total as u32,
            family: value
                .get("family")
                .and_then(Value::as_str)
                .filter(|family| !family.trim().is_empty())
                .map(|family| family.trim().to_string()),
        })
    }

    pub fn ratio(&self) -> f64 {
        f64::from(self.passed) / f64::from(self.total)
    }

    pub fn json(&self) -> Value {
        serde_json::json!({
            "gate": self.gate,
            "passed": self.passed,
            "total": self.total,
            "family": self.family,
        })
    }

    pub fn line(&self) -> String {
        format!(
            "{}/{} checks{}",
            self.passed,
            self.total,
            self.family
                .as_ref()
                .map(|family| format!(" in {family}"))
                .unwrap_or_default()
        )
    }
}

/// The check, the state it left, and what it said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verification {
    pub command: String,
    pub state: State,
    pub output: String,
    /// Present only when the check opted into the progress contract. A missing value is not zero progress.
    pub progress: Option<Progress>,
}

impl Verification {
    pub fn could_not_run(command: &str, why: impl Into<String>) -> Self {
        Verification {
            command: command.to_string(),
            state: State::CouldNotRun,
            output: why.into(),
            progress: None,
        }
    }

    pub fn passed(&self) -> bool {
        self.state == State::Passed
    }

    /// The state and, when the check reported it, how far it got. Used in a run's learned context and in
    /// the reason a branch failed, so a later attempt sees `3/5` instead of a bare `failed`.
    pub fn state_line(&self) -> String {
        match &self.progress {
            Some(progress) => format!("{} ({})", self.state.name(), progress.line()),
            None => self.state.name().to_string(),
        }
    }
}

/// Running the check.
pub trait Verifier {
    fn verify(&self, task: &TaskSpec, command: &str) -> Verification;
}

/// Runs the profile's check as a program and reads its exit status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandVerifier {
    limit: usize,
    /// How long the check may run. `None` is the caller's own command run untimed, which is what the
    /// profile check has always done; a goal check names a bound because it runs on the heartbeat thread.
    deadline: Option<std::time::Duration>,
}

impl CommandVerifier {
    pub fn new(limit: usize) -> Self {
        CommandVerifier {
            limit,
            deadline: None,
        }
    }

    /// A verifier whose check is killed after `deadline`. A check that hangs must not be able to hold
    /// the only thread that continues goals.
    pub fn with_deadline(limit: usize, deadline: std::time::Duration) -> Self {
        CommandVerifier {
            limit,
            deadline: Some(deadline),
        }
    }
}

impl Verifier for CommandVerifier {
    fn verify(&self, task: &TaskSpec, command: &str) -> Verification {
        if command.trim().is_empty() {
            return Verification::could_not_run(
                command,
                "this profile configures no check, so nothing was verified",
            );
        }

        // Not caged, on purpose: the check is the caller's own command and not the model's program, and a
        // check that cannot reach its toolchain is not a check. The cage is for what the model wrote.
        match command::run_within(command, task.root(), self.limit, None, self.deadline) {
            Ok(run) => {
                let progress = Progress::parse(&run.output);
                let gate_ok = progress.as_ref().map(|p| p.gate).unwrap_or(true);
                let state = if run.passed() && gate_ok {
                    State::Passed
                } else {
                    State::Failed
                };
                let output = if run.passed() && !gate_ok {
                    format!("exit 0 but the terminal gate failed\n{}", run.output)
                } else if run.passed() {
                    run.output
                } else {
                    format!("exit {}\n{}", run.status, run.output)
                };
                Verification {
                    command: command.to_string(),
                    state,
                    output,
                    progress,
                }
            }
            Err(fault) => Verification::could_not_run(command, fault.detail()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch(name: &str) -> PathBuf {
        let dir = crate::scratch::root().join(format!("verify-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch root");
        dir
    }

    #[test]
    fn progress_is_read_from_the_last_check_line() {
        let output =
            "noise\nHADES-CHECK {\"gate\":false,\"passed\":3,\"total\":5,\"family\":\"parser\"}\n";
        let progress = Progress::parse(output).expect("a progress line");
        assert_eq!(progress.passed, 3);
        assert_eq!(progress.total, 5);
        assert!(!progress.gate);
        assert_eq!(progress.family.as_deref(), Some("parser"));
        assert!((progress.ratio() - 0.6).abs() < f64::EPSILON);
    }

    #[test]
    fn a_malformed_or_impossible_progress_line_is_ignored() {
        assert!(Progress::parse("HADES-CHECK {").is_none());
        assert!(Progress::parse("HADES-CHECK {\"gate\":true,\"passed\":6,\"total\":5}").is_none());
        assert!(Progress::parse("HADES-CHECK {\"gate\":true,\"passed\":0,\"total\":0}").is_none());
    }

    #[test]
    fn a_zero_exit_with_a_failed_gate_is_a_failure() {
        let root = scratch("gate");
        let task = TaskSpec::new("make it pass", root);
        let verifier = CommandVerifier::new(8_000);
        let verification = verifier.verify(
            &task,
            "printf 'HADES-CHECK {\"gate\":false,\"passed\":3,\"total\":5}\\n'; exit 0",
        );
        assert_eq!(verification.state, State::Failed);
        assert_eq!(verification.progress.expect("progress").passed, 3);
    }

    #[test]
    fn a_zero_exit_with_a_passed_gate_is_a_pass() {
        let root = scratch("pass");
        let task = TaskSpec::new("make it pass", root);
        let verifier = CommandVerifier::new(8_000);
        let verification = verifier.verify(
            &task,
            "printf 'HADES-CHECK {\"gate\":true,\"passed\":5,\"total\":5}\\n'; exit 0",
        );
        assert_eq!(verification.state, State::Passed);
    }
}
