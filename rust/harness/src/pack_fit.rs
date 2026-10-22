//! Fitting a pack into a home: decisions, install, uninstall, adapt and merge.
//!
//! `pack` owns the archive and the plan classes. This module owns the live effects: it reads a reviewed
//! decision file, refuses everything it cannot prove before writing, and records each installed item in a
//! catalog that `uninstall` can read back. A pack is data; a catalog row is the claim that this home put that
//! data somewhere.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::cases::{self, Case};
use crate::pack::{self, Kind, Pack, Provided, Row};
use crate::{profile, scratch, skills, tuning};

const MAX_PROMPT: usize = 96 * 1024;
const MAX_TEXT: usize = 32 * 1024;
const RESERVED_SKILLS: [&str; 3] = ["builtin", "proposed", "verify"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Install,
    Skip,
    Replace,
}

impl Action {
    fn parse(text: &str) -> Result<Action, String> {
        match text {
            "install" => Ok(Action::Install),
            "skip" => Ok(Action::Skip),
            "replace" => Ok(Action::Replace),
            other => Err(format!(
                "`{other}` is not an action; say install, skip or replace"
            )),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Action::Install => "install",
            Action::Skip => "skip",
            Action::Replace => "replace",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub kind: Kind,
    pub identity: String,
    pub action: Action,
}

impl Decision {
    pub fn to_value(&self) -> Value {
        json!({
            "kind": self.kind.name(),
            "identity": self.identity,
            "action": self.action.name(),
        })
    }
}

/// One row in `<state>/packs/catalog.jsonl`. `live` is relative to the state for skills and cases, and to
/// the home for harnesses, because that is where the loader reads them. `content` is always under the state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub kind: Kind,
    pub identity: String,
    pub pack: String,
    pub version: String,
    pub pack_digest: String,
    pub digest: String,
    pub live_digest: String,
    pub live: String,
    pub content: String,
    pub generation: Option<u64>,
    pub at: u64,
}

impl Installed {
    pub fn to_value(&self) -> Value {
        json!({
            "kind": self.kind.name(),
            "identity": self.identity,
            "pack": self.pack,
            "version": self.version,
            "pack_digest": self.pack_digest,
            "digest": self.digest,
            "live_digest": self.live_digest,
            "live": self.live,
            "content": self.content,
            "generation": self.generation,
            "at": self.at,
        })
    }

    fn from_value(value: &Value) -> Result<Installed, String> {
        let object = value
            .as_object()
            .ok_or_else(|| "a catalog row is not an object".to_string())?;
        let allowed = [
            "kind",
            "identity",
            "pack",
            "version",
            "pack_digest",
            "digest",
            "live_digest",
            "live",
            "content",
            "generation",
            "at",
        ];
        for key in object.keys() {
            if !allowed.contains(&key.as_str()) {
                return Err(format!("catalog row has an unknown key `{key}`"));
            }
        }
        let string = |key: &str| -> Result<String, String> {
            object
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| format!("catalog row `{key}` must be a string"))
        };
        let kind = object
            .get("kind")
            .and_then(Value::as_str)
            .and_then(kind_from_name)
            .ok_or_else(|| "catalog row has no valid kind".to_string())?;
        let generation =
            match object.get("generation") {
                None | Some(Value::Null) => None,
                Some(value) => Some(value.as_u64().ok_or_else(|| {
                    "catalog row `generation` must be a number or null".to_string()
                })?),
            };
        let at = object
            .get("at")
            .and_then(Value::as_u64)
            .ok_or_else(|| "catalog row `at` must be a number".to_string())?;
        Ok(Installed {
            kind,
            identity: string("identity")?,
            pack: string("pack")?,
            version: string("version")?,
            pack_digest: string("pack_digest")?,
            digest: string("digest")?,
            live_digest: string("live_digest")?,
            live: string("live")?,
            content: string("content")?,
            generation,
            at,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyReceipt {
    pub items: Vec<Installed>,
    pub skipped: usize,
}

impl ApplyReceipt {
    pub fn text(&self) -> String {
        let mut text = format!(
            "applied: {} item(s), {} skipped\n",
            self.items.len(),
            self.skipped
        );
        for item in &self.items {
            text.push_str(&format!(
                "  {} {} from {} {}\n",
                item.kind.name(),
                item.identity,
                item.pack,
                item.version
            ));
            if let Some(generation) = item.generation {
                text.push_str(&format!("    generation: {generation}\n"));
            }
        }
        text
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UninstallReceipt {
    pub removed: Vec<String>,
    pub kept: Vec<String>,
    pub promoted: Vec<String>,
    pub content_removed: bool,
}

impl UninstallReceipt {
    pub fn text(&self) -> String {
        let mut text = format!("removed: {}\n", self.removed.len());
        for path in &self.removed {
            text.push_str(&format!("  {path}\n"));
        }
        if !self.kept.is_empty() {
            text.push_str("kept (edited, shared, or changed):\n");
            for path in &self.kept {
                text.push_str(&format!("  {path}\n"));
            }
        }
        if !self.promoted.is_empty() {
            text.push_str("kept (promoted to an active skill):\n");
            for path in &self.promoted {
                text.push_str(&format!("  {path}\n"));
            }
        }
        if self.content_removed {
            text.push_str("content: removed\n");
        }
        text
    }
}

pub fn catalog_path(state: &Path) -> PathBuf {
    state.join("packs").join("catalog.jsonl")
}

pub fn events_path(state: &Path) -> PathBuf {
    state.join("packs").join("events.jsonl")
}

pub fn content_root(state: &Path, id: &str, version: &str) -> Result<PathBuf, String> {
    pack::valid_id(id)?;
    pack::valid_version(version)?;
    Ok(state.join("packs").join(id).join(version))
}

fn kind_from_name(name: &str) -> Option<Kind> {
    match name {
        "skill" => Some(Kind::Skill),
        "harness" => Some(Kind::Harness),
        "case" => Some(Kind::Case),
        _ => None,
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

/// A name that can be one path segment. This is deliberately stricter than a pack id: `a/b` is a valid pack
/// id but not a valid skill directory.
pub fn safe_component(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "._-".contains(character))
}

fn valid_skill_name(name: &str) -> Result<(), String> {
    if !safe_component(name) {
        return Err(format!(
            "`{name}` is not a skill name; use letters, digits, `.`, `_` and `-` in one segment"
        ));
    }
    if RESERVED_SKILLS.contains(&name) {
        return Err(format!("`{name}` is reserved by the skills directory"));
    }
    Ok(())
}

fn valid_profile_name(name: &str) -> Result<(), String> {
    if !safe_component(name) {
        return Err(format!("`{name}` is not a profile name"));
    }
    Ok(())
}

fn case_profile(id: &str) -> Result<String, String> {
    if !cases::valid_id(id) {
        return Err(format!("`{id}` is not a case id"));
    }
    let (head, _) = id
        .rsplit_once('-')
        .ok_or_else(|| format!("`{id}` is not a case id"))?;
    Ok(head.to_string())
}

fn relative(path: &str) -> Result<Vec<&str>, String> {
    if path.is_empty() || path.starts_with('/') || path.contains('\\') {
        return Err(format!("`{path}` is not a relative path"));
    }
    let parts: Vec<&str> = path.split('/').collect();
    if parts
        .iter()
        .any(|part| part.is_empty() || *part == "." || *part == "..")
    {
        return Err(format!("`{path}` has an empty or dot segment"));
    }
    Ok(parts)
}

/// Reject a symlink anywhere under `base` on the way to `relative_path`. Missing components are allowed:
/// a path that does not exist cannot be followed.
fn no_symlink(base: &Path, path: &str) -> Result<(), String> {
    let parts = relative(path)?;
    let mut at = base.to_path_buf();
    for part in parts {
        at.push(part);
        match fs::symlink_metadata(&at) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(format!(
                    "{} is a symlink; a pack uninstall will not follow it",
                    at.display()
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => {
                return Err(format!("could not stat {}: {error}", at.display()));
            }
        }
    }
    Ok(())
}

fn path_in(base: &Path, relative_path: &str) -> Result<PathBuf, String> {
    relative(relative_path)?;
    no_symlink(base, relative_path)?;
    Ok(base.join(relative_path))
}

fn validate_row(row: &Installed) -> Result<(), String> {
    pack::valid_id(&row.pack)?;
    pack::valid_version(&row.version)?;
    if row.pack_digest.len() != 64 || !row.pack_digest.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "catalog row for `{}` has no pack digest",
            row.identity
        ));
    }
    if row.digest.len() != 64 || !row.digest.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "catalog row for `{}` has no item digest",
            row.identity
        ));
    }
    if row.live_digest.len() != 64 || !row.live_digest.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "catalog row for `{}` has no live digest",
            row.identity
        ));
    }
    let content = format!("packs/{}/{}", row.pack, row.version);
    if row.content != content {
        return Err(format!(
            "catalog row for `{}` names content `{}`, not `{content}`",
            row.identity, row.content
        ));
    }
    match row.kind {
        Kind::Skill => {
            valid_skill_name(&row.identity)?;
            let active = format!("skills/{}", row.identity);
            let proposed = format!("skills/proposed/{}", row.identity);
            if row.live != active && row.live != proposed {
                return Err(format!(
                    "catalog skill `{}` names `{}`, not its active or proposed path",
                    row.identity, row.live
                ));
            }
        }
        Kind::Harness => {
            valid_profile_name(&row.identity)?;
            if row.live != format!("harness/{}", row.identity) {
                return Err(format!(
                    "catalog harness `{}` names `{}`",
                    row.identity, row.live
                ));
            }
        }
        Kind::Case => {
            let profile = case_profile(&row.identity)?;
            if row.live != format!("cases/{profile}/{}", row.identity) {
                return Err(format!(
                    "catalog case `{}` names `{}`",
                    row.identity, row.live
                ));
            }
        }
    }
    Ok(())
}

