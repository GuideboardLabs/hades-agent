//! A question with options, which is what the ask bar renders.
//!
//! Two kinds, because two different things are being asked. A **permission** is raised by the floor:
//! the machine is about to do something the person should decide, and the options are allow once, allow
//! for the session, or deny. A **choice** is raised by the model: the work forks, both branches are
//! legitimate, and the person's preference decides. The distinction is enforced, not advisory: a choice
//! may not carry an allow effect, and a permission may not be a free-form fork.
//!
//! The shape is deliberately small. An option has a stable key (so a log can name it), a label (what the
//! bar shows), a detail (what changes if it is picked), an effect (what a permission does with it) and a
//! recommendation. `other` is reserved for the implicit decline row, so no producer can take the name.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::approval::{Answer, ApprovalBroker, ApprovalRequest, PendingId, Scope};
use crate::fault::Fault;
use crate::policy::Decision;

/// What kind of question this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskKind {
    /// The floor is asking whether an action may proceed.
    Permission,
    /// The work forks and the person's preference decides.
    Choice,
}

impl AskKind {
    pub fn name(self) -> &'static str {
        match self {
            AskKind::Permission => "permission",
            AskKind::Choice => "choice",
        }
    }

    pub fn parse(text: &str) -> Option<AskKind> {
        Some(match text {
            "permission" => AskKind::Permission,
            "choice" => AskKind::Choice,
            _ => return None,
        })
    }
}

/// What picking an option does. `Free` is a choice answer: the model reads the key, the broker records
/// it, and nothing is allowed or denied by it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    AllowOnce,
    AllowSession,
    Deny,
    Free,
}

impl Effect {
    pub fn name(self) -> &'static str {
        match self {
            Effect::AllowOnce => "allow-once",
            Effect::AllowSession => "allow-session",
            Effect::Deny => "deny",
            Effect::Free => "free",
        }
    }

    pub fn parse(text: &str) -> Option<Effect> {
        Some(match text {
            "allow-once" => Effect::AllowOnce,
            "allow-session" => Effect::AllowSession,
            "deny" => Effect::Deny,
            "free" => Effect::Free,
            _ => return None,
        })
    }

    /// The decision a permission answer records. A free choice records `allow` as "answered", and the
    /// option key is the real payload.
    pub fn decision(self) -> Decision {
        match self {
            Effect::Deny => Decision::Deny,
            _ => Decision::Allow,
        }
    }

    pub fn scope(self) -> Scope {
        match self {
            Effect::AllowSession => Scope::Session,
            _ => Scope::Once,
        }
    }

    fn allows(self) -> bool {
        matches!(self, Effect::AllowOnce | Effect::AllowSession)
    }
}

/// One row of the bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub key: String,
    pub label: String,
    pub detail: String,
    pub effect: Effect,
    pub recommended: bool,
}

impl Choice {
    pub fn new(
        key: impl Into<String>,
        label: impl Into<String>,
        detail: impl Into<String>,
        effect: Effect,
    ) -> Self {
        Choice {
            key: key.into(),
            label: label.into(),
            detail: detail.into(),
            effect,
            recommended: false,
        }
    }

    pub fn recommended(mut self) -> Self {
        self.recommended = true;
        self
    }

    fn json(&self) -> Value {
        json!({
            "key": self.key,
            "label": self.label,
            "detail": self.detail,
            "effect": self.effect.name(),
            "recommended": self.recommended,
        })
    }
}

/// A question, its options, and what happens if nobody answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    pub kind: AskKind,
    /// A stable name for the family of question, so a preference can be learned for "storage" without
    /// tying it to one wording. Empty means: derive it from the prompt.
    pub family: String,
    pub prompt: String,
    pub why: String,
    /// A blocking question stops the run. An advisory one takes the default and continues.
    pub blocking: bool,
    /// The key taken when the question is not answered in time. Required for a blocking question.
    pub default: Option<String>,
    pub options: Vec<Choice>,
}

