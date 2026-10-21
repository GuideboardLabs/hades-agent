//! Proposals from finished work: the half of the learning loop that turns a run into a procedure.
//!
//! A proposal is written when a run was accepted but not on its first attempt, because that is the case
//! where the transcript contains something worth keeping: the program that finally worked and the check it
//! satisfied. It is never auto-loaded. It lands under `<state>/skills/proposed/` and only `hades skills
//! promote` moves it into the directory the agent reads, because a skill is an instruction and an
//! instruction that appears without review is the failure this whole system is built to avoid.

use std::path::{Path, PathBuf};

/// A stable, non-reversible fingerprint of a check command. The text is never stored in a proposal: a
/// promoted skill that carries the verifier teaches the next run how to satisfy the grader instead of the task.
pub fn fingerprint(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

pub fn proposed_dir(state: &Path) -> PathBuf {
    state.join("skills").join("proposed")
}

pub fn installed_dir(state: &Path) -> PathBuf {
    state.join("skills")
}

/// One proposal or installed skill: its slug and its file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub path: PathBuf,
}

/// Write a proposal for an accepted run that took more than one attempt. Returns the path, or `None` when
/// there is nothing to propose (a first-attempt pass) or the name is already taken, because overwriting a
/// proposal would hide the earlier evidence.
pub fn propose(
    state: &Path,
    profile: &str,
    goal: &str,
    check: &str,
    program: &str,
    session: &str,
    attempts: u32,
) -> Result<Option<PathBuf>, String> {
    if attempts < 2 {
        return Ok(None);
    }
    let name = mneme::slugify(goal, 6);
    if name.is_empty() {
        return Ok(None);
    }
    let dir = proposed_dir(state);
    crate::scratch::state_dir(&dir)
        .map_err(|error| format!("could not make {}: {error}", dir.display()))?;
    let path = dir.join(format!("{name}.md"));
    if path.exists() {
        return Ok(None);
    }
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0);
    let text = format!(
        "---\nname: {name}\nstatus: proposed\nprofile: {profile}\ncheck_fingerprint: {fingerprint}\nsession: {session}\nattempts: {attempts}\ncontract: TODO\nregression: []\nholdout: []\nversion: 1\nat: {at}\n---\n\n# {goal}\n\n## When to use\n\n{goal}\n\n## What worked\n\n```sh\n{program}\n```\n\n## How it was checked\n\nThe run's check passed. Its command is withheld from this proposal; `check_fingerprint` records which check it was. Fill in `contract` with inputs, outputs, scope and failure modes, name at least two regression cases and one holdout in `regression:` and `holdout:`, then run `hades skills verify {name}`.\n\nPromote this with `hades skills promote {name}` only after reading it.\n",
        goal = goal.trim(),
        program = program.trim(),
        fingerprint = fingerprint(check),
    );
    std::fs::write(&path, text)
        .map_err(|error| format!("could not write {}: {error}", path.display()))?;
    Ok(Some(path))
}

/// A proposal whose body is a runbook that has already passed a fresh replay. The check command is not in
/// the file; only its fingerprint is, and the replay session is named so a person can read the evidence.
#[allow(clippy::too_many_arguments)]
pub fn propose_runbook(
    state: &Path,
    name: &str,
    summary: &str,
    runbook: &str,
    session: &str,
    replay: &str,
    profile: &str,
    check: &str,
) -> Result<PathBuf, String> {
    let dir = proposed_dir(state);
    crate::scratch::state_dir(&dir)
        .map_err(|error| format!("could not make {}: {error}", dir.display()))?;
    let path = dir.join(format!("{name}.md"));
    if path.exists() {
        return Err(format!("a proposal called `{name}` already exists"));
    }
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0);
    let text = format!(
        "---\nname: {name}\nstatus: proposed\nprofile: {profile}\ncheck_fingerprint: {fingerprint}\nsession: {session}\nreplay: {replay}\ncontract: TODO\nregression: []\nholdout: []\nversion: 1\nat: {at}\nsummary: {summary}\n---\n\n# {name}\n\n{runbook}\n\nFill in `contract`, `regression:` and `holdout:` before promotion, then run `hades skills verify {name}`.\n",
        fingerprint = fingerprint(check),
        summary = summary.trim(),
        runbook = runbook.trim(),
    );
    std::fs::write(&path, text)
        .map_err(|error| format!("could not write {}: {error}", path.display()))?;
    Ok(path)
}

