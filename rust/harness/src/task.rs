//! The work, the budget and the answer, as values.
//!
//! A task is not a prompt string. It is a goal and a root, because the harness needs to know where the
//! program it writes will run, and "where" is not something a prompt can be trusted to say.

use std::path::{Path, PathBuf};

/// One piece of work: what is wanted, and where it happens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSpec {
    goal: String,
    root: PathBuf,
}

impl TaskSpec {
    pub fn new(goal: impl Into<String>, root: impl Into<PathBuf>) -> Self {
        TaskSpec {
            goal: goal.into(),
            root: root.into(),
        }
    }

    pub fn goal(&self) -> &str {
        &self.goal
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// What a turn may spend. A turn is one round trip, so context is the only budget it has; the budgets
/// that span turns (wall clock, attempts, spend) belong to the caller that runs many turns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    context_tokens: usize,
}

impl Budget {
    pub fn new(context_tokens: usize) -> Self {
        Budget { context_tokens }
    }

    pub fn context_tokens(&self) -> usize {
        self.context_tokens
    }
}

/// What the model spent answering, when the model can say.
///
/// Reported by the endpoint rather than estimated, and carried all the way to the log: a turn that cost
/// 12,000 prompt tokens and a turn that cost 40 are the same turn to a reader looking only at the
/// program. A model that reports nothing leaves this at zero, which is a fact about the endpoint and not
/// a claim that the turn was free.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    pub prompt_tokens: u64,
    /// Of `prompt_tokens`, how many the provider served from its prefix cache. A long agent loop
    /// re-sends the whole transcript every step, so this is the difference between "60M tokens of
    /// work" and "60M tokens, almost all of them re-reads". Zero means the provider said nothing,
    /// not that nothing was cached.
    pub cached_tokens: u64,
    pub generated_tokens: u64,
    pub millis: u128,
    /// Characters of reasoning the model produced before its answer, when the endpoint reported any. The
    /// same thing the agentic bench records as a think-or-not column: a model that deliberates before
    /// every one-line program is a different tool from one that does not, even at the same score.
    pub reasoning_chars: u64,
}

impl Usage {
    /// Whether the endpoint said anything about cost at all.
    pub fn is_known(&self) -> bool {
        self.prompt_tokens > 0 || self.generated_tokens > 0
    }
}

/// What the model answered with: one draft.
///
/// An answer that could be two things at once would need a rule for which wins, and that rule would be a
/// bug waiting for the first model that answers with both. A program is still the default and the general
/// case; an edit is the narrow case that costs the edit rather than the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Draft {
    /// One shell program, run through the tool seam.
    Program(String),
    /// Anchored edits, planned and written by [`crate::patch`].
    Edits(Vec<crate::patch::FileEdit>),
    /// An answer that cannot be used, with the reason the next attempt should be shown. Not a fault: a
    /// model that writes a bad edit object can correct it, which is what the retry loop is for.
    Refused(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelAnswer {
    draft: Draft,
    usage: Option<Usage>,
    /// What the model said to itself before answering, capped. Kept because it is the most informative
    /// artifact when a model misbehaves: a model that repeats an exploration it already did has a reason,
    /// and the reason is in here. Diagnosing that from a character count is impossible, which is how two
    /// real runs were spent guessing.
    reasoning: Option<String>,
}

impl ModelAnswer {
    pub fn new(program: impl Into<String>) -> Self {
        ModelAnswer {
            draft: Draft::Program(program.into()),
            usage: None,
            reasoning: None,
        }
    }

    /// An answer that is edits rather than a program.
    pub fn editing(edits: Vec<crate::patch::FileEdit>) -> Self {
        ModelAnswer {
            draft: Draft::Edits(edits),
            usage: None,
            reasoning: None,
        }
    }

    /// An answer that says why it could not be used. The turn logs it and writes nothing.
    pub fn refused(reason: impl Into<String>) -> Self {
        ModelAnswer {
            draft: Draft::Refused(reason.into()),
            usage: None,
            reasoning: None,
        }
    }

    /// The same answer, with what the model thought first.
    pub fn with_reasoning(mut self, reasoning: impl Into<String>) -> Self {
        let text = reasoning.into();
        if !text.trim().is_empty() {
            self.reasoning = Some(text);
        }
        self
    }

    /// What the model said before it answered, when the endpoint reported any.
    pub fn reasoning(&self) -> Option<&str> {
        self.reasoning.as_deref()
    }

    /// The reasoning, cut to something a log can carry: enough to diagnose, not the whole deliberation.
    pub fn reasoning_for_log(&self) -> Option<String> {
        self.reasoning()
            .map(|text| crate::context::cut_head(text, 2000))
    }

    /// The same answer, with what the model said it spent. A scripted answer has none, because nobody
    /// spent anything.
    pub fn with_usage(mut self, usage: Usage) -> Self {
        self.usage = Some(usage);
        self
    }

    pub fn draft(&self) -> &Draft {
        &self.draft
    }

    pub fn program(&self) -> &str {
        match &self.draft {
            Draft::Program(program) => program,
            _ => "",
        }
    }

    pub fn edits(&self) -> Option<&[crate::patch::FileEdit]> {
        match &self.draft {
            Draft::Edits(edits) => Some(edits),
            _ => None,
        }
    }

    pub fn refusal(&self) -> Option<&str> {
        match &self.draft {
            Draft::Refused(reason) => Some(reason),
            _ => None,
        }
    }

    /// What the model wrote, as text: the program, the rendered edits, or the refusal. This is what the
    /// log, the judge and the next attempt read, so an edit names itself instead of leaving a blank.
    pub fn rendered(&self) -> String {
        match &self.draft {
            Draft::Program(program) => program.clone(),
            Draft::Edits(edits) => crate::patch::render(edits),
            Draft::Refused(reason) => format!("refused: {reason}"),
        }
    }

    /// What the endpoint said the answer cost, or nothing when it said nothing.
    pub fn usage(&self) -> Option<Usage> {
        self.usage
    }

    pub fn is_empty(&self) -> bool {
        match &self.draft {
            Draft::Program(program) => program.trim().is_empty(),
            Draft::Edits(edits) => edits.is_empty(),
            Draft::Refused(reason) => reason.trim().is_empty(),
        }
    }
}
