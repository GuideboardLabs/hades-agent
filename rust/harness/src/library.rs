//! The offline library: a local shelf of papers and books that can be read without a network.
//!
//! A fresh install has no entries. There is no seed set and no placeholder: an entry exists only after a
//! person runs `hades library add`. The original bytes are kept beside a canonical text view; search,
//! research and the agent read the canonical text, never the PDF.
//!
//! The store is deliberately separate from Mneme. Mneme is the note graph; this is a document shelf with
//! its own database, its own delete path and its own boundary. A report may cite a library entry, but the
//! library is not imported into the vault by existing.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::agent::strip_html;
use crate::research::{Fetched, SearchHit, Web};

pub const DB_NAME: &str = "library.db";
pub const MAX_INPUT_BYTES: u64 = 128 * 1024 * 1024;
pub const MAX_TEXT_CHARS: usize = 8_000_000;
pub const MAX_EPUB_MEMBERS: usize = 512;
pub const CHUNK_CHARS: usize = 1_400;
pub const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS entries (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    authors TEXT NOT NULL,
    year INTEGER,
    format TEXT NOT NULL,
    added_at_ms INTEGER NOT NULL,
    bytes INTEGER NOT NULL,
    pages INTEGER,
    source_sha256 TEXT NOT NULL,
    text_sha256 TEXT NOT NULL,
    original TEXT NOT NULL,
    canonical TEXT NOT NULL,
    tags TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS chunks (
    id INTEGER PRIMARY KEY,
    entry_id TEXT NOT NULL REFERENCES entries(id) ON DELETE CASCADE,
    ord INTEGER NOT NULL,
    page INTEGER,
    start INTEGER NOT NULL,
    end INTEGER NOT NULL,
    text TEXT NOT NULL,
    UNIQUE(entry_id, ord)
);
CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
    text, entry_id UNINDEXED, tokenize='unicode61 remove_diacritics 2'
);
CREATE TRIGGER IF NOT EXISTS chunks_ai AFTER INSERT ON chunks BEGIN
    INSERT INTO chunks_fts(rowid, text, entry_id) VALUES (new.id, new.text, new.entry_id);
END;
CREATE TRIGGER IF NOT EXISTS chunks_ad AFTER DELETE ON chunks BEGIN
    DELETE FROM chunks_fts WHERE rowid = old.id;
END;
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    Pdf,
    Epub,
    Latex,
    Markdown,
    Text,
    Html,
}