pub fn read_catalog(state: &Path) -> Result<Vec<Installed>, String> {
    let path = catalog_path(state);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("could not read {}: {error}", path.display())),
    };
    let mut rows = Vec::new();
    for (number, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line).map_err(|error| {
            format!(
                "{} line {} is not JSON: {error}",
                path.display(),
                number + 1
            )
        })?;
        let row = Installed::from_value(&value)?;
        validate_row(&row)
            .map_err(|error| format!("{} line {}: {error}", path.display(), number + 1))?;
        rows.push(row);
    }
    Ok(rows)
}

/// Distinguishes two catalog writes in the same process without relying on the clock.
static CATALOG: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn write_catalog(state: &Path, rows: &[Installed]) -> Result<(), String> {
    let path = catalog_path(state);
    if let Some(parent) = path.parent() {
        scratch::state_dir(parent)
            .map_err(|error| format!("could not make {}: {error}", parent.display()))?;
    }
    // A name of its own per write. `catalog-<pid>.tmp` collided between two installs in one process, which is
    // exactly the case a concurrent install creates: one thread renamed the other thread's file away.
    let temporary = path.with_file_name(format!(
        ".catalog-{}-{}-{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0),
        CATALOG.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .map_err(|error| format!("could not write {}: {error}", temporary.display()))?;
    for row in rows {
        writeln!(file, "{}", row.to_value())
            .map_err(|error| format!("could not write {}: {error}", temporary.display()))?;
    }
    file.sync_all()
        .map_err(|error| format!("could not sync {}: {error}", temporary.display()))?;
    scratch::private_file(&temporary)
        .map_err(|error| format!("could not protect {}: {error}", temporary.display()))?;
    fs::rename(&temporary, &path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        format!("could not replace {}: {error}", path.display())
    })
}

fn append_event(state: &Path, event: Value) -> Result<(), String> {
    let path = events_path(state);
    if let Some(parent) = path.parent() {
        scratch::state_dir(parent)
            .map_err(|error| format!("could not make {}: {error}", parent.display()))?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|error| format!("could not open {}: {error}", path.display()))?;
    scratch::private_file(&path)
        .map_err(|error| format!("could not protect {}: {error}", path.display()))?;
    writeln!(file, "{event}")
        .map_err(|error| format!("could not write {}: {error}", path.display()))
}

pub fn parse_decisions(text: &str) -> Result<Vec<Decision>, String> {
    let value: Value = serde_json::from_str(text)
        .map_err(|error| format!("the decision file is not JSON: {error}"))?;
    let object = value
        .as_object()
        .ok_or_else(|| "the decision file must be one JSON object".to_string())?;
    for key in object.keys() {
        if key != "items" {
            return Err(format!("the decision file has an unknown key `{key}`"));
        }
    }
    let items = object
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| "the decision file needs an `items` array".to_string())?;
    if items.is_empty() {
        return Err("the decision file has no items".to_string());
    }
    let mut seen: Vec<(Kind, String)> = Vec::new();
    let mut decisions = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let map = item
            .as_object()
            .ok_or_else(|| format!("decision {} is not an object", index + 1))?;
        for key in map.keys() {
            if !["kind", "identity", "action"].contains(&key.as_str()) {
                return Err(format!("decision {} has an unknown key `{key}`", index + 1));
            }
        }
        let kind = map
            .get("kind")
            .and_then(Value::as_str)
            .and_then(kind_from_name)
            .ok_or_else(|| format!("decision {} has no valid kind", index + 1))?;
        let identity = map
            .get("identity")
            .and_then(Value::as_str)
            .filter(|identity| !identity.is_empty())
            .ok_or_else(|| format!("decision {} has no identity", index + 1))?
            .to_string();
        let action = Action::parse(
            map.get("action")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("decision {} has no action", index + 1))?,
        )?;
        if seen.contains(&(kind, identity.clone())) {
            return Err(format!(
                "decision {} repeats {} `{identity}`",
                index + 1,
                kind.name()
            ));
        }
        seen.push((kind, identity.clone()));
        decisions.push(Decision {
            kind,
            identity,
            action,
        });
    }
    Ok(decisions)
}

