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

/// The longest run name the journal uses as a directory name verbatim. A run name is a session id, so it
/// is free text and can exceed this; past it the name is bounded by a digest instead of cut, because a cut
/// makes `<run>` and `<run>-fork-2` the same directory.
pub const MAX_RUN: usize = 96;

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
///
/// A name that fits in [`MAX_RUN`] is used as it maps. A longer one keeps a readable prefix and is told
/// apart by a 96-bit digest of the *whole* mapped name: cutting it at the limit made the parent run
/// `<run>` and its branch candidate `<run>-fork-2` the same 96 characters, so the parent and both
/// candidates shared one directory. The digest is of the mapped name rather than the raw one, so the
/// separator rule above decides the answer for every name, long or short. The one way two runs can still
/// share a directory is a session deliberately named exactly the derived name of another; that needs the
/// digest of a name it does not have, and the alternative was refusing a legal session id.
pub fn safe_run(run: &str) -> String {
    let mut out = String::with_capacity(run.len().min(MAX_RUN));
    for ch in run.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() || out == "." || out == ".." {
        return "run".to_string();
    }
    if out.len() <= MAX_RUN {
        return out;
    }
    const DIGEST: usize = 24;
    let digest = mneme::sha256::hex(out.as_bytes());
    let mut bounded: String = out.chars().take(MAX_RUN - 1 - DIGEST).collect();
    bounded.push('-');
    bounded.push_str(&digest[..DIGEST]);
    bounded
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

    /// The journal for another run in the same state directory as this one.
    ///
    /// A branch candidate is a session of its own, so its writes belong in its own run directory. Filing
    /// them under the parent made a losing candidate's entry outlive the rollback that put its file back:
    /// the parent's rewind then refused a tree nobody had edited, and the session that actually wrote had
    /// no journal to show. The run name is resolved in one place, [`safe_run`], so a sibling cannot land
    /// outside the state directory this journal lives in.
    pub fn sibling(&self, run: &str) -> Journal {
        let run = safe_run(run);
        // `<state>/journal/<run>` → `<state>/journal`, which is where a sibling's directory belongs.
        let holder = self.root.parent().unwrap_or(&self.root);
        Journal {
            root: holder.join(&run),
            run,
        }
    }

    pub fn dir(&self) -> &Path {
        &self.root
    }

    /// Copy another run's captures into this journal, as this run's entries.
    ///
    /// A branch candidate journals under its own session because a losing candidate's entry must not
    /// outlive the rollback that put its file back. A winner's writes are kept by the parent run, though,
    /// so the run's own `rewind <run>` has to cover them: this copies the winner's entries, and the bytes
    /// they name, into the parent journal with the run field rewritten to the directory they now live in.
    /// Blobs are content addressed, so a path the parent already captured costs no second copy.
    ///
    /// The copy is a record of the whole run or of no run. Every blob an entry names is staged first, and
    /// only then are the lines published, so a failure on the last entry cannot leave the first ones in
    /// the parent index: a parent holding half a run made `hades rewind <run>` report a complete undo of
    /// half a run while the uncopied path kept the winner's bytes. A record that is absent is a clean "no
    /// journal for run"; a record that is a silent prefix is a recovery tool lying about what it undid.
    ///
    /// Returns how many entries were adopted. Empty input is a no-op, and so is a journal adopting itself:
    /// appending a journal's own lines back to it would duplicate every entry.
    pub fn absorb(&self, other: &Journal) -> Result<usize, String> {
        if other.dir() == self.dir() {
            return Ok(0);
        }
        let entries = other.entries();
        if entries.is_empty() {
            return Ok(0);
        }
        // Stage, then publish. A staged blob is invisible to a reader until an index line names it, so a
        // blob that is staged into a record the publish never makes costs a file and no correctness.
        let mut staged = Vec::with_capacity(entries.len());
        for entry in &entries {
            if !entry.before.is_empty() {
                let kept = other.blob(&entry.before);
                let bytes = std::fs::read(&kept).map_err(|error| {
                    format!(
                        "could not read the bytes {} names at {}: {error}",
                        entry.path,
                        kept.display()
                    )
                })?;
                self.put_blob(&entry.before, &bytes)?;
            }
            staged.push(Entry {
                run: self.run.clone(),
                ..entry.clone()
            });
        }
        self.publish(&staged)?;
        Ok(staged.len())
    }

    /// Publish every staged line to the index at once.
    ///
    /// A capture appends one synced line at a time, and that is fine because one capture is one entry.
    /// Adopting another run's record is a batch, and a sequence of appends cannot be all-or-nothing: the
    /// file is therefore written whole beside the old one, synced, and renamed into place. A reader sees
    /// the old record or the new one, never a prefix of the new one, and a write that fails leaves the old
    /// record exactly as it was.
    fn publish(&self, entries: &[Entry]) -> Result<(), String> {
        let holder = self.root.parent();
        let fresh_holder = holder.is_some_and(|holder| !holder.is_dir());
        let fresh_run = !self.root.is_dir();
        let fresh_index = !self.index().is_file();
        let fresh_state = holder
            .and_then(|holder| holder.parent())
            .is_some_and(|state| !state.is_dir());
        crate::scratch::state_dir(&self.root)
            .map_err(|error| format!("{}: {error}", self.root.display()))?;
        // The lines already filed stay: this publishes a longer record, not a different one. A torn tail
        // an earlier kill left behind is carried through byte for byte, so the reader still counts it.
        let mut bytes = match std::fs::read(self.index()) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(format!("{}: {error}", self.index().display())),
        };
        for entry in entries {
            serde_json::to_writer(&mut bytes, entry)
                .map_err(|error| format!("could not write a journal line: {error}"))?;
            bytes.push(b'\n');
        }
        // The same rule a blob is stored under: a name nobody else can pick, then a rename.
        let temp = self.root.join(format!(
            ".{INDEX}.{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        let write = (|| -> std::io::Result<()> {
            let mut file = std::fs::File::create(&temp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            Ok(())
        })();
        if let Err(error) = write {
            let _ = std::fs::remove_file(&temp);
            return Err(format!(
                "could not write {}: {error}",
                self.index().display()
            ));
        }
        if let Err(error) = std::fs::rename(&temp, self.index()) {
            let _ = std::fs::remove_file(&temp);
            return Err(format!(
                "could not publish {}: {error}",
                self.index().display()
            ));
        }
        #[cfg(test)]
        syncs::note_index();
        // The rename replaced a name inside the run directory, so that directory has to reach the disk
        // before the record is reported. A run this call created has its directory synced by
        // `sync_created`; a run that already existed still needs the sync here, because its index name
        // was replaced rather than appended to.
        if fresh_holder || fresh_run || fresh_state {
            self.sync_created(fresh_holder, fresh_run, fresh_index, fresh_state)?;
        } else {
            std::fs::File::open(&self.root)
                .and_then(|dir| dir.sync_all())
                .map_err(|error| format!("could not sync {}: {error}", self.root.display()))?;
            #[cfg(test)]
            syncs::note_run_dir();
        }
        Ok(())
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
        // Which names this call is about to create. A capture into a run that already exists owes no
        // directory sync, because the names were made durable by the capture that created them.
        let holder = self.root.parent();
        let fresh_holder = holder.is_some_and(|holder| !holder.is_dir());
        let fresh_run = !self.root.is_dir();
        let fresh_blobs = !blobs.is_dir();
        // `<state>` is a name in its own parent, and `--state` can name a path that did not exist: the
        // first capture then creates `<state>` itself, and the sync of `<state>` covers its contents but
        // not the name it lives under.
        let fresh_state = holder
            .and_then(|holder| holder.parent())
            .is_some_and(|state| !state.is_dir());
        crate::scratch::state_dir(&blobs)
            .map_err(|error| format!("{}: {error}", blobs.display()))?;
        if fresh_holder || fresh_run || fresh_blobs || fresh_state {
            self.sync_created(fresh_holder, fresh_run, fresh_blobs, fresh_state)?;
        }
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
        let holder = self.root.parent();
        let fresh_holder = holder.is_some_and(|holder| !holder.is_dir());
        let fresh_run = !self.root.is_dir();
        let fresh_index = !self.index().is_file();
        let fresh_state = holder
            .and_then(|holder| holder.parent())
            .is_some_and(|state| !state.is_dir());
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
        // The line's bytes are on the disk; its name and the names above it are separate directory
        // entries. The first capture of a run creates `<state>/journal/<run>`, `<run>/blobs` and
        // `<run>/index.jsonl`; until those names are durable, the order this module promises can be
        // undone by a power loss. Ordered after the file's own sync, so the entry never becomes durable
        // before the line it names.
        if fresh_run || fresh_index || fresh_state {
            self.sync_created(fresh_holder, fresh_run, fresh_index, fresh_state)?;
        }
        #[cfg(test)]
        syncs::note_index();
        Ok(())
    }

    /// Make the names a capture created durable.
    ///
    /// `sync_blobs` makes a blob's name durable inside `blobs/`, and a file's `sync_all` covers only its
    /// own bytes. The directories above them are separate names: `<state>/journal/<run>` lives inside
    /// `<state>/journal`, `blobs/` and `index.jsonl` live inside the run directory, `<state>/journal`
    /// itself lives inside `<state>`, and `<state>` lives inside its own parent. A capture that creates
    /// them is the one that owes the sync, which is why the caller says which were new: a run that already
    /// exists pays nothing here. The first capture of a run in a state directory that has never held a
    /// journal paid nothing for `<state>`, so the run's own name was reachable only through a directory the
    /// filesystem had never been told about; and when `--state` names a path that did not exist, that same
    /// capture creates `<state>` as a name in *its* parent, which needs the same sync one level up.
    fn sync_created(
        &self,
        fresh_holder: bool,
        fresh_run: bool,
        fresh_inside: bool,
        fresh_state: bool,
    ) -> Result<(), String> {
        if fresh_state
            && let Some(above) = self
                .root
                .parent()
                .and_then(|holder| holder.parent())
                .and_then(|state| state.parent())
        {
            std::fs::File::open(above)
                .and_then(|dir| dir.sync_all())
                .map_err(|error| format!("could not sync {}: {error}", above.display()))?;
            #[cfg(test)]
            syncs::note_state_parent_dir();
        }
        if fresh_holder && let Some(state) = self.root.parent().and_then(|holder| holder.parent()) {
            std::fs::File::open(state)
                .and_then(|dir| dir.sync_all())
                .map_err(|error| format!("could not sync {}: {error}", state.display()))?;
            #[cfg(test)]
            syncs::note_state_dir();
        }
        if fresh_run && let Some(holder) = self.root.parent() {
            std::fs::File::open(holder)
                .and_then(|dir| dir.sync_all())
                .map_err(|error| format!("could not sync {}: {error}", holder.display()))?;
            #[cfg(test)]
            syncs::note_holder_dir();
        }
        if fresh_run || fresh_inside {
            std::fs::File::open(&self.root)
                .and_then(|dir| dir.sync_all())
                .map_err(|error| format!("could not sync {}: {error}", self.root.display()))?;
            #[cfg(test)]
            syncs::note_run_dir();
        }
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

/// How much of the run journal to keep.
///
/// Every field is optional and every field is a cap: a caller that names none keeps everything, which is
/// what a state directory did before this existed. The fields compose, and the stricter of them wins, so
/// `--keep-days 30 --keep-runs 5` keeps the five newest runs that are also under thirty days old.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Keep {
    /// Keep at most this many runs, newest first.
    pub runs: Option<usize>,
    /// Keep runs whose newest entry is younger than this many days.
    pub days: Option<i64>,
    /// Keep the journal under this many bytes, newest runs first. A cap smaller than the newest run alone
    /// cannot be met, and the report says so instead of deleting the run the person is in.
    pub bytes: Option<u64>,
}

/// What one prune did, or would do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pruned {
    /// Runs removed, newest first, with the bytes each held.
    pub removed: Vec<(String, u64)>,
    /// Runs left in place, newest first.
    pub kept: Vec<(String, u64)>,
    /// Bytes the removed runs held.
    pub freed: u64,
    /// Set when the byte cap could not be met without removing the newest run.
    pub blocked: Option<String>,
}

