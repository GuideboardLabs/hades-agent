//! Reading a run as evidence, and writing the composition down.
//!
//! The harness tuning loop needs two things from a finished run: what it cost, and whether the check
//! passed. It needs one thing from the editor: a candidate prompt. This module is the part of that loop
//! that can be read and tested without a model: the receipt, the transcript we are willing to show an
//! editor (with the check redacted), the choice between candidates, and the library note.
//!
//! Why redact the check: an editor that can read `--verify "grep -q 42 out.txt"` will write a prompt that
//! makes the model produce `42` in `out.txt`, which is not the task. The editor may see that a run failed
//! and what the model did; it may not see the string it is being graded by. The candidate is then probed
//! against the real check, which the editor never saw.
//!
//! An edit that survives the probe is adopted and numbered. Each adoption writes the harness it replaced
//! under `<home>/control/harness/<profile>/<n>/` with a manifest naming every file it kept: the two harness
//! files and every skill, because a skill is an instruction the run loads and an edit to one is a harness
//! edit. The generation records the floor, policy and profile defaults it was captured under, so a rollback
//! cannot cross a change to any of the three. `hades refine --rollback <n>` puts one back.

use std::path::Path;

use serde_json::Value;

use crate::verify::Progress;

/// What one finished run cost and whether it passed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    pub passed: bool,
    pub answers: usize,
    pub millis: u64,
    pub answer_bytes: usize,
    pub check: String,
    /// The run stopped at an approval question and nobody was there to answer it. Different from a check
    /// that failed, and different from a probe that crashed: the arm was never measured.
    pub awaiting: bool,
    /// The check's partial report, when it opted into the contract.
    pub progress: Option<Progress>,
    /// The protected holdout check, when a person supplied one. `None` means unmeasured.
    pub holdout: Option<bool>,
}

impl Receipt {
    pub fn line(&self) -> String {
        format!(
            "{} answer(s), {} ms, {} bytes, check {}{}",
            self.answers,
            self.millis,
            self.answer_bytes,
            if self.check.is_empty() {
                "not run"
            } else {
                self.check.as_str()
            },
            self.progress
                .as_ref()
                .map(|progress| format!(" ({})", progress.line()))
                .unwrap_or_default()
        )
    }
}

/// Read one session log into a receipt. A missing or unreadable log is a run that did not happen, which is
/// a failure with a reason rather than a panic.
pub fn receipt(log: &Path) -> Receipt {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let mut answers = 0;
    let mut millis = 0;
    let mut answer_bytes = 0;
    let mut check = String::new();
    let mut awaiting = false;
    let mut progress: Option<Progress> = None;

    for line in text.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let kind = entry
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let payload = entry.get("payload").unwrap_or(&Value::Null);
        match kind {
            "tool_call" => {
                if let Some(model) = payload.get("model") {
                    answers += 1;
                    millis += model.get("millis").and_then(Value::as_u64).unwrap_or(0);
                }
                if let Some(program) = payload.get("program").and_then(Value::as_str) {
                    answer_bytes += program.len();
                }
            }
            "tool_result" if payload.get("tool").and_then(Value::as_str) == Some("verify") => {
                check = payload
                    .get("state")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                progress = payload.get("progress").and_then(Progress::from_value);
            }
            "checkpoint"
                if payload.get("node").and_then(Value::as_str) == Some("awaiting")
                    || payload.get("condition").and_then(Value::as_str) == Some("Asked") =>
            {
                awaiting = true;
            }
            _ => {}
        }
    }

    Receipt {
        passed: check == "passed",
        answers,
        millis,
        answer_bytes,
        check,
        awaiting,
        progress,
        holdout: None,
    }
}

/// A transcript an editor may see: what the model answered and what came back, with every verify program
/// replaced by its outcome and the check text redacted wherever it appears.
pub fn transcript(log: &Path, check: &str, cap: usize) -> String {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let mut said = String::new();
    let mut last_tool = String::new();

    for line in text.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let kind = entry
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let payload = entry.get("payload").unwrap_or(&Value::Null);
        match kind {
            "tool_call" => {
                let tool = payload.get("tool").and_then(Value::as_str).unwrap_or("?");
                let program = payload.get("program").and_then(Value::as_str).unwrap_or("");
                last_tool = tool.to_string();
                if tool == "verify" {
                    said.push_str("\n[the check ran; its program is withheld]\n");
                } else {
                    said.push_str(&format!(
                        "\n--- {tool}\n{}\n",
                        redact(program, check).trim_end()
                    ));
                }
            }
            "tool_result" => {
                let output = payload.get("output").and_then(Value::as_str).unwrap_or("");
                let state = payload.get("state").and_then(Value::as_str).unwrap_or("");
                if last_tool == "verify" {
                    said.push_str(&format!("[check: {state}]\n"));
                } else if !output.trim().is_empty() {
                    let shown = clip(redact(output, check).trim_end(), 400);
                    said.push_str(&format!("{shown}\n"));
                }
            }
            "checkpoint"
                if payload.get("condition").and_then(Value::as_str) == Some("Exhausted") =>
            {
                said.push_str("\n[the run exhausted its attempts]\n");
            }
            _ => {}
        }
        if said.len() > cap {
            break;
        }
    }
    clip(&said, cap)
}

fn clip(text: &str, cap: usize) -> String {
    if text.len() <= cap {
        return text.to_string();
    }
    let mut end = cap;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[truncated]", &text[..end])
}

/// Remove the check's own text. The editor is told a check exists, never what it is.
pub fn redact(text: &str, check: &str) -> String {
    let check = check.trim();
    if check.is_empty() {
        return text.to_string();
    }
    text.replace(check, "<the check>")
}

/// One row of the composition table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub name: String,
    pub receipt: Receipt,
    pub note: String,
}

/// The best row: a passing row beats a failing one, then fewer milliseconds, then fewer answers. `base` is
/// the reference and is never chosen as an adoption target, because copying the base over itself is not a
/// composition.
pub fn choose(rows: &[Row]) -> Option<usize> {
    let mut best: Option<usize> = None;
    for (index, row) in rows.iter().enumerate() {
        if row.name == "base" {
            continue;
        }
        let better = match best {
            None => true,
            Some(at) => {
                let current = &rows[at];
                match (row.receipt.passed, current.receipt.passed) {
                    (true, false) => true,
                    (false, true) => false,
                    (true, true) => match (row.receipt.holdout, current.receipt.holdout) {
                        (Some(true), Some(false)) => true,
                        (Some(false), Some(true)) => false,
                        _ => {
                            let row_progress = row.receipt.progress.as_ref().map(Progress::ratio);
                            let current_progress =
                                current.receipt.progress.as_ref().map(Progress::ratio);
                            match (row_progress, current_progress) {
                                (Some(left), Some(right)) if left != right => left > right,
                                _ => {
                                    (row.receipt.millis, row.receipt.answers)
                                        < (current.receipt.millis, current.receipt.answers)
                                }
                            }
                        }
                    },
                    _ => {
                        // A failure that got further is the more useful row. Without a progress report the
                        // two are equal, so the old cost tie-break still decides.
                        let row_progress = row.receipt.progress.as_ref().map(Progress::ratio);
                        let current_progress =
                            current.receipt.progress.as_ref().map(Progress::ratio);
                        match (row_progress, current_progress) {
                            (Some(left), Some(right)) if left != right => left > right,
                            (Some(_), None) => true,
                            (None, Some(_)) => false,
                            _ => {
                                (row.receipt.millis, row.receipt.answers)
                                    < (current.receipt.millis, current.receipt.answers)
                            }
                        }
                    }
                }
            }
        };
        if better {
            best = Some(index);
        }
    }
    best
}

