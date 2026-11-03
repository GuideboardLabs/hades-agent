//! The run journal: the bytes a step is about to overwrite, kept so the step can be undone.
//!
//! One directory per run under `<state>/journal/<run>/`:
//!
//! - `index.jsonl` is append only, one line per captured path. A line is
//!   `{at, run, step, tool, path, before, after, kind, bytes}`: when, which run and step, which tool,
//!   which path, the digest of the bytes it held, the digest of the bytes that replaced them, what kind
//!   of capture it was, and how many bytes the path held when it was read.
//! - `blobs/<sha256>` holds the previous bytes, stored once per digest. Content addressing is what keeps
//!   a run that rewrites one file twenty times from costing twenty copies: the second capture of the same
//!   bytes finds the blob already there.
//!
//! The shape mirrors [`crate::activity`]: one line per event, append only, and a line that will not parse
//! is skipped rather than taking the rest of the history with it. A journal is written by a process that
//! can be killed between the `write` and the `fsync`, so a torn tail is the ordinary case, not the
//! exceptional one.
//!
//! Two limits are stated here because they are the difference between an honest record and a false one:
//!
//! - A file over [`MAX_BLOB`] is recorded as [`Kind::TooLarge`] with its size and no bytes. A rewind names
//!   that path as not recoverable instead of reporting a restore it did not make.
//! - A path named like a credential is recorded as [`Kind::Secret`] with its size and no bytes, and is
//!   never opened. The journal must not become the second copy of a key.
//!
//! Hashing is [`mneme::sha256`], the same path the pack installer uses, so the journal adds no dependency
//! and no second answer to "what is this file".

use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// The index file inside a run's directory.
pub const INDEX: &str = "index.jsonl";
/// The directory inside a run's directory that holds captured bytes.
pub const BLOBS: &str = "blobs";

/// The largest file whose previous bytes are stored. Above this the path is named as [`Kind::TooLarge`]
/// and no bytes are kept: a journal that fills the disk it is protecting is a second outage.
pub const MAX_BLOB: u64 = 8 * 1024 * 1024;

/// What the capture found at the path before the write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Ordinary file: its previous bytes are in `blobs/<before>`.
    File,
    /// Nothing was there, so a rewind removes whatever the step created.
    Absent,
    /// A file past [`MAX_BLOB`]. Named, sized, and not recoverable.
    TooLarge,
    /// A name that looks like a credential. Named, sized, never read, and not recoverable.
    Secret,
}

impl Kind {
    pub fn name(&self) -> &'static str {
        match self {
            Kind::File => "file",
            Kind::Absent => "absent",
            Kind::TooLarge => "too_large",
            Kind::Secret => "secret",
        }
    }

    /// Whether a rewind can put this path back from what the journal kept.
    pub fn recoverable(&self) -> bool {
        matches!(self, Kind::File | Kind::Absent)
    }
}

/// One captured path, as it appears in the index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Unix seconds, the same clock the activity ledger uses.
    pub at: i64,
    /// Which run this belongs to, as the journal directory spells it.
    pub run: String,
    /// Which step of the run captured this. `hades rewind --to <step>` leaves the tree as it stood after
    /// that step, so the entries it undoes are the ones at a later step.
    pub step: u64,
    /// The tool that asked to write.
    pub tool: String,
    /// The path, resolved: the policy hands out the path that was checked, and that is the one recorded.
    pub path: String,
    /// The digest of the bytes the path held, or empty when it held none (or was not read).
    pub before: String,
    /// The digest of the bytes the write left behind, or empty when the write did not land.
    pub after: String,
    /// What the capture found before the write.
    pub kind: Kind,
    /// How many bytes the path held when it was captured. Zero for an absent path.
    pub bytes: u64,
}

/// What one read of an index produced: the entries that parsed, and how many lines could not be read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scan {
    pub entries: Vec<Entry>,
    /// Lines that did not parse, including the torn tail a kill mid-append leaves behind. Counted rather
    /// than hidden, because a run whose index is losing lines is a run whose record has holes in it.
    pub skipped: usize,
}