/// One run as the pruner sees it: its directory name, when it last wrote, and how much it holds.
struct Held {
    name: String,
    at: i64,
    bytes: u64,
    dir: PathBuf,
}

/// Reclaim journal space by removing runs.
///
/// Whole runs, never a file at a time: a blob lives inside its own run's `blobs/` directory, so a removed
/// run cannot orphan an entry in a run that stays, and nothing here has to reason about sharing. The newest
/// run is never removed even when it alone is over the byte cap, because pruning the run someone is working
/// in is the one outcome a retention tool must not produce; the report names that instead.
///
/// `now` is passed rather than read so a test can age a journal without waiting. `dry` computes the same
/// answer and writes nothing, which is what makes the command safe to run first.
pub fn prune(state: &Path, keep: &Keep, now: i64, dry: bool) -> Result<Pruned, String> {
    let holder = state.join("journal");
    let mut held: Vec<Held> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&holder) {
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            let Some(name) = dir.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let journal = Journal {
                root: dir.clone(),
                run: name.to_string(),
            };
            // A run whose index will not parse still holds bytes somebody may want, so its age comes from
            // the newest entry it does have and, failing that, from the directory itself rather than zero.
            let scan = journal.scan();
            let newest = scan.entries.iter().map(|entry| entry.at).max();
            let at = newest.unwrap_or_else(|| modified(&dir).unwrap_or(0));
            held.push(Held {
                name: name.to_string(),
                at,
                bytes: held_bytes(&dir),
                dir,
            });
        }
    }
    // Newest first, and the name breaks a tie so two runs captured in the same second have one order.
    held.sort_by(|a, b| b.at.cmp(&a.at).then_with(|| b.name.cmp(&a.name)));

    let mut report = Pruned::default();
    let mut removed: Vec<usize> = Vec::new();
    for (index, run) in held.iter().enumerate() {
        if index == 0 {
            continue; // the newest run is never a candidate
        }
        let too_many = keep.runs.is_some_and(|runs| index >= runs);
        let too_old = keep.days.is_some_and(|days| run.at < now - days * 86_400);
        if too_many || too_old {
            removed.push(index);
        }
    }

    // Then the byte cap, oldest first, still never touching the newest run.
    if let Some(cap) = keep.bytes {
        let mut total: u64 = held
            .iter()
            .enumerate()
            .filter(|(index, _)| !removed.contains(index))
            .map(|(_, run)| run.bytes)
            .sum();
        for index in (1..held.len()).rev() {
            if total <= cap {
                break;
            }
            if removed.contains(&index) {
                continue;
            }
            total -= held[index].bytes;
            removed.push(index);
        }
        if total > cap {
            report.blocked = Some(format!(
                "the newest run alone holds {} byte(s), over the {} byte cap: it was kept and the cap was not met",
                held[0].bytes, cap
            ));
        }
    }

    for index in &removed {
        let run = &held[*index];
        report.freed += run.bytes;
        report.removed.push((run.name.clone(), run.bytes));
        if !dry {
            std::fs::remove_dir_all(&run.dir)
                .map_err(|error| format!("{}: {error}", run.dir.display()))?;
        }
    }
    for (index, run) in held.iter().enumerate() {
        if !removed.contains(&index) {
            report.kept.push((run.name.clone(), run.bytes));
        }
    }
    Ok(report)
}

