//! The failure corpus: a failed run becomes a task that can be run again.
//!
//! A case is not the transcript. It is a task, an oracle, a starting tree and the conditions it failed
//! under: goal, check, profile, model, policy, the full resolved ceiling, and a pinned copy of the tree
//! before the run's first write. Replaying the model's wrong answer would test the transcript; giving the
//! model the same goal and the same start tests the harness.
//!
//! Storage is append-only and machine-local, beside sessions and shadows:
//!
//! ```text
//! <state>/cases/<profile>.jsonl              one JSON object per line
//! <state>/cases/<profile>/<id>/tree/         the pinned starting tree
//! <state>/cases/<profile>/<id>/runs/<n>.jsonl  the eval session a result cites
//! ```
//!
//! Three kinds of line. `case` declares one; a later `case` line with the same id wins (that is how
//! `forget` retires it). `seen` records that the same failure happened again without rewriting the tree or
//! racing another process. `result` records one eval arm; `status` derives only from `arm: "base"`, so a
//! candidate that passes can never flip the case to fixed on its own.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::graph::{Ended, Exhausted, Run};
use crate::shadow::{Shadows, SnapshotId, Subject};

/// The names a snapshot skips by design. A case records them so a replay knows what the pinned tree is
/// missing on purpose rather than by accident.
pub const IGNORED: [&str; 3] = [".git", "target", "node_modules"];

/// A warning at this size per profile. Not a cap: silently deleting the oldest failure is how a corpus
/// becomes a lie.
pub const WARN_BYTES: u64 = 512 * 1024 * 1024;

/// How many attempts a case was given, and how long, and how many tokens. All four, because a `tokens`
/// failure replayed with no token budget can pass for a reason the failing run never had.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    pub attempts: u32,
    pub minutes: u64,
    pub tokens: Option<u64>,
    pub width: u32,
}

impl Bounds {
    pub fn line(&self) -> String {
        match self.tokens {
            Some(tokens) => format!(
                "{} attempts, {} minutes, {tokens} tokens, width {}",
                self.attempts, self.minutes, self.width
            ),
            None => format!(
                "{} attempts, {} minutes, no token ceiling, width {}",
                self.attempts, self.minutes, self.width
            ),
        }
    }
}

/// One task the harness failed, with everything needed to put it back on the bench.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Case {
    pub id: String,
    pub goal: String,
    pub check: String,
    /// The frozen verifier at record time: the check command's digest plus every protected file's. A case
    /// whose ruler cannot be reproduced is `broken`, not passed.
    pub ruler: Option<String>,
    /// A command that succeeds while the vulnerability is present and must fail after the fix.
    pub attack: Option<String>,
    pub profile: String,
    pub model: String,
    pub policy: String,
    pub bounds: Bounds,
    pub attempts_used: u32,
    pub cause: String,
    pub tree: String,
    pub snapshot: String,
    pub source: String,
    pub ignored: Vec<String>,
    pub skipped_special: u64,
    pub partial: bool,
    pub bytes: u64,
    pub files: u64,
    pub status: String,
    pub at: i64,
}

/// One eval arm's result. `outcome` is `pass`, `fail`, `stale`, `broken` or `blocked`; `arm` is `base` or
/// `candidate`; the two never overwrite each other, and only `base` moves the case's status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultLine {
    pub id: String,
    pub arm: String,
    pub outcome: String,
    pub attack: Option<String>,
    pub attempts: u32,
    pub spent: u64,
    pub seconds_milli: u64,
    pub session: String,
    pub state: String,
    pub harness: Option<String>,
    pub model: String,
    pub at: i64,
}

/// What a surface asked for: record every eligible failure, none, or one with a partial tree too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Auto,
    Off,
    Force,
}

impl Mode {
    pub fn parse(text: &str) -> Result<Mode, String> {
        match text {
            "auto" => Ok(Mode::Auto),
            "off" => Ok(Mode::Off),
            "force" => Ok(Mode::Force),
            other => Err(format!(
                "`--case {other}`: say auto (record), off (do not record) or force (record a partial tree)"
            )),
        }
    }
}