/// Whether the chosen candidate beats the reference on the metric that matters: a pass where the base
/// failed, or an equal pass for less. A tie keeps the base, because an edit with no evidence is churn.
pub fn beats_base(base: &Row, candidate: &Row) -> bool {
    // A candidate that fails the protected holdout is not adoptable, even when it passes the check it was
    // tuned against. That is the whole point of keeping a second check out of the editor's view.
    if candidate.receipt.holdout == Some(false) {
        return false;
    }
    match (candidate.receipt.passed, base.receipt.passed) {
        (true, false) => true,
        (false, true) => false,
        (true, true) => match (candidate.receipt.holdout, base.receipt.holdout) {
            (Some(true), Some(false)) => true,
            (Some(false), Some(true)) => false,
            _ => {
                (candidate.receipt.millis, candidate.receipt.answers)
                    < (base.receipt.millis, base.receipt.answers)
            }
        },
        (false, false) => false,
    }
}

/// The library note: what was tried, what it cost, what was adopted and why. Written even when nothing is
/// adopted, because a failed candidate is the evidence that the base is still the best harness we have.
pub fn experience(
    goal: &str,
    check: &str,
    rows: &[Row],
    adopted: Option<&str>,
    lesson: &str,
    candidate_dirs: &[(String, String)],
) -> String {
    let mut text = format!("# Harness tune: {}\n\n", one_line(goal));
    text.push_str(&format!(
        "- check: `{}`\n- adopted: `{}`\n\n",
        if check.trim().is_empty() {
            "none"
        } else {
            check.trim()
        },
        adopted.unwrap_or("none (base kept)")
    ));
    text.push_str("| variant | check | answers | model ms | answer bytes | note |\n");
    text.push_str("|---|---|---|---|---|---|\n");
    for row in rows {
        text.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            row.name,
            if row.receipt.check.is_empty() {
                "not run"
            } else {
                row.receipt.check.as_str()
            },
            row.receipt.answers,
            row.receipt.millis,
            row.receipt.answer_bytes,
            one_line(&row.note)
        ));
    }
    if !candidate_dirs.is_empty() {
        text.push_str("\n## Candidates\n\n");
        for (name, dir) in candidate_dirs {
            text.push_str(&format!("- `{name}`: `{dir}`\n"));
        }
    }
    if !lesson.trim().is_empty() {
        text.push_str(&format!("\n## Lesson\n\n{}\n", lesson.trim()));
    }
    text
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The editor's answer: one JSON object, with the check's own text forbidden anywhere in it.
pub fn candidate_from(
    reply: &str,
    check: &str,
) -> Result<(String, String, String, String), String> {
    let text = reply.trim();
    let text = text
        .strip_prefix("```json")
        .or_else(|| text.strip_prefix("```"))
        .unwrap_or(text)
        .trim_end_matches("```")
        .trim();
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|error| format!("the editor did not answer with JSON: {error}"))?;
    let system = value
        .get("system")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if system.is_empty() {
        return Err("the editor's candidate has no system prompt".to_string());
    }
    if system.len() > crate::harness_files::CAP {
        return Err("the editor's system prompt is over 32 KiB".to_string());
    }
    let guidance = value
        .get("guidance")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if guidance.len() > crate::harness_files::CAP {
        return Err("the editor's tool guidance is over 32 KiB".to_string());
    }
    if !check.trim().is_empty()
        && (system.contains(check.trim()) || guidance.contains(check.trim()))
    {
        return Err("the editor wrote the check into its candidate".to_string());
    }
    let reason = value
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("no reason given")
        .trim()
        .to_string();
    let lesson = value
        .get("lesson")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    Ok((system, guidance, reason, lesson))
}

/// Copy the prompt and guidance a target holds into `versions/<seconds>/` before an adoption overwrites
/// them. Returns the directory, or `None` when there was nothing to keep. A version is a plain directory:
/// restoring is a copy back, and nothing depends on a manifest that could itself be wrong.
///
/// Superseded by [`capture`], which numbers a generation, names every file it kept and refuses to restore
/// across a floor change. This stays so the `versions/` directories an earlier release wrote can still be
/// read while they are migrated, and it is no longer called by `hades tune`.
pub fn keep_version(target: &Path) -> Result<Option<std::path::PathBuf>, String> {
    let files: Vec<&str> = ["system.md", "tool_guidance.md"]
        .into_iter()
        .filter(|name| target.join(name).is_file())
        .collect();
    if files.is_empty() {
        return Ok(None);
    }
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0);
    let dir = target.join("versions").join(seconds.to_string());
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("could not make {}: {error}", dir.display()))?;
    for name in files {
        std::fs::copy(target.join(name), dir.join(name))
            .map_err(|error| format!("could not keep the previous {name}: {error}"))?;
    }
    Ok(Some(dir))
}

/// Copy one candidate over a harness directory: the system prompt always, the guidance when the candidate
/// has one. A candidate that drops the guidance removes the target's, because dropping it is part of the
/// edit and leaving the old file behind would make the adopted harness a blend of two.
pub fn adopt(target: &Path, source: &Path) -> Result<(), String> {
    std::fs::create_dir_all(target)
        .map_err(|error| format!("could not make {}: {error}", target.display()))?;
    std::fs::copy(source.join("system.md"), target.join("system.md")).map_err(|error| {
        format!(
            "could not copy the adopted prompt into {}: {error}",
            target.display()
        )
    })?;
    let guidance = source.join("tool_guidance.md");
    if guidance.is_file() {
        std::fs::copy(&guidance, target.join("tool_guidance.md"))
            .map_err(|error| format!("could not copy the adopted guidance: {error}"))?;
    } else {
        let stale = target.join("tool_guidance.md");
        match std::fs::remove_file(&stale) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "the candidate drops the guidance but {} could not be removed: {error}",
                    stale.display()
                ));
            }
        }
    }
    Ok(())
}

/// One captured file inside a generation: where it belongs, and the digest of the bytes that were recorded.
///
/// The path is relative to the generation's own root and always starts with `harness/` or `skills/`, so a
/// restore can resolve it without knowing where the home and state directories were at capture time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerationFile {
    pub path: String,
    pub sha256: String,
}

/// A numbered harness state: what one adoption replaced, and the evidence that produced it.
///
/// The order matters and it is the reason this type exists. A generation is the state *before* an
/// adoption, so restoring generation `n` undoes adoption `n` and the first adoption is undoable too. The
/// parent pointer makes the chain readable, and the check records what the adoption that came after this
/// state was measured with. `harness` is the absolute directory those harness files belonged to, so a
/// restore into a different `--harness` is refused rather than silently writing a tree that never held them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Generation {
    pub id: u64,
    pub parent: Option<u64>,
    pub harness: Option<String>,
    pub files: Vec<GenerationFile>,
    pub source_session: String,
    pub check: String,
    pub created: u64,
}

/// The file a generation is described by, beside the copies it names.
pub const MANIFEST: &str = "manifest.json";
/// The digest of the floor, policy and profile defaults this generation was captured under.
const GUARD: &str = "guard.txt";

/// The five floor categories, read from the floor itself so a category added there is in the digest. There
/// is no second list here to fall behind: `Category::all` is the only enumeration.
const GUARDED_CATEGORIES: [crate::floor::Category; 5] = crate::floor::Category::all();

