//! Putting a run's writes back.
//!
//! The journal records what was there before a step changed it. This module reads that record, decides
//! what putting it back means for each path, and refuses when doing so would destroy newer work.
//!
//! The refusal is the feature, not a safety rail on it. A rewind that trusted the journal alone would
//! silently overwrite anything a person wrote after the run, and "undo" would be a command that costs work
//! it was asked to save. So every path is hashed as it stands now and compared with the digest the entry
//! recorded as what the write left. Equal means the file is exactly what the run made, and undoing it
//! loses nothing. Different means somebody edited it since, and the command names the path and both
//! digests and stops. `--force` is the deliberate override, and it is spelled out because it is a
//! different decision.
//!
//! Two things are deliberately not here. A rewind is a person's command, so it never runs inside a turn.
//! And the journal is not written to while rewinding: undoing is not a run, and a run journal that
//! recorded its own undo would need a second journal to undo that.

use crate::journal::{Journal, Kind};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

/// What a rewind was asked for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    /// Leave the tree as it stood after this step: every change captured at a later step is undone, and
    /// the changes the run made at or before it are kept. `--to 0` therefore undoes everything the run did
    /// after its first step, which is what a person means by "put it back to the beginning".
    pub to: Option<u64>,
    /// Only paths matching this glob. Matched against the whole path and against any suffix that starts
    /// after a `/`, so `--path '*.md'` and `--path notes/*.md` both do what a person means.
    pub path: Option<String>,
    /// Put the recorded bytes back even when the file moved on since the run.
    pub force: bool,
    /// Print the plan and write nothing.
    pub dry_run: bool,
}

/// What putting one path back means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Write the blob the entry names over the path.
    Restore { digest: String },
    /// The path did not exist before the step, so putting it back removes it. An empty file and a file
    /// that should not exist are different outcomes.
    Remove,
}

impl Action {
    pub fn verb(&self) -> &'static str {
        match self {
            Action::Restore { .. } => "restore",
            Action::Remove => "remove",
        }
    }
}

/// One path the rewind will touch, with the evidence for touching it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    pub path: String,
    /// The step whose capture is being undone: the oldest entry the filters allowed for this path.
    pub step: u64,
    pub tool: String,
    pub action: Action,
    /// The digest the entry recorded as what the run left, from the *last* capture of the path.
    pub expected: String,
    /// The step that digest came from. Carried so a refusal quotes the digest beside its own step rather
    /// than beside the step whose bytes are being put back.
    pub expected_step: u64,
    /// The digest the path has now, or empty when it is not there.
    pub found: String,
    /// Why the path could not be hashed now, when there is something there that cannot be read. Kept apart
    /// from `found` so "I could not look" is never reported as "there is nothing".
    pub unreadable: Option<String>,
    /// Whether those two differ, which is the whole reason this command can refuse.
    pub drifted: bool,
}

/// A path the journal names but cannot put back, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub path: String,
    pub step: u64,
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub run: String,
    pub steps: Vec<Planned>,
    pub skipped: Vec<Skipped>,
}

impl Plan {
    /// The paths that moved on since the run.
    pub fn drifted(&self) -> Vec<&Planned> {
        self.steps.iter().filter(|step| step.drifted).collect()
    }

    /// Whether this plan can be carried out without `--force`.
    pub fn blocked(&self) -> bool {
        self.steps.iter().any(|step| step.drifted)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub restored: Vec<String>,
    pub removed: Vec<String>,
    pub skipped: Vec<Skipped>,
}

impl Report {
    pub fn touched(&self) -> usize {
        self.restored.len() + self.removed.len()
    }

