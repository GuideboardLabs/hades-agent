//! One anchored edit, applied exactly or refused.
//!
//! `write_file` costs a whole file and overwrites whatever was there. This module is the middle verb: the
//! model sends the text it expects to find and the text that replaces it, and the tool either applies it
//! once or says why it did not. There is no fuzzy matching and no offset guessing, because a patch that
//! lands in the wrong place is worse than a refusal: it writes a plausible file nobody asked for.
//!
//! The rule, in one place so the tool, the governed answer and their tests cannot disagree:
//!
//! 1. The file must exist and be UTF-8; this never creates one.
//! 2. Every `old` must be non-empty and must appear exactly once in the text as it stands when that edit
//!    is applied. Edits apply in order, to the in-memory text.
//! 3. Any failure means nothing is written. The file is written once, after every edit has applied.
//! 4. The write is a temp file plus rename in the same directory, with the original mode copied, the temp
//!    synced, and the directory synced after the rename.
//!
//! An answer may name several files. [`plan_all`] plans every file in memory before [`write_all`] writes
//! one, so a bad anchor anywhere means nothing is written at all. A write that fails part way names the
//! files already written; atomicity across files is not promised and is not claimed.
//!
//! Stated limits. A directory this user cannot write defeats temp-plus-rename even when the file itself is
//! writable; the refusal says so. The result is owned by this process's user, not the original owner. A
//! hardlink to the old inode and any extended attribute are not carried over. Two concurrent patches of one
//! file still race between the read and the rename, narrower than `write_file`'s last-writer-wins but not
//! gone, because the file has no lock.

use crate::path::PathPolicy;
use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// One anchored edit: the text to find, and what replaces it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub old: String,
    pub new: String,
}

/// One edit as an answer carries it: the file is part of the edit, because a governed answer may change
/// several files and a path on the outside could only name one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEdit {
    pub path: String,
    pub old: String,
    pub new: String,
}

/// What one applied edit did, for the person reading the result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// 1-based line where the anchor started, in the text as it stood before this edit.
    pub line: usize,
    pub removed: usize,
    pub added: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    pub before: usize,
    pub after: usize,
    pub hunks: Vec<Hunk>,
}

/// One file's future, computed and held before anything is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Canonical: the symlink is resolved once, here, so every write names the same target.
    pub path: PathBuf,
    pub text: String,
    pub before: usize,
    pub after: usize,
    pub hunks: Vec<Hunk>,
    /// Bytes the model spent on this file's anchors and replacements.
    pub sent: usize,
}

/// A file this large is not an edit; it is a rewrite wearing a patch's clothes.
pub const CAP: usize = 1024 * 1024;

/// An answer that carries more than this is a rewrite with extra steps, and the caps are what keep a
/// refusal cheap to read.
pub const MAX_EDITS: usize = 32;
pub const MAX_FILES: usize = 16;

/// Temp names must be unique across the worker threads a chat can spawn (up to four, one directory).
static TEMPS: AtomicU64 = AtomicU64::new(0);

/// The answer's edit object, when the text is one.
///
/// `None` means "this is not an edit answer, treat it as a program". `Some(Err)` means it is shaped like
/// one and cannot be read, which is a refusal the next attempt can act on. The predicate is deliberately
/// strict: a shell brace group is legal shell, so only `{"edits"` at the front of a parsed object claims
/// the text.
pub fn edits_from(text: &str) -> Option<Result<Vec<FileEdit>, String>> {
    let trimmed = text.trim();
    let rest = trimmed.strip_prefix('{')?;
    let shaped = rest.trim_start().starts_with("\"edits\"");
    let parsed: Value = match serde_json::from_str(trimmed) {
        Ok(parsed) => parsed,
        Err(error) => {
            if shaped {
                return Some(Err(format!(
                    "the edit object could not be read: {error}. Expected \
                     {{\"edits\":[{{\"path\":\"...\",\"old\":\"...\",\"new\":\"...\"}}]}}"
                )));
            }
            return None;
        }
    };
    let object = parsed.as_object()?;
    if !object.contains_key("edits") {
        return None;
    }
    if let Some(extra) = object.keys().find(|key| key.as_str() != "edits") {
        return Some(Err(format!(
            "the edit object has an unknown field `{extra}`; the only key is `edits`"
        )));
    }
    Some(file_edits(object.get("edits").unwrap_or(&Value::Null)))
}

