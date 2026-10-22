//! The audit report: counts, findings, and the lines a human reads.

use std::collections::BTreeMap;

use crate::problem::{Problem, ProblemKind};

/// What one pass over the vault found.
///
/// `counts` is keyed by area key, plus `stale`, in key order, which is what makes the lines stable
/// between runs and readable in a terminal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditReport {
    pub counts: BTreeMap<String, usize>,
    /// Paths behind a fence: printed as a record that the boundary is where it should be, never counted as a
    /// defect. The vault is the directory; the audited part is what lies outside these paths, so finding one is the
    /// fence working, not the vault being wrong.
    pub fenced: Vec<Problem>,
    pub problems: Vec<Problem>,
}

impl AuditReport {
    pub fn new() -> Self {
        AuditReport::default()
    }

    /// A clean report is the only acceptable reassurance: anything *wrong* makes the vault not ok.
    ///
    /// A fenced path is not wrong. It lives in [`AuditReport::fenced`], is printed, and is deliberately not
    /// counted here: an audit that fails because the boundary exists would be an audit that asks the vault to
    /// stop containing the things the boundary protects.
    pub fn ok(&self) -> bool {
        self.problems.is_empty()
    }

    /// One line per count, then one line per problem.
    pub fn lines(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .counts
            .iter()
            .map(|(key, value)| format!("{key}: {value}"))
            .collect();
        out.extend(self.fenced.iter().map(Problem::to_string));
        out.extend(self.problems.iter().map(Problem::to_string));
        out
    }

    /// The count for one key, or zero when the key was never counted.
    pub fn count(&self, key: &str) -> usize {
        self.counts.get(key).copied().unwrap_or(0)
    }

    pub fn problems_of(&self, kind: ProblemKind) -> Vec<&Problem> {
        self.problems
            .iter()
            .filter(|problem| problem.kind == kind)
            .collect()
    }
}
