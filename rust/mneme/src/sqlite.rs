//! The record: SQLite under the vault, markdown as its export.
//!
//! `STORAGE.md` is the decision and the schema's source of truth. This module is the first build of
//! it: a connection, the pragmas that are per connection, the v1 schema, and a newer `user_version`
//! refused instead of guessed at.
//!
//! The path rule from the decision is the whole routing table: a path is a note if and only if it is
//! `*.md` and its parent is exactly one of the five area folders under the root. Every other path is
//! a plain file on disk, which is how `charter.md` and `INDEX.md` stay files while notes are rows.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{
    Connection, OptionalExtension, Transaction, TransactionBehavior, params, params_from_iter,
};
use serde_json::{Map, Value};

use crate::area::Area;
use crate::fault::Fault;
use crate::fence::fenced;
use crate::frontmatter::{Field, Frontmatter};
use crate::note::relative_posix;
use crate::seams::NoteStore;
use crate::sha256;

/// The schema this build knows. A newer `user_version` is a refusal, never a migration guess.
pub const SCHEMA_VERSION: i64 = 1;

/// The v1 schema, verbatim from `STORAGE.md` (`rev2`: ordered tags, nullable summary). The pragmas in
/// that block are per connection and are applied in [`SqliteStore::open`], not here: `journal_mode` is
/// stored in the file, `foreign_keys` and `busy_timeout` are not, and a batch that forgets them leaves
/// orphan tags without an error.
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS note (
    id           INTEGER PRIMARY KEY,
    area         TEXT NOT NULL CHECK (area IN ('inbox','outbox','research','projects','topics')),
    slug         TEXT NOT NULL,
    title        TEXT,
    summary      TEXT,
    status       TEXT,
    source_url   TEXT,
    provenance   TEXT,
    captured     TEXT,
    body         TEXT NOT NULL,
    extra        TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(extra)),
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL,
    exported_hash TEXT,
    UNIQUE (area, slug),
    CHECK (status IS NULL OR status <> 'durable' OR (ifnull(summary,'') <> '' AND
           (ifnull(source_url,'') <> '' OR ifnull(provenance,'') <> '')))
);

CREATE TABLE IF NOT EXISTS tag (
    note_id INTEGER NOT NULL REFERENCES note(id) ON DELETE CASCADE,
    kind    TEXT NOT NULL CHECK (kind IN ('topic','project')),
    ordinal INTEGER NOT NULL,
    value   TEXT NOT NULL CHECK (value <> ''),
    PRIMARY KEY (note_id, kind, ordinal)
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS tag_retrieval ON tag (kind, value, note_id);

CREATE VIRTUAL TABLE IF NOT EXISTS note_fts USING fts5(
    title, summary, body,
    content = 'note', content_rowid = 'id', tokenize = 'unicode61'
);

CREATE TRIGGER IF NOT EXISTS note_ai AFTER INSERT ON note BEGIN
    INSERT INTO note_fts(rowid, title, summary, body)
    VALUES (new.id, new.title, new.summary, new.body);
END;
CREATE TRIGGER IF NOT EXISTS note_ad AFTER DELETE ON note BEGIN
    INSERT INTO note_fts(note_fts, rowid, title, summary, body)
    VALUES ('delete', old.id, old.title, old.summary, old.body);
END;
CREATE TRIGGER IF NOT EXISTS note_au AFTER UPDATE ON note BEGIN
    INSERT INTO note_fts(note_fts, rowid, title, summary, body)
    VALUES ('delete', old.id, old.title, old.summary, old.body);
    INSERT INTO note_fts(rowid, title, summary, body)
    VALUES (new.id, new.title, new.summary, new.body);
END;
"#;

/// One note a search found, with enough to cite it in a prompt and no more.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    pub path: PathBuf,
    pub title: String,
    pub summary: Option<String>,
    pub provenance: Option<String>,
    pub topics: Vec<String>,
    /// True when an exact topic tag put this note in the result, not only the text match.
    pub topic_match: bool,
    /// The path of the note that replaced this one, when a correction retired it. `None` is current
    /// truth; a hit carrying this is history that recall still shows, ranked after everything current.
    pub superseded_by: Option<String>,
    /// `bm25` when the text matched (lower is better), `0.0` for a topic-only hit.
    pub score: f64,
}

/// The SQLite store: one connection, one root, and the five areas.
pub struct SqliteStore {
    root: PathBuf,
    conn: Connection,
}

/// One row of `note`, before its tags are read.
struct Stored {
    title: Option<String>,
    summary: Option<String>,
    status: Option<String>,
    source_url: Option<String>,
    provenance: Option<String>,
    captured: Option<String>,
    body: String,
    extra: String,
}

