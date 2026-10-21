//! One turn of the loop.
//!
//! The order is the design's, with one deliberate change: **policy runs before the model, not after.**
//! The design asks the model first and gates the program; an action that is refused should not have spent
//! a model call to be refused, so the gate sits in front. Policy decides on the capability, because that
//! is what it can see before the model runs. What the policy cannot see is the program, so the contents
//! are checked after the model writes them by the hard floor (`crate::floor`), which asks a person even
//! when the policy allowed the shell. The floor's question is recorded with the same two entry kinds,
//! with the category as the action.
//!
//! Nothing here owns anything. The task, the profile, the policy and the seams are borrowed, because a
//! caller keeps them across turns and a turn is the one thing that does not survive the call.
//!
//! ## The log
//!
//! One turn appends, in this order:
//!
//! | Kind | Carries |
//! |---|---|
//! | `User` | the goal, the profile and the reason it was picked |
//! | `ApprovalDecision` | a refusal, and who refused: the policy, not a person; or a standing answer being honoured, and whose |
//! | `ApprovalRequest` | a question policy raised, with the action and the reason |
//! | `ToolCall` | the program the model wrote, and what the tool was allowed to do with it |
//! | `ToolResult` | the exit status and what it printed |
//! | `ToolCall` / `ToolResult`, tool `verify` | the check, recorded as a run rather than as a special case |
//! | `Checkpoint` | the check's state and the judge's verdict: what this point in the session was |
//!
//! A standing answer only speaks where the turn asks. A policy that allows does not need a person for the
//! shell, and a policy that denies is not open to appeal by a person answering a question that was never
//! asked. The floor is the second place that asks, and an answer given to the shell does not cover it.
//!
//! A fault ends the turn where it happened and what was already appended stays, because a half turn is
//! still evidence about where the half ended.

use serde_json::{Value, json};
use session_tree::{Clock, EntryId, Kind, Session, SessionStore};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::approval::{ApprovalBroker, ApprovalRequest};
use crate::context::{Bundle, Context};
use crate::fault::Fault;
use crate::judge::{Evaluator, Judgement, Report, Verdict};
use crate::model::FeedFn;
use crate::model::Harness;
use crate::policy::{Decision, Policy};
use crate::profile::Profile;
use crate::route::RoutingReason;
use crate::shadow::{Checkpointer, Subject};
use crate::task::{Draft, ModelAnswer, TaskSpec};
use crate::tool::{Call, Tool};
use crate::verify::{State, Verification, Verifier};
use crate::{SHELL, VERIFY};

/// Everything one turn runs on.
pub struct Seams<'a> {
    task: &'a TaskSpec,
    profile: &'a Profile,
    policy: &'a Policy,
    context: &'a dyn Context,
    harness: &'a dyn Harness,
    tool: &'a dyn Tool,
    verifier: &'a dyn Verifier,
    evaluator: Option<&'a dyn Evaluator>,
    broker: Option<&'a dyn ApprovalBroker>,
    checkpointer: Option<&'a dyn Checkpointer>,
    /// Where the model's pieces go while it writes them. None is a run nobody is watching.
    feed: Option<FeedFn<'a>>,
    /// A stop raised while this turn runs. Checked before the model and again before the program, so a
    /// program the model wrote after the stop is never executed.
    stopping: Option<&'a AtomicBool>,
    reason: RoutingReason,
}

impl<'a> Seams<'a> {
    /// The seven a turn cannot run without. The judge and the broker are added afterwards, because a turn
    /// without them is a real turn and a turn without a tool is not.
    pub fn new(
        task: &'a TaskSpec,
        profile: &'a Profile,
        policy: &'a Policy,
        context: &'a dyn Context,
        harness: &'a dyn Harness,
        tool: &'a dyn Tool,
        verifier: &'a dyn Verifier,
    ) -> Self {
        Seams {
            task,
            profile,
            policy,
            context,
            harness,
            tool,
            verifier,
            evaluator: None,
            broker: None,
            checkpointer: None,
            feed: None,
            stopping: None,
            reason: RoutingReason::chosen("the caller"),
        }
    }