impl Format {
    pub fn label(self) -> &'static str {
        match self {
            Format::Pdf => "pdf",
            Format::Epub => "epub",
            Format::Latex => "latex",
            Format::Markdown => "markdown",
            Format::Text => "text",
            Format::Html => "html",
        }
    }

    fn ext(self) -> &'static str {
        match self {
            Format::Pdf => "pdf",
            Format::Epub => "epub",
            Format::Latex => "tex",
            Format::Markdown => "md",
            Format::Text => "txt",
            Format::Html => "html",
        }
    }

    fn parse(value: &str) -> Option<Format> {
        match value {
            "pdf" => Some(Format::Pdf),
            "epub" => Some(Format::Epub),
            "latex" | "tex" => Some(Format::Latex),
            "markdown" | "md" => Some(Format::Markdown),
            "text" | "txt" => Some(Format::Text),
            "html" | "htm" => Some(Format::Html),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub title: String,
    pub authors: Vec<String>,
    pub year: Option<i32>,
    pub format: Format,
    pub added_at_ms: u64,
    pub bytes: u64,
    pub pages: Option<usize>,
    pub source_sha256: String,
    pub text_sha256: String,
    pub original: String,
    pub canonical: String,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct AddOptions {
    pub title: Option<String>,
    pub authors: Vec<String>,
    pub year: Option<i32>,
    pub tags: Vec<String>,
    pub ocr_lang: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddOutcome {
    pub entry: Entry,
    pub duplicate: bool,
    pub input: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImportReport {
    pub added: Vec<AddOutcome>,
    pub skipped: Vec<Skipped>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skipped {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    pub ord: usize,
    pub page: Option<usize>,
    pub start: usize,
    pub end: usize,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibHit {
    pub entry: Entry,
    pub ord: usize,
    pub page: Option<usize>,
    pub start: usize,
    pub end: usize,
    pub snippet: String,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyItem {
    pub id: String,
    pub ok: bool,
    pub problems: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyReport {
    pub ok: bool,
    pub entries: Vec<VerifyItem>,
}

#[derive(Debug, Clone)]
pub struct Library {
    root: PathBuf,
}

impl Library {
    /// Open or create a library. Creating the directory is not creating an entry: an empty shelf remains
    /// empty, and `list` says so.
    pub fn open(root: impl AsRef<Path>) -> Result<Library, String> {
        let root = root.as_ref();
        crate::scratch::state_dir(root).map_err(|error| format!("{}: {error}", root.display()))?;
        for child in ["files", "text", "trash"] {
            let path = root.join(child);
            fs::create_dir_all(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        }
        let root =
            fs::canonicalize(root).map_err(|error| format!("{}: {error}", root.display()))?;
        let library = Library { root };
        let conn = library.connect()?;
        let found: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|error| format!("library schema: {error}"))?;
        if found > SCHEMA_VERSION {
            return Err(format!(
                "library schema version {found} is newer than this build's {SCHEMA_VERSION}; refusing to guess"
            ));
        }
        if found == 0 {
            conn.execute_batch(SCHEMA)
                .map_err(|error| format!("library schema: {error}"))?;
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)
                .map_err(|error| format!("library schema: {error}"))?;
        }
        Ok(library)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn connect(&self) -> Result<Connection, String> {
        let db = self.root.join(DB_NAME);
        let conn = Connection::open(&db).map_err(|error| format!("{}: {error}", db.display()))?;
        conn.busy_timeout(std::time::Duration::from_millis(5_000))
            .map_err(|error| format!("library: {error}"))?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")
            .map_err(|error| format!("library: {error}"))?;
        let _mode: String = conn
            .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
            .map_err(|error| format!("library: {error}"))?;
        Ok(conn)
    }

    pub fn is_empty(&self) -> Result<bool, String> {
        let conn = self.connect()?;
        let count: i64 = conn
            .query_row("SELECT count(*) FROM entries", [], |row| row.get(0))
            .map_err(|error| format!("library: {error}"))?;
        Ok(count == 0)
    }

    pub fn list(&self) -> Result<Vec<Entry>, String> {
        let conn = self.connect()?;
        let mut statement = conn
            .prepare("SELECT id FROM entries ORDER BY lower(title), id")
            .map_err(|error| format!("library: {error}"))?;
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|error| format!("library: {error}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("library: {error}"))?;
        ids.into_iter()
            .map(|id| self.get_with(&conn, &id))
            .collect()
    }

    pub fn get(&self, id: &str) -> Result<Entry, String> {
        let conn = self.connect()?;
        self.get_with(&conn, id)
    }

    fn get_with(&self, conn: &Connection, id: &str) -> Result<Entry, String> {
        conn.query_row(
            "SELECT id, title, authors, year, format, added_at_ms, bytes, pages,
                    source_sha256, text_sha256, original, canonical, tags
             FROM entries WHERE id = ?1",
            [id],
            |row| {
                let authors: String = row.get(2)?;
                let tags: String = row.get(12)?;
                let format: String = row.get(4)?;
                Ok(Entry {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    authors: serde_json::from_str(&authors).unwrap_or_default(),
                    year: row.get(3)?,
                    format: Format::parse(&format).unwrap_or(Format::Text),
                    added_at_ms: row.get::<_, i64>(5)? as u64,
                    bytes: row.get::<_, i64>(6)? as u64,
                    pages: row.get::<_, Option<i64>>(7)?.map(|value| value as usize),
                    source_sha256: row.get(8)?,
                    text_sha256: row.get(9)?,
                    original: row.get(10)?,
                    canonical: row.get(11)?,
                    tags: serde_json::from_str(&tags).unwrap_or_default(),
                })
            },
        )
        .optional()
        .map_err(|error| format!("library: {error}"))?
        .ok_or_else(|| format!("no library entry `{id}`"))
    }

    pub fn add_path(
        &self,
        path: impl AsRef<Path>,
        options: &AddOptions,
    ) -> Result<ImportReport, String> {
        let path = path.as_ref();
        let metadata =
            fs::symlink_metadata(path).map_err(|error| format!("{}: {error}", path.display()))?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "{} is a symbolic link; import the file it names",
                path.display()
            ));
        }
        if metadata.is_dir() {
            let mut report = ImportReport::default();
            let mut files = Vec::new();
            walk(path, &mut files)?;
            files.sort();
            for file in files {
                match self.add_file(&file, options) {
                    Ok(outcome) => report.added.push(outcome),
                    Err(reason) => report.skipped.push(Skipped {
                        path: file.display().to_string(),
                        reason,
                    }),
                }
            }
            return Ok(report);
        }
        let outcome = self.add_file(path, options)?;
        Ok(ImportReport {
            added: vec![outcome],
            skipped: Vec::new(),
        })
    }

    fn add_file(&self, path: &Path, options: &AddOptions) -> Result<AddOutcome, String> {
        let metadata =
            fs::symlink_metadata(path).map_err(|error| format!("{}: {error}", path.display()))?;
        if !metadata.is_file() {
            return Err("not a regular file".to_string());
        }
        if metadata.len() > MAX_INPUT_BYTES {
            return Err(format!(
                "{} bytes is larger than the {} byte limit",
                metadata.len(),
                MAX_INPUT_BYTES
            ));
        }
        let bytes = fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
        let Some(format) = detect_format(path, &bytes) else {
            return Err(format!(
                "{} is not a supported format (pdf, epub, tex, md, txt, html)",
                path.display()
            ));
        };
        if matches!(
            format,
            Format::Markdown | Format::Text | Format::Latex | Format::Html
        ) && bytes.contains(&0)
        {
            return Err("the file looks binary; it is not a text format".to_string());
        }
        let title = options
            .title
            .clone()
            .unwrap_or_else(|| title_from_path(path));
        let extracted = extract(&title, path, format, &bytes, options)?;
        if extracted.text.trim().is_empty() {
            return Err("the extracted text is empty".to_string());
        }
        if extracted.text.chars().count() > MAX_TEXT_CHARS {
            return Err(format!(
                "the extracted text is larger than {MAX_TEXT_CHARS} characters; split the document"
            ));
        }
        let source_sha256 = mneme::sha256::hex(&bytes);
        let header = format!(
            "<!-- hades-library format={} original-sha256={} -->\n\n",
            format.label(),
            source_sha256
        );
        let header = if extracted.tags.iter().any(|tag| tag == "ocr") {
            format!(
                "<!-- hades-library format={} original-sha256={} ocr=1 -->\n\n",
                format.label(),
                source_sha256
            )
        } else {
            header
        };
        let canonical_text = format!("{header}{}", extracted.text.trim_end());
        let canonical_text = format!("{canonical_text}\n");
        let text_sha256 = mneme::sha256::hex(canonical_text.as_bytes());
        let id = entry_id(&title, &source_sha256);

        if let Some(existing) = self.find_duplicate(&source_sha256, &text_sha256)? {
            return Ok(AddOutcome {
                entry: existing,
                duplicate: true,
                input: path.display().to_string(),
            });
        }

        let original = format!("files/{id}.{}", format.ext());
        let canonical = format!("text/{id}.md");
        let original_path = self.relative(&original)?;
        let canonical_path = self.relative(&canonical)?;
        if original_path.exists() || canonical_path.exists() {
            return Err(format!(
                "the library already has files for `{id}` but no row"
            ));
        }
        atomic_write(&original_path, &bytes)?;
        if let Err(reason) = atomic_write(&canonical_path, canonical_text.as_bytes()) {
            let _ = fs::remove_file(&original_path);
            return Err(reason);
        }

        let entry = Entry {
            id: id.clone(),
            title: title.trim().to_string(),
            authors: options.authors.clone(),
            year: options.year,
            format,
            added_at_ms: now_ms(),
            bytes: metadata.len(),
            pages: extracted.pages,
            source_sha256,
            text_sha256,
            original,
            canonical,
            tags: sorted_unique(&options.tags, &extracted.tags),
        };
        if let Err(reason) = self.insert(&entry, &chunk_text(&canonical_text, CHUNK_CHARS)) {
            let _ = fs::remove_file(&original_path);
            let _ = fs::remove_file(&canonical_path);
            return Err(reason);
        }
        Ok(AddOutcome {
            entry,
            duplicate: false,
            input: path.display().to_string(),
        })
    }

    fn find_duplicate(&self, source: &str, text: &str) -> Result<Option<Entry>, String> {
        let conn = self.connect()?;
        let id: Option<String> = conn
            .query_row(
                "SELECT id FROM entries WHERE source_sha256 = ?1 OR text_sha256 = ?2 LIMIT 1",
                params![source, text],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| format!("library: {error}"))?;
        match id {
            Some(id) => self.get_with(&conn, &id).map(Some),
            None => Ok(None),
        }
    }

    fn insert(&self, entry: &Entry, chunks: &[Chunk]) -> Result<(), String> {
        let mut conn = self.connect()?;
        let tx = conn
            .transaction()
            .map_err(|error| format!("library: {error}"))?;
        tx.execute(
            "INSERT INTO entries
             (id, title, authors, year, format, added_at_ms, bytes, pages,
              source_sha256, text_sha256, original, canonical, tags)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                entry.id,
                entry.title,
                serde_json::to_string(&entry.authors).unwrap_or_else(|_| "[]".into()),
                entry.year,
                entry.format.label(),
                entry.added_at_ms as i64,
                entry.bytes as i64,
                entry.pages.map(|value| value as i64),
                entry.source_sha256,
                entry.text_sha256,
                entry.original,
                entry.canonical,
                serde_json::to_string(&entry.tags).unwrap_or_else(|_| "[]".into()),
            ],
        )
        .map_err(|error| format!("library: {error}"))?;
        for chunk in chunks {
            tx.execute(
                "INSERT INTO chunks (entry_id, ord, page, start, end, text)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    entry.id,
                    chunk.ord as i64,
                    chunk.page.map(|value| value as i64),
                    chunk.start as i64,
                    chunk.end as i64,
                    chunk.text,
                ],
            )
            .map_err(|error| format!("library: {error}"))?;
        }
        tx.commit().map_err(|error| format!("library: {error}"))
    }

    pub fn text(&self, id: &str) -> Result<(Entry, String), String> {
        let entry = self.get(id)?;
        let path = self.relative(&entry.canonical)?;
        let text =
            fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        Ok((entry, text))
    }

    pub fn excerpt(
        &self,
        id: &str,
        ord: Option<usize>,
        budget: usize,
    ) -> Result<(Entry, String), String> {
        let (entry, text) = self.text(id)?;
        let chunks = chunk_text(&text, CHUNK_CHARS);
        let Some(chunk) = ord
            .and_then(|ord| chunks.iter().find(|chunk| chunk.ord == ord))
            .or_else(|| chunks.first())
        else {
            return Ok((entry, String::new()));
        };
        let first = chunk.ord.saturating_sub(1);
        let last = (chunk.ord + 2).min(chunks.len().saturating_sub(1));
        let slice = &chunks[first..=last];
        let joined = slice
            .iter()
            .map(|chunk| chunk.text.trim())
            .collect::<Vec<_>>()
            .join("\n\n");
        Ok((entry, clip(&joined, budget)))
    }

    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<LibHit>, String> {
        let terms = terms(query);
        if terms.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let conn = self.connect()?;
        let match_query = terms
            .iter()
            .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" OR ");
        let mut statement = conn
            .prepare(
                "SELECT c.entry_id, c.ord, c.page, c.start, c.end, c.text,
                        bm25(chunks_fts) AS score
                 FROM chunks_fts JOIN chunks c ON c.id = chunks_fts.rowid
                 WHERE chunks_fts MATCH ?1
                 ORDER BY score, c.entry_id, c.ord
                 LIMIT ?2",
            )
            .map_err(|error| format!("library: {error}"))?;
        let rows = statement
            .query_map(params![match_query, (limit * 8) as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)? as usize,
                    row.get::<_, Option<i64>>(2)?.map(|value| value as usize),
                    row.get::<_, i64>(3)? as usize,
                    row.get::<_, i64>(4)? as usize,
                    row.get::<_, String>(5)?,
                    row.get::<_, f64>(6)?,
                ))
            })
            .map_err(|error| format!("library: {error}"))?;
        let mut seen = BTreeSet::new();
        let mut hits = Vec::new();
        for row in rows {
            let (id, ord, page, start, end, text, score) =
                row.map_err(|error| format!("library: {error}"))?;
            if !seen.insert(id.clone()) {
                continue;
            }
            let entry = self.get_with(&conn, &id)?;
            hits.push(LibHit {
                entry,
                ord,
                page,
                start,
                end,
                snippet: snippet(&text, &terms, 240),
                score,
            });
            if hits.len() >= limit {
                break;
            }
        }
        Ok(hits)
    }

    pub fn verify(&self, id: Option<&str>) -> Result<VerifyReport, String> {
        let entries = match id {
            Some(id) => vec![self.get(id)?],
            None => self.list()?,
        };
        let conn = self.connect()?;
        let mut report = VerifyReport {
            ok: true,
            entries: Vec::new(),
        };
        for entry in entries {
            let mut problems = Vec::new();
            match fs::read(self.relative(&entry.original)?) {
                Ok(bytes) => {
                    let digest = mneme::sha256::hex(&bytes);
                    if digest != entry.source_sha256 {
                        problems.push(format!(
                            "original hash is {digest}, not {}",
                            entry.source_sha256
                        ));
                    }
                }
                Err(error) => problems.push(format!("original cannot be read: {error}")),
            }
            let text = match fs::read_to_string(self.relative(&entry.canonical)?) {
                Ok(text) => {
                    let digest = mneme::sha256::hex(text.as_bytes());
                    if digest != entry.text_sha256 {
                        problems.push(format!("text hash is {digest}, not {}", entry.text_sha256));
                    }
                    Some(text)
                }
                Err(error) => {
                    problems.push(format!("canonical text cannot be read: {error}"));
                    None
                }
            };
            if let Some(text) = text {
                let mut statement = conn
                    .prepare(
                        "SELECT ord, start, end, text FROM chunks WHERE entry_id = ?1 ORDER BY ord",
                    )
                    .map_err(|error| format!("library: {error}"))?;
                let rows = statement
                    .query_map([&entry.id], |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, String>(3)?,
                        ))
                    })
                    .map_err(|error| format!("library: {error}"))?;
                for row in rows {
                    let (ord, start, end, stored) =
                        row.map_err(|error| format!("library: {error}"))?;
                    let start = start as usize;
                    let end = end as usize;
                    if text.get(start..end) != Some(stored.as_str()) {
                        problems.push(format!("chunk {ord} does not match the canonical text"));
                    }
                }
            }
            if !problems.is_empty() {
                report.ok = false;
            }
            report.entries.push(VerifyItem {
                id: entry.id,
                ok: problems.is_empty(),
                problems,
            });
        }
        Ok(report)
    }

    pub fn remove(&self, id: &str, purge: bool) -> Result<Entry, String> {
        let entry = self.get(id)?;
        let original = self.relative(&entry.original)?;
        let canonical = self.relative(&entry.canonical)?;
        let stamp = now_ms();
        let trash = self.root.join("trash").join(format!("{id}-{stamp}"));
        if !purge {
            fs::create_dir_all(&trash).map_err(|error| format!("{}: {error}", trash.display()))?;
            if original.exists() {
                fs::rename(&original, trash.join("original"))
                    .map_err(|error| format!("{}: {error}", original.display()))?;
            }
            if canonical.exists() {
                fs::rename(&canonical, trash.join("canonical.md"))
                    .map_err(|error| format!("{}: {error}", canonical.display()))?;
            }
        }
        let conn = self.connect()?;
        let removed = conn
            .execute("DELETE FROM entries WHERE id = ?1", [id])
            .map_err(|error| format!("library: {error}"))?;
        if removed == 0 {
            if !purge {
                restore(&trash, &original, &canonical);
            }
            return Err(format!("no library entry `{id}`"));
        }
        if purge {
            let _ = fs::remove_file(original);
            let _ = fs::remove_file(canonical);
        }
        Ok(entry)
    }

    fn relative(&self, relative: &str) -> Result<PathBuf, String> {
        let path = Path::new(relative);
        if path.is_absolute()
            || path.components().any(|part| {
                matches!(
                    part,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err(format!("library path `{relative}` escapes the library"));
        }
        let full = self.root.join(path);
        if !full.starts_with(&self.root) {
            return Err(format!("library path `{relative}` escapes the library"));
        }
        Ok(full)
    }
}

fn restore(trash: &Path, original: &Path, canonical: &Path) {
    if let Some(parent) = original.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Some(parent) = canonical.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::rename(trash.join("original"), original);
    let _ = fs::rename(trash.join("canonical.md"), canonical);
    let _ = fs::remove_dir(trash);
}

fn walk(dir: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = fs::read_dir(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("{}: {error}", dir.display()))?;
        let path = entry.path();
        let metadata =
            fs::symlink_metadata(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            walk(&path, files)?;
        } else if metadata.is_file() {
            files.push(path);
        }
    }
    Ok(())
}

