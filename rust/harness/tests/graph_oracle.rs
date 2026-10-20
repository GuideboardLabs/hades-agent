//! The oracle for the walk: attempts, the return path, and the declared ways out.
//!
//! The graph's shape is data, so most of it is tested in the module. What is tested here is the walk itself,
//! against a scripted model: a first attempt that fails and a second that passes is the whole reason the
//! graph exists, and it is testable offline because the model is a seam.

use std::path::{Path, PathBuf};

use harness::approval::{ApprovalBroker, InMemory};
use harness::patch::FileEdit;
use harness::repomap::Cache;
use harness::route::RoutingReason;
use harness::{
    Bounds, CommandVerifier, Decision, Effort, Ended, Exhausted, ModelAnswer, Parts, Policy,
    Profile, ProfileId, Role, Rule, Runner, ScriptedModel, Shadows, Shell, TaskSpec, Usage,
};
use session_tree::{Clock, FileStore, Kind, Session, SessionId};

const FIXED: &str = "2026-10-17T00:00:00+00:00";
const LIMIT: usize = 8 * 1024;

#[derive(Clone)]
struct FixedClock;

impl Clock for FixedClock {
    fn now(&self) -> String {
        FIXED.to_string()
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = scratch_base().join(format!("hades-graph-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

fn session(root: &Path, name: &str) -> Session<FileStore, FixedClock> {
    Session::create(
        FileStore::new(root.join("sessions")),
        FixedClock,
        SessionId::new(name),
    )
    .expect("a session")
}

fn profile(verify: &str) -> Profile {
    Profile {
        id: ProfileId::new("test"),
        role: Role::Patch,
        model: "scripted".to_string(),
        effort: Effort::Medium,
        context_tokens: 4096,
        verify: verify.to_string(),
        judge: false,
        plan_first: false,
        edits: true,
        shadow: true,
        attempts: 3,
        minutes: 10,
        network: true,
    }
}

fn open() -> Policy {
    harness::policy::named(harness::policy::OPEN).expect("the open policy")
}

fn guarded() -> Policy {
    harness::policy::named(harness::policy::GUARDED).expect("the guarded policy")
}

/// Everything one test's walk needs, owned by the test so the borrows into it are ordinary.
struct Rig {
    root: PathBuf,
    task: TaskSpec,
    profile: Profile,
    policy: Policy,
    shell: Shell,
    verifier: CommandVerifier,
    model: ScriptedModel,
    shadows: Shadows,
}

impl Rig {
    /// A root of its own, a check that names a file, a policy, and a script for the model.
    fn new(name: &str, verify: &str, policy: Policy, model: ScriptedModel) -> Rig {
        let root = scratch(name);
        let shadows = Shadows::new(root.with_extension("shadows"));
        Rig {
            task: TaskSpec::new("make made.txt exist", root.clone()),
            shell: Shell::new(root.clone(), LIMIT),
            verifier: CommandVerifier::new(LIMIT),
            profile: profile(verify),
            policy,
            model,
            shadows,
            root,
        }
    }

    fn parts<'a>(
        &'a self,
        broker: Option<&'a dyn ApprovalBroker>,
        cache: &'a mut Cache,
    ) -> Parts<'a> {
        Parts {
            task: &self.task,
            profile: &self.profile,
            policy: &self.policy,
            model: &self.model,
            tool: &self.shell,
            verifier: &self.verifier,
            broker,
            checkpointer: None,
            evaluator: None,
            cache,
            reason: RoutingReason::chosen("the test"),
        }
    }

    /// The same parts, with a checkpointer lent in. Branching needs one, because a candidate that is not
    /// taken has to be rolled back before the next candidate starts.
    fn parts_shadowed<'a>(
        &'a self,
        broker: Option<&'a dyn ApprovalBroker>,
        cache: &'a mut Cache,
    ) -> Parts<'a> {
        let mut parts = self.parts(broker, cache);
        parts.checkpointer = Some(&self.shadows);
        parts
    }

    /// The same parts, with a judge lent in. The walker is the second place the judge is attached, and
    /// this is how the wiring is proved without a model.
    fn parts_judged<'a>(
        &'a self,
        judge: &'a dyn harness::judge::Evaluator,
        cache: &'a mut Cache,
    ) -> Parts<'a> {
        let mut parts = self.parts(None, cache);
        parts.evaluator = Some(judge);
        parts
    }

    fn session(&self, name: &str) -> Session<FileStore, FixedClock> {
        session(&self.root, name)
    }
}

