//! The approval broker: the question a policy can raise, as an object that outlives the process.
//!
//! This is the piece the design says every other harness got wrong, and the specific failure is worth
//! naming: a broker held in memory loses the question when the process restarts, so the run resumes with
//! the question gone and nobody notices. Here a question is a *record*. It has an identity, it is written
//! before anyone answers it, and it can be answered by policy, by a person, or over an API, from another
//! process, in any order.
//!
//! Three rules the shape enforces:
//!
//! - **An answer has a scope.** `Once` answers the question that was asked; `Session` answers the action
//!   for the rest of the session it was raised in. Without the distinction a person is asked the same
//!   question every turn, and a person asked the same question every turn stops reading it.
//! - **An answer says who gave it.** The record keeps it, because a decision nobody can attribute is a
//!   decision nobody can revisit.
//! - **An answer is not silently revisable.** Answering a question twice is refused rather than applied, so
//!   the record stays a record of what was decided, by whom, and when.
//!
//! One implementation of those rules ([`Ledger`]), two places to keep it: in memory for one process, and on
//! disk for the case that matters. Two copies of a governance rule drift apart, and the drift is silent.
//!
//! Stated limits, because a guarantee is only worth what it excludes: the file is append-only JSONL with a
//! single writer assumed, no lock and no `fsync`, so a machine that loses power can lose the last line. What
//! it does guarantee is that a *separate process* sees every question that was recorded, which is the
//! property the design asks for.

use std::collections::BTreeMap;
use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{Value, json};
use session_tree::{Clock, SystemClock};

use crate::fault::Fault;
use crate::policy::Decision;
use crate::question::Ask;

/// How long an answer lasts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// This question, once. The next turn asks again.
    Once,
    /// This action, for the rest of the session the question was raised in.
    Session,
}

impl Scope {
    pub fn name(self) -> &'static str {
        match self {
            Scope::Once => "once",
            Scope::Session => "session",
        }
    }

    fn parse(text: &str) -> Option<Scope> {
        Some(match text {
            "once" => Scope::Once,
            "session" => Scope::Session,
            _ => return None,
        })
    }
}

/// The identity of a question. Numbered rather than random: two runs that reach the same second question
/// produce the same ids, which is what makes a record diffable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PendingId(pub u64);

impl fmt::Display for PendingId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// A question, and what it is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRequest {
    pub id: PendingId,
    /// The session the question was raised in. A `Session`-scoped answer is keyed to it, so an answer given
    /// during one piece of work cannot silently loosen another.
    pub session: String,
    pub action: String,
    pub reason: String,
    pub at: String,
    /// The options the ask bar renders. `None` on a legacy record, which is answered by the three
    /// built-in choices.
    pub ask: Option<Ask>,
}

/// A decision about a question, with who made it and how long it lasts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    pub decision: Decision,
    pub scope: Scope,
    pub by: String,
    pub at: String,
    /// The option key that was picked, when the question had options.
    pub option: Option<String>,
    /// The person's own words when they declined with `Other`.
    pub note: Option<String>,
}

/// A standing answer for an action in a session: what was decided, by whom, from which question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Standing {
    pub decision: Decision,
    pub by: String,
    pub question: PendingId,
    pub scope: Scope,
}

/// One question and what came of it, for a surface that renders them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub request: ApprovalRequest,
    pub answer: Option<Answer>,
}

