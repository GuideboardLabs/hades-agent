//! The checkpointer: a copy of the working tree, taken before a program runs, that can be put back.
//!
//! Every program a model writes is a change to a tree a person owns, and the design asks for a shadow
//! snapshot of every tool effect. A copy is the honest baseline: it is obviously correct, it needs no
//! dependency, and it is measured here rather than assumed cheap.
//!
//! Three cheap alternatives were considered and are **not** it, each for a reason worth keeping:
//!
//! - **Hardlinking the tree instead of copying.** An in-place write to a hardlinked file mutates the
//!   snapshot, so the snapshot is silently wrong exactly when it is needed. This is the bug that makes a
//!   hardlink snapshot worse than none, because it fails at the moment it is trusted.
//! - **A delta chain keyed on `(size, mtime)`.** A missed change in a safety net is a correctness bug, not
//!   a slow path. If the measured copy cost turns out to be unacceptable, the delta chain is the next step
//!   and it comes with a content check, not only a metadata one.
//! - **`git write-tree` / `git stash create`.** It works only inside a git work tree, it writes into the
//!   user's object database, and a harness that must work on a plain directory cannot depend on it.
//!
//! Three limits are stated rather than hidden. `.git` is always ignored, so a program that rewrites the
//! repo's own history is not covered: the shadows cover the working tree and not the object database.
//! The store is never allowed inside the tree it copies, because a `--state` under the root would
//! otherwise make the copy recurse into its own output. And a tree past a measured bound is **declined**,
//! not copied: measured on this machine, the harness's own dev tree is 659 MB over 32,451 files and takes
//! 5.4 s to copy, which is a cost per turn nobody would keep. A declined snapshot is recorded as plainly
//! as a taken one, because a run that cannot be undone is a fact about the run.

use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

use serde_json::{Map, Value, json};

use crate::fault::Fault;

/// Which snapshot of which session. A session's snapshots are numbered from one, in the order taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotId {
    pub session: String,
    pub n: u64,
}

impl SnapshotId {
    pub fn new(session: impl Into<String>, n: u64) -> Self {
        SnapshotId {
            session: session.into(),
            n,
        }
    }

    /// How a snapshot is named in a log or on a command line: `cli#3`.
    pub fn label(&self) -> String {
        format!("{}#{}", self.session, self.n)
    }
}

/// The tree a snapshot covers, and the paths it must never read or written while covering it.
#[derive(Debug, Clone)]
pub struct Subject {
    session: String,
    root: PathBuf,
    ignore: Vec<String>,
    never_touch: Vec<PathBuf>,
}

impl Subject {
    /// The tree to snapshot, and the session the snapshot belongs to.
    pub fn new(session: impl Into<String>, root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        // Canonical where possible, because the never-touch check is a path-prefix test and a relative root
        // would silently never match an absolute store.
        let root = fs::canonicalize(&root).unwrap_or(root);
        Subject {
            session: session.into(),
            root,
            ignore: vec![
                ".git".to_string(),
                "target".to_string(),
                "node_modules".to_string(),
            ],
            never_touch: Vec::new(),
        }
    }

    /// Names no part of a path may have. The default already carries the three a repository does not read.
    pub fn ignoring(mut self, names: impl IntoIterator<Item = String>) -> Self {
        self.ignore.extend(names);
        self
    }

    /// A subtree the snapshot must not read or write at all. The store puts itself here.
    pub fn never_touching(mut self, path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        self.never_touch
            .push(fs::canonicalize(&path).unwrap_or(path));
        self
    }