    /// The exit status and the line that says why, for a caller that has to report to a script as well as to
    /// a person.
    ///
    /// Three outcomes, because two of them used to be reported as success. `0` is everything the plan named
    /// was put back. `2` is a partial undo: some paths were written and some were skipped, which is a real
    /// answer and a different one. `1` is nothing recoverable touched at all, which is the failure mode a
    /// recovery tool must never print as success.
    pub fn status(&self) -> (i32, String) {
        let touched = self.touched();
        let skipped = self.skipped.len();
        match (touched, skipped) {
            (_, 0) => (0, String::new()),
            (0, skipped) => (
                1,
                format!(
                    "nothing recoverable was put back: {skipped} path(s) skipped, 0 restored, 0 removed\n"
                ),
            ),
            (touched, skipped) => (
                2,
                format!(
                    "partial undo: {touched} path(s) put back, {skipped} skipped, and the skipped ones are named above with why\n"
                ),
            ),
        }
    }
}

/// Decide what a rewind would do. Reads the journal and hashes the tree; writes nothing.
///
/// Two entries per path matter, and they are not the same one. The *first* capture says what to restore:
/// undoing a run means returning the path to the state it was in before the run touched it, so a path
/// written twice comes back as its oldest recorded `before` rather than as the middle version. The *last*
/// capture says what to compare against: the question the refusal asks is whether anything changed after
/// the run finished with that path, and the run's final `after` is the only digest that answers it. Using
/// one entry for both would refuse every path a run wrote more than once.
pub fn plan(journal: &Journal, options: &Options) -> Plan {
    let mut planned = Plan {
        run: journal.run().to_string(),
        ..Plan::default()
    };
    // Sorted by path, so a plan reads the same way twice and a partial failure is reproducible.
    let entries = journal.entries();
    // (first capture, last capture) for each path, in path order.
    let mut spans: BTreeMap<&str, (&crate::journal::Entry, &crate::journal::Entry)> =
        BTreeMap::new();
    for entry in entries.iter() {
        if let Some(to) = options.to
            && entry.step <= to
        {
            continue;
        }
        if let Some(pattern) = &options.path
            && !matches(pattern, &entry.path)
        {
            continue;
        }
        spans
            .entry(entry.path.as_str())
            .and_modify(|(_, last)| *last = entry)
            .or_insert((entry, entry));
    }
    for (first, last) in spans.into_values() {
        if !first.kind.recoverable() {
            planned.skipped.push(Skipped {
                path: first.path.clone(),
                step: first.step,
                reason: format!(
                    "recorded as {} at step {}: no bytes were kept, so this path cannot be rewound",
                    first.kind.name(),
                    first.step
                ),
            });
            continue;
        }
        let (found, unreadable) = match digest_at(Path::new(&first.path)) {
            Ok(Some(found)) => (found, None),
            Ok(None) => (String::new(), None),
            Err(said) => (String::new(), Some(said)),
        };
        // The whole reason this command can refuse: the path is hashed as it stands now and compared with
        // the digest the run recorded as what it left. Anything else is somebody's newer work, or something
        // that cannot be checked at all, and both are reasons not to write over it.
        let drifted = unreadable.is_some() || found != last.after;
        planned.steps.push(Planned {
            path: first.path.clone(),
            step: first.step,
            tool: first.tool.clone(),
            action: match first.kind {
                Kind::Absent => Action::Remove,
                _ => Action::Restore {
                    digest: first.before.clone(),
                },
            },
            expected: last.after.clone(),
            expected_step: last.step,
            found,
            unreadable,
            drifted,
        });
    }
    planned
}

/// Carry out a plan. Refuses, naming every drifted path with both digests, unless `force` was asked for.
///
/// `Err` means exactly one thing: the refusal, and nothing was written. A path that cannot be put back
/// while the loop runs — a missing blob, bytes that no longer hash to their name, a directory that will
/// not take the write — is a skip in the report, because a failure on the third path is not a reason to
/// lose the first two. Reporting such a failure as a refusal printed "would restore" beside a file the
/// command had already restored. That is why [`run`] returns a report and cannot fail.
///
/// The drift check happens in [`plan`]; this repeats it as a guard because a caller can build a `Plan` by
/// hand and the cost of refusing twice is nothing next to restoring over somebody's edit.
pub fn apply(journal: &Journal, plan: &Plan, force: bool) -> Result<Report, String> {
    if force {
        return Ok(run(journal, plan));
    }
    if plan.blocked() {
        let mut said = String::from("refused: the run's work cannot be verified as it was left:\n");
        for step in plan.drifted() {
            said.push_str(&format!(
                "  {}\n    recorded at step {}: {}\n",
                step.path,
                step.expected_step,
                digest(&step.expected)
            ));
            // Which of the two findings it is, said plainly: a file that cannot be looked at is not a file
            // that is gone, and the difference decides whether the person goes looking for an edit or for a
            // permission.
            match &step.unreadable {
                Some(why) => said.push_str(&format!("    could not be read now: {why}\n")),
                None => said.push_str(&format!(
                    "    found now:          {}\n",
                    digest(&step.found)
                )),
            }
        }
        said.push_str(
            "Nothing was written. Put these bytes back anyway with --force, or rewind a path at a time \
             with --path.",
        );
        return Err(said);
    }
    Ok(run(journal, plan))
}

/// Do the undo, path by path. It cannot fail as a whole: a path that will not go back is named in the
/// report with its reason and the loop carries on, because the entries beside it are still recoverable and
/// a person running a recovery command needs everything the command could get back, not the prefix of it
/// before the first permission error.
fn run(journal: &Journal, plan: &Plan) -> Report {
    let mut report = Report {
        skipped: plan.skipped.clone(),
        ..Report::default()
    };
    for step in &plan.steps {
        match &step.action {
            Action::Restore { digest } => {
                let blob = journal.blob(digest);
                let bytes = match std::fs::read(&blob) {
                    Ok(bytes) => bytes,
                    // A blob that is gone is one path's problem, not the undo's: the entries beside it can
                    // still be put back, and the reason says which path was lost and where it was looked for.
                    Err(error) => {
                        report.skipped.push(Skipped {
                            path: step.path.clone(),
                            step: step.step,
                            reason: format!(
                                "the kept bytes are not readable at {}: {error}",
                                blob.display()
                            ),
                        });
                        continue;
                    }
                };
                // The name of a blob is the digest of its bytes, so a blob that does not hash to the digest
                // the entry names is not the file that was captured. Writing it would be a restore the
                // journal cannot account for, which is the one thing this command exists not to do.
                let hashed = mneme::sha256::hex(&bytes);
                if hashed != *digest {
                    report.skipped.push(Skipped {
                        path: step.path.clone(),
                        step: step.step,
                        reason: format!(
                            "the stored bytes do not hash to the digest the entry names: {digest} kept, {hashed} stored"
                        ),
                    });
                    continue;
                }
                match restore(Path::new(&step.path), &bytes) {
                    Ok(()) => report.restored.push(step.path.clone()),
                    // One path that will not take the write is that path's problem. The undo says which
                    // one, says why, and puts back everything else it can.
                    Err(said) => report.skipped.push(Skipped {
                        path: step.path.clone(),
                        step: step.step,
                        reason: said,
                    }),
                }
            }
            Action::Remove => {
                match std::fs::remove_file(&step.path) {
                    Ok(()) => report.removed.push(step.path.clone()),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        // Nothing to undo: the path the run created is already gone.
                    }
                    Err(error) => report.skipped.push(Skipped {
                        path: step.path.clone(),
                        step: step.step,
                        reason: format!("could not remove {}: {error}", step.path),
                    }),
                }
            }
        }
    }
    report
}