struct Prepared {
    decision: Decision,
    row: Row,
    source: PathBuf,
    target: PathBuf,
    existing: Option<PathBuf>,
    backup: Option<PathBuf>,
}

fn provided_for(pack: &Pack, kind: Kind, identity: &str) -> Result<Provided, String> {
    pack.manifest
        .provides
        .all()
        .into_iter()
        .find(|item| item.kind == kind && item.identity == identity)
        .cloned()
        .ok_or_else(|| format!("the pack provides no {} `{identity}`", kind.name()))
}

fn backup_path(state: &Path, pack: &str, kind: Kind, identity: &str) -> Result<PathBuf, String> {
    pack::valid_id(pack)?;
    let root = state
        .join("packs")
        .join("replaced")
        .join(pack)
        .join(now().to_string());
    let base = root.join(format!("{}-{}", kind.name(), identity));
    scratch::state_dir(&root)
        .map_err(|error| format!("could not make {}: {error}", root.display()))?;
    if !base.exists() {
        return Ok(base);
    }
    for number in 1..1000 {
        let candidate = root.join(format!("{}-{}-{number}", kind.name(), identity));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(format!(
        "could not find a free backup name under {}",
        root.display()
    ))
}

fn preflight(
    pack: &Pack,
    home: &Path,
    state: &Path,
    activate: bool,
    decisions: &[Decision],
    rows: &[Row],
) -> Result<Vec<Prepared>, String> {
    let mut prepared = Vec::new();
    let mut accepted = 0usize;
    for decision in decisions {
        if decision.action == Action::Skip {
            continue;
        }
        let row = rows
            .iter()
            .find(|row| row.kind == decision.kind && row.identity == decision.identity)
            .ok_or_else(|| {
                format!(
                    "the pack has no {} `{}`",
                    decision.kind.name(),
                    decision.identity
                )
            })?;
        if row.class == "incompatible" {
            return Err(format!(
                "{} `{}` is incompatible: {}",
                decision.kind.name(),
                decision.identity,
                row.reason
            ));
        }
        match decision.action {
            Action::Install => {
                if !matches!(row.class.as_str(), "new" | "redundant") {
                    return Err(format!(
                        "{} `{}` is `{}`; install is only for new or redundant items",
                        decision.kind.name(),
                        decision.identity,
                        row.class
                    ));
                }
            }
            Action::Replace => {
                if row.class != "conflict" {
                    return Err(format!(
                        "{} `{}` is `{}`; replace is only for a conflict",
                        decision.kind.name(),
                        decision.identity,
                        row.class
                    ));
                }
            }
            Action::Skip => unreachable!(),
        }
        let provided = provided_for(pack, decision.kind, &decision.identity)?;
        let source = pack.root().join(&provided.path);
        let (target, existing, backup) = match decision.kind {
            Kind::Skill => {
                valid_skill_name(&decision.identity)?;
                let proposed = state
                    .join("skills")
                    .join("proposed")
                    .join(&decision.identity);
                let active = state.join("skills").join(&decision.identity);
                if proposed.exists() && active.exists() {
                    return Err(format!(
                        "skill `{}` exists as both a proposal and an active skill",
                        decision.identity
                    ));
                }
                let target = if activate || (active.exists() && !proposed.exists()) {
                    active.clone()
                } else {
                    proposed.clone()
                };
                let existing = if proposed.exists() {
                    Some(proposed)
                } else if active.exists() {
                    Some(active)
                } else {
                    None
                };
                match (decision.action, existing.clone()) {
                    (Action::Install, Some(_)) => {
                        return Err(format!(
                            "a skill called `{}` already exists; use replace or skip",
                            decision.identity
                        ));
                    }
                    (Action::Replace, None) => {
                        return Err(format!(
                            "replace for skill `{}` needs an existing path",
                            decision.identity
                        ));
                    }
                    (Action::Replace, Some(path)) => {
                        let backup =
                            backup_path(state, &pack.manifest.id, Kind::Skill, &decision.identity)?;
                        (target, Some(path), Some(backup))
                    }
                    (Action::Install, None) => (target, None, None),
                    (Action::Skip, _) => unreachable!(),
                }
            }
            Kind::Harness => {
                valid_profile_name(&decision.identity)?;
                let target = home.join("harness").join(&decision.identity);
                if decision.action == Action::Install && target.exists() {
                    return Err(format!(
                        "harness `{}` already exists; use replace or skip",
                        decision.identity
                    ));
                }
                if decision.action == Action::Replace && !target.exists() {
                    return Err(format!(
                        "replace for harness `{}` needs an existing path",
                        decision.identity
                    ));
                }
                (target, None, None)
            }
            Kind::Case => {
                let case = read_case(&source)?;
                if decision.identity != case.id {
                    return Err(format!(
                        "case identity `{}` does not match case.json `{}`",
                        decision.identity, case.id
                    ));
                }
                let expected = cases::case_id(&case.profile, &case.goal, &case.check, &case.tree);
                if case.id != expected {
                    return Err(format!(
                        "case `{}` is not the id of its profile, goal, check and tree (expected {expected})",
                        case.id
                    ));
                }
                let target = cases::profile_dir(state, &case.profile).join(&case.id);
                let existing = if target.exists() {
                    Some(target.clone())
                } else {
                    None
                };
                match (decision.action, existing.is_some()) {
                    (Action::Install, true) => {
                        return Err(format!(
                            "a case called `{}` is already installed; use replace or skip",
                            case.id
                        ));
                    }
                    (Action::Replace, false) => {
                        return Err(format!(
                            "replace for case `{}` needs an existing path",
                            case.id
                        ));
                    }
                    _ => {}
                }
                let backup = if decision.action == Action::Replace {
                    Some(backup_path(state, &pack.manifest.id, Kind::Case, &case.id)?)
                } else {
                    None
                };
                (target, existing, backup)
            }
        };
        if let Some(backup) = &backup
            && backup.exists()
        {
            return Err(format!("backup {} already exists", backup.display()));
        }
        accepted += 1;
        prepared.push(Prepared {
            decision: decision.clone(),
            row: row.clone(),
            source,
            target,
            existing,
            backup,
        });
    }
    if accepted == 0 {
        return Err("every decision is a skip; there is nothing to apply".to_string());
    }
    Ok(prepared)
}

fn read_case(source: &Path) -> Result<Case, String> {
    let text = fs::read_to_string(source.join("case.json"))
        .map_err(|error| format!("could not read case.json: {error}"))?;
    let value: Value =
        serde_json::from_str(&text).map_err(|error| format!("case.json is not JSON: {error}"))?;
    Case::from_value(&value).ok_or_else(|| "case.json has no readable fields".to_string())
}

fn digest_live(path: &Path) -> Result<String, String> {
    if path.is_dir() {
        pack::digest_tree(path)
    } else if path.is_file() {
        let bytes = fs::read(path)
            .map_err(|error| format!("could not read {}: {error}", path.display()))?;
        Ok(mneme::sha256::hex(&bytes))
    } else {
        Err(format!("{} is not a file or directory", path.display()))
    }
}

fn remove_live(path: &Path) -> Result<(), String> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("could not stat {}: {error}", path.display())),
    };
    if meta.file_type().is_symlink() {
        return Err(format!(
            "{} is a symlink; refusing to remove it",
            path.display()
        ));
    }
    if meta.is_dir() {
        fs::remove_dir_all(path)
            .map_err(|error| format!("could not remove {}: {error}", path.display()))
    } else {
        fs::remove_file(path)
            .map_err(|error| format!("could not remove {}: {error}", path.display()))
    }
}