impl SqliteStore {
    /// Open (or create) the database and make it current.
    ///
    /// The pragmas are set here rather than in the schema because two of the three are per
    /// connection. A connection that forgets `foreign_keys` deletes a note and leaves its tags
    /// behind, silently, which is the trap the decision page records.
    pub fn open(db: impl AsRef<Path>, root: impl AsRef<Path>) -> Result<Self, Fault> {
        let db = db.as_ref();
        let root = root.as_ref();
        if let Some(parent) = db.parent() {
            fs::create_dir_all(parent).map_err(|error| io(parent, error))?;
        }
        fs::create_dir_all(root).map_err(|error| io(root, error))?;
        let root = fs::canonicalize(root).map_err(|error| io(root, error))?;

        let conn = Connection::open(db).map_err(|error| io(db, error))?;
        conn.busy_timeout(Duration::from_millis(5_000))
            .map_err(|error| io(db, error))?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")
            .map_err(|error| io(db, error))?;
        // This one returns a row (`wal`), so it is a query rather than an execute.
        let _mode: String = conn
            .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
            .map_err(|error| io(db, error))?;
        conn.execute_batch(SCHEMA).map_err(|error| io(db, error))?;

        let found: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|error| io(db, error))?;
        if found == 0 {
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)
                .map_err(|error| io(db, error))?;
        } else if found > SCHEMA_VERSION {
            return Err(Fault::Io {
                path: db.display().to_string(),
                detail: format!(
                    "schema version {found} is newer than this build's {SCHEMA_VERSION}; refusing to guess"
                ),
            });
        }

        Ok(SqliteStore { root, conn })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The database connection, for this module's tests and for the query seam H4b adds.
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    fn path_of(&self, area: Area, slug: &str) -> PathBuf {
        self.root.join(area.folder()).join(format!("{slug}.md"))
    }

    fn area_folder(&self, folder: &Path) -> Option<Area> {
        Area::ALL
            .into_iter()
            .find(|area| folder == self.root.join(area.folder()))
    }

    fn load(&self, area: Area, slug: &str) -> Result<Option<Stored>, Fault> {
        self.conn
            .query_row(
                "SELECT title, summary, status, source_url, provenance, captured, body, extra \
                 FROM note WHERE area = ?1 AND slug = ?2",
                params![area.key(), slug],
                |row| {
                    Ok(Stored {
                        title: row.get(0)?,
                        summary: row.get(1)?,
                        status: row.get(2)?,
                        source_url: row.get(3)?,
                        provenance: row.get(4)?,
                        captured: row.get(5)?,
                        body: row.get(6)?,
                        extra: row.get(7)?,
                    })
                },
            )
            .optional()
            .map_err(|error| io(&self.path_of(area, slug), error))
    }

    fn tags(&self, area: Area, slug: &str) -> Result<Vec<(String, String)>, Fault> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT t.kind, t.value FROM tag t JOIN note n ON n.id = t.note_id \
                 WHERE n.area = ?1 AND n.slug = ?2 ORDER BY t.kind, t.ordinal",
            )
            .map_err(|error| io(&self.path_of(area, slug), error))?;
        let rows = statement
            .query_map(params![area.key(), slug], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|error| io(&self.path_of(area, slug), error))?;
        let mut found = Vec::new();
        for row in rows {
            found.push(row.map_err(|error| io(&self.path_of(area, slug), error))?);
        }
        Ok(found)
    }

    /// The canonical bytes of a note: exactly what [`crate::Vault::write_note`] would have written.
    fn render(&self, area: Area, slug: &str, stored: &Stored) -> Result<String, Fault> {
        let mut frontmatter = Frontmatter {
            title: stored.title.clone(),
            summary: stored.summary.clone(),
            status: stored.status.clone(),
            source_url: stored.source_url.clone(),
            provenance: stored.provenance.clone(),
            captured: stored.captured.clone(),
            extra: decode_extra(&stored.extra).map_err(|detail| Fault::Io {
                path: self.path_of(area, slug).display().to_string(),
                detail,
            })?,
            ..Frontmatter::default()
        };
        for (kind, value) in self.tags(area, slug)? {
            match kind.as_str() {
                "topic" => frontmatter.topics.push(value),
                "project" => frontmatter.projects.push(value),
                other => {
                    return Err(Fault::Io {
                        path: self.path_of(area, slug).display().to_string(),
                        detail: format!("unknown tag kind {other:?}"),
                    });
                }
            }
        }
        Ok(format!(
            "{}\n{}\n",
            frontmatter.render(),
            stored.body.trim()
        ))
    }

    fn store_note(&self, area: Area, slug: &str, text: &str) -> Result<(), Fault> {
        let path = self.path_of(area, slug);
        let (frontmatter, body) = Frontmatter::parse(text);
        let extra = encode_extra(&frontmatter.extra);
        let transaction = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)
            .map_err(|error| io(&path, error))?;
        transaction
            .execute(
                "INSERT INTO note (area, slug, title, summary, status, source_url, provenance, \
                                   captured, body, extra, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, \
                         strftime('%Y-%m-%dT%H:%M:%fZ','now'), strftime('%Y-%m-%dT%H:%M:%fZ','now')) \
                 ON CONFLICT(area, slug) DO UPDATE SET \
                    title = excluded.title, summary = excluded.summary, status = excluded.status, \
                    source_url = excluded.source_url, provenance = excluded.provenance, \
                    captured = excluded.captured, body = excluded.body, extra = excluded.extra, \
                    updated_at = excluded.updated_at",
                params![
                    area.key(),
                    slug,
                    frontmatter.title,
                    frontmatter.summary,
                    frontmatter.status,
                    frontmatter.source_url,
                    frontmatter.provenance,
                    frontmatter.captured,
                    body.trim(),
                    extra
                ],
            )
            .map_err(|error| io(&path, error))?;

        let id: i64 = transaction
            .query_row(
                "SELECT id FROM note WHERE area = ?1 AND slug = ?2",
                params![area.key(), slug],
                |row| row.get(0),
            )
            .map_err(|error| io(&path, error))?;
        transaction
            .execute("DELETE FROM tag WHERE note_id = ?1", [id])
            .map_err(|error| io(&path, error))?;
        for (kind, values) in [
            ("topic", &frontmatter.topics),
            ("project", &frontmatter.projects),
        ] {
            for (ordinal, value) in values.iter().enumerate() {
                transaction
                    .execute(
                        "INSERT INTO tag (note_id, kind, ordinal, value) VALUES (?1, ?2, ?3, ?4)",
                        params![id, kind, ordinal as i64, value],
                    )
                    .map_err(|error| io(&path, error))?;
            }
        }
        transaction.commit().map_err(|error| io(&path, error))
    }

    fn delete_note(&self, area: Area, slug: &str) -> Result<(), Fault> {
        let path = self.path_of(area, slug);
        let transaction = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)
            .map_err(|error| io(&path, error))?;
        transaction
            .execute(
                "DELETE FROM note WHERE area = ?1 AND slug = ?2",
                params![area.key(), slug],
            )
            .map_err(|error| io(&path, error))?;
        transaction.commit().map_err(|error| io(&path, error))
    }

    fn note_paths(&self, area: Option<Area>) -> Result<Vec<PathBuf>, Fault> {
        let sql = match area {
            Some(_) => "SELECT area, slug FROM note WHERE area = ?1 ORDER BY slug",
            None => "SELECT area, slug FROM note ORDER BY area, slug",
        };
        let mut statement = self
            .conn
            .prepare(sql)
            .map_err(|error| io(&self.root, error))?;
        let mut found = Vec::new();
        let collect =
            |rows: &mut rusqlite::Rows<'_>, found: &mut Vec<PathBuf>| -> Result<(), Fault> {
                while let Some(row) = rows.next().map_err(|error| io(&self.root, error))? {
                    let key: String = row.get(0).map_err(|error| io(&self.root, error))?;
                    let slug: String = row.get(1).map_err(|error| io(&self.root, error))?;
                    if let Some(area) = Area::parse(&key) {
                        found.push(self.path_of(area, &slug));
                    }
                }
                Ok(())
            };
        match area {
            Some(area) => {
                let mut rows = statement
                    .query([area.key()])
                    .map_err(|error| io(&self.root, error))?;
                collect(&mut rows, &mut found)?;
            }
            None => {
                let mut rows = statement.query([]).map_err(|error| io(&self.root, error))?;
                collect(&mut rows, &mut found)?;
            }
        }
        found.sort();
        Ok(found)
    }

    /// The disk walk, with the area folders optionally skipped: their notes come from rows, not from
    /// a possibly stale export.
    fn walk_disk(&self, folder: &Path, skip_areas: bool) -> Result<Vec<PathBuf>, Fault> {
        let skipped: Vec<PathBuf> = if skip_areas {
            Area::ALL
                .into_iter()
                .map(|area| self.root.join(area.folder()))
                .collect()
        } else {
            Vec::new()
        };
        let mut found: Vec<PathBuf> = Vec::new();
        let mut pending: Vec<PathBuf> = vec![folder.to_path_buf()];
        while let Some(current) = pending.pop() {
            let entries = match fs::read_dir(&current) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(io(&current, error)),
            };
            for entry in entries {
                let entry = entry.map_err(|error| io(&current, error))?;
                let kind = entry.file_type().map_err(|error| io(&current, error))?;
                let path = entry.path();
                if kind.is_dir() {
                    if !skipped.contains(&path) {
                        pending.push(path);
                    }
                } else if path.extension() == Some(std::ffi::OsStr::new("md")) {
                    found.push(path);
                }
            }
        }
        found.sort();
        Ok(found)
    }

    /// Notes that match any term in their title, summary or body, unioned with exact topic tags.
    ///
    /// `terms` are already normalized (lowercase, three characters or more). The store quotes each one
    /// for FTS5, so a goal containing `AND`, `*` or a quote is a set of words rather than a query. The
    /// order is topic match first, then bm25 ascending, then path; a topic-only hit scores 0.0 and
    /// sorts after a note that matched both.
    pub fn search(&self, terms: &[String], topics: &[String]) -> Result<Vec<SearchHit>, Fault> {
        let mut by_id: HashMap<i64, SearchHit> = HashMap::new();

        if !terms.is_empty() {
            let query = terms
                .iter()
                .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
                .collect::<Vec<_>>()
                .join(" OR ");
            let mut statement = self
                .conn
                .prepare(
                    "SELECT n.id, n.area, n.slug, n.title, n.summary, n.provenance, n.extra,
                            bm25(note_fts, 5.0, 2.0, 1.0) AS score
                     FROM note_fts JOIN note n ON n.id = note_fts.rowid
                     WHERE note_fts MATCH ?1
                     ORDER BY score, n.slug",
                )
                .map_err(|error| io(&self.root, error))?;
            let rows = statement
                .query_map([query], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, f64>(7)?,
                    ))
                })
                .map_err(|error| io(&self.root, error))?;
            for row in rows {
                let (id, area, slug, title, summary, provenance, extra, score) =
                    row.map_err(|error| io(&self.root, error))?;
                let Some(area) = Area::parse(&area) else {
                    continue;
                };
                by_id.entry(id).or_insert_with(|| SearchHit {
                    path: self.root.join(area.folder()).join(format!("{slug}.md")),
                    title: title.unwrap_or_else(|| slug.clone()),
                    summary,
                    provenance,
                    topics: Vec::new(),
                    topic_match: false,
                    superseded_by: retired(&extra),
                    score,
                });
            }
        }

        if !topics.is_empty() {
            let placeholders = vec!["?"; topics.len()].join(",");
            let sql = format!(
                "SELECT n.id, n.area, n.slug, n.title, n.summary, n.provenance, n.extra
                 FROM note n JOIN tag t ON t.note_id = n.id
                 WHERE t.kind = 'topic' AND lower(t.value) IN ({placeholders})
                 GROUP BY n.id
                 ORDER BY n.slug"
            );
            let mut statement = self
                .conn
                .prepare(&sql)
                .map_err(|error| io(&self.root, error))?;
            let rows = statement
                .query_map(params_from_iter(topics), |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, String>(6)?,
                    ))
                })
                .map_err(|error| io(&self.root, error))?;
            for row in rows {
                let (id, area, slug, title, summary, provenance, extra) =
                    row.map_err(|error| io(&self.root, error))?;
                let Some(area) = Area::parse(&area) else {
                    continue;
                };
                by_id
                    .entry(id)
                    .and_modify(|hit| hit.topic_match = true)
                    .or_insert_with(|| SearchHit {
                        path: self.root.join(area.folder()).join(format!("{slug}.md")),
                        title: title.unwrap_or_else(|| slug.clone()),
                        summary,
                        provenance,
                        topics: Vec::new(),
                        topic_match: true,
                        superseded_by: retired(&extra),
                        score: 0.0,
                    });
            }
        }

        if by_id.is_empty() {
            return Ok(Vec::new());
        }

        // Every hit's topics in one query, in frontmatter order.
        let ids: Vec<i64> = by_id.keys().copied().collect();
        let placeholders = vec!["?"; ids.len()].join(",");
        let sql = format!(
            "SELECT note_id, value FROM tag
             WHERE kind = 'topic' AND note_id IN ({placeholders})
             ORDER BY note_id, ordinal"
        );
        let mut statement = self
            .conn
            .prepare(&sql)
            .map_err(|error| io(&self.root, error))?;
        let rows = statement
            .query_map(params_from_iter(&ids), |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|error| io(&self.root, error))?;
        for row in rows {
            let (id, value) = row.map_err(|error| io(&self.root, error))?;
            if let Some(hit) = by_id.get_mut(&id) {
                hit.topics.push(value);
            }
        }

        let mut hits: Vec<SearchHit> = by_id.into_values().collect();
        hits.sort_by(|a, b| {
            b.topic_match
                .cmp(&a.topic_match)
                .then(a.score.total_cmp(&b.score))
                .then(a.path.cmp(&b.path))
        });
        Ok(hits)
    }

    /// Whether the record holds no notes. The CLI uses this to decide whether a vault tree has ever
    /// been imported; nothing else should need it.
    pub fn is_empty(&self) -> Result<bool, Fault> {
        let count: i64 = self
            .conn
            .query_row("SELECT count(*) FROM note", [], |row| row.get(0))
            .map_err(|error| io(&self.root, error))?;
        Ok(count == 0)
    }

    /// Read every note file under `source` into the record. Fenced paths are never read: they are
    /// skipped and reported, the same rule the vault applies.
    pub fn import_tree(&self, source: &Path, fences: &[String]) -> Result<ImportReport, Fault> {
        let mut report = ImportReport::default();
        for path in self.walk_disk(source, false)? {
            let relative = relative_posix(&path, source);
            if fenced(Path::new(&relative), fences) {
                report.fenced.push(relative);
                continue;
            }
            let Some((area, slug)) = note_path(source, &path) else {
                continue;
            };
            let text = fs::read_to_string(&path).map_err(|error| io(&path, error))?;
            self.store_note(area, &slug, &text)?;
            self.set_exported_hash(area, &slug, &sha256::hex(text.as_bytes()))?;
            report.imported += 1;
        }
        Ok(report)
    }

    /// Write every row to `target`, refusing to overwrite a file whose hash proves a hand edit.
    ///
    /// Idempotent: a file whose bytes already match is not touched. A file that differs from the
    /// bytes last written is a conflict unless `force`, because the record is the source and a
    /// silent overwrite is the one data loss this design can commit.
    pub fn export_tree(&self, target: &Path, force: bool) -> Result<ExportReport, Fault> {
        let mut report = ExportReport::default();
        // The five areas exist even when empty: a person opening the vault expects its doors.
        for area in Area::ALL {
            let folder = target.join(area.folder());
            fs::create_dir_all(&folder).map_err(|error| io(&folder, error))?;
        }
        for path in self.note_paths(None)? {
            let Some((area, slug)) = note_path(&self.root, &path) else {
                continue;
            };
            let stored = self.load(area, &slug)?.ok_or_else(|| Fault::MissingNote {
                path: path.display().to_string(),
            })?;
            let rendered = self.render(area, &slug, &stored)?;
            let rendered_hash = sha256::hex(rendered.as_bytes());
            let out = target.join(area.folder()).join(format!("{slug}.md"));
            if let Some(parent) = out.parent() {
                fs::create_dir_all(parent).map_err(|error| io(parent, error))?;
            }

            if let Ok(bytes) = fs::read(&out) {
                let file_hash = sha256::hex(&bytes);
                let unchanged = bytes == rendered.as_bytes();
                if unchanged {
                    if self.exported_hash(area, &slug)?.as_deref() != Some(&file_hash) {
                        self.set_exported_hash(area, &slug, &file_hash)?;
                    }
                    report.unchanged += 1;
                    continue;
                }
                let known = self.exported_hash(area, &slug)?;
                let hand_edited = known.as_deref().is_some_and(|last| last != file_hash);
                if !force && (hand_edited || known.is_none()) {
                    report.refused.push(out);
                    continue;
                }
            }

            fs::write(&out, &rendered).map_err(|error| io(&out, error))?;
            self.set_exported_hash(area, &slug, &rendered_hash)?;
            report.written += 1;
        }
        Ok(report)
    }

    /// Write one note's file, the same rendering `export_tree` uses. A just-recorded note has no file,
    /// so the only conflict is a file that was already there by hand; it is left alone rather than
    /// overwritten, which is the rule the bulk export follows too.
    fn export_note(&self, out: &Path, area: Area, slug: &str) -> Result<(), Fault> {
        let stored = self.load(area, slug)?.ok_or_else(|| Fault::MissingNote {
            path: out.display().to_string(),
        })?;
        let rendered = self.render(area, slug, &stored)?;
        let hash = sha256::hex(rendered.as_bytes());
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent).map_err(|error| io(parent, error))?;
        }
        if let Ok(bytes) = fs::read(out)
            && bytes != rendered.as_bytes()
        {
            return Err(Fault::Io {
                path: out.display().to_string(),
                detail: "a different file is already at the export path; refused to overwrite it"
                    .to_string(),
            });
        }
        fs::write(out, &rendered).map_err(|error| io(out, error))?;
        self.set_exported_hash(area, slug, &hash)?;
        Ok(())
    }

    fn exported_hash(&self, area: Area, slug: &str) -> Result<Option<String>, Fault> {
        self.conn
            .query_row(
                "SELECT exported_hash FROM note WHERE area = ?1 AND slug = ?2",
                params![area.key(), slug],
                |row| row.get(0),
            )
            .optional()
            .map(|row| row.flatten())
            .map_err(|error| io(&self.path_of(area, slug), error))
    }

    fn set_exported_hash(&self, area: Area, slug: &str, hash: &str) -> Result<(), Fault> {
        self.conn
            .execute(
                "UPDATE note SET exported_hash = ?3 WHERE area = ?1 AND slug = ?2",
                params![area.key(), slug, hash],
            )
            .map_err(|error| io(&self.path_of(area, slug), error))?;
        Ok(())
    }
}