#[test]
fn a_check_that_times_out_ends_the_walk_without_spending_another_attempt() {
    // The check is killed by its deadline. That is missing evidence, not a wrong answer: a second attempt
    // would spend a model call to learn nothing, so the walk stops where the check did.
    let mut rig = Rig::new(
        "timeout-check",
        "sleep 5",
        open(),
        ScriptedModel::new(["true", "true", "true"]),
    );
    rig.verifier = CommandVerifier::with_deadline(LIMIT, std::time::Duration::from_millis(200));
    let mut session = rig.session("timeout-check");
    let mut cache = Cache::new();
    let mut runner =
        Runner::new(rig.parts(None, &mut cache), &mut session).with_bounds(Bounds::new(3, 10));
    let walked = runner.run().expect("a walk");

    match &walked.ended {
        Ended::GaveUp {
            attempts,
            exhausted,
            reason,
        } => {
            assert_eq!(*attempts, 1, "the timeout spent another attempt: {reason}");
            assert_eq!(*exhausted, Exhausted::TimedOut, "{reason}");
            assert!(reason.contains("timed_out"), "{reason}");
        }
        other => panic!("expected a giving-up, got {other:?}"),
    }
    let nodes: Vec<&str> = walked.visits.iter().map(|v| v.node.as_str()).collect();
    assert_eq!(nodes, ["attempt", "gave_up"], "{nodes:?}");
    assert_eq!(walked.ended.code(), 2, "a no-verdict end, not a failure");
}

#[test]
fn a_failed_attempt_is_followed_by_another_and_a_pass_is_accepted() {
    // The check is a file, so the first attempt can genuinely fail and the second can genuinely pass. This
    // is the whole point of the graph: the model is told what happened and tries again.
    let rig = Rig::new(
        "retry",
        "test -f made.txt",
        open(),
        // Attempt one does nothing; attempt two makes the file the check wants.
        ScriptedModel::new(["true", "touch made.txt"]),
    );
    let mut session = rig.session("retry");
    let mut cache = Cache::new();
    let mut runner =
        Runner::new(rig.parts(None, &mut cache), &mut session).with_bounds(Bounds::new(3, 10));
    let walked = runner.run().expect("a walk");

    assert!(rig.root.join("made.txt").exists(), "the second attempt ran");
    let nodes: Vec<&str> = walked.visits.iter().map(|v| v.node.as_str()).collect();
    assert_eq!(nodes, ["attempt", "attempt", "accept", "done"], "{nodes:?}");
    assert!(matches!(walked.ended, Ended::Accepted { attempts: 2, .. }));
    assert_eq!(walked.ended.code(), 0);
}

#[test]
fn the_second_attempt_is_told_what_the_first_one_tried() {
    let rig = Rig::new(
        "return-path",
        "test -f made.txt",
        open(),
        ScriptedModel::new(["echo i tried this", "touch made.txt"]),
    );
    let mut session = rig.session("return-path");
    let mut cache = Cache::new();
    let mut runner =
        Runner::new(rig.parts(None, &mut cache), &mut session).with_bounds(Bounds::new(3, 10));
    let walked = runner.run().expect("a walk");

    // The run carries what it learned, and the failed attempt is in it.
    assert_eq!(walked.tried().len(), 1, "the failure was not recorded");
    assert!(
        walked.tried()[0].program().contains("i tried this"),
        "{:?}",
        walked.tried()[0]
    );
    assert_eq!(walked.tried()[0].state(), "failed");

    // And it reached the second attempt's prompt: the log records the context size of each attempt, and the
    // second is larger by the return path. This is the observable half of "the model is told what it tried".
    let contexts: Vec<u64> = session
        .entries()
        .iter()
        .filter(|entry| entry.kind == Kind::User)
        .map(|entry| {
            entry
                .payload
                .get("context_tokens")
                .and_then(|value| value.as_u64())
                .unwrap_or(0)
        })
        .collect();
    assert_eq!(contexts.len(), 2, "two attempts, two prompts");
    assert!(
        contexts[1] > contexts[0],
        "the second attempt got no more context than the first: {contexts:?}"
    );
}

