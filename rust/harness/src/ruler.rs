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
                        let mut seen =
                            vec![std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone())];
                        walk(root, &path, &mut seen, &mut |file| {
                            insert_file(root, file, &mut files)
                        })?;
                    }
                }
                Protect::Manifest(spec) => {
                    let path = root.join(spec);
                    if let Ok(text) = std::fs::read_to_string(&path) {
                        // The projection, not the bytes: a dependency may be added, a test target may not.
                        files.insert(
                            relative_key(Path::new(spec)),
                            mneme::sha256::hex(manifest_projection(&text).as_bytes()),
                        );
                    } else if std::fs::symlink_metadata(&path).is_ok() {
                        return Err(format!(
                            "the ruler could not read `{}`: a build file it cannot read is not a frozen one",
                            path.display()
                        ));
                    }
                }
                Protect::InlineTests => {
                    let mut seen: Vec<PathBuf> = Vec::new();
                    walk(root, root, &mut seen, &mut |file| {
                        if file.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                            return Ok(());
                        }
                        // A file the ruler cannot read as text is skipped rather than fatal: a generated
                        // or binary `.rs` is not source, and refusing the run over it would be a false alarm.
                        let Ok(text) = std::fs::read_to_string(file) else {
                            return Ok(());
                        };
                        let relative = relative_key(file.strip_prefix(root).unwrap_or(file));
                        let mut names: BTreeMap<String, usize> = BTreeMap::new();
                        for region in cfg_test_regions(&text) {
                            let count = names.entry(region.name.clone()).or_insert(0);
                            *count += 1;
                            let key = if *count == 1 {
                                format!("{relative}#cfg(test)::{}", region.name)
                            } else {
                                format!("{relative}#cfg(test)::{}@{count}", region.name)
                            };
                            files.insert(
                                key,
                                mneme::sha256::hex(&text.as_bytes()[region.start..region.end]),
                            );
                        }
                        Ok(())
                    })?;
                }
                Protect::Suffix(suffix) => {
                    let mut seen =
                        vec![std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())];
                    walk(root, root, &mut seen, &mut |file| {
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

/// Walk a directory, entering a symlinked directory as well as a real one, and skipping the three trees
/// nobody protects: a version-control store, a build directory, or a package manager's cache. Those names
/// are skipped at every depth, which is also what the snapshot's ignore list does.
///
/// A link used to be recorded and not entered, which left the contents of a pre-existing `tests/ -> ../shared`
/// unfrozen: editing the target passed the check with every entry unchanged. Entry is guarded by the
/// canonical paths already on the stack, so a link back up the tree ends at the loop rather than running on.
fn walk(
    root: &Path,
    dir: &Path,
    seen: &mut Vec<PathBuf>,
    visit: &mut impl FnMut(&Path) -> Result<(), String>,
) -> Result<(), String> {
    let _ = root;
    walk_into(dir, seen, 0, visit)
}

fn walk_into(
    dir: &Path,
    seen: &mut Vec<PathBuf>,
    depth: usize,
    visit: &mut impl FnMut(&Path) -> Result<(), String>,
) -> Result<(), String> {
    // A tree this deep is a generator or a mistake; either way the guard is cheaper than the answer.
    if depth > 64 {
        return Ok(());
    }
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
            // The link itself is an entry, whichever way it points. If it points at a directory, the
            // contents are entries too: the check follows the link, so the ruler has to.
            visit(&path)?;
            if !path.is_dir() {
                continue;
            }
            let Ok(canonical) = std::fs::canonicalize(&path) else {
                continue;
            };
            if seen.contains(&canonical) {
                continue;
            }
            seen.push(canonical);
            walk_into(&path, seen, depth + 1, visit)?;
            seen.pop();
            continue;
        }
        if meta.is_dir() {
            if matches!(name, ".git" | "target" | "node_modules") {
                continue;
            }
            walk_into(&path, seen, depth + 1, visit)?;
        } else if meta.is_file() {
            visit(&path)?;
        }
    }
    Ok(())
}

