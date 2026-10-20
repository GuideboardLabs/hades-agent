//! The graph: the workflow as data, with every recovery path declared before the run starts.
//!
//! A loop runs one cycle until a condition the model invents at runtime; a graph makes the control flow
//! explicit, so the path taken when something goes wrong is a property of the program rather than of the
//! model's improvisation. The distinction that matters is not aesthetic. A loop asks the model to invent a
//! recovery at runtime, which is why a loop demos beautifully and a graph is what you want at three in the
//! morning. A graph declares the recovery path up front, and a declared path can be tested: this module
//! tests that every condition a node can produce has an edge to somewhere, which is the same statement as
//! "there is no way to end up in a state nobody thought about".
//!
//! What is *not* here, and should not be: a general workflow engine. The nodes are the units of work this
//! harness already has ([`crate::turn::Seams::turn`] is the whole of one), the conditions are the outcomes
//! the turn already returns, and the graph adds only what was missing, which is where each of those
//! outcomes goes and how many times the whole thing may try.
//!
//! A loop is still the right shape *inside* a node. Attempts are bounded by [`Bounds`] and the attempt node
//! walks the same cycle repeatedly; the graph is what decides how many times, what a failed attempt tells
//! the next one, and where a person has to be asked.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use serde_json::json;
use session_tree::{Clock, Session, SessionStore};

use crate::approval::{ApprovalBroker, ApprovalRequest};
use crate::context::{self, Budgeted, Learned, Section, Try};
use crate::fault::Fault;
use crate::model::Harness;
use crate::policy::Policy;
use crate::profile::Profile;
use crate::repomap::Cache;
use crate::route::RoutingReason;
use crate::shadow::{Checkpointer, Snapshot, Subject};
use crate::task::TaskSpec;
use crate::tool::Tool;
use crate::turn::{Outcome, Seams};
use crate::verify::{State, Verifier};

/// The graph's shape, as data: what can happen, and where each outcome goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Graph {
    nodes: Vec<Node>,
    edges: Vec<Edge>,
}

/// A unit of work, named. The name is what the log records, so it is stable and short.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    id: String,
    kind: Work,
}

/// What a node does. A new shape would need a reason: every one here is a step the walk actually takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Work {
    /// One turn: build context, gate, ask the model, run the program, run the check.
    Attempt,
    /// The return path: an accepted result becomes a constraint for what comes next.
    Accept,
    /// Hand it to a person. The question is already durable; this node is the end of the run, not a block.
    Hand,
    /// A terminal: the run is over, and the outcome says why.
    Done,
    /// A terminal: the run gave up, and the outcome says what it tried.
    GaveUp,
    /// A terminal: a person must decide before this can go further.
    Awaiting,
    /// A terminal: a person stopped the run. It is not a failure and not a retry; the work is over.
    Stopped,
}

/// The condition under which an edge is taken. Every one of these must lead somewhere: see
/// [`Graph::undeclared`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum When {
    /// The check passed.
    Passed,
    /// The check failed and the bounds still allow another attempt.
    Failed,
    /// The answer was refused before it wrote anything, and the bounds still allow another attempt. Not a
    /// failed check: no check ran, and accepting a no-op because the tree happened to satisfy the check
    /// would record work that was never done.
    Unwritten,
    /// The check passed at the attempt bound, so the run stops: a pass that could have gone further is
    /// still a pass, and stopping early is what the bound means.
    Exhausted,
    /// The check could not run. Not a pass, not a failure.
    CouldNotRun,
    /// A fault: the model, the tool or the endpoint broke.
    Faulted,
    /// Policy refused the action before the model was asked.
    Refused,
    /// Policy raised a question, and the run has to end with it open.
    Asked,
    /// A person stopped the run. Distinct from a fault and from a refusal: the work did not fail, it was
    /// ended, and retrying it would be doing what the person just stopped.
    Stopped,
    /// No condition: the edge is the only way out of the node.
    Always,
}

/// One edge: from a node, on a condition, to a node, for a stated reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    from: String,
    when: When,
    to: String,
    why: String,
}

/// How long a whole run may take, how many times it may ask the model, and how many tokens it may spend.
/// Declared, because a graph without bounds is a loop with extra steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    attempts: u32,
    millis: u128,
    /// The token allowance for the whole run, when the caller declared one. `None` is an allowance nobody
    /// declared, not a free one: a bound nobody set is not a bound of zero.
    tokens: Option<u64>,
    /// How many candidate turns one visit to the attempt node may run. One is the ordinary loop; more is a
    /// speculative branch that keeps the first candidate the check accepts.
    width: u32,
}

impl Bounds {
    pub fn new(attempts: u32, minutes: u64) -> Self {
        Bounds {
            attempts: attempts.max(1),
            millis: minutes as u128 * 60_000,
            tokens: None,
            width: 1,
        }
    }

    /// A token allowance for the whole run. The sum is what the endpoint reported, prompt and generated,
    /// because a budget that counted only the answer would let a run spend its whole allowance on context.
    pub fn with_tokens(mut self, tokens: u64) -> Self {
        self.tokens = Some(tokens);
        self
    }