/// What the record path did. A skipped case is still a line on the receipt: a failure that cannot be
/// reproduced has to say why, or the corpus looks complete when it is not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recorded {
    Case(Box<Case>),
    Skipped(String),
}

pub fn cases_dir(state: &Path) -> PathBuf {
    state.join("cases")
}

pub fn profile_dir(state: &Path, profile: &str) -> PathBuf {
    cases_dir(state).join(safe_profile(profile))
}

pub fn ledger(state: &Path, profile: &str) -> PathBuf {
    cases_dir(state).join(format!("{}.jsonl", safe_profile(profile)))
}

/// A profile id is used as a file name. Only `[a-z0-9-]` survive; anything else becomes `-`, and an empty
/// result becomes `profile`.
pub fn safe_profile(profile: &str) -> String {
    let mut out = String::new();
    for character in profile.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() || character == '-' {
            out.push(character);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "profile".to_string()
    } else {
        trimmed
    }
}

/// A case id is a file name and later reaches `remove_dir_all`. Refuse anything that is not the shape this
/// module writes: a run of `[a-z0-9-]`, a dash, then exactly twelve lowercase hex digits.
pub fn valid_id(id: &str) -> bool {
    if id.len() < 15 || id.len() > 80 {
        return false;
    }
    let Some((head, tail)) = id.rsplit_once('-') else {
        return false;
    };
    !head.is_empty()
        && head.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        })
        && tail.len() == 12
        && tail.chars().all(|character| character.is_ascii_hexdigit())
}

/// The id is a digest of what makes the task the same task: profile, goal, check and the pinned tree.
pub fn case_id(profile: &str, goal: &str, check: &str, tree: &str) -> String {
    let digest = mneme::sha256::hex(format!("{profile}\n{goal}\n{check}\n{tree}").as_bytes());
    format!("{}-{}", safe_profile(profile), &digest[..12])
}

/// The digest of a tree with no files in it, taken from a real empty directory so it cannot drift from what
/// `tree_digest` computes for one.
pub fn empty_tree_digest(state: &Path) -> Result<String, String> {
    let dir = state
        .join("tmp")
        .join(format!("empty-tree-{}", std::process::id()));
    crate::scratch::state_dir(&dir)
        .map_err(|error| format!("could not make {}: {error}", dir.display()))?;
    let digest = tree_digest(&dir)?;
    let _ = std::fs::remove_dir_all(&dir);
    Ok(digest)
}

/// A digest over the sorted `(path, mode, file digest)` list. Directories are not entries: an empty
/// directory carries no content a replay can lose, and a mode alone on a directory is not evidence.
pub fn tree_digest(root: &Path) -> Result<String, String> {
    let mut files: Vec<(String, u32, String)> = Vec::new();
    walk(root, root, &mut files)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut text = String::new();
    for (path, mode, digest) in files {
        text.push_str(&format!("{path}\0{mode:o}\0{digest}\n"));
    }
    Ok(mneme::sha256::hex(text.as_bytes()))
}

fn walk(root: &Path, at: &Path, out: &mut Vec<(String, u32, String)>) -> Result<(), String> {
    let listing =
        fs::read_dir(at).map_err(|error| format!("could not read {}: {error}", at.display()))?;
    let mut entries: Vec<PathBuf> = listing.flatten().map(|entry| entry.path()).collect();
    entries.sort();
    for path in entries {
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("could not stat {}: {error}", path.display()))?;
        if metadata.is_dir() {
            walk(root, &path, out)?;
        } else if metadata.is_file() {
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let bytes = fs::read(&path)
                .map_err(|error| format!("could not read {}: {error}", path.display()))?;
            #[cfg(unix)]
            let mode = {
                use std::os::unix::fs::PermissionsExt;
                metadata.permissions().mode() & 0o7777
            };
            #[cfg(not(unix))]
            let mode = 0;
            out.push((relative, mode, mneme::sha256::hex(&bytes)));
        }
        // A special path is not part of the digest: a pinned tree that contains one is partial, and the
        // digest is computed over what was actually pinned.
    }
    Ok(())
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or(0)
}