    pub fn session(&self) -> &str {
        &self.session
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Whether this path is outside the subject: a name in the ignore list, or under a never-touch path.
    /// One implementation, because two copies of a rule about what may be touched drift apart silently.
    fn skips(&self, path: &Path) -> bool {
        if self.never_touch.iter().any(|kept| path.starts_with(kept)) {
            return true;
        }
        // Named parts of the path *below the root*, not of the root itself. A snapshot of a tree that happens
        // to live under a directory called `target` is a snapshot of that tree; judging the whole absolute
        // path would find the ancestor and copy nothing, which is the quietest way to have no safety net.
        let judged = path.strip_prefix(&self.root).unwrap_or(path);
        let parts: Vec<Component<'_>> = judged.components().collect();
        // A directory is never a secret, whatever it is called: `src/id_gen/` is source, and pruning it
        // would drop the subtree from every snapshot. Only the entry itself can be a credential file.
        let is_dir = path.is_dir();
        parts.iter().enumerate().any(|(index, part)| match part {
            Component::Normal(name) => {
                let name = name.to_string_lossy();
                // The ignore list names directories as well as files, so it judges every component. The
                // secret rule names *files*: a directory called `id_gen` or `keys.env` is source, and
                // dropping it from the snapshot would leave a subtree `hades undo` silently cannot restore.
                self.ignore.iter().any(|ignored| name == ignored.as_str())
                    || (!is_dir && index + 1 == parts.len() && secret_name(&name))
            }
            _ => false,
        })
    }
}

/// A name a snapshot must never copy. Credentials live in these files, and a shadow is written to the
/// state directory, which outlives the run that made it. Exact names, a prefix and suffixes, not globs:
/// the crate has no glob dependency and a rule an auditor can read is better than a pattern.
fn secret_name(name: &str) -> bool {
    matches!(
        name,
        ".env" | ".netrc" | ".npmrc" | ".git-credentials" | ".htpasswd" | "credentials"
    ) || name.starts_with("id_")
        || name.ends_with(".pem")
        || name.ends_with(".key")
        || name.ends_with(".env")
        // Certificate bundles and infrastructure state hold the same kind of secret as a `.pem`, and the
        // shadow is written where it outlives the run.
        || name.ends_with(".p12")
        || name.ends_with(".pfx")
        || name.ends_with(".tfstate")
}

/// What a restore did. A restore that did nothing says so with numbers rather than with `ok`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restored {
    /// Files written back from the snapshot.
    pub put_back: usize,
    /// Files removed, because the snapshot does not have them and a program made them.
    pub removed: usize,
    /// Paths left exactly as they are, because they are outside the subject.
    pub left_alone: usize,
    /// Bytes written back.
    pub bytes: u64,
}

impl Restored {
    pub fn line(&self) -> String {
        format!(
            "put back {}, removed {}, left alone {}, {} bytes",
            self.put_back, self.removed, self.left_alone, self.bytes
        )
    }
}

/// What a checkpointer did when it was asked for a snapshot.
///
/// A snapshot that was not taken is a fact about the run, so it is a value and not an error: the program
/// still runs, and the log says the run cannot be rolled back. Refusing the whole turn because a safety net
/// did not fit would trade a real capability for a missing one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Snapshot {
    Taken(SnapshotId),
    Declined(String),
}

impl Snapshot {
    /// The id, when there is one.
    pub fn id(&self) -> Option<&SnapshotId> {
        match self {
            Snapshot::Taken(id) => Some(id),
            Snapshot::Declined(_) => None,
        }
    }

    /// How it reads in a log or on a command line. A decline carries its reason with it, so nobody has to
    /// read a second line to find out why the undo they are counting on does not exist.
    pub fn label(&self) -> String {
        match self {
            Snapshot::Taken(id) => id.label(),
            Snapshot::Declined(why) => format!("declined ({why})"),
        }
    }
}

/// The capability. Two implementations, which is what earns a trait.
pub trait Checkpointer {
    /// Copy the subject's tree into the store, or say why it will not, and return which it did.
    fn snapshot(&self, subject: &Subject) -> Result<Snapshot, Fault>;
    /// Put the subject's tree back the way the snapshot found it, and say what that took.
    fn restore(&self, subject: &Subject, id: &SnapshotId) -> Result<Restored, Fault>;
    /// What this may do, said out loud, because a run that does not say whether it can be undone cannot be
    /// judged afterwards.
    fn describe(&self) -> String;
}

/// A checkpointer that copies nothing. A run without a store says so rather than implying it can be undone.
#[derive(Debug, Default, Clone, Copy)]
pub struct Nothing;

impl Checkpointer for Nothing {
    fn snapshot(&self, _subject: &Subject) -> Result<Snapshot, Fault> {
        Ok(Snapshot::Declined(
            "nothing is snapshotted, so nothing can be rolled back".to_string(),
        ))
    }

    fn restore(&self, _subject: &Subject, id: &SnapshotId) -> Result<Restored, Fault> {
        Err(Fault::harness(format!(
            "snapshot {} was never taken: nothing is snapshotted",
            id.label()
        )))
    }

    fn describe(&self) -> String {
        "nothing is snapshotted, so nothing can be rolled back".to_string()
    }
}