/// The actions a policy digest probes, from the policy. The rules themselves are private and an action
/// list is a better question anyway: what matters is the decision and the reason a person would read for
/// it.
const PROBED: [&str; 4] = crate::policy::probes();

fn sha256(bytes: &[u8]) -> String {
    mneme::sha256::hex(bytes)
}

/// Where a profile's generations live: `<home>/control/harness/<profile>/<n>/`.
pub fn generations_root(home: &Path, profile: &str) -> std::path::PathBuf {
    home.join("control").join("harness").join(profile)
}

fn generation_dir(home: &Path, profile: &str, id: u64) -> std::path::PathBuf {
    generations_root(home, profile).join(id.to_string())
}

/// Read one generation's manifest, or `None` when that number was never written.
fn read_generation(home: &Path, profile: &str, id: u64) -> Result<Option<Generation>, String> {
    let path = generation_dir(home, profile, id).join(MANIFEST);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("could not read {}: {error}", path.display())),
    };
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| format!("{} is not a manifest: {error}", path.display()))?;
    let number = |value: Option<&serde_json::Value>| value.and_then(serde_json::Value::as_u64);
    let string = |value: Option<&serde_json::Value>| {
        value
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let Some(id) = number(value.get("id")) else {
        return Err(format!("{} has no generation id", path.display()));
    };
    let mut files = Vec::new();
    for file in value
        .get("files")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        let Some(named) = file.get("path").and_then(serde_json::Value::as_str) else {
            return Err(format!("{} has a file with no path", path.display()));
        };
        files.push(GenerationFile {
            path: named.to_string(),
            sha256: string(file.get("sha256")),
        });
    }
    Ok(Some(Generation {
        id,
        parent: number(value.get("parent")),
        harness: value
            .get("harness")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        files,
        source_session: string(value.get("source_session")),
        check: string(value.get("check")),
        created: number(value.get("created")).unwrap_or(0),
    }))
}

/// Every generation a profile has, oldest first. A numeric directory without a manifest is not a
/// generation; it is the debris of a write that did not finish, and it is skipped rather than reported as
/// an empty harness.
pub fn generations(home: &Path, profile: &str) -> Result<Vec<Generation>, String> {
    let root = generations_root(home, profile);
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Ok(Vec::new());
    };
    let mut ids: Vec<u64> = entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().to_string_lossy().parse::<u64>().ok())
        .collect();
    ids.sort_unstable();
    let mut found = Vec::new();
    for id in ids {
        if let Some(generation) = read_generation(home, profile, id)? {
            found.push(generation);
        }
    }
    Ok(found)
}

fn latest_generation(home: &Path, profile: &str) -> Result<Option<Generation>, String> {
    Ok(generations(home, profile)?.pop())
}

/// The bytes a generation would record: the two harness files when they exist, and every skill file.
///
/// Sorted, because a manifest whose order moved with the filesystem would be a diff nobody can read.
fn captured(harness: &Path, state: &Path) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut found: Vec<(String, Vec<u8>)> = Vec::new();
    for name in ["system.md", "tool_guidance.md"] {
        let path = harness.join(name);
        match std::fs::read(&path) {
            Ok(bytes) => found.push((format!("harness/{name}"), bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("could not read {}: {error}", path.display())),
        }
    }
    let skills = state.join("skills");
    for path in crate::skills::skill_files(state) {
        let bytes = std::fs::read(&path)
            .map_err(|error| format!("could not read {}: {error}", path.display()))?;
        let Some(relative) = relative_posix(&skills, &path) else {
            return Err(format!(
                "{} is not under {}, so a generation could not name it",
                path.display(),
                skills.display()
            ));
        };
        found.push((format!("skills/{relative}"), bytes));
    }
    found.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(found)
}

fn relative_posix(base: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(base).ok()?;
    let mut parts = Vec::new();
    for part in relative.components() {
        match part {
            std::path::Component::Normal(name) => parts.push(name.to_string_lossy().to_string()),
            _ => return None,
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

/// Write the harness the profile holds now as the next generation, before an adoption replaces it.
///
/// `None` covers two cases that are the same fact: there is nothing to keep, or the files are exactly what
/// the last generation recorded, in which case a second identical generation is noise rather than history.
pub fn capture(
    home: &Path,
    profile: &str,
    harness: &Path,
    state: &Path,
    source_session: &str,
    check: &str,
    guard: &str,
) -> Result<Option<Generation>, String> {
    let files: Vec<GenerationFile> = captured(harness, state)?
        .into_iter()
        .map(|(path, bytes)| GenerationFile {
            sha256: sha256(&bytes),
            path,
        })
        .collect();
    if files.is_empty() {
        return Ok(None);
    }
    let previous = latest_generation(home, profile)?;
    if previous.as_ref().is_some_and(|last| last.files == files) {
        return Ok(None);
    }
    let root = generations_root(home, profile);
    std::fs::create_dir_all(&root)
        .map_err(|error| format!("could not make {}: {error}", root.display()))?;
    // The claim is `create_dir`, not `create_dir_all`: two adoptions that read the same "last" number race
    // for the same directory, and only one wins it. The loser steps to the next number. A directory that
    // already exists is not ours — it is a write that did not finish, or another adoption's — so writing
    // into it would overwrite the only copy of somebody else's bytes.
    let mut id = previous.as_ref().map(|last| last.id + 1).unwrap_or(1);
    let dir = loop {
        let candidate = generation_dir(home, profile, id);
        match std::fs::create_dir(&candidate) {
            Ok(()) => break candidate,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                id += 1;
                if id > u64::from(u32::MAX) {
                    return Err(format!(
                        "could not find a free generation number under {}: every number up to {id} is taken",
                        root.display()
                    ));
                }
            }
            Err(error) => {
                return Err(format!(
                    "could not claim generation {id} at {}: {error}",
                    candidate.display()
                ));
            }
        }
    };
    // The parent is the newest generation that exists *now*, after the claim: a concurrent adoption that
    // took the number just below ours is the state this one actually replaced, not the one we read first.
    let parent = generations(home, profile)?
        .into_iter()
        .filter(|generation| generation.id < id)
        .map(|generation| generation.id)
        .next_back();
    let generation = Generation {
        id,
        parent,
        harness: Some(absolute(harness).display().to_string()),
        files,
        source_session: source_session.to_string(),
        check: check.to_string(),
        created: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_secs())
            .unwrap_or(0),
    };
    let bytes = captured(harness, state)?;
    for file in &generation.files {
        let Some((_, content)) = bytes.iter().find(|(path, _)| *path == file.path) else {
            return Err(format!("{} was not read twice the same way", file.path));
        };
        let to = dir.join(&file.path);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("could not make {}: {error}", parent.display()))?;
        }
        std::fs::write(&to, content)
            .map_err(|error| format!("could not write {}: {error}", to.display()))?;
    }
    std::fs::write(dir.join(GUARD), format!("{guard}\n"))
        .map_err(|error| format!("could not write the guard of generation {id}: {error}"))?;
    std::fs::write(dir.join(MANIFEST), manifest_text(&generation))
        .map_err(|error| format!("could not write the manifest of generation {id}: {error}"))?;
    Ok(Some(generation))
}