    pub fn judging(mut self, evaluator: &'a dyn Evaluator) -> Self {
        self.evaluator = Some(evaluator);
        self
    }

    pub fn brokering(mut self, broker: &'a dyn ApprovalBroker) -> Self {
        self.broker = Some(broker);
        self
    }

    /// A place for the model's reasoning and content while it writes them, so a surface can show the run
    /// happening instead of waiting for it to end.
    pub fn feeding(mut self, feed: FeedFn<'a>) -> Self {
        self.feed = Some(feed);
        self
    }

    /// A checkpointer, so the tree the program starts from is recoverable. Without one a turn says that
    /// nothing can be rolled back rather than implying it can.
    pub fn checkpointing(mut self, checkpointer: &'a dyn Checkpointer) -> Self {
        self.checkpointer = Some(checkpointer);
        self
    }

    /// The stop this turn watches. Checked twice: before the model is asked, and after it answers but before
    /// the program runs. The second check is the one that matters, because a stop must not be followed by
    /// the side effect the model had just written.
    pub fn stopping(mut self, flag: &'a AtomicBool) -> Self {
        self.stopping = Some(flag);
        self
    }

    fn stopped(&self) -> bool {
        self.stopping
            .is_some_and(|flag| flag.load(Ordering::SeqCst))
    }

