//! Tools: the bindings a program can reach.
//!
//! The design's move is that the action space is *code*, not a menu of function calls, so "the tool" is
//! the thing that runs a program and hands back what it said. That is why [`Tool`] is small and why there
//! is exactly one of them here.
//!
//! [`Shell`] runs a program. By itself it runs with the caller's rights and says so; with a [`Cage`] on it,
//! the program is confined to the task's root and denied the network, and the log records which of those two
//! it was. The difference is a field, and a field can be read.

use std::path::{Path, PathBuf};

use crate::cage::Cage;
use crate::command::{self, Run};
use crate::fault::Fault;

/// What a tool is called, and what it is for. Thin on purpose: this is the human-readable contract, and a
/// machine-readable schema belongs with the model call that needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSchema {
    pub name: String,
    pub summary: String,
}

/// What a tool is asked to do: one program, run once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    pub program: String,
}

impl Call {
    pub fn new(program: impl Into<String>) -> Self {
        Call {
            program: program.into(),
        }
    }
}

/// A capability the program can use.
pub trait Tool {
    fn schema(&self) -> ToolSchema;
    fn call(&self, call: &Call) -> Result<Run, Fault>;

    /// What this tool allows, for the log. A default that admits there is nothing to say, so an
    /// implementation cannot leave the field blank by forgetting it.
    fn describe(&self) -> String {
        "nothing is confined: the program runs with the caller's rights".to_string()
    }
}

/// The shell, rooted at the task's directory, and confined when a cage is on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shell {
    root: PathBuf,
    limit: usize,
    cage: Option<Cage>,
    /// Where a model's program may write temporary files. `None` leaves the inherited `TMPDIR` alone, which
    /// under a cage is a path the program cannot use.
    tmp: Option<PathBuf>,
}

impl Shell {
    /// A shell with nothing confined. For a caller that wants the loop without the machinery, and for the
    /// tests of everything above this line.
    pub fn new(root: impl Into<PathBuf>, limit: usize) -> Self {
        Shell {
            root: root.into(),
            limit,
            cage: None,
            tmp: None,
        }
    }

    /// The same shell, confined. This is what a run should use, and the CLI does.
    pub fn caged(mut self, cage: Cage) -> Self {
        self.cage = Some(cage);
        self
    }

    /// The directory the program's `TMPDIR` points at. The caller's cage must grant it writable.
    pub fn with_tmp(mut self, tmp: impl Into<PathBuf>) -> Self {
        self.tmp = Some(tmp.into());
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn cage(&self) -> Option<&Cage> {
        self.cage.as_ref()
    }
}

impl Tool for Shell {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: crate::SHELL.to_string(),
            summary: "run a shell program in the task's directory and read what it printed"
                .to_string(),
        }
    }

    /// A model's program, run under the scrubbed environment.
    ///
    /// Every program this tool runs was written by a model, so the child never sees the environment the
    /// harness was started with: a repo script that prints `env` would otherwise read a provider key the
    /// model was never given. The checker is not this tool; it runs as the caller's own command and keeps
    /// the caller's environment (`verify.rs`), which is the deliberate difference.
    fn call(&self, call: &Call) -> Result<Run, Fault> {
        command::run_clean_in(
            &call.program,
            &self.root,
            self.limit,
            self.cage.as_ref(),
            self.tmp.as_deref(),
        )
    }

    fn describe(&self) -> String {
        match &self.cage {
            Some(cage) => cage.describe(),
            None => "nothing is confined: the program runs with the caller's rights".to_string(),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    /// The surface the run graph uses, not `command::run_clean` directly: this is the test that fails when
    /// the scrub is applied one layer below the tool the model's programs actually reach.
    #[test]
    fn the_tool_a_run_wires_runs_without_the_harness_environment() {
        // The shell path a governed run builds: no cage, so nothing but the environment can confound this.
        let root = crate::scratch::root().join(format!("tool-env-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("a scratch root");

        unsafe {
            std::env::set_var("HADES_TOOL_SECRET", "«redacted:sk-…»");
        }
        let shell = Shell::new(&root, 100_000);
        let run = shell
            .call(&Call::new("env"))
            .expect("the tool runs a program");
        unsafe {
            std::env::remove_var("HADES_TOOL_SECRET");
        }

        assert!(
            !run.output.contains("HADES_TOOL_SECRET"),
            "the tool handed the model's program the harness environment: {}",
            run.output
        );
        assert!(
            run.output.contains("PATH="),
            "a shell still needs PATH: {}",
            run.output
        );
    }
}
