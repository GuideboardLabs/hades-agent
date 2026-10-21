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
    /// The check ran past its wall-clock bound and was killed. It is not a failure of the work: the check
    /// never reached a verdict, and reporting it as `Failed` states a judgement nobody made.
    TimedOut,
    /// The check never ran: no command configured, or the harness could not start it. Not a result.
    CouldNotRun,
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            State::Passed => "passed",
            State::Failed => "failed",
            State::TimedOut => "timed_out",
            State::CouldNotRun => "could_not_run",
        }
    }

    pub fn is_a_result(self) -> bool {
        !matches!(self, State::CouldNotRun | State::TimedOut)
    }

    /// The state a log recorded, by the name `name()` writes. `None` for a name this build does not know:
    /// a log from a newer build is not silently read as a state this one understands.
    pub fn from_name(name: &str) -> Option<State> {
        Some(match name {
            "passed" => State::Passed,
            "failed" => State::Failed,
            "timed_out" => State::TimedOut,
            "could_not_run" => State::CouldNotRun,
            _ => return None,
        })
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
    /// The exit status the check returned, when it returned one. `None` means nothing ran, which is not
    /// the same as a non-zero status and must not be read as one. `Some(-1)` means the process ended
    /// without a status of its own: a signal, or the deadline killing it.
    pub status: Option<i32>,
    /// Whether `output` is a slice of what the check wrote rather than all of it.
    pub truncated: bool,
    /// The sha256 of `output` exactly as recorded here, so a later reader can check the record against
    /// the bytes it holds. A truncated output is digested as the slice it is, and `truncated` says so.
    pub output_digest: String,
    /// Present only when the check opted into the progress contract. A missing value is not zero progress.
    pub progress: Option<Progress>,
    /// The attack's result, when this run carries one. A security pass is `check passed AND attack
    /// failed`; a check that is green while the exploit still works is a failed security result.
    pub attack: Option<AttackReport>,
}

/// What a security case's second oracle did. The attack succeeds when the vulnerability is present, so its
/// success is the run's failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttackReport {
    pub command: String,
    pub succeeded: bool,
    /// The exit status the attack returned, when it returned one. `None` means it could not run.
    pub status: Option<i32>,
    pub output: String,
}

impl AttackReport {
    pub fn line(&self) -> String {
        match (self.succeeded, self.status) {
            (true, Some(status)) => format!("attack: succeeded (exit {status})"),
            (false, Some(status)) => format!("attack: blocked (exit {status})"),
            (true, None) => "attack: succeeded (no exit status)".to_string(),
            (false, None) => "attack: could not run".to_string(),
        }
    }

    /// `succeeded`, `blocked`, or `not_run` for a log or a result line.
    pub fn name(&self) -> &'static str {
        match (self.succeeded, self.status) {
            (_, None) => "not_run",
            (true, Some(_)) => "succeeded",
            (false, Some(_)) => "blocked",
        }
    }
}

impl Verification {
    pub fn could_not_run(command: &str, why: impl Into<String>) -> Self {
        let why = why.into();
        Verification {
            command: command.to_string(),
            state: State::CouldNotRun,
            output_digest: mneme::sha256::hex(why.as_bytes()),
            output: why,
            status: None,
            truncated: false,
            progress: None,
            attack: None,
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
                let state = if run.timed_out {
                    State::TimedOut
                } else if run.passed() && gate_ok {
                    State::Passed
                } else {
                    State::Failed
                };
                let output = if run.passed() && !gate_ok {
                    format!("exit 0 but the terminal gate failed\n{}\n", run.output)
                } else if run.passed() {
                    run.output
                } else {
                    format!("exit {}\n{}\n", run.status, run.output)
                };
                Verification {
                    command: command.to_string(),
                    state,
                    output_digest: mneme::sha256::hex(output.as_bytes()),
                    output,
                    status: Some(run.status),
                    truncated: run.truncated,
                    progress,
                    attack: None,
                }
            }
            Err(fault) => Verification::could_not_run(command, fault.detail()),
        }
    }
}