/// Raising questions, and answering them.
///
/// Deliberately not `&mut self`: a question outlives the call that raised it, and a surface answering one
/// must not have to hold the run in order to do it.
pub trait ApprovalBroker {
    fn request(&self, session: &str, action: &str, reason: &str) -> Result<ApprovalRequest, Fault> {
        self.request_ask(session, action, reason, None)
    }
    /// Raise a question with the options a surface renders. `None` is a legacy permission question.
    fn request_ask(
        &self,
        session: &str,
        action: &str,
        reason: &str,
        ask: Option<Ask>,
    ) -> Result<ApprovalRequest, Fault>;
    /// Every question in order, answered or not.
    fn questions(&self) -> Result<Vec<Question>, Fault>;
    /// The answer if there is one. `None` means still pending, which is a state and not an error.
    fn answer(&self, id: PendingId) -> Result<Option<Answer>, Fault>;
    /// A `Session`-scoped answer for this action, which is what stops a person being asked twice.
    fn standing(&self, session: &str, action: &str) -> Result<Option<Standing>, Fault>;
    fn resolve(
        &self,
        id: PendingId,
        decision: Decision,
        scope: Scope,
        by: &str,
    ) -> Result<Answer, Fault> {
        self.resolve_answer(id, decision, scope, None, None, by)
    }
    /// Answer with the option key the bar showed. The effect on the question decides allow or deny.
    fn resolve_choice(
        &self,
        id: PendingId,
        key: &str,
        note: Option<&str>,
        by: &str,
    ) -> Result<Answer, Fault> {
        let question = self
            .questions()?
            .into_iter()
            .find(|question| question.request.id == id)
            .ok_or_else(|| Fault::harness(format!("no question {id} was ever asked")))?;
        let ask = question
            .request
            .ask
            .ok_or_else(|| Fault::harness(format!("question {id} has no options")))?;
        let choice = ask
            .option(key)
            .ok_or_else(|| Fault::harness(format!("question {id} has no option `{key}`")))?;
        self.resolve_answer(
            id,
            choice.effect.decision(),
            choice.effect.scope(),
            Some(key),
            note,
            by,
        )
    }
    fn resolve_answer(
        &self,
        id: PendingId,
        decision: Decision,
        scope: Scope,
        option: Option<&str>,
        note: Option<&str>,
        by: &str,
    ) -> Result<Answer, Fault>;
}

/// One line of the record.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Event {
    Asked {
        id: PendingId,
        session: String,
        action: String,
        reason: String,
        at: String,
        ask: Option<Ask>,
    },
    Answered {
        id: PendingId,
        decision: Decision,
        scope: Scope,
        by: String,
        at: String,
        option: Option<String>,
        note: Option<String>,
    },
}

impl Event {
    fn line(&self) -> String {
        let value = match self {
            Event::Asked {
                id,
                session,
                action,
                reason,
                at,
                ask,
            } => json!({
                "v": 1,
                "event": "asked",
                "id": id.0,
                "session": session,
                "action": action,
                "reason": reason,
                "at": at,
                "ask": ask.as_ref().map(Ask::json),
            }),
            Event::Answered {
                id,
                decision,
                scope,
                by,
                at,
                option,
                note,
            } => json!({
                "v": 1,
                "event": "answered",
                "id": id.0,
                "decision": decision.name(),
                "scope": scope.name(),
                "by": by,
                "at": at,
                "option": option,
                "note": note,
            }),
        };
        value.to_string()
    }

    /// `line` is the line number, so a record that cannot be read fails with its location rather than being
    /// skipped. A skipped line is a question that quietly stopped existing.
    fn parse(text: &str, line: usize) -> Result<Event, Fault> {
        let value: Value = serde_json::from_str(text)
            .map_err(|error| Fault::harness(format!("line {line}: not JSON: {error}")))?;

        let number = |key: &str| -> Result<u64, Fault> {
            value[key]
                .as_u64()
                .ok_or_else(|| Fault::harness(format!("line {line}: {key} is not a number")))
        };
        let text_of = |key: &str| -> Result<String, Fault> {
            value[key]
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| Fault::harness(format!("line {line}: {key} is not a string")))
        };

        match value["event"].as_str() {
            Some("asked") => Ok(Event::Asked {
                id: PendingId(number("id")?),
                session: text_of("session")?,
                action: text_of("action")?,
                reason: text_of("reason")?,
                at: text_of("at")?,
                ask: match value.get("ask") {
                    None | Some(Value::Null) => None,
                    Some(ask) => Some(
                        Ask::parse(ask)
                            .map_err(|error| Fault::harness(format!("line {line}: {error}")))?,
                    ),
                },
            }),
            Some("answered") => {
                let decision = match value["decision"].as_str() {
                    Some("allow") => Decision::Allow,
                    Some("deny") => Decision::Deny,
                    Some("ask") => Decision::Ask,
                    _ => return Err(Fault::harness(format!("line {line}: unknown decision"))),
                };
                let scope = value["scope"]
                    .as_str()
                    .and_then(Scope::parse)
                    .ok_or_else(|| Fault::harness(format!("line {line}: unknown scope")))?;
                Ok(Event::Answered {
                    id: PendingId(number("id")?),
                    decision,
                    scope,
                    by: text_of("by")?,
                    at: text_of("at")?,
                    option: value["option"].as_str().map(str::to_string),
                    note: value["note"].as_str().map(str::to_string),
                })
            }
            _ => Err(Fault::harness(format!("line {line}: unknown event"))),
        }
    }
}