    /// Candidates per attempt visit, bounded at four. A wider branch is not a better branch: the budget is
    /// shared, so width multiplies model calls without multiplying attempts.
    pub fn with_width(mut self, width: u32) -> Self {
        self.width = width.clamp(1, 4);
        self
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    /// The token allowance, when one was declared.
    pub fn tokens(&self) -> Option<u64> {
        self.tokens
    }

    /// The wall clock allowance, in minutes, for a header that has to state the bounds it ran under.
    pub fn minutes(&self) -> u64 {
        (self.millis / 60_000) as u64
    }

    /// The declared bounds as one phrase, so a sentence stating them cannot drift from the values.
    pub fn declared(&self) -> String {
        match self.tokens {
            Some(tokens) => format!(
                "{} attempts, {} minutes, {tokens} tokens",
                self.attempts,
                self.minutes()
            ),
            None => format!("{} attempts, {} minutes", self.attempts, self.minutes()),
        }
    }

    /// Which allowance the run has spent, given how many attempts it has made, how long it has been going
    /// and what the endpoint said it spent. `None` means the run may go on.
    ///
    /// One cause is named at a time, and attempts and time are asked before tokens: a run that reached an
    /// attempt bound it set is not relabelled by the spend that came with it.
    pub fn exhausted(
        &self,
        attempts: u32,
        elapsed: std::time::Duration,
        tokens: u64,
    ) -> Option<Exhausted> {
        if attempts >= self.attempts {
            return Some(Exhausted::Attempts);
        }
        if elapsed.as_millis() >= self.millis {
            return Some(Exhausted::Minutes);
        }
        if self.tokens.is_some_and(|budget| tokens >= budget) {
            return Some(Exhausted::Tokens);
        }
        None
    }

    /// Whether the run has spent its allowance, given how many attempts it has made, how long it has been
    /// going and what it has spent.
    pub fn spent(&self, attempts: u32, elapsed: std::time::Duration, tokens: u64) -> bool {
        self.exhausted(attempts, elapsed, tokens).is_some()
    }
}

/// A token allowance, refused before it is used when it cannot mean what the caller thinks. Zero is not a
/// budget: a run that may not spend is not a run. A budget below one call's completion cap is refused too,
/// because that call would be allowed to spend more than the whole run before anything could stop it.
pub fn budget_tokens(tokens: u64, max_tokens: Option<u64>) -> Result<u64, String> {
    if tokens == 0 {
        return Err(
            "`--budget-tokens 0` is not a budget: a run that may not spend is not a run"
                .to_string(),
        );
    }
    if let Some(call) = max_tokens.filter(|call| *call > tokens) {
        return Err(format!(
            "`--budget-tokens {tokens}` is less than `--max-tokens {call}`: one call could spend the whole \
             run's allowance"
        ));
    }
    Ok(tokens)
}

/// What a run ran out of. The first three are declared budgets; the rest are the ways a run stops that no
/// budget describes, kept typed so a reader never has to parse the human sentence to know which happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exhausted {
    /// The attempt allowance was spent.
    Attempts,
    /// The wall clock allowance was spent.
    Minutes,
    /// The token allowance was spent.
    Tokens,
    /// The model, the tool or the endpoint broke. A wrong endpoint does not fix itself.
    Fault,
    /// Policy denied the action outright. No answer would change it.
    Refused,
    /// The check could not run, which is not evidence either way.
    CouldNotRun,
}

impl Exhausted {
    /// The cause as a word, for the log and for `hades explain`. The sentence in `reason` says the rest.
    pub fn name(&self) -> &'static str {
        match self {
            Exhausted::Attempts => "attempts",
            Exhausted::Minutes => "minutes",
            Exhausted::Tokens => "tokens",
            Exhausted::Fault => "a fault",
            Exhausted::Refused => "a refusal",
            Exhausted::CouldNotRun => "a check that could not run",
        }
    }
}

/// What a run has spent, in tokens, as the endpoint reported it. Prompt and generated together, because
/// both are spent and a bound that counted only the answer would under-report every call.
fn tokens(spent: &[crate::task::Usage]) -> u64 {
    spent
        .iter()
        .map(|usage| usage.prompt_tokens + usage.generated_tokens)
        .sum()
}

impl Default for Bounds {
    fn default() -> Self {
        Bounds::new(3, 10)
    }
}

impl Graph {
    /// The graph this harness ships: try, accept a pass, retry a failure while the bounds allow, hand a
    /// refusal or a question to a person, and give up on a fault or a check that could not run.
    ///
    /// Each edge carries its reason, because an edge with no reason is a line nobody can delete.
    pub fn shipped() -> Graph {
        let edge = |from: &str, when: When, to: &str, why: &str| Edge {
            from: from.to_string(),
            when,
            to: to.to_string(),
            why: why.to_string(),
        };
        Graph {
            nodes: vec![
                Node::new("attempt", Work::Attempt),
                Node::new("accept", Work::Accept),
                Node::new("hand", Work::Hand),
                Node::new("done", Work::Done),
                Node::new("gave_up", Work::GaveUp),
                Node::new("awaiting", Work::Awaiting),
                Node::new("stopped", Work::Stopped),
            ],
            edges: vec![
                edge(
                    "attempt",
                    When::Passed,
                    "accept",
                    "the check passed, so the result is worth keeping",
                ),
                edge(
                    "attempt",
                    When::Failed,
                    "attempt",
                    "the check failed but the run may try again",
                ),
                edge(
                    "attempt",
                    When::Unwritten,
                    "attempt",
                    "the answer wrote nothing and said why, so the next attempt is shown the reason",
                ),
                edge(
                    "attempt",
                    When::Exhausted,
                    "gave_up",
                    "the bounds are spent, so trying again would be ignoring them",
                ),
                edge(
                    "attempt",
                    When::CouldNotRun,
                    "gave_up",
                    "a check that could not run is not evidence of anything, and retrying a broken check \
                     only wastes attempts",
                ),
                edge(
                    "attempt",
                    When::Faulted,
                    "gave_up",
                    "a fault is reported rather than retried: a wrong endpoint does not fix itself",
                ),
                edge(
                    "attempt",
                    When::Refused,
                    "gave_up",
                    "policy denied the action outright, and a denial is not a question: no answer would \
                     change it, so the run stops and says so",
                ),
                edge(
                    "attempt",
                    When::Asked,
                    "hand",
                    "a question exists and outlives this process, so the run ends with it open",
                ),
                edge(
                    "attempt",
                    When::Stopped,
                    "stopped",
                    "a person stopped the run, so no further attempt is made: a stop is not a retry",
                ),
                edge(
                    "accept",
                    When::Always,
                    "done",
                    "an accepted result is the end of the run",
                ),
                edge(
                    "hand",
                    When::Always,
                    "awaiting",
                    "a person has to decide, and that is a state",
                ),
            ],
        }
    }

    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    pub fn edges(&self) -> &[Edge] {
        &self.edges
    }

    pub fn node(&self, id: &str) -> Option<&Node> {
        self.nodes.iter().find(|node| node.id == id)
    }

    /// The node an edge leads to, or nothing when no edge matches: nothing is a bug in the graph, not a
    /// state to handle at runtime, which is why [`Graph::undeclared`] exists and is tested.
    pub fn next(&self, from: &str, when: When) -> Option<&Node> {
        let edge = self
            .edges
            .iter()
            .find(|edge| edge.from == from && edge.when == when)?;
        self.node(&edge.to)
    }

    /// The reason an edge gives for itself, for the log.
    pub fn why(&self, from: &str, when: When) -> Option<&str> {
        self.edges
            .iter()
            .find(|edge| edge.from == from && edge.when == when)
            .map(|edge| edge.why.as_str())
    }

    /// Every condition a node can produce that has no edge out of it. Empty for a graph with its recovery
    /// paths declared, which is the property worth testing rather than the walk itself.
    pub fn undeclared(&self, from: &str, conditions: &[When]) -> Vec<When> {
        conditions
            .iter()
            .copied()
            .filter(|when| self.next(from, *when).is_none())
            .collect()
    }