/// A manifest with its dependency tables removed.
///
/// The line scan is a table-header machine, not a TOML parser: a header line decides whether the lines
/// under it are frozen, and only headers whose path names a dependency table are dropped. That covers
/// `[dependencies]`, `[target.'cfg(unix)'.dev-dependencies]`, `[workspace.dependencies]` and the dotted
/// `[dependencies.serde]` form, because every segment is tested rather than just the last one.
///
/// What stays frozen is everything the check's meaning depends on: `[[test]]`, `[lib]`, `[features]`,
/// `[patch]`, `[profile]`. A model that adds a crate does not change what `cargo test` runs; a model that
/// adds a test target pointed at a file it wrote changes it completely.
fn manifest_projection(text: &str) -> String {
    let mut projected = String::with_capacity(text.len());
    let mut in_dependency_table = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            let header = trimmed.trim_start_matches('[').trim_end_matches(']').trim();
            in_dependency_table = header.split('.').any(|segment| {
                segment
                    .trim_matches(['"', '\'', ' '])
                    .ends_with("dependencies")
            });
        }
        if in_dependency_table {
            continue;
        }
        projected.push_str(line);
        projected.push('\n');
    }
    projected
}

/// One `#[cfg(test)]` item.
struct Region {
    /// The item's name, from `mod tests` or `fn helper`; unnamed items are numbered by the caller.
    name: String,
    start: usize,
    end: usize,
}

/// Every `#[cfg(test)]`-attributed item in a Rust file, by byte range.
///
/// A real parser would need a dependency this crate does not carry, and the question is narrow: where do the
/// attributes start and where does the item end. Comments, string literals, raw strings and char literals
/// are skipped so a `}` or a `#[cfg(test)]` inside them cannot move a boundary. `#[cfg(not(test))]` is
/// excluded: that is the code that is *not* the test.
fn cfg_test_regions(text: &str) -> Vec<Region> {
    let bytes = text.as_bytes();
    let mut regions = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if let Some(next) = skip_non_code(text, i) {
            i = next;
            continue;
        }
        if bytes[i] == b'#' && bytes.get(i + 1) == Some(&b'[') {
            let Some(close) = attribute_end(text, i) else {
                return regions;
            };
            let attribute: String = text[i + 2..close]
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect();
            if is_cfg_test(&attribute)
                && let Some((_, end)) = item_range(text, close + 1)
            {
                let name = item_name(text, close + 1).unwrap_or_else(|| "item".to_string());
                regions.push(Region {
                    name,
                    start: i,
                    end,
                });
                i = end;
                continue;
            }
            i = close + 1;
            continue;
        }
        i += 1;
    }
    regions
}

/// `cfg(test)`, or a conjunction or disjunction that contains it. `cfg(not(test))` is not a test.
fn is_cfg_test(attribute: &str) -> bool {
    attribute == "cfg(test)"
        || attribute.starts_with("cfg(all(test")
        || attribute.starts_with("cfg(any(test")
        || attribute.starts_with("cfg(test,")
}

/// The end of the attribute that starts at `at` (`#[`), skipping strings and comments inside it.
fn attribute_end(text: &str, at: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut j = at + 2;
    let mut depth = 1usize;
    while j < bytes.len() {
        if let Some(next) = skip_non_code(text, j) {
            j = next;
            continue;
        }
        match bytes[j] {
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(j);
                }
            }
            _ => {}
        }
        j += 1;
    }
    None
}

/// The byte just past the `}` that closes the item starting at `from`, skipping stacked attributes.
///
/// A `;` before any `{` is an item with no body (`use`, a const without braces), which has nothing to
/// freeze.
fn item_range(text: &str, from: usize) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut j = from;
    let mut depth = 0usize;
    let mut opened = false;
    while j < bytes.len() {
        if let Some(next) = skip_non_code(text, j) {
            j = next;
            continue;
        }
        if bytes[j] == b'#' && bytes.get(j + 1) == Some(&b'[') {
            let end = attribute_end(text, j)?;
            j = end + 1;
            continue;
        }
        match bytes[j] {
            b'{' => {
                depth += 1;
                opened = true;
            }
            b'}' => {
                if !opened {
                    return None;
                }
                depth -= 1;
                if depth == 0 {
                    return Some((from, j + 1));
                }
            }
            b';' if !opened => return None,
            _ => {}
        }
        j += 1;
    }
    None
}