/// The record in memory, and the rules over it.
///
/// This is the only place the three rules live. Where the record is kept is somebody else's problem, and
/// the mutators hand back the [`Event`] they folded so that a store writes exactly what was applied and can
/// never record something the rules did not see.
#[derive(Debug, Clone, Default)]
struct Ledger {
    asked: Vec<ApprovalRequest>,
    answers: BTreeMap<u64, Answer>,
}

/// The fields of one answer, grouped so the ledger's resolve call stays readable.
struct Answering<'a> {
    decision: Decision,
    scope: Scope,
    option: Option<&'a str>,
    note: Option<&'a str>,
    by: &'a str,
    at: &'a str,
}

impl Ledger {
    fn of(text: &str) -> Result<Ledger, Fault> {
        let mut ledger = Ledger::default();
        for (index, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            ledger.apply(Event::parse(line, index + 1)?);
        }
        Ok(ledger)
    }

    fn apply(&mut self, event: Event) {
        match event {
            Event::Asked {
                id,
                session,
                action,
                reason,
                at,
                ask,
            } => self.asked.push(ApprovalRequest {
                id,
                session,
                action,
                reason,
                at,
                ask,
            }),
            Event::Answered {
                id,
                decision,
                scope,
                by,
                at,
                option,
                note,
            } => {
                self.answers.insert(
                    id.0,
                    Answer {
                        decision,
                        scope,
                        by,
                        at,
                        option,
                        note,
                    },
                );
            }
        }
    }

    fn next_id(&self) -> PendingId {
        PendingId(self.asked.last().map_or(0, |request| request.id.0 + 1))
    }

    fn request_of(&self, id: PendingId) -> Option<&ApprovalRequest> {
        self.asked.iter().find(|request| request.id == id)
    }

    fn answer(&self, id: PendingId) -> Option<Answer> {
        self.answers.get(&id.0).cloned()
    }

    /// The newest `Session`-scoped answer for this action. Newest wins, so a later decision supersedes an
    /// earlier one instead of the pair of them being ambiguous.
    fn standing(&self, session: &str, action: &str) -> Option<Standing> {
        self.asked
            .iter()
            .filter(|request| request.session == session && request.action == action)
            .filter_map(|request| {
                self.answers
                    .get(&request.id.0)
                    .filter(|answer| answer.scope == Scope::Session)
                    .map(|answer| Standing {
                        decision: answer.decision,
                        by: answer.by.clone(),
                        question: request.id,
                        scope: answer.scope,
                    })
            })
            .next_back()
    }

    fn questions(&self) -> Vec<Question> {
        self.asked
            .iter()
            .map(|request| Question {
                request: request.clone(),
                answer: self.answer(request.id),
            })
            .collect()
    }

    fn ask(
        &mut self,
        session: &str,
        action: &str,
        reason: &str,
        ask: Option<Ask>,
        at: &str,
    ) -> (Event, ApprovalRequest) {
        let event = Event::Asked {
            id: self.next_id(),
            session: session.to_string(),
            action: action.to_string(),
            reason: reason.to_string(),
            at: at.to_string(),
            ask,
        };
        self.apply(event.clone());
        let request = self
            .request_of(match &event {
                Event::Asked { id, .. } => *id,
                Event::Answered { .. } => unreachable!("ask builds a question"),
            })
            .cloned()
            .expect("the question was just added");
        (event, request)
    }

    /// Whether this answer may be recorded. In one place, so both stores refuse the same things.
    fn check_resolvable(&self, id: PendingId) -> Result<(), Fault> {
        self.request_of(id)
            .ok_or_else(|| Fault::harness(format!("no question {id} was ever asked")))?;
        if let Some(answer) = self.answers.get(&id.0) {
            return Err(Fault::harness(format!(
                "question {id} was already answered {} by {}",
                answer.decision.name(),
                answer.by
            )));
        }
        Ok(())
    }

    fn resolve(&mut self, id: PendingId, answer: Answering<'_>) -> Result<(Event, Answer), Fault> {
        self.check_resolvable(id)?;
        let event = Event::Answered {
            id,
            decision: answer.decision,
            scope: answer.scope,
            by: answer.by.to_string(),
            at: answer.at.to_string(),
            option: answer.option.map(str::to_string),
            note: answer.note.map(str::to_string),
        };
        self.apply(event.clone());
        let answer = self.answer(id).expect("the answer was just added");
        Ok((event, answer))
    }
}