/// Record a failed run as a case, when it is one. The snapshot label comes from the run, not from
/// `shadows/<session>/1`: a resumed session appends to a log whose first snapshot belongs to an earlier run.
pub fn record(
    state: &Path,
    run: &Run,
    goal: &str,
    source: &str,
    check: &str,
    attack: Option<&str>,
    mode: Mode,
) -> Result<Recorded, String> {
    if mode == Mode::Off {
        return Ok(Recorded::Skipped(
            "recording is off (`--case off`)".to_string(),
        ));
    }
    let cause = match &run.ended {
        Ended::GaveUp { exhausted, .. } => match exhausted {
            Exhausted::Attempts => "attempts",
            Exhausted::Minutes => "minutes",
            Exhausted::Tokens => "tokens",
            // A fault, a refusal, a check that could not run or timed out is not a failure of the harness's
            // work: replaying it reproduces the environment, not the harness.
            _ => {
                return Ok(Recorded::Skipped(format!(
                    "the run ended on {}, which is not a bound the harness can replay",
                    exhausted.name()
                )));
            }
        },
        _ => {
            return Ok(Recorded::Skipped(
                "only a giving-up run becomes a case".to_string(),
            ));
        }
    };
    let Some(label) = &run.snapshot else {
        return Ok(Recorded::Skipped(format!(
            "there is no tree to pin: {}",
            run.snapshot_declined
                .as_deref()
                .unwrap_or("no snapshot was taken")
        )));
    };
    let Some((session, n)) = label.split_once('#') else {
        return Ok(Recorded::Skipped(format!(
            "the snapshot label `{label}` does not name a tree"
        )));
    };
    let Ok(n) = n.parse::<u64>() else {
        return Ok(Recorded::Skipped(format!(
            "the snapshot label `{label}` does not name a tree"
        )));
    };
    let shadows = Shadows::new(state.join("shadows"));
    let id = SnapshotId::new(session, n);
    let manifest = shadows
        .manifest(&id)
        .map_err(|fault| format!("the tree for {label} has no manifest: {}", fault.detail()))?;
    let tree_source = state.join("shadows").join(session).join(n.to_string());
    // A snapshot of an empty tree has a manifest and no directory: there were no files to copy. It is a
    // valid starting point, and the case pins an empty tree rather than declining one.
    let empty_tree = !tree_source.is_dir();
    if empty_tree && !manifest.files.is_empty() {
        return Ok(Recorded::Skipped(format!(
            "the snapshot {label} names is missing"
        )));
    }
    if manifest.special > 0 && mode != Mode::Force {
        return Ok(Recorded::Skipped(format!(
            "the tree skipped {} special path(s) (symlinks, fifos, sockets); pass `--case force` to pin a \
             partial tree",
            manifest.special
        )));
    }

    let profile = run.profile.clone();
    let check = check.to_string();
    let tree = if empty_tree {
        empty_tree_digest(state)?
    } else {
        tree_digest(&tree_source)?
    };
    let id = case_id(&profile, goal, &check, &tree);
    let dir = profile_dir(state, &profile).join(&id);
    if !valid_id(&id) {
        return Err(format!(
            "the case id `{id}` is not a name this module writes"
        ));
    }

    // Dedupe by id, atomically: copy into a temp directory, rename it into place. A rename onto an existing
    // case tree fails rather than replacing evidence, and the failure is the duplicate signal.
    let tree_target = dir.join("tree");
    if tree_target.is_dir() {
        append(
            state,
            &profile,
            &json!({"kind": "seen", "id": id, "at": now(), "source": source}),
        )?;
        let case = find(state, &profile, &id)?
            .unwrap_or_else(|| fallback_case(run, goal, &check, &tree, &id, source));
        return Ok(Recorded::Case(Box::new(case)));
    }
    let temp = dir.join(format!("tree.tmp.{}", std::process::id()));
    if temp.exists() {
        let _ = fs::remove_dir_all(&temp);
    }
    crate::scratch::state_dir(&dir)
        .map_err(|error| format!("could not make {}: {error}", dir.display()))?;
    if empty_tree {
        crate::scratch::state_dir(&temp)
            .map_err(|error| format!("could not make {}: {error}", temp.display()))?;
    } else {
        copy_dir(&tree_source, &temp)?;
    }
    match fs::rename(&temp, &tree_target) {
        Ok(()) => {}
        Err(_error) if tree_target.exists() => {
            let _ = fs::remove_dir_all(&temp);
            append(
                state,
                &profile,
                &json!({"kind": "seen", "id": id, "at": now(), "source": source}),
            )?;
            let case = find(state, &profile, &id)?
                .unwrap_or_else(|| fallback_case(run, goal, &check, &tree, &id, source));
            return Ok(Recorded::Case(Box::new(case)));
        }
        Err(error) => {
            let _ = fs::remove_dir_all(&temp);
            return Err(format!(
                "could not pin the tree at {}: {error}",
                tree_target.display()
            ));
        }
    }

    let case = Case {
        id: id.clone(),
        goal: goal.to_string(),
        check,
        ruler: run.ruler.as_ref().map(|ruler| ruler.digest()),
        attack: attack.map(str::to_string),
        profile: profile.clone(),
        model: run.model.clone(),
        policy: run.policy.clone(),
        bounds: Bounds {
            attempts: run.bounds.attempts(),
            minutes: run.bounds.minutes(),
            tokens: run.bounds.tokens(),
            width: run.bounds.width(),
        },
        attempts_used: run.attempts(),
        cause: cause.to_string(),
        tree,
        snapshot: label.clone(),
        source: source.to_string(),
        ignored: IGNORED.iter().map(|name| name.to_string()).collect(),
        skipped_special: manifest.special,
        partial: manifest.special > 0,
        bytes: manifest.bytes,
        files: manifest.files.len() as u64,
        status: "open".to_string(),
        at: now(),
    };
    // The case line is written only after the rename, so a crash leaves no case pointing at a half-copied
    // tree.
    append(state, &profile, &case.to_value())?;
    Ok(Recorded::Case(Box::new(case)))
}