/// Turn a run name into one directory name.
///
/// A run is a session id, and a session id is free text: it can carry a slash, a colon or a `..`. This is
/// the one place that decides what a run is called on disk, so the answer cannot drift between the writer
/// and the reader.
pub fn safe_run(run: &str) -> String {
    let mut out = String::with_capacity(run.len().min(96));
    for ch in run.chars().take(96) {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() || out == "." || out == ".." {
        return "run".to_string();
    }
    out
}

/// A path that looks like a credential, by name. The runtime already refuses to read some of these into a
/// model's context; this keeps the journal from keeping a second copy of the same file.
pub fn is_secret_name(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    lower == ".env"
        || lower.starts_with(".env.")
        || lower.ends_with(".pem")
        || lower.ends_with(".key")
        || lower == "auth.json"
        || lower.starts_with("id_rsa")
        || lower.starts_with("id_ed25519")
        || lower.starts_with("id_ecdsa")
}

/// One run's journal. A handle, not a handle to a file: it holds two paths and is cheap to make per call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Journal {
    root: PathBuf,
    run: String,
}

impl Journal {
    /// The journal for `run` inside `state`. Nothing is created until something is written.
    pub fn at(state: &Path, run: &str) -> Journal {
        let run = safe_run(run);
        Journal {
            root: state.join("journal").join(&run),
            run,
        }
    }

    pub fn run(&self) -> &str {
        &self.run
    }

    pub fn dir(&self) -> &Path {
        &self.root
    }

    pub fn index(&self) -> PathBuf {
        self.root.join(INDEX)
    }

    pub fn blob(&self, digest: &str) -> PathBuf {
        self.root.join(BLOBS).join(digest)
    }

    /// Store previous bytes once per digest. A blob that is already there is left alone: the same bytes
    /// under the same name are the same bytes.
    pub fn put_blob(&self, digest: &str, bytes: &[u8]) -> Result<(), String> {
        if digest.is_empty() {
            return Err("a blob needs a digest".to_string());
        }
        let blobs = self.root.join(BLOBS);
        crate::scratch::state_dir(&blobs)
            .map_err(|error| format!("{}: {error}", blobs.display()))?;
        let blob = self.blob(digest);
        if blob.is_file() {
            // The bytes are already there, but the name still has to be durable before an index line can
            // point at it: a capture killed between its rename and its directory sync leaves exactly this.
            sync_blobs(&blobs)?;
            return Ok(());
        }
        // A name nobody else can pick, then a rename: a reader never sees half a blob, and two captures of
        // the same bytes racing each other both end with the whole thing there.
        let temp = blobs.join(format!(
            ".{digest}.{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        let write = (|| -> std::io::Result<()> {
            let mut file = std::fs::File::create(&temp)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            Ok(())
        })();
        if let Err(error) = write {
            let _ = std::fs::remove_file(&temp);
            return Err(format!("could not store a blob: {error}"));
        }
        if let Err(error) = std::fs::rename(&temp, &blob) {
            let _ = std::fs::remove_file(&temp);
            return Err(format!("could not store a blob: {error}"));
        }
        // The file's own `sync_all` says the bytes reached the platter, not that the name did. Syncing the
        // directory after the rename is what makes the name durable, and the index line that names this
        // blob is written after this returns.
        sync_blobs(&blobs)?;
        Ok(())
    }