#[test]
fn a_run_that_spends_its_token_budget_stops_after_the_call_that_spent_it() {
    // The endpoint reported 1,000 prompt and 500 generated tokens and the run was given 1,200, so the
    // token bound is spent after one call while the attempt bound still has room. The check fails, which is
    // what makes the token budget the only reason to stop: a pass would still be accepted.
    let rig = Rig::new(
        "tokens",
        "test -f never.txt",
        open(),
        ScriptedModel::answers([
            ModelAnswer::new("true").with_usage(Usage {
                prompt_tokens: 1000,
                generated_tokens: 500,
                millis: 5,
                reasoning_chars: 0,
            }),
            ModelAnswer::new("true"),
        ]),
    );
    let mut session = rig.session("tokens");
    let mut cache = Cache::new();
    let mut runner = Runner::new(rig.parts(None, &mut cache), &mut session)
        .with_bounds(Bounds::new(3, 10).with_tokens(1200));
    let walked = runner.run().expect("a walk");

    assert_eq!(
        walked.attempts(),
        1,
        "the token budget stopped the second call"
    );
    assert_eq!(
        walked.spent.len(),
        1,
        "the one call that was made is on the record"
    );
    assert_eq!(walked.spent[0].prompt_tokens, 1000);
    assert_eq!(walked.spent[0].generated_tokens, 500);
    // The predicate has to see the sum of both halves: a budget that counted only the answer would call
    // this run 500 tokens and let it spend twenty more calls' worth of prompt.
    let bounds = Bounds::new(3, 10).with_tokens(1200);
    assert_eq!(
        bounds.exhausted(1, std::time::Duration::from_secs(1), 1500),
        Some(Exhausted::Tokens)
    );
    assert_eq!(
        bounds.exhausted(1, std::time::Duration::from_secs(1), 1199),
        None
    );
    // A run with no declared token budget is not a run with a budget of zero.
    assert_eq!(
        Bounds::new(3, 10).exhausted(1, std::time::Duration::from_secs(1), u64::MAX),
        None
    );
    let nodes: Vec<&str> = walked.visits.iter().map(|v| v.node.as_str()).collect();
    assert_eq!(nodes, ["attempt", "gave_up"], "{nodes:?}");
    // And the end says which allowance it was, not merely that something ran out.
    assert_eq!(walked.ended.cause(), Some(Exhausted::Tokens));
}

#[test]
fn every_giving_up_carries_the_cause_it_stopped_for() {
    // A run that spends its attempts says so. Not the sentence: the type, so a surface can print "attempts"
    // without parsing prose that a later wording change would break.
    let rig = Rig::new(
        "cause-attempts",
        "test -f never.txt",
        open(),
        ScriptedModel::new(["true", "true"]),
    );
    let mut session = rig.session("cause-attempts");
    let mut cache = Cache::new();
    let mut runner =
        Runner::new(rig.parts(None, &mut cache), &mut session).with_bounds(Bounds::new(2, 10));
    let walked = runner.run().expect("a walk");
    assert_eq!(
        walked.ended.cause(),
        Some(Exhausted::Attempts),
        "{:?}",
        walked.ended
    );

    // A fault is a cause too, and it is not an attempt count: nothing was retried because nothing could be.
    let rig = Rig::new(
        "cause-fault",
        "test -f never.txt",
        open(),
        ScriptedModel::new(Vec::<String>::new()),
    );
    let mut session = rig.session("cause-fault");
    let mut cache = Cache::new();
    let mut runner =
        Runner::new(rig.parts(None, &mut cache), &mut session).with_bounds(Bounds::new(3, 10));
    let walked = runner.run().expect("a walk");
    assert_eq!(
        walked.ended.cause(),
        Some(Exhausted::Fault),
        "{:?}",
        walked.ended
    );
}

#[test]
fn a_run_that_never_passes_gives_up_at_the_bound_rather_than_for_ever() {
    let rig = Rig::new(
        "bounds",
        "test -f never.txt",
        open(),
        // Every attempt does nothing, so the check never passes. The bound is what stops it.
        ScriptedModel::new(["true", "true", "true", "true", "true"]),
    );
    let mut session = rig.session("bounds");
    let mut cache = Cache::new();
    let mut runner =
        Runner::new(rig.parts(None, &mut cache), &mut session).with_bounds(Bounds::new(2, 10));
    let walked = runner.run().expect("a walk");

    assert_eq!(walked.attempts(), 2, "the bound was two attempts");
    assert!(matches!(walked.ended, Ended::GaveUp { attempts: 2, .. }));
    assert_eq!(walked.ended.code(), 1);
    assert!(
        walked.ended.line().contains("gave up after 2"),
        "{}",
        walked.ended.line()
    );
    // Two failures were recorded, so the account of what was tried is complete.
    assert_eq!(walked.tried().len(), 2);
    let nodes: Vec<&str> = walked.visits.iter().map(|v| v.node.as_str()).collect();
    assert_eq!(nodes, ["attempt", "attempt", "gave_up"], "{nodes:?}");
}

