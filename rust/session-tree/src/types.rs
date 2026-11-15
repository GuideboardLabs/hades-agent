//! The values: ids, kinds, entries, and the classified error.

use std::fmt;

use serde_json::Value;

/// A position in one session's log. Ids are per session, and a fork reuses them on purpose so a
/// branch lines up line for line with the history it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EntryId(pub u64);

impl fmt::Display for EntryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A session's identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(pub String);

impl SessionId {
    pub fn new(value: impl Into<String>) -> Self {
        SessionId(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Whether a string may be a session id that arrived from outside.
///
/// An id becomes a file name under `<state>/sessions`, so it is checked where the id type lives rather
/// than once per surface. [`SessionId::new`] stays infallible for ids read back from disk; anything a
/// person or a socket supplies is checked with this first.
pub fn valid_session(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        })
}

/// What an entry is. One enum, so a typo cannot invent a state, and an unknown kind is a load error
/// rather than a new variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Meta,
    User,
    Assistant,
    ToolCall,
    ToolResult,
    ApprovalRequest,
    ApprovalDecision,
    Checkpoint,
    Compaction,
    /// A plan written before a program, when the profile asks for one. Its own kind, because a plan is
    /// evidence a person can read and a program is not: folding it into an assistant message would make
    /// "was there a plan before this attempt" a text search.
    Plan,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Meta => "meta",
            Kind::User => "user",
            Kind::Assistant => "assistant",
            Kind::ToolCall => "tool_call",
            Kind::ToolResult => "tool_result",
            Kind::ApprovalRequest => "approval_request",
            Kind::ApprovalDecision => "approval_decision",
            Kind::Checkpoint => "checkpoint",
            Kind::Compaction => "compaction",
            Kind::Plan => "plan",
        }
    }

    /// `None` for anything unrecognised, so a loader fails loudly instead of guessing.
    pub fn parse(value: &str) -> Option<Kind> {
        Some(match value {
            "meta" => Kind::Meta,
            "user" => Kind::User,
            "assistant" => Kind::Assistant,
            "tool_call" => Kind::ToolCall,
            "tool_result" => Kind::ToolResult,
            "approval_request" => Kind::ApprovalRequest,
            "approval_decision" => Kind::ApprovalDecision,
            "checkpoint" => Kind::Checkpoint,
            "compaction" => Kind::Compaction,
            "plan" => Kind::Plan,
            _ => return None,
        })
    }
}

/// One entry in the log.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub id: EntryId,
    pub parent: Option<EntryId>,
    pub kind: Kind,
    pub payload: Value,
    pub at: String,
}

/// Fallibility is a type, and it is classified. `CorruptLog` carries the line, because a truncated
/// write has to fail loudly with its location rather than being skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fault {
    CorruptLog {
        path: String,
        line: usize,
        detail: String,
    },
    UnknownEntry {
        session: String,
        id: EntryId,
    },
    /// A create that would overwrite an existing session. An append-only store never clobbers.
    SessionExists {
        session: String,
    },
    Io {
        path: String,
        detail: String,
    },
}

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Fault::CorruptLog { path, line, detail } => write!(f, "{path}: line {line}: {detail}"),
            Fault::UnknownEntry { session, id } => write!(f, "session {session} has no entry {id}"),
            Fault::SessionExists { session } => {
                write!(
                    f,
                    "session {session} already exists, refusing to overwrite it"
                )
            }
            Fault::Io { path, detail } => write!(f, "{path}: {detail}"),
        }
    }
}

impl std::error::Error for Fault {}

#[cfg(test)]
mod tests {
    use super::{Kind, valid_session};

    #[test]
    fn plan_is_a_kind_with_one_spelling() {
        assert_eq!(Kind::Plan.as_str(), "plan");
        assert_eq!(Kind::parse("plan"), Some(Kind::Plan));
        assert_eq!(Kind::parse("Plan"), None);
    }

    /// The one rule every surface imports: a session id is a plain file name.
    #[test]
    fn a_session_id_is_a_plain_file_name() {
        assert!(valid_session("http-1"));
        assert!(valid_session("tg-123456"));
        assert!(valid_session("daemon"));
        assert!(!valid_session(""));
        assert!(!valid_session(".."));
        assert!(!valid_session("a/b"));
        assert!(!valid_session("../../x"));
        assert!(!valid_session("a.b"));
        assert!(!valid_session(&"a".repeat(65)));
        assert!(valid_session(&"a".repeat(64)));
    }
}
