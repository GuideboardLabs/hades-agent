//! The skills a fresh install ships with.
//!
//! A shared project cannot inherit the machine's personal catalog: a new user gets a small, general set
//! that covers the work an agent actually does, and nothing that only makes sense on the author's box.
//! Eleven procedures, each one a task shape rather than a topic. They are embedded in the binary, so a
//! release tarball has them without a second directory, and `hades init` seeds them into the state so a
//! person can read and edit them.
//!
//! Seeding never overwrites a file that is already there. An upgraded binary adds a new built-in, and a
//! locally edited one keeps the edit. `hades skills list` separates built-in, promoted and proposed, so
//! "where did this procedure come from" is answerable.

use std::path::{Path, PathBuf};

pub struct Builtin {
    pub name: &'static str,
    pub body: &'static str,
    /// Files shipped beside `SKILL.md`, relative to the skill folder. They are seeded when missing, so an
    /// upgraded binary adds a helper without overwriting a locally edited skill.
    pub files: &'static [(&'static str, &'static str)],
}

pub const ALL: [Builtin; 11] = [
    Builtin {
        name: "plan-a-change",
        body: include_str!("../../skills/plan-a-change/SKILL.md"),
        files: &[(
            "scripts/plan_lint.py",
            include_str!("../../skills/plan-a-change/scripts/plan_lint.py"),
        )],
    },
    Builtin {
        name: "repair-a-failing-check",
        body: include_str!("../../skills/repair-a-failing-check/SKILL.md"),
        files: &[(
            "scripts/check_loop.py",
            include_str!("../../skills/repair-a-failing-check/scripts/check_loop.py"),
        )],
    },
    Builtin {
        name: "review-a-diff",
        body: include_str!("../../skills/review-a-diff/SKILL.md"),
        files: &[(
            "scripts/diff_brief.py",
            include_str!("../../skills/review-a-diff/scripts/diff_brief.py"),
        )],
    },
    Builtin {
        name: "research-with-sources",
        body: include_str!("../../skills/research-with-sources/SKILL.md"),
        files: &[(
            "scripts/source_check.py",
            include_str!("../../skills/research-with-sources/scripts/source_check.py"),
        )],
    },
    Builtin {
        name: "write-for-humans",
        body: include_str!("../../skills/write-for-humans/SKILL.md"),
        files: &[(
            "scripts/prose_lint.py",
            include_str!("../../skills/write-for-humans/scripts/prose_lint.py"),
        )],
    },
    Builtin {
        name: "safety-before-acting",
        body: include_str!("../../skills/safety-before-acting/SKILL.md"),
        files: &[(
            "scripts/risk_scan.py",
            include_str!("../../skills/safety-before-acting/scripts/risk_scan.py"),
        )],
    },
    Builtin {
        name: "keep-memory-useful",
        body: include_str!("../../skills/keep-memory-useful/SKILL.md"),
        files: &[(
            "scripts/note_check.py",
            include_str!("../../skills/keep-memory-useful/scripts/note_check.py"),
        )],
    },
    Builtin {
        name: "create-a-skill",
        body: include_str!("../../skills/create-a-skill/SKILL.md"),
        files: &[(
            "scripts/validate_skill.py",
            include_str!("../../skills/create-a-skill/scripts/validate_skill.py"),
        )],
    },
    Builtin {
        name: "land-a-change",
        body: include_str!("../../skills/land-a-change/SKILL.md"),
        files: &[(
            "scripts/preflight.py",
            include_str!("../../skills/land-a-change/scripts/preflight.py"),
        )],
    },
    Builtin {
        name: "build-an-mcp-server",
        body: include_str!("../../skills/build-an-mcp-server/SKILL.md"),
        files: &[(
            "scripts/mcp_probe.py",
            include_str!("../../skills/build-an-mcp-server/scripts/mcp_probe.py"),
        )],
    },
    Builtin {
        name: "run-and-verify",
        body: include_str!("../../skills/run-and-verify/SKILL.md"),
        files: &[(
            "scripts/wait_ready.py",
            include_str!("../../skills/run-and-verify/scripts/wait_ready.py"),
        )],
    },
];