    /// The conditions an attempt node can be left in. The runner can produce exactly these, so this list
    /// and the edges out of `attempt` have to agree.
    pub fn attempt_conditions() -> Vec<When> {
        vec![
            When::Passed,
            When::Failed,
            When::Unwritten,
            When::Exhausted,
            When::CouldNotRun,
            When::Faulted,
            When::Refused,
            When::Asked,
            When::Stopped,
        ]
    }
}

impl Node {
    pub fn new(id: impl Into<String>, kind: Work) -> Self {
        Node {
            id: id.into(),
            kind,
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn kind(&self) -> Work {
        self.kind
    }
}

/// One visit to a node, and the condition it was left in. The walk, as it happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Visit {
    pub node: String,
    pub when: When,
    /// The edge's own reason for being there.
    pub why: Option<String>,
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ended {
    /// The check passed and the result was accepted.
    Accepted { attempts: u32, program: String },
    /// The run stopped without a pass, and this is why. The typed cause names the allowance that ran out,
    /// so a reader does not have to parse `reason` to know whether raising `--attempts` would help.
    GaveUp {
        attempts: u32,
        reason: String,
        exhausted: Exhausted,
    },
    /// A person has to answer before this can go further.
    Awaiting {
        attempts: u32,
        question: Box<ApprovalRequest>,
    },
    /// A person stopped the run. Not a pass and not a failure: the work was ended, and the count says how
    /// many attempts had actually been made when it stopped.
    Stopped { attempts: u32 },
}

impl Ended {
    /// The typed cause of a giving-up, when there was one. `None` for the ends that are not a giving-up.
    pub fn cause(&self) -> Option<Exhausted> {
        match self {
            Ended::GaveUp { exhausted, .. } => Some(*exhausted),
            _ => None,
        }
    }

    pub fn code(&self) -> i32 {
        match self {
            Ended::Accepted { .. } => 0,
            Ended::GaveUp { .. } => 1,
            Ended::Awaiting { .. } => 2,
            Ended::Stopped { .. } => 1,
        }
    }

    pub fn line(&self) -> String {
        match self {
            Ended::Accepted { attempts, .. } => {
                format!("accepted after {attempts} attempt(s)")
            }
            Ended::GaveUp {
                attempts, reason, ..
            } => {
                format!("gave up after {attempts} attempt(s): {reason}")
            }
            Ended::Awaiting { attempts, question } => format!(
                "awaiting an answer to question #{} after {attempts} attempt(s)",
                question.id.0
            ),
            Ended::Stopped { attempts } => format!("stopped after {attempts} attempt(s)"),
        }
    }
}

/// Everything one walk of the graph produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub visits: Vec<Visit>,
    pub ended: Ended,
    pub learned: Learned,
    /// What each attempt cost, in order. Empty entries when the endpoint said nothing, which is what a
    /// scripted model does: nobody spent anything.
    pub spent: Vec<crate::task::Usage>,
    /// Every candidate a speculative visit ran, in order. Empty for an ordinary single-attempt visit.
    pub branches: Vec<Candidate>,
    /// Where this run's one note was filed, when a memory was attached and the run was accepted.
    pub recorded: Option<PathBuf>,
    /// Why the note was not filed, when a memory was attached and the run was accepted but the write failed.
    pub memory_error: Option<String>,
    /// What the judge said about the last attempt, when the profile asked for one.
    pub verdict: Option<crate::judge::Judgement>,
}

/// One candidate of a speculative attempt: its own session, its own check result, and the program it ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub session: String,
    pub condition: String,
    pub passed: bool,
    pub program: String,
    /// The check's partial report, when it gave one. A pass always wins; this only orders failures.
    pub progress: Option<crate::verify::Progress>,
}

impl Run {
    /// The number of times the model was asked. A branch asks once per candidate, so this counts every ask
    /// rather than every visit to the attempt node.
    pub fn attempts(&self) -> u32 {
        match &self.ended {
            Ended::Accepted { attempts, .. }
            | Ended::GaveUp { attempts, .. }
            | Ended::Awaiting { attempts, .. }
            | Ended::Stopped { attempts } => *attempts,
        }
    }

    /// The programs tried, with what the check said, in order. This is the run's own account of itself.
    pub fn tried(&self) -> &[Try] {
        self.learned.tries()
    }
}

/// What a walk is told to remember: the section already recalled by the caller, so recall is once per
/// run rather than once per attempt, and the writer that files the one accepted outcome.
pub struct Remembering<'a> {
    pub section: Section,
    pub writer: &'a dyn crate::memory::Memory,
    pub topics: Vec<String>,
}

/// What a walker needs in order to work. Public fields, because every one of them is a borrow the caller
/// already holds and a builder over eight borrows would be ceremony.
pub struct Parts<'a> {
    pub task: &'a TaskSpec,
    pub profile: &'a Profile,
    pub policy: &'a Policy,
    pub model: &'a dyn Harness,
    pub tool: &'a dyn Tool,
    pub verifier: &'a dyn Verifier,
    pub broker: Option<&'a dyn ApprovalBroker>,
    pub checkpointer: Option<&'a dyn Checkpointer>,
    /// The judge, when the profile asks for one and the caller has one to lend.
    pub evaluator: Option<&'a dyn crate::judge::Evaluator>,
    /// The map's scan, held by whoever owns this walker. A walk that reaches the map without it pays the
    /// cold scan on every attempt, which is the cost this field exists to stop paying.
    pub cache: &'a mut Cache,
    pub reason: RoutingReason,
}

/// The walk. Holds the parts and the session, because a node's job is to use them.
pub struct Runner<'a, S, C>
where
    S: SessionStore + Clone,
    C: Clock + Clone,
{
    graph: Graph,
    bounds: Bounds,
    task: &'a TaskSpec,
    profile: &'a Profile,
    policy: &'a Policy,
    model: &'a dyn Harness,
    tool: &'a dyn Tool,
    verifier: &'a dyn Verifier,
    broker: Option<&'a dyn ApprovalBroker>,
    checkpointer: Option<&'a dyn Checkpointer>,
    evaluator: Option<&'a dyn crate::judge::Evaluator>,
    cache: &'a mut Cache,
    reason: RoutingReason,
    memory: Option<Remembering<'a>>,
    /// Where the model's pieces go while it writes them, when a surface is watching.
    feed: Option<crate::model::FeedFn<'a>>,
    /// A person's stop, when the caller can receive one. Checked before an attempt and again before the
    /// program runs; a stop is cooperative, so a call already in flight finishes first.
    stopping: Option<&'a AtomicBool>,
    session: &'a mut Session<S, C>,
    /// The program a passed attempt ran, carried to the accept node so the constraint can name it.
    asked: Option<String>,
    /// What the last node that stopped something said, so the terminal node can repeat it.
    last_reason: Option<String>,
    /// Which allowance the last stopping node ran out of, so the giving-up end carries a typed cause rather
    /// than a sentence someone has to parse.
    last_cause: Option<Exhausted>,
    /// The question a refused or pending attempt produced, carried to the awaiting node.
    last_question: Option<ApprovalRequest>,
    /// The last judge's verdict, for a receipt. The session log remains the record; this is so `hades run`
    /// can print what the judge said without reading its own log back.
    last_verdict: Option<crate::judge::Judgement>,
    /// Where a finished walk is scored. Every entry point that walks gets this, because a scoreboard the
    /// daemon does not write is a scoreboard the live bot never feeds.
    scoreboard: Option<&'a Path>,
}