/// Write the bytes beside the path and rename, so a reader sees the old file or the whole new one.
fn restore(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no directory to write in", path.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let temp = parent.join(format!(
        ".{name}.hades-rewind-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::write(&temp, bytes)
        .and_then(|()| std::fs::rename(&temp, path))
        .map_err(|error| {
            let _ = std::fs::remove_file(&temp);
            format!("could not restore {}: {error}", path.display())
        })
}

/// The digest of a file as it stands.
///
/// `Ok(None)` is "nothing is there", which is a fact about the tree. `Err` is "there is something there and
/// it could not be read", which is a different fact: reporting the second as the first turns a broken file
/// into a deleted one, and sends the person reading the refusal looking for the wrong thing.
///
/// Streamed rather than read whole: a file a run wrote can be large, and the check only needs the bytes
/// once through the hasher.
pub fn digest_at(path: &Path) -> Result<Option<String>, String> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        // Only "not found" is absence. A permission refusal is not an absent file either.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    let mut digest = mneme::sha256::Digest::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => digest.update(&buffer[..read]),
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(Some(digest.finish()))
}

/// A digest for a person to read: short enough to compare by eye, or a word when there is none.
fn digest(value: &str) -> String {
    if value.is_empty() {
        return "(nothing recorded)".to_string();
    }
    value.to_string()
}