fn temp_beside(target: &Path, label: &str) -> Result<PathBuf, String> {
    let parent = target
        .parent()
        .ok_or_else(|| format!("{} has no parent", target.display()))?;
    scratch::state_dir(parent)
        .map_err(|error| format!("could not make {}: {error}", parent.display()))?;
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| label.to_string());
    let temp = parent.join(format!(".{name}.{label}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp);
    Ok(temp)
}

fn install_skill(prepared: &Prepared, activate: bool) -> Result<Installed, String> {
    let temp = temp_beside(&prepared.target, "install")?;
    if let (Some(existing), Some(backup)) = (&prepared.existing, &prepared.backup)
        && let Err(error) = fs::rename(existing, backup)
    {
        return Err(format!("could not back up {}: {error}", existing.display()));
    }
    let result = (|| {
        pack::copy_tree(&prepared.source, &temp)?;
        let skill = temp.join("SKILL.md");
        if !skill.is_file() {
            return Err(format!(
                "skill `{}` has no SKILL.md",
                prepared.decision.identity
            ));
        }
        let text = fs::read_to_string(&skill)
            .map_err(|error| format!("could not read {}: {error}", skill.display()))?;
        let text = skills::set_status(
            &skills::ensure_defaults(&text),
            if activate { "active" } else { "proposed" },
        );
        fs::write(&skill, text)
            .map_err(|error| format!("could not write {}: {error}", skill.display()))?;
        fs::rename(&temp, &prepared.target)
            .map_err(|error| format!("could not install {}: {error}", prepared.target.display()))?;
        digest_live(&prepared.target)
    })();
    let _ = fs::remove_dir_all(&temp);
    let live_digest = match result {
        Ok(digest) => digest,
        Err(error) => {
            let _ = remove_live(&prepared.target);
            if let (Some(backup), Some(existing)) = (&prepared.backup, &prepared.existing) {
                let _ = fs::rename(backup, existing);
            }
            return Err(error);
        }
    };
    Ok(Installed {
        kind: Kind::Skill,
        identity: prepared.decision.identity.clone(),
        pack: String::new(),
        version: String::new(),
        pack_digest: String::new(),
        digest: prepared.row.digest.clone(),
        live_digest,
        live: String::new(),
        content: String::new(),
        generation: None,
        at: now(),
    })
}

fn install_harness(
    prepared: &Prepared,
    pack: &Pack,
    home: &Path,
    state: &Path,
) -> Result<Installed, String> {
    let source = prepared.source.clone();
    if !source.join("system.md").is_file() {
        return Err(format!(
            "harness `{}` has no system.md",
            prepared.decision.identity
        ));
    }
    let source_text = fs::read_to_string(source.join("system.md"))
        .map_err(|error| format!("could not read harness: {error}"))?;
    if source_text.trim().is_empty() {
        return Err(format!(
            "harness `{}` has an empty system.md",
            prepared.decision.identity
        ));
    }
    let generation = tuning::record_adoption(
        home,
        &prepared.decision.identity,
        &prepared.target,
        state,
        &source,
        &format!("pack:{}@{}", pack.manifest.id, pack.manifest.version),
        "",
        &tuning::guard_digest(),
    )?;
    let live_digest = digest_live(&prepared.target)?;
    Ok(Installed {
        kind: Kind::Harness,
        identity: prepared.decision.identity.clone(),
        pack: pack.manifest.id.clone(),
        version: pack.manifest.version.clone(),
        pack_digest: pack.digest.clone(),
        digest: prepared.row.digest.clone(),
        live_digest,
        live: String::new(),
        content: String::new(),
        generation: generation.map(|generation| generation.id),
        at: now(),
    })
}

fn install_case(prepared: &Prepared, state: &Path, pack: &Pack) -> Result<Installed, String> {
    let case = read_case(&prepared.source)?;
    let tree = prepared.source.join("tree");
    let source = format!("pack:{}@{}", pack.manifest.id, pack.manifest.version);

    // 1 and 2: verify the new tree and copy it beside the target before the installed case is touched.
    let staged = cases::stage(state, &case, &tree)?;
    let existing = match &prepared.backup {
        Some(_) => Some(
            prepared
                .existing
                .as_ref()
                .ok_or_else(|| "replace has no existing case path".to_string())?
                .clone(),
        ),
        None => None,
    };

    // 3: move the old case aside. The ledger still names it, so a failure below can put it back whole.
    let mut restored = None;
    if let (Some(backup), Some(existing)) = (&prepared.backup, &existing)
        && let Err(error) = fs::rename(existing, backup)
    {
        staged.discard();
        return Err(format!("could not back up {}: {error}", existing.display()));
    }
    if let (Some(backup), Some(existing)) = (&prepared.backup, &existing) {
        restored = Some((backup.clone(), existing.clone()));
    }

    // 4 and 5: move the new tree into place and append the line that makes it active.
    let dir = match cases::commit(state, &case, staged, &source) {
        Ok(dir) => dir,
        Err(error) => {
            // 6: the new tree is gone, so the old one goes back exactly where it was and the old ledger
            // line — never retired before the new line existed — still names it.
            if let Some((backup, existing)) = &restored {
                let _ = fs::rename(backup, existing);
                // `backup_path` makes the stamp directory before it knows the move will happen, so an
                // empty one is left behind unless it is removed here.
                if let Some(parent) = backup.parent() {
                    let _ = fs::remove_dir(parent);
                }
            }
            return Err(error);
        }
    };

    // 7: the old line is superseded rather than retired. The ledger is append-only and the last `case`
    // line for an id wins, so appending `retired` here would retire the case that just landed; the old
    // line stays as history and only its backup tree is reclaimed.
    if let Some((backup, _)) = &restored {
        let _ = fs::remove_dir_all(backup);
        if let Some(parent) = backup.parent() {
            let _ = fs::remove_dir(parent);
        }
    }

    let live_digest = digest_live(&dir)?;
    Ok(Installed {
        kind: Kind::Case,
        identity: case.id.clone(),
        pack: pack.manifest.id.clone(),
        version: pack.manifest.version.clone(),
        pack_digest: pack.digest.clone(),
        digest: prepared.row.digest.clone(),
        live_digest,
        live: format!("cases/{}/{}", cases::safe_profile(&case.profile), case.id),
        content: String::new(),
        generation: None,
        at: now(),
    })
}

pub fn apply(
    pack: &Pack,
    home: &Path,
    state: &Path,
    decisions: &[Decision],
    activate: bool,
) -> Result<ApplyReceipt, String> {
    let rows = pack::plan(pack, state)?;
    let prepared = preflight(pack, home, state, activate, decisions, &rows)?;
    let content = content_root(state, &pack.manifest.id, &pack.manifest.version)?;
    let existing_rows = read_catalog(state)?;
    let content_name = format!("packs/{}/{}", pack.manifest.id, pack.manifest.version);
    if existing_rows.iter().any(|row| row.content == content_name) {
        return Err(format!(
            "{} is already installed; uninstall it before applying again",
            content_name
        ));
    }
    if content.exists() {
        fs::remove_dir_all(&content)
            .map_err(|error| format!("could not clear {}: {error}", content.display()))?;
    }
    pack::copy_tree(pack.root(), &content)?;

    let mut rows_now = existing_rows;
    let mut installed = Vec::new();
    let mut displaced: Vec<String> = Vec::new();
    for item in prepared {
        let mut row = match item.decision.kind {
            Kind::Harness => install_harness(&item, pack, home, state)?,
            Kind::Skill => install_skill(&item, activate)?,
            Kind::Case => install_case(&item, state, pack)?,
        };
        row.pack = pack.manifest.id.clone();
        row.version = pack.manifest.version.clone();
        row.pack_digest = pack.digest.clone();
        row.content = content_name.clone();
        row.live = match row.kind {
            Kind::Skill => live_path(state, &item.target)?,
            Kind::Harness => live_path(home, &item.target)?,
            Kind::Case => live_path(state, &item.target)?,
        };
        let before = rows_now.len();
        rows_now.retain(|old| !(old.kind == row.kind && old.identity == row.identity));
        if rows_now.len() != before {
            for old in read_catalog(state)?
                .iter()
                .filter(|old| old.kind == row.kind && old.identity == row.identity)
            {
                displaced.push(old.content.clone());
            }
        }
        rows_now.push(row.clone());
        write_catalog(state, &rows_now)?;
        installed.push(row);
    }
    for old in displaced {
        if old != content_name
            && !rows_now.iter().any(|row| row.content == old)
            && state.join(&old).exists()
        {
            fs::remove_dir_all(state.join(&old))
                .map_err(|error| format!("could not remove {}: {error}", old))?;
        }
    }
    append_event(
        state,
        json!({
            "event": "apply",
            "pack": pack.manifest.id,
            "version": pack.manifest.version,
            "items": installed.len(),
            "at": now(),
        }),
    )?;
    Ok(ApplyReceipt {
        items: installed,
        skipped: decisions
            .iter()
            .filter(|decision| decision.action == Action::Skip)
            .count(),
    })
}

fn live_path(base: &Path, target: &Path) -> Result<String, String> {
    let rel = target
        .strip_prefix(base)
        .map_err(|_| format!("{} is not under {}", target.display(), base.display()))?;
    let mut parts = Vec::new();
    for part in rel.components() {
        let std::path::Component::Normal(name) = part else {
            return Err(format!("{} is not a plain path", target.display()));
        };
        parts.push(name.to_string_lossy().to_string());
    }
    if parts.is_empty() {
        return Err(format!("{} is {}", target.display(), base.display()));
    }
    Ok(parts.join("/"))
}

fn row_base(row: &Installed, home: &Path, state: &Path) -> Result<PathBuf, String> {
    let base = match row.kind {
        Kind::Harness => home,
        Kind::Skill | Kind::Case => state,
    };
    path_in(base, &row.live)
}

pub fn uninstall(
    home: &Path,
    state: &Path,
    id: &str,
    version: Option<&str>,
) -> Result<UninstallReceipt, String> {
    pack::valid_id(id)?;
    if let Some(version) = version {
        pack::valid_version(version)?;
    }
    let rows = read_catalog(state)?;
    let selected: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| row.pack == id && version.is_none_or(|version| row.version == version))
        .map(|(index, _)| index)
        .collect();
    if selected.is_empty() {
        return Err(format!("no installed items for pack `{id}`"));
    }
    let mut removed = Vec::new();
    let mut removed_contents: Vec<String> = Vec::new();
    let mut kept = Vec::new();
    let mut promoted = Vec::new();
    let keep_rows: Vec<Installed> = rows
        .iter()
        .enumerate()
        .filter(|(index, _)| !selected.contains(index))
        .map(|(_, row)| row.clone())
        .collect();
    for index in &selected {
        let row = &rows[*index];
        let path = row_base(row, home, state)?;
        let shared = keep_rows.iter().any(|other| other.live == row.live);
        if shared {
            kept.push(row.live.clone());
            continue;
        }
        if row.kind == Kind::Skill && !path.exists() {
            let active = state.join("skills").join(&row.identity);
            if active.exists() {
                promoted.push(row.live.clone());
                continue;
            }
        }
        if path.exists() {
            let digest = digest_live(&path)?;
            if digest != row.live_digest {
                kept.push(row.live.clone());
                continue;
            }
        }
        match row.kind {
            Kind::Case => {
                let profile = case_profile(&row.identity)?;
                if path.exists() {
                    cases::forget(state, &profile, &row.identity)?;
                }
            }
            Kind::Harness => {
                if let Some(generation) = row.generation {
                    if path.exists() {
                        tuning::restore_harness(
                            home,
                            &row.identity,
                            &path,
                            generation,
                            &tuning::guard_digest(),
                        )?;
                    }
                } else if path.exists() {
                    remove_live(&path)?;
                }
            }
            Kind::Skill => {
                if path.exists() {
                    remove_live(&path)?;
                }
            }
        }
        removed.push(row.live.clone());
        removed_contents.push(row.content.clone());
    }
    write_catalog(state, &keep_rows)?;
    let mut content_removed = false;
    for content in &removed_contents {
        if !keep_rows.iter().any(|keep| keep.content == *content) && state.join(content).exists() {
            fs::remove_dir_all(state.join(content)).map_err(|error| {
                format!(
                    "could not remove {}: {error}",
                    state.join(content).display()
                )
            })?;
            content_removed = true;
        }
    }
    append_event(
        state,
        json!({
            "event": "uninstall",
            "pack": id,
            "version": version,
            "removed": removed.len(),
            "kept": kept.len(),
            "promoted": promoted.len(),
            "at": now(),
        }),
    )?;
    Ok(UninstallReceipt {
        removed,
        kept,
        promoted,
        content_removed,
    })
}