impl<'a, S, C> Runner<'a, S, C>
where
    S: SessionStore + Clone,
    C: Clock + Clone,
{
    /// A walker over the parts it needs. Not a [`Seams`], because a walker builds its own context for each
    /// attempt: a context builder here would be a parameter that is never read, and the repo map is the
    /// expensive part of building one.
    pub fn new(parts: Parts<'a>, session: &'a mut Session<S, C>) -> Self {
        Runner {
            graph: Graph::shipped(),
            bounds: Bounds::default(),
            task: parts.task,
            profile: parts.profile,
            policy: parts.policy,
            model: parts.model,
            tool: parts.tool,
            verifier: parts.verifier,
            broker: parts.broker,
            checkpointer: parts.checkpointer,
            evaluator: parts.evaluator,
            cache: parts.cache,
            reason: parts.reason,
            memory: None,
            feed: None,
            stopping: None,
            session,
            asked: None,
            last_reason: None,
            last_cause: None,
            last_question: None,
            last_verdict: None,
            scoreboard: None,
        }
    }

    /// What the last attempt's judge said, when one ran.
    pub fn verdict(&self) -> Option<&crate::judge::Judgement> {
        self.last_verdict.as_ref()
    }

    pub fn with_bounds(mut self, bounds: Bounds) -> Self {
        self.bounds = bounds;
        self
    }

    pub fn with_graph(mut self, graph: Graph) -> Self {
        self.graph = graph;
        self
    }

    /// A place for the model's reasoning and content while it writes them. Every attempt feeds the same
    /// sink, because a person watching does not care which retry the words came from.
    pub fn with_feed(mut self, feed: crate::model::FeedFn<'a>) -> Self {
        self.feed = Some(feed);
        self
    }

    /// Give the walk one section in front of every attempt and a writer for the one accepted outcome.
    pub fn with_memory(mut self, memory: Remembering<'a>) -> Self {
        self.memory = Some(memory);
        self
    }

    /// A stop a person can raise while the walk runs. It is checked before each attempt and, inside the
    /// turn, before the program runs, so a stop never lets a model's program start after it was asked for.
    /// It cannot abort a model call or a shell command already in flight; the run ends at the next boundary.
    pub fn with_stop(mut self, flag: &'a AtomicBool) -> Self {
        self.stopping = Some(flag);
        self
    }

    fn stopped(&self) -> bool {
        self.stopping
            .is_some_and(|flag| flag.load(Ordering::SeqCst))
    }

    /// Score every finished walk in `state`. A write failure is ignored: a scoreboard is advice, and losing
    /// a row must never turn a finished run into a failed one.
    pub fn with_scoreboard(mut self, state: &'a Path) -> Self {
        self.scoreboard = Some(state);
        self
    }

    /// Walk the graph until a terminal node. Every step is recorded, and the log is the session's.
    pub fn run(&mut self) -> Result<Run, Fault> {
        let started = Instant::now();
        // Where the reading starts: what this session already established, rather than an empty mind. A
        // second run on a session id is a resume, and a resume that hands the next attempt the same prompt
        // as the first is a second roll of the same dice wearing the word "retry".
        let mut learned = Learned::from_log(self.session);
        // Where this run's own constraints begin. The log carries earlier runs, and a note that cites all
        // of them repeats history instead of saying what this walk established.
        let prior = learned.constraints().len();
        let mut visits = Vec::new();
        let mut branches: Vec<Candidate> = Vec::new();
        let mut attempts = 0u32;
        let mut node = "attempt".to_string();
        let mut spent: Vec<crate::task::Usage> = Vec::new();
        // The edge that brought us to the node we are on: where from, and under which condition. A
        // terminal node has no edge out of it, so this is how its checkpoint says why the walk ended.
        let mut arrived: Option<(String, When)> = None;

        loop {
            let here = self
                .graph
                .node(&node)
                .ok_or_else(|| Fault::harness(format!("the graph has no node called {node}")))?
                .clone();

            // A terminal node is entered, recorded, and that is the end of the walk. Checking this before
            // any work means a terminal never has to have an edge out of it, which is the one node kind
            // where "no way on" is the correct answer.
            if let Some(ended) = self.ended(here.kind(), attempts)? {
                let (from, when) = arrived
                    .take()
                    .unwrap_or_else(|| (here.id.clone(), When::Always));
                let why = self.graph.why(&from, when).map(str::to_string);
                visits.push(Visit {
                    node: here.id.clone(),
                    when,
                    why: why.clone(),
                });
                // An end, not a transition: nothing was taken, so there is no edge to name. What the record
                // keeps is the reason the walk stopped.
                // The typed cause and the sentence it came with go into the record: `hades explain` reads
                // the log, and a cause that only lived in this process would die with the process.
                let exhausted = ended.cause().map(|cause| cause.name());
                let reason = match &ended {
                    Ended::GaveUp { reason, .. } => Some(reason.as_str()),
                    _ => None,
                };
                self.session.append(
                    session_tree::Kind::Checkpoint,
                    json!({
                        "node": here.id,
                        "condition": format!("{when:?}"),
                        "ended": true,
                        "why": why,
                        "reason": reason,
                        "exhausted": exhausted,
                        "attempt": attempts,
                    }),
                )?;
                // Atropos, at the end of the walk: what this run *read* is history now, and if it is too
                // much of the next bundle it is cut here, before the caller releases the session's lease. A
                // compaction that cannot be written does not fail a run that already succeeded; it says so
                // and the log keeps everything either way.
                if let Err(fault) =
                    crate::compact::if_due(self.session, &learned, self.profile.context_tokens)
                {
                    eprintln!("compaction: not written, {fault}");
                }
                let mut recorded = None;
                let mut memory_error = None;
                if let (Some(memory), Ended::Accepted { attempts, program }) =
                    (&self.memory, &ended)
                {
                    let fresh: Vec<String> = learned.constraints()[prior..]
                        .iter()
                        .map(|constraint| constraint.text().to_string())
                        .collect();
                    let summary = fresh
                        .last()
                        .cloned()
                        .unwrap_or_else(|| format!("a run that passed: {program}"));
                    let note = crate::memory::OutcomeNote {
                        goal: self.task.goal().to_string(),
                        session: self.session.id().to_string(),
                        run: 1,
                        attempt: *attempts as usize,
                        title: self.task.goal().chars().take(80).collect(),
                        summary,
                        constraints: fresh,
                        check: self.profile.verify.clone(),
                        files: Vec::new(),
                        topics: memory.topics.clone(),
                    };
                    match memory.writer.record(&note) {
                        Ok(done) => recorded = Some(done.path),
                        Err(said) => memory_error = Some(said),
                    }
                }
                if let Some(state) = self.scoreboard {
                    let (used, passed) = match &ended {
                        Ended::Accepted { attempts, .. } => (*attempts, true),
                        Ended::GaveUp { attempts, .. }
                        | Ended::Awaiting { attempts, .. }
                        | Ended::Stopped { attempts } => (*attempts, false),
                    };
                    let at = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|since| since.as_secs())
                        .unwrap_or(0);
                    let _ = crate::scoreboard::record(
                        state,
                        &crate::scoreboard::Record {
                            profile: self.profile.id.as_str().to_string(),
                            passed,
                            attempts: used,
                            millis: started.elapsed().as_millis() as u64,
                            // What the endpoints said this walk spent, summed over every attempt and every
                            // candidate: the scoreboard's job is to reconcile with the model's own numbers.
                            prompt_tokens: spent.iter().map(|usage| usage.prompt_tokens).sum(),
                            generated_tokens: spent
                                .iter()
                                .map(|usage| usage.generated_tokens)
                                .sum(),
                            at,
                        },
                    );
                }
                return Ok(Run {
                    visits,
                    ended,
                    learned,
                    spent,
                    branches,
                    recorded,
                    memory_error,
                    verdict: self.last_verdict.clone(),
                });
            }

            let when = match here.kind() {
                Work::Attempt => {
                    // A stop is checked before the count grows: an attempt that was never made should not
                    // appear in the ending as one that was. The turn checks again before the program, so a
                    // stop raised while the model was answering still prevents the side effect.
                    if self.stopped() {
                        self.last_reason = Some("stopped by a person".to_string());
                        When::Stopped
                    } else {
                        let remaining = self.bounds.attempts().saturating_sub(attempts);
                        let width = self.bounds.width().min(remaining).max(1);
                        if width > 1 {
                            // A speculative visit spends its candidates from the same attempt budget. The
                            // first pass wins and the rest are rolled back, so width costs model calls
                            // rather than a second budget.
                            let before = branches.len();
                            let (when, program, usage) =
                                self.branch(width, &mut learned, &mut branches, started)?;
                            let made = (branches.len() - before) as u32;
                            attempts += made;
                            spent.extend(usage);
                            if when == When::Passed {
                                self.asked = Some(program);
                            }
                            // The last candidate that could run was the last the budget allowed. The spend is
                            // read here because the candidates' own usage is what the endpoint reported.
                            let out =
                                self.bounds
                                    .exhausted(attempts, started.elapsed(), tokens(&spent));
                            if when == When::Failed {
                                match out {
                                    Some(cause) => {
                                        self.last_cause = Some(cause);
                                        When::Exhausted
                                    }
                                    None => when,
                                }
                            } else if when == When::Exhausted {
                                // The branch stopped itself when the clock ran out: its loop checks time,
                                // and the parent's token total is only read once the candidates are home.
                                self.last_cause = Some(Exhausted::Minutes);
                                when
                            } else if when == When::Refused {
                                self.last_cause = Some(Exhausted::Refused);
                                when
                            } else {
                                when
                            }
                        } else {
                            attempts += 1;
                            // The token total is only known after the call, so the allowance is read inside
                            // the attempt: a run cannot be told it is out of tokens it has not spent yet.
                            let before = tokens(&spent);
                            let (when, program, usage) =
                                self.attempt(attempts, before, started.elapsed(), &mut learned)?;
                            if let Some(usage) = usage {
                                spent.push(usage);
                            }
                            if when == When::Passed {
                                // The pass is carried to the accept node by the walk; the program rides
                                // along so the accepted constraint can name it.
                                self.asked = Some(program);
                            }
                            when
                        }
                    }
                }
                Work::Accept => {
                    self.accept(&mut learned);
                    When::Always
                }
                Work::Hand => When::Always,
                Work::Done | Work::GaveUp | Work::Awaiting | Work::Stopped => When::Always,
            };

            let target = self
                .graph
                .next(&here.id, when)
                .ok_or_else(|| {
                    Fault::harness(format!(
                        "the graph declared no edge out of {} for {when:?}, which is a bug in the graph \
                         rather than a state to handle",
                        here.id
                    ))
                })?
                .clone();

            visits.push(Visit {
                node: here.id.clone(),
                when,
                why: self.graph.why(&here.id, when).map(str::to_string),
            });
            self.record(&here.id, when, &target.id, attempts)?;

            // The edge just taken travels with the walk, so the last line of the log explains why the run
            // stopped where it did.
            arrived = Some((here.id.clone(), when));
            node = target.id.clone();
        }
    }

    /// Where the run is, and why it moved: a checkpoint, because that is what it is, and the session tree's
    /// kinds are the Python spec's rather than this module's to extend.
    fn record(&mut self, from: &str, when: When, to: &str, attempts: u32) -> Result<(), Fault> {
        self.session.append(
            session_tree::Kind::Checkpoint,
            json!({
                "node": to,
                "condition": format!("{when:?}"),
                "edge": format!("{from} -> {to}"),
                "why": self.graph.why(from, when),
                "attempt": attempts,
            }),
        )?;
        Ok(())
    }

    /// The end of the walk, when the node is one. Nothing for a node that does work.
    fn ended(&mut self, kind: Work, attempts: u32) -> Result<Option<Ended>, Fault> {
        let ended = match kind {
            Work::Done => Some(Ended::Accepted {
                attempts,
                program: self.asked.take().unwrap_or_default(),
            }),
            Work::GaveUp => {
                let exhausted = self.last_cause.take().ok_or_else(|| {
                    Fault::harness(
                        "the run reached the giving-up node without a cause, which is a bug in the walk: \
                         nothing recorded which allowance it ran out of"
                            .to_string(),
                    )
                })?;
                Some(Ended::GaveUp {
                    attempts,
                    reason: self.last_reason.clone().unwrap_or_else(|| {
                        "the run stopped and the node that stopped it said nothing".to_string()
                    }),
                    exhausted,
                })
            }
            Work::Awaiting => Some(Ended::Awaiting {
                attempts,
                question: Box::new(self.last_question.clone().ok_or_else(|| {
                    Fault::harness(
                        "the run reached the awaiting node without a question, which is a bug in the \
                         walk: nothing raised one"
                            .to_string(),
                    )
                })?),
            }),
            Work::Attempt | Work::Accept | Work::Hand => None,
            Work::Stopped => Some(Ended::Stopped { attempts }),
        };
        Ok(ended)
    }

    /// Run one candidate of a speculative visit: a full walk of its own over a forked session, bounded to a
    /// single attempt. Reusing the walk is the point: a candidate has exactly the semantics of a turn, its
    /// log is a session, and its check is the same check.
    fn branch_once(&mut self, fork: &mut Session<S, C>, minutes: u64) -> Result<Run, Fault> {
        let parts = Parts {
            task: self.task,
            profile: self.profile,
            policy: self.policy,
            model: self.model,
            tool: self.tool,
            verifier: self.verifier,
            broker: self.broker,
            // The branch's base snapshot already makes the candidate undoable. A second snapshot inside the
            // candidate would copy the tree again for no new guarantee, so the branch pays for one copy.
            checkpointer: None,
            evaluator: self.evaluator,
            cache: self.cache,
            reason: self.reason.clone(),
        };
        let mut runner = Runner::new(parts, fork).with_bounds(Bounds::new(1, minutes));
        if let Some(feed) = self.feed {
            runner = runner.with_feed(feed);
        }
        if let Some(stopping) = self.stopping {
            runner = runner.with_stop(stopping);
        }
        runner.run()
    }

    /// Record a branch batch that ended before a winner: a question, a stop, a refusal, or a fault. The
    /// parent log names every candidate session even when the batch did not choose one.
    fn note_branch(&mut self, branches: &[Candidate]) -> Result<(), Fault> {
        self.session.append(
            session_tree::Kind::Checkpoint,
            json!({
                "node": "attempt",
                "branched": branches.len(),
                "winner": null,
                "candidates": branches.iter().map(|b| json!({
                    "session": b.session,
                    "condition": b.condition,
                    "passed": b.passed,
                    "progress": b.progress.as_ref().map(|progress| progress.json()),
                })).collect::<Vec<_>>(),
            }),
        )?;
        Ok(())
    }

    /// A speculative visit: `width` candidates from the same tree, each in its own session, stopped at the
    /// first one the check accepts. A failed candidate is rolled back before the next, so a branch that is
    /// not taken leaves no edit behind. A join with no winner restores the base and says so.
    fn branch(
        &mut self,
        width: u32,
        learned: &mut Learned,
        branches: &mut Vec<Candidate>,
        started: std::time::Instant,
    ) -> Result<(When, String, Vec<crate::task::Usage>), Fault> {
        let checkpointer = self.checkpointer.ok_or_else(|| {
            Fault::harness(
                "branching needs a shadow: a candidate edits the tree, and the next one must start from the \
                 same state (run with --shadow on)"
                    .to_string(),
            )
        })?;
        let subject = Subject::new(self.session.id().to_string(), self.task.root());
        let base = match checkpointer.snapshot(&subject)? {
            Snapshot::Taken(id) => id,
            Snapshot::Declined(why) => {
                return Err(Fault::harness(format!(
                    "branching needs a snapshot of the tree and the checkpointer declined: {why}"
                )));
            }
        };

        let mut spent = Vec::new();
        let mut last = String::from("no candidate ran");
        for index in 0..width {
            if index > 0 {
                // Put the tree back before the next candidate, so candidates are independent.
                checkpointer.restore(&subject, &base)?;
            }
            if self.stopped() {
                self.last_reason = Some("stopped by a person".to_string());
                return Ok((When::Stopped, String::new(), spent));
            }
            if started.elapsed().as_millis() >= self.bounds.millis {
                last = format!(
                    "the {}-minute bound was spent after {} candidate(s)",
                    self.bounds.minutes(),
                    index
                );
                break;
            }

            let mut fork = self.session.fork(None)?;
            let session_id = fork.id().to_string();
            let minutes = self
                .bounds
                .minutes()
                .saturating_sub((started.elapsed().as_secs() / 60).min(self.bounds.minutes()));
            let walked = self.branch_once(&mut fork, minutes.max(1))?;
            // Some outcomes are not candidate failures: a question must reach a person, a stop ends the
            // walk, and a policy refusal is final. Only a failed check or an unwritten answer is rolled back.
            if let Ended::Awaiting { question, .. } = &walked.ended {
                self.last_question = Some((**question).clone());
                self.last_reason = Some("a question is open".to_string());
                branches.push(Candidate {
                    session: session_id,
                    condition: "a question is open".to_string(),
                    passed: false,
                    program: String::new(),
                    progress: None,
                });
                self.note_branch(branches)?;
                return Ok((When::Asked, String::new(), spent));
            }
            if matches!(walked.ended, Ended::Stopped { .. }) {
                self.last_reason = Some("stopped by a person".to_string());
                branches.push(Candidate {
                    session: session_id,
                    condition: "stopped".to_string(),
                    passed: false,
                    program: String::new(),
                    progress: None,
                });
                self.note_branch(branches)?;
                return Ok((When::Stopped, String::new(), spent));
            }
            if walked.visits.last().map(|visit| visit.when) == Some(When::Refused) {
                self.last_reason = Some("the policy refused a candidate".to_string());
                branches.push(Candidate {
                    session: session_id,
                    condition: "refused".to_string(),
                    passed: false,
                    program: String::new(),
                    progress: None,
                });
                self.note_branch(branches)?;
                return Ok((When::Refused, String::new(), spent));
            }

            let passed = matches!(walked.ended, Ended::Accepted { .. });
            // The sub-run's own single attempt is always "exhausted" when it fails, which is true of its
            // one-attempt budget and useless as a description of the candidate. Report what the check said
            // instead, and fall back to the ended reason only when there is no try to read.
            let condition = if passed {
                "passed".to_string()
            } else {
                walked
                    .tried()
                    .last()
                    .map(|tried| format!("the check {}", tried.state()))
                    .unwrap_or_else(|| match &walked.ended {
                        Ended::GaveUp { reason, .. } => reason.clone(),
                        Ended::Awaiting { .. } => "a question is open".to_string(),
                        Ended::Stopped { .. } => "stopped".to_string(),
                        Ended::Accepted { .. } => "passed".to_string(),
                    })
            };
            let program = match &walked.ended {
                Ended::Accepted { program, .. } => program.clone(),
                _ => String::new(),
            };
            spent.extend(walked.spent.iter().cloned());
            if let Some(verdict) = walked.verdict.clone() {
                self.last_verdict = Some(verdict);
            }
            branches.push(Candidate {
                session: session_id,
                condition: condition.clone(),
                passed,
                program: program.clone(),
                progress: walked
                    .tried()
                    .last()
                    .and_then(|tried| tried.progress())
                    .cloned(),
            });

            if passed {
                self.session.append(
                    session_tree::Kind::Checkpoint,
                    json!({
                        "node": "attempt",
                        "branched": width,
                        "winner": branches.last().map(|b| b.session.clone()),
                        "candidates": branches.iter().map(|b| json!({
                            "session": b.session,
                            "condition": b.condition,
                            "passed": b.passed,
                        })).collect::<Vec<_>>(),
                    }),
                )?;
                return Ok((When::Passed, program, spent));
            }

            last = condition;
            if let Some(tried) = walked.tried().last() {
                learned.tried(tried.clone());
            }
        }

        // No candidate passed: the run leaves the tree as it found it. A failed branch is evidence in its
        // own session, not an edit the next step has to clean up.
        checkpointer.restore(&subject, &base)?;
        let best = branches
            .iter()
            .filter(|candidate| !candidate.passed)
            .filter_map(|candidate| {
                candidate
                    .progress
                    .as_ref()
                    .map(|progress| (candidate, progress.ratio()))
            })
            .max_by(|left, right| left.1.total_cmp(&right.1));
        self.last_reason = Some(match best {
            Some((candidate, _)) => format!(
                "{} candidate(s) ran and none passed: {}; the best partial was {} at {}",
                branches.len(),
                last,
                candidate.condition,
                candidate
                    .progress
                    .as_ref()
                    .map(|progress| progress.line())
                    .unwrap_or_default()
            ),
            None => format!(
                "{} candidate(s) ran and none passed: {last}",
                branches.len()
            ),
        });
        self.session.append(
            session_tree::Kind::Checkpoint,
            json!({
                "node": "attempt",
                "branched": branches.len(),
                "winner": null,
                "partial": best.map(|(candidate, ratio)| json!({
                    "session": candidate.session,
                    "condition": candidate.condition,
                    "ratio": ratio,
                    "progress": candidate.progress.as_ref().map(|progress| progress.json()),
                })),
                "candidates": branches.iter().map(|b| json!({
                    "session": b.session,
                    "condition": b.condition,
                    "passed": b.passed,
                    "progress": b.progress.as_ref().map(|progress| progress.json()),
                })).collect::<Vec<_>>(),
            }),
        )?;
        let exhausted = started.elapsed().as_millis() >= self.bounds.millis;
        Ok((
            if exhausted {
                When::Exhausted
            } else {
                When::Failed
            },
            String::new(),
            spent,
        ))
    }

    /// One turn, with the run's learned context in front of it. Returns the condition the attempt was left
    /// in, and the program it ran (empty when there was none).
    ///
    /// `before` is what the earlier attempts spent, in tokens, because the token bound can only be decided
    /// once this call reports its own cost: reading it beforehand would spend a call past the allowance.
    fn attempt(
        &mut self,
        attempt: u32,
        before: u64,
        elapsed: std::time::Duration,
        learned: &mut Learned,
    ) -> Result<(When, String, Option<crate::task::Usage>), Fault> {
        let mut sections = context::sections(
            self.task,
            self.profile,
            self.profile.context_tokens,
            learned,
            self.cache,
        )?;
        if let Some(memory) = &self.memory {
            sections.insert(0, memory.section.clone());
        }
        let builder = Budgeted::new(sections);
        let seams = Seams::new(
            self.task,
            self.profile,
            self.policy,
            &builder,
            self.model,
            self.tool,
            self.verifier,
        )
        .because(self.reason.clone());
        let seams = match self.feed {
            Some(feed) => seams.feeding(feed),
            None => seams,
        };
        let seams = match self.broker {
            Some(broker) => seams.brokering(broker),
            None => seams,
        };
        let seams = match self.checkpointer {
            Some(checkpointer) => seams.checkpointing(checkpointer),
            None => seams,
        };
        let seams = match self.evaluator {
            Some(evaluator) => seams.judging(evaluator),
            None => seams,
        };
        let seams = match self.stopping {
            Some(stopping) => seams.stopping(stopping),
            None => seams,
        };

        let outcome = match seams.turn(self.session) {
            Ok(turn) => turn.outcome,
            Err(fault) => {
                // A fault ends the attempt and is reported rather than swallowed into a retry.
                self.last_reason = Some(format!("{} fault: {}", fault.class(), fault.detail()));
                self.last_cause = Some(Exhausted::Fault);
                return Ok((When::Faulted, String::new(), None));
            }
        };

        match outcome {
            Outcome::Ran {
                answer,
                run,
                verification,
                verdict,
            } => {
                self.last_verdict = Some(verdict);
                let program = answer.rendered();
                // The call has reported what it cost, so now the whole run's spend is known.
                let usage = answer.usage();
                let cost =
                    before + usage.map_or(0, |usage| usage.prompt_tokens + usage.generated_tokens);
                let out = self.bounds.exhausted(attempt, elapsed, cost);
                let when = match verification.state {
                    State::Passed => When::Passed,
                    State::Failed if out.is_some() => When::Exhausted,
                    State::Failed => When::Failed,
                    State::CouldNotRun => When::CouldNotRun,
                };
                if verification.state == State::CouldNotRun {
                    // A check that could not run is not a budget end: the run stops because the evidence is
                    // missing, and no allowance was involved.
                    self.last_cause = Some(Exhausted::CouldNotRun);
                } else if when == When::Exhausted {
                    self.last_cause = out;
                }
                if verification.state != State::Passed {
                    learned.tried(
                        Try::new(
                            program.clone(),
                            format!("{}\n{}", verification.output, run.output),
                            verification.state_line(),
                        )
                        .observed(run.status, run.output.trim().is_empty())
                        .progressed(verification.progress.clone()),
                    );
                    self.last_reason = Some(if when == When::Exhausted {
                        format!(
                            "the bounds were spent after {attempt} attempt(s) ({}), and the check still {}",
                            self.bounds.declared(),
                            verification.state_line()
                        )
                    } else {
                        format!(
                            "attempt {attempt} did not pass: the check {}",
                            verification.state_line()
                        )
                    });
                }
                Ok((when, program, usage))
            }
            Outcome::Unwritten { reason } => {
                let out = self.bounds.exhausted(attempt, elapsed, before);
                let when = if out.is_some() {
                    self.last_cause = out;
                    self.last_reason = Some(format!(
                        "the bounds were spent after {attempt} attempt(s) ({}) and the last answer wrote \
                         nothing: {reason}",
                        self.bounds.declared()
                    ));
                    When::Exhausted
                } else {
                    learned.tried(
                        Try::new(
                            "an answer that wrote nothing".to_string(),
                            reason.clone(),
                            "unwritten",
                        )
                        .observed(1, false),
                    );
                    self.last_reason = Some(format!("attempt {attempt} wrote nothing: {reason}"));
                    When::Unwritten
                };
                Ok((when, String::new(), None))
            }
            Outcome::Refused { reason } => {
                self.last_reason = Some(reason);
                self.last_cause = Some(Exhausted::Refused);
                Ok((When::Refused, String::new(), None))
            }
            Outcome::Stopped => {
                self.last_reason = Some("stopped by a person".to_string());
                Ok((When::Stopped, String::new(), None))
            }
            Outcome::Awaiting { question } => {
                self.last_question = Some(question);
                self.last_reason = Some("a question is open".to_string());
                Ok((When::Asked, String::new(), None))
            }
        }
    }

    /// The return path: what an accepted result establishes for whatever comes next. The constraints
    /// themselves live in `context::accepted`, because reading a log back has to rebuild the same ones.
    fn accept(&mut self, learned: &mut Learned) {
        let program = self.asked.take().unwrap_or_default();
        for constraint in context::accepted(&self.profile.verify, &program) {
            learned.accept(constraint);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_condition_an_attempt_can_produce_has_a_declared_edge() {
        // This is the whole argument for a graph over a loop: the recovery path is declared up front, and
        // "declared" is testable. An undeclared condition is a state nobody thought about, which is exactly
        // what a loop discovers at three in the morning.
        let graph = Graph::shipped();
        let undeclared = graph.undeclared("attempt", &Graph::attempt_conditions());
        assert!(
            undeclared.is_empty(),
            "an attempt can be left in {undeclared:?} with nowhere to go"
        );
    }

    #[test]
    fn a_check_that_passed_is_accepted_and_a_failure_gets_another_attempt() {
        let graph = Graph::shipped();
        assert_eq!(
            graph.next("attempt", When::Passed).map(Node::id),
            Some("accept")
        );
        assert_eq!(
            graph.next("attempt", When::Failed).map(Node::id),
            Some("attempt")
        );
        // But not for ever: exhausted goes somewhere else, which is what makes the bound real.
        assert_eq!(
            graph.next("attempt", When::Exhausted).map(Node::id),
            Some("gave_up")
        );
    }

    #[test]
    fn a_denial_stops_the_run_and_a_question_waits_for_a_person() {
        // These are two different things and a graph that conflated them would be wrong about one of them.
        // A denial is not a failure to retry, because retrying asks the same person the same question; but
        // it is also not a question, because no answer changes a policy. A question, on the other hand, is
        // durable and belongs to whoever can answer it, so the run ends with it open rather than blocking.
        let graph = Graph::shipped();
        assert_eq!(
            graph.next("attempt", When::Refused).map(Node::id),
            Some("gave_up")
        );
        assert_eq!(
            graph.next("attempt", When::Asked).map(Node::id),
            Some("hand")
        );
        assert_eq!(
            graph.next("hand", When::Always).map(Node::id),
            Some("awaiting")
        );
    }

    #[test]
    fn a_fault_is_reported_rather_than_retried() {
        let graph = Graph::shipped();
        assert_eq!(
            graph.next("attempt", When::Faulted).map(Node::id),
            Some("gave_up")
        );
        assert_eq!(
            graph.next("attempt", When::CouldNotRun).map(Node::id),
            Some("gave_up")
        );
    }

    #[test]
    fn every_node_is_reachable_from_the_start_and_every_edge_leads_somewhere() {
        let graph = Graph::shipped();
        let mut reached: Vec<String> = vec!["attempt".to_string()];
        let mut frontier = vec!["attempt".to_string()];
        while let Some(from) = frontier.pop() {
            for edge in graph.edges().iter().filter(|edge| edge.from == from) {
                assert!(
                    graph.node(&edge.to).is_some(),
                    "{} -> {} leads nowhere",
                    edge.from,
                    edge.to
                );
                if !reached.contains(&edge.to) {
                    reached.push(edge.to.clone());
                    frontier.push(edge.to.clone());
                }
            }
        }
        for node in graph.nodes() {
            assert!(reached.contains(&node.id), "{} is unreachable", node.id);
        }
    }

    #[test]
    fn every_edge_says_why_it_exists() {
        // An edge with no reason is a line nobody can delete.
        for edge in Graph::shipped().edges() {
            assert!(!edge.why.trim().is_empty(), "{} -> {}", edge.from, edge.to);
        }
    }

    #[test]
    fn the_bounds_stop_a_run_that_keeps_failing() {
        let bounds = Bounds::new(3, 10);
        assert!(!bounds.spent(2, std::time::Duration::from_secs(1), 0));
        assert!(bounds.spent(3, std::time::Duration::from_secs(1), 0));
        assert!(bounds.spent(1, std::time::Duration::from_secs(600), 0));
        // A bound of zero attempts is still one attempt: a run that cannot try is not a run.
        assert_eq!(Bounds::new(0, 1).attempts(), 1);
        // A token bound is spent at the allowance, not past it, and only when one was declared.
        let budgeted = Bounds::new(3, 10).with_tokens(1500);
        assert_eq!(budgeted.exhausted(1, std::time::Duration::ZERO, 1499), None);
        assert_eq!(
            budgeted.exhausted(1, std::time::Duration::ZERO, 1500),
            Some(Exhausted::Tokens)
        );
        assert_eq!(Bounds::new(3, 10).tokens(), None);
    }

    #[test]
    fn a_terminal_node_says_which_kind_of_end_it_is() {
        assert_eq!(
            Ended::Accepted {
                attempts: 1,
                program: "true".into()
            }
            .code(),
            0
        );
        assert_eq!(
            Ended::GaveUp {
                attempts: 3,
                reason: "never passed".into(),
                exhausted: Exhausted::Attempts
            }
            .code(),
            1
        );
        assert!(
            Ended::GaveUp {
                attempts: 3,
                reason: "never passed".into(),
                exhausted: Exhausted::Attempts
            }
            .line()
            .contains("3 attempt")
        );
    }
}
