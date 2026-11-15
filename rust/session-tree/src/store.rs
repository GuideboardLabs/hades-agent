//! One JSONL file per session, one JSON object per line, append-only.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::SCHEMA_VERSION;
use crate::seams::SessionStore;
use crate::types::{Entry, EntryId, Fault, Kind, SessionId};

/// A directory of `<session id>.jsonl` files. Nothing else lives here.
#[derive(Debug, Clone)]
pub struct FileStore {
    root: PathBuf,
}

impl FileStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        FileStore { root: root.into() }
    }

    pub fn path_of(&self, session: &SessionId) -> PathBuf {
        self.root.join(format!("{}.jsonl", session.as_str()))
    }

    fn io(path: &Path, detail: impl std::fmt::Display) -> Fault {
        Fault::Io {
            path: path.display().to_string(),
            detail: detail.to_string(),
        }
    }

    fn corrupt(path: &Path, line: usize, detail: impl Into<String>) -> Fault {
        Fault::CorruptLog {
            path: path.display().to_string(),
            line,
            detail: detail.into(),
        }
    }

    /// Read the entries appended to a session's log since a byte offset.
    ///
    /// The daemon asks this once a second for each run it holds, and a log is cumulative: reading a
    /// three-megabyte history to look at the five entries a run wrote is work with no answer in it.
    ///
    /// `from` must be a line boundary, which the length of the file at the moment a run started always is.
    /// A line that will not decode is reported, never skipped, for the same reason [`FileStore::load`]
    /// reports it: a silently skipped line rewrites history. The duplicate-id and parent-before-child
    /// checks are not repeated here, because they need the entries before the window; the window holds
    /// what one process appended under a lease, so the chain up to `from` was read when it was taken on.
    pub fn since(&self, session: &SessionId, from: u64) -> Result<Vec<Entry>, Fault> {
        let path = self.path_of(session);
        let file = File::open(&path).map_err(|e| FileStore::io(&path, e))?;
        let mut reader = BufReader::new(file);
        reader
            .seek(SeekFrom::Start(from))
            .map_err(|e| FileStore::io(&path, e))?;
        let mut entries: Vec<Entry> = Vec::new();
        for (index, line) in reader.lines().enumerate() {
            let raw = line.map_err(|e| FileStore::io(&path, e))?;
            if raw.trim().is_empty() {
                continue;
            }
            // The line number counts from the start of the window: this read never sees the lines before
            // `from`, and a number invented for them would point at the wrong place in the file.
            entries.push(decode(&path, index + 1, &raw)?);
        }
        Ok(entries)
    }
}

/// One entry as a line of JSON. Field order is stable so a diff of two logs reads in order.
fn encode(entry: &Entry) -> Value {
    let mut map = Map::new();
    map.insert("v".into(), json!(SCHEMA_VERSION));
    map.insert("id".into(), json!(entry.id.0));
    map.insert(
        "parent".into(),
        match entry.parent {
            Some(parent) => json!(parent.0),
            None => Value::Null,
        },
    );
    map.insert("kind".into(), json!(entry.kind.as_str()));
    map.insert("payload".into(), entry.payload.clone());
    map.insert("at".into(), json!(entry.at));
    Value::Object(map)
}

/// Decode one line, failing loudly with the line number on anything unexpected. A truncated write
/// must never be skipped, because a skipped line silently rewrites history.
fn decode(path: &Path, line: usize, raw: &str) -> Result<Entry, Fault> {
    let value: Value = serde_json::from_str(raw)
        .map_err(|error| FileStore::corrupt(path, line, format!("not valid JSON: {error}")))?;
    let object = value
        .as_object()
        .ok_or_else(|| FileStore::corrupt(path, line, "entry is not a JSON object"))?;

    for key in REQUESTED_KEYS {
        if !object.contains_key(key) {
            return Err(FileStore::corrupt(path, line, format!("missing key {key}")));
        }
    }

    let version = object["v"]
        .as_u64()
        .ok_or_else(|| FileStore::corrupt(path, line, "schema version is not a number"))?;
    if version != SCHEMA_VERSION {
        return Err(FileStore::corrupt(
            path,
            line,
            format!(
                "schema version {version} is not supported, this build writes {SCHEMA_VERSION}"
            ),
        ));
    }

    let id = object["id"]
        .as_u64()
        .ok_or_else(|| FileStore::corrupt(path, line, "id is not a number"))?;

    let parent = match &object["parent"] {
        Value::Null => None,
        Value::Number(number) => match number.as_u64() {
            Some(value) => Some(EntryId(value)),
            None => {
                return Err(FileStore::corrupt(
                    path,
                    line,
                    "parent is not a whole number",
                ));
            }
        },
        _ => {
            return Err(FileStore::corrupt(
                path,
                line,
                "parent is neither null nor a number",
            ));
        }
    };

    let kind_text = object["kind"]
        .as_str()
        .ok_or_else(|| FileStore::corrupt(path, line, "kind is not a string"))?;
    let kind = Kind::parse(kind_text)
        .ok_or_else(|| FileStore::corrupt(path, line, format!("unknown kind {kind_text}")))?;

    let at = object["at"]
        .as_str()
        .ok_or_else(|| FileStore::corrupt(path, line, "at is not a string"))?
        .to_string();

    Ok(Entry {
        id: EntryId(id),
        parent,
        kind,
        payload: object["payload"].clone(),
        at,
    })
}