pub fn detect_format(path: &Path, bytes: &[u8]) -> Option<Format> {
    let ext = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "pdf" => return Some(Format::Pdf),
        "epub" => return Some(Format::Epub),
        "tex" | "latex" => return Some(Format::Latex),
        "md" | "markdown" => return Some(Format::Markdown),
        "txt" | "text" => return Some(Format::Text),
        "html" | "htm" => return Some(Format::Html),
        _ => {}
    }
    if bytes.starts_with(b"%PDF-") {
        return Some(Format::Pdf);
    }
    if bytes.starts_with(b"PK\x03\x04")
        && bytes
            .get(..256)
            .is_some_and(|head| head.windows(20).any(|part| part == b"application/epub+zip"))
    {
        return Some(Format::Epub);
    }
    None
}

pub fn entry_id(title: &str, sha256: &str) -> String {
    let mut slug = String::new();
    for ch in title.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
        if slug.len() >= 48 {
            break;
        }
    }
    let slug = slug.trim_matches('-');
    let slug = if slug.is_empty() { "document" } else { slug };
    let short = sha256.get(..8).unwrap_or("00000000");
    format!("l-{slug}-{short}")
}

fn title_from_path(path: &Path) -> String {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("document")
        .replace(['_', '-'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("entry");
    let temp = path.with_file_name(format!(".{file_name}.tmp-{}", std::process::id()));
    {
        let mut file =
            fs::File::create(&temp).map_err(|error| format!("{}: {error}", temp.display()))?;
        file.write_all(bytes)
            .map_err(|error| format!("{}: {error}", temp.display()))?;
        file.sync_all()
            .map_err(|error| format!("{}: {error}", temp.display()))?;
    }
    if let Err(error) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(format!("{}: {error}", path.display()));
    }
    Ok(())
}

struct Extracted {
    text: String,
    pages: Option<usize>,
    tags: Vec<String>,
}

fn extract(
    title: &str,
    path: &Path,
    format: Format,
    bytes: &[u8],
    options: &AddOptions,
) -> Result<Extracted, String> {
    match format {
        Format::Pdf => extract_pdf(path, options.ocr_lang.as_deref().unwrap_or("eng")),
        Format::Epub => extract_epub(path),
        Format::Latex => {
            let text = String::from_utf8(bytes.to_vec())
                .map_err(|_| "the LaTeX file is not UTF-8".to_string())?;
            Ok(Extracted {
                text: latex_to_markdown(&text),
                pages: None,
                tags: Vec::new(),
            })
        }
        Format::Markdown => {
            let text = String::from_utf8(bytes.to_vec())
                .map_err(|_| "the Markdown file is not UTF-8".to_string())?;
            Ok(Extracted {
                text: normalize(&text),
                pages: None,
                tags: Vec::new(),
            })
        }
        Format::Text => {
            let text = String::from_utf8(bytes.to_vec())
                .map_err(|_| "the text file is not UTF-8".to_string())?;
            Ok(Extracted {
                text: format!("# {}\n\n{}", title.trim(), normalize(&text)),
                pages: None,
                tags: Vec::new(),
            })
        }
        Format::Html => {
            let text = String::from_utf8(bytes.to_vec())
                .map_err(|_| "the HTML file is not UTF-8".to_string())?;
            Ok(Extracted {
                text: strip_html(&text),
                pages: None,
                tags: Vec::new(),
            })
        }
    }
}

fn extract_pdf(path: &Path, ocr_lang: &str) -> Result<Extracted, String> {
    let output = Command::new("pdftotext")
        .args(["-layout", "-enc", "UTF-8"])
        .arg(path)
        .arg("-")
        .output()
        .map_err(|error| format!("PDF import needs pdftotext from poppler-utils: {error}"))?;
    if !output.status.success() {
        let said = String::from_utf8_lossy(&output.stderr);
        return Err(format!("pdftotext failed: {}", clip(said.trim(), 300)));
    }
    if output.stdout.len() > 32 * 1024 * 1024 {
        return Err("the PDF text is too large to index".to_string());
    }
    let raw = String::from_utf8_lossy(&output.stdout);
    if raw.trim().is_empty() {
        return ocr_pdf(path, ocr_lang);
    }
    let pages = raw.split('\u{c}').count();
    let mut text = String::new();
    for (index, page) in raw.split('\u{c}').enumerate() {
        text.push_str(&format!("\n\n<!-- page {} -->\n\n", index + 1));
        text.push_str(page.trim_end());
    }
    Ok(Extracted {
        text,
        pages: Some(pages),
        tags: Vec::new(),
    })
}

const OCR_DPI: u32 = 300;
const MAX_OCR_PAGES: usize = 400;
const MAX_OCR_BYTES: usize = 64 * 1024 * 1024;

fn ocr_pdf(path: &Path, lang: &str) -> Result<Extracted, String> {
    let pages = pdf_pages(path)?;
    if pages == 0 {
        return Err("the PDF has no pages to OCR".to_string());
    }
    if pages > MAX_OCR_PAGES {
        return Err(format!(
            "the PDF has {pages} pages; OCR is capped at {MAX_OCR_PAGES}"
        ));
    }
    let scratch =
        std::env::temp_dir().join(format!("hades-ocr-{}-{}", std::process::id(), now_ms()));
    fs::create_dir_all(&scratch).map_err(|error| {
        format!(
            "could not create OCR scratch {}: {error}",
            scratch.display()
        )
    })?;
    let result = (|| {
        let mut text = String::new();
        for page in 1..=pages {
            let page_arg = page.to_string();
            let dpi = OCR_DPI.to_string();
            let output = Command::new("pdftoppm")
                .args([
                    "-r",
                    &dpi,
                    "-gray",
                    "-png",
                    "-singlefile",
                    "-f",
                    &page_arg,
                    "-l",
                    &page_arg,
                ])
                .arg(path)
                .arg(scratch.join("page"))
                .output()
                .map_err(|error| format!("PDF OCR needs pdftoppm from poppler-utils: {error}"))?;
            if !output_ok(&output) {
                return Err(format!(
                    "pdftoppm failed on page {page}: {}",
                    clip(String::from_utf8_lossy(&output.stderr).trim(), 300)
                ));
            }
            let image = scratch.join("page.png");
            let output = Command::new("tesseract")
                .arg(&image)
                .args(["stdout", "-l", lang, "--psm", "3"])
                .output()
                .map_err(|error| format!("PDF OCR needs tesseract: {error}"))?;
            if !output_ok(&output) {
                return Err(format!(
                    "tesseract failed on page {page}: {}",
                    clip(String::from_utf8_lossy(&output.stderr).trim(), 300)
                ));
            }
            let page_text = String::from_utf8_lossy(&output.stdout);
            let page_text = page_text.trim();
            if !page_text.is_empty() {
                text.push_str(&format!("\n\n<!-- page {page} ocr=1 -->\n\n{page_text}"));
            }
            if text.len() > MAX_OCR_BYTES {
                return Err(format!(
                    "OCR produced more than {MAX_OCR_BYTES} bytes; split the PDF"
                ));
            }
        }
        if text.trim().is_empty() {
            return Err(
                "OCR produced no text; the scan may be blank or the language is wrong".to_string(),
            );
        }
        Ok(Extracted {
            text,
            pages: Some(pages),
            tags: vec!["ocr".to_string()],
        })
    })();
    let _ = fs::remove_dir_all(&scratch);
    result
}

fn pdf_pages(path: &Path) -> Result<usize, String> {
    let output = Command::new("pdfinfo")
        .arg(path)
        .output()
        .map_err(|error| format!("PDF OCR needs pdfinfo from poppler-utils: {error}"))?;
    if !output_ok(&output) {
        return Err(format!(
            "pdfinfo failed: {}",
            clip(String::from_utf8_lossy(&output.stderr).trim(), 300)
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let pages = stdout
        .lines()
        .find_map(|line| line.strip_prefix("Pages:"))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .ok_or_else(|| "pdfinfo did not report a page count".to_string())?;
    Ok(pages)
}

fn output_ok(output: &std::process::Output) -> bool {
    output.status.success()
}

fn extract_epub(path: &Path) -> Result<Extracted, String> {
    let package = read_epub_package(path)?;
    let toc = epub_toc(path, &package)?;
    let mut text = String::new();
    if !toc.is_empty() {
        text.push_str("## Contents\n\n");
        for (index, (title, _)) in toc.iter().enumerate() {
            text.push_str(&format!("{}. {}\n", index + 1, title));
        }
        text.push('\n');
    }
    let mut chapters = 0usize;
    for idref in &package.spine {
        let Some(item) = package.manifest.get(idref) else {
            return Err(format!(
                "the EPUB spine names `{idref}` but the manifest does not"
            ));
        };
        let lower = item.media_type.to_ascii_lowercase();
        if !(lower.contains("xhtml") || lower.contains("html")) {
            continue;
        }
        let page = epub_text(path, &item.file)?;
        let (body, notes) = epub_html(&page);
        if body.trim().is_empty() {
            continue;
        }
        chapters += 1;
        let title = toc
            .iter()
            .find(|(_, href)| href == &item.file)
            .map(|(title, _)| title.as_str())
            .unwrap_or(&item.id);
        text.push_str(&format!(
            "\n\n<!-- chapter {chapters}: {title} ({}) -->\n\n",
            item.file
        ));
        text.push_str(body.trim());
        if !notes.is_empty() {
            text.push_str("\n\n## Notes\n\n");
            for (label, note) in notes {
                text.push_str(&format!("[^{label}]: {note}\n"));
            }
        }
    }
    if text.trim().is_empty() {
        return Err("the EPUB spine has no readable chapters".to_string());
    }
    Ok(Extracted {
        text,
        pages: None,
        tags: Vec::new(),
    })
}

#[derive(Debug, Clone)]
struct EpubItem {
    id: String,
    file: String,
    media_type: String,
    properties: String,
}

#[derive(Debug, Clone)]
struct EpubPackage {
    manifest: BTreeMap<String, EpubItem>,
    spine: Vec<String>,
    toc: Option<String>,
}

fn epub_member(path: &Path, member: &str) -> Result<Vec<u8>, String> {
    let output = Command::new("unzip")
        .args(["-p"])
        .arg(path)
        .arg(member)
        .output()
        .map_err(|error| format!("EPUB import needs unzip: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "unzip could not read `{member}`: {}",
            clip(String::from_utf8_lossy(&output.stderr).trim(), 300)
        ));
    }
    Ok(output.stdout)
}

fn epub_text(path: &Path, member: &str) -> Result<String, String> {
    let bytes = epub_member(path, member)?;
    String::from_utf8(bytes).map_err(|_| format!("the EPUB member `{member}` is not UTF-8"))
}

fn read_epub_package(path: &Path) -> Result<EpubPackage, String> {
    let container = epub_text(path, "META-INF/container.xml")
        .map_err(|_| "the EPUB has no META-INF/container.xml".to_string())?;
    let opf = attr_value(&container, "full-path")
        .ok_or_else(|| "the EPUB container does not name an OPF package".to_string())?;
    if !safe_member(&opf) {
        return Err(format!("the EPUB package path `{opf}` is unsafe"));
    }
    let opf_dir = opf.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");
    let xml = epub_text(path, &opf)?;
    let mut manifest = BTreeMap::new();
    for tag in tags(&xml, "item") {
        let Some(id) = attr_value(tag, "id") else {
            continue;
        };
        let Some(href) = attr_value(tag, "href") else {
            continue;
        };
        let Some(file) = join_member(opf_dir, &href) else {
            return Err(format!(
                "the EPUB manifest href `{href}` escapes the package"
            ));
        };
        manifest.insert(
            id.clone(),
            EpubItem {
                id,
                file,
                media_type: attr_value(tag, "media-type").unwrap_or_default(),
                properties: attr_value(tag, "properties").unwrap_or_default(),
            },
        );
        if manifest.len() > MAX_EPUB_MEMBERS {
            return Err(format!(
                "the EPUB manifest has more than {MAX_EPUB_MEMBERS} items"
            ));
        }
    }
    let spine_tag = tags(&xml, "spine").into_iter().next();
    let toc = spine_tag.and_then(|tag| attr_value(tag, "toc"));
    let mut spine = Vec::new();
    for tag in tags(&xml, "itemref") {
        if let Some(idref) = attr_value(tag, "idref") {
            spine.push(idref);
            if spine.len() > MAX_EPUB_MEMBERS {
                return Err(format!(
                    "the EPUB spine has more than {MAX_EPUB_MEMBERS} items"
                ));
            }
        }
    }
    if spine.is_empty() {
        return Err("the EPUB package has no spine".to_string());
    }
    Ok(EpubPackage {
        manifest,
        spine,
        toc,
    })
}

fn epub_toc(path: &Path, package: &EpubPackage) -> Result<Vec<(String, String)>, String> {
    if let Some(item) = package.manifest.values().find(|item| {
        item.properties
            .split_whitespace()
            .any(|value| value == "nav")
    }) {
        let xml = epub_text(path, &item.file)?;
        let mut toc = Vec::new();
        for tag in tags(&xml, "a") {
            let Some(href) = attr_value(tag, "href") else {
                continue;
            };
            let Some(end) = xml[tag_end(&xml, tag)..].to_ascii_lowercase().find("</a>") else {
                continue;
            };
            let start = tag_end(&xml, tag);
            let title = strip_html(&xml[start..start + end]);
            if !title.trim().is_empty() {
                toc.push((
                    title.trim().to_string(),
                    join_member(member_dir(&item.file), &href).unwrap_or(href),
                ));
            }
        }
        if !toc.is_empty() {
            return Ok(toc);
        }
    }
    let Some(toc_id) = package.toc.as_deref() else {
        return Ok(Vec::new());
    };
    let Some(item) = package.manifest.get(toc_id) else {
        return Ok(Vec::new());
    };
    let xml = epub_text(path, &item.file)?;
    let mut toc = Vec::new();
    let mut title: Option<String> = None;
    for tag in tags_any(&xml, &["text", "content"]) {
        if tag.starts_with("<text") {
            title = Some(strip_html(tag_inner(&xml, tag)).trim().to_string());
        } else if let (Some(title), Some(src)) = (title.take(), attr_value(tag, "src"))
            && !title.is_empty()
        {
            toc.push((
                title,
                join_member(member_dir(&item.file), &src).unwrap_or(src),
            ));
        }
    }
    Ok(toc)
}

fn join_member(dir: &str, href: &str) -> Option<String> {
    if href.starts_with('/') || href.contains('\\') {
        return None;
    }
    let mut parts: Vec<&str> = dir.split('/').filter(|part| !part.is_empty()).collect();
    for part in href.split('/') {
        match part {
            "" | "." => {}
            ".." => return None,
            value => parts.push(value),
        }
    }
    Some(parts.join("/"))
}

fn member_dir(file: &str) -> &str {
    file.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("")
}

fn tags<'a>(xml: &'a str, name: &str) -> Vec<&'a str> {
    tags_any(xml, &[name])
        .into_iter()
        .filter(|tag| tag_name(tag) == name)
        .collect()
}

fn tags_any<'a>(xml: &'a str, names: &[&str]) -> Vec<&'a str> {
    let mut found = Vec::new();
    let mut from = 0usize;
    while let Some(at) = xml[from..].find('<') {
        let start = from + at;
        let rest = &xml[start + 1..];
        let end = rest.find('>').map(|end| start + 1 + end + 1);
        let Some(end) = end else { break };
        let tag = &xml[start..end];
        let name = tag_name(tag);
        if names.contains(&name) {
            found.push(tag);
        }
        from = end;
    }
    found
}