/// The `edits` array, read by hand: the crate has no serde derive and three fields do not need one.
fn file_edits(value: &Value) -> Result<Vec<FileEdit>, String> {
    let Some(items) = value.as_array() else {
        return Err("`edits` has to be a list of {path, old, new} objects".to_string());
    };
    if items.is_empty() {
        return Err("`edits` is empty; an edit answer needs at least one edit".to_string());
    }
    let mut edits = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let Some(object) = item.as_object() else {
            return Err(format!(
                "edit {}: expected an object with `path`, `old` and `new`",
                index + 1
            ));
        };
        for key in object.keys() {
            if key != "path" && key != "old" && key != "new" {
                return Err(format!("edit {}: unknown field `{key}`", index + 1));
            }
        }
        let field = |name: &str| {
            object
                .get(name)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| format!("edit {}: `{name}` has to be a string", index + 1))
        };
        edits.push(FileEdit {
            path: field("path")?,
            old: field("old")?,
            new: field("new")?,
        });
    }
    Ok(edits)
}

/// The edits as text, for the log, the judge and the next attempt. Not a unified diff: no line numbers, no
/// context, because the anchors are the context.
pub fn render(edits: &[FileEdit]) -> String {
    let mut text = String::new();
    for edit in edits {
        text.push_str(&format!("--- {}\n", edit.path));
        for line in edit.old.lines() {
            text.push('-');
            text.push_str(line);
            text.push('\n');
        }
        for line in edit.new.lines() {
            text.push('+');
            text.push_str(line);
            text.push('\n');
        }
    }
    text
}

/// Plan one file: resolve, read, apply every edit in memory, and hold the result.
pub fn plan(policy: &PathPolicy, path: &str, edits: &[Edit]) -> Result<Plan, String> {
    let resolved = policy
        .write(Path::new(path))
        .map_err(|reason| format!("Refused: {reason}."))?;
    plan_at(&resolved, edits)
}