#[test]
fn a_fault_ends_the_run_and_is_reported_rather_than_retried() {
    let rig = Rig::new(
        "fault",
        "test -f made.txt",
        open(),
        // An empty script is a model that cannot answer, which is the shape of every fault: the endpoint
        // broke, the key is wrong, the answer was an empty string.
        ScriptedModel::new(Vec::<String>::new()),
    );
    let mut session = rig.session("fault");
    let mut cache = Cache::new();
    let mut runner =
        Runner::new(rig.parts(None, &mut cache), &mut session).with_bounds(Bounds::new(3, 10));
    let walked = runner.run().expect("a walk");

    assert_eq!(walked.attempts(), 1, "a fault is not retried");
    match &walked.ended {
        Ended::GaveUp { reason, .. } => {
            assert!(reason.contains("fault"), "{reason}");
            assert!(reason.contains("no answer left"), "{reason}");
        }
        other => panic!("expected a giving-up, got {other:?}"),
    }
}

#[test]
fn a_question_ends_the_run_with_an_answer_a_person_can_give() {
    let rig = Rig::new(
        "question",
        "test -f made.txt",
        guarded(),
        ScriptedModel::new(["touch made.txt"]),
    );
    let mut session = rig.session("question");
    let broker = InMemory::new();
    let mut cache = Cache::new();
    let mut runner = Runner::new(rig.parts(Some(&broker), &mut cache), &mut session);
    let walked = runner.run().expect("a walk");

    let nodes: Vec<&str> = walked.visits.iter().map(|v| v.node.as_str()).collect();
    assert_eq!(nodes, ["attempt", "hand", "awaiting"], "{nodes:?}");
    match &walked.ended {
        Ended::Awaiting { question, .. } => {
            assert_eq!(question.action, "shell", "the question names the action");
            assert!(!question.reason.is_empty());
        }
        other => panic!("expected awaiting, got {other:?}"),
    }
    assert_eq!(walked.ended.code(), 2);
    // Nothing ran: the question is the whole result.
    assert!(!rig.root.join("made.txt").exists());
}

#[test]
fn a_policy_that_denies_outright_stops_the_run_with_the_reason() {
    // Deny is not Ask: there is no question to answer, only a policy a person has to change. The graph sends
    // it to the giving-up node, because awaiting an answer to a question nobody asked would be a lie about
    // what is happening.
    let policy = Policy::new(vec![Rule::new("shell", Decision::Deny, "not here")]);
    let rig = Rig::new(
        "deny",
        "test -f made.txt",
        policy,
        ScriptedModel::new(["touch made.txt"]),
    );
    let mut session = rig.session("deny");
    let broker = InMemory::new();
    let mut cache = Cache::new();
    let mut runner = Runner::new(rig.parts(Some(&broker), &mut cache), &mut session);
    let walked = runner.run().expect("a walk");

    let nodes: Vec<&str> = walked.visits.iter().map(|v| v.node.as_str()).collect();
    assert_eq!(nodes, ["attempt", "gave_up"], "{nodes:?}");
    match &walked.ended {
        Ended::GaveUp { reason, .. } => assert!(reason.contains("not here"), "{reason}"),
        other => panic!("expected giving up, got {other:?}"),
    }
}