fn fallback_case(run: &Run, goal: &str, check: &str, tree: &str, id: &str, source: &str) -> Case {
    Case {
        id: id.to_string(),
        goal: goal.to_string(),
        check: check.to_string(),
        ruler: run.ruler.as_ref().map(|ruler| ruler.digest()),
        attack: None,
        profile: run.profile.clone(),
        model: run.model.clone(),
        policy: run.policy.clone(),
        bounds: Bounds {
            attempts: run.bounds.attempts(),
            minutes: run.bounds.minutes(),
            tokens: run.bounds.tokens(),
            width: run.bounds.width(),
        },
        attempts_used: run.attempts(),
        cause: "attempts".to_string(),
        tree: tree.to_string(),
        snapshot: run.snapshot.clone().unwrap_or_default(),
        source: source.to_string(),
        ignored: IGNORED.iter().map(|name| name.to_string()).collect(),
        skipped_special: 0,
        partial: false,
        bytes: 0,
        files: 0,
        status: "open".to_string(),
        at: now(),
    }
}

impl Case {
    pub(crate) fn to_value(&self) -> Value {
        json!({
            "kind": "case",
            "id": self.id,
            "at": self.at,
            "profile": self.profile,
            "goal": self.goal,
            "check": self.check,
            "ruler": self.ruler,
            "attack": self.attack,
            "model": self.model,
            "policy": self.policy,
            "bounds": {
                "attempts": self.bounds.attempts,
                "minutes": self.bounds.minutes,
                "budget_tokens": self.bounds.tokens,
                "width": self.bounds.width,
            },
            "attempts_used": self.attempts_used,
            "cause": self.cause,
            "tree": self.tree,
            "snapshot": self.snapshot,
            "source": self.source,
            "ignored": self.ignored,
            "skipped_special": self.skipped_special,
            "partial": self.partial,
            "bytes": self.bytes,
            "files": self.files,
            "status": self.status,
        })
    }