fn tag_name(tag: &str) -> &str {
    let rest = tag.trim_start_matches('<').trim_start_matches('/');
    let end = rest
        .find(|ch: char| ch.is_whitespace() || ch == '>' || ch == '/')
        .unwrap_or(rest.len());
    &rest[..end]
}

fn tag_end(xml: &str, tag: &str) -> usize {
    let start = tag.as_ptr() as usize - xml.as_ptr() as usize;
    start + tag.len()
}

fn tag_inner<'a>(xml: &'a str, tag: &'a str) -> &'a str {
    let start = tag_end(xml, tag);
    let name = tag_name(tag);
    let close = format!("</{name}>");
    xml[start..]
        .find(&close)
        .map(|end| &xml[start..start + end])
        .unwrap_or("")
}

fn attr_value(tag: &str, name: &str) -> Option<String> {
    let mut from = 0usize;
    while let Some(at) = tag[from..].find(name) {
        let start = from + at;
        let before = tag[..start].chars().next_back();
        let after = &tag[start + name.len()..];
        if before.is_some_and(char::is_whitespace) {
            let rest = after.trim_start();
            if let Some(rest) = rest.strip_prefix('=') {
                let rest = rest.trim_start();
                let quote = rest.chars().next()?;
                if quote == '"' || quote == '\'' {
                    let value = &rest[1..];
                    let end = value.find(quote)?;
                    return Some(value[..end].to_string());
                }
            }
        }
        from = start + name.len();
    }
    None
}