/// Everything one run's directory holds, in bytes.
fn held_bytes(dir: &Path) -> u64 {
    let mut total = 0;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.metadata() {
            Ok(meta) if meta.is_dir() => total += held_bytes(&path),
            Ok(meta) => total += meta.len(),
            Err(_) => {}
        }
    }
    total
}

/// When a path last changed, in unix seconds. Used only as an age of last resort.
fn modified(path: &Path) -> Option<i64> {
    let changed = std::fs::metadata(path).ok()?.modified().ok()?;
    let since = changed.duration_since(std::time::UNIX_EPOCH).ok()?;
    i64::try_from(since.as_secs()).ok()
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

impl Capture<'_> {
    /// Cover a step whose writes cannot be predicted: scan, run, scan again, record what moved.
    ///
    /// The alternative was to guess which paths a command would touch, which cannot be done for a
    /// redirection, an editor, or a build. What this keeps is the bytes each changed path held *before* the
    /// command ran, so a rewind restores them exactly as it restores a `write_file`. What it cannot keep,
    /// past [`Caps`], is counted in [`gaps`] rather than left to look covered.
    pub fn shell<T, E, F>(&self, root: &Path, caps: &Caps, run: F) -> Result<T, E>
    where
        F: FnOnce() -> Result<T, E>,
    {
        let journal = self.journal;
        let before = scan_tree(root, caps, Some(journal));
        let outcome = run();
        let after = scan_tree(root, caps, None);
        let at = epoch();
        let note =
            |path: &str, kind: Kind, before: String, after: String, bytes: u64, step: u64| {
                let entry = Entry {
                    at,
                    run: journal.run().to_string(),
                    step,
                    tool: self.tool.to_string(),
                    path: path.to_string(),
                    before,
                    after,
                    kind,
                    bytes,
                };
                if let Err(said) = journal.append(&entry) {
                    note_gap(&said);
                }
            };
        // A path that was there and is now different or gone: recoverable, with its old bytes kept.
        for (path, (digest, bytes)) in &before.files {
            let now = after
                .files
                .get(path)
                .map(|(digest, _)| digest.clone())
                .unwrap_or_default();
            if &now == digest {
                continue;
            }
            note(path, Kind::File, digest.clone(), now, *bytes, self.step);
        }
        // A path the command created. The journal says it was absent, so a rewind removes it.
        for (path, (digest, _)) in &after.files {
            if before.files.contains_key(path) {
                continue;
            }
            note(
                path,
                Kind::Absent,
                String::new(),
                digest.clone(),
                0,
                self.step,
            );
        }
        let uncovered = before.uncovered() + after.uncovered();
        if uncovered > 0 {
            note_gap(&format!(
                "a shell step at step {} left {uncovered} path(s) outside the journal's caps (over {} bytes \
                 each, {} bytes total, or {} files); those paths cannot be rewound",
                self.step, caps.max_file, caps.max_total, caps.max_files
            ));
        }
        outcome
    }
}

