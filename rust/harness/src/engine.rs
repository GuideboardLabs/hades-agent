//! One place that builds a session, its seams and its cage.
//!
//! `turn` and `run` used to assemble the same nine objects inline, twice, and they had already drifted:
//! the turn's cage asked each language runtime where it lives (`.with_runtimes()`) and the walk's did not,
//! so a Python program run by the graph died with `ModuleNotFoundError: No module named 'encodings'` while
//! the same program run by `turn` worked. Two copies of an assembly are two chances to assemble it
//! differently, and the difference is silent until something fails inside the cage.
//!
//! The daemon is what makes this an extraction rather than a tidy-up: it is the third caller, and a daemon
//! that rebuilt these objects itself would be a second engine wearing a different name. The pieces the
//! daemon exists to hold across turns, the shadow store and the repo-map cache, arrive here as fields of
//! [`Assembly`] rather than as something a caller reaches for afterwards.
//!
//! What is deliberately *not* here:
//!
//! - **the model.** It is a seam the caller supplies, because the CLI stands in for it with a program and
//!   the daemon stands in for it with a script, and neither of those is this module's business.
//! - **the context bundle.** A walker builds its own per attempt, so building one here as well would pay
//!   for the repo map twice. [`Engine::context`] builds the single one a lone turn offers, and a caller
//!   that is going to walk simply does not call it.
//! - **the bounds.** They are a property of one walk, not of the engine.

use std::path::PathBuf;

use session_tree::{FileStore, Session, SessionId, SessionStore, SystemClock};

use crate::Cage;
use crate::Harness;
use crate::approval::{ApprovalBroker, OnDisk};
use crate::context::{self, Budgeted, Context, Learned, Section};
use crate::graph::Parts;
use crate::policy::Policy;
use crate::profile::Profile;
use crate::repomap::Cache;
use crate::route::RoutingReason;
use crate::shadow::{Checkpointer, Nothing, Shadows};
use crate::task::TaskSpec;
use crate::tool::{Shell, Tool};
use crate::turn::Seams;
use crate::verify::CommandVerifier;

/// Bytes of a program's output the harness keeps, wherever it runs one.
pub const LIMIT: usize = 8 * 1024;

/// What a caller has to decide before anything can be built.
///
/// Public fields, because every one of them is a decision the caller already made and a builder over nine
/// of them would be ceremony.
pub struct Assembly {
    pub state: PathBuf,
    pub root: PathBuf,
    pub goal: String,
    pub session_id: String,
    /// The profile after the caller has applied its overrides and capped the budget by the model's window.
    pub profile: Profile,
    pub policy: Policy,
    /// The name the policy was asked for by, so a log says which one was chosen rather than only what it does.
    pub policy_name: String,
    pub reason: RoutingReason,
    pub caged: bool,
    /// Where the shadow snapshots go. `None` means nothing is snapshotted, which the run says out loud.
    pub shadow: Option<PathBuf>,
}

/// Everything one run needs except the model and the program.
pub struct Engine {
    task: TaskSpec,
    profile: Profile,
    policy: Policy,
    policy_name: String,
    reason: RoutingReason,
    session_id: SessionId,
    store: FileStore,
    shell: Shell,
    verifier: CommandVerifier,
    broker: OnDisk,
    checkpointer: Box<dyn Checkpointer>,
    /// The map's scan, held across turns. The daemon is why it is a field: a turn that does not consult the
    /// cache pays the cold scan again, which makes the cache a decoration.
    cache: Cache,
    cache_path: PathBuf,
    root: PathBuf,
    state: PathBuf,
}

impl Engine {
    /// Build the shell, the verifier, the session store and the question record. Reads nothing and runs
    /// nothing: a caller can build an engine for a run it then decides not to make.
    pub fn open(spec: Assembly) -> Result<Engine, String> {
        // Confined by default: a run executes a program a model wrote, and the safe arrangement has to be
        // the one you get by not thinking about it. The scratch directory is inside the cage because a
        // program that wants somewhere for a temporary file finds `/tmp` closed and has to be pointed at a
        // place it may write, which is better than being handed the whole of `/tmp`.
        let shell = if spec.caged {
            let scratch = spec.state.join("tmp");
            std::fs::create_dir_all(&scratch)
                .map_err(|error| format!("could not make {}: {error}", scratch.display()))?;
            // `with_runtimes()` asks each interpreter where its own standard library lives, because a
            // Python whose prefix is outside the granted paths dies before it runs a line. Leaving it off
            // is the bug this module was extracted to stop repeating.
            let cage = Cage::around(spec.root.clone())
                .also_writable(scratch)
                .with_runtimes();
            Shell::new(spec.root.clone(), LIMIT).caged(cage)
        } else {
            Shell::new(spec.root.clone(), LIMIT)
        };

        let verifier = CommandVerifier::new(LIMIT);
        let broker = OnDisk::open(spec.state.join("approvals.jsonl")).map_err(|fault| {
            format!("could not open the record of questions: {}", fault.detail())
        })?;

        // The map's scan, loaded once and saved when a caller is done. A cache that will not read is a cold
        // start and a line saying why, never a failed engine: the map is the product, the cache an
        // optimisation of it.
        let cache_path = spec.state.join("repomap.json");
        let cache = match Cache::load(&cache_path) {
            Ok(cache) => cache,
            Err(fault) => {
                if cache_path.exists() {
                    eprintln!("repomap: cold, {}", fault.detail());
                }
                Cache::new()
            }
        };

        Ok(Engine {
            task: TaskSpec::new(spec.goal, spec.root.clone()),
            profile: spec.profile,
            policy: spec.policy,
            policy_name: spec.policy_name,
            reason: spec.reason,
            session_id: SessionId::new(&spec.session_id),
            store: FileStore::new(spec.state.join("sessions")),
            shell,
            verifier,
            broker,
            checkpointer: match spec.shadow {
                Some(store) => Box::new(Shadows::new(store)),
                None => Box::new(Nothing),
            },
            cache,
            cache_path,
            root: spec.root,
            state: spec.state,
        })
    }

