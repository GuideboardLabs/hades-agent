#![recursion_limit = "512"]

//! The Hades harness: one turn of the loop, built rather than translated.
//!
//! The three crates under it are ports, because they had an oracle: the session tree, the vault, and
//! governance. This one has none, so it is designed, and the design rule is the whole reason it exists:
//! **the substrate is translated, the engine is designed**. Writing it in Python first would leak dicts
//! and duck typing into types that are meant to be strict, and the strictness is the product.
//!
//! One turn is:
//!
//! 1. build context under a budget,
//! 2. ask the model for a program,
//! 3. let policy decide before anything runs,
//! 4. run the program,
//! 5. run the check, which is a command,
//! 6. judge the turn, when the profile asks for a judge,
//! 7. append what happened, in the order it happened.
//!
//! Everything the turn touches outside itself arrives as a seam: the model, the tool, the verifier, the
//! judge, the approval broker, the context builder. That is what makes a turn testable with no model
//! call in the loop, which is the only way to know the loop works before the model is wired in.
//!
//! The pieces that hold state across turns exist: the checkpointer in [`shadow`], the daemon in [`daemon`],
//! the map cache in [`repomap`], and the one assembly in [`engine`] that builds them for both.
//!
//! Deliberately absent, each its own piece of work rather than a stub:
//!
//! - **the sandbox at the seam.** [`tool::Shell`] runs a program with the caller's rights. The cage in
//!   [`cage`] is applied when an engine is assembled and is on by default there, so the default path is
//!   confined; a caller that builds a `Shell` itself gets a shell, which is the honest statement.
//! - **the router's scoreboard** is [`scoreboard`]: one row per finished walk, a pass rate and mean time
//!   per profile, and a choice that abstains until three runs exist. The rules always run first; the
//!   scoreboard answers only the fall-through, and every choice records which half made it.

pub mod acp;
pub mod activity;
pub mod agent;
pub mod approval;
pub mod boundary;
pub mod browser;
pub mod builtin;
pub mod cage;
pub mod cases;
pub mod claim;
pub mod cli;
pub mod command;
pub mod compact;
pub mod config;
pub mod context;
pub mod cron;
pub mod daemon;
pub mod distill;
pub mod doctor;
pub mod engine;
pub mod eval;
pub mod fault;
pub mod fetch;
// libcurl loaded at run time: one connection pool for a run, and a fallback when it is absent.
mod fetch_pool;
pub mod floor;
pub mod gateway;
pub mod graph;
pub mod harness_files;
pub mod http;
pub mod httpd;
pub mod judge;
pub mod library;
pub mod mcp;
pub mod memory;
pub mod mode;
pub mod model;
pub mod pack;
pub mod pack_fit;
pub mod packs_install;
pub mod packs_menu;
pub mod patch;
pub mod path;
pub mod policy;
pub mod profile;
pub mod question;
pub mod recipe;
pub mod repomap;
pub mod research;
pub mod retry;
pub mod route;
pub mod ruler;
pub mod scoreboard;
/// Where a test writes: never inside the source tree's `target/`. Public because the integration
/// tests are separate crates and need the same root as the unit tests.
pub mod scratch;
pub mod service;
pub mod shadow;
pub mod skills;
pub mod task;
pub mod taste;
pub mod tool;
pub mod tui;
pub mod tuning;
pub mod turn;
pub mod verify;
pub mod vision;

pub use approval::{
    Answer, ApprovalBroker, ApprovalRequest, InMemory, OnDisk, PendingId, Question, Scope, Standing,
};
pub use cage::Cage;
pub use context::{Budgeted, Bundle, Constraint, Context, Learned, Section, Try, sections};
pub use daemon::{Daemon, ask, live};
pub use engine::{Assembly, Engine, LIMIT};
pub use fault::{Class, Fault};
pub use graph::{
    BaselineGate, Bounds, Edge, Ended, Exhausted, Graph, Node, Parts, Run, Runner, Visit, When,
    Work,
};
pub use judge::{Evaluator, Judgement, Report, Verdict};
pub use model::{
    Harness, Key, OpenAi, Preset, Scripted as ScriptedModel, from_preset, preset, presets,
};
pub use policy::{Decision, Policy, Rule, Ruling};
pub use profile::{Effort, Profile, ProfileId, Role};
pub use repomap::{Definition, File as MappedFile, Map as RepoMap};
pub use route::{Router, RoutingReason, Rules};
pub use shadow::{Checkpointer, Manifest, Nothing, Restored, Shadows, SnapshotId, Subject};
pub use task::{Budget, ModelAnswer, TaskSpec, Usage};
pub use tool::{Call, Shell, Tool, ToolSchema};
pub use turn::{Outcome, Seams, Turn};
pub use verify::{CommandVerifier, State, Verification, Verifier};

/// The name the shell tool answers to in the policy, the log and the schema. One constant, so a rule
/// cannot be written against a name nothing uses.
pub const SHELL: &str = "shell";

/// The tool an edit answer is recorded under, beside the program the shell seam runs.
pub const PATCH: &str = "patch";

/// The name the check answers to in the log. The check is a run like any other, so it is recorded as one
/// rather than as a special case.
pub const VERIFY: &str = "verify";

/// Process-wide environment, shared by every test in this binary. `set_var` is not per-test: a test that sets
/// `HADES_PROXY` or `PATH` and runs beside another one changes what that test sees, and the failure surfaces
/// somewhere else entirely. Tests that rewrite it take this lock and put the old value back.
#[cfg(test)]
pub(crate) mod test_env {
    use std::sync::{Mutex, MutexGuard, OnceLock};

    fn lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    /// Held for the duration of a test that rewrites the process environment. One lock, not one per module:
    /// a per-module lock cannot order a test in `vision` against a test in `command`, and both can be running.
    pub(crate) fn hold() -> MutexGuard<'static, ()> {
        lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
