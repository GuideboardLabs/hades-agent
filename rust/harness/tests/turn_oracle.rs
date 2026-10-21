//! The oracle for one turn of the loop, with no model call anywhere in it.
//!
//! That is the whole reason the harness is a seam: a loop whose first honest run needs a network round trip
//! has no offline proof that it works, and the offline proof is the only one available before a model is
//! wired in.

use std::path::{Path, PathBuf};

use harness::approval::{ApprovalBroker, InMemory, OnDisk, Scope};
use harness::command::Run;
use harness::context::Section;
use harness::judge::{Evaluator, Judgement, Report, Verdict};
use harness::patch::FileEdit;
use harness::{
    Budgeted, Call, Class, CommandVerifier, Decision, Effort, Fault, ModelAnswer, Outcome, Policy,
    Profile, ProfileId, Role, Rule, ScriptedModel, Seams, Shell, State, TaskSpec, Tool, ToolSchema,
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
    let dir = scratch_base().join(format!("hades-turn-{name}-{}", std::process::id()));
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

fn profile(verify: &str, judge: bool) -> Profile {
    Profile {
        id: ProfileId::new("test"),
        role: Role::Patch,
        model: Some("scripted".to_string()),
        effort: Effort::Medium,
        context_tokens: 4096,
        verify: verify.to_string(),
        judge,
        plan_first: false,
        edits: true,
        shadow: true,
        attempts: 3,
        minutes: 10,
        protect: Vec::new(),
        network: true,
    }
}

fn allow() -> Policy {
    Policy::new(vec![Rule::new(
        harness::SHELL,
        Decision::Allow,
        "the test allows the shell",
    )])
}

fn context(task: &TaskSpec) -> Budgeted {
    Budgeted::new(vec![Section::new("goal", task.goal())])
}

#[test]
fn a_turn_that_passes_runs_the_program_and_logs_it_in_order() {
    let root = scratch("pass");
    let task = TaskSpec::new("list the files", &root);
    let work = profile("true", true);
    let policy = allow();
    let model = ScriptedModel::one("echo hello");
    let shell = Shell::new(&root, LIMIT);
    let verifier = CommandVerifier::new(LIMIT);
    let judge = harness::judge::Scripted::always(Verdict::Met);
    let builder = context(&task);

    let mut log = session(&root, "pass");
    let turn = Seams::new(&task, &work, &policy, &builder, &model, &shell, &verifier)
        .judging(&judge)
        .turn(&mut log)
        .expect("a turn");

    match &turn.outcome {
        Outcome::Ran {
            run,
            verification,
            verdict,
            ..
        } => {
            assert!(run.passed(), "{run:?}");
            assert_eq!(run.output.trim(), "hello");
            assert_eq!(verification.state, State::Passed);
            assert_eq!(verdict.verdict, Verdict::Met);
        }
        other => panic!("expected a run, got {other:?}"),
    }

    assert_eq!(
        turn.entries.len(),
        6,
        "user, call, result, check call, check result, checkpoint"
    );
    assert_eq!(log.len(), 7, "six appended after the meta entry");

    let kinds: Vec<Kind> = log.entries().iter().map(|entry| entry.kind).collect();
    assert_eq!(
        kinds,
        vec![
            Kind::Meta,
            Kind::User,
            Kind::ToolCall,
            Kind::ToolResult,
            Kind::ToolCall,
            Kind::ToolResult,
            Kind::Checkpoint,
        ]
    );

    // The check is recorded as a run, with the tool named `verify`, so a reader can tell the work from the
    // check on it.
    assert_eq!(log.entries()[4].payload["tool"], harness::VERIFY);
    assert_eq!(log.entries()[5].payload["state"], "passed");
    assert_eq!(log.entries()[6].payload["verdict"], "met");

    // The request carries what was asked for and which profile answered, which is what makes a later
    // comparison possible.
    assert_eq!(log.entries()[1].payload["profile"], "test");
    assert_eq!(log.entries()[1].payload["reason"], "chosen by the caller");
}

#[test]
fn a_check_that_fails_is_a_result_and_not_a_fault() {
    let root = scratch("fail");
    let task = TaskSpec::new("make the check fail", &root);
    let work = profile("exit 4", false);
    let policy = allow();
    let model = ScriptedModel::one("echo worked");
    let shell = Shell::new(&root, LIMIT);
    let verifier = CommandVerifier::new(LIMIT);
    let builder = context(&task);

    let mut log = session(&root, "fail");
    let turn = Seams::new(&task, &work, &policy, &builder, &model, &shell, &verifier)
        .turn(&mut log)
        .expect("the turn completed: a failing check is data");

    match &turn.outcome {
        Outcome::Ran {
            run, verification, ..
        } => {
            assert!(run.passed(), "the program was fine");
            assert_eq!(verification.state, State::Failed);
            assert!(
                verification.output.contains("exit 4"),
                "{}",
                verification.output
            );
            assert!(!verification.passed());
        }
        other => panic!("expected a run, got {other:?}"),
    }
}

#[test]
fn a_check_that_cannot_run_is_not_a_pass() {
    let root = scratch("nocheck");
    let task = TaskSpec::new("nothing to check", &root);
    let work = profile("", false);
    let policy = allow();
    let model = ScriptedModel::one("true");
    let shell = Shell::new(&root, LIMIT);
    let verifier = CommandVerifier::new(LIMIT);
    let builder = context(&task);

    let mut log = session(&root, "nocheck");
    let turn = Seams::new(&task, &work, &policy, &builder, &model, &shell, &verifier)
        .turn(&mut log)
        .expect("a turn");

    let Outcome::Ran { verification, .. } = &turn.outcome else {
        panic!("expected a run")
    };
    assert_eq!(verification.state, State::CouldNotRun);
    assert!(!verification.state.is_a_result());
    assert!(
        !verification.passed(),
        "a check that never ran is not a pass"
    );
}

#[test]
fn a_model_fault_is_the_models_and_the_half_turn_stays() {
    let root = scratch("model");
    let task = TaskSpec::new("ask a model that has nothing to say", &root);
    let work = profile("true", false);
    let policy = allow();
    let model = ScriptedModel::new(Vec::<String>::new());
    let shell = Shell::new(&root, LIMIT);
    let verifier = CommandVerifier::new(LIMIT);
    let builder = context(&task);

    let mut log = session(&root, "model");
    let error = Seams::new(&task, &work, &policy, &builder, &model, &shell, &verifier)
        .turn(&mut log)
        .expect_err("no answer, so no turn");

    assert_eq!(error.class(), Class::Model);
    assert!(error.class().counts_against_the_model());

    assert_eq!(
        log.len(),
        2,
        "the request is recorded even when the answer is not"
    );
    assert_eq!(log.entries()[1].kind, Kind::User);
}

#[test]
fn a_denied_action_does_not_run_anything() {
    let root = scratch("deny");
    let task = TaskSpec::new("touch a file", &root);
    let work = profile("true", false);
    let policy = Policy::new(vec![Rule::new(
        harness::SHELL,
        Decision::Deny,
        "the shell is closed here",
    )]);
    let model = ScriptedModel::one("touch ran");
    let shell = Shell::new(&root, LIMIT);
    let verifier = CommandVerifier::new(LIMIT);
    let builder = context(&task);

    let mut log = session(&root, "deny");
    let turn = Seams::new(&task, &work, &policy, &builder, &model, &shell, &verifier)
        .turn(&mut log)
        .expect("a refusal is a turn that ended, not a fault");

    // Evidence, not a counter: the program would have created this file.
    assert!(!root.join("ran").exists(), "nothing ran");

    match &turn.outcome {
        Outcome::Refused { reason } => assert_eq!(reason, "the shell is closed here"),
        other => panic!("expected a refusal, got {other:?}"),
    }

    assert_eq!(log.len(), 3, "meta, the request, and who refused");
    assert_eq!(log.entries()[2].kind, Kind::ApprovalDecision);
    assert_eq!(log.entries()[2].payload["decision"], "deny");
    assert_eq!(
        log.entries()[2].payload["by"],
        "policy",
        "policy refused, and the log says so rather than blaming a person"
    );
}

#[test]
fn a_question_policy_raises_is_pending_rather_than_guessed() {
    let root = scratch("ask");
    let task = TaskSpec::new("touch a file", &root);
    let work = profile("true", false);
    let policy = Policy::new(vec![Rule::new(
        harness::SHELL,
        Decision::Ask,
        "ask a person before the shell runs",
    )]);
    let model = ScriptedModel::one("touch ran");
    let shell = Shell::new(&root, LIMIT);
    let verifier = CommandVerifier::new(LIMIT);
    let builder = context(&task);
    let broker = InMemory::new();

    let mut log = session(&root, "ask");
    let turn = Seams::new(&task, &work, &policy, &builder, &model, &shell, &verifier)
        .brokering(&broker)
        .turn(&mut log)
        .expect("a turn");

    let Outcome::Awaiting { question } = &turn.outcome else {
        panic!("expected a question, got {:?}", turn.outcome)
    };

    assert_eq!(question.action, harness::SHELL);
    assert_eq!(
        question.session, "ask",
        "the question knows which session raised it"
    );
    assert_eq!(broker.answer(question.id).expect("readable"), None);
    assert!(
        !root.join("ran").exists(),
        "nothing ran while the question waits"
    );

    // The question is answerable while the turn is over: no lock is held anywhere.
    broker
        .resolve(question.id, Decision::Allow, Scope::Session, "operator")
        .expect("the question is waiting to be answered");
    assert_eq!(
        broker
            .answer(question.id)
            .expect("readable")
            .expect("an answer")
            .decision,
        Decision::Allow
    );

    assert_eq!(log.len(), 3, "meta, the request, and the question");
    assert_eq!(log.entries()[2].kind, Kind::ApprovalRequest);
    assert_eq!(
        log.entries()[2].payload["reason"],
        "ask a person before the shell runs"
    );
}

#[test]
fn a_standing_answer_lets_the_next_turn_run_without_asking() {
    // The design's headline property, end to end: a question is raised, a person answers it for the session,
    // and the next turn proceeds. The record is reopened from disk in between, so nothing here depends on a
    // process staying alive.
    let root = scratch("standing");
    let approvals = root.join("state/approvals.jsonl");
    let task = TaskSpec::new("say hello", &root);
    let work = profile("true", false);
    let policy = Policy::new(vec![Rule::new(
        harness::SHELL,
        Decision::Ask,
        "ask a person before the shell runs",
    )]);
    let shell = Shell::new(&root, LIMIT);
    let verifier = CommandVerifier::new(LIMIT);
    let builder = context(&task);

    let broker = OnDisk::open(&approvals).expect("a record");
    let mut log = session(&root, "standing");

    let asking = ScriptedModel::one("echo first");
    let turn = Seams::new(&task, &work, &policy, &builder, &asking, &shell, &verifier)
        .brokering(&broker)
        .turn(&mut log)
        .expect("a turn");
    let Outcome::Awaiting { question } = &turn.outcome else {
        panic!("expected a question, got {:?}", turn.outcome)
    };

    // Another process answers it: a fresh broker over the same file, sharing nothing but the file.
    let other = OnDisk::open(&approvals).expect("the same record");
    other
        .resolve(question.id, Decision::Allow, Scope::Session, "operator")
        .expect("answered");
    assert_eq!(other.questions().expect("readable").len(), 1);

    // And the next turn runs, recording what let it.
    let next = ScriptedModel::one("echo second");
    let turn = Seams::new(&task, &work, &policy, &builder, &next, &shell, &verifier)
        .brokering(&other)
        .turn(&mut log)
        .expect("a turn");

    match &turn.outcome {
        Outcome::Ran {
            run, verification, ..
        } => {
            assert_eq!(run.output.trim(), "second");
            assert_eq!(verification.state, State::Passed);
        }
        other => panic!("expected the turn to run, got {other:?}"),
    }

    // The log says who let it through, so it does not read as a policy that allowed it.
    let decision = &log.entries()[4];
    assert_eq!(decision.kind, Kind::ApprovalDecision);
    assert_eq!(decision.payload["decision"], "allow");
    assert_eq!(decision.payload["by"], "operator");
    assert_eq!(decision.payload["question"], 0);
}

#[test]
fn a_standing_no_refuses_the_next_turn_without_asking_again() {
    let root = scratch("standingno");
    let approvals = root.join("state/approvals.jsonl");
    let task = TaskSpec::new("touch a file", &root);
    let work = profile("true", false);
    let policy = Policy::new(vec![Rule::new(
        harness::SHELL,
        Decision::Ask,
        "ask a person before the shell runs",
    )]);
    let shell = Shell::new(&root, LIMIT);
    let verifier = CommandVerifier::new(LIMIT);
    let builder = context(&task);
    let broker = OnDisk::open(&approvals).expect("a record");
    let mut log = session(&root, "standingno");

    let asking = ScriptedModel::one("touch ran");
    let turn = Seams::new(&task, &work, &policy, &builder, &asking, &shell, &verifier)
        .brokering(&broker)
        .turn(&mut log)
        .expect("a turn");
    let Outcome::Awaiting { question } = &turn.outcome else {
        panic!("expected a question")
    };
    broker
        .resolve(question.id, Decision::Deny, Scope::Session, "operator")
        .expect("answered");

    let next = ScriptedModel::one("touch ran");
    let turn = Seams::new(&task, &work, &policy, &builder, &next, &shell, &verifier)
        .brokering(&broker)
        .turn(&mut log)
        .expect("a turn");

    match &turn.outcome {
        Outcome::Refused { reason } => {
            assert!(reason.contains("operator already said no"), "{reason}")
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(!root.join("ran").exists());
    assert_eq!(
        broker.questions().expect("readable").len(),
        1,
        "asked once, not twice"
    );
}

#[test]
fn an_answer_for_one_question_only_asks_again() {
    let root = scratch("once");
    let task = TaskSpec::new("say hello twice", &root);
    let work = profile("true", false);
    let policy = Policy::new(vec![Rule::new(
        harness::SHELL,
        Decision::Ask,
        "ask a person before the shell runs",
    )]);
    let shell = Shell::new(&root, LIMIT);
    let verifier = CommandVerifier::new(LIMIT);
    let builder = context(&task);
    let broker = InMemory::new();
    let mut log = session(&root, "once");

    let first = ScriptedModel::one("echo one");
    let turn = Seams::new(&task, &work, &policy, &builder, &first, &shell, &verifier)
        .brokering(&broker)
        .turn(&mut log)
        .expect("a turn");
    let Outcome::Awaiting { question } = &turn.outcome else {
        panic!("expected a question")
    };
    broker
        .resolve(question.id, Decision::Allow, Scope::Once, "operator")
        .expect("answered");

    let second = ScriptedModel::one("echo two");
    let turn = Seams::new(&task, &work, &policy, &builder, &second, &shell, &verifier)
        .brokering(&broker)
        .turn(&mut log)
        .expect("a turn");

    assert!(
        matches!(turn.outcome, Outcome::Awaiting { .. }),
        "an answer for one question does not answer the next: {:?}",
        turn.outcome
    );
    assert_eq!(broker.questions().expect("readable").len(), 2);
}

#[test]
fn a_question_with_nobody_to_ask_refuses_rather_than_allows() {
    let root = scratch("nobroker");
    let task = TaskSpec::new("touch a file", &root);
    let work = profile("true", false);
    let policy = Policy::new(vec![Rule::new(
        harness::SHELL,
        Decision::Ask,
        "ask a person before the shell runs",
    )]);
    let model = ScriptedModel::one("touch ran");
    let shell = Shell::new(&root, LIMIT);
    let verifier = CommandVerifier::new(LIMIT);
    let builder = context(&task);

    let mut log = session(&root, "nobroker");
    let turn = Seams::new(&task, &work, &policy, &builder, &model, &shell, &verifier)
        .turn(&mut log)
        .expect("a turn");

    match &turn.outcome {
        Outcome::Refused { reason } => {
            assert!(reason.contains("nothing can ask"), "{reason}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(!root.join("ran").exists());
    assert_eq!(log.entries()[2].payload["by"], "policy");
}

#[test]
fn a_profile_that_asks_for_no_judge_is_recorded_as_unjudged() {
    let root = scratch("unjudged");
    let task = TaskSpec::new("nothing to judge", &root);
    let work = profile("true", false);
    let policy = allow();
    let model = ScriptedModel::one("true");
    let shell = Shell::new(&root, LIMIT);
    let verifier = CommandVerifier::new(LIMIT);
    let builder = context(&task);
    let judge = harness::judge::Scripted::always(Verdict::Met);

    let mut log = session(&root, "unjudged");
    let turn = Seams::new(&task, &work, &policy, &builder, &model, &shell, &verifier)
        .judging(&judge)
        .turn(&mut log)
        .expect("a turn");

    let Outcome::Ran { verdict, .. } = &turn.outcome else {
        panic!("expected a run")
    };
    assert_eq!(verdict.verdict, Verdict::Unjudged);
    assert!(!verdict.verdict.is_a_verdict());
    assert!(
        verdict.reason.contains("does not ask"),
        "{}",
        verdict.reason
    );
    assert_eq!(log.entries()[6].payload["verdict"], "unjudged");
}

#[test]
fn a_judge_that_is_asked_for_and_absent_says_so() {
    let root = scratch("nojudge");
    let task = TaskSpec::new("judge this", &root);
    let work = profile("true", true);
    let policy = allow();
    let model = ScriptedModel::one("true");
    let shell = Shell::new(&root, LIMIT);
    let verifier = CommandVerifier::new(LIMIT);
    let builder = context(&task);

    let mut log = session(&root, "nojudge");
    let turn = Seams::new(&task, &work, &policy, &builder, &model, &shell, &verifier)
        .turn(&mut log)
        .expect("a turn");

    let Outcome::Ran { verdict, .. } = &turn.outcome else {
        panic!("expected a run")
    };
    assert_eq!(verdict.verdict, Verdict::Unjudged);
    assert!(
        verdict.reason.contains("none is attached"),
        "{}",
        verdict.reason
    );
}

#[test]
fn a_section_that_does_not_fit_is_dropped_whole_and_named_in_the_log() {
    let root = scratch("dropped");
    let task = TaskSpec::new("tiny", &root);
    let mut work = profile("true", false);
    work.context_tokens = 10;
    let policy = allow();
    let model = ScriptedModel::one("true");
    let shell = Shell::new(&root, LIMIT);
    let verifier = CommandVerifier::new(LIMIT);
    let builder = Budgeted::new(vec![
        Section::new("goal", "tiny"),
        Section::new("huge", "x".repeat(4000)),
    ]);

    let mut log = session(&root, "dropped");
    Seams::new(&task, &work, &policy, &builder, &model, &shell, &verifier)
        .turn(&mut log)
        .expect("a turn");

    assert_eq!(log.entries()[1].payload["dropped"][0], "huge");
    assert_eq!(log.entries()[1].payload["context_tokens"], 1);
}

#[test]
fn a_tool_that_cannot_run_is_a_harness_fault_and_says_what_it_tried() {
    let root = scratch("tool");
    let task = TaskSpec::new("run something", &root);
    let work = profile("true", false);
    let policy = allow();
    let model = ScriptedModel::one("echo hello");
    let broken = Broken;
    let verifier = CommandVerifier::new(LIMIT);
    let builder = context(&task);

    let mut log = session(&root, "tool");
    let error = Seams::new(&task, &work, &policy, &builder, &model, &broken, &verifier)
        .turn(&mut log)
        .expect_err("the tool would not run");

    assert_eq!(error.class(), Class::Harness);
    assert!(!error.class().counts_against_the_model());
    assert_eq!(log.len(), 4, "the attempt is recorded before the failure");
    assert_eq!(log.entries()[3].kind, Kind::ToolResult);
    assert!(
        log.entries()[3].payload["error"]
            .as_str()
            .expect("a string")
            .contains("would not spawn")
    );
}

struct Broken;

impl Tool for Broken {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "broken".to_string(),
            summary: "never runs".to_string(),
        }
    }

    fn call(&self, _call: &Call) -> Result<Run, Fault> {
        Err(Fault::harness("the tool would not spawn"))
    }
}

#[test]
fn a_long_output_is_cut_at_both_ends_and_the_cut_is_recorded() {
    let root = scratch("cut");
    let task = TaskSpec::new("print a lot", &root);
    let work = profile("true", false);
    let policy = allow();
    let model = ScriptedModel::one("seq 1 5000");
    let shell = Shell::new(&root, 200);
    let verifier = CommandVerifier::new(LIMIT);
    let builder = context(&task);

    let mut log = session(&root, "cut");
    let turn = Seams::new(&task, &work, &policy, &builder, &model, &shell, &verifier)
        .turn(&mut log)
        .expect("a turn");

    let Outcome::Ran { run, .. } = &turn.outcome else {
        panic!("expected a run")
    };
    assert!(run.truncated);
    assert!(run.output.contains("bytes omitted"), "{}", run.output);
    assert_eq!(log.entries()[3].payload["truncated"], true);
}

#[test]
fn a_second_turn_resumes_the_log_and_hangs_off_the_first() {
    let root = scratch("resume");
    let task = TaskSpec::new("say hello twice", &root);
    let work = profile("true", false);
    let policy = allow();
    let builder = context(&task);
    let shell = Shell::new(&root, LIMIT);
    let verifier = CommandVerifier::new(LIMIT);

    let store = FileStore::new(root.join("sessions"));
    let id = SessionId::new("resume");
    let mut log = Session::create(store.clone(), FixedClock, id.clone()).expect("a session");

    let first = ScriptedModel::one("echo one");
    Seams::new(&task, &work, &policy, &builder, &first, &shell, &verifier)
        .turn(&mut log)
        .expect("the first turn");
    assert_eq!(log.len(), 7);

    // Resume is a read: the second turn continues off the head of what is on disk.
    let mut log = Session::load(store.clone(), FixedClock, id).expect("a reload");
    let head = log.head().expect("a head");
    let second = ScriptedModel::one("echo two");
    Seams::new(&task, &work, &policy, &builder, &second, &shell, &verifier)
        .turn(&mut log)
        .expect("the second turn");

    assert_eq!(log.entries()[7].parent, Some(head));
    assert_eq!(
        log.len(),
        13,
        "seven from the first turn, six from the second"
    );

    // And both programs are in the one log, in order.
    let programs: Vec<&str> = log
        .entries()
        .iter()
        .filter_map(|entry| entry.payload["program"].as_str())
        .collect();
    assert_eq!(programs, vec!["echo one", "true", "echo two", "true"]);
}

#[test]
fn every_shipped_rule_names_a_profile_that_exists() {
    let rules = harness::Rules::shipped();
    for goal in [
        "fix the broken test",
        "sweep every module",
        "what does this function do",
        "make it nicer",
    ] {
        let task = TaskSpec::new(goal, "/tmp");
        let (id, reason) = harness::Router::pick(&rules, &task);
        assert!(
            harness::profile::named(id.as_str()).is_some(),
            "a rule picked {id} for {goal}, and no profile by that name is shipped"
        );
        assert!(!reason.as_str().is_empty(), "a choice without a reason");
    }
}

#[test]
fn the_log_says_what_the_tool_was_allowed_to_do() {
    // A log that records the program without recording the cage cannot answer "what could that program have
    // reached", which is the first question asked of a run that did something surprising.
    let root = scratch("sandboxlog");
    let task = TaskSpec::new("say hello", &root);
    let work = profile("true", false);
    let policy = Policy::new(vec![Rule::new(harness::SHELL, Decision::Allow, "allowed")]);
    let builder = context(&task);
    let shell = Shell::new(&root, LIMIT).caged(harness::Cage::around(&root));
    let verifier = CommandVerifier::new(LIMIT);
    let model = ScriptedModel::one("echo hello");
    let mut log = session(&root, "sandbox");

    Seams::new(&task, &work, &policy, &builder, &model, &shell, &verifier)
        .turn(&mut log)
        .expect("a turn");

    let call = log
        .entries()
        .iter()
        .find(|entry| entry.kind == Kind::ToolCall)
        .expect("a tool call");
    let sandbox = call.payload["sandbox"]
        .as_str()
        .expect("what the tool allows");
    assert!(sandbox.contains("network: closed"), "{sandbox}");
    assert!(sandbox.contains("writable:"), "{sandbox}");

    // And a turn whose tool is not caged says that instead, rather than saying nothing.
    let open = Shell::new(&root, LIMIT);
    let mut log = session(&root, "sandbox-open");
    let model = ScriptedModel::one("echo hello");
    Seams::new(&task, &work, &policy, &builder, &model, &open, &verifier)
        .turn(&mut log)
        .expect("a turn");
    let call = log
        .entries()
        .iter()
        .find(|entry| entry.kind == Kind::ToolCall)
        .expect("a tool call");
    assert_eq!(
        call.payload["sandbox"],
        "nothing is confined: the program runs with the caller's rights"
    );
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
fn an_edit_answer_writes_the_file_and_is_logged_as_a_patch() {
    let root = scratch("edit");
    std::fs::write(root.join("note.txt"), "alpha\nbeta\n").expect("a file");
    let task = TaskSpec::new("change beta", &root);
    let work = profile("grep -q BETA note.txt", false);
    let policy = allow();
    let model = ScriptedModel::answers([ModelAnswer::editing(vec![FileEdit {
        path: "note.txt".to_string(),
        old: "beta".to_string(),
        new: "BETA".to_string(),
    }])]);
    let shell = Shell::new(&root, LIMIT);
    let verifier = CommandVerifier::new(LIMIT);
    let builder = context(&task);

    let mut log = session(&root, "edit");
    let turn = Seams::new(&task, &work, &policy, &builder, &model, &shell, &verifier)
        .turn(&mut log)
        .expect("a turn");

    match &turn.outcome {
        Outcome::Ran {
            run, verification, ..
        } => {
            assert_eq!(run.status, 0, "{run:?}");
            assert!(run.output.starts_with("patched"), "{}", run.output);
            assert_eq!(verification.state, State::Passed);
        }
        other => panic!("expected a run, got {other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(root.join("note.txt")).expect("the file"),
        "alpha\nBETA\n"
    );

    let call = log
        .entries()
        .iter()
        .find(|entry| entry.kind == Kind::ToolCall)
        .expect("the call");
    assert_eq!(call.payload["tool"], "patch");
    assert!(
        call.payload.get("program").is_none(),
        "an edit is not a program"
    );
    assert!(
        call.payload["rendered"]
            .as_str()
            .unwrap_or_default()
            .contains("-beta"),
        "{}",
        call.payload["rendered"]
    );
    assert!(
        call.payload.get("sandbox").is_none(),
        "an edit does not claim a cage"
    );
    assert_eq!(
        turn.entries.len(),
        6,
        "call, result, check, checkpoint and the two before"
    );
}

#[test]
fn a_refused_edit_writes_nothing_and_runs_no_check() {
    let root = scratch("refused-edit");
    std::fs::write(root.join("note.txt"), "alpha\n").expect("a file");
    let task = TaskSpec::new("change beta", &root);
    // The check would pass on the untouched tree, which is exactly why a refusal must not reach it.
    let work = profile("true", false);
    let policy = allow();
    let model = ScriptedModel::answers([ModelAnswer::editing(vec![FileEdit {
        path: "note.txt".to_string(),
        old: "missing".to_string(),
        new: "x".to_string(),
    }])]);
    let shell = Shell::new(&root, LIMIT);
    let verifier = CommandVerifier::new(LIMIT);
    let builder = context(&task);

    let mut log = session(&root, "refused-edit");
    let turn = Seams::new(&task, &work, &policy, &builder, &model, &shell, &verifier)
        .turn(&mut log)
        .expect("a turn");

    match &turn.outcome {
        Outcome::Unwritten { reason } => assert!(reason.contains("not found"), "{reason}"),
        other => panic!("expected nothing written, got {other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(root.join("note.txt")).expect("the file"),
        "alpha\n",
        "the file is untouched"
    );
    assert_eq!(turn.entries.len(), 3, "user, call, result: no check");
    let kinds: Vec<Kind> = log.entries().iter().map(|entry| entry.kind).collect();
    assert_eq!(
        kinds,
        vec![Kind::Meta, Kind::User, Kind::ToolCall, Kind::ToolResult]
    );
}

#[test]
fn an_answer_that_could_not_be_read_is_recorded_and_writes_nothing() {
    let root = scratch("unreadable");
    let task = TaskSpec::new("change beta", &root);
    let work = profile("true", false);
    let policy = allow();
    let model = ScriptedModel::answers([ModelAnswer::refused(
        "the edit object could not be read: expected {\"edits\":[...]}",
    )]);
    let shell = Shell::new(&root, LIMIT);
    let verifier = CommandVerifier::new(LIMIT);
    let builder = context(&task);

    let mut log = session(&root, "unreadable");
    let turn = Seams::new(&task, &work, &policy, &builder, &model, &shell, &verifier)
        .turn(&mut log)
        .expect("a turn");

    match &turn.outcome {
        Outcome::Unwritten { reason } => assert!(reason.contains("could not be read"), "{reason}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
    let call = log
        .entries()
        .iter()
        .find(|entry| entry.kind == Kind::ToolCall)
        .expect("the call");
    assert_eq!(call.payload["tool"], "patch");
    assert!(
        call.payload["refused"]
            .as_str()
            .unwrap_or_default()
            .contains("could not be read")
    );
}

#[test]
fn a_plan_first_turn_records_the_plan_before_the_program() {
    let root = scratch("plan-first");
    let task = TaskSpec::new("write hello", &root);
    let mut work = profile("true", false);
    work.plan_first = true;
    let policy = allow();
    let model =
        ScriptedModel::one("echo hello").planning(["1. write hello\n2. the check runs true"]);
    let shell = Shell::new(&root, LIMIT);
    let verifier = CommandVerifier::new(LIMIT);
    let builder = context(&task);

    let mut log = session(&root, "plan-first");
    let turn = Seams::new(&task, &work, &policy, &builder, &model, &shell, &verifier)
        .turn(&mut log)
        .expect("a turn");

    assert!(
        matches!(turn.outcome, Outcome::Ran { .. }),
        "{:?}",
        turn.outcome
    );
    let kinds: Vec<Kind> = log.entries().iter().map(|entry| entry.kind).collect();
    assert_eq!(
        kinds,
        vec![
            Kind::Meta,
            Kind::User,
            Kind::Plan,
            Kind::ToolCall,
            Kind::ToolResult,
            Kind::ToolCall,
            Kind::ToolResult,
            Kind::Checkpoint,
        ]
    );
    assert!(
        log.entries()[2].payload["plan"]
            .as_str()
            .expect("a plan")
            .contains("write hello")
    );
}

#[test]
fn an_empty_plan_writes_nothing_and_runs_nothing() {
    let root = scratch("plan-empty");
    let task = TaskSpec::new("write hello", &root);
    let mut work = profile("true", false);
    work.plan_first = true;
    let policy = allow();
    let model = ScriptedModel::one("touch ran.txt").planning(Vec::<String>::new());
    let shell = Shell::new(&root, LIMIT);
    let verifier = CommandVerifier::new(LIMIT);
    let builder = context(&task);

    let mut log = session(&root, "plan-empty");
    let turn = Seams::new(&task, &work, &policy, &builder, &model, &shell, &verifier)
        .turn(&mut log)
        .expect("a turn");
    match &turn.outcome {
        Outcome::Unwritten { reason } => {
            assert!(reason.contains("plan came back empty"), "{reason}")
        }
        other => panic!("expected nothing written, got {other:?}"),
    }
    assert!(!root.join("ran.txt").exists());
    assert!(!log.entries().iter().any(|entry| entry.kind == Kind::Plan));
}

#[test]
fn a_plan_survives_a_compaction_and_comes_back_in_the_prompt() {
    let root = scratch("plan-compacted");
    let task = TaskSpec::new("write hello", &root);
    let mut work = profile("true", false);
    work.plan_first = true;
    let policy = allow();
    let model = ScriptedModel::one("echo one").planning(["1. say one"]);
    let shell = Shell::new(&root, LIMIT);
    let verifier = CommandVerifier::new(LIMIT);
    let builder = context(&task);
    let mut log = session(&root, "plan-compacted");

    Seams::new(&task, &work, &policy, &builder, &model, &shell, &verifier)
        .turn(&mut log)
        .expect("the first turn");
    log.compact(None, "compacted").expect("a cut");

    let learned = harness::context::Learned::from_log(&log);
    assert_eq!(learned.plan(), Some("1. say one"));
    assert!(
        learned
            .sections()
            .iter()
            .any(|section| section.name() == "the plan you wrote"
                && section.text().contains("say one")),
        "the plan must come back in the prompt: {:?}",
        learned
            .sections()
            .iter()
            .map(|s| s.name())
            .collect::<Vec<_>>()
    );
}

#[test]
fn a_second_turn_on_the_same_session_does_not_plan_again() {
    let root = scratch("plan-once");
    let task = TaskSpec::new("write hello", &root);
    let mut work = profile("true", false);
    work.plan_first = true;
    let policy = allow();
    let first_model = ScriptedModel::one("echo one").planning(["1. say one"]);
    let shell = Shell::new(&root, LIMIT);
    let verifier = CommandVerifier::new(LIMIT);
    let builder = context(&task);
    let mut log = session(&root, "plan-once");

    Seams::new(
        &task,
        &work,
        &policy,
        &builder,
        &first_model,
        &shell,
        &verifier,
    )
    .turn(&mut log)
    .expect("the first turn");

    // No plan is queued, so a second plan ask would be empty and the turn would write nothing. It runs,
    // which is the proof that the plan is per session.
    let second_model = ScriptedModel::one("echo two");
    let turn = Seams::new(
        &task,
        &work,
        &policy,
        &builder,
        &second_model,
        &shell,
        &verifier,
    )
    .turn(&mut log)
    .expect("the second turn");
    assert!(
        matches!(turn.outcome, Outcome::Ran { .. }),
        "{:?}",
        turn.outcome
    );
    assert_eq!(
        log.entries()
            .iter()
            .filter(|entry| entry.kind == Kind::Plan)
            .count(),
        1
    );
}

#[test]
fn a_reused_session_plans_again_for_a_different_goal() {
    let root = scratch("plan-two-goals");
    let first = TaskSpec::new("write hello", &root);
    let mut work = profile("true", false);
    work.plan_first = true;
    let policy = allow();
    let shell = Shell::new(&root, LIMIT);
    let verifier = CommandVerifier::new(LIMIT);
    let mut log = session(&root, "plan-two-goals");

    let first_model = ScriptedModel::one("echo one").planning(["1. write hello"]);
    Seams::new(
        &first,
        &work,
        &policy,
        &context(&first),
        &first_model,
        &shell,
        &verifier,
    )
    .turn(&mut log)
    .expect("the first turn");

    // The same session, a second piece of work. A plan written for the first goal is not a plan for this
    // one, so the turn must ask again; if it does not, the queued plan stays unused and the count is one.
    let second = TaskSpec::new("write goodbye", &root);
    let second_model = ScriptedModel::one("echo two").planning(["1. write goodbye"]);
    let turn = Seams::new(
        &second,
        &work,
        &policy,
        &context(&second),
        &second_model,
        &shell,
        &verifier,
    )
    .turn(&mut log)
    .expect("the second turn");
    assert!(
        matches!(turn.outcome, Outcome::Ran { .. }),
        "{:?}",
        turn.outcome
    );

    let plans: Vec<&session_tree::Entry> = log
        .entries()
        .iter()
        .filter(|entry| entry.kind == Kind::Plan)
        .collect();
    assert_eq!(plans.len(), 2, "a new goal needs its own plan");
    assert_eq!(plans[1].payload["goal"], "write goodbye");
    assert_eq!(plans[1].payload["plan"], "1. write goodbye");
}

#[test]
fn a_profile_that_accepts_programs_only_refuses_an_edit_before_writing() {
    let root = scratch("edit-refused");
    std::fs::write(root.join("note.txt"), "alpha\nbeta\n").expect("a file");
    let task = TaskSpec::new("change beta", &root);
    let mut work = profile("grep -q BETA note.txt", false);
    work.edits = false;
    let policy = allow();
    let model = ScriptedModel::answers([ModelAnswer::editing(vec![FileEdit {
        path: "note.txt".to_string(),
        old: "beta".to_string(),
        new: "BETA".to_string(),
    }])]);
    let shell = Shell::new(&root, LIMIT);
    let verifier = CommandVerifier::new(LIMIT);
    let builder = context(&task);
    let mut log = session(&root, "edit-refused");

    let turn = Seams::new(&task, &work, &policy, &builder, &model, &shell, &verifier)
        .turn(&mut log)
        .expect("a turn");

    match &turn.outcome {
        Outcome::Unwritten { reason } => {
            assert!(reason.contains("accepts a program"), "{reason}");
        }
        other => panic!("expected an unwritten turn, got {other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(root.join("note.txt")).expect("the file"),
        "alpha\nbeta\n",
        "an edit the profile refuses must not be applied"
    );
    let call = log
        .entries()
        .iter()
        .find(|entry| entry.kind == Kind::ToolCall)
        .expect("the refusal is recorded");
    assert_eq!(call.payload["tool"], "patch");
    assert_eq!(
        call.payload["refused"],
        "this profile accepts a program, not edit answers"
    );
}

/// A judge that reports what asking it cost, the way the endpoint judge does. The judgement carries the
/// usage because the judge is a model call, and a run that counted every call except this one would end
/// with a spend the endpoint's own numbers do not reconcile with.
struct Costly;

impl Evaluator for Costly {
    fn judge(&self, _report: &Report<'_>) -> Judgement {
        Judgement::new(Verdict::Met, "the script says so").with_usage(harness::Usage {
            prompt_tokens: 40,
            generated_tokens: 2,
            millis: 1,
            reasoning_chars: 0,
        })
    }
}

#[test]
fn a_judged_turn_carries_the_judge_call_in_its_usage() {
    let root = scratch("judge-usage");
    let task = TaskSpec::new("list the files", &root);
    let work = profile("true", true);
    let policy = allow();
    let model =
        ScriptedModel::answers([ModelAnswer::new("echo hello").with_usage(harness::Usage {
            prompt_tokens: 100,
            generated_tokens: 10,
            millis: 4,
            reasoning_chars: 0,
        })]);
    let shell = Shell::new(&root, LIMIT);
    let verifier = CommandVerifier::new(LIMIT);
    let builder = context(&task);
    let mut log = session(&root, "judge-usage");

    let turn = Seams::new(&task, &work, &policy, &builder, &model, &shell, &verifier)
        .judging(&Costly)
        .turn(&mut log)
        .expect("a turn");

    let usage = turn.usage.expect("the turn reports its spend");
    assert_eq!(
        usage.prompt_tokens, 140,
        "the answer's call and the judge's call are both spent"
    );
    assert_eq!(usage.generated_tokens, 12);
    assert!(matches!(turn.outcome, Outcome::Ran { .. }));
}

#[test]
fn an_unwritten_turn_carries_the_call_it_paid_for() {
    // The answer was refused, so nothing ran and nothing was checked. The call still happened, and a
    // token budget that only counted turns that ran would fire one call later than the call that spent it.
    let root = scratch("unwritten-usage");
    let task = TaskSpec::new("list the files", &root);
    let work = profile("true", false);
    let policy = allow();
    let model = ScriptedModel::answers([ModelAnswer::refused("not a program").with_usage(
        harness::Usage {
            prompt_tokens: 250,
            generated_tokens: 5,
            millis: 3,
            reasoning_chars: 0,
        },
    )]);
    let shell = Shell::new(&root, LIMIT);
    let verifier = CommandVerifier::new(LIMIT);
    let builder = context(&task);
    let mut log = session(&root, "unwritten-usage");

    let turn = Seams::new(&task, &work, &policy, &builder, &model, &shell, &verifier)
        .turn(&mut log)
        .expect("a turn");

    assert!(matches!(turn.outcome, Outcome::Unwritten { .. }));
    let usage = turn
        .usage
        .expect("an unwritten turn still paid for its call");
    assert_eq!(usage.prompt_tokens, 250);
    assert_eq!(usage.generated_tokens, 5);
}