/// Every skill file in a directory, sorted by name.
pub fn list_in(dir: &Path) -> Vec<Skill> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<Skill> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|kind| kind == "md"))
        .filter_map(|path| {
            Some(Skill {
                name: path.file_stem()?.to_string_lossy().to_string(),
                path,
            })
        })
        .collect();
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

pub fn proposed(state: &Path) -> Vec<Skill> {
    let dir = proposed_dir(state);
    let mut found = directories_with_skill(&dir);
    found.extend(list_in(&dir));
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found.dedup_by(|a, b| a.name == b.name);
    found
}

/// The file a proposal is read from. A flat proposal is `<name>.md`; a directory proposal is
/// `<name>/SKILL.md`. Both existing at once is ambiguous and refused rather than silently picked.
pub fn proposal_path(state: &Path, name: &str) -> Result<PathBuf, String> {
    let flat = proposed_dir(state).join(format!("{name}.md"));
    let tree = proposed_dir(state).join(name).join("SKILL.md");
    match (flat.is_file(), tree.is_file()) {
        (true, false) => Ok(flat),
        (false, true) => Ok(tree),
        (true, true) => Err(format!(
            "there are two proposals called `{name}`: {} and {}",
            flat.display(),
            tree.display()
        )),
        (false, false) => Err(format!("there is no proposal called `{name}`")),
    }
}

/// The directory a directory-shaped proposal owns, or `None` for a flat proposal.
pub fn proposal_tree(state: &Path, name: &str) -> Option<PathBuf> {
    let dir = proposed_dir(state).join(name);
    (dir.join("SKILL.md").is_file()).then_some(dir)
}

pub fn proposal_text(state: &Path, name: &str) -> Result<String, String> {
    let path = proposal_path(state, name)?;
    std::fs::read_to_string(&path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))
}

/// Copy the support files of a proposal into an installed directory. `SKILL.md` is not copied; the caller
/// writes the active text it already has. A flat proposal has no support files.
pub fn copy_proposal_support(state: &Path, name: &str, to: &Path) -> Result<(), String> {
    let Some(from) = proposal_tree(state, name) else {
        return Ok(());
    };
    copy_support_tree(&from, to, Some("SKILL.md"))
}

fn copy_support_tree(from: &Path, to: &Path, skip: Option<&str>) -> Result<(), String> {
    std::fs::create_dir_all(to)
        .map_err(|error| format!("could not make {}: {error}", to.display()))?;
    let mut entries: Vec<PathBuf> = std::fs::read_dir(from)
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
        let meta = std::fs::symlink_metadata(&path)
            .map_err(|error| format!("could not stat {}: {error}", path.display()))?;
        if meta.is_dir() {
            copy_support_tree(&path, &target, None)?;
        } else if meta.is_file() {
            std::fs::copy(&path, &target)
                .map_err(|error| format!("could not copy {}: {error}", path.display()))?;
        }
    }
    Ok(())
}

pub fn installed(state: &Path) -> Vec<Skill> {
    let mut found = directories_with_skill(&installed_dir(state));
    found.extend(list_in(&installed_dir(state)));
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found.dedup_by(|a, b| a.name == b.name);
    found
}

/// The eleven procedures shipped in the binary and seeded into the state at init.
pub fn builtin_dir(state: &Path) -> PathBuf {
    state.join("skills").join("builtin")
}

pub fn builtin(state: &Path) -> Vec<Skill> {
    directories_with_skill(&builtin_dir(state))
}