    /// Append one line. Never rewrites the file, so a reader that opened it earlier still sees a prefix.
    pub fn append(&self, entry: &Entry) -> Result<(), String> {
        crate::scratch::state_dir(&self.root)
            .map_err(|error| format!("{}: {error}", self.root.display()))?;
        let mut text = serde_json::to_string(entry)
            .map_err(|error| format!("could not write a journal line: {error}"))?;
        text.push('\n');
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.index())
            .map_err(|error| format!("{}: {error}", self.index().display()))?;
        file.write_all(text.as_bytes())
            .map_err(|error| format!("{}: {error}", self.index().display()))?;
        // The line is an entry only once it is on the disk, and the caller reports the capture when this
        // returns. `sync_all` rather than `sync_data`: the new length has to be durable too, or the entry
        // comes back truncated after a power loss. Cost measured on this machine's root ext4 disk: 1.87 ms
        // per synced append against 0.001 ms unsynced over 2,000 appends, so roughly 2 ms per capture,
        // which is why this is one sync per append and not a batch.
        file.sync_all()
            .map_err(|error| format!("{}: {error}", self.index().display()))?;
        #[cfg(test)]
        syncs::note_index();
        Ok(())
    }

    /// Everything the index holds, oldest first, plus the count of lines that would not parse.
    pub fn scan(&self) -> Scan {
        let Ok(bytes) = std::fs::read(self.index()) else {
            return Scan::default();
        };
        let mut scan = Scan::default();
        for line in bytes.split(|byte| *byte == b'\n') {
            if line.iter().all(|byte| byte.is_ascii_whitespace()) {
                continue;
            }
            match serde_json::from_slice::<Entry>(line) {
                Ok(entry) => scan.entries.push(entry),
                // A tail cut off by a kill, a line from a version whose shape this one does not know, or
                // bytes that are not a line at all. Counted and skipped: the history around it is still
                // the history, and a reader that refuses to answer is worse than one that says where the
                // hole is.
                Err(_) => scan.skipped += 1,
            }
        }
        scan
    }

    /// The entries that parsed.
    pub fn entries(&self) -> Vec<Entry> {
        self.scan().entries
    }
}

/// One write, captured. Both mutating file tools go through this, and so does the governed turn, so a
/// path cannot be written by any of them without its previous bytes being kept. There is no uncaptured
/// variant: a writer that has no journal has no `Capture` to hand, which is why the type is a reference
/// rather than an option.
///
/// The order is the guarantee: read the previous bytes, store the blob, then write. A blob that cannot be
/// stored stops the capture before the index line is written, so the index never names a path whose bytes
/// are not there. A journal failure does not stop the write — the same contract the activity ledger holds,
/// because a state directory that cannot be written is not a reason to refuse all work — but it is
/// counted in [`gaps`] and said on stderr, so a run whose record has holes in it says so.
pub struct Capture<'a> {
    journal: &'a Journal,
    step: u64,
    tool: &'a str,
}

impl<'a> Capture<'a> {
    pub fn new(journal: &'a Journal, step: u64, tool: &'a str) -> Capture<'a> {
        Capture {
            journal,
            step,
            tool,
        }
    }

    /// Read what is at `path`, keep it, then run `write`, then record what the write left behind.
    ///
    /// `after` is the bytes the caller is about to write. Passing them rather than re-reading the file is
    /// what makes the recorded `after` the digest of this write and not of whatever a concurrent process
    /// put there a microsecond later.
    pub fn write<E, F>(&self, path: &Path, after: &[u8], write: F) -> Result<(), E>
    where
        F: FnOnce() -> Result<(), E>,
    {
        let journal = self.journal;
        let found = match read_before(path) {
            Ok(found) => found,
            Err(said) => {
                note_gap(&said);
                return write();
            }
        };
        if found.kind == Kind::File
            && let Err(said) = journal.put_blob(&found.before, &found.bytes)
        {
            // The bytes could not be kept, so the index must not claim they were.
            note_gap(&said);
            return write();
        }
        let outcome = write();
        if outcome.is_ok()
            && let Err(said) = journal.append(&Entry {
                at: epoch(),
                run: journal.run().to_string(),
                step: self.step,
                tool: self.tool.to_string(),
                path: path.display().to_string(),
                before: found.before,
                after: mneme::sha256::hex(after),
                kind: found.kind,
                bytes: found.size,
            })
        {
            note_gap(&said);
        }
        outcome
    }
}

/// What was at the path before the write.
struct Before {
    kind: Kind,
    before: String,
    bytes: Vec<u8>,
    size: u64,
}