/// Whether a path matches a glob: the whole path, or any suffix starting after a `/`.
pub fn matches(pattern: &str, path: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    if glob(&pattern, &path.chars().collect::<Vec<char>>()) {
        return true;
    }
    for (index, _) in path.match_indices('/') {
        if glob(&pattern, &path[index + 1..].chars().collect::<Vec<char>>()) {
            return true;
        }
    }
    false
}

fn glob(pattern: &[char], text: &[char]) -> bool {
    match pattern.split_first() {
        None => text.is_empty(),
        Some(('*', rest)) => {
            let mut tail = text;
            loop {
                if glob(rest, tail) {
                    return true;
                }
                if tail.is_empty() {
                    return false;
                }
                tail = &tail[1..];
            }
        }
        Some(('?', rest)) => !text.is_empty() && glob(rest, &text[1..]),
        Some((first, rest)) => {
            text.first() == Some(first) && glob(rest, text.get(1..).unwrap_or(&[]))
        }
    }
}

/// How a plan is being shown to a person.
///
/// A plan is always what the command would do; only [`Mode::Applied`] describes what it did. Keeping the
/// two apart is what stops a refusal from printing "restore <path>" directly above "Nothing was written".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// A dry run: the plan with the conditional, and the line that says nothing was written.
    DryRun,
    /// A refusal: the plan with the conditional, because none of it happened.
    Refused,
    /// What the command did, with the report's own counts.
    Applied,
}

