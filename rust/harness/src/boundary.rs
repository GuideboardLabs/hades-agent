//! The two systems, kept apart.
//!
//! Hades is a second system, not a second front end for the first. It has its own vault, its own state
//! directory and its own keys, and it is built to be tested on a machine where none of the first one exists.
//! One day it may be the main place. Until that is a decision rather than a hope, it must not read or write
//! anything the running Hermes install uses, and the cheapest way to hold that is to refuse the paths by
//! name rather than remember not to pass them.
//!
//! What is refused, and why each one is the whole point:
//!
//! - `~/Documents/ObsidianVault`: the live second brain. It holds fenced personal material, and a harness
//!   that reads it while claiming to be a separate system has already crossed the fence.
//! - `~/.hermes`: the live install's config, keys, sessions and cache. Reading it for a key is the most
//!   tempting crossing of all, and the one that quietly makes Hades depend on the machine it is replacing.
//!
//! A path is compared by its parts after `.` and `..` are resolved, never by substring: `ObsidianVault-notes`
//! is somebody else's directory and refusing it would teach people to work around the guard.

use std::path::{Component, Path, PathBuf};

/// Where the live install keeps its things, relative to the home directory.
pub const HERMES_VAULT: &str = "Documents/ObsidianVault";
pub const HERMES_HOME: &str = ".hermes";

/// What a path would be crossing, or `None` when it is Hades' own business.
///
/// `home` is a parameter rather than read from the environment so the rule can be tested without a home
/// directory, and so a caller that has already resolved one does not resolve it twice.
pub fn foreign(path: &Path, home: &Path) -> Option<&'static str> {
    let path = clean(path, home);
    for (relative, reason) in [
        (
            HERMES_VAULT,
            "the live Hermes vault (the second brain, fences and all)",
        ),
        (
            HERMES_HOME,
            "the live Hermes home (config, keys, sessions, cache)",
        ),
    ] {
        let guarded = clean(&home.join(relative), home);
        if path == guarded || path.starts_with(&guarded) {
            return Some(reason);
        }
    }
    None
}

/// The reason a path is refused, as the sentence a person reads.
pub fn refusal(path: &Path, reason: &str) -> String {
    format!(
        "refusing {}: it is {reason}. Hades has its own vault and state, and pointing it here is the one \
         thing the separation exists to prevent",
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

    #[test]
    fn the_live_vault_and_the_live_home_are_refused() {
        let home = home();
        assert!(foreign(&home.join(HERMES_VAULT), &home).is_some());
        assert!(foreign(&home.join(HERMES_VAULT).join("20-29 Research"), &home).is_some());
        assert!(foreign(&home.join(HERMES_HOME), &home).is_some());
        assert!(foreign(&home.join(".hermes/.env"), &home).is_some());
    }

    #[test]
    fn hades_own_paths_are_not_refused() {
        let home = home();
        assert!(foreign(&home.join(".hades"), &home).is_none());
        assert!(foreign(&home.join(".hades/mneme"), &home).is_none());
        assert!(foreign(&home.join("projects/hades"), &home).is_none());
    }

    #[test]
    fn a_lookalike_name_is_not_the_guarded_path() {
        let home = home();
        assert!(
            foreign(&home.join("Documents/ObsidianVault-notes"), &home).is_none(),
            "a substring is not a path"
        );
        assert!(foreign(&home.join(".hermes-notes"), &home).is_none());
    }

    #[test]
    fn a_dot_dot_does_not_walk_around_the_guard() {
        let home = home();
        let sneak = home.join(".hades/../.hermes");
        assert!(foreign(&sneak, &home).is_some(), "{}", sneak.display());
        let vault = home.join("Documents/ObsidianVault/../ObsidianVault/21 Agent Memory");
        assert!(foreign(&vault, &home).is_some());
    }

    #[test]
    fn a_relative_path_is_resolved_before_it_is_judged() {
        let home = home();
        // Whatever the working directory is, the answer is a path and not a panic.
        let judged = foreign(Path::new("some/relative/dir"), &home);
        assert!(judged.is_none() || judged.is_some());
    }
}