// ── adapt and merge ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adapted {
    pub text: String,
    pub guidance: Option<String>,
    pub changes: Vec<String>,
    pub conflicts: Vec<String>,
}

pub fn item_text(pack: &Pack, kind: Kind, identity: &str) -> Result<String, String> {
    let (text, support) = provided_text(pack, kind, identity)?;
    let mut out = text;
    for (name, body) in support {
        out.push_str(&format!("\n--- {name} ---\n{body}"));
    }
    Ok(out)
}

fn provided_text(
    pack: &Pack,
    kind: Kind,
    identity: &str,
) -> Result<(String, Vec<(String, String)>), String> {
    let provided = provided_for(pack, kind, identity)?;
    let root = pack.root().join(&provided.path);
    match kind {
        Kind::Skill => {
            valid_skill_name(identity)?;
            let text = fs::read_to_string(root.join("SKILL.md"))
                .map_err(|error| format!("could not read {}: {error}", root.display()))?;
            let mut support = Vec::new();
            for (relative, path, _) in pack::walk_regular(&root)? {
                if relative == "SKILL.md" {
                    continue;
                }
                let body = fs::read_to_string(&path)
                    .map_err(|error| format!("could not read {}: {error}", path.display()))?;
                support.push((relative, body));
            }
            Ok((text, support))
        }
        Kind::Harness => {
            valid_profile_name(identity)?;
            let system = fs::read_to_string(root.join("system.md"))
                .map_err(|error| format!("could not read harness: {error}"))?;
            let guidance = fs::read_to_string(root.join("tool_guidance.md")).ok();
            Ok((
                system,
                guidance
                    .map(|guidance| vec![("tool_guidance.md".to_string(), guidance)])
                    .unwrap_or_default(),
            ))
        }
        Kind::Case => Err("a case is not adapted".to_string()),
    }
}