impl Ask {
    /// The floor's question. Three options, and `deny` is the default so a timeout is safe.
    pub fn permission(prompt: impl Into<String>, why: impl Into<String>) -> Self {
        Ask {
            kind: AskKind::Permission,
            family: "permission".into(),
            prompt: prompt.into(),
            why: why.into(),
            blocking: true,
            default: Some("deny".into()),
            options: vec![
                Choice::new(
                    "allow-once",
                    "Allow once",
                    "this action, this turn only",
                    Effect::AllowOnce,
                )
                .recommended(),
                Choice::new(
                    "allow-session",
                    "Allow this session",
                    "the same action until this chat ends",
                    Effect::AllowSession,
                ),
                Choice::new(
                    "deny",
                    "Deny",
                    "no change; Hades continues without it",
                    Effect::Deny,
                ),
            ],
        }
    }

    /// A fork from the model. The caller supplies the options and marks one recommended.
    pub fn choice(
        prompt: impl Into<String>,
        why: impl Into<String>,
        blocking: bool,
        options: Vec<Choice>,
    ) -> Self {
        // A blocking question always has a safe default: its own recommendation, if it named one.
        let default = if blocking {
            options
                .iter()
                .find(|choice| choice.recommended)
                .map(|choice| choice.key.clone())
        } else {
            None
        };
        Ask {
            kind: AskKind::Choice,
            family: String::new(),
            prompt: prompt.into(),
            why: why.into(),
            blocking,
            default,
            options,
        }
    }

    pub fn recommended(&self) -> Option<&Choice> {
        self.options.iter().find(|choice| choice.recommended)
    }

    pub fn option(&self, key: &str) -> Option<&Choice> {
        self.options.iter().find(|choice| choice.key == key)
    }

    /// The key taken when nobody answers: the named default, else the recommendation, else deny.
    pub fn fallback(&self) -> String {
        self.default
            .clone()
            .or_else(|| self.recommended().map(|choice| choice.key.clone()))
            .unwrap_or_else(|| "deny".into())
    }

    pub fn validate(&self) -> Result<(), String> {
        let prompt = self.prompt.trim();
        if prompt.is_empty() {
            return Err("a question needs a prompt".into());
        }
        if prompt.chars().count() > 240 {
            return Err("a question prompt is limited to 240 characters".into());
        }
        if self.why.trim().is_empty() {
            return Err("a question needs a reason, in one sentence".into());
        }
        if self.why.chars().count() > 240 {
            return Err("a question reason is limited to 240 characters".into());
        }
        if self.options.len() < 2 || self.options.len() > 4 {
            return Err("a question needs 2 to 4 options".into());
        }
        let mut seen = std::collections::BTreeSet::new();
        for choice in &self.options {
            if choice.key == "other" {
                return Err("the key `other` is reserved for the decline row".into());
            }
            if choice.key.is_empty()
                || choice.key.len() > 32
                || !choice
                    .key
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            {
                return Err(format!(
                    "option key `{}` must be lowercase letters, digits or '-'",
                    choice.key
                ));
            }
            if !seen.insert(choice.key.clone()) {
                return Err(format!("option key `{}` appears twice", choice.key));
            }
            if choice.label.trim().is_empty() {
                return Err(format!("option `{}` needs a label", choice.key));
            }
            if choice.label.chars().count() > 60 {
                return Err(format!(
                    "option `{}` label is limited to 60 characters",
                    choice.key
                ));
            }
            if choice.detail.trim().is_empty() {
                return Err(format!(
                    "option `{}` needs a detail: what changes if it is picked",
                    choice.key
                ));
            }
            if choice.detail.chars().count() > 160 {
                return Err(format!(
                    "option `{}` detail is limited to 160 characters",
                    choice.key
                ));
            }
        }
        let recommended = self
            .options
            .iter()
            .filter(|choice| choice.recommended)
            .count();
        if recommended != 1 {
            return Err(format!(
                "exactly one option must be recommended, found {recommended}"
            ));
        }
        match self.kind {
            AskKind::Permission => {
                if self
                    .options
                    .iter()
                    .any(|choice| choice.effect == Effect::Free)
                {
                    return Err("a permission option cannot have the free effect".into());
                }
                if !self.options.iter().any(|choice| choice.effect.allows()) {
                    return Err("a permission needs an allow option".into());
                }
                if !self
                    .options
                    .iter()
                    .any(|choice| choice.effect == Effect::Deny)
                {
                    return Err("a permission needs a deny option".into());
                }
                if self
                    .recommended()
                    .is_some_and(|choice| !choice.effect.allows())
                {
                    return Err("a permission cannot recommend denying itself".into());
                }
            }
            AskKind::Choice => {
                if self
                    .options
                    .iter()
                    .any(|choice| choice.effect != Effect::Free)
                {
                    return Err("a choice option must have the free effect".into());
                }
            }
        }
        if self.blocking {
            let default = self
                .default
                .as_deref()
                .ok_or("a blocking question needs a default option")?;
            if self.option(default).is_none() {
                return Err(format!("the default `{default}` is not an option"));
            }
        } else if let Some(default) = self.default.as_deref()
            && self.option(default).is_none()
        {
            return Err(format!("the default `{default}` is not an option"));
        }
        Ok(())
    }