fn manifest_text(generation: &Generation) -> String {
    let files: Vec<serde_json::Value> = generation
        .files
        .iter()
        .map(|file| serde_json::json!({ "path": file.path, "sha256": file.sha256 }))
        .collect();
    let value = serde_json::json!({
        "id": generation.id,
        "parent": generation.parent,
        "harness": generation.harness,
        "files": files,
        "source_session": generation.source_session,
        "check": generation.check,
        "created": generation.created,
    });
    format!(
        "{}\n",
        serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_string())
    )
}

/// Whether an adoption of `source` over `target` would change anything.
///
/// Adopting the same candidate twice is the case this is for: the second adoption is not a second
/// generation, because a history of identical states hides the one change that mattered.
pub fn same(source: &Path, target: &Path) -> bool {
    for name in ["system.md", "tool_guidance.md"] {
        match (
            std::fs::read(source.join(name)),
            std::fs::read(target.join(name)),
        ) {
            (Ok(left), Ok(right)) if left == right => {}
            (Err(left), Err(right))
                if left.kind() == std::io::ErrorKind::NotFound
                    && right.kind() == std::io::ErrorKind::NotFound => {}
            _ => return false,
        }
    }
    true
}

/// Adopt a candidate and record the harness it replaced as the next generation, in one step, so a caller
/// cannot copy without capturing or capture without copying.
#[allow(clippy::too_many_arguments)]
pub fn record_adoption(
    home: &Path,
    profile: &str,
    harness: &Path,
    state: &Path,
    source: &Path,
    source_session: &str,
    check: &str,
    guard: &str,
) -> Result<Option<Generation>, String> {
    if same(source, harness) {
        return Ok(None);
    }
    let generation = capture(home, profile, harness, state, source_session, check, guard)?;
    adopt(harness, source)?;
    Ok(generation)
}

/// The path a generation records and a restore compares: absolute, with symlinks resolved when the path
/// exists. A recorded path and a requested path that name the same directory must compare equal.
fn absolute(path: &Path) -> std::path::PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(path))
                .unwrap_or_else(|_| path.to_path_buf())
        }
    })
}

/// Put the files a generation recorded back where they belong. Only the files the manifest names are
/// touched: a skill added after the generation is left alone, because a rollback of the harness is not a
/// deletion of work done since.
///
/// Verification is a whole pass before the first write: a generation that is tampered with in its last
/// file is refused without any earlier file having been overwritten. A refused rollback changes nothing.
pub fn restore(
    home: &Path,
    profile: &str,
    harness: &Path,
    state: &Path,
    id: u64,
    guard: &str,
) -> Result<Generation, String> {
    let dir = generation_dir(home, profile, id);
    let generation = read_generation(home, profile, id)?.ok_or_else(|| {
        format!(
            "there is no generation {id} for profile `{profile}` under {}",
            generations_root(home, profile).display()
        )
    })?;
    let recorded = std::fs::read_to_string(dir.join(GUARD)).unwrap_or_default();
    if recorded.trim() != guard.trim() {
        return Err(format!(
            "generation {id} was captured under a different floor, policy or set of profile defaults; \
             restoring it would cross a floor change, so it is refused"
        ));
    }
    let keeps_harness = generation
        .files
        .iter()
        .any(|file| file.path.starts_with("harness/"));
    if let Some(recorded) = &generation.harness
        && absolute(Path::new(recorded)) != absolute(harness)
    {
        return Err(format!(
            "generation {id} was captured from {recorded}, and {} is a different harness; restoring \
             it would put those bytes in a tree that never held them, so it is refused",
            harness.display()
        ));
    }
    if keeps_harness && !harness.join("system.md").is_file() {
        return Err(format!(
            "{} has no system.md, so it is not the harness generation {id} was captured from; it is \
             refused",
            harness.display()
        ));
    }
    // Pass one: resolve every destination and check every recorded byte. Nothing is written yet.
    let mut writes: Vec<(std::path::PathBuf, Vec<u8>)> = Vec::new();
    for file in &generation.files {
        let to = destination(harness, state, &file.path)?;
        let from = dir.join(&file.path);
        let bytes = std::fs::read(&from)
            .map_err(|error| format!("generation {id} is missing {}: {error}", file.path))?;
        if sha256(&bytes) != file.sha256 {
            return Err(format!(
                "{} in generation {id} does not match the hash in its manifest, so it is refused rather \
                 than restored",
                file.path
            ));
        }
        writes.push((to, bytes));
    }
    // Pass two: every file verified, so the rollback can begin. Each write goes through a temporary
    // sibling and a rename, so a half-written file is never what a later read sees.
    for (to, bytes) in writes {
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("could not make {}: {error}", parent.display()))?;
        }
        let temporary = to.with_file_name(format!(
            ".{}.restore-{}.tmp",
            to.file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default(),
            std::process::id()
        ));
        std::fs::write(&temporary, bytes)
            .map_err(|error| format!("could not restore {}: {error}", to.display()))?;
        std::fs::rename(&temporary, &to).map_err(|error| {
            let _ = std::fs::remove_file(&temporary);
            format!("could not restore {}: {error}", to.display())
        })?;
    }
    Ok(generation)
}

/// Where a manifest path belongs: `harness/...` under the harness directory, `skills/...` under the state's
/// skills directory, and nothing else. A manifest is data, and data that can name `../` is a write outside
/// the two trees the path is allowed to touch.
fn destination(harness: &Path, state: &Path, path: &str) -> Result<std::path::PathBuf, String> {
    let (area, rest) = path.split_once('/').ok_or_else(|| {
        format!("`{path}` names no tree, so it is outside what a generation may hold")
    })?;
    if rest.is_empty()
        || rest
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(format!(
            "`{path}` is outside the harness and skills trees, so it is refused"
        ));
    }
    let base = match area {
        "harness" => harness,
        "skills" => &state.join("skills"),
        other => {
            return Err(format!(
                "`{other}/` is outside the harness and skills trees, so it is refused"
            ));
        }
    };
    Ok(base.join(rest))
}

/// The text the guard digest is made of: the floor's five actions, the policy's rulings, and every shipped
/// profile default. One string so that a person can read what a digest actually covers.
pub fn guard_text() -> String {
    let mut text = String::new();
    for category in GUARDED_CATEGORIES {
        text.push_str(&format!(
            "floor {} {}\n",
            category.action(),
            category.phrase()
        ));
    }
    for name in crate::policy::shipped() {
        let Some(policy) = crate::policy::named(name) else {
            continue;
        };
        for action in PROBED {
            let ruling = policy.decide(action);
            text.push_str(&format!(
                "policy {name} {action} {} {}\n",
                ruling.decision().name(),
                ruling.reason()
            ));
        }
    }
    text.push_str(&crate::profile::parameter_text());
    text
}

/// A digest of [`guard_text`]. Recorded with every generation and compared on every restore.
pub fn guard_digest() -> String {
    crate::skills::fingerprint(&guard_text())
}

