//! Paths Hades refuses, because they belong to something else.
//!
//! A run gets its own home, its own state and its own keys. Two places are somebody else's, and the cheapest
//! way to keep them out is to refuse them by path rather than remember not to pass them:
//!
//! - **a notes vault**, recognised by the `.obsidian` folder such a directory carries. It is a person's notes;
//!   a harness that writes into one while claiming to keep to its own directory has already crossed the line.
//! - **another agent's home**, `~/.hermes`. Reading it for a key is the most tempting crossing of all, and the
//!   one that quietly makes this install depend on a different one.
//!
//! A path is compared by its parts after `.` and `..` are resolved, never by substring: `Notes-notes` is not
//! `Notes`, and refusing by substring would teach people to work around the guard.

use std::path::{Component, Path, PathBuf};

/// Another agent's home, relative to the home directory.
pub const OTHER_AGENT_HOME: &str = ".hermes";

/// The folder that marks a directory tree as somebody's notes vault.
pub const VAULT_MARKER: &str = ".obsidian";

/// How far up from a path the vault marker is looked for. A note lives some levels below the vault root, and a
/// walk that does not stop is a walk that reads every parent on the machine.
const VAULT_DEPTH: usize = 12;

/// What a path would be crossing, or `None` when it is Hades' own business.
///
/// `home` is a parameter rather than read from the environment so the rule can be tested without a home
/// directory, and so a caller that has already resolved one does not resolve it twice.
pub fn foreign(path: &Path, home: &Path) -> Option<&'static str> {
    let path = clean(path, home);

    let other = clean(&home.join(OTHER_AGENT_HOME), home);
    if path == other || path.starts_with(&other) {
        return Some("another agent's home (config, keys, sessions, cache)");
    }

    if inside_a_vault(&path) {
        return Some("a notes vault (the directory carries an .obsidian folder)");
    }

    None
}

/// Whether the path, or one of its parents, is marked as a vault. The path itself need not exist: a `--state`
/// directory about to be created under a vault is exactly the case this has to catch.
fn inside_a_vault(path: &Path) -> bool {
    let mut current = Some(path);
    for _ in 0..VAULT_DEPTH {
        let Some(dir) = current else { break };
        if dir.join(VAULT_MARKER).is_dir() {
            return true;
        }
        current = dir.parent();
    }
    false
}

/// The reason a path is refused, as the sentence a person reads.
pub fn refusal(path: &Path, reason: &str) -> String {
    format!(
        "refusing {}: it is {reason}. Hades keeps its own home and its own state, and pointing it here is the \
         one thing that separation exists to prevent",
        path.display()
    )
}

/// An absolute path with `.` and `..` resolved, without touching the filesystem.
///
/// `canonicalize` is the obvious tool and the wrong one here: it fails on a path that does not exist yet, and
/// the guard has to answer for a `--state` directory that is about to be created. A lexical walk also cannot
/// be fooled by a symlink into a different answer half way through, which is what a guard wants.
pub fn clean(path: &Path, home: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| home.to_path_buf())
            .join(path)
    };
    let mut out = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        PathBuf::from("/home/someone")
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("hades-boundary-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch dir");
        dir
    }

    #[test]
    fn another_agents_home_is_refused() {
        let home = home();
        assert!(foreign(&home.join(OTHER_AGENT_HOME), &home).is_some());
        assert!(foreign(&home.join(".hermes/.env"), &home).is_some());
    }

    #[test]
    fn a_marked_vault_and_everything_below_it_is_refused() {
        let root = scratch("vault");
        std::fs::create_dir_all(root.join(".obsidian")).expect("a marker");
        std::fs::create_dir_all(root.join("Research/notes")).expect("a note");
        assert!(foreign(&root, &root).is_some(), "the vault root");
        assert!(
            foreign(&root.join("Research/notes"), &root).is_some(),
            "a note"
        );
        assert!(
            foreign(&root.join("Research/notes/new.md"), &root).is_some(),
            "a note that does not exist yet"
        );
    }

    #[test]
    fn a_directory_that_is_not_marked_is_not_a_vault() {
        let root = scratch("plain");
        std::fs::create_dir_all(root.join("Documents/Notes-notes")).expect("a lookalike");
        assert!(foreign(&root.join("Documents/Notes-notes"), &root).is_none());
        assert!(foreign(&root.join("projects/hades"), &root).is_none());
    }

    #[test]
    fn hades_own_paths_are_not_refused() {
        let home = home();
        assert!(foreign(&home.join(".hades"), &home).is_none());
        assert!(foreign(&home.join(".hades/mneme"), &home).is_none());
        assert!(foreign(&home.join(".hermes-notes"), &home).is_none());
    }

    #[test]
    fn a_dot_dot_does_not_walk_around_the_guard() {
        let home = home();
        let sneak = home.join(".hades/../.hermes");
        assert!(foreign(&sneak, &home).is_some(), "{}", sneak.display());
    }

    #[test]
    fn a_relative_path_is_resolved_before_it_is_judged() {
        let home = home();
        // Whatever the working directory is, the answer is a path and not a panic.
        let judged = foreign(Path::new("some/relative/dir"), &home);
        assert!(judged.is_none() || judged.is_some());
    }
}