    /// The session this engine works in: the one already on disk if there is one, a new one otherwise.
    /// Resuming is exactly this, which is why it is a read rather than a mode.
    pub fn session(&self) -> Result<Session<FileStore, SystemClock>, String> {
        let store = self.store.clone();
        let id = self.session_id.clone();
        if store.exists(&id) {
            Session::load(store, SystemClock, id.clone())
                .map_err(|fault| format!("could not read session {}: {fault}", id.as_str()))
        } else {
            Session::create(store, SystemClock, id.clone())
                .map_err(|fault| format!("could not start session {}: {fault}", id.as_str()))
        }
    }

    /// The single bundle a lone turn offers. A walker must not call this: it builds its own per attempt.
    pub fn context(&mut self) -> Result<Budgeted, String> {
        self.context_with(&Learned::new())
    }

    /// The same bundle with what the session already established in front of it. This is the whole
    /// difference between a new run and a resume: the log is read back, and the turn is told what the log
    /// knows. A caller that passes an empty `Learned` gets the fresh-prompt behaviour, which is why the
    /// default is the empty one.
    pub fn context_with(&mut self, learned: &Learned) -> Result<Budgeted, String> {
        self.context_with_memory(learned, None)
    }

    /// The same bundle with an optional memory section in front of the rest.
    ///
    /// The section is offered like every other, so `Budgeted::build` still applies the budget; it goes
    /// first because a recall cut after the task is a recall the model never saw. The caller has already
    /// capped its text with the recall budget, so a memory section cannot crowd out the task on its own.
    pub fn context_with_memory(
        &mut self,
        learned: &Learned,
        memory: Option<(&str, &str)>,
    ) -> Result<Budgeted, String> {
        let mut sections = context::sections(
            &self.task,
            &self.profile,
            self.profile.context_tokens,
            learned,
            &mut self.cache,
        )
        .map_err(|fault| fault.detail().to_string())?;
        if let Some((name, text)) = memory
            && !text.is_empty()
        {
            sections.insert(0, Section::new(name, text));
        }
        Ok(Budgeted::new(sections))
    }

    /// What the map cache did, for the line a caller prints next to the run.
    pub fn cache(&self) -> &Cache {
        &self.cache
    }

    /// Write the cache beside the state. Called when a run is over, not per attempt: a 17 MB file is not
    /// something to rewrite between attempts, and the value of the cache is the *next* run.
    ///
    /// Two runs on one state and root each hold their own copy, and the last save wins. That is a cache that
    /// loses some work, never a corrupted one: the write is a temp file and a rename.
    pub fn save_cache(&self) -> Result<(), String> {
        self.cache
            .save(&self.cache_path)
            .map_err(|fault| fault.detail().to_string())
    }

    /// The seams one turn runs with.
    pub fn seams<'a>(&'a self, context: &'a dyn Context, model: &'a dyn Harness) -> Seams<'a> {
        Seams::new(
            &self.task,
            &self.profile,
            &self.policy,
            context,
            model,
            &self.shell,
            &self.verifier,
        )
        .brokering(&self.broker)
        .checkpointing(&*self.checkpointer)
        .because(self.reason.clone())
    }

    /// The same parts, for a walker, which builds its own context per attempt.
    pub fn parts<'a>(
        &'a mut self,
        model: &'a dyn Harness,
        evaluator: Option<&'a dyn crate::judge::Evaluator>,
    ) -> Parts<'a> {
        Parts {
            task: &self.task,
            profile: &self.profile,
            policy: &self.policy,
            model,
            tool: &self.shell,
            verifier: &self.verifier,
            broker: Some(&self.broker as &dyn ApprovalBroker),
            checkpointer: Some(&*self.checkpointer),
            evaluator,
            cache: &mut self.cache,
            reason: self.reason.clone(),
        }
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn log_path(&self) -> PathBuf {
        self.store.path_of(&self.session_id)
    }

    pub fn profile(&self) -> &Profile {
        &self.profile
    }

    pub fn policy_name(&self) -> &str {
        &self.policy_name
    }

    pub fn root(&self) -> &std::path::Path {
        &self.root
    }

    pub fn state(&self) -> &std::path::Path {
        &self.state
    }

    /// What the cage allowed, said out loud, because a run that does not say what it could reach cannot be
    /// audited afterwards.
    pub fn cage(&self) -> String {
        self.shell.describe()
    }

    /// What the checkpointer may do, said out loud, for the same reason the cage is.
    pub fn shadows(&self) -> String {
        self.checkpointer.describe()
    }
}