/// A broker for a single process: the same rules, no file.
pub struct InMemory {
    ledger: Mutex<Ledger>,
    clock: Box<dyn Clock + Send + Sync>,
}

impl fmt::Debug for InMemory {
    /// Hand-written because the clock is a trait object: a broker's record is worth printing, a clock is not.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InMemory")
            .field("questions", &self.lock().asked.len())
            .finish_non_exhaustive()
    }
}

impl Default for InMemory {
    fn default() -> Self {
        InMemory::new()
    }
}

impl InMemory {
    pub fn new() -> Self {
        InMemory::with_clock(SystemClock)
    }

    /// With a clock of your own, so a record can be asserted on.
    pub fn with_clock(clock: impl Clock + Send + Sync + 'static) -> Self {
        InMemory {
            ledger: Mutex::new(Ledger::default()),
            clock: Box::new(clock),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Ledger> {
        self.ledger.lock().expect("the broker lock")
    }
}

impl ApprovalBroker for InMemory {
    fn request_ask(
        &self,
        session: &str,
        action: &str,
        reason: &str,
        ask: Option<Ask>,
    ) -> Result<ApprovalRequest, Fault> {
        let at = self.clock.now();
        let (_, request) = self.lock().ask(session, action, reason, ask, &at);
        Ok(request)
    }

    fn questions(&self) -> Result<Vec<Question>, Fault> {
        Ok(self.lock().questions())
    }

    fn answer(&self, id: PendingId) -> Result<Option<Answer>, Fault> {
        Ok(self.lock().answer(id))
    }

    fn standing(&self, session: &str, action: &str) -> Result<Option<Standing>, Fault> {
        Ok(self.lock().standing(session, action))
    }

    fn resolve_answer(
        &self,
        id: PendingId,
        decision: Decision,
        scope: Scope,
        option: Option<&str>,
        note: Option<&str>,
        by: &str,
    ) -> Result<Answer, Fault> {
        let at = self.clock.now();
        let (_, answer) = self.lock().resolve(
            id,
            Answering {
                decision,
                scope,
                option,
                note,
                by,
                at: &at,
            },
        )?;
        Ok(answer)
    }
}

/// A broker that keeps the record in a file, which is what makes a question outlive the process.
pub struct OnDisk {
    path: PathBuf,
    clock: Box<dyn Clock + Send + Sync>,
}

impl fmt::Debug for OnDisk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OnDisk")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl OnDisk {
    /// Open the record at `path`, creating it if it is not there.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, Fault> {
        OnDisk::opened_with(path, SystemClock)
    }

    pub fn opened_with(
        path: impl Into<PathBuf>,
        clock: impl Clock + Send + Sync + 'static,
    ) -> Result<Self, Fault> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| Fault::harness(format!("{}: {error}", parent.display())))?;
        }
        if !path.exists() {
            std::fs::write(&path, b"")
                .map_err(|error| Fault::harness(format!("{}: {error}", path.display())))?;
        }
        OnDisk::read(&path)?;
        Ok(OnDisk {
            path,
            clock: Box::new(clock),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read the whole record. The file is small and reading it whole is what makes two processes agreeing
    /// trivial, with no cache to invalidate and no state to get stale.
    fn read(path: &Path) -> Result<Ledger, Fault> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| Fault::harness(format!("{}: {error}", path.display())))?;
        Ledger::of(&text)
    }

    fn append(&self, event: &Event) -> Result<(), Fault> {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&self.path)
            .map_err(|error| Fault::harness(format!("{}: {error}", self.path.display())))?;
        writeln!(file, "{}", event.line())
            .map_err(|error| Fault::harness(format!("{}: {error}", self.path.display())))
    }
}

impl ApprovalBroker for OnDisk {
    fn request_ask(
        &self,
        session: &str,
        action: &str,
        reason: &str,
        ask: Option<Ask>,
    ) -> Result<ApprovalRequest, Fault> {
        let at = self.clock.now();
        let mut ledger = OnDisk::read(&self.path)?;
        let (event, request) = ledger.ask(session, action, reason, ask, &at);
        self.append(&event)?;
        Ok(request)
    }

    fn questions(&self) -> Result<Vec<Question>, Fault> {
        Ok(OnDisk::read(&self.path)?.questions())
    }

    fn answer(&self, id: PendingId) -> Result<Option<Answer>, Fault> {
        Ok(OnDisk::read(&self.path)?.answer(id))
    }