fn plan_at(path: &Path, edits: &[Edit]) -> Result<Plan, String> {
    if edits.is_empty() {
        return Err("a patch needs at least one edit".to_string());
    }
    if edits.len() > MAX_EDITS {
        return Err(format!(
            "{} edits is more than one patch may carry (the most is {MAX_EDITS})",
            edits.len()
        ));
    }
    // Canonicalize first: temp-plus-rename replaces a symlink with a regular file unless the target is
    // named, which is a silent change nobody asked for.
    let path = std::fs::canonicalize(path)
        .map_err(|error| format!("could not resolve {}: {error}", path.display()))?;
    let mut text = std::fs::read_to_string(&path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let before = text.len();
    if before > CAP {
        return Err(format!(
            "{} is over 1 MiB; a patch is for an edit, not a rewrite",
            path.display()
        ));
    }

    let mut hunks = Vec::with_capacity(edits.len());
    for (index, edit) in edits.iter().enumerate() {
        if edit.old.is_empty() {
            return Err(format!(
                "edit {}: `old` is empty, and an empty anchor matches everywhere",
                index + 1
            ));
        }
        if edit.new.len() > CAP {
            return Err(format!("edit {}: `new` is over 1 MiB", index + 1));
        }
        let offset = once(&text, &edit.old, &path, index + 1)?;
        let line = text[..offset].bytes().filter(|byte| *byte == b'\n').count() + 1;
        hunks.push(Hunk {
            line,
            removed: lines(&edit.old),
            added: lines(&edit.new),
        });
        text.replace_range(offset..offset + edit.old.len(), &edit.new);
    }

    let after = text.len();
    let sent = edits
        .iter()
        .map(|edit| edit.old.len() + edit.new.len())
        .sum();
    Ok(Plan {
        path,
        text,
        before,
        after,
        hunks,
        sent,
    })
}

/// Plan every file in an answer, or refuse the whole answer. Nothing is written here.
///
/// Files are grouped by canonical path, so `a.txt` and `./a.txt` are one file and their edits apply in
/// the order the answer gave them.
///
/// A policy refusal comes back in the shape the floor refusal uses (`Refused: <reason>.`), so the model
/// reads a refusal rather than a path error; planning errors keep their own shapes.
pub fn plan_all(policy: &PathPolicy, edits: &[FileEdit]) -> Result<Vec<Plan>, String> {
    if edits.is_empty() {
        return Err("an edit answer needs at least one edit".to_string());
    }
    if edits.len() > MAX_EDITS {
        return Err(format!(
            "the answer carries {} edits, more than one answer may carry (the most is {MAX_EDITS})",
            edits.len()
        ));
    }
    let mut order: Vec<PathBuf> = Vec::new();
    let mut groups: std::collections::HashMap<PathBuf, Vec<Edit>> =
        std::collections::HashMap::new();
    for edit in edits {
        let resolved = policy
            .write(Path::new(&edit.path))
            .map_err(|reason| format!("Refused: {reason}."))?;
        let key = std::fs::canonicalize(&resolved)
            .map_err(|error| format!("could not resolve {}: {error}", resolved.display()))?;
        if !groups.contains_key(&key) {
            order.push(key.clone());
        }
        groups.entry(key).or_default().push(Edit {
            old: edit.old.clone(),
            new: edit.new.clone(),
        });
    }
    if order.len() > MAX_FILES {
        return Err(format!(
            "the answer names {} files, more than one answer may name (the most is {MAX_FILES})",
            order.len()
        ));
    }
    let mut plans = Vec::with_capacity(order.len());
    for path in order {
        let edits = groups.remove(&path).unwrap_or_default();
        match plan_at(&path, &edits) {
            Ok(plan) => plans.push(plan),
            Err(said) => return Err(format!("nothing was written: {said}")),
        }
    }
    Ok(plans)
}

/// Write every planned file, naming what landed if one fails. Returns the report the model reads.
pub fn write_all(plans: &[Plan]) -> Result<String, String> {
    let mut written: Vec<String> = Vec::new();
    for plan in plans {
        if let Err(said) = write_atomic(&plan.path, &plan.text) {
            return Err(if written.is_empty() {
                format!("nothing was written: {said}")
            } else {
                format!(
                    "wrote {} before the write failed: {said}",
                    written.join(", ")
                )
            });
        }
        written.push(plan.path.display().to_string());
    }
    let hunks: usize = plans.iter().map(|plan| plan.hunks.len()).sum();
    let before: usize = plans.iter().map(|plan| plan.before).sum();
    let after: usize = plans.iter().map(|plan| plan.after).sum();
    let sent: usize = plans.iter().map(|plan| plan.sent).sum();
    let files: Vec<String> = plans
        .iter()
        .map(|plan| format!("{} ({} edit(s))", plan.path.display(), plan.hunks.len()))
        .collect();
    Ok(format!(
        "patched {} file(s): {}; {} edit(s), {} -> {} bytes; {} bytes sent",
        plans.len(),
        files.join(", "),
        hunks,
        before,
        after,
        sent
    ))
}

/// Apply every edit to one file, or none. Kept as the one-file verb the tests and the chat tool use.
pub fn apply(path: &Path, edits: &[Edit]) -> Result<Applied, String> {
    let plan = plan_at(path, edits)?;
    write_atomic(&plan.path, &plan.text)?;
    Ok(Applied {
        before: plan.before,
        after: plan.after,
        hunks: plan.hunks,
    })
}

/// The one byte offset where `anchor` starts, or the refusal that says why there is not one.
fn once(text: &str, anchor: &str, path: &Path, edit: usize) -> Result<usize, String> {
    let mut found = text.match_indices(anchor);
    let Some((offset, _)) = found.next() else {
        // The one mismatch a model otherwise retries blindly: the file is CRLF and the anchor is LF.
        let normalised_file = text.replace("\r\n", "\n");
        let normalised_anchor = anchor.replace("\r\n", "\n");
        if normalised_file.matches(&normalised_anchor).count() == 1 {
            return Err(format!(
                "edit {edit}: the text was not found in {}; the file uses CRLF line endings, so `old` \
                 has to contain `\\r\\n`",
                path.display()
            ));
        }
        return Err(format!(
            "edit {edit}: the text to replace was not found in {}",
            path.display()
        ));
    };
    let matches = 1 + found.count();
    if matches > 1 {
        return Err(format!(
            "edit {edit}: the text to replace appears {matches} times in {}; add surrounding lines so it \
             names exactly one",
            path.display()
        ));
    }
    Ok(offset)
}

/// Lines in a piece of text, not counting the empty tail a final newline produces.
fn lines(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let parts = text.split('\n').count();
    if text.ends_with('\n') {
        parts - 1
    } else {
        parts
    }
}

/// Write once, through a temp file in the same directory, keeping the mode and syncing both the file and
/// the directory entry.
fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no directory to write in", path.display()))?;
    let metadata = std::fs::metadata(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let temp = parent.join(format!(
        ".{name}.hades-patch-{}-{}-{}",
        std::process::id(),
        TEMPS.fetch_add(1, Ordering::SeqCst),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0)
    ));

    let result = (|| -> Result<(), String> {
        let mut file = std::fs::File::create(&temp).map_err(|error| {
            format!(
                "could not create a temporary file in {}: {error}. A patch is atomic and needs a \
                 writable directory, even when the file itself is writable",
                parent.display()
            )
        })?;
        file.write_all(text.as_bytes())
            .map_err(|error| format!("could not write {}: {error}", temp.display()))?;
        file.sync_all()
            .map_err(|error| format!("could not sync {}: {error}", temp.display()))?;
        drop(file);
        std::fs::set_permissions(&temp, metadata.permissions())
            .map_err(|error| format!("could not keep the mode of {}: {error}", path.display()))?;
        std::fs::rename(&temp, path)
            .map_err(|error| format!("could not replace {}: {error}", path.display()))?;
        // The rename is visible; the directory entry itself is synced so a power loss does not resurrect
        // the old name with none of the new bytes.
        let directory = std::fs::File::open(parent)
            .map_err(|error| format!("could not open {}: {error}", parent.display()))?;
        directory
            .sync_all()
            .map_err(|error| format!("could not sync {}: {error}", parent.display()))?;
        Ok(())
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// The tool as the chat loop calls it: arguments in, the result the model reads back.
///
/// Public because the integration oracle drives it directly; `agent::execute` is private. The policy
/// comes from the world the tool runs in, and the path it may touch is the path the policy returns.
pub fn tool(args: &Value, policy: &PathPolicy) -> String {
    let path = args.get("path").and_then(Value::as_str).unwrap_or("");
    if path.is_empty() {
        return "no path given".to_string();
    }
    let Some(items) = args.get("edits").and_then(Value::as_array) else {
        return "`edits` has to be a list of {old, new} objects".to_string();
    };
    if items.is_empty() {
        return "`edits` is empty; a patch needs at least one edit".to_string();
    }
    let mut edits = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let Some(object) = item.as_object() else {
            return format!(
                "edit {}: expected an object with `old` and `new`",
                index + 1
            );
        };
        for key in object.keys() {
            if key != "old" && key != "new" {
                return format!("edit {}: unknown field `{key}`", index + 1);
            }
        }
        let old = match object.get("old").and_then(Value::as_str) {
            Some(old) => old.to_string(),
            None => return format!("edit {}: `old` has to be a string", index + 1),
        };
        let new = match object.get("new").and_then(Value::as_str) {
            Some(new) => new.to_string(),
            None => return format!("edit {}: `new` has to be a string", index + 1),
        };
        edits.push(Edit { old, new });
    }

    match plan(policy, path, &edits) {
        Ok(plan) => {
            let sent = plan.sent;
            match write_all(std::slice::from_ref(&plan)) {
                Ok(_) => {
                    let mut text = format!(
                        "patched {}: {} edit(s), {} -> {} bytes; {} bytes sent",
                        plan.path.display(),
                        plan.hunks.len(),
                        plan.before,
                        plan.after,
                        sent
                    );
                    for (index, hunk) in plan.hunks.iter().enumerate() {
                        text.push_str(&format!(
                            "\n  edit {}: line {}, -{} +{}",
                            index + 1,
                            hunk.line,
                            hunk.removed,
                            hunk.added
                        ));
                    }
                    text
                }
                Err(said) => said,
            }
        }
        Err(said) => said,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(old: &str, new: &str) -> Edit {
        Edit {
            old: old.to_string(),
            new: new.to_string(),
        }
    }

    #[test]
    fn one_anchor_changes_only_its_bytes() {
        let applied = apply_text("alpha\nbeta\ngamma\n", &[edit("beta", "BETA")]);
        assert_eq!(applied.unwrap().after, 17);
    }

    /// The pure rule, without a filesystem: apply the same loop to a string.
    fn apply_text(text: &str, edits: &[Edit]) -> Result<Applied, String> {
        let before = text.len();
        let mut text = text.to_string();
        let mut hunks = Vec::new();
        for (index, edit) in edits.iter().enumerate() {
            if edit.old.is_empty() {
                return Err(format!("edit {}: `old` is empty", index + 1));
            }
            let offset = once(&text, &edit.old, Path::new("memory"), index + 1)?;
            hunks.push(Hunk {
                line: text[..offset].bytes().filter(|byte| *byte == b'\n').count() + 1,
                removed: lines(&edit.old),
                added: lines(&edit.new),
            });
            text.replace_range(offset..offset + edit.old.len(), &edit.new);
        }
        Ok(Applied {
            before,
            after: text.len(),
            hunks,
        })
    }

    #[test]
    fn an_anchor_twice_is_refused() {
        let said = apply_text("x\nx\n", &[edit("x", "y")]).expect_err("ambiguous");
        assert!(said.contains("appears 2 times"), "{said}");
    }

    #[test]
    fn an_anchor_absent_is_refused() {
        let said = apply_text("alpha\n", &[edit("beta", "BETA")]).expect_err("absent");
        assert!(said.contains("not found"), "{said}");
    }

    #[test]
    fn an_empty_anchor_is_refused() {
        let said = apply_text("alpha\n", &[edit("", "BETA")]).expect_err("empty");
        assert!(said.contains("empty"), "{said}");
    }

    #[test]
    fn edits_apply_in_order() {
        let applied = apply_text(
            "one\ntwo\nthree\n",
            &[edit("one", "ONE"), edit("three", "THREE")],
        )
        .expect("both apply");
        assert_eq!(applied.hunks.len(), 2);
        assert_eq!(applied.hunks[0].line, 1);
        assert_eq!(applied.hunks[1].line, 3);
    }

    #[test]
    fn a_first_edit_can_make_the_second_ambiguous() {
        // The replacement introduces a second copy of the next anchor; the rule refuses rather than
        // guessing which one was meant.
        let said = apply_text("a\nb\n", &[edit("a", "b"), edit("b", "c")])
            .expect_err("the second anchor now appears twice");
        assert!(
            said.contains("edit 2") && said.contains("2 times"),
            "{said}"
        );
    }

    #[test]
    fn a_crlf_file_asks_for_crlf_in_the_anchor() {
        let said =
            apply_text("alpha\r\nbeta\r\n", &[edit("alpha\n", "ALPHA\n")]).expect_err("CRLF");
        assert!(said.contains("CRLF"), "{said}");
    }

    #[test]
    fn line_counts_ignore_the_final_newline() {
        assert_eq!(lines("a"), 1);
        assert_eq!(lines("a\n"), 1);
        assert_eq!(lines("a\nb\n"), 2);
        assert_eq!(lines(""), 0);
    }

    #[test]
    fn an_edit_object_is_read_and_rendered() {
        let said = edits_from(r#"{"edits":[{"path":"a.txt","old":"old","new":"new"}]}"#)
            .expect("edit-shaped")
            .expect("readable");
        assert_eq!(said.len(), 1);
        assert_eq!(said[0].path, "a.txt");
        let rendered = render(&said);
        assert!(rendered.contains("--- a.txt"), "{rendered}");
        assert!(rendered.contains("-old"), "{rendered}");
        assert!(rendered.contains("+new"), "{rendered}");
    }

    #[test]
    fn a_brace_group_is_still_a_program() {
        assert!(edits_from(r#"{ echo "edits"; }"#).is_none());
    }

    #[test]
    fn a_malformed_edit_object_is_named_rather_than_run() {
        let said = edits_from(r#"{"edits":[{"path":"a.txt","old":}]}"#)
            .expect("edit-shaped")
            .expect_err("unreadable");
        assert!(said.contains("could not be read"), "{said}");
    }

    #[test]
    fn an_unknown_field_in_an_edit_object_is_named() {
        let said = edits_from(r#"{"edits":[],"path":"a.txt"}"#)
            .expect("edit-shaped")
            .expect_err("unknown key");
        assert!(said.contains("unknown field `path`"), "{said}");
    }

    #[test]
    fn a_plain_json_object_is_not_an_edit_answer() {
        assert!(edits_from(r#"{"answer": 1}"#).is_none());
    }

    #[test]
    fn more_edits_than_the_cap_are_refused_by_name() {
        let many: Vec<Edit> = (0..MAX_EDITS + 1).map(|_| edit("a", "b")).collect();
        let said = plan_at(Path::new("/nonexistent"), &many).expect_err("cap");
        assert!(said.contains("more than one patch"), "{said}");
    }
}