/// Resolve a skill by name across installed, proposed and built-in directories. Installed wins, because a
/// person's promoted copy is the one they mean when both exist. The returned path is the SKILL.md file.
pub fn find(state: &Path, name: &str) -> Option<Skill> {
    for list in [installed(state), proposed(state), builtin(state)] {
        if let Some(skill) = list.into_iter().find(|skill| skill.name == name) {
            return Some(skill);
        }
    }
    None
}

fn directories_with_skill(dir: &Path) -> Vec<Skill> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<Skill> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir() && path.join("SKILL.md").is_file())
        .filter_map(|path| {
            Some(Skill {
                name: path.file_name()?.to_string_lossy().to_string(),
                path: path.join("SKILL.md"),
            })
        })
        .collect();
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

/// Every `SKILL.md` under the state's skills directory, recursively and sorted by path.
///
/// A skill is an instruction the agent loads, so it is part of the harness a generation records. The walk
/// is depth-limited: a link that points at a directory above itself would otherwise be an endless capture.
pub fn skill_files(state: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    collect_skills(&state.join("skills"), 0, &mut found);
    found.sort();
    found
}

fn collect_skills(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
    if depth > 8 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_skills(&path, depth + 1, found);
        } else if path.file_name().is_some_and(|name| name == "SKILL.md") {
            found.push(path);
        }
    }
}

/// What a proposal's frontmatter says about the evidence behind it. A legacy proposal (written before the
/// contract existed) has no `version` and no `contract` key at all, which is different from a `TODO`: the
/// first can be promoted with `--force`, the second is a procedure nobody has described yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evidence {
    pub legacy: bool,
    pub contract: Option<String>,
    pub regression: Vec<String>,
    pub holdout: Vec<String>,
    pub version: Option<String>,
    pub conflicts: Vec<String>,
}

pub fn evidence(text: &str) -> Evidence {
    let Some(front) = frontmatter(text) else {
        return Evidence {
            legacy: true,
            contract: None,
            regression: Vec::new(),
            holdout: Vec::new(),
            version: None,
            conflicts: Vec::new(),
        };
    };
    let contract = front.get("contract").cloned();
    let version = front.get("version").cloned();
    let legacy = contract.is_none() && version.is_none();
    Evidence {
        legacy,
        contract,
        regression: list(front.get("regression")),
        holdout: list(front.get("holdout")),
        version,
        conflicts: list(front.get("conflicts")),
    }
}

/// The `key: value` lines between the first two `---` fences. Deliberately not a YAML parser: a proposal is
/// written by this program, and a parser that accepts more than it writes is a second format to maintain.
fn frontmatter(text: &str) -> Option<std::collections::BTreeMap<String, String>> {
    let text = text.strip_prefix("---\n")?;
    let end = text.find("\n---")?;
    let mut fields = std::collections::BTreeMap::new();
    for line in text[..end].lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        fields.insert(key.trim().to_string(), value.trim().to_string());
    }
    Some(fields)
}

fn list(value: Option<&String>) -> Vec<String> {
    let Some(value) = value else {
        return Vec::new();
    };
    value
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(|item| item.trim().trim_matches('"').trim_matches('\'').to_string())
        .filter(|item| !item.is_empty())
        .collect()
}

