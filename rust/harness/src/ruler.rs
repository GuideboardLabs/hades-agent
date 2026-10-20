//! The ruler: a digest of the check command and the files that decide it, taken before the first write.
//!
//! A check is only evidence if it means the same thing at the start and at the end of a run. The model
//! may edit the tree the check reads, and `patch`/`shell` may write any path under the root, so a run
//! whose check is `cargo test` can weaken an assertion and still turn red into green. The ruler closes
//! that: the command text and every declared protected file are hashed before the baseline, re-hashed
//! after the check returns, and a difference ends the run as [`crate::graph::Ended::RulerChanged`].
//!
//! What it does not cover, stated here because a ruler that pretends to cover it is worse than none:
//!
//! - **Inline test modules.** `cargo test` runs `#[cfg(test)]` modules inside `src/*.rs`, and a file
//!   ruler cannot freeze a region of a file the model must edit. The shipped defaults protect `tests/`
//!   and `benches/`, and `--protect` can name another file or directory; moving the tests out of `src/`
//!   or running a separate test crate is the way to close that gap.
//! - **A check that is a script.** The command text is always hashed, so changing `./check.sh` to
//!   `./check.sh --skip x` is caught. Editing the script's contents is caught only when the script is
//!   named in `protect`.
//! - **Symlinks.** A symlink is an entry in its own right: the digest is the link's target plus the digest
//!   of what it resolves to, so adding one under a protected directory, retargeting one, or changing the
//!   file it points at is all a change. What is not covered is the *contents* of a pre-existing symlinked
//!   directory: the walk records the link and does not descend through it, because descending would make
//!   the ruler's path set depend on where a link went.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use crate::profile::Protect;

/// A frozen check. `files` is relative to the root and sorted, so a new or removed file is as visible as
/// an edited one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ruler {
    pub check: String,
    pub check_sha256: String,
    pub files: BTreeMap<String, String>,
    /// How many rules were declared. Kept because `files` empty is ambiguous: a run with no rule and a
    /// run whose rule matched nothing are both command-only, and only one of them was meant to protect.
    pub rules: usize,
    /// When the ruler was taken, in seconds since the Unix epoch. Evidence about the run, not part of
    /// what the digest means.
    pub at: u64,
}

impl Ruler {
    /// Freeze the check and every declared path under `root`. A rule that escapes the root is refused
    /// here as well as at parse time: the caller may build a [`Protect`] directly.
    pub fn take(check: &str, root: &Path, protect: &[Protect]) -> Result<Ruler, String> {
        let check = check.trim().to_string();
        let mut files: BTreeMap<String, String> = BTreeMap::new();
        for rule in protect {
            rule.check_inside(root)?;
            match rule {
                Protect::File(spec) => {
                    let path = root.join(spec);
                    // `symlink_metadata`, not `is_file`: a rule naming a path that is a symlink - or a
                    // directory - is still an entry the model can replace, and a missing entry is the one
                    // an attacker creates.
                    if std::fs::symlink_metadata(&path).is_ok_and(|meta| !meta.is_dir()) {
                        insert_file(root, &path, &mut files)?;
                    }
                }
                Protect::Dir(spec) => {
                    let path = root.join(spec);
                    if path.is_dir() {
                        walk(root, &path, &mut |file| insert_file(root, file, &mut files))?;
                    }
                }
                Protect::Suffix(suffix) => {
                    walk(root, root, &mut |file| {
                        let matches = file
                            .file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| name.ends_with(suffix.as_str()));
                        if matches {
                            insert_file(root, file, &mut files)
                        } else {
                            Ok(())
                        }
                    })?;
                }
            }
        }
        Ok(Ruler {
            check_sha256: mneme::sha256::hex(check.as_bytes()),
            check,
            files,
            rules: protect.len(),
            at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_secs())
                .unwrap_or(0),
        })
    }

    /// The first difference, named the way a person would look it up: the check, a changed file, an
    /// added file, or a removed one.
    pub fn diff(&self, other: &Ruler) -> Option<String> {
        if self.check_sha256 != other.check_sha256 {
            return Some(format!(
                "the check command changed: `{}` became `{}`",
                self.check, other.check
            ));
        }
        for (path, digest) in &self.files {
            match other.files.get(path) {
                None => return Some(format!("the protected file `{path}` was removed")),
                Some(now) if now != digest => {
                    return Some(format!("the protected file `{path}` changed"));
                }
                Some(_) => {}
            }
        }
        for path in other.files.keys() {
            if !self.files.contains_key(path) {
                return Some(format!("the protected file `{path}` was added"));
            }
        }
        None
    }

    /// The digest a receipt can print without wrapping the line: enough of the SHA-256 to compare two
    /// rulers by eye.
    pub fn short(&self) -> &str {
        self.check_sha256.get(..12).unwrap_or(&self.check_sha256)
    }

    /// The sentence a refusal uses, so the run's end line and the receipt cannot drift.
    pub fn changed_line(&self) -> String {
        format!(
            "the check changed during the run; the result is not accepted (ruler {})",
            self.short()
        )
    }
}

/// Hash one entry into the map, keyed by its path relative to the root.
///
/// A regular file is its bytes. A symlink is its target *and* the digest of what it resolves to: the target
/// alone would miss an edit to the file the check actually reads, and the contents alone would miss a
/// retarget. A dangling link is still an entry, so a link that is created and then pointed somewhere is a
/// change rather than a read error that aborts the run.
fn insert_file(
    root: &Path,
    path: &Path,
    files: &mut BTreeMap<String, String>,
) -> Result<(), String> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| format!("`{}` is not under the root", path.display()))?;
    let digest = match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            let target = std::fs::read_link(path).map_err(|error| {
                format!("the ruler could not read `{}`: {error}", path.display())
            })?;
            let resolved = std::fs::read(path)
                .map(|bytes| mneme::sha256::hex(&bytes))
                .unwrap_or_else(|_| "unresolved".to_string());
            // A newline cannot appear in a path, so the two halves cannot be shifted into each other.
            mneme::sha256::hex(format!("link:{}\n{resolved}", target.to_string_lossy()).as_bytes())
        }
        _ => {
            let bytes = std::fs::read(path).map_err(|error| {
                format!("the ruler could not read `{}`: {error}", path.display())
            })?;
            mneme::sha256::hex(&bytes)
        }
    };
    files.insert(relative_key(relative), digest);
    Ok(())
}

fn relative_key(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Walk a directory without following symlinks and without entering the three trees nobody protects:
/// a version-control store, a build directory, or a package manager's cache. Those names are skipped at
/// every depth, which is also what the snapshot's ignore list does.
fn walk(
    root: &Path,
    dir: &Path,
    visit: &mut impl FnMut(&Path) -> Result<(), String>,
) -> Result<(), String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(());
    };
    let mut entries: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|e| e.path())
        .collect();
    entries.sort();
    for path in entries {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            // A link that points at a directory is recorded and not entered: descending through it would
            // make the hashed set depend on where the link went, and a loop would not terminate.
            visit(&path)?;
            continue;
        }
        if meta.is_dir() {
            if matches!(name, ".git" | "target" | "node_modules") {
                continue;
            }
            walk(root, &path, visit)?;
        } else if meta.is_file() {
            visit(&path)?;
        }
    }
    let _ = root;
    Ok(())
}