/// A check with a second oracle. The attack **succeeds while the vulnerability is present**, so a green
/// check beside a working exploit is a failed security result, not a pass. A baseline that is red because
/// the attack succeeds is the case a security run exists to fix.
///
/// The attack that cannot run is a fault, not a pass: an unmeasured exploit is not a blocked one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attacked {
    check: CommandVerifier,
    attack: String,
    limit: usize,
    deadline: Option<std::time::Duration>,
}

impl Attacked {
    pub fn new(
        limit: usize,
        deadline: Option<std::time::Duration>,
        attack: impl Into<String>,
    ) -> Self {
        Attacked {
            check: CommandVerifier { limit, deadline },
            attack: attack.into(),
            limit,
            deadline,
        }
    }
}

impl Verifier for Attacked {
    fn verify(&self, task: &TaskSpec, command: &str) -> Verification {
        let mut checked = self.check.verify(task, command);
        let report =
            match command::run_within(&self.attack, task.root(), self.limit, None, self.deadline) {
                Ok(run) => AttackReport {
                    command: self.attack.clone(),
                    succeeded: run.passed(),
                    status: Some(run.status),
                    output: run.output,
                },
                Err(fault) => AttackReport {
                    command: self.attack.clone(),
                    succeeded: false,
                    status: None,
                    output: fault.detail().to_string(),
                },
            };
        if report.status.is_none() {
            checked.state = State::CouldNotRun;
            checked.output = format!(
                "{}\nthe attack could not run, so it is unmeasured: {}",
                checked.output.trim_end(),
                report.output.trim()
            );
        } else if report.succeeded && checked.state == State::Passed {
            checked.state = State::Failed;
            checked.output = format!(
                "the exploit still succeeds, so the check is not evidence of a fix\n{}",
                checked.output.trim_end()
            );
        }
        checked.output_digest = mneme::sha256::hex(checked.output.as_bytes());
        checked.attack = Some(report);
        checked
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
    fn a_sleeping_check_is_timed_out_not_failed() {
        let root = scratch("timeout");
        let task = TaskSpec::new("make it pass", root);
        let verifier = CommandVerifier::with_deadline(8_000, std::time::Duration::from_millis(200));
        let verification = verifier.verify(&task, "sleep 5");
        assert_eq!(verification.state, State::TimedOut);
        assert_eq!(verification.status, Some(-1));
        assert!(!verification.state.is_a_result());
        assert_eq!(verification.output_digest.len(), 64);
        assert!(
            verification
                .output_digest
                .chars()
                .all(|c| c.is_ascii_hexdigit())
        );
    }

    #[test]
    fn a_passing_check_records_its_status_and_digest() {
        let root = scratch("status");
        let task = TaskSpec::new("make it pass", root);
        let verifier = CommandVerifier::new(8_000);
        let verification = verifier.verify(&task, "printf ok; exit 0");
        assert_eq!(verification.state, State::Passed);
        assert_eq!(verification.status, Some(0));
        assert!(!verification.truncated);
        assert_eq!(verification.output_digest, mneme::sha256::hex(b"ok"));
    }

    #[test]
    fn a_truncated_check_says_the_output_is_a_slice_and_digests_what_it_kept() {
        let root = scratch("truncated");
        let task = TaskSpec::new("make it pass", root);
        let verifier = CommandVerifier::new(200);
        let verification = verifier.verify(&task, "seq 1 4000");
        assert!(verification.truncated);
        assert_eq!(
            verification.output_digest,
            mneme::sha256::hex(verification.output.as_bytes())
        );
    }

    #[test]
    fn a_check_that_never_ran_has_no_status() {
        let root = scratch("none");
        let task = TaskSpec::new("make it pass", root);
        let verification = CommandVerifier::new(8_000).verify(&task, "  ");
        assert_eq!(verification.state, State::CouldNotRun);
        assert_eq!(verification.status, None);
        assert!(!verification.truncated);
        assert!(!verification.state.is_a_result());
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