/// Every key an entry must carry, checked before anything is read out of it.
const REQUESTED_KEYS: [&str; 6] = ["v", "id", "parent", "kind", "payload", "at"];

/// The directory this store owns: created if absent, and 0700.
///
/// `create_dir_all` takes its mode from the caller's umask, which on a desktop is `022` or `002`. A
/// transcript is the most sensitive file the harness keeps, and a state directory that is not under a
/// 0700 home (`HADES_HOME=/tmp/x`, a library user) has nothing else protecting it. An existing directory
/// at a looser mode is tightened too: this store owns it.
fn private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = fs::symlink_metadata(path)?.permissions().mode();
        if mode & 0o077 != 0 {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(())
}

/// A new session log, at 0600 from the first instant. `File::create` takes the umask, which leaves the
/// transcript group-readable for as long as the file lives.
fn private_create(path: &Path) -> io::Result<File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path)
}

impl SessionStore for FileStore {
    fn create(&self, session: &SessionId, entries: &[Entry]) -> Result<(), Fault> {
        private_dir(&self.root).map_err(|e| FileStore::io(&self.root, e))?;
        let path = self.path_of(session);
        if path.exists() {
            // An append-only store never clobbers: the caller picks a fresh id instead.
            return Err(Fault::SessionExists {
                session: session.0.clone(),
            });
        }
        let mut file = private_create(&path).map_err(|e| FileStore::io(&path, e))?;
        for entry in entries {
            writeln!(file, "{}", encode(entry)).map_err(|e| FileStore::io(&path, e))?;
        }
        Ok(())
    }

    fn load(&self, session: &SessionId) -> Result<Vec<Entry>, Fault> {
        let path = self.path_of(session);
        let file = File::open(&path).map_err(|e| FileStore::io(&path, e))?;
        let mut entries: Vec<Entry> = Vec::new();
        let mut seen: Vec<EntryId> = Vec::new();

        for (index, line) in BufReader::new(file).lines().enumerate() {
            let number = index + 1;
            let raw = line.map_err(|e| FileStore::io(&path, e))?;
            if raw.trim().is_empty() {
                continue;
            }
            let entry = decode(&path, number, &raw)?;
            if seen.contains(&entry.id) {
                return Err(FileStore::corrupt(
                    &path,
                    number,
                    format!("duplicate id {}", entry.id),
                ));
            }
            if let Some(parent) = entry.parent
                && !seen.contains(&parent)
            {
                return Err(FileStore::corrupt(
                    &path,
                    number,
                    format!("parent {parent} does not appear before this entry"),
                ));
            }
            seen.push(entry.id);
            entries.push(entry);
        }
        Ok(entries)
    }

    fn exists(&self, session: &SessionId) -> bool {
        self.path_of(session).exists()
    }

    fn append(&self, session: &SessionId, entry: &Entry) -> Result<(), Fault> {
        let path = self.path_of(session);
        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(|e| FileStore::io(&path, e))?;
        writeln!(file, "{}", encode(entry)).map_err(|e| FileStore::io(&path, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;
        fs::symlink_metadata(path)
            .expect("a path")
            .permissions()
            .mode()
            & 0o777
    }

    fn scratch(name: &str) -> PathBuf {
        let named = std::env::var("CARGO_TARGET_DIR")
            .ok()
            .filter(|dir| !dir.trim().is_empty());
        let base = match named {
            Some(dir) => PathBuf::from(dir).join("scratch"),
            None => {
                PathBuf::from(std::env::var("HOME").expect("a home")).join(".hades/test-scratch")
            }
        };
        let dir = base.join("session-tree").join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("scratch");
        fs::canonicalize(&dir).expect("canonical scratch")
    }

    /// A transcript used to take the caller's umask: `umask 002` left `<state>/sessions` at 0775 and the
    /// log at 0664, so every session in a state directory that is not under the 0700 default home was
    /// readable by the group.
    #[cfg(unix)]
    #[test]
    fn the_session_store_is_private_even_under_a_permissive_umask() {
        let dir = scratch("private-modes");
        let store = FileStore::new(dir.join("state/sessions"));
        let session = SessionId::new("s1");

        // SAFETY: the umask is process-wide; this test sets it for the length of one create and restores
        // it before it asserts. The window only makes files another test creates tighter, never looser.
        let old = unsafe { libc::umask(0o002) };
        let created = store.create(&session, &[]);
        let found = (
            mode(&dir.join("state/sessions")),
            mode(&store.path_of(&session)),
        );
        unsafe { libc::umask(old) };

        created.expect("create");
        assert_eq!(
            found.0, 0o700,
            "the sessions directory took the caller's umask, so the transcripts are group-readable"
        );
        assert_eq!(
            found.1, 0o600,
            "a new session log took the caller's umask, so its transcript is group-readable"
        );
    }

    /// The twin: a sessions directory an earlier version left at 0775 is tightened when this one creates a
    /// log in it, because the store owns the directory.
    #[cfg(unix)]
    #[test]
    fn creating_a_log_tightens_a_sessions_directory_an_earlier_version_left_open() {
        let dir = scratch("private-modes-existing");
        let sessions = dir.join("state/sessions");
        fs::create_dir_all(&sessions).expect("a sessions directory");
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&sessions, fs::Permissions::from_mode(0o775))
                .expect("loosen the directory");
        }
        let store = FileStore::new(&sessions);
        store
            .create(&SessionId::new("s1"), &[])
            .expect("create the log");
        assert_eq!(
            mode(&sessions),
            0o700,
            "an existing sessions directory was left group-readable"
        );
    }
}