#[test]
fn the_walk_is_recorded_as_checkpoints_naming_the_edge_it_took() {
    let rig = Rig::new(
        "log",
        "test -f made.txt",
        open(),
        ScriptedModel::new(["true", "touch made.txt"]),
    );
    let mut session = rig.session("log");
    let mut cache = Cache::new();
    let mut runner = Runner::new(rig.parts(None, &mut cache), &mut session);
    let walked = runner.run().expect("a walk");

    let transitions: Vec<(String, String)> = session
        .entries()
        .iter()
        .filter(|entry| entry.kind == Kind::Checkpoint)
        .filter_map(|entry| {
            let node = entry.payload.get("node")?.as_str()?.to_string();
            // The turn's own checkpoint has no edge; a graph transition always does.
            let edge = entry.payload.get("edge")?.as_str()?.to_string();
            Some((node, edge))
        })
        .collect();

    // One record per transition, and none for the end: nothing was taken at a terminal node.
    assert_eq!(transitions.len(), walked.visits.len() - 1);
    assert_eq!(
        transitions
            .iter()
            .map(|(_, edge)| edge.as_str())
            .collect::<Vec<_>>(),
        ["attempt -> attempt", "attempt -> accept", "accept -> done"]
    );
    // The terminal node's checkpoint carries the reason the edge into it gave, which is how a log explains
    // why a run stopped where it did.
    let last = session
        .entries()
        .iter()
        .rev()
        .find(|entry| {
            entry.kind == Kind::Checkpoint
                && entry.payload.get("node").and_then(|value| value.as_str()) == Some("done")
        })
        .expect("a checkpoint for the terminal node");
    assert!(
        last.payload
            .get("why")
            .and_then(|value| value.as_str())
            .is_some_and(|why| why.contains("end of the run")),
        "{:?}",
        last.payload
    );
}

#[test]
fn the_same_script_walks_the_same_shape_twice() {
    // Two walks of the same script land on the same shape, so a failing graph test is a fact about the graph
    // rather than about timing.
    let mut shapes = Vec::new();
    for run in 0..2 {
        let rig = Rig::new(
            &format!("twice-{run}"),
            "test -f made.txt",
            open(),
            ScriptedModel::new(["true", "touch made.txt"]),
        );
        let mut session = rig.session("twice");
        let mut cache = Cache::new();
        let mut runner = Runner::new(rig.parts(None, &mut cache), &mut session);
        let walked = runner.run().expect("a walk");
        shapes.push(
            walked
                .visits
                .iter()
                .map(|visit| format!("{}:{:?}", visit.node, visit.when))
                .collect::<Vec<_>>(),
        );
    }
    assert_eq!(shapes[0], shapes[1]);
}

/// A stop before the first attempt makes no attempt. It is not a failure and not a retry: the count says
/// what actually happened, which is nothing.
#[test]
fn a_stop_before_the_first_attempt_makes_no_attempt() {
    use std::sync::atomic::AtomicBool;

    let rig = Rig::new(
        "stopped",
        "test -f made.txt",
        open(),
        ScriptedModel::new(["touch made.txt"]),
    );
    let mut session = rig.session("stopped");
    let mut cache = Cache::new();
    let stop = AtomicBool::new(true);
    let mut runner = Runner::new(rig.parts(None, &mut cache), &mut session)
        .with_bounds(Bounds::new(3, 10))
        .with_stop(&stop);
    let walked = runner.run().expect("a walk");

    assert_eq!(walked.ended, Ended::Stopped { attempts: 0 });
    assert_eq!(walked.ended.line(), "stopped after 0 attempt(s)");
    assert_eq!(walked.ended.code(), 1, "a stopped run is not a pass");
    assert!(
        !rig.root.join("made.txt").exists(),
        "a stopped walk ran a program"
    );
    let nodes: Vec<&str> = walked
        .visits
        .iter()
        .map(|visit| visit.node.as_str())
        .collect();
    assert_eq!(nodes, ["attempt", "stopped"], "{nodes:?}");
}

/// A scratch base inside this crate's own `target/`, never the machine's temp directory.
///
/// The agent that runs these tests keeps its temp under the live install, and Hades refuses to write there:
/// that refusal is the boundary doing its job, so the tests must not ask it to make an exception.
fn scratch_base() -> std::path::PathBuf {
    // Created and then canonicalized: a path with `..` in it compares unequal to the same directory reached
    // by the store, and the snapshot's never-touch test is a prefix test. The first run of this helper put the
    // store inside the tree it was copying, and the copy recursed into itself until the kernel refused the
    // file name.
    let base = harness::scratch::root();
    std::fs::create_dir_all(&base).ok();
    std::fs::canonicalize(&base).unwrap_or(base)
}

