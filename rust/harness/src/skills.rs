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
    std::fs::create_dir_all(&dir)
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
        "---\nname: {name}\nstatus: proposed\nprofile: {profile}\ncheck_fingerprint: {fingerprint}\nsession: {session}\nattempts: {attempts}\nat: {at}\n---\n\n# {goal}\n\n## When to use\n\n{goal}\n\n## What worked\n\n```sh\n{program}\n```\n\n## How it was checked\n\nThe run's check passed. Its command is withheld from this proposal; `check_fingerprint` records which check it was.\n\nPromote this with `hades skills promote {name}` only after reading it.\n",
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
    std::fs::create_dir_all(&dir)
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
        "---\nname: {name}\nstatus: proposed\nprofile: {profile}\ncheck_fingerprint: {fingerprint}\nsession: {session}\nreplay: {replay}\nat: {at}\nsummary: {summary}\n---\n\n# {name}\n\n{runbook}\n\nPromote this with `hades skills promote {name}` only after reading it and the replay session.\n",
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
    list_in(&proposed_dir(state))
}

pub fn installed(state: &Path) -> Vec<Skill> {
    let mut found = directories_with_skill(&installed_dir(state));
    found.extend(list_in(&installed_dir(state)));
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found.dedup_by(|a, b| a.name == b.name);
    found
}

/// The seven procedures shipped in the binary and seeded into the state at init.
pub fn builtin_dir(state: &Path) -> PathBuf {
    state.join("skills").join("builtin")
}

pub fn builtin(state: &Path) -> Vec<Skill> {
    directories_with_skill(&builtin_dir(state))
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

/// Move a proposal into the directory the agent reads, flipping `status: proposed` to `status: active`.
pub fn promote(state: &Path, name: &str) -> Result<PathBuf, String> {
    let from = proposed_dir(state).join(format!("{name}.md"));
    let text = std::fs::read_to_string(&from)
        .map_err(|_| format!("there is no proposal called `{name}`"))?;
    let text = text.replace("status: proposed", "status: active");
    let dir = installed_dir(state).join(name);
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("could not make {}: {error}", dir.display()))?;
    let to = dir.join("SKILL.md");
    std::fs::write(&to, text)
        .map_err(|error| format!("could not write {}: {error}", to.display()))?;
    std::fs::remove_file(&from)
        .map_err(|error| format!("could not remove {}: {error}", from.display()))?;
    Ok(to)
}

/// Delete a proposal. A rejection is a decision and it is recorded by the file being gone, not by a status
/// the next reader has to interpret.
pub fn reject(state: &Path, name: &str) -> Result<(), String> {
    let path = proposed_dir(state).join(format!("{name}.md"));
    std::fs::remove_file(&path).map_err(|_| format!("there is no proposal called `{name}`"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = crate::scratch::root().join(format!("skills-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch state");
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
        let target = promote(&state, "make-the-test-pass").expect("a promotion");
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
}