    fn standing(&self, session: &str, action: &str) -> Result<Option<Standing>, Fault> {
        Ok(OnDisk::read(&self.path)?.standing(session, action))
    }

    fn resolve_answer(
        &self,
        id: PendingId,
        decision: Decision,
        scope: Scope,
        option: Option<&str>,
        note: Option<&str>,
        by: &str,
    ) -> Result<Answer, Fault> {
        let at = self.clock.now();
        let mut ledger = OnDisk::read(&self.path)?;
        let (event, answer) = ledger.resolve(
            id,
            Answering {
                decision,
                scope,
                option,
                note,
                by,
                at: &at,
            },
        )?;
        self.append(&event)?;
        Ok(answer)
    }
}

#[cfg(test)]
mod tests {

    /// A scratch base outside the source tree: the build directory when cargo names one, the temp
    /// directory otherwise. Never `<manifest>/../target`, which belongs to the build and, on a machine
    /// with a nearly full root filesystem, is the wrong disk to fill.
    fn scratch_base() -> std::path::PathBuf {
        crate::scratch::root()
    }

    use super::*;

    #[derive(Clone)]
    struct FixedClock;

    impl Clock for FixedClock {
        fn now(&self) -> String {
            "2026-10-17T00:00:00+00:00".to_string()
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = scratch_base().join(format!("hades-broker-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        dir
    }

    fn record(name: &str) -> PathBuf {
        scratch(name).join("approvals.jsonl")
    }

    #[test]
    fn a_question_asked_in_one_process_is_visible_in_the_next() {
        let path = record("reopen");
        {
            let broker = OnDisk::opened_with(&path, FixedClock).expect("a broker");
            let asked = broker
                .request("cli", "shell", "policy asked")
                .expect("a question");
            assert_eq!(asked.id, PendingId(0));
            assert_eq!(asked.session, "cli");
            assert_eq!(asked.at, "2026-10-17T00:00:00+00:00");
            assert_eq!(broker.answer(asked.id).expect("readable"), None);
        }

        // A different instance with no shared memory over the same file: the design's whole point.
        let broker = OnDisk::opened_with(&path, FixedClock).expect("the same record");
        let questions = broker.questions().expect("readable");
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].request.action, "shell");
        assert_eq!(questions[0].request.reason, "policy asked");
        assert_eq!(
            questions[0].answer, None,
            "still unanswered after a restart"
        );
        assert_eq!(broker.answer(PendingId(0)).expect("readable"), None);

        let answer = broker
            .resolve(PendingId(0), Decision::Allow, Scope::Session, "operator")
            .expect("answered");
        assert_eq!(answer.by, "operator");
        assert_eq!(answer.scope, Scope::Session);
    }

    #[test]
    fn an_asked_question_survives_a_broker_that_never_saw_it_asked() {
        let path = record("second");
        OnDisk::opened_with(&path, FixedClock)
            .expect("a broker")
            .request("one", "shell", "asked")
            .expect("asked");

        // A second writer continues the same record and does not reuse the id.
        let other = OnDisk::opened_with(&path, FixedClock).expect("the same record");
        let next = other.request("two", "vault", "asked").expect("asked");
        assert_eq!(next.id, PendingId(1));
        assert_eq!(other.questions().expect("readable").len(), 2);
    }

    #[test]
    fn a_session_answer_stands_for_the_action_and_a_once_answer_does_not() {
        let path = record("standing");
        let broker = OnDisk::opened_with(&path, FixedClock).expect("a broker");

        let once = broker.request("cli", "shell", "asked").expect("asked");
        broker
            .resolve(once.id, Decision::Allow, Scope::Once, "operator")
            .expect("answered");
        assert_eq!(
            broker.standing("cli", "shell").expect("readable"),
            None,
            "an answer to one question must not answer the next"
        );

        let more = broker
            .request("cli", "shell", "asked again")
            .expect("asked");
        broker
            .resolve(more.id, Decision::Allow, Scope::Session, "operator")
            .expect("answered");
        let standing = broker
            .standing("cli", "shell")
            .expect("readable")
            .expect("a standing answer");
        assert_eq!(standing.decision, Decision::Allow);
        assert_eq!(standing.by, "operator");
        assert_eq!(standing.question, more.id);

        // Scoped to the session and the action, and to nothing else.
        assert_eq!(broker.standing("other", "shell").expect("readable"), None);
        assert_eq!(broker.standing("cli", "vault").expect("readable"), None);
    }

