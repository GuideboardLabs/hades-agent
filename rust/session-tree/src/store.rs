//! One JSONL file per session, one JSON object per line, append-only.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
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

impl SessionStore for FileStore {
    fn create(&self, session: &SessionId, entries: &[Entry]) -> Result<(), Fault> {
        fs::create_dir_all(&self.root).map_err(|e| FileStore::io(&self.root, e))?;
        let path = self.path_of(session);
        if path.exists() {
            // An append-only store never clobbers: the caller picks a fresh id instead.
            return Err(Fault::SessionExists {
                session: session.0.clone(),
            });
        }
        let mut file = File::create(&path).map_err(|e| FileStore::io(&path, e))?;
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