/// What a tree scan will keep, and what it will refuse to keep.
///
/// A shell step has no path to capture, so the only honest way to cover it is to look at the tree on both
/// sides of the step and keep the bytes that are about to change. That means reading files a command might
/// never touch, so the scan is bounded and the bounds are visible: past them a path is counted in
/// [`gaps`], never quietly left out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    /// Largest single file kept. Past this the path is named in the gap count and not stored.
    pub max_file: u64,
    /// Ceiling on all bytes kept for one shell step.
    pub max_total: u64,
    /// Ceiling on how many files one scan will look at.
    pub max_files: usize,
}

impl Default for Caps {
    fn default() -> Self {
        Caps {
            max_file: 8 * 1024 * 1024,
            max_total: 64 * 1024 * 1024,
            max_files: 4_000,
        }
    }
}

/// Directory names a tree scan never enters. The first three are the ones that make a scan useless if
/// included; state is skipped because the journal must never capture its own blobs.
const SKIP_DIRS: [&str; 7] = [
    ".git",
    "target",
    "node_modules",
    ".venv",
    "__pycache__",
    ".hades",
    "dist",
];

/// One scan of a tree: what it kept, and what it refused to keep and why.
#[derive(Debug, Default, Clone)]
pub struct TreeScan {
    /// Path as written, to (digest, bytes).
    pub files: std::collections::BTreeMap<String, (String, u64)>,
    pub too_large: usize,
    pub secret: usize,
    pub over_budget: usize,
    pub over_count: usize,
}

impl TreeScan {
    /// Every path this scan could not cover, which a rewind of a shell step has to report as a limit.
    pub fn uncovered(&self) -> usize {
        self.too_large + self.secret + self.over_budget + self.over_count
    }
}

/// Walk `root` and record what is there, keeping the bytes when a journal is given.
///
/// Keeping the bytes is the difference between "we know this file changed" and "we can put it back", and
/// it is why the pre-scan is handed the journal and the post-scan is not.
pub fn scan_tree(root: &Path, caps: &Caps, keep: Option<&Journal>) -> TreeScan {
    let mut scan = TreeScan::default();
    let mut spent: u64 = 0;
    walk(root, caps, keep, &mut scan, &mut spent);
    scan
}