    pub(crate) fn from_value(value: &Value) -> Option<Case> {
        let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
        let bounds = value.get("bounds").cloned().unwrap_or(Value::Null);
        Some(Case {
            id: text("id")?,
            goal: text("goal").unwrap_or_default(),
            check: text("check").unwrap_or_default(),
            ruler: text("ruler"),
            attack: text("attack"),
            profile: text("profile").unwrap_or_default(),
            model: text("model").unwrap_or_default(),
            policy: text("policy").unwrap_or_else(|| "guarded".to_string()),
            bounds: Bounds {
                attempts: bounds.get("attempts").and_then(Value::as_u64).unwrap_or(3) as u32,
                minutes: bounds.get("minutes").and_then(Value::as_u64).unwrap_or(10),
                tokens: bounds.get("budget_tokens").and_then(Value::as_u64),
                width: bounds.get("width").and_then(Value::as_u64).unwrap_or(1) as u32,
            },
            attempts_used: value
                .get("attempts_used")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32,
            cause: text("cause").unwrap_or_default(),
            tree: text("tree").unwrap_or_default(),
            snapshot: text("snapshot").unwrap_or_default(),
            source: text("source").unwrap_or_default(),
            ignored: value
                .get("ignored")
                .and_then(Value::as_array)
                .map(|names| {
                    names
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            skipped_special: value
                .get("skipped_special")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            partial: value
                .get("partial")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            bytes: value.get("bytes").and_then(Value::as_u64).unwrap_or(0),
            files: value.get("files").and_then(Value::as_u64).unwrap_or(0),
            status: text("status").unwrap_or_else(|| "open".to_string()),
            at: value.get("at").and_then(Value::as_i64).unwrap_or(0),
        })
    }
}

/// Fold one profile's ledger. The latest `case` line per id wins; `seen` never changes the record; a `base`
/// result moves the status. A candidate result is kept in the ledger but cannot change what the case is.
pub fn load(state: &Path, profile: &str) -> Vec<Case> {
    let text = fs::read_to_string(ledger(state, profile)).unwrap_or_default();
    let mut cases: std::collections::BTreeMap<String, Case> = std::collections::BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut base: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for line in text.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match entry
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "case" => {
                if let Some(case) = Case::from_value(&entry) {
                    if !cases.contains_key(&case.id) {
                        order.push(case.id.clone());
                    }
                    cases.insert(case.id.clone(), case);
                }
            }
            "result" if entry.get("arm").and_then(Value::as_str) == Some("base") => {
                let id = entry.get("id").and_then(Value::as_str).unwrap_or_default();
                let outcome = entry
                    .get("result")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                base.insert(id.to_string(), outcome.to_string());
            }
            _ => {}
        }
    }
    order
        .into_iter()
        .filter_map(|id| {
            let mut case = cases.remove(&id)?;
            if case.status == "retired" {
                return Some(case);
            }
            if let Some(outcome) = base.get(&id) {
                case.status = match outcome.as_str() {
                    "pass" => "fixed",
                    "stale" => "stale",
                    "broken" => "broken",
                    "fail" | "blocked" => "open",
                    _ => case.status.as_str(),
                }
                .to_string();
            }
            Some(case)
        })
        .collect()
}

/// Every profile with a ledger, in name order.
pub fn profiles(state: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(cases_dir(state)) else {
        return Vec::new();
    };
    let mut found: Vec<String> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|kind| kind == "jsonl"))
        .filter_map(|path| {
            path.file_stem()
                .map(|name| name.to_string_lossy().to_string())
        })
        .collect();
    found.sort();
    found
}

