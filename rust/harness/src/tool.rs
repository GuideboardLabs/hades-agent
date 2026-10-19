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
}

impl Shell {
    /// A shell with nothing confined. For a caller that wants the loop without the machinery, and for the
    /// tests of everything above this line.
    pub fn new(root: impl Into<PathBuf>, limit: usize) -> Self {
        Shell {
            root: root.into(),
            limit,
            cage: None,
        }
    }

    /// The same shell, confined. This is what a run should use, and the CLI does.
    pub fn caged(mut self, cage: Cage) -> Self {
        self.cage = Some(cage);
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

    fn call(&self, call: &Call) -> Result<Run, Fault> {
        command::run(&call.program, &self.root, self.limit, self.cage.as_ref())
    }

    fn describe(&self) -> String {
        match &self.cage {
            Some(cage) => cage.describe(),
            None => "nothing is confined: the program runs with the caller's rights".to_string(),
        }
    }
}