/// What an import did. `fenced` is a record, not a failure.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ImportReport {
    pub imported: usize,
    pub fenced: Vec<String>,
}

/// What an export did. `refused` names the hand-edited files it would not overwrite.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ExportReport {
    pub written: usize,
    pub unchanged: usize,
    pub refused: Vec<PathBuf>,
}

/// A path is a note only inside one of the five area folders. The rule is here so the store and its
/// callers cannot disagree about it. The slug is the file name without `.md`, which is the export
/// identity in `STORAGE.md`.
pub fn note_path(root: &Path, path: &Path) -> Option<(Area, String)> {
    if path.extension() != Some(std::ffi::OsStr::new("md")) {
        return None;
    }
    let slug = path.file_name()?.to_str()?.strip_suffix(".md")?.to_string();
    let parent = path.parent()?;
    let area = Area::ALL
        .into_iter()
        .find(|area| parent == root.join(area.folder()))?;
    Some((area, slug))
}

impl NoteStore for SqliteStore {
    fn exists(&self, path: &Path) -> bool {
        match note_path(&self.root, path) {
            Some((area, slug)) => self
                .conn
                .query_row(
                    "SELECT 1 FROM note WHERE area = ?1 AND slug = ?2",
                    params![area.key(), slug],
                    |_| Ok(()),
                )
                .optional()
                .is_ok_and(|found| found.is_some()),
            None => path.exists(),
        }
    }