/// Refuse a promotion that has no evidence. Returns the warnings a `--force` promotion must print.
pub fn promotable(
    evidence: &Evidence,
    verification: Option<&serde_json::Value>,
) -> Result<Vec<String>, String> {
    if !evidence.conflicts.is_empty() {
        return Err(format!(
            "this proposal has unresolved conflicts: {}",
            evidence.conflicts.join("; ")
        ));
    }
    if evidence.legacy {
        return Ok(vec![
            "this proposal predates the contract; it has no inputs, outputs, scope or failure modes"
                .to_string(),
        ]);
    }
    match evidence.contract.as_deref() {
        None | Some("") | Some("TODO") => {
            return Err(
                "this proposal has no contract: fill in inputs, outputs, scope and failure modes"
                    .to_string(),
            );
        }
        Some(_) => {}
    }
    if evidence.regression.len() < 2 {
        return Err(format!(
            "this proposal names {} regression case(s); a skill needs at least two",
            evidence.regression.len()
        ));
    }
    if evidence.holdout.is_empty() {
        return Err("this proposal names no holdout case".to_string());
    }
    let Some(receipt) = verification else {
        return Err(
            "this proposal has not been verified: run `hades skills verify <name>` first"
                .to_string(),
        );
    };
    if receipt.get("passed").and_then(serde_json::Value::as_bool) != Some(true) {
        return Err("the last verification did not pass".to_string());
    }
    let verified: Vec<String> = receipt
        .get("cases")
        .and_then(serde_json::Value::as_array)
        .map(|cases| {
            cases
                .iter()
                .filter_map(|case| case.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let mut wanted = evidence.regression.clone();
    wanted.extend(evidence.holdout.clone());
    if !wanted.iter().all(|case| verified.contains(case)) {
        return Err(
            "the verification receipt does not cover every case this proposal names".to_string(),
        );
    }
    Ok(Vec::new())
}

/// The verify receipt's path, written beside the proposal so promotion reads one file.
pub fn verification_path(state: &Path, name: &str) -> PathBuf {
    proposed_dir(state).join(format!("{name}.verify.json"))
}

/// The profile a proposal was written for, from its frontmatter.
pub fn proposal_profile(text: &str) -> Option<String> {
    frontmatter(text).and_then(|fields| fields.get("profile").cloned())
}

/// Rewrite one frontmatter field, preserving every other field and the body. A file with no frontmatter
/// gets one; a field already present is replaced rather than duplicated.
pub fn set_field(text: &str, key: &str, value: &str) -> String {
    rewrite_frontmatter(text, &[(key.to_string(), value.to_string())])
}

/// Add evidence fields a pack or an edit left out. Existing values are kept: the tool never invents a
/// contract or a case name over one a person wrote.
pub fn ensure_defaults(text: &str) -> String {
    let existing = frontmatter(text).unwrap_or_default();
    let defaults = [
        ("contract", "TODO".to_string()),
        ("regression", "[]".to_string()),
        ("holdout", "[]".to_string()),
        ("version", "1".to_string()),
    ];
    let missing: Vec<(String, String)> = defaults
        .into_iter()
        .filter(|(key, _)| !existing.contains_key(*key))
        .map(|(key, value)| (key.to_string(), value))
        .collect();
    if missing.is_empty() {
        text.to_string()
    } else {
        rewrite_frontmatter(text, &missing)
    }
}

pub fn set_status(text: &str, status: &str) -> String {
    set_field(text, "status", status)
}

fn rewrite_frontmatter(text: &str, updates: &[(String, String)]) -> String {
    let (mut fields, body) = match text.strip_prefix("---\n").and_then(|rest| {
        rest.find("\n---").map(|end| {
            let front = &rest[..end];
            let body = rest[end + 4..].trim_start_matches('\n').to_string();
            let mut map = std::collections::BTreeMap::new();
            for line in front.lines() {
                if let Some((key, value)) = line.split_once(':') {
                    map.insert(key.trim().to_string(), value.trim().to_string());
                }
            }
            (map, body)
        })
    }) {
        Some(parts) => parts,
        None => (
            std::collections::BTreeMap::new(),
            text.trim_start_matches('\n').to_string(),
        ),
    };
    for (key, value) in updates {
        fields.insert(key.clone(), value.clone());
    }
    let mut out = String::from("---\n");
    for (key, value) in fields {
        out.push_str(&format!("{key}: {value}\n"));
    }
    out.push_str("---\n\n");
    out.push_str(&body);
    out
}

/// Move a proposal into the directory the agent reads, flipping `status: proposed` to `status: active`.
pub fn promote(state: &Path, name: &str) -> Result<PathBuf, String> {
    promote_with(state, name, false)
}

/// The same, with the contract gate. `force` promotes and prints the warnings instead of refusing; it never
/// erases the missing evidence, and the installed file keeps the `TODO` it was promoted with.
pub fn promote_with(state: &Path, name: &str, force: bool) -> Result<PathBuf, String> {
    let from = proposal_path(state, name)?;
    let text = std::fs::read_to_string(&from)
        .map_err(|_| format!("there is no proposal called `{name}`"))?;
    let evidence = evidence(&text);
    if !evidence.conflicts.is_empty() {
        return Err(format!(
            "`{name}` has unresolved conflicts and cannot be promoted: {}",
            evidence.conflicts.join("; ")
        ));
    }
    let receipt = std::fs::read_to_string(verification_path(state, name))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok());
    let warnings = match promotable(&evidence, receipt.as_ref()) {
        Ok(warnings) => warnings,
        Err(said) if !force => return Err(said),
        Err(said) => vec![said],
    };
    for warning in &warnings {
        eprintln!("warning: {name}: {warning}");
    }
    let text = set_status(&text, "active");
    let dir = installed_dir(state).join(name);
    crate::scratch::state_dir(&dir)
        .map_err(|error| format!("could not make {}: {error}", dir.display()))?;
    if let Some(tree) = proposal_tree(state, name) {
        copy_support_tree(&tree, &dir, Some("SKILL.md"))?;
    }
    let to = dir.join("SKILL.md");
    std::fs::write(&to, text)
        .map_err(|error| format!("could not write {}: {error}", to.display()))?;
    if let Some(tree) = proposal_tree(state, name) {
        std::fs::remove_dir_all(&tree)
            .map_err(|error| format!("could not remove {}: {error}", tree.display()))?;
    } else {
        std::fs::remove_file(&from)
            .map_err(|error| format!("could not remove {}: {error}", from.display()))?;
    }
    Ok(to)
}

/// Delete a proposal. A rejection is a decision and it is recorded by the file being gone, not by a status
/// the next reader has to interpret.
pub fn reject(state: &Path, name: &str) -> Result<(), String> {
    let path = proposal_path(state, name)?;
    if let Some(tree) = proposal_tree(state, name) {
        std::fs::remove_dir_all(&tree)
            .map_err(|error| format!("could not remove {}: {error}", tree.display()))
    } else {
        std::fs::remove_file(&path).map_err(|_| format!("there is no proposal called `{name}`"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = crate::scratch::root().join(format!("skills-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        crate::scratch::state_dir(&dir).expect("a scratch state");
        dir
    }

    #[test]
    fn a_first_attempt_pass_proposes_nothing() {
        let state = scratch("first");
        let path = propose(
            &state,
            "fix",
            "make the test pass",
            "cargo test",
            "true",
            "s1",
            1,
        )
        .expect("no error");
        assert!(path.is_none());
        assert!(proposed(&state).is_empty());
    }

    #[test]
    fn a_second_attempt_pass_becomes_a_proposal_that_is_not_installed() {
        let state = scratch("second");
        let path = propose(
            &state,
            "fix",
            "make the test pass",
            "cargo test --quiet",
            "cargo fix --allow-dirty",
            "s1",
            2,
        )
        .expect("a proposal")
        .expect("a path");
        assert!(path.starts_with(proposed_dir(&state)));
        let text = std::fs::read_to_string(&path).expect("the file");
        assert!(text.contains("status: proposed"), "{text}");
        assert!(text.contains("cargo fix --allow-dirty"), "{text}");
        assert!(
            !text.contains("cargo test --quiet"),
            "the check command must not leak into a proposal: {text}"
        );
        assert!(text.contains("check_fingerprint:"), "{text}");
        assert!(
            installed(&state).is_empty(),
            "nothing is loaded before promotion"
        );
    }

    #[test]
    fn promotion_moves_the_file_and_flips_the_status() {
        let state = scratch("promote");
        propose(
            &state,
            "fix",
            "make the test pass",
            "cargo test",
            "true",
            "s1",
            2,
        )
        .expect("a proposal");
        let target = promote_with(&state, "make-the-test-pass", true).expect("a forced promotion");
        assert_eq!(
            target,
            state.join("skills/make-the-test-pass/SKILL.md"),
            "the installed shape is the one the agent reads"
        );
        assert!(target.exists());
        assert!(
            installed(&state)
                .iter()
                .any(|skill| skill.name == "make-the-test-pass")
        );
        let text = std::fs::read_to_string(&target).expect("the promoted file");
        assert!(text.contains("status: active"), "{text}");
        assert!(proposed(&state).is_empty());
    }

    #[test]
    fn a_todo_contract_blocks_promotion_without_force() {
        let state = scratch("todo");
        propose(
            &state,
            "fix",
            "make the test pass",
            "cargo test",
            "true",
            "s1",
            2,
        )
        .expect("a proposal");
        assert!(
            promote(&state, "make-the-test-pass").is_err(),
            "a proposal with a TODO contract is not evidence"
        );
        assert!(
            promote_with(&state, "make-the-test-pass", true).is_ok(),
            "--force promotes and keeps the warning"
        );
    }

    #[test]
    fn a_filled_contract_still_needs_a_verification_receipt() {
        let text = "---\nname: x\nprofile: fix\ncontract: takes a goal, returns a patch\nregression: [a, b]\nholdout: [c]\nversion: 1\n---\n\nbody\n";
        let evidence = evidence(text);
        assert!(!evidence.legacy);
        assert_eq!(evidence.regression, vec!["a", "b"]);
        assert_eq!(evidence.holdout, vec!["c"]);
        assert!(
            promotable(&evidence, None).is_err(),
            "a contract with no receipt is a claim, not evidence"
        );
        let receipt = serde_json::json!({"passed": true, "cases": ["a", "b", "c"]});
        assert!(promotable(&evidence, Some(&receipt)).is_ok());
    }

    #[test]
    fn every_skill_file_is_found_at_any_depth() {
        let state = scratch("walk");
        for at in ["skills/builtin/one", "skills/installed/two"] {
            std::fs::create_dir_all(state.join(at)).expect("a skill directory");
            std::fs::write(state.join(at).join("SKILL.md"), "body").expect("a skill");
        }
        std::fs::create_dir_all(state.join("skills/proposed")).expect("a proposal directory");
        std::fs::write(state.join("skills/proposed/three.md"), "body").expect("a proposal");
        let found = skill_files(&state);
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(
            found[0].ends_with("skills/builtin/one/SKILL.md"),
            "{found:?}"
        );
        assert!(
            found[1].ends_with("skills/installed/two/SKILL.md"),
            "{found:?}"
        );
    }

    #[test]
    fn a_rejection_removes_the_proposal() {
        let state = scratch("reject");
        propose(
            &state,
            "fix",
            "make the test pass",
            "cargo test",
            "true",
            "s1",
            2,
        )
        .expect("a proposal");
        reject(&state, "make-the-test-pass").expect("a rejection");
        assert!(proposed(&state).is_empty());
    }

    #[test]
    fn find_prefers_an_installed_copy_and_falls_back_to_builtin() {
        let state = scratch("find");
        for at in ["skills/builtin/one", "skills/one"] {
            std::fs::create_dir_all(state.join(at)).expect("a skill directory");
            std::fs::write(state.join(at).join("SKILL.md"), at).expect("a skill");
        }
        let found = find(&state, "one").expect("the installed copy");
        assert!(
            found.path.ends_with("skills/one/SKILL.md"),
            "{}",
            found.path.display()
        );
        std::fs::remove_dir_all(state.join("skills/one")).expect("removed the installed copy");
        let found = find(&state, "one").expect("the built-in copy");
        assert!(
            found.path.ends_with("skills/builtin/one/SKILL.md"),
            "{}",
            found.path.display()
        );
        assert!(find(&state, "missing").is_none());
    }
}