pub fn find(state: &Path, profile: &str, id: &str) -> Result<Option<Case>, String> {
    if !valid_id(id) {
        return Err(format!("`{id}` is not a case id"));
    }
    Ok(load(state, profile).into_iter().find(|case| case.id == id))
}

/// Append one line. The directory is created 0700 and the file 0600 by the same helpers the state root
/// uses; the corpus is machine-local and may hold a tree's contents.
fn append(state: &Path, profile: &str, value: &Value) -> Result<(), String> {
    let path = ledger(state, profile);
    if let Some(parent) = path.parent() {
        crate::scratch::state_dir(parent)
            .map_err(|error| format!("could not make {}: {error}", parent.display()))?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|error| format!("could not open {}: {error}", path.display()))?;
    crate::scratch::private_file(&path)
        .map_err(|error| format!("could not protect {}: {error}", path.display()))?;
    writeln!(file, "{value}")
        .map_err(|error| format!("could not write {}: {error}", path.display()))
}

/// Append one eval result. Public because the eval module owns the report and this module owns the format.
pub fn append_result(state: &Path, profile: &str, result: &ResultLine) -> Result<(), String> {
    if !valid_id(&result.id) {
        return Err(format!("`{}` is not a case id", result.id));
    }
    append(
        state,
        profile,
        &json!({
            "kind": "result",
            "id": result.id,
            "at": result.at,
            "arm": result.arm,
            "result": result.outcome,
            "attack": result.attack,
            "attempts": result.attempts,
            "spent": result.spent,
            "seconds": result.seconds_milli as f64 / 1000.0,
            "session": result.session,
            "state": result.state,
            "harness": result.harness,
            "model": result.model,
        }),
    )
}

/// A verified copy of a case tree, made beside its target and not yet visible as the installed case.
///
/// Staging exists so a replacement can be validated and copied *before* the installed case is moved out of
/// the way. Copying after the old tree has been renamed to a backup is the shape that loses the case: a
/// failed copy or a failed ledger append leaves neither the old case nor a new one.
pub(crate) struct Staged {
    dir: PathBuf,
}