    /// The family this question belongs to. Permissions are their own family; a choice uses the explicit
    /// family or a slug of its prompt, which is stable enough to learn from and short enough to print.
    pub fn family(&self) -> String {
        if !self.family.is_empty() {
            return self.family.clone();
        }
        slug(&self.prompt)
    }

    pub fn json(&self) -> Value {
        json!({
            "kind": self.kind.name(),
            "family": self.family(),
            "prompt": self.prompt,
            "why": self.why,
            "blocking": self.blocking,
            "default": self.default,
            "options": self.options.iter().map(Choice::json).collect::<Vec<_>>(),
        })
    }

    pub fn parse(value: &Value) -> Result<Ask, String> {
        let text = |key: &str| -> Result<String, String> {
            value[key]
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("ask.{key} is not a string"))
        };
        let kind = value["kind"]
            .as_str()
            .and_then(AskKind::parse)
            .ok_or("ask.kind is not permission or choice")?;
        let options = value["options"]
            .as_array()
            .ok_or("ask.options is not an array")?
            .iter()
            .map(|option| -> Result<Choice, String> {
                let field = |key: &str| -> Result<String, String> {
                    option[key]
                        .as_str()
                        .map(str::to_string)
                        .ok_or_else(|| format!("ask option.{key} is not a string"))
                };
                let effect = option["effect"]
                    .as_str()
                    .and_then(Effect::parse)
                    .ok_or("ask option.effect is unknown")?;
                Ok(Choice {
                    key: field("key")?,
                    label: field("label")?,
                    detail: field("detail")?,
                    effect,
                    recommended: option["recommended"].as_bool().unwrap_or(false),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let ask = Ask {
            kind,
            family: value["family"].as_str().unwrap_or("").to_string(),
            prompt: text("prompt")?,
            why: text("why")?,
            blocking: value["blocking"].as_bool().unwrap_or(false),
            default: value["default"].as_str().map(str::to_string),
            options,
        };
        ask.validate()?;
        Ok(ask)
    }
}

/// How many questions a surface may raise before it has to decide for itself.
#[derive(Debug)]
pub struct Budget {
    limit: u32,
    used: AtomicU32,
}

impl Budget {
    pub fn new(limit: u32) -> Self {
        Budget {
            limit,
            used: AtomicU32::new(0),
        }
    }

    /// Take one question. `false` means the budget is spent.
    pub fn take(&self) -> bool {
        let mut current = self.used.load(Ordering::SeqCst);
        loop {
            if current >= self.limit {
                return false;
            }
            match self.used.compare_exchange(
                current,
                current + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return true,
                Err(seen) => current = seen,
            }
        }
    }

    pub fn left(&self) -> u32 {
        self.limit.saturating_sub(self.used.load(Ordering::SeqCst))
    }
}

/// An answer that arrived after the run had already taken its recommendation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Late {
    pub id: PendingId,
    pub assumed: String,
    pub chosen: Option<String>,
    pub note: Option<String>,
}

/// The question surface a chat is handed: one broker, one budget, and the wait.
pub struct Desk {
    broker: Arc<dyn ApprovalBroker + Send + Sync>,
    budget: Budget,
    /// Questions the run moved past. Their answers are delivered at the next step boundary instead of
    /// pretending the work can be rewound.
    assumed: std::sync::Mutex<Vec<(PendingId, String)>>,
}

impl std::fmt::Debug for Desk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Desk")
            .field("questions_left", &self.budget.left())
            .finish_non_exhaustive()
    }
}