    /// The parts, for a caller that builds one of these per attempt and needs the same ones again. A
    /// getter per field rather than a clone of the struct, because every field is already a borrow.
    pub fn task(&self) -> &'a TaskSpec {
        self.task
    }

    pub fn profile(&self) -> &'a Profile {
        self.profile
    }

    pub fn policy(&self) -> &'a Policy {
        self.policy
    }

    pub fn harness(&self) -> &'a dyn Harness {
        self.harness
    }

    pub fn tool(&self) -> &'a dyn Tool {
        self.tool
    }

    pub fn verifier(&self) -> &'a dyn Verifier {
        self.verifier
    }

    pub fn broker(&self) -> Option<&'a dyn ApprovalBroker> {
        self.broker
    }

    pub fn checkpointer(&self) -> Option<&'a dyn Checkpointer> {
        self.checkpointer
    }

    pub fn reason(&self) -> RoutingReason {
        self.reason.clone()
    }

    pub fn because(mut self, reason: RoutingReason) -> Self {
        self.reason = reason;
        self
    }

    /// The bundle this turn would build, for a caller that wants to show it before running.
    pub fn context(&self) -> Result<Bundle, Fault> {
        self.context.build(self.task, &self.profile.budget())
    }

    /// Run one turn, appending what happened to `session` as it goes.
    pub fn turn<S, C>(&self, session: &mut Session<S, C>) -> Result<Turn, Fault>
    where
        S: SessionStore + Clone,
        C: Clock + Clone,
    {
        let mut entries = Vec::new();
        let budget = self.profile.budget();

        // 1. Context, under a budget, and the record of what was asked for. The request is logged before
        //    anything can refuse it: a turn that never ran is still a turn that was asked for.
        let bundle = self.context.build(self.task, &budget)?;
        entries.push(session.append(
            Kind::User,
            json!({
                "goal": self.task.goal(),
                "root": self.task.root().display().to_string(),
                "profile": self.profile.id.as_str(),
                "model": self.profile.model,
                "effort": self.profile.effort.name(),
                "reason": self.reason.as_str(),
                "context_tokens": bundle.tokens(),
                "dropped": bundle.dropped(),
            }),
        )?);

        // 2. A stop that arrived before anything was decided. The request is on the record; nothing else
        //    happens, and the walk records the edge it took out of the attempt.
        if self.stopped() {
            return Ok(Turn {
                entries,
                outcome: Outcome::Stopped,
                usage: None,
            });
        }

        // 3. Policy, before the model. A deny stops the turn here, and an ask turns it into a question
        //    that outlives the call rather than a guess.
        let ruling = self.policy.decide(SHELL);
        match ruling.decision() {
            Decision::Deny => {
                entries.push(session.append(
                    Kind::ApprovalDecision,
                    json!({
                        "action": SHELL,
                        "decision": "deny",
                        "by": "policy",
                        "reason": ruling.reason(),
                    }),
                )?);
                return Ok(Turn {
                    entries,
                    outcome: Outcome::Refused {
                        reason: ruling.reason().to_string(),
                    },
                    usage: None,
                });
            }
            Decision::Ask => {
                if let Some(turn) =
                    self.permission(session, &mut entries, SHELL, ruling.reason())?
                {
                    return Ok(turn);
                }
            }
            Decision::Allow => {}
        }

        // 3. A plan, before any program, when the profile asks for one. One per task: a retry after a
        //    failed check already has the plan, and asking again spends a call to restate it. The goal is
        //    part of the entry, because a session can be reused for a second piece of work; a plan written
        //    for a different goal is not a plan for this one, and treating it as one runs a task on the
        //    previous task's instructions.
        let planned_for_this_goal = session.entries().iter().any(|entry| {
            entry.kind == Kind::Plan
                && entry.payload.get("goal").and_then(Value::as_str) == Some(self.task.goal())
        });
        if self.profile.plan_first && !planned_for_this_goal {
            let plan = self.harness.plan(self.task, &bundle, &budget)?;
            if self.stopped() {
                return Ok(Turn {
                    entries,
                    outcome: Outcome::Stopped,
                    usage: None,
                });
            }
            if plan.trim().is_empty() {
                return Ok(Turn {
                    entries,
                    outcome: Outcome::Unwritten {
                        reason: "the plan came back empty".to_string(),
                    },
                    usage: None,
                });
            }
            entries.push(session.append(
                Kind::Plan,
                json!({ "plan": plan, "goal": self.task.goal() }),
            )?);
        }

        // 3. The model, asked once.
        let answer = self.harness.run(self.task, &bundle, &budget, self.feed)?;

        // 3a. A stop raised while the model was answering. The call in flight has already finished, so the
        //     only honest thing left is not to act on what it wrote: no tool call, no check, no snapshot.
        if self.stopped() {
            return Ok(Turn {
                entries,
                outcome: Outcome::Stopped,
                usage: answer.usage(),
            });
        }

        // 3b. The hard floor, now that the answer is visible. A program is checked as a command; an edit
        //     is checked per path, because a path is the thing a write can touch. Neither a policy nor a
        //     standing answer to the shell unlocks a category: a person answers the category itself.
        //
        //     A profile that accepts programs only refuses an edit here, before anything is planned or
        //     written. It is an Unwritten turn with a reason, not a silent application: the model can see
        //     that this profile would not take its shape and answer again with a program.
        if !self.profile.edits
            && let Draft::Edits(_) = answer.draft()
        {
            let reason = "this profile accepts a program, not edit answers".to_string();
            entries.push(session.append(
                Kind::ToolCall,
                json!({ "tool": "patch", "refused": reason }),
            )?);
            entries.push(session.append(
                Kind::ToolResult,
                json!({ "tool": "patch", "status": 1, "truncated": false, "output": reason }),
            )?);
            return Ok(Turn {
                entries,
                outcome: Outcome::Unwritten {
                    reason: reason.clone(),
                },
                usage: answer.usage(),
            });
        }
        match answer.draft() {
            Draft::Program(program) => {
                if let Some(floor) =
                    crate::floor::check(SHELL, &json!({ "command": program }), self.task.root())
                    && let Some(turn) =
                        self.permission(session, &mut entries, floor.action(), &floor.reason())?
                {
                    return Ok(turn);
                }
            }
            Draft::Edits(edits) => {
                let mut asked: Vec<&str> = Vec::new();
                for edit in edits {
                    if asked.contains(&edit.path.as_str()) {
                        continue;
                    }
                    asked.push(edit.path.as_str());
                    if let Some(floor) = crate::floor::check(
                        "patch",
                        &json!({ "path": edit.path }),
                        self.task.root(),
                    ) && let Some(turn) =
                        self.permission(session, &mut entries, floor.action(), &floor.reason())?
                    {
                        return Ok(turn);
                    }
                }
            }
            // Nothing was attempted, so there is nothing to gate: the turn records why and stops.
            Draft::Refused(_) => {}
        }

        // 4. The answer, recorded before it is acted on. What the model wrote goes in with the tool,
        // because a log that says a command ran without saying what it could reach is a log that cannot
        // answer the question asked of it afterwards.
        let mut payload = self.base_payload(&answer);
        match answer.draft() {
            Draft::Refused(reason) => {
                payload.insert("tool".to_string(), json!("patch"));
                payload.insert("refused".to_string(), json!(reason));
                payload.insert("rendered".to_string(), json!(answer.rendered()));
                entries.push(session.append(Kind::ToolCall, serde_json::Value::Object(payload))?);
                entries.push(session.append(
                    Kind::ToolResult,
                    json!({ "tool": "patch", "status": 1, "truncated": false, "output": reason }),
                )?);
                // Nothing was written, so no check runs: a check of a tree nothing touched is not evidence.
                Ok(Turn {
                    entries,
                    outcome: Outcome::Unwritten {
                        reason: reason.clone(),
                    },
                    usage: answer.usage(),
                })
            }
            Draft::Edits(edits) => {
                payload.insert("tool".to_string(), json!("patch"));
                payload.insert(
                    "edits".to_string(),
                    json!(
                        edits
                            .iter()
                            .map(|edit| json!({
                                "path": edit.path,
                                "old": edit.old,
                                "new": edit.new,
                            }))
                            .collect::<Vec<_>>()
                    ),
                );
                payload.insert("rendered".to_string(), json!(answer.rendered()));
                // Plan every file first. A bad anchor anywhere means no file is written, so the snapshot
                // and the call are only recorded once the whole answer is known to apply.
                let plans = match crate::patch::plan_all(
                    &crate::path::PathPolicy::for_root(self.task.root()),
                    edits,
                ) {
                    Ok(plans) => plans,
                    Err(reason) => {
                        entries.push(
                            session.append(Kind::ToolCall, serde_json::Value::Object(payload))?,
                        );
                        entries.push(session.append(
                            Kind::ToolResult,
                            json!({ "tool": "patch", "status": 1, "truncated": false, "output": reason }),
                        )?);
                        return Ok(Turn {
                            entries,
                            outcome: Outcome::Unwritten { reason },
                            usage: answer.usage(),
                        });
                    }
                };
                self.snapshot(session, &mut payload)?;
                entries.push(session.append(Kind::ToolCall, serde_json::Value::Object(payload))?);
                let run = match crate::patch::write_all(&plans) {
                    Ok(report) => crate::command::Run {
                        status: 0,
                        output: report,
                        truncated: false,
                        timed_out: false,
                    },
                    Err(reason) => {
                        entries.push(session.append(
                            Kind::ToolResult,
                            json!({ "tool": "patch", "status": 1, "truncated": false, "output": reason }),
                        )?);
                        return Ok(Turn {
                            entries,
                            outcome: Outcome::Unwritten { reason },
                            usage: answer.usage(),
                        });
                    }
                };
                entries.push(session.append(
                    Kind::ToolResult,
                    json!({
                        "tool": "patch",
                        "status": run.status,
                        "truncated": run.truncated,
                        "output": run.output,
                    }),
                )?);
                self.finish(session, &mut entries, &answer, run)
            }
            Draft::Program(program) => {
                payload.insert("tool".to_string(), json!(SHELL));
                payload.insert("program".to_string(), json!(program));
                // What the program could reach. An edit has no cage: it is path-checked and written by
                // this process, not run in a sandbox, so it does not claim one.
                payload.insert("sandbox".to_string(), json!(self.tool.describe()));
                self.snapshot(session, &mut payload)?;
                entries.push(session.append(Kind::ToolCall, serde_json::Value::Object(payload))?);
                let run = match self.tool.call(&Call::new(program.clone())) {
                    Ok(run) => run,
                    Err(fault) => {
                        entries.push(session.append(
                            Kind::ToolResult,
                            json!({ "tool": SHELL, "status": -1, "error": fault.detail() }),
                        )?);
                        return Err(fault);
                    }
                };
                entries.push(session.append(
                    Kind::ToolResult,
                    json!({
                        "tool": SHELL,
                        "status": run.status,
                        "truncated": run.truncated,
                        "output": run.output,
                    }),
                )?);
                self.finish(session, &mut entries, &answer, run)
            }
        }
    }

    /// The part of a tool call every answer carries: what the model spent, and what it said to itself.
    fn base_payload(&self, answer: &ModelAnswer) -> serde_json::Map<String, serde_json::Value> {
        let mut payload = serde_json::Map::new();
        if let Some(usage) = answer.usage() {
            payload.insert(
                "model".to_string(),
                json!({
                    "prompt_tokens": usage.prompt_tokens,
                    "generated_tokens": usage.generated_tokens,
                    "millis": usage.millis,
                    "reasoning_chars": usage.reasoning_chars,
                }),
            );
        }
        if let Some(reasoning) = answer.reasoning_for_log() {
            payload.insert("reasoning".to_string(), json!(reasoning));
        }
        payload
    }

    /// The tree the answer is about to start from, recorded beside it, so a change can be attributed to
    /// the run that made it and undone afterwards. A snapshot that cannot be taken is a fault rather than
    /// a shrug: a run that believes it can be rolled back when it cannot is worse than one that knows.
    fn snapshot<S, C>(
        &self,
        session: &Session<S, C>,
        payload: &mut serde_json::Map<String, serde_json::Value>,
    ) -> Result<(), Fault>
    where
        S: SessionStore + Clone,
        C: Clock + Clone,
    {
        if let Some(checkpointer) = self.checkpointer {
            let subject = Subject::new(session.id().as_str(), self.task.root());
            let snapshot = checkpointer.snapshot(&subject)?;
            payload.insert("snapshot".to_string(), json!(snapshot.label()));
        }
        Ok(())
    }

    /// The check, the judge and the checkpoint: what happens after an answer has been acted on. Shared by
    /// both kinds of answer so the check cannot be skipped by one of them.
    fn finish<S, C>(
        &self,
        session: &mut Session<S, C>,
        entries: &mut Vec<EntryId>,
        answer: &ModelAnswer,
        run: crate::command::Run,
    ) -> Result<Turn, Fault>
    where
        S: SessionStore + Clone,
        C: Clock + Clone,
    {
        let verification = self.check(session, entries)?;
        let rendered = answer.rendered();
        let verdict = match (self.profile.judge, self.evaluator) {
            (true, Some(evaluator)) => evaluator.judge(&Report {
                goal: self.task.goal(),
                program: &rendered,
                output: &run.output,
                verification: &verification,
            }),
            (true, None) => {
                Judgement::unjudged("this profile asks for a judge and none is attached")
            }
            (false, _) => Judgement::unjudged("this profile does not ask for a judge"),
        };
        entries.push(session.append(
            Kind::Checkpoint,
            json!({
                "check": verification.state.name(),
                "command": verification.command,
                "status": verification.status,
                "truncated": verification.truncated,
                "sha256": verification.output_digest,
                "progress": verification.progress.as_ref().map(|progress| progress.json()),
                "verdict": verdict.verdict.name(),
                "reason": verdict.reason,
                "judge_model": verdict.identity.as_ref().map(|identity| identity.model.clone()),
                "judge_prompt": verdict.identity.as_ref().map(|identity| identity.prompt_sha256.clone()),
            }),
        )?);
        let usage = total(answer.usage(), verdict.usage.as_deref().copied());
        Ok(Turn {
            entries: std::mem::take(entries),
            outcome: Outcome::Ran {
                answer: answer.clone(),
                run,
                verification: Box::new(verification),
                verdict,
            },
            usage,
        })
    }

    /// The one way this turn asks a person. `Some` means the turn ended here (a refusal, or a question
    /// recorded for a later answer); `None` means a standing answer for this session let the action
    /// through, and the reason it did is now in the log.
    ///
    /// Both asks come through here, the policy's and the floor's, because two copies of a governance
    /// rule drift and the drift is silent.
    fn permission<S, C>(
        &self,
        session: &mut Session<S, C>,
        entries: &mut Vec<EntryId>,
        action: &str,
        reason: &str,
    ) -> Result<Option<Turn>, Fault>
    where
        S: SessionStore + Clone,
        C: Clock + Clone,
    {
        let Some(broker) = self.broker else {
            // No broker means nobody to ask. Refusing is the safe direction, and it is recorded as a
            // refusal rather than quietly becoming an allow.
            let reason = format!("nothing can ask for {action}: {reason}");
            entries.push(session.append(
                Kind::ApprovalDecision,
                json!({
                    "action": action,
                    "decision": "deny",
                    "by": "policy",
                    "reason": reason,
                }),
            )?);
            return Ok(Some(Turn {
                entries: std::mem::take(entries),
                outcome: Outcome::Refused { reason },
                usage: None,
            }));
        };
        let session_id = session.id().as_str().to_string();
        match broker.standing(&session_id, action)? {
            // A person already answered this for this session. Honoured, and recorded, because a turn
            // that ran without asking should say what let it.
            Some(standing) if standing.decision == Decision::Allow => {
                entries.push(session.append(
                    Kind::ApprovalDecision,
                    json!({
                        "action": action,
                        "decision": "allow",
                        "by": standing.by,
                        "question": standing.question.0,
                        "reason": "a standing answer for this session",
                    }),
                )?);
                Ok(None)
            }
            // And the same standing answer in the other direction is a refusal, so nobody has to
            // answer no twice.
            Some(standing) => {
                let reason = format!(
                    "{} already said no to {action} for this session",
                    standing.by
                );
                entries.push(session.append(
                    Kind::ApprovalDecision,
                    json!({
                        "action": action,
                        "decision": "deny",
                        "by": standing.by,
                        "question": standing.question.0,
                        "reason": reason,
                    }),
                )?);
                Ok(Some(Turn {
                    entries: std::mem::take(entries),
                    outcome: Outcome::Refused { reason },
                    usage: None,
                }))
            }
            None => {
                let question = broker.request(&session_id, action, reason)?;
                entries.push(session.append(
                    Kind::ApprovalRequest,
                    json!({
                        "id": question.id.0,
                        "action": question.action,
                        "reason": question.reason,
                    }),
                )?);
                Ok(Some(Turn {
                    entries: std::mem::take(entries),
                    outcome: Outcome::Awaiting { question },
                    usage: None,
                }))
            }
        }
    }

    /// The check, appended as a call and a result. Kept apart from the body because it is the one step
    /// that must not be able to fail quietly: a check that could not run still gets a `ToolResult`.
    fn check<S, C>(
        &self,
        session: &mut Session<S, C>,
        entries: &mut Vec<EntryId>,
    ) -> Result<Verification, Fault>
    where
        S: SessionStore + Clone,
        C: Clock + Clone,
    {
        entries.push(session.append(
            Kind::ToolCall,
            json!({ "tool": VERIFY, "program": self.profile.verify }),
        )?);
        let verification = self.verifier.verify(self.task, &self.profile.verify);
        entries.push(session.append(
            Kind::ToolResult,
            json!({
                "tool": VERIFY,
                "state": verification.state.name(),
                "status": verification.status,
                "truncated": verification.truncated,
                "sha256": verification.output_digest,
                "output": verification.output,
                "progress": verification.progress.as_ref().map(|progress| progress.json()),
                "attack": verification.attack.as_ref().map(|attack| attack.name()),
                "attack_line": verification.attack.as_ref().map(|attack| attack.line()),
                "attack_output": verification.attack.as_ref().map(|attack| attack.output.clone()),
            }),
        )?);
        Ok(verification)
    }
}

