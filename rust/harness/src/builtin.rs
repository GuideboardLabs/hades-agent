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
}

pub const ALL: [Builtin; 11] = [
    Builtin {
        name: "plan-a-change",
        body: include_str!("../../skills/plan-a-change/SKILL.md"),
    },
    Builtin {
        name: "repair-a-failing-check",
        body: include_str!("../../skills/repair-a-failing-check/SKILL.md"),
    },
    Builtin {
        name: "review-a-diff",
        body: include_str!("../../skills/review-a-diff/SKILL.md"),
    },
    Builtin {
        name: "research-with-sources",
        body: include_str!("../../skills/research-with-sources/SKILL.md"),
    },
    Builtin {
        name: "write-for-humans",
        body: include_str!("../../skills/write-for-humans/SKILL.md"),
    },
    Builtin {
        name: "safety-before-acting",
        body: include_str!("../../skills/safety-before-acting/SKILL.md"),
    },
    Builtin {
        name: "keep-memory-useful",
        body: include_str!("../../skills/keep-memory-useful/SKILL.md"),
    },
    Builtin {
        name: "create-a-skill",
        body: include_str!("../../skills/create-a-skill/SKILL.md"),
    },
    Builtin {
        name: "land-a-change",
        body: include_str!("../../skills/land-a-change/SKILL.md"),
    },
    Builtin {
        name: "build-an-mcp-server",
        body: include_str!("../../skills/build-an-mcp-server/SKILL.md"),
    },
    Builtin {
        name: "run-and-verify",
        body: include_str!("../../skills/run-and-verify/SKILL.md"),
    },
];

/// Where a seeded built-in lives: `<state>/skills/builtin/<name>/SKILL.md`.
pub fn dir(state: &Path) -> PathBuf {
    state.join("skills").join("builtin")
}

/// Write every built-in that is not already on disk. Returns how many were written this time.
pub fn write_all(state: &Path) -> Result<usize, String> {
    let root = dir(state);
    let mut written = 0;
    for skill in &ALL {
        let folder = root.join(skill.name);
        let path = folder.join("SKILL.md");
        if path.is_file() {
            continue;
        }
        std::fs::create_dir_all(&folder)
            .map_err(|error| format!("could not make {}: {error}", folder.display()))?;
        std::fs::write(&path, skill.body)
            .map_err(|error| format!("could not write {}: {error}", path.display()))?;
        written += 1;
    }
    Ok(written)
}

pub fn names() -> Vec<&'static str> {
    ALL.iter().map(|skill| skill.name).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../target/scratch")
            .join(format!("builtin-{name}-{}", std::process::id()));
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
        }
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