impl Desk {
    pub fn new(broker: Arc<dyn ApprovalBroker + Send + Sync>, limit: u32) -> Arc<Self> {
        Arc::new(Desk {
            broker,
            budget: Budget::new(limit),
            assumed: std::sync::Mutex::new(Vec::new()),
        })
    }

    /// Remember that the run continued without this answer, so a later answer can be delivered.
    pub fn note_assumed(&self, id: PendingId, key: &str) {
        if let Ok(mut assumed) = self.assumed.lock() {
            assumed.push((id, key.to_string()));
        }
    }

    /// Answers that arrived for questions already assumed. Each is delivered once.
    pub fn drain_late(&self) -> Vec<Late> {
        let Ok(mut assumed) = self.assumed.lock() else {
            return Vec::new();
        };
        let mut late = Vec::new();
        assumed.retain(|(id, key)| match self.broker.answer(*id) {
            Ok(Some(answer)) => {
                if answer.option.as_deref() != Some(key.as_str()) || answer.note.is_some() {
                    late.push(Late {
                        id: *id,
                        assumed: key.clone(),
                        chosen: answer.option,
                        note: answer.note,
                    });
                }
                false
            }
            _ => true,
        });
        late
    }

    /// A floor question. Permissions do not spend the model's question budget: safety is not a quota.
    pub fn permission(
        &self,
        session: &str,
        action: &str,
        reason: &str,
        ask: Ask,
    ) -> Result<ApprovalRequest, Fault> {
        self.broker.request_ask(session, action, reason, Some(ask))
    }

    /// A model question. `None` means the budget is spent and the model must decide.
    pub fn choice(
        &self,
        session: &str,
        action: &str,
        reason: &str,
        ask: Ask,
    ) -> Result<Option<ApprovalRequest>, Fault> {
        if !self.budget.take() {
            return Ok(None);
        }
        self.broker
            .request_ask(session, action, reason, Some(ask))
            .map(Some)
    }

    pub fn answer(&self, id: PendingId) -> Result<Option<Answer>, Fault> {
        self.broker.answer(id)
    }

    pub fn wait(
        &self,
        id: PendingId,
        timeout: Duration,
        stop: Option<&AtomicBool>,
    ) -> Result<Option<Answer>, Fault> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(answer) = self.broker.answer(id)? {
                return Ok(Some(answer));
            }
            if stop.is_some_and(|flag| flag.load(Ordering::SeqCst)) || Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    pub fn left(&self) -> u32 {
        self.budget.left()
    }
}