#[test]
fn a_refused_edit_is_not_accepted_because_the_check_already_passes() {
    // The check is satisfied before the run. A refusal wrote nothing, so accepting it would record work
    // that was never done: the refusal has to be its own condition.
    let rig = Rig::new(
        "unwritten",
        "true",
        open(),
        ScriptedModel::answers([ModelAnswer::editing(vec![FileEdit {
            path: "absent.txt".to_string(),
            old: "x".to_string(),
            new: "y".to_string(),
        }])]),
    );
    let mut session = rig.session("unwritten");
    let mut cache = Cache::new();
    let mut runner =
        Runner::new(rig.parts(None, &mut cache), &mut session).with_bounds(Bounds::new(1, 10));
    let walked = runner.run().expect("a walk");

    let nodes: Vec<&str> = walked.visits.iter().map(|v| v.node.as_str()).collect();
    assert_eq!(nodes, ["attempt", "gave_up"], "{nodes:?}");
    assert!(
        matches!(walked.ended, Ended::GaveUp { attempts: 1, .. }),
        "{:?}",
        walked.ended
    );
}

#[test]
fn a_refused_edit_is_followed_by_another_edit_that_passes() {
    let rig = Rig::new(
        "unwritten-retry",
        "grep -q BETA note.txt",
        open(),
        ScriptedModel::answers([
            ModelAnswer::editing(vec![FileEdit {
                path: "note.txt".to_string(),
                old: "missing".to_string(),
                new: "x".to_string(),
            }]),
            ModelAnswer::editing(vec![FileEdit {
                path: "note.txt".to_string(),
                old: "alpha".to_string(),
                new: "BETA".to_string(),
            }]),
        ]),
    );
    std::fs::write(rig.root.join("note.txt"), "alpha\n").expect("a file");
    let mut session = rig.session("unwritten-retry");
    let mut cache = Cache::new();
    let mut runner =
        Runner::new(rig.parts(None, &mut cache), &mut session).with_bounds(Bounds::new(3, 10));
    let walked = runner.run().expect("a walk");

    let nodes: Vec<&str> = walked.visits.iter().map(|v| v.node.as_str()).collect();
    assert_eq!(nodes, ["attempt", "attempt", "accept", "done"], "{nodes:?}");
    assert!(matches!(walked.ended, Ended::Accepted { attempts: 2, .. }));
    assert_eq!(
        std::fs::read_to_string(rig.root.join("note.txt")).expect("the file"),
        "BETA\n"
    );
}

#[test]
fn a_plan_first_run_records_the_plan_before_the_first_call() {
    let rig = Rig::new(
        "plan-run",
        "test -f made.txt",
        open(),
        ScriptedModel::new(["touch made.txt"])
            .planning(["1. create made.txt\n2. the check sees it"]),
    );
    let mut rig = rig;
    rig.profile.plan_first = true;
    let mut session = rig.session("plan-run");
    let mut cache = Cache::new();
    let walked = Runner::new(rig.parts(None, &mut cache), &mut session)
        .run()
        .expect("a run");

    assert!(
        matches!(walked.ended, Ended::Accepted { .. }),
        "{:?}",
        walked.ended
    );
    assert!(rig.root.join("made.txt").exists());
    let kinds: Vec<Kind> = session.entries().iter().map(|entry| entry.kind).collect();
    let plan = kinds
        .iter()
        .position(|kind| *kind == Kind::Plan)
        .expect("a plan");
    let call = kinds
        .iter()
        .position(|kind| *kind == Kind::ToolCall)
        .expect("a call");
    assert!(
        plan < call,
        "the plan must come before the program: {kinds:?}"
    );
}

#[test]
fn a_plan_first_run_with_an_empty_plan_gives_up_and_writes_nothing() {
    let mut rig = Rig::new(
        "plan-empty-run",
        "test -f made.txt",
        open(),
        ScriptedModel::new(["touch made.txt"]).planning(Vec::<String>::new()),
    );
    rig.profile.plan_first = true;
    let mut session = rig.session("plan-empty-run");
    let mut cache = Cache::new();
    let walked = Runner::new(rig.parts(None, &mut cache), &mut session)
        .run()
        .expect("a run");

    assert!(
        matches!(walked.ended, Ended::GaveUp { .. }),
        "{:?}",
        walked.ended
    );
    assert!(!rig.root.join("made.txt").exists());
    assert!(
        !session
            .entries()
            .iter()
            .any(|entry| entry.kind == Kind::Plan)
    );
}