fn walk(dir: &Path, caps: &Caps, keep: Option<&Journal>, scan: &mut TreeScan, spent: &mut u64) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if metadata.is_dir() {
            if SKIP_DIRS.contains(&name.as_ref()) {
                continue;
            }
            walk(&path, caps, keep, scan, spent);
            continue;
        }
        if !metadata.is_file() {
            continue;
        }
        if scan.files.len() >= caps.max_files {
            scan.over_count += 1;
            continue;
        }
        if is_secret_name(&path) {
            scan.secret += 1;
            continue;
        }
        if metadata.len() > caps.max_file {
            scan.too_large += 1;
            continue;
        }
        if *spent + metadata.len() > caps.max_total {
            scan.over_budget += 1;
            continue;
        }
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };
        let digest = mneme::sha256::hex(&bytes);
        if let Some(journal) = keep
            && journal.put_blob(&digest, &bytes).is_err()
        {
            // The bytes could not be kept, so the index must not claim they were.
            scan.over_budget += 1;
            continue;
        }
        *spent += metadata.len();
        // Absolute, because that is what a `write_file` entry records and what a rewind resolves.
        scan.files
            .insert(path.display().to_string(), (digest, metadata.len()));
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

/// The clock a prune decides with, in unix seconds.
///
/// Exposed so the caller can pass the same instant the entries were written against, and so a test can age a
/// journal by naming a time rather than waiting for one.
pub fn now() -> i64 {
    epoch()
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
pub const CAPTURE: [&str; 3] = ["write_file", "patch", "shell"];

/// The tools the runtime dispatches that do not put bytes through this journal, and what covers them
/// instead. Every name in [`crate::agent::TOOLS`] must appear in exactly one of these two lists: a tool
/// added to the dispatch table without a decision here fails the suite, which is the difference between a
/// writer that is uncovered on purpose and one that escaped by being forgotten.
///
/// `shell` is captured differently from the other two, which is why it is named in this comment rather
/// than left to be guessed at: a command can write through a redirection, an editor, or a build, so there
/// is no "the path this tool writes". [`Capture::shell`] scans the tree before the step and after it, keeps
/// the bytes that are about to change, and records one entry per path that actually moved. The caps on that
/// scan are in [`Caps`], and a file past them is counted as a gap rather than left silently uncovered.
pub const NOT_CAPTURED: [&str; 22] = [
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
        /// `<state>/journal`: the directory the run's own name lives in.
        static HOLDER_DIR: Cell<u64> = const { Cell::new(0) };
        /// `<state>/journal/<run>`: the directory `blobs/` and `index.jsonl` live in.
        static RUN_DIR: Cell<u64> = const { Cell::new(0) };
        /// `<state>`: the directory `<state>/journal` lives in.
        static STATE_DIR: Cell<u64> = const { Cell::new(0) };
        /// The directory `<state>`'s own name lives in.
        static STATE_PARENT: Cell<u64> = const { Cell::new(0) };
    }

    pub fn reset() {
        BLOBS_DIR.with(|cell| cell.set(0));
        INDEX.with(|cell| cell.set(0));
        HOLDER_DIR.with(|cell| cell.set(0));
        RUN_DIR.with(|cell| cell.set(0));
        STATE_DIR.with(|cell| cell.set(0));
        STATE_PARENT.with(|cell| cell.set(0));
    }

    pub fn blobs_dir() -> u64 {
        BLOBS_DIR.with(|cell| cell.get())
    }

    pub fn index() -> u64 {
        INDEX.with(|cell| cell.get())
    }

    pub fn holder_dir() -> u64 {
        HOLDER_DIR.with(|cell| cell.get())
    }

    pub fn run_dir() -> u64 {
        RUN_DIR.with(|cell| cell.get())
    }

    pub fn state_dir() -> u64 {
        STATE_DIR.with(|cell| cell.get())
    }

    pub fn state_parent() -> u64 {
        STATE_PARENT.with(|cell| cell.get())
    }

    pub fn note_blobs_dir() {
        BLOBS_DIR.with(|cell| cell.set(cell.get() + 1));
    }

    pub fn note_state_parent_dir() {
        STATE_PARENT.with(|cell| cell.set(cell.get() + 1));
    }

    pub fn note_index() {
        INDEX.with(|cell| cell.set(cell.get() + 1));
    }

    pub fn note_state_dir() {
        STATE_DIR.with(|cell| cell.set(cell.get() + 1));
    }

    pub fn note_holder_dir() {
        HOLDER_DIR.with(|cell| cell.set(cell.get() + 1));
    }

    pub fn note_run_dir() {
        RUN_DIR.with(|cell| cell.set(cell.get() + 1));
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

    /// One run under `state`, holding `bytes` of blob and one index line stamped `at`.
    fn run_with(state: &Path, run: &str, at: i64, bytes: usize) -> Journal {
        let journal = Journal::at(state, run);
        let bytes = vec![b'x'; bytes];
        let digest = mneme::sha256::hex(&bytes);
        journal.put_blob(&digest, &bytes).expect("a blob");
        journal
            .append(&Entry {
                at,
                run: journal.run().to_string(),
                step: 1,
                tool: "write_file".to_string(),
                path: format!("/tmp/{run}.txt"),
                before: digest.clone(),
                after: String::new(),
                kind: Kind::File,
                bytes: bytes.len() as u64,
            })
            .expect("an index line");
        journal
    }

    #[test]
    fn prune_keeps_the_newest_runs_and_removes_whole_ones() {
        let dir = scratch("journal-prune-runs");
        let state = dir.join("state");
        for (index, run) in ["oldest", "middle", "newest"].iter().enumerate() {
            run_with(&state, run, 1_000 + index as i64, 40);
        }
        let report = prune(
            &state,
            &Keep {
                runs: Some(2),
                ..Keep::default()
            },
            2_000,
            false,
        )
        .expect("a prune");

        assert_eq!(report.removed.len(), 1);
        assert_eq!(report.removed[0].0, "oldest", "the oldest run goes first");
        assert!(report.freed >= 40);
        assert_eq!(report.kept.len(), 2);
        assert!(
            !Journal::at(&state, "oldest").index().is_file(),
            "the run is gone"
        );
        assert!(
            Journal::at(&state, "newest").index().is_file(),
            "the newest stays"
        );
        // Removing a run removes its blobs with it: a blob lives inside the run it belongs to.
        assert!(!Journal::at(&state, "oldest").dir().exists());
    }

    #[test]
    fn prune_by_age_uses_the_newest_entry_rather_than_the_directory() {
        let dir = scratch("journal-prune-days");
        let state = dir.join("state");
        let day = 86_400;
        // Forty-five days in: one run last wrote on day one, the other on day forty, and the cap is thirty.
        run_with(&state, "ancient", day, 10);
        run_with(&state, "recent", 40 * day, 10);

        let report = prune(
            &state,
            &Keep {
                days: Some(30),
                ..Keep::default()
            },
            45 * day,
            false,
        )
        .expect("a prune");
        assert_eq!(report.removed.len(), 1);
        assert_eq!(report.removed[0].0, "ancient");
        assert!(Journal::at(&state, "recent").index().is_file());
    }

    #[test]
    fn the_newest_run_is_never_removed_and_a_cap_it_breaks_is_reported() {
        let dir = scratch("journal-prune-blocked");
        let state = dir.join("state");
        run_with(&state, "huge", 900, 4_000);
        run_with(&state, "small", 800, 10);

        // A cap smaller than the newest run alone cannot be met. The honest answer is to keep the run and
        // say so, not to delete the run the person is working in to satisfy a number.
        let report = prune(
            &state,
            &Keep {
                bytes: Some(100),
                ..Keep::default()
            },
            1_000,
            false,
        )
        .expect("a prune");
        assert_eq!(report.removed.len(), 1, "the older run still goes");
        assert_eq!(report.removed[0].0, "small");
        assert!(
            Journal::at(&state, "huge").index().is_file(),
            "the newest survives its own size"
        );
        let blocked = report.blocked.expect("the unmet cap is said out loud");
        assert!(blocked.contains("newest run alone"), "{blocked}");
    }

    #[test]
    fn a_dry_run_reports_the_same_plan_and_writes_nothing() {
        let dir = scratch("journal-prune-dry");
        let state = dir.join("state");
        for (index, run) in ["a", "b", "c"].iter().enumerate() {
            run_with(&state, run, 100 + index as i64, 20);
        }
        let keep = Keep {
            runs: Some(1),
            ..Keep::default()
        };
        let planned = prune(&state, &keep, 1_000, true).expect("a plan");
        assert_eq!(planned.removed.len(), 2);
        assert!(
            Journal::at(&state, "a").index().is_file(),
            "nothing was deleted"
        );

        let done = prune(&state, &keep, 1_000, false).expect("the prune");
        assert_eq!(
            planned
                .removed
                .iter()
                .map(|(name, _)| name.clone())
                .collect::<Vec<_>>(),
            done.removed
                .iter()
                .map(|(name, _)| name.clone())
                .collect::<Vec<_>>(),
            "the plan and the act agree"
        );
        assert!(!Journal::at(&state, "a").index().is_file());
    }

    #[test]
    fn no_cap_keeps_everything() {
        let dir = scratch("journal-prune-none");
        let state = dir.join("state");
        run_with(&state, "one", 100, 5);
        run_with(&state, "two", 200, 5);
        let report = prune(&state, &Keep::default(), 1_000, false).expect("a prune");
        assert!(report.removed.is_empty());
        assert_eq!(report.kept.len(), 2);
        assert_eq!(report.freed, 0);
    }

    #[test]
    fn a_shell_step_keeps_the_bytes_it_is_about_to_overwrite() {
        let dir = scratch("journal-shell-truncate");
        let root = dir.join("root");
        std::fs::create_dir_all(&root).expect("a root");
        let path = root.join("notes.txt");
        std::fs::write(&path, "the state before the command\n").expect("a file to lose");
        let journal = Journal::at(&dir.join("state"), "run-shell");
        let capture = Capture::new(&journal, 3, "shell");

        // The command, as the capture sees it: it only has to be a closure that changes the tree.
        capture
            .shell::<(), String, _>(&root, &Caps::default(), || {
                std::fs::write(&path, "").map_err(|error| error.to_string())
            })
            .expect("the command ran");

        let entries = journal.entries();
        let found = entries
            .iter()
            .find(|entry| entry.path == path.display().to_string())
            .unwrap_or_else(|| panic!("no entry for {}, {entries:#?}", path.display()));
        assert_eq!(found.kind, Kind::File);
        assert_eq!(
            found.before,
            mneme::sha256::hex(b"the state before the command\n")
        );
        assert_eq!(found.after, mneme::sha256::hex(b""));
        assert_eq!(
            found.tool, "shell",
            "the journal names the tool that did it"
        );
        let kept = std::fs::read(journal.blob(&found.before)).expect("the previous bytes");
        assert_eq!(kept, b"the state before the command\n");
    }

    #[test]
    fn a_file_a_shell_step_created_is_recorded_as_absent_before_so_a_rewind_removes_it() {
        let dir = scratch("journal-shell-create");
        let root = dir.join("root");
        std::fs::create_dir_all(&root).expect("a root");
        let made = root.join("built.bin");
        let journal = Journal::at(&dir.join("state"), "run-shell-create");
        let capture = Capture::new(&journal, 1, "shell");

        capture
            .shell::<(), String, _>(&root, &Caps::default(), || {
                std::fs::write(&made, b"compiled").map_err(|error| error.to_string())
            })
            .expect("the command ran");

        let found = journal
            .entries()
            .into_iter()
            .find(|entry| entry.path == made.display().to_string())
            .expect("an entry for the file the command created");
        assert_eq!(
            found.kind,
            Kind::Absent,
            "nothing was there before the step"
        );
        assert_eq!(found.before, "");
        assert_eq!(found.after, mneme::sha256::hex(b"compiled"));

        // And a rewind carries that out: the journal said absent, so the path goes away.
        let plan = crate::rewind::plan(&journal, &crate::rewind::Options::default());
        crate::rewind::apply(&journal, &plan, false).expect("the undo");
        assert!(!made.exists(), "the run created it, so the undo removes it");
    }

    #[test]
    fn the_journal_puts_back_what_a_shell_step_truncated() {
        let dir = scratch("journal-shell-rewind");
        let root = dir.join("root");
        std::fs::create_dir_all(&root).expect("a root");
        let path = root.join("data.csv");
        std::fs::write(&path, "a,b,c\n1,2,3\n").expect("a file to truncate");
        let journal = Journal::at(&dir.join("state"), "run-shell-rewind");
        let capture = Capture::new(&journal, 2, "shell");
        capture
            .shell::<(), String, _>(&root, &Caps::default(), || {
                std::fs::write(&path, "").map_err(|error| error.to_string())
            })
            .expect("the command ran");
        assert_eq!(std::fs::read_to_string(&path).expect("the damage"), "");

        let plan = crate::rewind::plan(&journal, &crate::rewind::Options::default());
        crate::rewind::apply(&journal, &plan, false).expect("the undo");
        assert_eq!(
            std::fs::read_to_string(&path).expect("the file again"),
            "a,b,c\n1,2,3\n"
        );
    }

    #[test]
    fn a_file_past_the_cap_is_named_as_uncovered_instead_of_being_called_covered() {
        let dir = scratch("journal-shell-caps");
        let root = dir.join("root");
        std::fs::create_dir_all(&root).expect("a root");
        std::fs::write(root.join("small.txt"), "tiny").expect("a small file");
        std::fs::write(root.join("big.bin"), vec![7u8; 4096]).expect("a big file");
        let caps = Caps {
            max_file: 1_024,
            ..Caps::default()
        };
        let scan = scan_tree(&root, &caps, None);
        assert!(scan.files.keys().any(|path| path.ends_with("small.txt")));
        assert!(!scan.files.keys().any(|path| path.ends_with("big.bin")));
        assert_eq!(scan.too_large, 1, "the cap is counted, not hidden");
        assert_eq!(scan.uncovered(), 1);
    }

    #[test]
    fn a_run_name_directory_does_not_capture_its_own_blobs() {
        let dir = scratch("journal-shell-self");
        let root = dir.join("root");
        std::fs::create_dir_all(root.join(".hades/journal")).expect("a state tree under the root");
        std::fs::write(root.join(".hades/journal/index.jsonl"), "{}").expect("state");
        std::fs::write(root.join(".git/HEAD"), "ref").ok();
        let scan = scan_tree(&root, &Caps::default(), None);
        assert!(
            scan.files.is_empty(),
            "a scan that walks its own blobs keeps nothing: {:#?}",
            scan.files.keys().collect::<Vec<_>>()
        );
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

    /// A run name is free text and can be longer than a directory name the journal uses verbatim. Cutting
    /// it at the limit made `<run>` and `<run>-fork-2` the same 96 characters, so the parent and both branch
    /// candidates resolved to one directory: the parent's rewind then refused on a path nobody had edited.
    /// The name is bounded by a hash of the whole thing instead, and the reader resolves a name through the
    /// same function, so `hades journal <run>` still finds it by the name a person has in hand.
    #[test]
    fn a_run_name_too_long_for_a_directory_is_bounded_by_its_hash() {
        let dir = scratch("journal-long-run");
        let run: String = "a".repeat(100);
        let fork = format!("{run}-fork-2");
        assert_ne!(
            safe_run(&run),
            safe_run(&fork),
            "a fork of a long session must not land in the parent's own directory"
        );
        // Two names that agree for the whole verbatim limit and differ after it are still two runs.
        let left = format!("{}x", "b".repeat(96));
        let right = format!("{}y", "b".repeat(96));
        assert_ne!(
            safe_run(&left),
            safe_run(&right),
            "the tail past the limit is what tells two long runs apart"
        );
        let name = safe_run(&run);
        assert!(
            name.len() <= MAX_RUN,
            "the directory name is bounded: {} bytes",
            name.len()
        );
        assert!(
            !name.contains('/') && name != "." && name != "..",
            "one filesystem-safe component: {name}"
        );
        assert_eq!(
            name,
            safe_run(&run),
            "the same run name resolves to the same directory for the reader"
        );

        // The reader resolves by name, so an entry written under a long run name is readable under it.
        let journal = Journal::at(&dir, &run);
        assert_ne!(
            journal.dir(),
            Journal::at(&dir, &fork).dir(),
            "the parent and its candidate cannot share one journal"
        );
        journal
            .append(&entry(&name, 0, &dir.join("note.txt")))
            .expect("a journal line");
        assert_eq!(
            Journal::at(&dir, &run).entries().len(),
            1,
            "the name in a person's hand finds the run that wrote"
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

    /// The first capture in a brand-new run directory creates three names that must survive a power loss
    /// before the index line depends on them: the run directory inside `<state>/journal`, and then `blobs/`
    /// and `index.jsonl` inside the run directory. Syncing the blob file, the blob directory and the index
    /// file says nothing about those names, so the order the module claims (blob bytes, blob name, index
    /// line) did not hold for the first capture of a run. A run that already exists pays nothing for this.
    #[test]
    fn the_first_capture_in_a_new_run_syncs_the_directories_it_created() {
        syncs::reset();
        let dir = scratch("journal-sync-dirs");
        let journal = Journal::at(&dir, "run-sync-dirs");
        let path = dir.join("note.txt");
        std::fs::write(&path, "before\n").expect("a file the capture will read");

        Capture::new(&journal, 0, "write_file")
            .write::<std::io::Error, _>(&path, b"after\n", || std::fs::write(&path, "after\n"))
            .expect("the write");

        assert!(dir.join("journal").is_dir(), "the run's directory exists");
        assert_eq!(
            syncs::holder_dir(),
            1,
            "`<state>/journal` is synced after the run directory is created, or the run's own name is \
             not durable"
        );
        assert!(
            syncs::run_dir() >= 1,
            "the run directory is synced after `blobs/` and `index.jsonl` are created, or their names are \
             not durable"
        );
        let (holder, run) = (syncs::holder_dir(), syncs::run_dir());

        // A second capture into a run that exists makes no new names, so it owes no new directory sync:
        // the cost of this fix lands once per run, not once per capture.
        std::fs::write(&path, "again\n").expect("a file");
        Capture::new(&journal, 1, "write_file")
            .write::<std::io::Error, _>(&path, b"later\n", || std::fs::write(&path, "later\n"))
            .expect("the second write");
        assert_eq!(
            syncs::holder_dir(),
            holder,
            "a second capture re-syncs the state directory"
        );
        assert_eq!(
            syncs::run_dir(),
            run,
            "a second capture re-syncs the run directory"
        );
    }

    /// `<state>/journal` is a name too, and the run's directory is reachable only through it. The first
    /// capture creates it, and nothing synced `<state>`, so on a power cut the whole tree could vanish even
    /// though the run directory, the blobs and the index inside it were all synced. `<state>` is a name in
    /// *its* parent, and `--state` may name a path that did not exist at all: then the first
    /// capture creates `<state>` too, and syncing `<state>` makes its contents durable without making
    /// `<state>`'s own name durable. One sync per fresh level, and a run in a state directory that
    /// already exists pays nothing.
    #[test]
    fn the_first_capture_in_a_new_state_syncs_the_state_directory_and_its_name() {
        syncs::reset();
        let base = scratch("journal-sync-state");
        // `<state>` itself does not exist, and neither does its parent: `--state` named a path where
        // nothing was, which is the case where `<state>`'s own name has a directory above it to pay for.
        let dir = base.join("nested").join("state");
        let journal = Journal::at(&dir, "run-sync-state");
        let path = base.join("note.txt");
        std::fs::write(&path, "before\n").expect("a file the capture will read");

        Capture::new(&journal, 0, "write_file")
            .write::<std::io::Error, _>(&path, b"after\n", || std::fs::write(&path, "after\n"))
            .expect("the write");

        assert_eq!(
            syncs::state_dir(),
            1,
            "`<state>` is synced once, after `<state>/journal` is created, or the journal's own name is \
             not durable"
        );
        assert_eq!(
            syncs::state_parent(),
            1,
            "`<state>`'s own name is synced into its parent, or the state directory a first capture \
             created is reachable only through a name the filesystem was never told about"
        );
        let (synced, named) = (syncs::state_dir(), syncs::state_parent());
        std::fs::write(&path, "again\n").expect("a file");
        Capture::new(&journal, 1, "write_file")
            .write::<std::io::Error, _>(&path, b"later\n", || std::fs::write(&path, "later\n"))
            .expect("the second write");
        assert_eq!(
            syncs::state_dir(),
            synced,
            "a state directory that already holds the journal is not re-synced by every capture"
        );
        assert_eq!(
            syncs::state_parent(),
            named,
            "a state directory whose name already exists pays nothing on a later capture"
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