/// A stable, printable family name from a question's wording.
fn slug(text: &str) -> String {
    let mut out = String::new();
    let mut gap = false;
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            if gap && !out.is_empty() {
                out.push('.');
            }
            gap = false;
            out.extend(ch.to_lowercase());
        } else {
            gap = true;
        }
        if out.len() >= 48 {
            break;
        }
    }
    if out.is_empty() { "choice".into() } else { out }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choice(key: &str, recommended: bool) -> Choice {
        let mut choice = Choice::new(key, key, "a branch", Effect::Free);
        choice.recommended = recommended;
        choice
    }

    #[test]
    fn the_floor_question_is_valid_and_defaults_to_deny() {
        let ask = Ask::permission("Delete 12 files?", "the delete cannot be undone");
        ask.validate().expect("valid");
        assert_eq!(ask.fallback(), "deny");
        assert_eq!(
            ask.recommended().map(|choice| choice.key.as_str()),
            Some("allow-once")
        );
    }

    #[test]
    fn a_choice_needs_exactly_one_recommendation() {
        let none = Ask::choice(
            "Which store?",
            "both fit",
            false,
            vec![choice("a", false), choice("b", false)],
        );
        assert!(none.validate().unwrap_err().contains("exactly one"));

        let two = Ask::choice(
            "Which store?",
            "both fit",
            false,
            vec![choice("a", true), choice("b", true)],
        );
        assert!(two.validate().unwrap_err().contains("exactly one"));
    }

    #[test]
    fn duplicate_and_reserved_keys_are_refused() {
        let duplicate = Ask::choice(
            "Which?",
            "fork",
            false,
            vec![choice("a", true), choice("a", false)],
        );
        assert!(duplicate.validate().unwrap_err().contains("twice"));

        let reserved = Ask::choice(
            "Which?",
            "fork",
            false,
            vec![choice("other", true), choice("b", false)],
        );
        assert!(reserved.validate().unwrap_err().contains("reserved"));
    }

    #[test]
    fn a_blocking_question_names_a_real_default() {
        let mut ask = Ask::choice(
            "Which?",
            "fork",
            true,
            vec![choice("a", true), choice("b", false)],
        );
        assert_eq!(
            ask.default.as_deref(),
            Some("a"),
            "a blocking choice takes its recommendation as the default"
        );
        ask.default = None;
        assert!(ask.validate().unwrap_err().contains("default"));
        ask.default = Some("missing".into());
        assert!(ask.validate().unwrap_err().contains("not an option"));
        ask.default = Some("a".into());
        ask.validate().expect("valid");
    }

    #[test]
    fn a_choice_cannot_carry_an_allow_effect() {
        let mut options = vec![choice("a", true), choice("b", false)];
        options[0].effect = Effect::AllowOnce;
        let ask = Ask::choice("Which?", "fork", false, options);
        assert!(ask.validate().unwrap_err().contains("free effect"));
    }

    #[test]
    fn a_question_round_trips_through_json() {
        let ask = Ask::permission("Delete 12 files?", "not reversible");
        let parsed = Ask::parse(&ask.json()).expect("round trip");
        assert_eq!(parsed, ask);
    }

    #[test]
    fn a_spent_budget_refuses_more_questions() {
        let budget = Budget::new(2);
        assert!(budget.take());
        assert!(budget.take());
        assert!(!budget.take());
        assert_eq!(budget.left(), 0);
    }

    #[test]
    fn an_answer_after_an_assumption_is_delivered_once() {
        use crate::approval::{ApprovalBroker as _, InMemory};
        let broker = Arc::new(InMemory::new());
        let desk = Desk::new(broker.clone(), 3);
        let ask = Ask::choice(
            "Which store?",
            "both fit",
            false,
            vec![
                Choice::new("sqlite", "SQLite", "one file", Effect::Free).recommended(),
                Choice::new("postgres", "Postgres", "a server", Effect::Free),
            ],
        );
        let prompt = ask.prompt.clone();
        let request = desk
            .choice("chat", "question", &prompt, ask)
            .expect("the question is recorded")
            .expect("a question");
        desk.note_assumed(request.id, "sqlite");
        assert!(desk.drain_late().is_empty(), "nothing answered yet");
        broker
            .resolve_choice(request.id, "postgres", Some("we run one"), "tui")
            .expect("the late answer");
        let late = desk.drain_late();
        assert_eq!(late.len(), 1);
        assert_eq!(late[0].assumed, "sqlite");
        assert_eq!(late[0].chosen.as_deref(), Some("postgres"));
        assert_eq!(late[0].note.as_deref(), Some("we run one"));
        assert!(desk.drain_late().is_empty(), "delivered once");
    }
}