fn home_brief(home: &Path, state: &Path) -> String {
    let mut text = String::new();
    text.push_str("Installed skills:\n");
    for skill in skills::installed(state) {
        let evidence = fs::read_to_string(&skill.path)
            .map(|text| skills::evidence(&text))
            .ok();
        let contract = evidence
            .and_then(|evidence| evidence.contract)
            .unwrap_or_else(|| "TODO".to_string());
        text.push_str(&format!(
            "- {}: {}\n",
            skill.name,
            contract.lines().next().unwrap_or("TODO")
        ));
    }
    text.push_str("Profiles:\n");
    for profile in profile::shipped() {
        text.push_str(&format!("- {} check: {}\n", profile.id, profile.verify));
    }
    text.push_str(&format!(
        "Home: {}; state: {}; harness root: {}\n",
        home.display(),
        state.display(),
        home.join("harness").display()
    ));
    if text.len() > 8 * 1024 {
        text.truncate(8 * 1024);
    }
    text
}

fn prompt_cap(prompt: String) -> Result<String, String> {
    if prompt.len() > MAX_PROMPT {
        return Err(format!(
            "the adaptation prompt is {} bytes; the cap is {MAX_PROMPT}",
            prompt.len()
        ));
    }
    Ok(prompt)
}

pub fn adapt_prompt(
    pack: &Pack,
    home: &Path,
    state: &Path,
    kind: Kind,
    identity: &str,
) -> Result<String, String> {
    if pack.manifest.derivatives != "allowed" {
        return Err(format!(
            "this pack's license says derivatives are `{}`; adaptation needs `allowed`",
            pack.manifest.derivatives
        ));
    }
    let (text, support) = provided_text(pack, kind, identity)?;
    if text.len() > MAX_TEXT {
        return Err(format!(
            "the item text is {} bytes; the cap is {MAX_TEXT}",
            text.len()
        ));
    }
    let mut prompt = format!(
        "Adapt one installed procedure to this Hades home.\n\nPack: {} {}\nLicense: {} (derivatives allowed)\n\\
         Item: {} `{identity}`\n\nThe current text follows. Rewrite it for this home. The support files are \\
         read-only context; do not fold them into the text.\n\n",
        pack.manifest.id,
        pack.manifest.version,
        pack.manifest.license,
        kind.name()
    );
    if kind == Kind::Skill {
        prompt.push_str("CURRENT SKILL.md (the only text you replace):\n");
    } else {
        prompt.push_str("CURRENT system.md (the system prompt):\n");
    }
    prompt.push_str(&text);
    prompt.push_str("\n\nSupport files (read-only):\n");
    for (name, body) in &support {
        prompt.push_str(&format!("--- {name} ---\n{body}\n"));
    }
    prompt.push_str("\nHome brief:\n");
    prompt.push_str(&home_brief(home, state));
    if kind == Kind::Skill {
        prompt.push_str(
            "\nReply with one JSON object and nothing else:\n\
             {\"text\":\"the complete new SKILL.md\",\"changes\":[\"literal snippet touched\"],\"conflicts\":[]}\n\
             `changes` must be non-empty and each entry must occur in the old or new text. Do not mention a \\
             check, test, grader, expected answer, score or rubric. Keep the text under 32 KiB.\n",
        );
    } else {
        prompt.push_str(
            "\nReply with one JSON object and nothing else:\n\
             {\"system\":\"the complete new system prompt\",\"guidance\":\"the complete new tool guidance\",\\
             \"changes\":[\"literal snippet touched\"],\"conflicts\":[]}\n\
             `changes` must be non-empty and each entry must occur in the old or new text. Do not add tools. \\
             Keep each field under 32 KiB.\n",
        );
    }
    prompt_cap(prompt)
}

fn strip_fence(reply: &str) -> &str {
    let text = reply.trim();
    text.strip_prefix("```json")
        .or_else(|| text.strip_prefix("```"))
        .unwrap_or(text)
        .trim_end_matches("```")
        .trim()
}

fn strings(value: Option<&Value>, name: &str) -> Result<Vec<String>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let array = value
        .as_array()
        .ok_or_else(|| format!("`{name}` must be an array"))?;
    let mut out = Vec::new();
    for item in array {
        out.push(
            item.as_str()
                .ok_or_else(|| format!("`{name}` must contain strings"))?
                .to_string(),
        );
    }
    Ok(out)
}

fn normalize(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn validate_changes(old: &str, new: &str, changes: &[String]) -> Result<(), String> {
    if changes.is_empty() {
        return Err("the model returned an empty change list".to_string());
    }
    let old = normalize(old);
    let new = normalize(new);
    for change in changes {
        let needle = normalize(change);
        if needle.is_empty() || (!old.contains(&needle) && !new.contains(&needle)) {
            return Err(format!(
                "the change `{change}` is not present in the old or adapted text"
            ));
        }
    }
    Ok(())
}

pub fn parse_adapt(kind: Kind, old: &str, reply: &str) -> Result<Adapted, String> {
    let value: Value = serde_json::from_str(strip_fence(reply))
        .map_err(|error| format!("the adapter did not answer with JSON: {error}"))?;
    let object = value
        .as_object()
        .ok_or_else(|| "the adapter's answer is not an object".to_string())?;
    let changes = strings(object.get("changes"), "changes")?;
    let conflicts = strings(object.get("conflicts"), "conflicts")?;
    let (text, guidance) = match kind {
        Kind::Skill => {
            let text = object
                .get("text")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .ok_or_else(|| "the adapter returned no text".to_string())?
                .to_string();
            (text, None)
        }
        Kind::Harness => {
            let system = object
                .get("system")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .ok_or_else(|| "the adapter returned no system prompt".to_string())?
                .to_string();
            let guidance = object
                .get("guidance")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string);
            (system, guidance)
        }
        Kind::Case => return Err("a case is not adapted".to_string()),
    };
    if text.len() > MAX_TEXT {
        return Err("the adapted text is over 32 KiB".to_string());
    }
    if let Some(guidance) = &guidance
        && guidance.len() > MAX_TEXT
    {
        return Err("the adapted guidance is over 32 KiB".to_string());
    }
    validate_changes(old, &text, &changes)?;
    Ok(Adapted {
        text,
        guidance,
        changes,
        conflicts,
    })
}