impl Staged {
    /// Throw the copy away. Called on every path that does not end in a commit.
    pub(crate) fn discard(self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Verify the recorded tree digest and copy the tree beside where it will land.
pub(crate) fn stage(state: &Path, case: &Case, source_tree: &Path) -> Result<Staged, String> {
    if !valid_id(&case.id) {
        return Err(format!("`{}` is not a case id", case.id));
    }
    let digest = tree_digest(source_tree)?;
    if digest != case.tree {
        return Err(format!(
            "case `{}` tree changed: recorded {}, found {digest}",
            case.id, case.tree
        ));
    }
    let base = profile_dir(state, &case.profile);
    crate::scratch::state_dir(&base)
        .map_err(|error| format!("could not make {}: {error}", base.display()))?;
    let dir = base.join(format!(".staged-{}-{}", case.id, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    copy_dir(source_tree, &dir.join("tree"))?;
    Ok(Staged { dir })
}

/// Move the staged tree into place and append the line that makes it active. A failed append removes the
/// tree again, so an installed directory never exists without a line naming it.
pub(crate) fn commit(
    state: &Path,
    case: &Case,
    staged: Staged,
    source: &str,
) -> Result<PathBuf, String> {
    let dir = profile_dir(state, &case.profile).join(&case.id);
    if dir.exists() {
        staged.discard();
        return Err(format!("a case called `{}` is already installed", case.id));
    }
    if let Err(error) = fs::rename(&staged.dir, &dir) {
        staged.discard();
        return Err(format!("could not install {}: {error}", dir.display()));
    }
    let mut installed = case.clone();
    installed.source = source.to_string();
    if let Err(error) = append(state, &case.profile, &installed.to_value()) {
        let _ = fs::remove_dir_all(&dir);
        return Err(error);
    }
    Ok(dir)
}

/// Retire a case: append a case line with `status: "retired"` and remove its tree. The evidence of the
/// case line stays; the tree is what the person asked to reclaim.
pub fn forget(state: &Path, profile: &str, id: &str) -> Result<Case, String> {
    if !valid_id(id) {
        return Err(format!("`{id}` is not a case id"));
    }
    let base = profile_dir(state, profile);
    let case = find(state, profile, id)?.ok_or_else(|| format!("there is no case `{id}`"))?;
    let dir = base.join(id);
    let resolved = fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
    let allowed = fs::canonicalize(&base).unwrap_or(base.clone());
    if !resolved.starts_with(&allowed) || resolved == allowed {
        return Err(format!(
            "the case directory {} is not under {}",
            resolved.display(),
            allowed.display()
        ));
    }
    let mut retired = case.clone();
    retired.status = "retired".to_string();
    append(state, profile, &retired.to_value())?;
    if dir.exists() {
        fs::remove_dir_all(&dir)
            .map_err(|error| format!("could not remove {}: {error}", dir.display()))?;
    }
    Ok(retired)
}

/// The tree a case pins, after checking it is still the tree the case recorded. A mismatch is `broken`: a
/// corpus that silently replays a changed tree is not evidence.
pub fn pinned_tree(state: &Path, case: &Case) -> Result<PathBuf, String> {
    let base = profile_dir(state, &case.profile);
    let tree = base.join(&case.id).join("tree");
    if !tree.is_dir() {
        return Err(format!("the tree for case {} is missing", case.id));
    }
    let digest = tree_digest(&tree)?;
    if digest != case.tree {
        return Err(format!(
            "the tree for case {} changed: recorded {}, found {digest}",
            case.id, case.tree
        ));
    }
    Ok(tree)
}

/// Copy a directory tree. Used for the pinned tree and for eval's throwaway copy. Modes are carried,
/// because an executable restored as 0644 is a broken replay.
pub fn copy_dir(from: &Path, to: &Path) -> Result<(), String> {
    crate::scratch::state_dir(to)
        .map_err(|error| format!("could not make {}: {error}", to.display()))?;
    let listing = fs::read_dir(from)
        .map_err(|error| format!("could not read {}: {error}", from.display()))?;
    let mut entries: Vec<PathBuf> = listing.flatten().map(|entry| entry.path()).collect();
    entries.sort();
    for path in entries {
        let relative = path
            .strip_prefix(from)
            .map_err(|_| format!("{} is not under {}", path.display(), from.display()))?;
        let target = to.join(relative);
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("could not stat {}: {error}", path.display()))?;
        if metadata.is_dir() {
            fs::create_dir_all(&target)
                .map_err(|error| format!("could not make {}: {error}", target.display()))?;
            copy_dir(&path, &target)?;
        } else if metadata.is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| format!("could not make {}: {error}", parent.display()))?;
            }
            fs::copy(&path, &target)
                .map_err(|error| format!("could not copy {}: {error}", path.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = metadata.permissions().mode() & 0o7777;
                let _ = fs::set_permissions(&target, fs::Permissions::from_mode(mode));
            }
        }
        // A special path was refused at record time unless forced; a forced partial case does not carry it
        // and says so through `partial`.
    }
    Ok(())
}

/// The bytes one profile's trees occupy, and how many cases are open. Statted on request, never on every
/// listing.
pub fn size(state: &Path, profile: &str) -> (u64, u64) {
    fn total(at: &Path) -> u64 {
        let mut bytes = 0;
        let Ok(listing) = fs::read_dir(at) else {
            return 0;
        };
        for entry in listing.flatten() {
            let path = entry.path();
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.is_dir() {
                bytes += total(&path);
            } else {
                bytes += metadata.len();
            }
        }
        bytes
    }
    let cases = load(state, profile);
    let bytes = cases
        .iter()
        .filter(|case| case.status != "retired")
        .map(|case| total(&profile_dir(state, &case.profile).join(&case.id)))
        .sum();
    (
        bytes,
        cases.iter().filter(|case| case.status == "open").count() as u64,
    )
}

/// A profile's corpus as text, one case per line. The size is only shown when asked, because statting every
/// tree on every listing is the cost this line exists to make visible rather than pay.
pub fn table(state: &Path, profile: &str, sized: bool) -> String {
    let cases = load(state, profile);
    if cases.is_empty() {
        return format!("{profile}: no cases\n");
    }
    let mut text = format!("{profile}: {} case(s)\n", cases.len());
    for case in cases {
        let size = if sized && case.status != "retired" {
            let (bytes, _) = size_of_case(state, &case);
            format!("  {} bytes", bytes)
        } else {
            String::new()
        };
        text.push_str(&format!(
            "  {:<24} {:<8} {:<8} {}{}\n    {} (check: {})\n",
            case.id,
            case.status,
            case.cause,
            case.bounds.line(),
            size,
            clip(&case.goal, 90),
            clip(&case.check, 60)
        ));
    }
    text
}

fn size_of_case(state: &Path, case: &Case) -> (u64, u64) {
    fn total(at: &Path) -> u64 {
        let mut bytes = 0;
        let Ok(listing) = fs::read_dir(at) else {
            return 0;
        };
        for entry in listing.flatten() {
            let path = entry.path();
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.is_dir() {
                bytes += total(&path);
            } else {
                bytes += metadata.len();
            }
        }
        bytes
    }
    (
        total(&profile_dir(state, &case.profile).join(&case.id)),
        case.files,
    )
}

fn clip(text: &str, cap: usize) -> String {
    let cut = text
        .char_indices()
        .map(|(at, _)| at)
        .take_while(|at| *at <= cap)
        .last()
        .unwrap_or(0);
    if cut < text.len() {
        format!("{}…", &text[..cut])
    } else {
        text.to_string()
    }
}

/// Whether a path is safely inside a profile's case directory. Used before any removal.
pub fn inside_profile(state: &Path, profile: &str, path: &Path) -> bool {
    let base = profile_dir(state, profile);
    let base = fs::canonicalize(&base).unwrap_or(base);
    let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    path.starts_with(&base) && path != base
}

/// The default check used by the record path when a surface has no profile check. Empty means the case is
/// not reproducible and the record path says so.
pub fn check_of(profile: &crate::profile::Profile) -> String {
    profile.verify.trim().to_string()
}

/// A `Subject` for the pinned tree, so a copy cannot recurse into the store.
pub fn subject(label: &str, root: &Path) -> Subject {
    Subject::new(label, root)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        crate::scratch::unique(name)
    }