/// The default bound on what a snapshot may copy, and it is not generous on purpose. Measured on this
/// machine: the harness's own dev tree is 659 MB over 32,451 files, and copying it costs 5.4 s before the
/// program has run a line. A safety net that costs that much per turn gets switched off, and an absent
/// safety net that nobody announced is worse than one that is declared. Past the bound the snapshot is
/// declined and the decline is recorded.
const DEFAULT_BYTES: u64 = 64 * 1024 * 1024;
const DEFAULT_FILES: u64 = 10_000;

/// The real one: a full copy of the subject under a store, plus a manifest and a cost line per snapshot.
#[derive(Debug, Clone)]
pub struct Shadows {
    store: PathBuf,
    limit_bytes: u64,
    limit_files: u64,
}

impl Shadows {
    pub fn new(store: impl Into<PathBuf>) -> Self {
        Shadows {
            store: store.into(),
            limit_bytes: DEFAULT_BYTES,
            limit_files: DEFAULT_FILES,
        }
    }

    /// Raise or lower the bound, for a caller that knows what its trees cost.
    pub fn up_to(mut self, bytes: u64, files: u64) -> Self {
        self.limit_bytes = bytes;
        self.limit_files = files;
        self
    }

    pub fn store(&self) -> &Path {
        &self.store
    }

    fn dir_of(&self, id: &SnapshotId) -> PathBuf {
        self.store.join(&id.session)
    }

    fn copy_of(&self, id: &SnapshotId) -> PathBuf {
        self.dir_of(id).join(id.n.to_string())
    }

    fn manifest_of(&self, id: &SnapshotId) -> PathBuf {
        self.dir_of(id).join(format!("{}.json", id.n))
    }

    fn cost_log(&self, session: &str) -> PathBuf {
        self.store.join(session).join("cost.jsonl")
    }

    /// The next number for a session, read off the store rather than kept in memory, so a second process
    /// cannot mint an id the first one already used.
    fn next_n(&self, session: &str) -> u64 {
        let mut highest = 0u64;
        if let Ok(listing) = fs::read_dir(self.store.join(session)) {
            for entry in listing.flatten() {
                if let Some(stem) = entry.path().file_stem().and_then(|stem| stem.to_str())
                    && let Ok(n) = stem.parse::<u64>()
                {
                    highest = highest.max(n);
                }
            }
        }
        highest + 1
    }

    /// The latest snapshot a session has, or `None` when it has none. `--to latest` is this.
    pub fn latest(&self, session: &str) -> Option<SnapshotId> {
        let n = self.next_n(session);
        (n > 1).then(|| SnapshotId::new(session, n - 1))
    }

    /// Every snapshot a session has, oldest first.
    pub fn taken(&self, session: &str) -> Vec<SnapshotId> {
        (1..self.next_n(session))
            .map(|n| SnapshotId::new(session, n))
            .collect()
    }

    /// The manifest of a snapshot, read back with the same discipline the session store uses: a malformed
    /// line fails loudly rather than being skipped.
    pub fn manifest(&self, id: &SnapshotId) -> Result<Manifest, Fault> {
        let path = self.manifest_of(id);
        let text = fs::read_to_string(&path).map_err(|error| {
            Fault::harness(format!("no snapshot at {}: {error}", path.display()))
        })?;
        let value: Value = serde_json::from_str(&text).map_err(|error| {
            Fault::harness(format!(
                "{}: the manifest will not read: {error}",
                path.display()
            ))
        })?;
        Manifest::from_value(&value, &path)
    }
}

/// One file in a snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotFile {
    pub path: String,
    pub bytes: u64,
    pub mode: u32,
}

/// What a snapshot holds, and what it cost to take.
#[derive(Debug, Clone)]
pub struct Manifest {
    pub session: String,
    pub n: u64,
    pub files: Vec<SnapshotFile>,
    pub bytes: u64,
    pub millis: u64,
    /// Paths that were skipped: ignored names, never-touch subtrees, secrets, and anything that is not a
    /// regular file. A count alone says something was left out; the names say what, which is what a person
    /// auditing a restore needs. Older manifests parse with an empty list.
    pub skipped: u64,
    pub skipped_names: Vec<String>,
}