fn epub_html(html: &str) -> (String, Vec<(String, String)>) {
    let (html, notes) = extract_footnotes(html);
    let html = mark_noterefs(&html);
    let html = replace_images(&html);
    (strip_html(&html), notes)
}

fn extract_footnotes(html: &str) -> (String, Vec<(String, String)>) {
    let mut out = String::new();
    let mut notes = Vec::new();
    let mut from = 0usize;
    while let Some(at) = html[from..].find("<aside") {
        let start = from + at;
        let Some(end) = html[start..].find('>').map(|at| start + at + 1) else {
            break;
        };
        let tag = &html[start..end];
        if tag_name(tag) != "aside" || attr_value(tag, "epub:type").as_deref() != Some("footnote") {
            out.push_str(&html[from..end]);
            from = end;
            continue;
        }
        let Some(close) = html[end..].find("</aside>").map(|at| end + at) else {
            out.push_str(&html[from..end]);
            from = end;
            continue;
        };
        let label = attr_value(tag, "id").unwrap_or_else(|| format!("{}", notes.len() + 1));
        let text = strip_html(&html[end..close]);
        notes.push((label, text.trim().to_string()));
        out.push_str(&html[from..start]);
        from = close + "</aside>".len();
    }
    out.push_str(&html[from..]);
    (out, notes)
}

fn mark_noterefs(html: &str) -> String {
    let mut out = String::new();
    let mut from = 0usize;
    while let Some(at) = html[from..].find("<a") {
        let start = from + at;
        let Some(end) = html[start..].find('>').map(|at| start + at + 1) else {
            break;
        };
        let tag = &html[start..end];
        let Some(href) = attr_value(tag, "href") else {
            out.push_str(&html[from..end]);
            from = end;
            continue;
        };
        if tag_name(tag) != "a" {
            out.push_str(&html[from..end]);
            from = end;
            continue;
        }
        if attr_value(tag, "epub:type").as_deref() != Some("noteref") || !href.starts_with('#') {
            out.push_str(&html[from..end]);
            from = end;
            continue;
        }
        let label = href.trim_start_matches('#');
        let Some(close) = html[end..].find("</a>").map(|at| end + at + 4) else {
            out.push_str(&html[from..end]);
            from = end;
            continue;
        };
        out.push_str(&html[from..start]);
        out.push_str(&format!("[^{label}]"));
        from = close;
    }
    out.push_str(&html[from..]);
    out
}

fn replace_images(html: &str) -> String {
    let mut out = String::new();
    let mut from = 0usize;
    while let Some(at) = html[from..].find("<img") {
        let start = from + at;
        let Some(end) = html[start..].find('>').map(|at| start + at + 1) else {
            break;
        };
        let tag = &html[start..end];
        let alt = attr_value(tag, "alt").unwrap_or_default();
        out.push_str(&html[from..start]);
        if alt.trim().is_empty() {
            out.push_str("![image]");
        } else {
            out.push_str(&format!("![{}]", alt.trim()));
        }
        from = end;
    }
    out.push_str(&html[from..]);
    out
}

fn safe_member(name: &str) -> bool {
    !name.starts_with("__MACOSX")
        && !name.starts_with('/')
        && !name.contains('\\')
        && !name.split('/').any(|part| part == "..")
}

fn normalize(text: &str) -> String {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    format!("{}\n", text.trim_end())
}

/// Expand simple `\newcommand{\name}{value}` definitions before the reading conversion.
/// This is deliberately narrow: no arguments, no conditionals, no TeX execution.
fn expand_latex_macros(input: &str) -> String {
    let mut definitions: Vec<(String, String)> = Vec::new();
    let mut kept = String::new();
    let mut index = 0;
    while index < input.len() {
        let rest = &input[index..];
        let command = ["\\newcommand", "\\renewcommand", "\\providecommand"]
            .into_iter()
            .find(|command| rest.starts_with(command));
        if let Some(command) = command {
            let mut at = index + command.len();
            while at < input.len() && input.as_bytes()[at].is_ascii_whitespace() {
                at += 1;
            }
            if let Some((name, after_name)) = brace_group(input, at) {
                let mut after = after_name;
                while after < input.len() && input.as_bytes()[after].is_ascii_whitespace() {
                    after += 1;
                }
                if input[after..].starts_with('[') {
                    after = input[after..]
                        .find(']')
                        .map(|end| after + end + 1)
                        .unwrap_or(after);
                    while after < input.len() && input.as_bytes()[after].is_ascii_whitespace() {
                        after += 1;
                    }
                }
                if let Some((value, end)) = brace_group(input, after) {
                    definitions.push((name.trim().trim_start_matches('\\').to_string(), value));
                    index = end;
                    continue;
                }
            }
        }
        let ch = rest.chars().next().unwrap();
        kept.push(ch);
        index += ch.len_utf8();
    }
    let mut text = kept;
    for _ in 0..4 {
        let mut changed = false;
        for (name, value) in &definitions {
            let with_braces = format!("\\{name}{{}}");
            if text.contains(&with_braces) {
                text = text.replace(&with_braces, value);
                changed = true;
            }
            let bare = format!("\\{name}");
            let mut rebuilt = String::new();
            let mut from = 0;
            while let Some(at) = text[from..].find(&bare) {
                let start = from + at;
                let end = start + bare.len();
                let next = text[end..].chars().next();
                let is_longer = next.is_some_and(|ch| ch.is_ascii_alphabetic());
                rebuilt.push_str(&text[from..start]);
                if is_longer {
                    rebuilt.push_str(&bare);
                } else {
                    rebuilt.push_str(value);
                    changed = true;
                }
                from = end;
            }
            rebuilt.push_str(&text[from..]);
            text = rebuilt;
        }
        if !changed {
            break;
        }
    }
    text
}

fn brace_group(text: &str, start: usize) -> Option<(String, usize)> {
    if !text[start..].starts_with('{') {
        return None;
    }
    let mut depth = 0usize;
    let mut index = start;
    let bytes = text.as_bytes();
    while index < bytes.len() {
        match bytes[index] {
            b'{' if !is_escaped(text, index) => depth += 1,
            b'}' if !is_escaped(text, index) => {
                depth -= 1;
                if depth == 0 {
                    return Some((text[start + 1..index].to_string(), index + 1));
                }
            }
            _ => {}
        }
        index += 1;
    }
    None
}