    fn write_tree(root: &Path) {
        fs::create_dir_all(root.join("src")).expect("a tree");
        fs::write(root.join("src/lib.rs"), "fn one() {}\n").expect("a file");
        fs::write(root.join("README.md"), "readme\n").expect("a file");
    }

    #[test]
    fn a_tree_digest_changes_when_a_file_changes() {
        let root = scratch("digest");
        write_tree(&root);
        let first = tree_digest(&root).expect("a digest");
        fs::write(root.join("README.md"), "changed\n").expect("a write");
        let second = tree_digest(&root).expect("a digest");
        assert_ne!(first, second);
    }

    #[test]
    fn a_case_id_is_a_name_the_remove_path_accepts() {
        let id = case_id("fix", "a goal", "cargo test", "abc");
        assert!(valid_id(&id), "{id}");
        assert!(!valid_id("../etc"));
        assert!(!valid_id("fix-xyz"));
        assert!(!valid_id("fix"));
    }

    #[test]
    fn a_forged_id_with_a_parent_segment_is_refused() {
        let state = scratch("forged");
        assert!(forget(&state, "fix", "../../etc").is_err());
        assert!(find(&state, "fix", "../../etc").is_err());
    }

    #[test]
    fn a_copy_carries_the_tree_and_its_modes() {
        let from = scratch("copy-from");
        let to = scratch("copy-to");
        write_tree(&from);
        copy_dir(&from, &to).expect("a copy");
        assert_eq!(
            tree_digest(&from).expect("from"),
            tree_digest(&to).expect("to")
        );
    }
}