/// Refuse a candidate that edits what this path may not touch.
///
/// The prompt is a sentence and the floor is code, so a candidate that quotes the floor's own vocabulary is
/// refused rather than argued with: the only thing such text can do is move a permission that the floor
/// will still enforce, which produces a harness that lies about itself. The check is refused for the
/// reason the editor never saw it: a prompt written against the check is a prompt for the grader. A
/// file named after a guardrail is refused because `harness_files` reads two names today and must never
/// learn a third.
pub fn refuses(source: &Path, check: &str) -> Result<(), String> {
    const RESERVED: [&str; 4] = ["floor.md", "policy.md", "check.md", "guardrails.md"];
    let mut forbidden: Vec<(&str, String)> = Vec::new();
    if !check.trim().is_empty() {
        forbidden.push(("the check", check.trim().to_string()));
    }
    for category in GUARDED_CATEGORIES {
        forbidden.push(("the floor", category.action().to_string()));
    }
    for name in crate::policy::shipped() {
        let Some(policy) = crate::policy::named(name) else {
            continue;
        };
        for action in PROBED {
            forbidden.push(("the policy", policy.decide(action).reason().to_string()));
        }
    }
    forbidden.dedup_by(|left, right| left.1 == right.1);
    let forbidden: Vec<(&str, String)> = forbidden
        .into_iter()
        .map(|(what, phrase)| (what, phrase.to_lowercase()))
        .collect();

    let mut files = Vec::new();
    walk(source, 0, &mut files);
    files.sort();
    for path in files {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        if RESERVED.contains(&name.as_str()) {
            return Err(format!(
                "`{name}` is a guardrail, and a candidate may edit a prompt, a skill description or a \
                 subagent spec and nothing else"
            ));
        }
        // Case-insensitive: a candidate that writes "This policy asks ..." is quoting the policy, and the
        // difference between a capital and a lower-case T is not a difference a guard should be argued with.
        let text =
            String::from_utf8_lossy(&std::fs::read(&path).unwrap_or_default()).to_lowercase();
        for (what, phrase) in &forbidden {
            if !phrase.is_empty() && text.contains(phrase) {
                let shown = path
                    .strip_prefix(source)
                    .map(|relative| relative.display().to_string())
                    .unwrap_or_else(|_| path.display().to_string());
                return Err(format!(
                    "`{shown}` quotes {what} (`{phrase}`), and the floor, the policy and the check are \
                     not text a candidate may edit"
                ));
            }
        }
    }
    Ok(())
}

fn walk(dir: &Path, depth: usize, found: &mut Vec<std::path::PathBuf>) {
    if depth > 8 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, depth + 1, found);
        } else if path.is_file() {
            found.push(path);
        }
    }
}