/// The manifest, written and read by hand for the same reason the session store does it by hand: the crate
/// keeps no dependency it does not need, and a key that is missing has to fail with the file that is wrong
/// rather than with a serde error about a field.
impl Manifest {
    fn to_value(&self) -> Value {
        let mut out = Map::new();
        out.insert("session".into(), json!(self.session));
        out.insert("n".into(), json!(self.n));
        out.insert("bytes".into(), json!(self.bytes));
        out.insert("millis".into(), json!(self.millis));
        out.insert("skipped".into(), json!(self.skipped));
        out.insert("skipped_names".into(), json!(self.skipped_names));
        out.insert(
            "files".into(),
            Value::Array(
                self.files
                    .iter()
                    .map(
                        |file| json!({ "path": file.path, "bytes": file.bytes, "mode": file.mode }),
                    )
                    .collect(),
            ),
        );
        Value::Object(out)
    }

    fn from_value(value: &Value, at: &Path) -> Result<Manifest, Fault> {
        let bad = |what: &str| Fault::harness(format!("{}: the manifest {what}", at.display()));
        let object = value
            .as_object()
            .ok_or_else(|| bad("is not a JSON object"))?;
        for key in ["session", "n", "bytes", "millis", "skipped", "files"] {
            if !object.contains_key(key) {
                return Err(bad(&format!("has no `{key}`")));
            }
        }
        let files = object["files"]
            .as_array()
            .ok_or_else(|| bad("does not list its files as an array"))?
            .iter()
            .map(|file| {
                let path = file["path"]
                    .as_str()
                    .ok_or_else(|| bad("has a file with no path"))?;
                Ok(SnapshotFile {
                    path: path.to_string(),
                    bytes: file["bytes"].as_u64().unwrap_or(0),
                    mode: file["mode"].as_u64().unwrap_or(0) as u32,
                })
            })
            .collect::<Result<Vec<_>, Fault>>()?;
        Ok(Manifest {
            session: object["session"].as_str().unwrap_or_default().to_string(),
            n: object["n"].as_u64().unwrap_or(0),
            files,
            bytes: object["bytes"].as_u64().unwrap_or(0),
            millis: object["millis"].as_u64().unwrap_or(0),
            skipped: object["skipped"].as_u64().unwrap_or(0),
            skipped_names: object["skipped_names"]
                .as_array()
                .map(|names| {
                    names
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
        })
    }
}

impl Checkpointer for Shadows {
    fn snapshot(&self, subject: &Subject) -> Result<Snapshot, Fault> {
        // The store may not be inside the tree it is about to copy, or the copy recurses into its own output.
        let subject = subject.clone().never_touching(self.store.clone());

        // What it would cost, before paying it. A tree past the bound is declined with its size in the
        // reason, so the run says out loud that it cannot be undone and says what to do about it.
        let (files, bytes) = measure(&subject)?;
        if bytes > self.limit_bytes || files > self.limit_files {
            return Ok(Snapshot::Declined(format!(
                "{} over {files} files is past the {} over {} a snapshot will copy; \
                 point --root at the subtree you are working in, or raise the bound with Shadows::up_to",
                human(bytes),
                human(self.limit_bytes),
                self.limit_files
            )));
        }

        let id = SnapshotId::new(subject.session(), self.next_n(subject.session()));
        let copy = self.copy_of(&id);
        if copy.exists() {
            return Err(Fault::harness(format!(
                "snapshot {} already exists, refusing to overwrite it",
                id.label()
            )));
        }

        let started = Instant::now();
        let mut files: Vec<SnapshotFile> = Vec::new();
        let mut bytes = 0u64;
        let mut skipped = 0u64;
        let mut skipped_names = Vec::new();
        copy_tree(
            &subject,
            subject.root(),
            &copy,
            &mut files,
            &mut bytes,
            &mut skipped,
            &mut skipped_names,
        )?;

        let millis = started.elapsed().as_millis() as u64;
        let manifest = Manifest {
            session: id.session.clone(),
            n: id.n,
            files,
            bytes,
            millis,
            skipped,
            skipped_names,
        };
        write_json(&self.manifest_of(&id), &manifest.to_value())?;
        // The cost is a fact in the log, not an estimate in a comment: a copy per attempt is only
        // defensible while somebody can see what it costs.
        let line = json!({
            "n": id.n,
            "files": manifest.files.len(),
            "bytes": manifest.bytes,
            "millis": manifest.millis,
            "skipped": manifest.skipped,
        });
        append_line(&self.cost_log(subject.session()), &line)?;
        Ok(Snapshot::Taken(id))
    }

    fn restore(&self, subject: &Subject, id: &SnapshotId) -> Result<Restored, Fault> {
        let subject = subject.clone().never_touching(self.store.clone());
        let manifest = self.manifest(id)?;
        let copy = self.copy_of(id);
        let mut restored = Restored {
            put_back: 0,
            removed: 0,
            left_alone: 0,
            bytes: 0,
        };

        // 1. Every file the snapshot holds goes back, written to a neighbour and renamed, so a reader never
        //    sees half a file.
        let mut known: HashSet<&str> = HashSet::new();
        for file in &manifest.files {
            known.insert(file.path.as_str());
            let target = subject.root().join(&file.path);
            if subject.skips(&target) {
                restored.left_alone += 1;
                continue;
            }
            let source = copy.join(&file.path);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|error| {
                    Fault::harness(format!("could not make {}: {error}", parent.display()))
                })?;
            }
            let staging = staging_path(&target);
            fs::copy(&source, &staging).map_err(|error| {
                Fault::harness(format!(
                    "could not put {} back from {}: {error}",
                    target.display(),
                    source.display()
                ))
            })?;
            fs::rename(&staging, &target).map_err(|error| {
                Fault::harness(format!("could not finish {}: {error}", target.display()))
            })?;
            restored.put_back += 1;
            restored.bytes += file.bytes;
        }

        // 2. Everything now under the root that the snapshot does not hold was made after it, so it goes.
        //    Ignored and never-touch paths are not read and not written, which is the same rule the snapshot
        //    obeyed on the way in.
        for path in present_files(subject.root())? {
            if subject.skips(&path) {
                restored.left_alone += 1;
                continue;
            }
            let Ok(relative) = path.strip_prefix(subject.root()) else {
                continue;
            };
            let relative = relative.to_string_lossy().replace('\\', "/");
            if !known.contains(relative.as_str()) {
                fs::remove_file(&path).map_err(|error| {
                    Fault::harness(format!("could not remove {}: {error}", path.display()))
                })?;
                restored.removed += 1;
            }
        }

        Ok(restored)
    }