pub fn latex_to_markdown(input: &str) -> String {
    let input = input.trim_start_matches('\u{feff}').replace("\r\n", "\n");
    let body = match input.find("\\begin{document}") {
        Some(start) => &input[start + "\\begin{document}".len()..],
        None => input.as_str(),
    };
    let body = body.split("\\end{document}").next().unwrap_or(body);
    let expanded = expand_latex_macros(body);
    let body = expanded.as_str();
    let mut out = String::new();
    let mut index = 0;
    while index < body.len() {
        let rest = &body[index..];
        let ch = rest.chars().next().unwrap();
        if ch == '%' && !is_escaped(body, index) {
            index = rest.find('\n').map(|at| index + at).unwrap_or(body.len());
            continue;
        }
        if ch == '~' {
            out.push(' ');
            index += 1;
            continue;
        }
        if ch != '\\' {
            out.push(ch);
            index += ch.len_utf8();
            continue;
        }
        if let Some(heading) = heading_command(rest) {
            let level = heading.1;
            let arg_start = index + heading.0;
            if let Some(end) = brace_end(body, arg_start) {
                out.push_str(&"#".repeat(level));
                out.push(' ');
                out.push_str(body[arg_start..end].trim());
                out.push_str("\n\n");
                index = end + 1;
                continue;
            }
        }
        if rest.starts_with("\\\\") {
            out.push('\n');
            index += 2;
            continue;
        }
        if rest.starts_with("\\par") {
            out.push_str("\n\n");
            index += 4;
            continue;
        }
        if rest.starts_with("\\item") {
            out.push_str("\n- ");
            index += 5;
            continue;
        }
        if let Some((rendered, next)) = math_span(body, index) {
            out.push_str(&rendered);
            index = next;
            continue;
        }
        if let Some((name, prefix)) = simple_command(rest) {
            if let Some(arg_start) = prefix {
                if let Some(end) = brace_end(body, index + arg_start) {
                    match name {
                        "emph" | "textit" => {
                            out.push('*');
                            out.push_str(body[index + arg_start..end].trim());
                            out.push('*');
                        }
                        "textbf" => {
                            out.push_str("**");
                            out.push_str(body[index + arg_start..end].trim());
                            out.push_str("**");
                        }
                        "texttt" => {
                            out.push('`');
                            out.push_str(body[index + arg_start..end].trim());
                            out.push('`');
                        }
                        "label" | "cite" | "citep" | "citet" | "ref" | "eqref"
                        | "includegraphics" | "begin" | "end" | "documentclass" | "usepackage"
                        | "newcommand" => {}
                        _ => {
                            out.push_str(&body[index..=end]);
                        }
                    }
                    index = end + 1;
                    continue;
                }
            } else if matches!(name, "newpage" | "clearpage" | "pagebreak") {
                out.push_str("\n\n");
                index += 1 + name.len();
                continue;
            }
        }
        if rest.len() > 1 {
            let next = rest.chars().nth(1).unwrap();
            if matches!(next, '%' | '&' | '_' | '#' | '$' | '{' | '}') {
                out.push(next);
                index += 1 + next.len_utf8();
                continue;
            }
        }
        out.push('\\');
        index += 1;
    }
    collapse_blank_lines(&out)
}

fn math_span(text: &str, index: usize) -> Option<(String, usize)> {
    let rest = text.get(index..)?;
    let (open, close, display) = if rest.starts_with("\\[") {
        ("\\[", "\\]", true)
    } else if rest.starts_with("\\(") {
        ("\\(", "\\)", false)
    } else {
        let after = rest.strip_prefix("\\begin{")?;
        let end = after.find('}')?;
        let name = &after[..end];
        let display = match name {
            "equation" | "equation*" | "displaymath" => true,
            "math" => false,
            "align" | "align*" | "gather" | "gather*" | "multline" | "multline*" | "eqnarray"
            | "eqnarray*" => true,
            _ => return None,
        };
        let end_tag = format!("\\end{{{name}}}");
        let start = index + "\\begin{".len() + end + 1;
        let close_at = text.get(start..)?.find(&end_tag)?;
        let content = text.get(start..start + close_at)?.trim();
        let rendered = if display {
            let body = if name.starts_with("align") || name.starts_with("eqnarray") {
                format!("\\begin{{aligned}}\n{content}\n\\end{{aligned}}")
            } else if name.starts_with("gather") {
                format!("\\begin{{gathered}}\n{content}\n\\end{{gathered}}")
            } else {
                content.to_string()
            };
            format!("\n$${body}$$\n")
        } else {
            format!("${content}$")
        };
        return Some((rendered, start + close_at + end_tag.len()));
    };
    let start = index + open.len();
    let close_at = text.get(start..)?.find(close)?;
    let content = text.get(start..start + close_at)?.trim();
    let rendered = if display {
        format!("\n$${content}$$\n")
    } else {
        format!("${content}$")
    };
    Some((rendered, start + close_at + close.len()))
}

fn heading_command(rest: &str) -> Option<(usize, usize)> {
    for (name, level) in [
        ("chapter", 1usize),
        ("section", 2),
        ("subsection", 3),
        ("subsubsection", 4),
        ("paragraph", 5),
    ] {
        let prefix = format!("\\{name}");
        if let Some(after) = rest.strip_prefix(&prefix) {
            let star = after.starts_with('*');
            let brace = if star {
                after.strip_prefix("*{")
            } else {
                after.strip_prefix('{')
            };
            if brace.is_some() {
                return Some((prefix.len() + usize::from(star) + 1, level));
            }
        }
    }
    None
}

fn simple_command(rest: &str) -> Option<(&str, Option<usize>)> {
    let rest = rest.strip_prefix('\\')?;
    let end = rest
        .find(|ch: char| !ch.is_ascii_alphabetic())
        .unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    let name = &rest[..end];
    let after = &rest[end..];
    let arg = after.strip_prefix('{').map(|_| end + 2);
    Some((name, arg))
}

fn brace_end(text: &str, start: usize) -> Option<usize> {
    let mut depth = 1usize;
    for (offset, ch) in text.get(start..)?.char_indices() {
        if ch == '{' {
            depth += 1;
        } else if ch == '}' {
            depth -= 1;
            if depth == 0 {
                return Some(start + offset);
            }
        }
    }
    None
}

fn is_escaped(text: &str, index: usize) -> bool {
    let mut count = 0usize;
    let mut at = index;
    while at > 0 {
        at -= 1;
        if text.as_bytes().get(at) == Some(&b'\\') {
            count += 1;
        } else {
            break;
        }
    }
    count % 2 == 1
}