    fn read(&self, path: &Path) -> Result<String, Fault> {
        let Some((area, slug)) = note_path(&self.root, path) else {
            return fs::read_to_string(path).map_err(|error| io(path, error));
        };
        let stored = self.load(area, &slug)?.ok_or_else(|| Fault::MissingNote {
            path: path.display().to_string(),
        })?;
        self.render(area, &slug, &stored)
    }

    fn write(&self, path: &Path, text: &str) -> Result<(), Fault> {
        match note_path(&self.root, path) {
            Some((area, slug)) => self.store_note(area, &slug, text),
            None => fs::write(path, text).map_err(|error| io(path, error)),
        }
    }

    fn export(&self, path: &Path) -> Result<(), Fault> {
        let Some((area, slug)) = note_path(&self.root, path) else {
            return Ok(());
        };
        self.export_note(path, area, &slug)
    }

    fn make_dirs(&self, path: &Path) -> Result<(), Fault> {
        if self.area_folder(path).is_some() {
            // Areas are logical rows plus a folder that `export` creates; there is nothing to make.
            return Ok(());
        }
        fs::create_dir_all(path).map_err(|error| io(path, error))
    }

    fn remove(&self, path: &Path) -> Result<(), Fault> {
        match note_path(&self.root, path) {
            Some((area, slug)) => self.delete_note(area, &slug),
            None => fs::remove_file(path).map_err(|error| io(path, error)),
        }
    }