/// The plan as a person reads it, before or after it ran.
pub fn render(plan: &Plan, report: Option<&Report>, mode: Mode) -> String {
    let mut text = String::new();
    if plan.steps.is_empty() && plan.skipped.is_empty() {
        text.push_str(&format!(
            "run {}: the journal records nothing to rewind\n",
            plan.run
        ));
        return text;
    }
    let would = !matches!(mode, Mode::Applied);
    // What the undo actually wrote, when there is a report to ask. A step the report does not name was
    // not written, and a line that reads "restore <path>" beside "1 skipped" is the same lie as a status
    // that says the undo worked.
    let written: Option<std::collections::BTreeSet<&str>> = report.map(|report| {
        report
            .restored
            .iter()
            .chain(report.removed.iter())
            .map(String::as_str)
            .collect()
    });
    for step in &plan.steps {
        let mark = if step.drifted { " (changed since)" } else { "" };
        let unwritten = match &written {
            Some(written) if !written.contains(step.path.as_str()) => " (not written)",
            _ => "",
        };
        text.push_str(&format!(
            "{:<9}{:<8} {} (step {}, {}){mark}{unwritten}\n",
            if would { "would" } else { "" },
            step.action.verb(),
            step.path,
            step.step,
            step.tool
        ));
    }
    // The skips the undo is responsible for, not only the ones the plan found: a blob that went missing
    // between planning and writing is discovered while the loop runs, and its reason is the thing a person
    // needs at 2 a.m. `Report` starts from the plan's skips, so this prints each one once.
    let skipped: &[Skipped] = match report {
        Some(report) => &report.skipped,
        None => &plan.skipped,
    };
    for skip in skipped {
        text.push_str(&format!("skip     {}: {}\n", skip.path, skip.reason));
    }
    match report {
        Some(report) => text.push_str(&format!(
            "{} restored, {} removed, {} skipped\n",
            report.restored.len(),
            report.removed.len(),
            report.skipped.len()
        )),
        None if mode == Mode::DryRun => text.push_str("nothing was written (--dry-run)\n"),
        None => {}
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::{Capture, Journal};

    fn scratch(name: &str) -> std::path::PathBuf {
        crate::scratch::unique(name)
    }

    /// A write through the real capture, which is the door both mutating tools use. The fixture is never
    /// a hand written journal line: a hand written line proves the reader and nothing else.
    fn write(journal: &Journal, path: &Path, before: &str, after: &str, step: u64) {
        std::fs::write(path, before).expect("the bytes before");
        Capture::new(journal, step, "write_file")
            .write::<std::io::Error, _>(path, after.as_bytes(), || std::fs::write(path, after))
            .expect("the write");
    }

    /// A path a run created, through the same capture.
    fn create(journal: &Journal, path: &Path, content: &str, step: u64) {
        Capture::new(journal, step, "write_file")
            .write::<std::io::Error, _>(path, content.as_bytes(), || std::fs::write(path, content))
            .expect("the write");
    }

    fn plan_of(journal: &Journal, options: Options) -> Plan {
        plan(journal, &options)
    }

    /// The refusal is the feature. A rewind that trusted the journal would overwrite whatever a person
    /// wrote after the run, so this names the path and both digests and writes nothing.
    #[test]
    fn a_rewind_refuses_when_the_file_changed_after_the_run() {
        let dir = scratch("rewind-drift");
        let journal = Journal::at(&dir, "run-drift");
        let path = dir.join("note.txt");
        write(
            &journal,
            &path,
            "the run left this\n",
            "the run's own bytes\n",
            0,
        );
        let recorded = digest_at(&path)
            .expect("the file can be read")
            .expect("the file the run left");
        std::fs::write(&path, "a person wrote this after\n").expect("a newer edit");

        let planned = plan_of(
            &journal,
            Options {
                ..Options::default()
            },
        );
        assert_eq!(planned.steps.len(), 1, "{planned:?}");
        assert!(
            planned.steps[0].drifted,
            "the file does not hold what the run left: {planned:?}"
        );
        assert_eq!(planned.steps[0].expected, recorded);

        let refused = apply(&journal, &planned, false).expect_err("a refusal");
        let now = mneme::sha256::hex(b"a person wrote this after\n");
        assert!(refused.contains(&recorded), "both digests: {refused}");
        assert!(refused.contains(&now), "both digests: {refused}");
        assert!(refused.contains("note.txt"), "the path: {refused}");
        assert_eq!(
            std::fs::read_to_string(&path).expect("the newer edit"),
            "a person wrote this after\n",
            "nothing was written"
        );

        // The deliberate override, and it puts the run's predecessor back.
        let report = apply(&journal, &planned, true).expect("forced");
        assert_eq!(report.restored.len(), 1);
        assert_eq!(
            std::fs::read_to_string(&path).expect("the restored file"),
            "the run left this\n"
        );
    }

    /// Bytes come back identical, and a file the run created is removed rather than left empty.
    #[test]
    fn a_rewind_restores_the_bytes_and_removes_what_the_run_created() {
        let dir = scratch("rewind-restore");
        let journal = Journal::at(&dir, "run-restore");
        let kept = dir.join("kept.txt");
        let made = dir.join("made.txt");
        std::fs::write(&kept, "the original bytes\n").expect("a file");
        let before = digest_at(&kept)
            .expect("a readable file")
            .expect("a digest");
        write(&journal, &kept, "the original bytes\n", "rewritten\n", 0);
        create(&journal, &made, "created by the run\n", 1);

        let planned = plan_of(&journal, Options::default());
        assert_eq!(planned.steps.len(), 2, "{planned:?}");
        let report = apply(&journal, &planned, false).expect("a rewind");

        assert_eq!(report.restored, vec![kept.display().to_string()]);
        assert_eq!(report.removed, vec![made.display().to_string()]);
        assert_eq!(
            digest_at(&kept).expect("a readable file"),
            Some(before),
            "bytes come back identical"
        );
        assert!(!made.exists(), "a created file is removed, not blanked");
    }

    /// `--to` leaves the tree as it stood after that step, which for a path written twice is the middle
    /// version rather than the first or the last.
    #[test]
    fn a_rewind_to_a_step_leaves_the_tree_as_it_stood_then() {
        let dir = scratch("rewind-to");
        let journal = Journal::at(&dir, "run-to");
        let path = dir.join("note.txt");
        write(&journal, &path, "v1\n", "v2\n", 0);
        write(&journal, &path, "v2\n", "v3\n", 1);

        let all = plan_of(&journal, Options::default());
        apply(&journal, &all, false).expect("a full rewind");
        assert_eq!(std::fs::read_to_string(&path).expect("v1"), "v1\n");

        write(&journal, &path, "v2\n", "v3\n", 1);
        assert_eq!(std::fs::read_to_string(&path).expect("v3"), "v3\n");
        let middle = plan_of(
            &journal,
            Options {
                to: Some(0),
                ..Options::default()
            },
        );
        assert_eq!(middle.steps.len(), 1, "{middle:?}");
        assert_eq!(middle.steps[0].step, 1);
        apply(&journal, &middle, false).expect("a rewind to step 0");
        assert_eq!(
            std::fs::read_to_string(&path).expect("v2"),
            "v2\n",
            "the state after step 0, not the beginning"
        );

        // Past the last step there is nothing to undo, which is a fact worth saying rather than an error.
        let nothing = plan_of(
            &journal,
            Options {
                to: Some(9),
                ..Options::default()
            },
        );
        assert!(nothing.steps.is_empty(), "{nothing:?}");
        assert!(render(&nothing, None, Mode::Applied).contains("nothing to rewind"));
    }

    /// A pattern picks the paths it names and leaves the rest alone.
    #[test]
    fn a_path_glob_rewinds_only_what_it_matches() {
        let dir = scratch("rewind-glob");
        let journal = Journal::at(&dir, "run-glob");
        let notes = dir.join("notes");
        std::fs::create_dir_all(&notes).expect("a directory");
        let one = notes.join("one.md");
        let two = notes.join("two.txt");
        write(&journal, &one, "one before\n", "one after\n", 0);
        write(&journal, &two, "two before\n", "two after\n", 1);

        let planned = plan_of(
            &journal,
            Options {
                path: Some("*.md".to_string()),
                ..Options::default()
            },
        );
        assert_eq!(planned.steps.len(), 1, "{planned:?}");
        assert_eq!(planned.steps[0].path, one.display().to_string());
        apply(&journal, &planned, false).expect("a rewind");
        assert_eq!(std::fs::read_to_string(&one).expect("one"), "one before\n");
        assert_eq!(
            std::fs::read_to_string(&two).expect("two"),
            "two after\n",
            "a path outside the pattern is untouched"
        );
    }

    /// A capture with no bytes behind it is named, not quietly skipped: a rewind that reported success
    /// having restored nothing is worse than one that refuses.
    #[test]
    fn a_path_with_no_kept_bytes_is_named_and_not_restored() {
        let dir = scratch("rewind-secret");
        let journal = Journal::at(&dir, "run-secret");
        let path = dir.join(".env");
        std::fs::write(&path, "TOKEN=one\n").expect("a file that looks like a key");
        Capture::new(&journal, 0, "write_file")
            .write::<std::io::Error, _>(&path, b"TOKEN=two\n", || {
                std::fs::write(&path, "TOKEN=two\n")
            })
            .expect("the write");

        let planned = plan_of(&journal, Options::default());
        assert!(planned.steps.is_empty(), "{planned:?}");
        assert_eq!(planned.skipped.len(), 1, "{planned:?}");
        assert!(planned.skipped[0].reason.contains("secret"), "{planned:?}");
        let text = render(&planned, None, Mode::Applied);
        assert!(text.contains("cannot be rewound"), "{text}");
    }

    /// A blob is named by the digest of the bytes it holds. If the bytes no longer hash to that name, the
    /// journal's content addressing is broken and writing them over a good file would be a restore nobody
    /// could prove. The path is named and skipped instead.
    #[test]
    fn a_blob_that_does_not_hash_to_its_name_is_never_written_over_the_file() {
        let dir = scratch("rewind-corrupt");
        let journal = Journal::at(&dir, "run-corrupt");
        let path = dir.join("note.txt");
        write(
            &journal,
            &path,
            "the run left this\n",
            "the run's bytes\n",
            0,
        );
        let blob = journal.blob(&mneme::sha256::hex(b"the run left this\n"));
        let mut bytes = std::fs::read(&blob).expect("the stored bytes");
        bytes.extend_from_slice(b"and some junk on the end");
        std::fs::write(&blob, &bytes).expect("a blob that no longer matches its name");

        let planned = plan_of(&journal, Options::default());
        assert_eq!(planned.steps.len(), 1, "{planned:?}");
        let report = apply(&journal, &planned, false)
            .expect("a rewind that will not write bytes it cannot account for");
        assert!(
            report.restored.is_empty(),
            "it reported a restore it did not make: {report:?}"
        );
        assert_eq!(report.skipped.len(), 1, "{report:?}");
        assert!(
            report.skipped[0]
                .reason
                .contains("do not hash to the digest the entry names"),
            "{}",
            report.skipped[0].reason
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("the file the run left"),
            "the run's bytes\n",
            "the corrupt bytes were written over the run's file"
        );
    }

    /// A blob that is not there at all is a skip with a reason, not a failure that takes the rest of the
    /// undo with it: one lost blob does not mean the other paths cannot be put back.
    #[test]
    fn a_missing_blob_skips_its_path_and_the_rest_still_rewinds() {
        let dir = scratch("rewind-missing-blob");
        let journal = Journal::at(&dir, "run-missing");
        let kept = dir.join("kept.txt");
        let lost = dir.join("lost.txt");
        write(&journal, &kept, "kept before\n", "kept after\n", 0);
        write(&journal, &lost, "lost before\n", "lost after\n", 1);
        std::fs::remove_file(journal.blob(&mneme::sha256::hex(b"lost before\n")))
            .expect("a blob that went missing");

        let planned = plan_of(&journal, Options::default());
        let report = apply(&journal, &planned, false).expect("the paths it can account for");
        assert_eq!(report.restored, vec![kept.display().to_string()]);
        assert_eq!(report.skipped.len(), 1, "{report:?}");
        assert_eq!(report.skipped[0].path, lost.display().to_string());
        assert_eq!(
            std::fs::read_to_string(&kept).expect("kept"),
            "kept before\n",
            "the path with bytes was restored"
        );
        assert_eq!(
            std::fs::read_to_string(&lost).expect("lost"),
            "lost after\n",
            "a path with no bytes left is named, not blanked"
        );
    }

    /// The refusal is about the digest the run left, and that digest came from the last capture of the path,
    /// not the first. Quoting it beside the first capture's step attributes a step-1 digest to step 0, which
    /// is the wrong line in the bug report a person pastes.
    #[test]
    fn a_refusal_names_the_step_the_digest_it_compared_came_from() {
        let dir = scratch("rewind-drift-step");
        let journal = Journal::at(&dir, "run-drift-step");
        let path = dir.join("note.txt");
        write(&journal, &path, "v1\n", "v2\n", 0);
        write(&journal, &path, "v2\n", "v3\n", 1);
        std::fs::write(&path, "a person wrote this\n").expect("a newer edit");

        let planned = plan_of(&journal, Options::default());
        assert_eq!(planned.steps.len(), 1, "{planned:?}");
        assert_eq!(
            planned.steps[0].step, 0,
            "the restore is still the oldest capture's"
        );
        assert_eq!(
            planned.steps[0].expected_step, 1,
            "the digest compared is the last capture's"
        );
        let refused = apply(&journal, &planned, false).expect_err("a refusal");
        assert!(
            refused.contains("recorded at step 1"),
            "the digest it compared is step 1's: {refused}"
        );
        assert!(
            !refused.contains("recorded at step 0"),
            "the first capture's step is beside a digest the last capture recorded: {refused}"
        );
    }

    /// "Could not read it" and "it is not there" are different findings, and a refusal that reported the
    /// first as the second would send a person looking for a deleted file instead of a broken one.
    #[test]
    fn a_path_that_cannot_be_read_now_is_refused_as_unreadable_not_as_absent() {
        let dir = scratch("rewind-unreadable");
        let journal = Journal::at(&dir, "run-unreadable");
        let path = dir.join("note.txt");
        write(&journal, &path, "before\n", "after\n", 0);
        std::fs::remove_file(&path).expect("the file the run wrote");
        // Something that is there and cannot be hashed as a file: a directory where the file was.
        std::fs::create_dir(&path).expect("a directory in the file's place");

        let planned = plan_of(&journal, Options::default());
        assert_eq!(planned.steps.len(), 1, "{planned:?}");
        assert!(
            planned.steps[0].drifted,
            "an unverifiable path is not put back: {planned:?}"
        );
        let refused = apply(&journal, &planned, false).expect_err("a refusal");
        assert!(
            refused.contains("could not be read now"),
            "the refusal says why it could not check: {refused}"
        );
        assert!(
            !refused.contains("(nothing recorded)"),
            "an unreadable path is not an absent one: {refused}"
        );
    }

    /// A skip found while the undo runs — a blob that is gone, or bytes that no longer hash to the name
    /// the entry gives them — is printed with its reason, not only counted. The partial sentence tells the
    /// reader "the skipped ones are named above with why", so a skip that is counted and not named makes
    /// that sentence a lie, and at 2 a.m. the reason is the only thing the reader needs.
    #[test]
    fn a_skip_found_during_the_undo_is_printed_with_its_reason() {
        let dir = scratch("rewind-apply-skip-reason");
        let journal = Journal::at(&dir, "run-apply-skip");
        let kept = dir.join("kept.txt");
        let lost = dir.join("lost.txt");
        write(&journal, &kept, "kept before\n", "kept after\n", 0);
        write(&journal, &lost, "lost before\n", "lost after\n", 1);
        std::fs::remove_file(journal.blob(&mneme::sha256::hex(b"lost before\n")))
            .expect("a blob that went missing");

        let planned = plan_of(&journal, Options::default());
        let report = apply(&journal, &planned, false).expect("the paths it can account for");
        let (code, said) = report.status();
        assert_eq!(
            code, 2,
            "one path put back and one skipped is a partial undo"
        );
        let text = format!("{}{said}", render(&planned, Some(&report), Mode::Applied));
        assert!(
            text.contains("lost.txt"),
            "the skipped path is named: {text}"
        );
        assert!(
            text.contains("kept bytes are not readable"),
            "the reason it was skipped is printed, not just counted: {text}"
        );
        assert!(
            text.contains("named above with why"),
            "the sentence the report ends with still describes what was printed: {text}"
        );
    }

    /// The pattern matcher, on the two shapes a person types.
    #[test]
    fn a_pattern_matches_a_whole_path_or_a_trailing_run_of_components() {
        assert!(matches("*.md", "/work/notes/one.md"));
        assert!(matches("notes/*.md", "/work/notes/one.md"));
        assert!(matches("/work/*/one.md", "/work/notes/one.md"));
        assert!(!matches("*.txt", "/work/notes/one.md"));
        assert!(
            matches("notes/*.md", "/work/other/notes/one.md"),
            "a relative pattern matches at any directory boundary, which is what a person typing one means"
        );
        assert!(matches("one.md", "/work/notes/one.md"));
        assert!(matches("note?.md", "/work/notes/note1.md"));
    }
}