fn collapse_blank_lines(text: &str) -> String {
    let mut out = String::new();
    let mut blank = 0usize;
    for line in text.lines() {
        if line.trim().is_empty() {
            blank += 1;
            if blank > 2 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    format!("{}\n", out.trim_end())
}

pub fn chunk_text(text: &str, max: usize) -> Vec<Chunk> {
    let markers = page_markers(text);
    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < text.len() {
        let mut end = (start + max).min(text.len());
        while end > start && !text.is_char_boundary(end) {
            end -= 1;
        }
        if end < text.len()
            && let Some(at) = text[start..end].rfind("\n\n")
            && at > max / 3
        {
            end = start + at;
        }
        if end <= start {
            end = (start + max).min(text.len());
            while end > start && !text.is_char_boundary(end) {
                end -= 1;
            }
            if end <= start {
                break;
            }
        }
        let slice = &text[start..end];
        if !slice.trim().is_empty() {
            chunks.push(Chunk {
                ord: chunks.len(),
                page: page_at(&markers, start),
                start,
                end,
                text: slice.to_string(),
            });
        }
        start = end;
    }
    chunks
}

fn page_markers(text: &str) -> Vec<(usize, usize)> {
    let mut markers = Vec::new();
    let mut from = 0usize;
    while let Some(at) = text[from..].find("<!-- page ") {
        let absolute = from + at;
        let Some(rest) = text.get(absolute + "<!-- page ".len()..) else {
            break;
        };
        let digits: String = rest.chars().take_while(|ch| ch.is_ascii_digit()).collect();
        if let Ok(page) = digits.parse::<usize>() {
            markers.push((absolute, page));
        }
        from = absolute + 1;
    }
    markers
}

fn page_at(markers: &[(usize, usize)], offset: usize) -> Option<usize> {
    markers
        .iter()
        .take_while(|(at, _)| *at <= offset)
        .last()
        .map(|(_, page)| *page)
}

pub fn terms(query: &str) -> Vec<String> {
    let mut out = Vec::new();
    for word in query.split(|ch: char| !ch.is_alphanumeric()) {
        let word = word.to_ascii_lowercase();
        if word.len() >= 2 && !out.contains(&word) {
            out.push(word);
        }
    }
    out
}

fn snippet(text: &str, terms: &[String], cap: usize) -> String {
    let text = if text.starts_with("<!-- hades-library")
        && let Some(end) = text.find("-->")
    {
        text[end + 3..].trim_start()
    } else {
        text
    };
    let lower = text.to_ascii_lowercase();
    let first = terms
        .iter()
        .filter_map(|term| lower.find(term))
        .min()
        .unwrap_or(0);
    let start = first.saturating_sub(cap / 3);
    let start = (0..=start)
        .rev()
        .find(|at| text.is_char_boundary(*at))
        .unwrap_or(0);
    clip(text[start..].trim(), cap)
}

pub fn clip(text: &str, cap: usize) -> String {
    if text.chars().count() <= cap {
        return text.to_string();
    }
    let cut = text
        .char_indices()
        .nth(cap)
        .map(|(at, _)| at)
        .unwrap_or(text.len());
    format!("{}...", text[..cut].trim_end())
}

fn sorted_unique(values: &[String], extra: &[String]) -> Vec<String> {
    let mut values: Vec<String> = values
        .iter()
        .chain(extra.iter())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect();
    values.sort();
    values.dedup();
    values
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Parse `library://<id>#c<ord>`. The fragment is optional; without it the reader starts at the first
/// chunk. An ID is a slug with no path separator, so it can never name a file outside the library.
pub fn parse_library_url(url: &str) -> Option<(String, Option<usize>)> {
    let rest = url.strip_prefix("library://")?;
    let (id, fragment) = rest.split_once('#').unwrap_or((rest, ""));
    if id.is_empty()
        || id.contains('/')
        || id.contains('\\')
        || !id
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
    {
        return None;
    }
    let ord = fragment
        .strip_prefix('c')
        .and_then(|value| value.parse::<usize>().ok());
    Some((id.to_string(), ord))
}

pub struct LibraryWeb {
    pub root: PathBuf,
}

impl LibraryWeb {
    pub fn new(root: impl Into<PathBuf>) -> LibraryWeb {
        LibraryWeb { root: root.into() }
    }
}

impl Web for LibraryWeb {
    fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, String> {
        let library = Library::open(&self.root)?;
        Ok(library
            .search(query, limit)?
            .into_iter()
            .map(|hit| SearchHit {
                title: hit.entry.title,
                url: format!("library://{}#c{}", hit.entry.id, hit.ord),
                snippet: hit.snippet,
            })
            .collect())
    }

    fn fetch(&self, url: &str) -> Result<Fetched, String> {
        let (id, ord) =
            parse_library_url(url).ok_or_else(|| format!("`{url}` is not a library URL"))?;
        let library = Library::open(&self.root)?;
        let (entry, body) = library.text(&id)?;
        let excerpt = library.excerpt(&id, ord, 8_000)?.1;
        Ok(Fetched {
            url: url.to_string(),
            final_url: format!("library://{id}"),
            title: entry.title,
            status: None,
            content_type: "text/markdown".to_string(),
            body,
            excerpt: if excerpt.trim().is_empty() {
                None
            } else {
                Some(excerpt)
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        crate::scratch::unique(name)
    }

    #[test]
    fn an_empty_library_lists_nothing() {
        let root = temp("library-empty");
        let library = Library::open(&root).expect("a library");
        assert!(library.is_empty().expect("a count"));
        assert!(library.list().expect("a list").is_empty());
        assert!(library.search("anything", 10).expect("a search").is_empty());
    }

    fn tiny_pdf(text: &str) -> String {
        let content = format!("BT /F1 14 Tf 20 100 Td ({text}) Tj ET");
        let objects = [
            "<</Type/Catalog/Pages 2 0 R>>".to_string(),
            "<</Type/Pages/Kids[3 0 R]/Count 1>>".to_string(),
            "<</Type/Page/Parent 2 0 R/MediaBox[0 0 300 200]/Resources<</Font<</F1 5 0 R>>>>/Contents 4 0 R>>".to_string(),
            format!("<</Length {}>>\nstream\n{}\nendstream", content.len(), content),
            "<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>".to_string(),
        ];
        let mut out = String::from("%PDF-1.4\n");
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.push_str(&format!("{} 0 obj\n{}\nendobj\n", index + 1, object));
        }
        let xref = out.len();
        out.push_str(&format!(
            "xref\n0 {}\n0000000000 65535 f \n",
            objects.len() + 1
        ));
        for offset in offsets {
            out.push_str(&format!("{offset:010} 00000 n \n"));
        }
        out.push_str(&format!(
            "trailer\n<</Size {} /Root 1 0 R>>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        ));
        out
    }

    #[test]
    fn a_pdf_is_converted_with_pdftotext() {
        // poppler is not part of a base image, and the import path has to say so rather than fail obscurely.
        // The test asserts the message when it is absent and the conversion when it is present, so a machine
        // without the tool checks the half of the behaviour that exists there instead of going red.
        let has_tool = std::process::Command::new("pdftotext")
            .arg("-v")
            .output()
            .is_ok();
        if !has_tool {
            eprintln!("skipping PDF conversion: pdftotext is not installed");
            let root = temp("library-pdf-missing");
            let source = root.join("paper.pdf");
            fs::write(&source, tiny_pdf("Hello")).expect("a pdf");
            let library = Library::open(root.join("library")).expect("a library");
            // Extraction may happen at import or on first read, so the complaint has to be found wherever
            // this version raises it.
            let said = match library.add_path(&source, &AddOptions::default()) {
                Ok(report) => match report.added.first() {
                    Some(entry) => match library.text(&entry.entry.id) {
                        Ok(_) => String::new(),
                        Err(error) => error,
                    },
                    None => "the import reported nothing".to_string(),
                },
                Err(error) => error,
            };
            assert!(
                said.contains("pdftotext"),
                "an import without pdftotext must name it: {said}"
            );
            return;
        }
        let root = temp("library-pdf");
        let source = root.join("paper.pdf");
        fs::write(&source, tiny_pdf("Hello from a PDF library")).expect("a pdf");
        let library = Library::open(root.join("library")).expect("a library");
        let report = library
            .add_path(&source, &AddOptions::default())
            .expect("an import");
        assert_eq!(report.added.len(), 1);
        assert_eq!(report.added[0].entry.format, Format::Pdf);
        let (_, text) = library.text(&report.added[0].entry.id).expect("text");
        assert!(text.contains("Hello from a PDF library"), "{text}");
    }

    #[test]
    fn a_scanned_pdf_is_ocrd_and_tagged() {
        let tools = ["pdftoppm", "convert", "tesseract"];
        if tools.iter().any(|tool| {
            std::process::Command::new(tool)
                .arg("--version")
                .output()
                .is_err()
        }) {
            eprintln!("skipping OCR test: {} is not installed", tools.join(", "));
            return;
        }
        let root = temp("library-ocr");
        let source = root.join("text.pdf");
        fs::write(&source, tiny_pdf("OCR sentinel")).expect("a pdf");
        let image = root.join("page.png");
        let status = std::process::Command::new("pdftoppm")
            .args(["-r", "200", "-png", "-singlefile"])
            .arg(&source)
            .arg(root.join("page"))
            .status()
            .expect("pdftoppm");
        assert!(status.success());
        let scanned = root.join("scan.pdf");
        let status = std::process::Command::new("convert")
            .arg(&image)
            .args(["-compress", "jpeg"])
            .arg(&scanned)
            .status()
            .expect("convert");
        assert!(status.success());
        assert!(
            String::from_utf8_lossy(
                &std::process::Command::new("pdftotext")
                    .arg(&scanned)
                    .arg("-")
                    .output()
                    .expect("pdftotext")
                    .stdout
            )
            .trim()
            .is_empty()
        );
        let library = Library::open(root.join("library")).expect("a library");
        let report = library
            .add_path(&scanned, &AddOptions::default())
            .expect("an OCR import");
        let entry = &report.added[0].entry;
        assert_eq!(entry.format, Format::Pdf);
        assert!(
            entry.tags.iter().any(|tag| tag == "ocr"),
            "{:?}",
            entry.tags
        );
        let (_, text) = library.text(&entry.id).expect("text");
        assert!(text.to_ascii_lowercase().contains("ocr sentinel"), "{text}");
        assert!(text.contains("ocr=1"), "{text}");
    }

    #[test]
    fn an_epub_is_unpacked_and_read() {
        let root = temp("library-epub");
        let book = root.join("book");
        fs::create_dir_all(book.join("META-INF")).expect("meta");
        fs::create_dir_all(book.join("OEBPS")).expect("oebps");
        fs::write(book.join("mimetype"), "application/epub+zip").expect("mimetype");
        fs::write(
            book.join("META-INF/container.xml"),
            "<?xml version=\"1.0\"?><container><rootfiles><rootfile full-path=\"OEBPS/content.opf\" media-type=\"application/oebps-package+xml\"/></rootfiles></container>",
        )
        .expect("container");
        fs::write(
            book.join("OEBPS/content.opf"),
            "<?xml version=\"1.0\"?><package version=\"3.0\"><manifest>\
             <item id=\"nav\" href=\"nav.xhtml\" media-type=\"application/xhtml+xml\" properties=\"nav\"/>\
             <item id=\"ch2\" href=\"ch2.xhtml\" media-type=\"application/xhtml+xml\"/>\
             <item id=\"ch1\" href=\"ch1.xhtml\" media-type=\"application/xhtml+xml\"/>\
             </manifest><spine><itemref idref=\"ch2\"/><itemref idref=\"ch1\"/></spine></package>",
        )
        .expect("opf");
        fs::write(
            book.join("OEBPS/nav.xhtml"),
            "<html><body><nav epub:type=\"toc\"><ol><li><a href=\"ch2.xhtml\">Second</a></li><li><a href=\"ch1.xhtml\">First</a></li></ol></nav></body></html>",
        )
        .expect("nav");
        fs::write(
            book.join("OEBPS/ch2.xhtml"),
            "<html><body><h1>Second chapter</h1><p>The EPUB says RDNA1 has a limit.<a epub:type=\"noteref\" href=\"#fn1\">1</a></p><img src=\"chart.png\" alt=\"A measured chart\"/><aside epub:type=\"footnote\" id=\"fn1\"><p>The sample was 12 cards.</p></aside></body></html>",
        )
        .expect("second");
        fs::write(
            book.join("OEBPS/ch1.xhtml"),
            "<html><body><h1>First chapter</h1><p>The opening chapter.</p></body></html>",
        )
        .expect("first");
        let source = root.join("book.epub");
        let status = std::process::Command::new("zip")
            .args(["-q", "-X", "-r"])
            .arg(&source)
            .arg(".")
            .current_dir(&book)
            .status()
            .expect("zip");
        assert!(status.success());
        let library = Library::open(root.join("library")).expect("a library");
        let report = library
            .add_path(&source, &AddOptions::default())
            .expect("an import");
        assert_eq!(report.added[0].entry.format, Format::Epub);
        let (_, text) = library.text(&report.added[0].entry.id).expect("text");
        assert!(text.contains("## Contents"), "{text}");
        let second = text.find("Second chapter").expect("second");
        let first = text.find("First chapter").expect("first");
        assert!(second < first, "spine order was lost:\n{text}");
        assert!(text.contains("![A measured chart]"), "{text}");
        assert!(text.contains("[^fn1]"), "{text}");
        assert!(text.contains("The sample was 12 cards."), "{text}");
    }

    #[test]
    fn a_markdown_file_is_imported_and_searched() {
        let root = temp("library-markdown");
        let source = root.join("paper.md");
        fs::write(
            &source,
            "# A paper\n\nThe Vulkan backend on RDNA1 has a documented limitation.\n",
        )
        .expect("a file");
        let library = Library::open(root.join("library")).expect("a library");
        let report = library
            .add_path(&source, &AddOptions::default())
            .expect("an import");
        assert_eq!(report.added.len(), 1);
        assert!(!report.added[0].duplicate);
        let hits = library.search("Vulkan RDNA1", 10).expect("a search");
        assert_eq!(hits.len(), 1);
        assert!(hits[0].snippet.contains("Vulkan"));
        let (_, text) = library.text(&report.added[0].entry.id).expect("text");
        assert!(text.contains("documented limitation"));
        assert!(library.verify(None).expect("a verify").ok);
    }

    #[test]
    fn a_duplicate_import_returns_the_first_entry() {
        let root = temp("library-duplicate");
        let source = root.join("paper.md");
        fs::write(&source, "The same bytes.\n").expect("a file");
        let library = Library::open(root.join("library")).expect("a library");
        let first = library
            .add_path(&source, &AddOptions::default())
            .expect("first");
        let second = library
            .add_path(&source, &AddOptions::default())
            .expect("second");
        assert!(second.added[0].duplicate);
        assert_eq!(first.added[0].entry.id, second.added[0].entry.id);
        assert_eq!(library.list().expect("a list").len(), 1);
    }

    #[test]
    fn a_latex_section_becomes_a_markdown_heading() {
        let text = "\\documentclass{article}\n\\begin{document}\n\\section{Results}\nThe \\emph{result} is good.\n\\end{document}\n";
        let markdown = latex_to_markdown(text);
        assert!(markdown.contains("## Results"));
        assert!(markdown.contains("*result*"));
        assert!(!markdown.contains("documentclass"));
    }

    #[test]
    fn a_latex_comment_is_dropped_but_an_escaped_percent_is_kept() {
        let text = "A 50\\% result. % this comment goes\nNext line.";
        let markdown = latex_to_markdown(text);
        assert!(markdown.contains("50% result."));
        assert!(!markdown.contains("this comment"));
    }

    #[test]
    fn latex_display_and_inline_math_become_markdown_math() {
        let text = "\\[\\frac{410}{240} = 1.708\\]\nand \\(a+b\\).";
        let markdown = latex_to_markdown(text);
        assert!(
            markdown.contains("$$\\frac{410}{240} = 1.708$$"),
            "{markdown}"
        );
        assert!(markdown.contains("$a+b$"), "{markdown}");
    }

    #[test]
    fn latex_equation_and_align_become_display_math() {
        let equation = "\\begin{equation}E = mc^2\\end{equation}";
        let markdown = latex_to_markdown(equation);
        assert!(markdown.contains("$$E = mc^2$$"), "{markdown}");
        let align = "\\begin{align}a &= b \\\\ c &= d\\end{align}";
        let markdown = latex_to_markdown(align);
        assert!(markdown.contains("\\begin{aligned}"), "{markdown}");
        assert!(markdown.contains("a &= b"), "{markdown}");
        assert!(!markdown.contains("\\begin{align}"), "{markdown}");
    }

    #[test]
    fn a_latex_macro_definition_is_expanded_and_removed() {
        let text = "\\newcommand{\\driver}{RDNA1}\nOn \\driver{} the result holds.";
        let markdown = latex_to_markdown(text);
        assert!(
            markdown.contains("On RDNA1 the result holds."),
            "{markdown}"
        );
        assert!(!markdown.contains("\\newcommand"), "{markdown}");
        assert!(!markdown.contains("{RDNA1}"), "{markdown}");
    }

    #[test]
    fn an_escaped_dollar_is_not_math() {
        let markdown = latex_to_markdown("Cost is \\$5.");
        assert!(markdown.contains("Cost is $5."), "{markdown}");
    }

    #[test]
    fn chunk_offsets_are_utf8_boundaries_and_cover_the_text() {
        let text = format!(
            "{}{}",
            "A paragraph with an accent é. ".repeat(220),
            "\n\nEnd."
        );
        let chunks = chunk_text(&text, 1_400);
        assert!(!chunks.is_empty());
        for chunk in &chunks {
            assert!(text.is_char_boundary(chunk.start));
            assert!(text.is_char_boundary(chunk.end));
            assert_eq!(&text[chunk.start..chunk.end], chunk.text);
        }
    }

    #[test]
    fn a_title_with_punctuation_makes_a_safe_id() {
        let id = entry_id("A Paper: Vol. 2 / Notes", "abcdef1234567890");
        assert_eq!(id, "l-a-paper-vol-2-notes-abcdef12");
        assert!(!id.contains('/'));
    }

    #[test]
    fn an_epub_member_with_traversal_is_skipped() {
        assert!(!safe_member("../outside.xhtml"));
        assert!(!safe_member("/absolute.xhtml"));
        assert!(!safe_member("__MACOSX/foo.xhtml"));
        assert!(safe_member("OEBPS/chapter1.xhtml"));
    }

    #[test]
    fn a_library_url_parses_its_chunk() {
        assert_eq!(
            parse_library_url("library://l-paper-abcdef12#c3"),
            Some(("l-paper-abcdef12".to_string(), Some(3)))
        );
        assert_eq!(
            parse_library_url("library://l-paper-abcdef12"),
            Some(("l-paper-abcdef12".to_string(), None))
        );
        assert_eq!(parse_library_url("library://../etc/passwd"), None);
    }

    #[test]
    fn verify_reports_a_tampered_canonical_file() {
        let root = temp("library-verify");
        let source = root.join("paper.md");
        fs::write(&source, "A verifiable sentence.\n").expect("a file");
        let library = Library::open(root.join("library")).expect("a library");
        let report = library
            .add_path(&source, &AddOptions::default())
            .expect("an import");
        let entry = &report.added[0].entry;
        let path = library.root().join(&entry.canonical);
        fs::write(path, "tampered\n").expect("a write");
        let verify = library.verify(None).expect("a verify");
        assert!(!verify.ok);
        assert!(
            verify.entries[0]
                .problems
                .iter()
                .any(|p| p.contains("text hash"))
        );
    }

    #[test]
    fn remove_moves_to_trash_and_purge_deletes() {
        let root = temp("library-remove");
        let source = root.join("paper.md");
        fs::write(&source, "A removable sentence.\n").expect("a file");
        let library = Library::open(root.join("library")).expect("a library");
        let report = library
            .add_path(&source, &AddOptions::default())
            .expect("an import");
        let id = report.added[0].entry.id.clone();
        library.remove(&id, false).expect("a remove");
        assert!(library.list().expect("a list").is_empty());
        let trash = library.root().join("trash");
        assert!(fs::read_dir(trash).expect("trash").next().is_some());

        let report = library
            .add_path(&source, &AddOptions::default())
            .expect("re-import");
        let id = report.added[0].entry.id.clone();
        library.remove(&id, true).expect("a purge");
        assert!(
            !library
                .root()
                .join("files")
                .join(format!("{id}.md"))
                .exists()
        );
    }
}