    fn remove_export(&self, path: &Path) -> Result<(), Fault> {
        if note_path(&self.root, path).is_none() {
            return Ok(());
        }
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io(path, error)),
        }
    }

    fn list(&self, folder: &Path) -> Result<Vec<PathBuf>, Fault> {
        if let Some(area) = self.area_folder(folder) {
            return self.note_paths(Some(area));
        }
        let entries = match fs::read_dir(folder) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(io(folder, error)),
        };
        let mut found: Vec<PathBuf> = Vec::new();
        for entry in entries {
            let path = entry.map_err(|error| io(folder, error))?.path();
            if path.extension() == Some(std::ffi::OsStr::new("md")) {
                found.push(path);
            }
        }
        found.sort();
        Ok(found)
    }

    fn walk(&self, folder: &Path) -> Result<Vec<PathBuf>, Fault> {
        if let Some(area) = self.area_folder(folder) {
            return self.note_paths(Some(area));
        }
        if folder == self.root.as_path() {
            let mut found = self.note_paths(None)?;
            found.extend(self.walk_disk(folder, true)?);
            found.sort();
            found.dedup();
            return Ok(found);
        }
        self.walk_disk(folder, false)
    }
}

impl std::fmt::Debug for SqliteStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteStore")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

fn io(path: &Path, detail: impl std::fmt::Display) -> Fault {
    Fault::Io {
        path: path.display().to_string(),
        detail: detail.to_string(),
    }
}

/// The note that retired this one, read from the extra frontmatter. An extra that will not decode is no
/// retirement rather than an error: the note stays current, which is the safe direction.
fn retired(extra: &str) -> Option<String> {
    let extra = decode_extra(extra).ok()?;
    match extra.get("superseded_by") {
        Some(Field::Scalar(text)) if !text.trim().is_empty() => Some(text.trim().to_string()),
        _ => None,
    }
}

fn encode_extra(extra: &BTreeMap<String, Field>) -> String {
    let mut object = Map::new();
    for (key, field) in extra {
        let value = match field {
            Field::Scalar(text) => Value::String(text.clone()),
            Field::List(items) => Value::Array(items.iter().cloned().map(Value::String).collect()),
        };
        object.insert(key.clone(), value);
    }
    Value::Object(object).to_string()
}