/// Where a seeded built-in lives: `<state>/skills/builtin/<name>/SKILL.md`.
pub fn dir(state: &Path) -> PathBuf {
    state.join("skills").join("builtin")
}

/// Write every built-in that is not already on disk. Returns how many skills were written this time.
/// Support files are seeded independently, so a binary upgrade can add a script to a skill whose body a
/// person has already edited. Nothing existing is overwritten.
pub fn write_all(state: &Path) -> Result<usize, String> {
    let root = dir(state);
    let mut written = 0;
    for skill in &ALL {
        let folder = root.join(skill.name);
        let path = folder.join("SKILL.md");
        if !path.is_file() {
            std::fs::create_dir_all(&folder)
                .map_err(|error| format!("could not make {}: {error}", folder.display()))?;
            std::fs::write(&path, skill.body)
                .map_err(|error| format!("could not write {}: {error}", path.display()))?;
            written += 1;
        }
        for (relative, body) in skill.files {
            let target = folder.join(relative);
            if target.is_file() {
                continue;
            }
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| format!("could not make {}: {error}", parent.display()))?;
            }
            std::fs::write(&target, body)
                .map_err(|error| format!("could not write {}: {error}", target.display()))?;
            executable(&target)?;
        }
    }
    Ok(written)
}

#[cfg(unix)]
fn executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    let mut mode = std::fs::metadata(path)
        .map_err(|error| format!("could not stat {}: {error}", path.display()))?
        .permissions();
    mode.set_mode(0o755);
    std::fs::set_permissions(path, mode)
        .map_err(|error| format!("could not mark {} executable: {error}", path.display()))
}

#[cfg(not(unix))]
fn executable(_path: &Path) -> Result<(), String> {
    Ok(())
}

pub fn names() -> Vec<&'static str> {
    ALL.iter().map(|skill| skill.name).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = crate::scratch::root().join(format!("builtin-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch state");
        dir
    }

    #[test]
    fn every_built_in_has_a_name_and_a_description() {
        for skill in &ALL {
            assert!(!skill.name.is_empty());
            assert!(
                skill.body.contains(&format!("name: {}", skill.name)),
                "{} names itself",
                skill.name
            );
            assert!(
                skill.body.contains("description:"),
                "{} has a description",
                skill.name
            );
            assert!(
                skill.body.len() > 500,
                "{} is a procedure, not a stub",
                skill.name
            );
            assert!(!skill.files.is_empty(), "{} ships a helper", skill.name);
            for (relative, body) in skill.files {
                assert!(
                    relative.starts_with("scripts/") && relative.ends_with(".py"),
                    "{} has an unexpected support file: {relative}",
                    skill.name
                );
                assert!(
                    body.contains("def main"),
                    "{} helper has no entry point",
                    skill.name
                );
            }
        }
    }

    #[test]
    fn seeding_writes_the_helpers_and_restores_a_missing_one() {
        let state = scratch("helpers");
        assert_eq!(write_all(&state).expect("seeded"), 11);
        let script = dir(&state).join("review-a-diff/scripts/diff_brief.py");
        assert!(script.is_file(), "the helper was seeded beside the body");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&script)
                .expect("mode")
                .permissions()
                .mode();
            assert_ne!(mode & 0o111, 0, "the helper is executable");
        }
        std::fs::remove_file(&script).expect("removed for the test");
        assert_eq!(write_all(&state).expect("seeded again"), 0);
        assert!(
            script.is_file(),
            "a missing helper is restored without touching the body"
        );
    }

    #[test]
    fn seeding_writes_eleven_and_never_overwrites_an_edit() {
        let state = scratch("seed");
        assert_eq!(write_all(&state).expect("seeded"), 11);
        let path = dir(&state).join("plan-a-change/SKILL.md");
        std::fs::write(&path, "edited").expect("an edit");
        assert_eq!(write_all(&state).expect("seeded again"), 0);
        assert_eq!(std::fs::read_to_string(&path).expect("the edit"), "edited");
    }
}
