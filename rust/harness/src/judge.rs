//! The judge: a second, small opinion about the turn, with its reason.
//!
//! A separate evaluator rather than the same model grading itself, and it is off unless a profile asks for
//! it. Both choices come from the same lesson: a self-assessment is the cheapest thing to produce and the
//! most expensive thing to trust, so when it runs, it runs as a distinct component whose verdict is
//! recorded next to the check's.
//!
//! `Unjudged` is a real value and not a placeholder. When no judge ran, the honest record says so; a
//! missing verdict quietly read as "met" is the failure mode this enum exists to prevent.

use std::collections::VecDeque;
use std::sync::Mutex;

use crate::verify::Verification;

/// What the judge thought.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Met,
    NotYet,
    Impossible,
    /// No judge ran, or one ran and declined to answer. Not the same as met.
    Unjudged,
}

impl Verdict {
    pub fn name(self) -> &'static str {
        match self {
            Verdict::Met => "met",
            Verdict::NotYet => "not_yet",
            Verdict::Impossible => "impossible",
            Verdict::Unjudged => "unjudged",
        }
    }

    pub fn is_a_verdict(self) -> bool {
        !matches!(self, Verdict::Unjudged)
    }
}

/// A verdict and the reason for it. The reason is not optional: a verdict without one cannot be argued
/// with, and a judge that cannot be argued with is a judge that gets trusted for the wrong reasons.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Judgement {
    pub verdict: Verdict,
    pub reason: String,
}

impl Judgement {
    pub fn new(verdict: Verdict, reason: impl Into<String>) -> Self {
        Judgement {
            verdict,
            reason: reason.into(),
        }
    }

    pub fn unjudged(reason: impl Into<String>) -> Self {
        Judgement::new(Verdict::Unjudged, reason)
    }
}

/// What the judge is shown. One value so the contract is visible: nothing else about the run is available
/// to it, which is what keeps it from grading the model on how confident the model sounded.
#[derive(Debug, Clone, Copy)]
pub struct Report<'a> {
    pub goal: &'a str,
    pub program: &'a str,
    pub output: &'a str,
    pub verification: &'a Verification,
}

/// Asking the judge.
pub trait Evaluator {
    fn judge(&self, report: &Report<'_>) -> Judgement;
}

/// Answers from a queue, then repeats the last one. Repeating the last is what makes it usable for a run
/// of turns where the answer does not change.
#[derive(Debug, Default)]
pub struct Scripted {
    answers: Mutex<VecDeque<Judgement>>,
}

impl Scripted {
    pub fn new(answers: Vec<Judgement>) -> Self {
        Scripted {
            answers: Mutex::new(answers.into()),
        }
    }

    /// The same answer every time.
    pub fn always(verdict: Verdict) -> Self {
        Scripted::new(vec![Judgement::new(verdict, "the script says so")])
    }
}

impl Evaluator for Scripted {
    fn judge(&self, _report: &Report<'_>) -> Judgement {
        let mut answers = self.answers.lock().expect("the judge lock");
        match answers.len() {
            0 => Judgement::unjudged("the judge had nothing to say"),
            1 => answers.front().expect("one answer").clone(),
            _ => answers.pop_front().expect("more than one answer"),
        }
    }
}

/// What the judge is told. It sees one attempt, never the conversation, and is asked for a verdict with a
/// reason. The check's state is included because "the command passed" and "the work was done" are
/// different claims, and the gap between them is the thing a judge is for.
pub const PROMPT: &str = "You judge one finished attempt by a coding agent. You do not run anything and you \
do not rewrite the work. Decide only whether the goal was met, not yet, or impossible, from the goal, the \
program, what it printed and the check's state. A check that passed on a program that could not have \
produced the result is not met. A program that failed the check is not met, even if it looks right. Answer \
with one JSON object and nothing else: {\"verdict\":\"met|not_yet|impossible\",\"reason\":\"one sentence\"}.";

/// A judge behind the same endpoint the turn used. It is a separate call with its own prompt, which is a
/// second opinion and not a second model; a `judge.model` setting is a later decision.
pub struct Endpoint {
    client: crate::model::OpenAi,
}

impl Endpoint {
    pub fn new(client: crate::model::OpenAi) -> Self {
        Endpoint { client }
    }
}

impl Evaluator for Endpoint {
    fn judge(&self, report: &Report<'_>) -> Judgement {
        let user = format!(
            "Goal: {}\n\nProgram:\n{}\n\nOutput:\n{}\n\nCheck: {}\n{}",
            report.goal,
            clip(report.program, 2_000),
            clip(report.output, 1_000),
            report.verification.state.name(),
            clip(&report.verification.output, 500),
        );
        let messages = serde_json::json!([
            { "role": "system", "content": PROMPT },
            { "role": "user", "content": user },
        ]);
        match self.client.chat(
            messages.as_array().expect("an array of messages"),
            None,
            &|_, _| {},
        ) {
            Ok(reply) => parse(&reply.content),
            Err(fault) => Judgement::unjudged(format!(
                "the judge could not be asked: {} fault: {}",
                fault.class(),
                fault.detail()
            )),
        }
    }
}

/// Read the judge's one JSON object. A verdict without a reason is not a verdict, and an answer that is not
/// the agreed shape is `unjudged` with the answer kept as the reason: a judge that cannot be parsed did not
/// judge, and pretending otherwise is the failure this enum exists to prevent.
pub fn parse(text: &str) -> Judgement {
    let text = text.trim();
    let text = text
        .strip_prefix("```json")
        .or_else(|| text.strip_prefix("```"))
        .unwrap_or(text)
        .trim_end_matches("```")
        .trim();
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return Judgement::unjudged(format!(
            "the judge did not answer with JSON: {}",
            clip(text, 200)
        ));
    };
    let verdict = match value
        .get("verdict")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
        .replace('-', "_")
        .as_str()
    {
        "met" => Verdict::Met,
        "not_yet" => Verdict::NotYet,
        "impossible" => Verdict::Impossible,
        other => {
            return Judgement::unjudged(format!(
                "the judge named an unknown verdict `{}`",
                clip(other, 40)
            ));
        }
    };
    let reason = value
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .trim();
    if reason.is_empty() {
        return Judgement::unjudged("the judge gave a verdict with no reason");
    }
    Judgement::new(verdict, clip(reason, 300))
}

fn clip(text: &str, cap: usize) -> String {
    if text.chars().count() <= cap {
        return text.to_string();
    }
    let mut out: String = text.chars().take(cap).collect();
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_judgement_is_one_json_object_with_a_reason() {
        let said = parse(r#"{"verdict":"met","reason":"the file exists and the check passed"}"#);
        assert_eq!(said.verdict, Verdict::Met);
        assert!(said.reason.contains("file exists"));
        let said = parse("```json\n{\"verdict\":\"not-yet\",\"reason\":\"no output\"}\n```");
        assert_eq!(said.verdict, Verdict::NotYet);
    }

    #[test]
    fn an_unparsable_or_reasonless_answer_is_unjudged() {
        assert_eq!(parse("looks fine to me").verdict, Verdict::Unjudged);
        assert_eq!(
            parse(r#"{"verdict":"met","reason":"  "}"#).verdict,
            Verdict::Unjudged
        );
        assert_eq!(
            parse(r#"{"verdict":"probably","reason":"x"}"#).verdict,
            Verdict::Unjudged
        );
        assert_eq!(parse(r#"{"verdict":"met"}"#).verdict, Verdict::Unjudged);
    }
}