/// How the turn ended. An enum rather than a struct of options, so a refused turn has no program to read
/// and no reader has to wonder whether it had one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Policy refused before the model was asked.
    Refused { reason: String },
    /// A person stopped the turn. No program ran.
    Stopped,
    /// Policy asked, and nobody has answered. Pending is a state: the run is not blocked, the question
    /// is not lost, and nothing was guessed.
    Awaiting { question: ApprovalRequest },
    /// The answer was refused before anything was written: a bad edit object, an anchor that does not
    /// match, a path that could not be planned. Distinct from a failed check, because no check ran and a
    /// check of a tree nothing touched is not evidence. Retryable while the bounds allow.
    Unwritten { reason: String },
    /// A program ran, and this is what came of it.
    Ran {
        answer: ModelAnswer,
        run: crate::command::Run,
        verification: Box<Verification>,
        verdict: Judgement,
    },
}

impl Outcome {
    /// One line for a person or a surface, built from what the outcome actually carries.
    pub fn summary(&self) -> String {
        match self {
            Outcome::Refused { reason } => format!("refused: {reason}"),
            Outcome::Stopped => "stopped by a person".to_string(),
            Outcome::Awaiting { question } => {
                format!("waiting on question {}: {}", question.id, question.reason)
            }
            Outcome::Unwritten { reason } => format!("nothing written: {reason}"),
            Outcome::Ran {
                run,
                verification,
                verdict,
                ..
            } => {
                let check = match verification.state {
                    State::Passed => "check passed".to_string(),
                    State::Failed => "check failed".to_string(),
                    State::TimedOut => "check timed out without a verdict".to_string(),
                    State::CouldNotRun => format!("no check ran ({})", verification.output),
                };
                let judged = if verdict.verdict == Verdict::Unjudged {
                    "unjudged".to_string()
                } else {
                    format!("{}: {}", verdict.verdict.name(), verdict.reason)
                };
                format!("ran, exit {}, {check}, {judged}", run.status)
            }
        }
    }
}

/// One turn: what it appended, how it ended, and what every model call it made cost.
///
/// `usage` is on the turn rather than only on `Ran`'s answer on purpose. An `Unwritten` answer, a
/// `Stopped` one and a judge call all spend tokens that the endpoint reported, and a budget that only
/// counted the turns that ran would fire later than the call that actually exhausted it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub entries: Vec<EntryId>,
    pub outcome: Outcome,
    pub usage: Option<crate::task::Usage>,
}

/// The sum of two calls' usage. `None` only when neither call reported anything, so an endpoint that
/// says nothing stays distinguishable from one that reports zero.
fn total(
    left: Option<crate::task::Usage>,
    right: Option<crate::task::Usage>,
) -> Option<crate::task::Usage> {
    match (left, right) {
        (None, None) => None,
        (left, right) => {
            let left = left.unwrap_or_default();
            let right = right.unwrap_or_default();
            Some(crate::task::Usage {
                prompt_tokens: left.prompt_tokens + right.prompt_tokens,
                generated_tokens: left.generated_tokens + right.generated_tokens,
                millis: left.millis + right.millis,
                reasoning_chars: left.reasoning_chars + right.reasoning_chars,
            })
        }
    }
}