    fn describe(&self) -> String {
        format!(
            "a shadow copy under {}, up to {}",
            self.store.display(),
            human(self.limit_bytes)
        )
    }
}

/// A size a person reads, because the number in a decline reason is the whole point of the message.
fn human(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    if bytes >= MB {
        format!("{} MB", bytes / MB)
    } else if bytes >= KB {
        format!("{} KB", bytes / KB)
    } else {
        format!("{bytes} bytes")
    }
}

/// What a tree would cost to copy: the regular files, and the bytes in them. One stat per path and no
/// reads, so the answer costs almost nothing next to the copy it is deciding about.
fn measure(subject: &Subject) -> Result<(u64, u64), Fault> {
    fn into(subject: &Subject, from: &Path, files: &mut u64, bytes: &mut u64) -> Result<(), Fault> {
        // A directory that cannot be read is skipped and counted by the copy; here it simply contributes
        // nothing, and the bound stays an estimate in the safe direction only if it never under-counts.
        let Ok(listing) = fs::read_dir(from) else {
            return Ok(());
        };
        for entry in listing.flatten() {
            let path = entry.path();
            if subject.skips(&path) {
                continue;
            }
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.is_dir() {
                into(subject, &path, files, bytes)?;
            } else if metadata.is_file() {
                *files += 1;
                *bytes += metadata.len();
            }
        }
        Ok(())
    }

    let mut files = 0u64;
    let mut bytes = 0u64;
    into(subject, subject.root(), &mut files, &mut bytes)?;
    Ok((files, bytes))
}

/// Where a restore stages a file before renaming it into place. A sibling with a leading dot, so it is
/// never mistaken for the file itself, and so a rename is within one directory.
fn staging_path(target: &Path) -> PathBuf {
    match target.file_name() {
        Some(name) => target.with_file_name(format!(".{}.hades-restore", name.to_string_lossy())),
        None => target.with_file_name(".hades-restore"),
    }
}