#[test]
fn a_judged_run_carries_the_verdict_out() {
    use harness::judge::{Scripted as ScriptedJudge, Verdict};
    let mut rig = Rig::new(
        "judged-run",
        "test -f made.txt",
        open(),
        ScriptedModel::new(["touch made.txt"]),
    );
    rig.profile.judge = true;
    let judge = ScriptedJudge::always(Verdict::Met);
    let mut session = rig.session("judged-run");
    let mut cache = Cache::new();
    let walked = Runner::new(rig.parts_judged(&judge, &mut cache), &mut session)
        .run()
        .expect("a run");

    assert!(
        matches!(walked.ended, Ended::Accepted { .. }),
        "{:?}",
        walked.ended
    );
    let verdict = walked.verdict.expect("a verdict");
    assert_eq!(verdict.verdict, Verdict::Met);
    let checkpoint = session
        .entries()
        .iter()
        .rev()
        .find(|entry| entry.kind == Kind::Checkpoint && entry.payload.get("verdict").is_some())
        .expect("a judged checkpoint");
    assert_eq!(checkpoint.payload["verdict"], "met");
}

#[test]
fn a_branch_keeps_the_first_candidate_the_check_accepts() {
    // Candidate one edits the tree and fails; candidate two passes. The failed branch is rolled back, so the
    // tree holds only the winner's work, and the parent log names the candidate session it came from.
    let rig = Rig::new(
        "branch-winner",
        "test -f made.txt",
        open(),
        ScriptedModel::new(["touch stray.txt", "touch made.txt"]),
    );
    let mut session = rig.session("branch-winner");
    let mut cache = Cache::new();
    let mut runner = Runner::new(rig.parts_shadowed(None, &mut cache), &mut session)
        .with_bounds(Bounds::new(2, 10).with_width(2));
    let walked = runner.run().expect("a walk");

    assert!(
        rig.root.join("made.txt").exists(),
        "the second candidate ran"
    );
    assert!(
        !rig.root.join("stray.txt").exists(),
        "the failed branch was rolled back"
    );
    assert_eq!(walked.branches.len(), 2);
    assert!(!walked.branches[0].passed, "{:?}", walked.branches[0]);
    assert!(walked.branches[1].passed, "{:?}", walked.branches[1]);
    assert_ne!(walked.branches[1].session, session.id().to_string());
    assert!(matches!(walked.ended, Ended::Accepted { attempts: 2, .. }));
    let nodes: Vec<&str> = walked.visits.iter().map(|v| v.node.as_str()).collect();
    assert_eq!(nodes, ["attempt", "accept", "done"], "{nodes:?}");
}

#[test]
fn a_branch_with_no_pass_leaves_the_tree_as_it_found_it() {
    let rig = Rig::new(
        "branch-empty",
        "test -f made.txt",
        open(),
        ScriptedModel::new(["touch stray.txt", "touch second.txt"]),
    );
    let mut session = rig.session("branch-empty");
    let mut cache = Cache::new();
    let mut runner = Runner::new(rig.parts_shadowed(None, &mut cache), &mut session)
        .with_bounds(Bounds::new(2, 10).with_width(2));
    let walked = runner.run().expect("a walk");

    assert!(
        matches!(walked.ended, Ended::GaveUp { attempts: 2, .. }),
        "{:?}",
        walked.ended
    );
    assert_eq!(walked.branches.len(), 2);
    assert!(walked.branches.iter().all(|candidate| !candidate.passed));
    assert!(!rig.root.join("stray.txt").exists());
    assert!(!rig.root.join("second.txt").exists());
    assert!(!rig.root.join("made.txt").exists());
}

#[test]
fn branch_width_cannot_outrun_the_attempt_budget() {
    let rig = Rig::new(
        "branch-budget",
        "test -f made.txt",
        open(),
        ScriptedModel::new(["true", "true", "true"]),
    );
    let mut session = rig.session("branch-budget");
    let mut cache = Cache::new();
    let mut runner = Runner::new(rig.parts_shadowed(None, &mut cache), &mut session)
        .with_bounds(Bounds::new(2, 10).with_width(4));
    let walked = runner.run().expect("a walk");

    assert_eq!(
        walked.branches.len(),
        2,
        "a width of four with two attempts runs two"
    );
    assert!(matches!(walked.ended, Ended::GaveUp { attempts: 2, .. }));
    assert_eq!(Bounds::new(4, 10).with_width(9).width(), 4);
}