pub fn line_diff(before: &str, after: &str) -> String {
    let old: Vec<&str> = before.lines().collect();
    let new: Vec<&str> = after.lines().collect();
    let mut start = 0;
    while start < old.len() && start < new.len() && old[start] == new[start] {
        start += 1;
    }
    let mut old_end = old.len();
    let mut new_end = new.len();
    while old_end > start && new_end > start && old[old_end - 1] == new[new_end - 1] {
        old_end -= 1;
        new_end -= 1;
    }
    let mut out = String::new();
    for line in &old[start..old_end] {
        out.push_str(&format!("- {line}\n"));
    }
    for line in &new[start..new_end] {
        out.push_str(&format!("+ {line}\n"));
    }
    out
}

pub fn adapted_report(old: &str, adapted: &Adapted) -> String {
    let mut text = line_diff(old, &adapted.text);
    text.push_str("changes:\n");
    for change in &adapted.changes {
        text.push_str(&format!("  {change}\n"));
    }
    if !adapted.conflicts.is_empty() {
        text.push_str("conflicts:\n");
        for conflict in &adapted.conflicts {
            text.push_str(&format!("  {conflict}\n"));
        }
    }
    text
}

pub fn accept_adapted(
    pack: &Pack,
    home: &Path,
    state: &Path,
    kind: Kind,
    identity: &str,
    adapted: &Adapted,
) -> Result<String, String> {
    match kind {
        Kind::Skill => {
            valid_skill_name(identity)?;
            let target = state.join("skills").join("proposed").join(identity);
            if target.exists() {
                return Err(format!("a proposal called `{identity}` already exists"));
            }
            let temp = temp_beside(&target, "adapt")?;
            let result = (|| {
                pack::copy_tree(
                    &pack.root().join(provided_for(pack, kind, identity)?.path),
                    &temp,
                )?;
                let text = skills::set_status(&skills::ensure_defaults(&adapted.text), "proposed");
                fs::write(temp.join("SKILL.md"), text)
                    .map_err(|error| format!("could not write the adapted skill: {error}"))?;
                fs::rename(&temp, &target)
                    .map_err(|error| format!("could not install {}: {error}", target.display()))?;
                Ok(target.display().to_string())
            })();
            let _ = fs::remove_dir_all(&temp);
            result
        }
        Kind::Harness => {
            valid_profile_name(identity)?;
            let target = home.join("harness").join(identity);
            let temp = temp_beside(&target, "adapt")?;
            let result = (|| {
                fs::write(temp.join("system.md"), &adapted.text).map_err(|error| {
                    format!("could not write the adapted system prompt: {error}")
                })?;
                if let Some(guidance) = &adapted.guidance {
                    fs::write(temp.join("tool_guidance.md"), guidance).map_err(|error| {
                        format!("could not write the adapted guidance: {error}")
                    })?;
                }
                let generation = tuning::record_adoption(
                    home,
                    identity,
                    &target,
                    state,
                    &temp,
                    &format!("pack-adapt:{}@{}", pack.manifest.id, pack.manifest.version),
                    "",
                    &tuning::guard_digest(),
                )?;
                Ok(match generation {
                    Some(generation) => format!("generation {}", generation.id),
                    None => "no generation (the target already held these bytes)".to_string(),
                })
            })();
            let _ = fs::remove_dir_all(&temp);
            result
        }
        Kind::Case => Err("a case is not adapted".to_string()),
    }
}

pub fn merge_prompt(
    pack: &Pack,
    home: &Path,
    state: &Path,
    identity: &str,
    into: &str,
) -> Result<String, String> {
    if pack.manifest.derivatives != "allowed" {
        return Err(format!(
            "this pack's license says derivatives are `{}`; merging needs `allowed`",
            pack.manifest.derivatives
        ));
    }
    let (incoming, support) = provided_text(pack, Kind::Skill, identity)?;
    let existing = skills::installed(state)
        .into_iter()
        .find(|skill| skill.name == into)
        .ok_or_else(|| format!("there is no installed skill called `{into}`"))?;
    let base = fs::read_to_string(&existing.path)
        .map_err(|error| format!("could not read {}: {error}", existing.path.display()))?;
    if incoming.len().max(base.len()) > MAX_TEXT {
        return Err("one side of the merge is over 32 KiB".to_string());
    }
    let mut prompt = format!(
        "Merge one pack skill into an existing installed skill. Never edit the existing skill.\n\n\\
         Pack: {} {}\nLicense: {} (derivatives allowed)\nIncoming skill: {identity}\n\\
         Existing skill: {into}\n\nINCOMING SKILL.md:\n{incoming}\n\nEXISTING SKILL.md:\n{base}\n\n\\
         Incoming support files (read-only):\n",
        pack.manifest.id, pack.manifest.version, pack.manifest.license
    );
    for (name, body) in &support {
        prompt.push_str(&format!("--- {name} ---\n{body}\n"));
    }
    prompt.push_str("\nHome brief:\n");
    prompt.push_str(&home_brief(home, state));
    prompt.push_str(
        "\nReply with one JSON object and nothing else:\n\
         {\"text\":\"the complete merged SKILL.md\",\"changes\":[\"literal snippet touched\"],\"conflicts\":[\"...\"]}\n\
         Keep both contracts where they do not contradict. Put every unresolved disagreement in `conflicts`. \\
         `changes` must be non-empty and each entry must occur in one of the two texts. Do not mention a check, \\
         test, grader, expected answer, score or rubric. Keep the text under 32 KiB.\n",
    );
    prompt_cap(prompt)
}

pub fn parse_merge(incoming: &str, base: &str, reply: &str) -> Result<Adapted, String> {
    let mut adapted = parse_adapt(Kind::Skill, &format!("{incoming}\n{base}"), reply)?;
    let old = format!("{incoming}\n{base}");
    adapted.changes.retain(|change| {
        let needle = normalize(change);
        !needle.is_empty()
            && (normalize(&old).contains(&needle) || normalize(&adapted.text).contains(&needle))
    });
    if adapted.changes.is_empty() {
        return Err("the merge returned no change present in either side".to_string());
    }
    Ok(adapted)
}