fn decode_extra(raw: &str) -> Result<BTreeMap<String, Field>, String> {
    let parsed: Value =
        serde_json::from_str(raw).map_err(|error| format!("extra is not JSON: {error}"))?;
    let object = parsed
        .as_object()
        .ok_or_else(|| "extra is not a JSON object".to_string())?;
    let mut extra = BTreeMap::new();
    for (key, value) in object {
        let field = match value {
            Value::String(text) => Field::Scalar(text.clone()),
            Value::Array(items) => {
                let mut list = Vec::new();
                for item in items {
                    let text = item.as_str().ok_or_else(|| {
                        format!("extra {key:?} holds a list member that is not a string")
                    })?;
                    list.push(text.to_string());
                }
                Field::List(list)
            }
            _ => {
                return Err(format!(
                    "extra {key:?} is neither a string nor a list of strings"
                ));
            }
        };
        extra.insert(key.clone(), field);
    }
    Ok(extra)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("the crate is inside the workspace")
            .join("target/scratch/sqlite")
            .join(name);
        fs::create_dir_all(&base).expect("scratch");
        fs::canonicalize(&base).expect("canonical scratch")
    }

    /// A scratch directory with nothing left from the previous run.
    fn clean(name: &str) -> PathBuf {
        let dir = scratch(name);
        fs::remove_dir_all(&dir).expect("clear scratch");
        fs::create_dir_all(&dir).expect("scratch");
        fs::canonicalize(&dir).expect("canonical scratch")
    }

    /// A fresh home, database and export removed, so a test never reads another run's rows.
    fn fresh(name: &str) -> (SqliteStore, PathBuf) {
        let dir = clean(name);
        let root = dir.join("vault");
        let store = SqliteStore::open(dir.join("mneme.db"), &root).expect("open");
        (store, root)
    }

    fn reopen(name: &str) -> (SqliteStore, PathBuf) {
        let dir = scratch(name);
        let root = dir.join("vault");
        (
            SqliteStore::open(dir.join("mneme.db"), &root).expect("reopen"),
            root,
        )
    }

    fn note(text: &str, topics: &[&str], extra: &[(&str, Field)]) -> String {
        let frontmatter = Frontmatter {
            title: Some("A, [bracket]".to_string()),
            summary: Some("s".to_string()),
            status: Some("durable".to_string()),
            topics: topics.iter().map(|topic| topic.to_string()).collect(),
            projects: vec!["hades".to_string()],
            source_url: Some("https://example.test/x".to_string()),
            provenance: Some("run 1".to_string()),
            captured: Some("2026-10-17".to_string()),
            extra: extra
                .iter()
                .map(|(key, field)| (key.to_string(), field.clone()))
                .collect(),
        };
        format!("{}\n{}\n", frontmatter.render(), text.trim())
    }

    /// A capture is a file as well as a row: the Inbox a person opens holds the note, and the
    /// index that counts candidates points at files that exist.
    #[test]
    fn a_capture_is_exported_to_the_inbox_it_counts() {
        let (store, root) = fresh("capture-export");
        let vault = crate::Vault::new(root.clone(), store, crate::SystemClock);
        let path = vault
            .capture("first line\nsecond line\n", None, None)
            .expect("capture");
        assert!(
            path.exists(),
            "the capture must be a file, not only a row: {}",
            path.display()
        );
        let text = fs::read_to_string(&path).expect("the exported file");
        assert!(text.contains("first line"), "{text}");
        assert!(text.contains("second line"), "{text}");
        let index = fs::read_to_string(root.join(crate::INDEX_NAME)).expect("the index");
        assert!(index.contains("1 candidate"), "{index}");
    }

    #[test]
    fn open_twice_is_idempotent() {
        let (store, _root) = fresh("open-twice");
        let version: i64 = store
            .connection()
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        drop(store);
        let (store, _root) = reopen("open-twice");
        let count: i64 = store
            .connection()
            .query_row("SELECT count(*) FROM note", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn a_newer_schema_is_refused() {
        let (store, _root) = fresh("newer");
        store
            .connection()
            .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        drop(store);
        let dir = scratch("newer");
        let refused =
            SqliteStore::open(dir.join("mneme.db"), dir.join("vault")).expect_err("must refuse");
        assert!(refused.to_string().contains("newer than this build"));
    }

    #[test]
    fn foreign_keys_is_on_for_this_connection() {
        let (store, _root) = fresh("cascade");
        let conn = store.connection();
        conn.execute(
            "INSERT INTO note (area, slug, body, created_at, updated_at) VALUES ('inbox','a','b','t','t')",
            [],
        )
        .unwrap();
        let id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO tag (note_id, kind, ordinal, value) VALUES (?1,'topic',0,'memory')",
            [id],
        )
        .unwrap();
        conn.execute("DELETE FROM note WHERE id = ?1", [id])
            .unwrap();
        let tags: i64 = conn
            .query_row("SELECT count(*) FROM tag", [], |row| row.get(0))
            .unwrap();
        assert_eq!(tags, 0, "the cascade did not fire, so foreign_keys is off");
    }

    #[test]
    fn fts5_is_compiled_in() {
        let (store, _root) = fresh("fts");
        store
            .connection()
            .execute(
                "INSERT INTO note (area, slug, title, body, created_at, updated_at) \
                 VALUES ('research','x','Recall','a body about cache invalidation','t','t')",
                [],
            )
            .unwrap();
        let hits: i64 = store
            .connection()
            .query_row(
                "SELECT count(*) FROM note_fts WHERE note_fts MATCH 'cache'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(hits, 1);
    }

    #[test]
    fn a_note_round_trips_through_the_store() {
        let (store, root) = fresh("round-trip");
        let path = root.join(Area::Research.folder()).join("2026-10-17-x.md");
        let text = note(
            "The body.",
            &["zeta", "alpha"],
            &[
                ("review", Field::Scalar("later".to_string())),
                (
                    "tags_extra",
                    Field::List(vec!["a".to_string(), "b".to_string()]),
                ),
            ],
        );
        store.write(&path, &text).expect("write");
        assert!(store.exists(&path));
        let read = store.read(&path).expect("read");
        assert_eq!(read, text, "the canonical bytes changed");
        assert_eq!(
            store.list(&root.join(Area::Research.folder())).unwrap(),
            vec![path.clone()]
        );
    }

    #[test]
    fn a_list_keeps_its_order() {
        let (store, root) = fresh("tag-order");
        let path = root.join(Area::Research.folder()).join("ordered.md");
        store
            .write(&path, &note("Body.", &["zeta", "alpha", "mid"], &[]))
            .expect("write");
        let read = store.read(&path).expect("read");
        assert!(
            read.contains("topics: [zeta, alpha, mid]"),
            "order was lost: {read}"
        );
    }

    #[test]
    fn charter_is_a_file_and_never_a_row() {
        let (store, root) = fresh("charter");
        let path = root.join("charter.md");
        store.write(&path, "# Charter\n").expect("write");
        assert_eq!(fs::read_to_string(&path).unwrap(), "# Charter\n");
        let count: i64 = store
            .connection()
            .query_row("SELECT count(*) FROM note", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
        assert_eq!(store.read(&path).unwrap(), "# Charter\n");
    }

    #[test]
    fn walk_of_the_root_is_rows_plus_root_files() {
        let (store, root) = fresh("walk-root");
        store
            .write(&root.join(Area::Inbox.folder()).join("a.md"), "body\n")
            .expect("inbox");
        store
            .write(&root.join(Area::Research.folder()).join("b.md"), "body\n")
            .expect("research");
        store
            .write(&root.join("INDEX.md"), "# index\n")
            .expect("index");
        let walked = store.walk(&root).expect("walk");
        assert_eq!(walked.len(), 3, "{walked:?}");
        assert!(walked.iter().any(|path| path.ends_with("a.md")));
        assert!(walked.iter().any(|path| path.ends_with("b.md")));
        assert!(walked.iter().any(|path| path.ends_with("INDEX.md")));
    }

    #[test]
    fn a_move_leaves_one_row() {
        let (store, root) = fresh("move");
        let inbox = root.join(Area::Inbox.folder()).join("x.md");
        let research = root.join(Area::Research.folder()).join("x.md");
        store.write(&inbox, "body\n").expect("inbox");
        store.write(&research, "body\n").expect("research");
        store.remove(&inbox).expect("remove");
        let count: i64 = store
            .connection()
            .query_row("SELECT count(*) FROM note", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
        assert!(store.exists(&research));
    }

    /// A source tree with two notes, canonical bytes, one with extras and unicode.
    fn seed(source: &Path) {
        let _ = fs::remove_dir_all(source);
        fs::create_dir_all(source).expect("seed");
        let first = note(
            "The body, with café.",
            &["zeta", "alpha"],
            &[
                ("kept", Field::Scalar("yes".to_string())),
                ("tags", Field::List(vec!["a".to_string(), "b".to_string()])),
            ],
        );
        fs::create_dir_all(source.join(Area::Research.folder())).unwrap();
        fs::write(
            source.join(Area::Research.folder()).join("2026-10-17-x.md"),
            &first,
        )
        .unwrap();
        let second = note("Another body.", &["memory"], &[]);
        fs::create_dir_all(source.join(Area::Inbox.folder())).unwrap();
        fs::write(
            source.join(Area::Inbox.folder()).join("2026-10-17-y.md"),
            &second,
        )
        .unwrap();
    }

    fn bytes(path: &Path) -> String {
        fs::read_to_string(path).expect("read")
    }

    #[test]
    fn import_then_export_is_byte_identical() {
        let source = scratch("round-source");
        seed(&source);
        let (store, home) = fresh("round-store");
        let imported = store.import_tree(&source, &[]).expect("import");
        assert_eq!(imported.imported, 2);
        let target = home.join("export");
        let report = store.export_tree(&target, false).expect("export");
        assert_eq!(report.written, 2, "{report:?}");
        for area in [Area::Research, Area::Inbox] {
            for entry in fs::read_dir(source.join(area.folder())).unwrap() {
                let from = entry.unwrap().path();
                let to = target.join(area.folder()).join(from.file_name().unwrap());
                assert_eq!(bytes(&from), bytes(&to), "{}", from.display());
            }
        }
    }

    #[test]
    fn export_is_idempotent() {
        let source = scratch("idem-source");
        seed(&source);
        let (store, home) = fresh("idem-store");
        store.import_tree(&source, &[]).expect("import");
        let target = home.join("export");
        store.export_tree(&target, false).expect("first");
        let report = store.export_tree(&target, false).expect("second");
        assert_eq!(report.written, 0);
        assert_eq!(report.unchanged, 2);
    }

    #[test]
    fn export_refuses_to_overwrite_a_hand_edit() {
        let source = scratch("conflict-source");
        seed(&source);
        let (store, home) = fresh("conflict-store");
        store.import_tree(&source, &[]).expect("import");
        let target = home.join("export");
        store.export_tree(&target, false).expect("export");
        let path = target.join(Area::Research.folder()).join("2026-10-17-x.md");
        let mut edited = bytes(&path);
        edited.push_str("\nhand edit\n");
        fs::write(&path, &edited).unwrap();

        let refused = store.export_tree(&target, false).expect("export");
        assert_eq!(refused.refused, vec![path.clone()]);
        assert_eq!(bytes(&path), edited, "the file was not touched");

        let forced = store.export_tree(&target, true).expect("forced");
        assert_eq!(forced.written, 1);
        assert_ne!(bytes(&path), edited);
    }

    #[test]
    fn a_fenced_path_is_never_imported() {
        let source = scratch("fence-source");
        seed(&source);
        fs::write(
            source.join(Area::Research.folder()).join("secret.md"),
            "---\ntitle: Secret\n---\n\nno\n",
        )
        .unwrap();
        let (store, _home) = fresh("fence-store");
        let report = store
            .import_tree(&source, &["20 Research/secret.md".to_string()])
            .expect("import");
        assert_eq!(report.imported, 2);
        assert_eq!(report.fenced, vec!["20 Research/secret.md".to_string()]);
        let count: i64 = store
            .connection()
            .query_row("SELECT count(*) FROM note", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn search_finds_text_and_exact_topics_then_dedups() {
        let (store, root) = fresh("search");
        let lantern = root.join(Area::Research.folder()).join("lantern.md");
        let systems = root.join(Area::Research.folder()).join("systems.md");
        let both = root.join(Area::Research.folder()).join("both.md");
        store
            .write(&lantern, &note("The lantern was green.", &["memory"], &[]))
            .unwrap();
        store
            .write(&systems, &note("An unrelated body.", &["systems"], &[]))
            .unwrap();
        store
            .write(
                &both,
                &note("A lantern inside a systems note.", &["systems"], &[]),
            )
            .unwrap();

        let hits = store
            .search(&["lantern".to_string()], &["systems".to_string()])
            .expect("search");
        let names: Vec<String> = hits
            .iter()
            .map(|hit| hit.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["both.md", "systems.md", "lantern.md"]);
        assert!(hits[0].topic_match && hits[0].score < 0.0);
        assert!(hits[1].topic_match && hits[1].score == 0.0);
        assert!(!hits[2].topic_match && hits[2].score < 0.0);
        assert_eq!(hits[0].topics, vec!["systems".to_string()]);

        let text_only = store.search(&["lantern".to_string()], &[]).expect("search");
        assert_eq!(text_only.len(), 2);
        assert!(text_only.iter().all(|hit| !hit.topic_match));
    }

    #[test]
    fn search_quotes_the_terms_instead_of_reading_them_as_syntax() {
        let (store, root) = fresh("search-syntax");
        let note_path = root.join(Area::Research.folder()).join("gate.md");
        store
            .write(&note_path, &note("The AND gate is open.", &[], &[]))
            .unwrap();

        assert_eq!(store.search(&["and".to_string()], &[]).unwrap().len(), 1);
        for term in ["*", "-", ":", "a\"b", "NEAR("] {
            let hits = store
                .search(&[term.to_string()], &[])
                .unwrap_or_else(|error| panic!("{term}: {error}"));
            assert!(hits.is_empty(), "{term} matched something");
        }
    }

    #[test]
    fn search_is_empty_with_no_terms_and_after_the_note_is_gone() {
        let (store, root) = fresh("search-empty");
        assert!(store.search(&[], &[]).unwrap().is_empty());

        let path = root.join(Area::Research.folder()).join("gone.md");
        store.write(&path, &note("A lantern.", &[], &[])).unwrap();
        assert_eq!(
            store.search(&["lantern".to_string()], &[]).unwrap().len(),
            1
        );
        store.remove(&path).unwrap();
        assert!(
            store
                .search(&["lantern".to_string()], &[])
                .unwrap()
                .is_empty()
        );
    }

    /// The parent commits one note, starts a child that opens its own transaction and inserts a second
    /// note, kills it before COMMIT, and reopens. The record must hold exactly the committed state.
    #[test]
    fn a_kill_between_begin_and_commit_leaves_the_previous_state() {
        if std::env::var_os("HADES_CRASH_CHILD").is_some() {
            return crash_child();
        }

        let (store, root) = fresh("crash");
        let home = root.parent().expect("the home").to_path_buf();
        let db = home.join("mneme.db");
        let ready = home.join("ready");
        let kept = root.join(Area::Inbox.folder()).join("kept.md");
        store
            .write(&kept, &note("Kept before the crash.", &[], &[]))
            .expect("the committed note");

        let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "sqlite::tests::a_kill_between_begin_and_commit_leaves_the_previous_state",
                "--nocapture",
            ])
            .env("HADES_CRASH_CHILD", "1")
            .env("HADES_CRASH_DB", &db)
            .env("HADES_CRASH_READY", &ready)
            .spawn()
            .expect("the child");

        for _ in 0..400 {
            if ready.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(ready.exists(), "the child never entered its transaction");
        child.kill().expect("kill the child");
        let _ = child.wait();

        let reopened = SqliteStore::open(&db, &root).expect("reopen after the crash");
        assert!(
            reopened.exists(&kept),
            "the committed note did not survive the crash"
        );
        assert!(
            !reopened.exists(&root.join(Area::Inbox.folder()).join("lost.md")),
            "an uncommitted insert survived the crash"
        );
    }

    /// The child half of the test above. It never returns: the parent kills it inside the transaction.
    fn crash_child() {
        let db = std::env::var("HADES_CRASH_DB").expect("db");
        let ready = std::env::var("HADES_CRASH_READY").expect("ready");
        let conn = rusqlite::Connection::open(db).expect("open");
        conn.execute_batch("PRAGMA foreign_keys=on; BEGIN IMMEDIATE;")
            .expect("begin");
        conn.execute(
            "INSERT INTO note (area, slug, title, provenance, body, created_at, updated_at)
             VALUES ('inbox', 'lost', 'Lost', 'test', 'uncommitted', '2026-10-17T00:00:00Z', '2026-10-17T00:00:00Z')",
            [],
        )
        .expect("insert");
        fs::write(&ready, b"ready").expect("signal");
        std::thread::sleep(std::time::Duration::from_secs(30));
        conn.execute_batch("COMMIT;").expect("commit");
        panic!("the parent did not kill the child");
    }
}