/// `mod tests` is `tests`; a name keeps the region stable when lines above it move.
fn item_name(text: &str, from: usize) -> Option<String> {
    // The window has to end on a character boundary: a 240-byte cut through a multi-byte character panics.
    let limit = (from + 240).min(text.len());
    let end = (from..=limit)
        .rev()
        .find(|index| text.is_char_boundary(*index))
        .unwrap_or(from);
    let head = &text[from..end];
    for keyword in [
        "mod ", "fn ", "struct ", "enum ", "trait ", "impl ", "const ", "static ",
    ] {
        if let Some(position) = head.find(keyword) {
            let rest = &head[position + keyword.len()..];
            let name: String = rest
                .chars()
                .take_while(|character| character.is_alphanumeric() || *character == '_')
                .collect();
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    None
}

/// If the byte at `at` opens a comment, a string, a raw string or a char literal, the byte just past it.
///
/// This is what keeps a `{` inside a string from closing a region early, and an attribute quoted in a doc
/// comment from being read as one.
fn skip_non_code(text: &str, at: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    if at >= bytes.len() {
        return None;
    }
    // Byte comparisons throughout: the caller advances one byte at a time, and a slice at a byte in the
    // middle of a multi-byte character panics. A source file with an accent in a comment is not exotic.
    if bytes[at] == b'/' && bytes.get(at + 1) == Some(&b'/') {
        return Some(
            bytes[at..]
                .iter()
                .position(|byte| *byte == b'\n')
                .map(|off| at + off)
                .unwrap_or(bytes.len()),
        );
    }
    if bytes[at] == b'/' && bytes.get(at + 1) == Some(&b'*') {
        let mut depth = 0usize;
        let mut j = at;
        while j + 1 < bytes.len() {
            if bytes[j] == b'/' && bytes[j + 1] == b'*' {
                depth += 1;
                j += 2;
                continue;
            }
            if bytes[j] == b'*' && bytes[j + 1] == b'/' {
                depth -= 1;
                j += 2;
                if depth == 0 {
                    return Some(j);
                }
                continue;
            }
            j += 1;
        }
        return Some(bytes.len());
    }
    if let Some((hashes, start)) = raw_string_start(text, at) {
        let mut j = start;
        while j < bytes.len() {
            if bytes[j] == b'"' {
                let closed = (j + 1..j + 1 + hashes).all(|index| bytes.get(index) == Some(&b'#'));
                if closed {
                    return Some(j + 1 + hashes);
                }
            }
            j += 1;
        }
        return Some(bytes.len());
    }
    if bytes[at] == b'"' {
        let mut j = at + 1;
        while j < bytes.len() {
            match bytes[j] {
                b'\\' => j += 2,
                b'"' => return Some(j + 1),
                _ => j += 1,
            }
        }
        return Some(bytes.len());
    }
    if bytes[at] == b'\'' {
        // A char literal closes; a lifetime does not. `'{'` is the case that matters for brace counting.
        let mut j = at + 1;
        if bytes.get(j) == Some(&b'\\') {
            j += 2;
        } else {
            j += 1;
        }
        if bytes.get(j) == Some(&b'\'') {
            return Some(j + 1);
        }
        return Some(at + 1);
    }
    None
}

/// Where a raw string starts: after `r`, `r#`, `br"` and the like, the index just past its opening quote.
fn raw_string_start(text: &str, at: usize) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut j = at;
    if bytes.get(j) == Some(&b'b') {
        j += 1;
    }
    if bytes.get(j) != Some(&b'r') {
        return None;
    }
    j += 1;
    let mut hashes = 0;
    while bytes.get(j) == Some(&b'#') {
        hashes += 1;
        j += 1;
    }
    if bytes.get(j) == Some(&b'"') {
        Some((hashes, j + 1))
    } else {
        None
    }
}