    #[test]
    fn a_later_decision_supersedes_an_earlier_one() {
        let path = record("supersede");
        let broker = OnDisk::opened_with(&path, FixedClock).expect("a broker");

        let first = broker.request("cli", "shell", "asked").expect("asked");
        broker
            .resolve(first.id, Decision::Allow, Scope::Session, "operator")
            .expect("answered");
        let second = broker
            .request("cli", "shell", "asked again")
            .expect("asked");
        broker
            .resolve(second.id, Decision::Deny, Scope::Session, "operator")
            .expect("answered");

        let standing = broker
            .standing("cli", "shell")
            .expect("readable")
            .expect("a standing answer");
        assert_eq!(standing.decision, Decision::Deny);
        assert_eq!(standing.question, PendingId(1), "the newest wins");
    }

    #[test]
    fn an_answer_is_not_silently_revisable() {
        let path = record("twice");
        let broker = OnDisk::opened_with(&path, FixedClock).expect("a broker");
        let asked = broker.request("cli", "shell", "asked").expect("asked");
        broker
            .resolve(asked.id, Decision::Allow, Scope::Once, "operator")
            .expect("answered");

        let error = broker
            .resolve(asked.id, Decision::Deny, Scope::Once, "somebody else")
            .expect_err("a second answer is refused");
        assert!(
            error.detail().contains("already answered"),
            "{}",
            error.detail()
        );

        // And the first answer is the one that still stands.
        assert_eq!(
            broker
                .answer(asked.id)
                .expect("readable")
                .expect("an answer")
                .decision,
            Decision::Allow
        );
    }

    #[test]
    fn answering_a_question_that_was_never_asked_is_refused() {
        let path = record("ghost");
        let broker = OnDisk::opened_with(&path, FixedClock).expect("a broker");
        let error = broker
            .resolve(PendingId(7), Decision::Allow, Scope::Once, "operator")
            .expect_err("no such question");
        assert!(
            error.detail().contains("was ever asked"),
            "{}",
            error.detail()
        );
    }

    #[test]
    fn the_same_rules_run_in_memory() {
        // Both stores fold the same ledger, so a rule that holds in one holds in the other. This is the test
        // that fails first if the two ever drift.
        let broker = InMemory::with_clock(FixedClock);
        let asked = broker.request("cli", "shell", "asked").expect("asked");
        broker
            .resolve(asked.id, Decision::Allow, Scope::Session, "operator")
            .expect("answered");
        assert!(broker.standing("cli", "shell").expect("readable").is_some());
        assert_eq!(broker.questions().expect("readable").len(), 1);
        assert!(
            broker
                .resolve(asked.id, Decision::Allow, Scope::Once, "operator")
                .is_err(),
            "the in-memory broker refuses a second answer too"
        );
        assert!(
            broker
                .resolve(PendingId(9), Decision::Allow, Scope::Once, "s")
                .is_err()
        );
    }

    #[test]
    fn a_record_that_cannot_be_read_fails_with_its_line() {
        let path = record("corrupt");
        std::fs::write(
            &path,
            "{\"v\":1,\"event\":\"asked\",\"id\":0,\"session\":\"cli\",\"action\":\"shell\",\"reason\":\"r\",\"at\":\"t\"}\nnot json\n",
        )
        .expect("a written record");
        let error = OnDisk::open(&path).expect_err("a corrupt record is an error");
        assert!(error.detail().contains("line 2"), "{}", error.detail());
    }

    #[test]
    fn the_record_reads_back_as_the_events_that_were_written() {
        let path = record("lines");
        let broker = OnDisk::opened_with(&path, FixedClock).expect("a broker");
        let asked = broker.request("cli", "shell", "asked").expect("asked");
        broker
            .resolve(asked.id, Decision::Deny, Scope::Once, "operator")
            .expect("answered");

        let text = std::fs::read_to_string(&path).expect("the record");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let asked_line: Value = serde_json::from_str(lines[0]).expect("JSON");
        assert_eq!(asked_line["event"], "asked");
        assert_eq!(asked_line["id"], 0);
        assert_eq!(asked_line["v"], 1);
        let answered_line: Value = serde_json::from_str(lines[1]).expect("JSON");
        assert_eq!(answered_line["event"], "answered");
        assert_eq!(answered_line["decision"], "deny");
        assert_eq!(answered_line["scope"], "once");
        assert_eq!(answered_line["by"], "operator");
    }
}