fn read_before(path: &Path) -> Result<Before, String> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Before {
                kind: Kind::Absent,
                before: String::new(),
                bytes: Vec::new(),
                size: 0,
            });
        }
        Err(error) => return Err(format!("could not read {}: {error}", path.display())),
    };
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    if is_secret_name(path) {
        return Ok(Before {
            kind: Kind::Secret,
            before: String::new(),
            bytes: Vec::new(),
            size: metadata.len(),
        });
    }
    if metadata.len() > MAX_BLOB {
        return Ok(Before {
            kind: Kind::TooLarge,
            before: String::new(),
            bytes: Vec::new(),
            size: metadata.len(),
        });
    }
    // Bound the read: the stat above can be a lie by the time the file is opened, and the extra byte is
    // what catches a file that grew in between.
    let file = std::fs::File::open(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let mut bytes = Vec::new();
    std::io::Read::take(file, MAX_BLOB + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    if bytes.len() as u64 > MAX_BLOB {
        return Ok(Before {
            kind: Kind::TooLarge,
            before: String::new(),
            bytes: Vec::new(),
            size: bytes.len() as u64,
        });
    }
    Ok(Before {
        kind: Kind::File,
        before: mneme::sha256::hex(&bytes),
        size: bytes.len() as u64,
        bytes,
    })
}

fn epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

/// Captures that could not be kept: a blob that would not write, an index line that would not append, a
/// path that could not be read. Counted so a surface can say the record has holes in it.
pub fn gaps() -> u64 {
    GAPS.load(std::sync::atomic::Ordering::SeqCst)
}

fn note_gap(said: &str) {
    GAPS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    eprintln!("journal: {said}");
}

static GAPS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The tools whose file writes go through [`Capture`]. Kept here, beside [`NOT_CAPTURED`], because the
/// question "which writers are covered" has to have one answer that a test can hold against the dispatch
/// table.
pub const CAPTURE: [&str; 2] = ["write_file", "patch"];

/// The tools the runtime dispatches that do not put bytes through this journal, and what covers them
/// instead. Every name in [`crate::agent::TOOLS`] must appear in exactly one of these two lists: a tool
/// added to the dispatch table without a decision here fails the suite, which is the difference between a
/// writer that is uncovered on purpose and one that escaped by being forgotten.
///
/// `shell` is the one that matters: it can truncate a file with a redirection, and it is covered by the
/// manifest diff around the step rather than by a per-write capture. Until that lands, a shell step is
/// recorded in the journal as nothing at all.
pub const NOT_CAPTURED: [&str; 23] = [
    "shell",
    "read_file",
    "list_dir",
    "search",
    "image",
    "browser",
    "fetch",
    "library",
    "recall",
    "remember",
    "session_search",
    "skills",
    "web_search",
    "react",
    "cron",
    "goal",
    "mcp",
    "delegate",
    "spawn",
    "children",
    "result",
    "tell",
    "ask",
];

/// How many `fsync` calls a capture issued, and to what.
///
/// Durability against a power loss is not observable from inside the process that asks for it: the
/// filesystem either replays the writes after the machine comes back or it does not, and no assertion in a
/// test can wait for that. Counting the syncs is the part of the guarantee this code owns, so the test
/// below holds the two calls in the order the guarantee needs.
///
/// Per thread, because cargo runs tests in parallel and every other test in this file captures too: a
/// process-wide counter would count their syncs and make this test pass or fail on scheduling.
#[cfg(test)]
pub(crate) mod syncs {
    use std::cell::Cell;

    thread_local! {
        static BLOBS_DIR: Cell<u64> = const { Cell::new(0) };
        static INDEX: Cell<u64> = const { Cell::new(0) };
    }

    pub fn reset() {
        BLOBS_DIR.with(|cell| cell.set(0));
        INDEX.with(|cell| cell.set(0));
    }

    pub fn blobs_dir() -> u64 {
        BLOBS_DIR.with(|cell| cell.get())
    }

    pub fn index() -> u64 {
        INDEX.with(|cell| cell.get())
    }

    pub fn note_blobs_dir() {
        BLOBS_DIR.with(|cell| cell.set(cell.get() + 1));
    }

    pub fn note_index() {
        INDEX.with(|cell| cell.set(cell.get() + 1));
    }
}

/// Make a directory's entries durable.
///
/// `File::open` on a directory gives a handle whose `fsync` covers the names in it. A file's own
/// `sync_all` covers its bytes; this is the other half, and the blob store needs both before the index
/// can name a blob.
fn sync_blobs(blobs: &Path) -> Result<(), String> {
    std::fs::File::open(blobs)
        .and_then(|dir| dir.sync_all())
        .map_err(|error| format!("could not sync {}: {error}", blobs.display()))?;
    #[cfg(test)]
    syncs::note_blobs_dir();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        crate::scratch::unique(name)
    }

    fn entry(run: &str, step: u64, path: &Path) -> Entry {
        Entry {
            at: epoch(),
            run: run.to_string(),
            step,
            tool: "write_file".to_string(),
            path: path.display().to_string(),
            before: String::new(),
            after: String::new(),
            kind: Kind::File,
            bytes: 0,
        }
    }

    /// A process killed between the write and the fsync leaves a line with no newline and half a body.
    /// The reader must return the lines before it and the lines after it, and never panic: one torn
    /// append must not take the rest of the run's history with it.
    #[test]
    fn a_line_written_by_a_killed_process_is_either_whole_or_absent() {
        let dir = scratch("journal-torn");
        let journal = Journal::at(&dir, "run-torn");
        for step in 0..3 {
            journal
                .append(&entry(
                    "run-torn",
                    step,
                    &dir.join(format!("note-{step}.txt")),
                ))
                .expect("a journal line");
        }
        let text = std::fs::read(journal.index()).expect("the index");
        let starts: Vec<usize> = std::iter::once(0)
            .chain(
                text.iter()
                    .enumerate()
                    .filter(|(_, byte)| **byte == b'\n')
                    .map(|(index, _)| index + 1),
            )
            .collect();
        assert_eq!(starts.len(), 4, "three lines and the end");
        // Cut the third line in half, the way a kill mid-append does.
        let half = (starts[3] - starts[2]) / 2;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(journal.index())
            .expect("the index to cut");
        file.set_len((starts[2] + half) as u64)
            .expect("a truncated tail");

        let scan = journal.scan();
        assert_eq!(
            scan.entries.len(),
            2,
            "the two whole lines come back: {:?}",
            scan.entries
        );
        assert_eq!(scan.skipped, 1, "and the torn one is counted, not hidden");
        assert_eq!(scan.entries[1].step, 1, "oldest first, in order");
    }

    /// The same cut, landing inside a multi-byte character. Reading the index as text fails at that
    /// point for the whole file, so the reader has to work on bytes and skip the line it cannot parse.
    #[test]
    fn a_tail_cut_inside_a_character_is_skipped_too() {
        let dir = scratch("journal-torn-utf8");
        let journal = Journal::at(&dir, "run-utf8");
        journal
            .append(&entry("run-utf8", 0, &dir.join("first.txt")))
            .expect("a journal line");
        journal
            .append(&entry("run-utf8", 1, &dir.join("café-ünicode.txt")))
            .expect("a journal line");
        let text = std::fs::read(journal.index()).expect("the index");
        let second = text
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|index| index + 1)
            .expect("a first line");
        // One byte into the third line is not enough for a character, let alone a line.
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(journal.index())
            .expect("the index to cut");
        file.set_len((second + 3) as u64).expect("a truncated tail");

        let scan = journal.scan();
        assert_eq!(scan.entries.len(), 1, "{:?}", scan.entries);
        assert_eq!(scan.skipped, 1);
    }

    /// Content addressing, stated as a test: two captures of the same bytes cost one blob.
    #[test]
    fn the_same_bytes_are_stored_once() {
        let dir = scratch("journal-dedupe");
        let journal = Journal::at(&dir, "run-dedupe");
        let bytes = b"the same bytes\n".repeat(10);
        let digest = mneme::sha256::hex(&bytes);
        journal.put_blob(&digest, &bytes).expect("a blob");
        journal.put_blob(&digest, &bytes).expect("a blob again");
        let blobs = std::fs::read_dir(journal.dir().join(BLOBS)).expect("the blob directory");
        let names: Vec<String> = blobs
            .flatten()
            .filter_map(|item| item.file_name().to_str().map(|name| name.to_string()))
            .filter(|name| !name.starts_with('.'))
            .collect();
        assert_eq!(names, vec![digest], "one file, named by its digest");
    }

    /// A run name is free text. It becomes one directory name, and two names that differ only in a
    /// character a path cannot hold must not land in the same directory.
    #[test]
    fn a_run_name_cannot_leave_its_directory() {
        let dir = scratch("journal-run-name");
        assert_eq!(safe_run("../../etc"), ".._.._etc");
        assert_eq!(safe_run(""), "run");
        assert_eq!(safe_run(".."), "run");
        assert_eq!(safe_run("chat:7"), "chat_7");
        let journal = Journal::at(&dir, "../../etc");
        assert!(
            journal.dir().starts_with(dir.join("journal")),
            "{}",
            journal.dir().display()
        );
    }

    /// A file named like a key is recorded, sized, and never opened.
    #[test]
    fn a_credential_is_named_and_its_bytes_are_not_read() {
        for name in [".env", "server.pem", "id_ed25519", "auth.json"] {
            assert!(is_secret_name(Path::new(name)), "{name}");
        }
        for name in ["notes.md", "environment.md", "pem.rs"] {
            assert!(!is_secret_name(Path::new(name)), "{name}");
        }
        let dir = scratch("journal-secret");
        let journal = Journal::at(&dir, "run-secret");
        let path = dir.join(".env");
        std::fs::write(&path, "TOKEN=abc123\n").expect("a file that looks like a key");
        Capture::new(&journal, 0, "write_file")
            .write(&path, b"TOKEN=replaced\n", || {
                std::fs::write(&path, "TOKEN=replaced\n")
            })
            .expect("the write");
        let entries = journal.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, Kind::Secret);
        assert_eq!(entries[0].before, "", "nothing was hashed");
        assert!(
            !journal
                .blob(&mneme::sha256::hex(b"TOKEN=abc123\n"))
                .exists(),
            "the previous bytes were never stored"
        );
    }

    /// The durability the module promises, held as the two calls that carry it: the blob's name is synced
    /// into its directory after the rename, and the index line is synced after the append. A kill between
    /// them can lose the last entry; a power loss cannot produce an entry whose bytes are not there, which
    /// is the claim the module doc makes.
    #[test]
    fn a_capture_syncs_the_blob_directory_and_the_index_before_it_returns() {
        syncs::reset();
        let dir = scratch("journal-sync");
        let journal = Journal::at(&dir, "run-sync");
        let path = dir.join("note.txt");
        std::fs::write(&path, "before\n").expect("a file the capture will read");

        Capture::new(&journal, 0, "write_file")
            .write::<std::io::Error, _>(&path, b"after\n", || std::fs::write(&path, "after\n"))
            .expect("the write");

        assert_eq!(
            syncs::blobs_dir(),
            1,
            "the blobs directory is synced once, after the rename, so the blob's name is durable before \
             the index names it"
        );
        assert_eq!(
            syncs::index(),
            1,
            "the index append is synced before the capture reports success"
        );
    }

    /// The anti drift test. The question is not "is every writer captured" — it is "is every tool the
    /// runtime dispatches a decision", because the second one fails when a new tool is added, and the
    /// first one only fails if somebody remembers to look.
    #[test]
    fn every_tool_the_runtime_dispatches_has_a_capture_decision() {
        let mut classified: Vec<&str> =
            CAPTURE.iter().chain(NOT_CAPTURED.iter()).copied().collect();
        classified.sort_unstable();
        let duplicated: Vec<&str> = classified
            .windows(2)
            .filter(|pair| pair[0] == pair[1])
            .map(|pair| pair[0])
            .collect();
        assert!(duplicated.is_empty(), "listed twice: {duplicated:?}");

        let mut dispatched: Vec<&str> = crate::agent::TOOLS.to_vec();
        dispatched.sort_unstable();
        assert_eq!(
            classified, dispatched,
            "a tool the runtime dispatches has no journal decision (add it to CAPTURE or NOT_CAPTURED), \
             or a decision names a tool that does not exist"
        );

        for name in CAPTURE {
            assert!(
                crate::agent::mutating(name),
                "{name} is captured but the mode gate does not treat it as a writer"
            );
        }
    }
}