/// Copy the tree, recording what went where. Directories are recreated; regular files are copied; anything
/// else (a symlink, a fifo, a socket) is skipped and counted, because a snapshot cannot promise to put back
/// something it does not understand.
fn copy_tree(
    subject: &Subject,
    from: &Path,
    to: &Path,
    files: &mut Vec<SnapshotFile>,
    bytes: &mut u64,
    skipped: &mut u64,
    skipped_names: &mut Vec<String>,
) -> Result<(), Fault> {
    let listing = match fs::read_dir(from) {
        Ok(listing) => listing,
        // A directory that cannot be read is not a reason to fail the whole snapshot, and it is the same
        // answer the repo map learned: skip it and say so in the count.
        Err(_) => {
            *skipped += 1;
            skipped_names.push(from.display().to_string());
            return Ok(());
        }
    };

    let mut entries: Vec<PathBuf> = listing.flatten().map(|entry| entry.path()).collect();
    entries.sort();

    for path in entries {
        if subject.skips(&path) {
            *skipped += 1;
            skipped_names.push(
                path.strip_prefix(subject.root())
                    .unwrap_or(&path)
                    .display()
                    .to_string(),
            );
            continue;
        }
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(_) => {
                *skipped += 1;
                continue;
            }
        };
        let relative = match path.strip_prefix(subject.root()) {
            Ok(relative) => relative.to_path_buf(),
            Err(_) => {
                *skipped += 1;
                continue;
            }
        };
        let target = to.join(&relative);

        if metadata.is_dir() {
            crate::scratch::state_dir(&target).map_err(|error| {
                Fault::harness(format!("could not make {}: {error}", target.display()))
            })?;
            copy_tree(subject, &path, to, files, bytes, skipped, skipped_names)?;
        } else if metadata.is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|error| {
                    Fault::harness(format!("could not make {}: {error}", parent.display()))
                })?;
            }
            fs::copy(&path, &target).map_err(|error| {
                Fault::harness(format!(
                    "could not copy {} to {}: {error}",
                    path.display(),
                    target.display()
                ))
            })?;
            #[cfg(unix)]
            let mode = {
                use std::os::unix::fs::PermissionsExt;
                metadata.permissions().mode()
            };
            #[cfg(not(unix))]
            let mode = 0;
            files.push(SnapshotFile {
                path: relative.to_string_lossy().replace('\\', "/"),
                bytes: metadata.len(),
                mode,
            });
            *bytes += metadata.len();
        } else {
            *skipped += 1;
        }
    }
    Ok(())
}

/// Every regular file under a root, in sorted order.
fn present_files(root: &Path) -> Result<Vec<PathBuf>, Fault> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let Ok(listing) = fs::read_dir(&directory) else {
            continue;
        };
        let mut entries: Vec<PathBuf> = listing.flatten().map(|entry| entry.path()).collect();
        entries.sort();
        for path in entries {
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.is_dir() {
                stack.push(path);
            } else if metadata.is_file() {
                found.push(path);
            }
        }
    }
    found.sort();
    Ok(found)
}

fn write_json(path: &Path, value: &Value) -> Result<(), Fault> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            Fault::harness(format!("could not make {}: {error}", parent.display()))
        })?;
    }
    let text = serde_json::to_string_pretty(value)
        .map_err(|error| Fault::harness(format!("could not write the manifest: {error}")))?;
    let staging = staging_path(path);
    fs::write(&staging, text.as_bytes()).map_err(|error| {
        Fault::harness(format!("could not write {}: {error}", staging.display()))
    })?;
    fs::rename(&staging, path)
        .map_err(|error| Fault::harness(format!("could not finish {}: {error}", path.display())))?;
    crate::scratch::private_file(path)
        .map_err(|error| Fault::harness(format!("could not protect {}: {error}", path.display())))
}

fn append_line(path: &Path, value: &Value) -> Result<(), Fault> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            Fault::harness(format!("could not make {}: {error}", parent.display()))
        })?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| Fault::harness(format!("could not open {}: {error}", path.display())))?;
    crate::scratch::private_file(path).map_err(|error| {
        Fault::harness(format!("could not protect {}: {error}", path.display()))
    })?;
    writeln!(file, "{value}")
        .map_err(|error| Fault::harness(format!("could not write {}: {error}", path.display())))
}

/// How a snapshot looks in a listing: one line, with what it cost.
pub fn summarise(manifest: &Manifest) -> String {
    format!(
        "{}#{}  {} files, {} bytes, {} ms, {} skipped",
        manifest.session,
        manifest.n,
        manifest.files.len(),
        manifest.bytes,
        manifest.millis,
        manifest.skipped
    )
}
