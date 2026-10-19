//! What the audit found.

use std::fmt;

/// One kind of problem, as an enum, so a new kind is a variant rather than a new string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ProblemKind {
    MissingSummary,
    MissingTopic,
    MissingProvenance,
    /// Printed from [`crate::report::AuditReport::fenced`], never counted against `ok()`: the path is behind
    /// a fence, which is the boundary doing its job.
    FencedPath,
    InboxOverCap,
    StaleInbox,
}

impl ProblemKind {
    /// The literal the report prints and anything downstream parses. Literal and lowercase, like every
    /// other key in the system.
    pub fn as_str(self) -> &'static str {
        match self {
            ProblemKind::MissingSummary => "missing_summary",
            ProblemKind::MissingTopic => "missing_topic",
            ProblemKind::MissingProvenance => "missing_provenance",
            ProblemKind::FencedPath => "fenced_path",
            ProblemKind::InboxOverCap => "inbox_over_cap",
            ProblemKind::StaleInbox => "stale_inbox",
        }
    }
}

impl fmt::Display for ProblemKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One finding: what is wrong, where, and enough detail to act on without opening the report again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub kind: ProblemKind,
    pub path: String,
    pub detail: String,
}

impl Problem {
    pub fn new(kind: ProblemKind, path: impl Into<String>, detail: impl Into<String>) -> Self {
        Problem {
            kind,
            path: path.into(),
            detail: detail.into(),
        }
    }
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {} ({})", self.kind, self.path, self.detail)
    }
}