#[test]
fn a_branch_with_no_pass_names_the_best_partial() {
    let check = "if [ -f b.txt ]; then printf 'HADES-CHECK {\"gate\":false,\"passed\":2,\"total\":3}\\n'; \
                 else printf 'HADES-CHECK {\"gate\":false,\"passed\":1,\"total\":3}\\n'; fi; exit 1";
    let rig = Rig::new(
        "branch-partial",
        check,
        open(),
        ScriptedModel::new(["touch a.txt", "touch b.txt"]),
    );
    let mut session = rig.session("branch-partial");
    let mut cache = Cache::new();
    let mut runner = Runner::new(rig.parts_shadowed(None, &mut cache), &mut session)
        .with_bounds(Bounds::new(2, 10).with_width(2));
    let walked = runner.run().expect("a walk");

    assert!(matches!(walked.ended, Ended::GaveUp { .. }));
    assert_eq!(walked.branches.len(), 2);
    let first = walked.branches[0].progress.as_ref().expect("progress");
    let second = walked.branches[1].progress.as_ref().expect("progress");
    assert_eq!((first.passed, first.total), (1, 3));
    assert_eq!((second.passed, second.total), (2, 3));
    match &walked.ended {
        Ended::GaveUp { reason, .. } => assert!(reason.contains("2/3"), "{reason}"),
        other => panic!("expected a give-up, got {other:?}"),
    }
}

/// A judge whose call costs tokens, as the endpoint judge does when it reports usage.
struct CostlyJudge;

impl harness::judge::Evaluator for CostlyJudge {
    fn judge(&self, _report: &harness::judge::Report<'_>) -> harness::judge::Judgement {
        harness::judge::Judgement::new(harness::judge::Verdict::NotYet, "not the file yet")
            .with_usage(Usage {
                prompt_tokens: 400,
                generated_tokens: 0,
                millis: 2,
                reasoning_chars: 0,
            })
    }
}

#[test]
fn a_judged_run_counts_the_judge_call_against_the_token_budget() {
    // The answer costs 800 and the judge costs 400. A budget of 1,200 is spent after one attempt only if
    // both calls are counted; counting the answer alone would let the judge spend for free.
    let rig = Rig::new(
        "judge-tokens",
        "test -f never.txt",
        open(),
        ScriptedModel::answers([ModelAnswer::new("true").with_usage(Usage {
            prompt_tokens: 800,
            generated_tokens: 0,
            millis: 3,
            reasoning_chars: 0,
        })]),
    );
    let mut rig = rig;
    rig.profile.judge = true;
    let mut session = rig.session("judge-tokens");
    let mut cache = Cache::new();
    let judge = CostlyJudge;
    let mut runner = Runner::new(rig.parts_judged(&judge, &mut cache), &mut session)
        .with_bounds(Bounds::new(3, 10).with_tokens(1200));
    let walked = runner.run().expect("a walk");

    assert_eq!(
        walked.attempts(),
        1,
        "the judge's call spent the last of it"
    );
    assert_eq!(walked.spent.len(), 1, "one attempt, one total");
    assert_eq!(walked.spent[0].prompt_tokens, 1200, "{:?}", walked.spent);
    assert_eq!(walked.ended.cause(), Some(Exhausted::Tokens));
}

#[test]
fn a_run_that_writes_nothing_still_pays_for_every_call() {
    // Two answers that write nothing, 600 tokens each, against a budget of 1,200. The second call is the
    // one that spends the budget, so the run must stop after it rather than take a third.
    let rig = Rig::new(
        "unwritten-tokens",
        "test -f never.txt",
        open(),
        ScriptedModel::answers([
            ModelAnswer::refused("not a program").with_usage(Usage {
                prompt_tokens: 600,
                generated_tokens: 0,
                millis: 1,
                reasoning_chars: 0,
            }),
            ModelAnswer::refused("still not a program").with_usage(Usage {
                prompt_tokens: 600,
                generated_tokens: 0,
                millis: 1,
                reasoning_chars: 0,
            }),
            ModelAnswer::refused("a third call that must not happen"),
        ]),
    );
    let mut session = rig.session("unwritten-tokens");
    let mut cache = Cache::new();
    let mut runner = Runner::new(rig.parts(None, &mut cache), &mut session)
        .with_bounds(Bounds::new(3, 10).with_tokens(1200));
    let walked = runner.run().expect("a walk");

    assert_eq!(walked.attempts(), 2, "the budget fired on the second call");
    assert_eq!(
        walked.spent.len(),
        2,
        "both calls are on the record: {:?}",
        walked.spent
    );
    assert_eq!(
        walked
            .spent
            .iter()
            .map(|usage| usage.prompt_tokens)
            .sum::<u64>(),
        1200
    );
    assert_eq!(walked.ended.cause(), Some(Exhausted::Tokens));
}