/// A directory name from a goal: lowercase words, hyphens, at most 40 characters.
pub fn slug(goal: &str) -> String {
    let mut slug = String::new();
    for word in goal.split_whitespace() {
        let word: String = word
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        if word.is_empty() {
            continue;
        }
        if !slug.is_empty() {
            slug.push('-');
        }
        slug.push_str(&word);
        if slug.len() >= 40 {
            break;
        }
    }
    if slug.is_empty() {
        "tune".to_string()
    } else {
        slug.truncate(40);
        slug.trim_end_matches('-').to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn log(name: &str, body: &str) -> PathBuf {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../target/scratch")
            .join(format!("hades-tune-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch");
        let path = dir.join("session.jsonl");
        std::fs::write(&path, body).expect("log");
        path
    }

    const RUN: &str = concat!(
        r#"{"kind":"tool_call","payload":{"tool":"shell","program":"echo hi","model":{"millis":1000}}}"#,
        "\n",
        r#"{"kind":"tool_result","payload":{"tool":"shell","output":"hi\n","state":"ok"}}"#,
        "\n",
        r#"{"kind":"tool_call","payload":{"tool":"verify","program":"grep -q 42 out.txt"}}"#,
        "\n",
        r#"{"kind":"tool_result","payload":{"tool":"verify","output":"exit 0\n","state":"passed"}}"#,
        "\n",
    );

    #[test]
    fn a_receipt_reads_answers_cost_and_the_check() {
        let path = log("receipt", RUN);
        let receipt = receipt(&path);
        assert!(receipt.passed);
        assert_eq!(receipt.answers, 1);
        assert_eq!(receipt.millis, 1000);
        assert_eq!(receipt.check, "passed");
        assert!(receipt.answer_bytes > 0);
        assert!(!receipt.awaiting);
    }

    #[test]
    fn a_probe_that_stopped_at_a_question_is_marked_awaiting_not_failed() {
        const PARKED: &str = concat!(
            r#"{"kind":"tool_call","payload":{"tool":"shell","program":"rm -rf build","model":{"millis":10}}}"#,
            "\n",
            r#"{"kind":"approval_request","payload":{"action":"permanently delete","reason":"the hard floor"}}"#,
            "\n",
            r#"{"kind":"checkpoint","payload":{"node":"awaiting","condition":"Asked","attempt":1}}"#,
            "\n",
        );
        let path = log("parked", PARKED);
        let receipt = receipt(&path);
        assert!(receipt.awaiting, "{receipt:?}");
        assert!(!receipt.passed);
        assert_eq!(receipt.answers, 1);
        assert!(receipt.check.is_empty(), "no verify ran");
    }

    #[test]
    fn the_transcript_withholds_the_check_and_redacts_it() {
        let path = log("transcript", RUN);
        let said = transcript(&path, "grep -q 42 out.txt", 4000);
        assert!(said.contains("echo hi"), "{said}");
        assert!(!said.contains("grep -q 42"), "{said}");
        assert!(said.contains("[the check ran"), "{said}");
        assert!(said.contains("[check: passed]"), "{said}");
    }

    #[test]
    fn redaction_is_exact_and_harmless_when_empty() {
        assert_eq!(redact("run a; then b", "a"), "run <the check>; then b");
        assert_eq!(redact("nothing", ""), "nothing");
    }

    fn row(name: &str, passed: bool, millis: u64, answers: usize) -> Row {
        Row {
            name: name.to_string(),
            receipt: Receipt {
                passed,
                answers,
                millis,
                answer_bytes: 10,
                check: if passed {
                    "passed".into()
                } else {
                    "failed".into()
                },
                awaiting: false,
                progress: None,
                holdout: None,
            },
            note: String::new(),
        }
    }

    #[test]
    fn a_pass_beats_a_faster_failure_and_a_cheaper_pass_wins_between_passes() {
        let base = row("base", false, 100, 1);
        let inv = row("invariant", true, 900, 2);
        let dom = row("domain", false, 50, 1);
        let rows = vec![base.clone(), inv.clone(), dom];
        assert_eq!(choose(&rows), Some(1));
        assert!(beats_base(&base, &inv));

        let base = row("base", true, 900, 2);
        let cheap = row("invariant", true, 800, 1);
        assert!(beats_base(&base, &cheap));
        let same = row("invariant", true, 900, 2);
        assert!(!beats_base(&base, &same));
    }

    #[test]
    fn the_experience_note_carries_the_table_and_the_lesson() {
        let rows = vec![row("base", true, 900, 2), row("invariant", true, 800, 1)];
        let note = experience(
            "build stats.sh",
            "test -f stats.sh",
            &rows,
            Some("invariant"),
            "say where to write",
            &[("invariant".into(), "/tmp/inv".into())],
        );
        assert!(note.contains("| base | passed | 2 | 900 |"), "{note}");
        assert!(note.contains("adopted: `invariant`"), "{note}");
        assert!(note.contains("say where to write"), "{note}");
    }

    #[test]
    fn a_candidate_is_one_json_object_and_may_not_name_the_check() {
        let good = r#"```json
{"reason":"needs a plan","system":"Plan first.","guidance":"","lesson":"say it"}
```"#;
        let (system, guidance, reason, _) =
            candidate_from(good, "grep -q 42 out.txt").expect("parsed");
        assert_eq!(system, "Plan first.");
        assert!(guidance.is_empty());
        assert_eq!(reason, "needs a plan");

        let leak = r#"{"reason":"x","system":"Run grep -q 42 out.txt","guidance":""}"#;
        let said = candidate_from(leak, "grep -q 42 out.txt").expect_err("refused");
        assert!(said.contains("wrote the check"), "{said}");

        let empty = r#"{"reason":"x","system":"   ","guidance":""}"#;
        assert!(candidate_from(empty, "").is_err());
        assert!(candidate_from("not json", "").is_err());
    }

    #[test]
    fn adoption_copies_the_prompt_and_drops_stale_guidance() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../target/scratch")
            .join(format!("hades-adopt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let source = root.join("source");
        let target = root.join("target");
        std::fs::create_dir_all(&source).expect("source");
        std::fs::create_dir_all(&target).expect("target");
        std::fs::write(source.join("system.md"), "new prompt").expect("system");
        std::fs::write(target.join("system.md"), "old prompt").expect("old system");
        std::fs::write(target.join("tool_guidance.md"), "stale").expect("stale");
        adopt(&target, &source).expect("adopted");
        assert_eq!(
            std::fs::read_to_string(target.join("system.md")).expect("read"),
            "new prompt"
        );
        assert!(
            !target.join("tool_guidance.md").exists(),
            "stale guidance survived"
        );
        std::fs::write(source.join("tool_guidance.md"), "new guidance").expect("guidance");
        adopt(&target, &source).expect("adopted again");
        assert_eq!(
            std::fs::read_to_string(target.join("tool_guidance.md")).expect("read"),
            "new guidance"
        );
    }

    #[test]
    fn a_slug_is_short_and_readable() {
        assert_eq!(
            slug("Build stats.sh, then run it!"),
            "build-statssh-then-run-it"
        );
        assert_eq!(slug("   "), "tune");
    }
}

#[cfg(test)]
mod version_tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../target/scratch")
            .join(format!("versions-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        dir
    }

    #[test]
    fn an_adoption_keeps_the_prompt_it_replaces() {
        let target = scratch("target");
        std::fs::write(target.join("system.md"), "old prompt").expect("the old prompt");
        std::fs::write(target.join("tool_guidance.md"), "old guidance").expect("the old guidance");
        let kept = keep_version(&target)
            .expect("no error")
            .expect("something to keep");
        assert!(kept.starts_with(target.join("versions")));
        assert_eq!(
            std::fs::read_to_string(kept.join("system.md")).expect("the kept prompt"),
            "old prompt"
        );
    }

    #[test]
    fn a_target_with_nothing_to_keep_says_so() {
        let target = scratch("empty");
        assert!(keep_version(&target).expect("no error").is_none());
    }
}

#[cfg(test)]
mod generation_tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn place(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../target/scratch")
            .join(format!("generations-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch home");
        dir
    }

    /// A home with a harness directory and one installed skill: the three files a generation must name.
    fn seeded(home: &Path) -> PathBuf {
        let harness = home.join("harness").join("fix");
        let skill = home.join("skills").join("alpha");
        std::fs::create_dir_all(&harness).expect("the harness directory");
        std::fs::create_dir_all(&skill).expect("the skill directory");
        std::fs::write(harness.join("system.md"), "first prompt").expect("the prompt");
        std::fs::write(harness.join("tool_guidance.md"), "read before writing").expect("guidance");
        std::fs::write(
            skill.join("SKILL.md"),
            "---\ndescription: alpha\n---\n\nDo the alpha thing.\n",
        )
        .expect("the skill");
        harness
    }

    fn manifest(home: &Path, profile: &str, id: u64) -> serde_json::Value {
        let path = generation_dir(home, profile, id).join(MANIFEST);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        serde_json::from_str(&text).expect("the manifest is JSON")
    }

    fn digest_of(generation: &Generation, path: &str) -> String {
        generation
            .files
            .iter()
            .find(|file| file.path == path)
            .unwrap_or_else(|| panic!("no {path} in {generation:?}"))
            .sha256
            .clone()
    }

    #[test]
    fn every_adoption_is_a_numbered_generation_that_names_each_file() {
        let home = place("numbering");
        let harness = seeded(&home);
        let first = capture(
            &home,
            "fix",
            &harness,
            &home,
            "session-1",
            "passed",
            "guard-a",
        )
        .expect("the first capture")
        .expect("something to keep");
        assert_eq!(first.id, 1);
        assert_eq!(first.parent, None, "the first generation has no parent");
        let paths: Vec<&str> = first.files.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "harness/system.md",
                "harness/tool_guidance.md",
                "skills/alpha/SKILL.md"
            ],
            "a generation names the harness files and every skill"
        );
        for file in &first.files {
            assert_eq!(file.sha256.len(), 64, "{file:?}");
            assert!(
                file.sha256.chars().all(|c| c.is_ascii_hexdigit()),
                "{file:?}"
            );
        }
        assert_eq!(
            std::fs::read_to_string(
                generation_dir(&home, "fix", 1)
                    .join("skills")
                    .join("alpha")
                    .join("SKILL.md")
            )
            .expect("the captured skill"),
            "---\ndescription: alpha\n---\n\nDo the alpha thing.\n",
            "what a generation records is the bytes, not the name"
        );

        std::fs::write(harness.join("system.md"), "second prompt").expect("the new prompt");
        let second = capture(
            &home,
            "fix",
            &harness,
            &home,
            "session-2",
            "passed",
            "guard-a",
        )
        .expect("the second capture")
        .expect("something to keep");
        assert_eq!(second.id, 2);
        assert_eq!(
            second.parent,
            Some(1),
            "each generation points at the one before"
        );
        assert_ne!(
            digest_of(&first, "harness/system.md"),
            digest_of(&second, "harness/system.md"),
            "a changed prompt is a different hash"
        );

        let recorded = manifest(&home, "fix", 2);
        assert_eq!(recorded["id"], 2);
        assert_eq!(recorded["parent"], 1);
        assert_eq!(recorded["source_session"], "session-2");
        assert_eq!(recorded["check"], "passed");
        assert!(recorded["created"].as_u64().is_some(), "{recorded}");
        let named: Vec<&str> = recorded["files"]
            .as_array()
            .expect("a file list")
            .iter()
            .map(|file| file["path"].as_str().expect("a path"))
            .collect();
        assert_eq!(named.len(), 3, "{recorded}");
        assert!(named.contains(&"skills/alpha/SKILL.md"), "{recorded}");

        // The same files again is not a new generation: history is changes, not captures.
        assert!(
            capture(
                &home,
                "fix",
                &harness,
                &home,
                "session-3",
                "passed",
                "guard-a"
            )
            .expect("a capture")
            .is_none()
        );
        assert_eq!(generations(&home, "fix").expect("the list").len(), 2);
    }

    #[test]
    fn adopting_the_same_candidate_twice_writes_one_generation_and_keeps_the_skill() {
        let home = place("idempotent");
        let harness = seeded(&home);
        let source = home.join("candidate");
        std::fs::create_dir_all(&source).expect("the candidate");
        std::fs::write(source.join("system.md"), "first prompt").expect("the same prompt");
        std::fs::write(source.join("tool_guidance.md"), "read before writing")
            .expect("the same guidance");

        // The candidate is what the harness already holds: an adoption with nothing to do.
        assert!(
            record_adoption(
                &home,
                "fix",
                &harness,
                &home,
                &source,
                "session-1",
                "passed",
                "guard-a"
            )
            .expect("an adoption")
            .is_none()
        );
        assert!(
            generations(&home, "fix").expect("the list").is_empty(),
            "a no-op adoption is not a generation"
        );

        std::fs::write(source.join("system.md"), "adopted prompt").expect("a new prompt");
        let first = record_adoption(
            &home,
            "fix",
            &harness,
            &home,
            &source,
            "session-2",
            "passed",
            "guard-a",
        )
        .expect("an adoption")
        .expect("a generation");
        assert_eq!(first.id, 1);
        assert_eq!(
            std::fs::read_to_string(harness.join("system.md")).expect("the adopted prompt"),
            "adopted prompt"
        );

        let again = record_adoption(
            &home,
            "fix",
            &harness,
            &home,
            &source,
            "session-3",
            "passed",
            "guard-a",
        )
        .expect("an adoption");
        assert!(again.is_none(), "the second adoption changes nothing");
        assert_eq!(
            generations(&home, "fix").expect("the list").len(),
            1,
            "an adoption that changes nothing is not a second generation"
        );
        assert!(
            first
                .files
                .iter()
                .any(|file| file.path == "skills/alpha/SKILL.md"),
            "the skill is part of what a generation keeps: {first:?}"
        );
    }

    #[test]
    fn a_skill_description_edit_round_trips_and_a_floor_change_is_refused() {
        let home = place("rollback");
        let harness = seeded(&home);
        let skill = home.join("skills").join("alpha").join("SKILL.md");
        let before = std::fs::read(&skill).expect("the skill before the edit");
        let source = home.join("candidate");
        std::fs::create_dir_all(&source).expect("the candidate");
        std::fs::write(source.join("system.md"), "adopted prompt").expect("the prompt");
        let generation = record_adoption(
            &home,
            "fix",
            &harness,
            &home,
            &source,
            "session-1",
            "passed",
            "guard-a",
        )
        .expect("an adoption")
        .expect("a generation");

        std::fs::write(
            &skill,
            "---\ndescription: alpha, revised\n---\n\nDo it twice.\n",
        )
        .expect("the edited skill");

        let restored =
            restore(&home, "fix", &harness, &home, generation.id, "guard-a").expect("a rollback");
        assert_eq!(restored.id, 1);
        assert_eq!(
            std::fs::read(&skill).expect("the restored skill"),
            before,
            "the skill description comes back byte for byte"
        );

        let refused = restore(&home, "fix", &harness, &home, 1, "guard-b")
            .expect_err("a generation from another floor is refused");
        assert!(refused.contains("floor"), "{refused}");
        assert_eq!(
            std::fs::read(&skill).expect("the skill after the refusal"),
            before,
            "a refused rollback touches nothing"
        );
    }

    #[test]
    fn a_generation_whose_recorded_file_was_edited_is_refused() {
        let home = place("tampered");
        let harness = seeded(&home);
        capture(
            &home,
            "fix",
            &harness,
            &home,
            "session-1",
            "passed",
            "guard-a",
        )
        .expect("a capture");
        std::fs::write(
            generation_dir(&home, "fix", 1)
                .join("harness")
                .join("system.md"),
            "not what was recorded",
        )
        .expect("a tampered file");
        let refused = restore(&home, "fix", &harness, &home, 1, "guard-a")
            .expect_err("a tampered generation is refused");
        assert!(refused.contains("does not match"), "{refused}");
    }

    #[test]
    fn a_manifest_may_not_name_a_path_outside_the_two_trees() {
        let home = place("escape");
        let harness = seeded(&home);
        capture(
            &home,
            "fix",
            &harness,
            &home,
            "session-1",
            "passed",
            "guard-a",
        )
        .expect("a capture");
        let path = generation_dir(&home, "fix", 1).join(MANIFEST);
        std::fs::write(
            &path,
            r#"{"id":1,"parent":null,"files":[{"path":"harness/../../escaped.md","sha256":"00"}],"source_session":"s","check":"passed","created":1}"#,
        )
        .expect("a hand-edited manifest");
        let refused = restore(&home, "fix", &harness, &home, 1, "guard-a").expect_err("refused");
        assert!(refused.contains("outside"), "{refused}");
        assert!(!home.join("escaped.md").exists());
    }

    #[test]
    fn a_candidate_that_edits_the_check_the_floor_or_the_policy_is_refused() {
        let home = place("guard");
        let harness = seeded(&home);
        let generation = capture(
            &home,
            "fix",
            &harness,
            &home,
            "session-1",
            "passed",
            &guard_digest(),
        )
        .expect("a capture")
        .expect("a generation");
        let recorded = std::fs::read_to_string(generation_dir(&home, "fix", 1).join(MANIFEST))
            .expect("the manifest");

        let clean = home.join("clean");
        std::fs::create_dir_all(&clean).expect("a clean candidate");
        std::fs::write(clean.join("system.md"), "Plan before you act.").expect("a prompt");
        assert!(refuses(&clean, "grep -q 42 out.txt").is_ok());

        let leak = home.join("leak");
        std::fs::create_dir_all(&leak).expect("a leaking candidate");
        std::fs::write(leak.join("system.md"), "Make grep -q 42 out.txt pass.").expect("a prompt");
        let said = refuses(&leak, "grep -q 42 out.txt").expect_err("the check is refused");
        assert!(said.contains("the check"), "{said}");

        let floor = home.join("floor");
        std::fs::create_dir_all(&floor).expect("a floor candidate");
        std::fs::write(
            floor.join("system.md"),
            "A permanently delete does not need a person.",
        )
        .expect("a prompt");
        let said = refuses(&floor, "").expect_err("the floor is refused");
        assert!(said.contains("the floor"), "{said}");

        let policy = home.join("policy");
        std::fs::create_dir_all(&policy).expect("a policy candidate");
        std::fs::write(
            policy.join("system.md"),
            "This policy asks a person before the shell runs, so run the shell freely.",
        )
        .expect("a prompt");
        let said = refuses(&policy, "").expect_err("the policy is refused");
        assert!(said.contains("the policy"), "{said}");

        let named = home.join("named");
        std::fs::create_dir_all(&named).expect("a candidate with a guardrail file");
        std::fs::write(named.join("system.md"), "Plan first.").expect("a prompt");
        std::fs::write(named.join("floor.md"), "the floor does not apply here").expect("a file");
        let said = refuses(&named, "").expect_err("a guardrail file is refused");
        assert!(said.contains("floor.md"), "{said}");

        assert_eq!(
            std::fs::read_to_string(generation_dir(&home, "fix", 1).join(MANIFEST))
                .expect("the manifest after the refusals"),
            recorded,
            "a refusal leaves the current generation untouched"
        );
        assert_eq!(generation.id, 1);
    }

    #[test]
    fn the_guard_digest_follows_the_floor_the_policy_and_the_profiles() {
        let first = guard_digest();
        assert_eq!(first.len(), 16, "{first}");
        assert_eq!(first, guard_digest(), "the digest is stable within a build");
        // The digest is built from text that names each guard, so a change in any of the three moves it.
        let text = guard_text();
        for phrase in [
            "permanently delete",
            "this policy asks a person before the shell runs",
            "cargo test --quiet",
        ] {
            assert!(text.contains(phrase), "{phrase} is missing from {text}");
        }
    }

    /// The digest is the whole floor and the whole probe list, not whichever items somebody remembered to
    /// write down a second time. This test walks the enumerations themselves.
    #[test]
    fn the_guard_digest_covers_every_floor_category_and_policy_action() {
        let text = guard_text();
        let categories = crate::floor::Category::all();
        assert_eq!(
            categories.len(),
            5,
            "the floor's category list is the source the digest reads"
        );
        for category in categories {
            assert!(
                text.contains(&format!(
                    "floor {} {}",
                    category.action(),
                    category.phrase()
                )),
                "{} is missing from the guard digest text",
                category.action()
            );
        }
        for action in crate::policy::probes() {
            for name in crate::policy::shipped() {
                let policy = crate::policy::named(name).expect("a shipped policy");
                let ruling = policy.decide(action);
                assert!(
                    text.contains(&format!(
                        "policy {name} {action} {} {}",
                        ruling.decision().name(),
                        ruling.reason()
                    )),
                    "policy {name} on {action} is missing from the guard digest text"
                );
            }
        }
    }

    #[test]
    fn a_refused_rollback_writes_none_of_the_files_it_verified_first() {
        let home = place("atomic");
        let harness = seeded(&home);
        let skill = home.join("skills").join("alpha").join("SKILL.md");
        let generation = capture(
            &home,
            "fix",
            &harness,
            &home,
            "session-1",
            "passed",
            "guard-a",
        )
        .expect("a capture")
        .expect("a generation");
        // What the adoption did after the capture: both files now differ from the generation.
        std::fs::write(harness.join("system.md"), "adopted prompt").expect("the adopted prompt");
        std::fs::write(&skill, "adopted skill").expect("the adopted skill");

        // The tampered file sorts last, so a one-pass restore would already have overwritten the prompt.
        let last = generation
            .files
            .last()
            .expect("a generation names files")
            .path
            .clone();
        assert_eq!(
            last, "skills/alpha/SKILL.md",
            "the test tampers with the last file the manifest names"
        );
        std::fs::write(generation_dir(&home, "fix", 1).join(&last), "tampered")
            .expect("a tampered generation");

        let refused = restore(&home, "fix", &harness, &home, 1, "guard-a")
            .expect_err("a tampered generation is refused");
        assert!(refused.contains("does not match"), "{refused}");
        assert_eq!(
            std::fs::read_to_string(harness.join("system.md")).expect("the prompt"),
            "adopted prompt",
            "a refusal must not restore the files it verified before the tampered one"
        );
        assert_eq!(
            std::fs::read_to_string(&skill).expect("the skill"),
            "adopted skill",
            "a refusal touches nothing"
        );
    }

    #[test]
    fn a_pre_created_generation_number_is_stepped_over_not_written_into() {
        let home = place("claimed");
        let harness = seeded(&home);
        let first = capture(
            &home,
            "fix",
            &harness,
            &home,
            "session-1",
            "passed",
            "guard-a",
        )
        .expect("a capture")
        .expect("a generation");
        assert_eq!(first.id, 1);

        // A number that already exists belongs to somebody else: a crashed write or another adoption.
        let taken = generation_dir(&home, "fix", 2);
        std::fs::create_dir_all(&taken).expect("a pre-created number");
        std::fs::write(taken.join("evidence.txt"), "not ours").expect("the other write's bytes");

        std::fs::write(harness.join("system.md"), "second prompt").expect("a changed prompt");
        let second = capture(
            &home,
            "fix",
            &harness,
            &home,
            "session-2",
            "passed",
            "guard-a",
        )
        .expect("a capture")
        .expect("a generation");
        assert_eq!(second.id, 3, "the taken number is skipped");
        assert_eq!(
            second.parent,
            Some(1),
            "the chain points at the state it replaced"
        );
        assert!(
            !taken.join(MANIFEST).exists(),
            "a pre-created number is not adopted as this generation"
        );
        assert_eq!(
            std::fs::read_to_string(taken.join("evidence.txt")).expect("the other write's bytes"),
            "not ours",
            "the other write is left alone"
        );
        assert!(generation_dir(&home, "fix", 3).join(MANIFEST).is_file());
    }

    #[test]
    fn a_rollback_into_a_different_harness_is_refused_and_writes_nothing() {
        let home = place("wrong-harness");
        let harness = seeded(&home);
        capture(
            &home,
            "fix",
            &harness,
            &home,
            "session-1",
            "passed",
            "guard-a",
        )
        .expect("a capture")
        .expect("a generation");
        std::fs::write(harness.join("system.md"), "adopted prompt").expect("the adopted prompt");

        let other = home.join("harness").join("other");
        std::fs::create_dir_all(&other).expect("a different harness");
        std::fs::write(other.join("system.md"), "somebody else's prompt")
            .expect("the other prompt");
        let refused = restore(&home, "fix", &other, &home, 1, "guard-a")
            .expect_err("the wrong harness is refused");
        assert!(refused.contains("different harness"), "{refused}");
        assert_eq!(
            std::fs::read_to_string(other.join("system.md")).expect("the other prompt"),
            "somebody else's prompt",
            "a refused rollback writes nothing into the wrong tree"
        );
        assert_eq!(
            std::fs::read_to_string(harness.join("system.md")).expect("the adopted prompt"),
            "adopted prompt",
            "a refused rollback leaves the right tree alone too"
        );

        // A generation from before the path was recorded falls back to refusing a tree with no prompt.
        let empty = home.join("harness").join("empty");
        std::fs::create_dir_all(&empty).expect("an empty directory");
        let path = generation_dir(&home, "fix", 1).join(MANIFEST);
        let mut recorded: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("the manifest"))
                .expect("a manifest");
        recorded["harness"] = serde_json::Value::from(empty.display().to_string());
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&recorded).expect("json"),
        )
        .expect("a manifest without a matching harness");
        let refused = restore(&home, "fix", &empty, &home, 1, "guard-a")
            .expect_err("a directory with no system.md is refused");
        assert!(refused.contains("no system.md"), "{refused}");
        assert!(
            std::fs::read_dir(&empty)
                .expect("the empty directory")
                .next()
                .is_none(),
            "a refused rollback left files in the empty directory"
        );
    }
}

#[cfg(test)]
mod progress_tests {
    use super::*;

    fn row_with(
        name: &str,
        passed: bool,
        millis: u64,
        progress: Option<(u32, u32)>,
        holdout: Option<bool>,
    ) -> Row {
        Row {
            name: name.to_string(),
            receipt: Receipt {
                passed,
                answers: 1,
                millis,
                answer_bytes: 10,
                check: if passed {
                    "passed".into()
                } else {
                    "failed".into()
                },
                awaiting: false,
                progress: progress.map(|(passed, total)| Progress {
                    gate: false,
                    passed,
                    total,
                    family: None,
                }),
                holdout,
            },
            note: String::new(),
        }
    }

    #[test]
    fn a_failure_that_got_further_is_the_better_row() {
        let base = row_with("base", false, 10, Some((1, 5)), None);
        let near = row_with("near", false, 900, Some((4, 5)), None);
        let quick = row_with("quick", false, 50, Some((1, 5)), None);
        let rows = vec![base, near, quick];
        assert_eq!(choose(&rows), Some(1), "4/5 beats 1/5 even when slower");
    }

    #[test]
    fn a_failed_holdout_is_never_adopted() {
        let base = row_with("base", true, 100, None, Some(true));
        let candidate = row_with("candidate", true, 50, None, Some(false));
        assert!(!beats_base(&base, &candidate));
    }

    #[test]
    fn a_passed_holdout_breaks_a_tie_on_cost() {
        let base = row_with("base", true, 100, None, Some(false));
        let candidate = row_with("candidate", true, 500, None, Some(true));
        assert!(beats_base(&base, &candidate));
    }
}