pub fn accept_merge(
    pack: &Pack,
    home: &Path,
    state: &Path,
    identity: &str,
    into: &str,
    name: &str,
    adapted: &Adapted,
) -> Result<String, String> {
    valid_skill_name(name)?;
    if name == into {
        return Err(
            "the merged skill needs a new name; the target is never overwritten".to_string(),
        );
    }
    let target = state.join("skills").join("proposed").join(name);
    if target.exists() {
        return Err(format!("a proposal called `{name}` already exists"));
    }
    let incoming = pack
        .root()
        .join(provided_for(pack, Kind::Skill, identity)?.path);
    let existing = skills::installed(state)
        .into_iter()
        .find(|skill| skill.name == into)
        .ok_or_else(|| format!("there is no installed skill called `{into}`"))?;
    let base = existing
        .path
        .parent()
        .ok_or_else(|| "the installed skill has no directory".to_string())?
        .to_path_buf();
    let temp = temp_beside(&target, "merge")?;
    let result = (|| {
        copy_support(&incoming, &temp, Some("SKILL.md"))?;
        copy_support(&base, &temp, Some("SKILL.md"))?;
        let mut text = skills::ensure_defaults(&adapted.text);
        text = skills::set_status(&text, "proposed");
        text = skills::set_field(&text, "merged_from", into);
        text = skills::set_field(
            &text,
            "pack",
            &format!("{}@{}", pack.manifest.id, pack.manifest.version),
        );
        if !adapted.conflicts.is_empty() {
            let conflict = serde_json::to_string(&adapted.conflicts)
                .map_err(|error| format!("could not encode conflicts: {error}"))?;
            text = skills::set_field(&text, "conflicts", &conflict);
        }
        fs::write(temp.join("SKILL.md"), text)
            .map_err(|error| format!("could not write the merged skill: {error}"))?;
        fs::rename(&temp, &target)
            .map_err(|error| format!("could not install {}: {error}", target.display()))?;
        Ok(target.display().to_string())
    })();
    let _ = fs::remove_dir_all(&temp);
    let _ = home;
    result
}

fn copy_support(from: &Path, to: &Path, skip: Option<&str>) -> Result<(), String> {
    fs::create_dir_all(to).map_err(|error| format!("could not make {}: {error}", to.display()))?;
    let mut entries: Vec<PathBuf> = fs::read_dir(from)
        .map_err(|error| format!("could not read {}: {error}", from.display()))?
        .flatten()
        .map(|entry| entry.path())
        .collect();
    entries.sort();
    for path in entries {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        if skip == Some(name.as_str()) {
            continue;
        }
        let target = to.join(&name);
        let meta = fs::symlink_metadata(&path)
            .map_err(|error| format!("could not stat {}: {error}", path.display()))?;
        if meta.is_dir() {
            copy_support(&path, &target, None)?;
        } else if meta.is_file() {
            if target.exists() {
                let old = fs::read(&target)
                    .map_err(|error| format!("could not read {}: {error}", target.display()))?;
                let new = fs::read(&path)
                    .map_err(|error| format!("could not read {}: {error}", path.display()))?;
                if old != new {
                    return Err(format!(
                        "support file collision at {}: both sides carry different bytes",
                        target.display()
                    ));
                }
            } else {
                fs::copy(&path, &target)
                    .map_err(|error| format!("could not copy {}: {error}", path.display()))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex() -> String {
        "a".repeat(64)
    }

    #[test]
    fn a_decision_file_refuses_unknown_keys_actions_and_repeats() {
        let good = crate::pack_fit::parse_decisions(
            r#"{"items":[{"kind":"skill","identity":"a","action":"install"}]}"#,
        )
        .expect("a decision");
        assert_eq!(good.len(), 1);
        assert_eq!(good[0].action, Action::Install);

        for bad in [
            r#"{"items":[]}"#,
            r#"{"items":[{"kind":"skill","identity":"a","action":"later"}]}"#,
            r#"{"items":[{"kind":"ghost","identity":"a","action":"install"}]}"#,
            r#"{"items":[{"kind":"skill","identity":"","action":"install"}]}"#,
            r#"{"items":[{"kind":"skill","identity":"a","action":"install"},{"kind":"skill","identity":"a","action":"skip"}]}"#,
            r#"{"items":[{"kind":"skill","identity":"a","action":"install"}],"extra":1}"#,
        ] {
            assert!(crate::pack_fit::parse_decisions(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_catalog_row_must_name_its_own_live_and_content_paths() {
        let row = Installed {
            kind: Kind::Skill,
            identity: "release-notes".to_string(),
            pack: "demo".to_string(),
            version: "1.0.0".to_string(),
            pack_digest: hex(),
            digest: hex(),
            live_digest: hex(),
            live: "skills/proposed/release-notes".to_string(),
            content: "packs/demo/1.0.0".to_string(),
            generation: None,
            at: 1,
        };
        assert!(validate_row(&row).is_ok());

        let mut escaped = row.clone();
        escaped.live = "skills/../outside".to_string();
        assert!(validate_row(&escaped).is_err());

        let mut content = row.clone();
        content.content = "packs/other/1.0.0".to_string();
        assert!(validate_row(&content).is_err());

        let mut harness = row.clone();
        harness.kind = Kind::Harness;
        harness.identity = "research".to_string();
        harness.live = "harness/research".to_string();
        assert!(validate_row(&harness).is_ok());
    }

    #[test]
    fn a_change_must_appear_in_the_old_or_new_text() {
        let old = "Use the old command\nand keep the note.";
        let new = "Use the new command\nand keep the note.";
        let changes = vec!["new command".to_string()];
        assert!(validate_changes(old, new, &changes).is_ok());
        assert!(validate_changes(old, new, &["not in either".to_string()]).is_err());
        assert!(validate_changes(old, new, &[]).is_err());
    }

    #[test]
    fn an_adapted_reply_is_parsed_with_or_without_a_fence() {
        let reply =
            "```json\n{\"text\":\"new body\",\"changes\":[\"new body\"],\"conflicts\":[]}\n```";
        let adapted = parse_adapt(Kind::Skill, "old body", reply).expect("a reply");
        assert_eq!(adapted.text, "new body");
        assert!(adapted.conflicts.is_empty());

        let harness = parse_adapt(
            Kind::Harness,
            "old system",
            r#"{"system":"new system","guidance":"new guidance","changes":["new system"],"conflicts":["one"]}"#,
        )
        .expect("a harness reply");
        assert_eq!(harness.guidance.as_deref(), Some("new guidance"));
        assert_eq!(harness.conflicts, vec!["one"]);
    }

    #[test]
    fn support_files_with_different_bytes_do_not_collide_silently() {
        let root = scratch::unique("pack-fit-collision");
        let a = root.join("a");
        let b = root.join("b");
        let to = root.join("to");
        fs::create_dir_all(&a).expect("a");
        fs::create_dir_all(&b).expect("b");
        fs::write(a.join("run.sh"), "one").expect("a file");
        fs::write(b.join("run.sh"), "two").expect("b file");
        copy_support(&a, &to, None).expect("first side");
        assert!(copy_support(&b, &to, None).is_err());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_line_diff_shows_only_the_changed_middle() {
        let diff = line_diff("one\ntwo\nthree", "one\nchanged\nthree");
        assert_eq!(diff, "- two\n+ changed\n");
    }
}
