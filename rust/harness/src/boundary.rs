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
//!
//! Resolution also follows the filesystem: a symlink is judged by where it really leads, not by the spelling
//! of the path that names it. `work -> NotesVault/Research` is inside the vault even though nothing above
//! `work` carries the marker, and a path that does not exist yet is judged by its deepest existing parent.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

/// Another agent's home, relative to the home directory.
pub const OTHER_AGENT_HOME: &str = ".hermes";

/// The folder that marks a directory tree as somebody's notes vault.
pub const VAULT_MARKER: &str = ".obsidian";

/// What a path would be crossing, or `None` when it is Hades' own business.
///
/// `home` is a parameter rather than read from the environment so the rule can be tested without a home
/// directory, and so a caller that has already resolved one does not resolve it twice.
pub fn foreign(path: &Path, home: &Path) -> Option<&'static str> {
    // Resolve both sides the same way. Comparing a resolved path against a lexical home inverts the
    // other-agent rule on any machine whose `$HOME` is itself a symlink.
    let home = resolve(home, home);
    let path = resolve(path, &home);

    let other = resolve(&home.join(OTHER_AGENT_HOME), &home);
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
///
/// The walk reaches the filesystem root. It used to stop at twelve parents, which meant a note deep enough in
/// a tree was judged as if the marker were not there; depth is not a boundary.
fn inside_a_vault(path: &Path) -> bool {
    path.ancestors().any(|dir| dir.join(VAULT_MARKER).is_dir())
}

/// The reason a path is refused, as the sentence a person reads.
pub fn refusal(path: &Path, reason: &str) -> String {
    format!(
        "refusing {}: it is {reason}. Hades keeps its own home and its own state, and pointing it here is the \
         one thing that separation exists to prevent",
        path.display()
    )
}

/// One path, judged against the home this process runs under.
///
/// Every surface calls this: the CLI's flags and defaults, the TUI's `--state` and `--config`, and the vision
/// reader. A guard that each call site has to remember is a guard the newest call site will not have.
pub fn guard(path: &Path) -> Result<PathBuf, String> {
    let home =
        std::env::var("HOME").map_err(|_| "no HOME, so pass an explicit path".to_string())?;
    if let Some(reason) = foreign(path, Path::new(&home)) {
        return Err(refusal(path, reason));
    }
    Ok(path.to_path_buf())
}

/// `HADES_HOME`, or `$HOME/.hades`, after the same guard every explicit path meets.
///
/// The default state, config, vault, keys and skills are all derived from this value, so a home inside a notes
/// vault or another agent's install would carry every derived path past the guard. Validate once, here.
pub fn hades_home() -> Result<PathBuf, String> {
    let candidate = match std::env::var("HADES_HOME") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => {
            let home = std::env::var("HOME")
                .map_err(|_| "no HOME and no HADES_HOME, so pass an explicit path".to_string())?;
            PathBuf::from(home).join(".hades")
        }
    };
    guard(&candidate)
}

/// An absolute path with every component resolved the way the kernel would resolve it.
///
/// `canonicalize` alone is the wrong tool: it fails on a path that does not exist yet (a `--state` directory
/// about to be created) and on a dangling symlink whose destination has not been made. This walks one
/// component at a time: `.` is dropped, `..` pops the *already resolved* path, and a symlink is replaced by
/// its target, so `work/..` where `work -> /NotesVault/Research` is `/NotesVault`, not the directory that
/// spelled the link. A dangling link is followed by `read_link`, which does not require the target to exist.
///
/// A loop is capped at 40 hops and then treated as the link's own name; the kernel refuses such a path too
/// (`ELOOP`), so the guard answering lexically cannot let a write through.
pub fn resolve(path: &Path, home: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| home.to_path_buf())
            .join(path)
    };

    let mut pending: VecDeque<OsString> = VecDeque::new();
    for component in absolute.components() {
        match component {
            Component::RootDir | Component::Prefix(_) | Component::CurDir => {}
            Component::Normal(name) => pending.push_back(name.to_os_string()),
            Component::ParentDir => pending.push_back(OsString::from("..")),
        }
    }

    let mut out = PathBuf::from("/");
    let mut hops = 0u32;
    while let Some(name) = pending.pop_front() {
        if name == ".." {
            out.pop();
            if out.as_os_str().is_empty() {
                out = PathBuf::from("/");
            }
            continue;
        }
        let candidate = out.join(&name);
        let is_link = std::fs::symlink_metadata(&candidate)
            .map(|meta| meta.file_type().is_symlink())
            .unwrap_or(false);
        if !is_link {
            out.push(&name);
            continue;
        }
        hops += 1;
        if hops > 40 {
            out.push(&name);
            continue;
        }
        let Ok(target) = std::fs::read_link(&candidate) else {
            out.push(&name);
            continue;
        };
        if target.is_absolute() {
            out = PathBuf::from("/");
        }
        let mut injected: Vec<OsString> = Vec::new();
        for component in target.components() {
            match component {
                Component::RootDir | Component::Prefix(_) | Component::CurDir => {}
                Component::Normal(name) => injected.push(name.to_os_string()),
                Component::ParentDir => injected.push(OsString::from("..")),
            }
        }
        for part in injected.into_iter().rev() {
            pending.push_front(part);
        }
    }
    out
}

/// An absolute path with `.` and `..` resolved, without touching the filesystem.
///
/// This is a spelling helper, not the guard: it cannot see a symlink, and it is exactly the function the
/// boundary used to trust before the audit. `resolve` is what decides where a path really lives.
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

    /// A symlink is judged by where it leads. The old spelling-only walk found no marker above `work`,
    /// because the marker lives at the destination, not in the name that points there.
    #[test]
    fn a_symlink_into_a_vault_is_refused() {
        let vault = scratch("symlink-vault");
        std::fs::create_dir_all(vault.join(".obsidian")).expect("a marker");
        std::fs::create_dir_all(vault.join("Research/notes")).expect("a note");
        let outside = scratch("symlink-outside");
        let alias = outside.join("work");
        std::os::unix::fs::symlink(vault.join("Research/notes"), &alias).expect("an alias");

        let reason = foreign(&alias, &outside).expect("the alias leads into a vault");
        assert!(reason.contains("notes vault"), "{reason}");
        assert!(
            foreign(&alias.join("a-note-that-does-not-exist.md"), &outside).is_some(),
            "a new file below the alias is below the vault too"
        );
    }

    /// The marker is looked for at every parent, not at the first twelve. A deep note used to pass.
    #[test]
    fn a_deep_path_below_a_vault_is_refused() {
        let vault = scratch("deep-vault");
        std::fs::create_dir_all(vault.join(".obsidian")).expect("a marker");
        let mut deep = vault.clone();
        for level in 0..20 {
            deep = deep.join(format!("level-{level}"));
        }
        std::fs::create_dir_all(&deep).expect("a deep tree");
        assert!(foreign(&deep, &vault).is_some(), "{}", deep.display());
        assert!(
            foreign(&deep.join("new.md"), &vault).is_some(),
            "a note that does not exist yet, at the same depth"
        );
    }

    #[test]
    fn a_symlink_to_a_plain_directory_is_not_refused() {
        let plain = scratch("symlink-plain");
        std::fs::create_dir_all(plain.join("projects/hades")).expect("a tree");
        let outside = scratch("symlink-plain-outside");
        let alias = outside.join("work");
        std::os::unix::fs::symlink(plain.join("projects/hades"), &alias).expect("an alias");
        assert!(foreign(&alias, &outside).is_none());
    }

    /// A link whose destination has not been created yet is still read: `read_link` does not require the
    /// target to exist, and `mkdir -p` would create it inside the vault.
    #[test]
    fn a_dangling_symlink_into_a_vault_is_refused() {
        let vault = scratch("dangling-vault");
        std::fs::create_dir_all(vault.join(".obsidian")).expect("a marker");
        let outside = scratch("dangling-outside");
        let alias = outside.join("work");
        std::os::unix::fs::symlink(vault.join("not-created-yet"), &alias).expect("an alias");
        let reason =
            foreign(&alias.join("new-state"), &outside).expect("a dangling link into a vault");
        assert!(reason.contains("notes vault"), "{reason}");
    }

    /// `..` is applied to the path the link really names, not to the spelling that contains the link.
    #[test]
    fn a_dot_dot_after_a_symlink_is_resolved_after_the_link() {
        let vault = scratch("dotdot-vault");
        std::fs::create_dir_all(vault.join(".obsidian")).expect("a marker");
        std::fs::create_dir_all(vault.join("sub")).expect("a subtree");
        let outside = scratch("dotdot-outside");
        std::os::unix::fs::symlink(vault.join("sub"), outside.join("work")).expect("an alias");
        let sneak = outside.join("work/..");
        assert!(
            foreign(&sneak, &outside).is_some(),
            "{} resolved to {}",
            sneak.display(),
            resolve(&sneak, &outside).display()
        );
    }

    /// When `$HOME` is a symlink, the resolved path and the resolved home must still meet.
    #[test]
    fn a_symlinked_home_does_not_invert_the_other_agent_rule() {
        let real = scratch("home-real");
        std::fs::create_dir_all(real.join(".hermes")).expect("the other agent");
        let holder = scratch("home-alias");
        let alias = holder.join("home");
        std::os::unix::fs::symlink(&real, &alias).expect("a home alias");
        assert!(foreign(&alias.join(".hermes/config"), &alias).is_some());
        assert!(foreign(&alias.join("projects"), &alias).is_none());
    }
}
