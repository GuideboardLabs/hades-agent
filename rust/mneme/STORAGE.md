# Mneme storage: SQLite is the record, markdown is the export

**Status: signed off 2026-10-17 (C7 in `2026-10-17_2050-hades-h3-replan.md`). The three questions at the
foot are answered; the schema and the contract stand.** No store code landed with this page: the store is
H4a, and this page is its specification.

## The question

Mneme's notes are markdown files today: `Vault<FileVault>` reads and writes them, governance audits them,
the index is rendered from their frontmatter. Every recall is a directory walk. That worked while the vault
was a port and the corpus was empty. It does not survive the Memory seam: Lachesis has to hand a turn a
budgeted subgraph, Clotho has to write an outcome back, and both are queries, not walks.

So: is the **record** the files, or is it a database that renders the files?

## The decision

**SQLite is the record. Markdown is an idempotent export.** The database lives at `~/.hades/mneme.db`; the
configured `vault` is the export root. Every write goes to the database first. The file tree is written from
it, never read back except by an explicit `hades mneme import`.

## Why

1. **Recall is a query.** A cold walk over 12,957 files cost 2.262 s, which is why the repo map has a cache.
   A memory that walks its own tree on every turn is the same mistake in a more expensive place. `tag` and
   `note_fts` answer "notes for this goal, under this budget" without opening a file.
2. **The one-writer rule becomes a lock, not only a type.** The file store has no lock, and nothing in the
   code claims otherwise: two writers on one note are undetected. SQLite gives the same rule a transaction,
   and `BEGIN IMMEDIATE` refuses a second writer instead of interleaving with it.
3. **Atomicity.** `FileVault::write` is `fs::write`, which truncates before it writes. A crash can leave a
   torn note. A transaction leaves the previous row.
4. **The export is a complete backup.** If the database is lost, `import` rebuilds it from the tree, extras
   and provenance included. The tree stays readable by today's `FileVault`, which is what makes the decision
   reversible.

## What stays a file

- `charter.md` is configuration, not memory. It is read at session start and edited by hand; it never enters
  the database.
- `INDEX.md` is a view. It is rendered from the database and written on export, as it is written after every
  vault write today.
- Session logs are `session-tree` JSONL and are not Mneme. They are not copied into this database.

## Schema (version 1)

```sql
PRAGMA journal_mode = wal;
PRAGMA foreign_keys = on;
PRAGMA busy_timeout = 5000;

CREATE TABLE note (
    id           INTEGER PRIMARY KEY,
    area         TEXT NOT NULL CHECK (area IN ('inbox','outbox','research','projects','topics')),
    slug         TEXT NOT NULL,              -- the file name without .md; the export identity
    title        TEXT,                       -- absent stays absent, so a round trip is byte-identical
    summary      TEXT,                       -- absent stays absent; '' is a written empty value
    status       TEXT,                       -- absent stays absent; 'durable' once filed
    source_url   TEXT,
    provenance   TEXT,
    captured     TEXT,                       -- YYYY-MM-DD
    body         TEXT NOT NULL,              -- trimmed, exactly what the body is
    extra        TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(extra)),  -- unknown keys, scalar or list
    created_at   TEXT NOT NULL,              -- ISO 8601 UTC
    updated_at   TEXT NOT NULL,
    exported_hash TEXT,                      -- sha256 of the bytes last written; null = never exported
    UNIQUE (area, slug),
    CHECK (status IS NULL OR status <> 'durable' OR (summary <> '' AND
           (ifnull(source_url,'') <> '' OR ifnull(provenance,'') <> '')))
);

CREATE TABLE tag (
    note_id INTEGER NOT NULL REFERENCES note(id) ON DELETE CASCADE,
    kind    TEXT NOT NULL CHECK (kind IN ('topic','project')),
    ordinal INTEGER NOT NULL,                -- the order the frontmatter listed it in
    value   TEXT NOT NULL CHECK (value <> ''),
    PRIMARY KEY (note_id, kind, ordinal)
) WITHOUT ROWID;
CREATE INDEX tag_retrieval ON tag (kind, value, note_id);

CREATE VIRTUAL TABLE note_fts USING fts5(
    title, summary, body,
    content = 'note', content_rowid = 'id', tokenize = 'unicode61'
);

CREATE TRIGGER note_ai AFTER INSERT ON note BEGIN
    INSERT INTO note_fts(rowid, title, summary, body)
    VALUES (new.id, new.title, new.summary, new.body);
END;
CREATE TRIGGER note_ad AFTER DELETE ON note BEGIN
    INSERT INTO note_fts(note_fts, rowid, title, summary, body)
    VALUES ('delete', old.id, old.title, old.summary, old.body);
END;
CREATE TRIGGER note_au AFTER UPDATE ON note BEGIN
    INSERT INTO note_fts(note_fts, rowid, title, summary, body)
    VALUES ('delete', old.id, old.title, old.summary, old.body);
    INSERT INTO note_fts(rowid, title, summary, body)
    VALUES (new.id, new.title, new.summary, new.body);
END;
```

*(rev2, H4a step 2.)* `tag.ordinal` exists because a frontmatter list is ordered and an export
that sorted its topics would not be byte-identical. `summary` is nullable for the same reason: absent
and `summary:` written empty are different bytes. The v1 label stays because nothing has been released.

The typed links are the two frontmatter keys, stored as typed rows. A body-level `[[...]]` graph is
deliberately **not** in this decision: a `link` table with no writer would be speculative, and the markdown
subset has no link syntax to populate it. It arrives with that syntax, not before it.

## The rules the schema holds, and the one it cannot

Held by the schema: a durable note has a summary and provenance (the CHECK), a tag is never empty, a slug is
unique inside its area, and deleting a note deletes its tags.

Not held by the schema: **a durable note needs at least one topic**, because that is a cross-table rule. It
stays where it is today, in the curator and in the type, and the audit query checks it:

```sql
SELECT n.id FROM note n
WHERE n.status = 'durable'
  AND NOT EXISTS (SELECT 1 FROM tag t WHERE t.note_id = n.id AND t.kind = 'topic');
```

Stale keeps today's rule exactly: `coalesce(captured, substr(slug,1,10))` older than the cutoff, so a note
whose frontmatter carries no date still ages by its filename.

Two pragmas are per connection, not properties of the file: `foreign_keys` and `busy_timeout`. A connection
that forgets `foreign_keys` deletes a note and leaves its tags behind, silently; the store sets both when it
opens, and one test proves the cascade. `journal_mode` is a property of the file and is set once.

## Import and export: the contract

- **Export is idempotent and refuses to destroy a hand edit.** Render the row to bytes with the same
  formatter the file store uses. If the file is absent, write it. If the file's hash equals `exported_hash`,
  write only when the bytes differ, then update the hash. If the file's hash differs from `exported_hash`, a
  person changed the export: refuse, name the path, and require `--force` or an `import` first.
- **Import is explicit and keyed by `(area, slug)`.** It parses frontmatter into columns, unknown keys into
  `extra`, and replaces the row's body and tags. A file moved between areas is a delete and an insert, which
  is honest for an explicit import and is not a sync protocol.
- **Auto-import once.** Opening a vault with no database imports its tree and marks every file exported with
  its own hash. After that the database is the record and nothing imports implicitly.
- **Audit has one implementation.** `audit` reads the store. A tree with no database is audited through a
  one-shot import into `~/.hades/tmp/audit-<pid>.db`, which is removed afterwards; the file-tree reader is
  not kept alive as a second audit.
- **Fences are an import guard, not a table.** A fenced path is skipped at import and reported; nothing fenced
  ever gets a row, so there is nothing to fence on export.

## What the engine gains, and what it does not

The store is the seam Lachesis and Clotho will ask: get by id, tags by kind, and one budgeted query that
returns notes for a goal cut at a token count. `NoteStore` retires from the engine path and survives as the
export writer, because writing markdown is still a file operation.

Not in this decision: embeddings or vector search, retirement of notes, ranking beyond FTS5 bm25, the
session log, and any migration of WEFT.

## Cost

- **One new dependency.** `mneme` currently says its dependency list is empty on purpose. This adds a
  SQLite binding with bundled SQLite and FTS5 enabled, and a C compiler to the build. `serde_json` is used
  for `extra` and is already in the workspace (`harness`, `session-tree`), so it is not a new crate in the
  build. The system `sqlite3` here is
  3.45.1 with FTS5 (checked), so the schema is known-good; the build must not depend on the system library.
- **The port oracle becomes the semantic oracle.** `mneme/tests/port_oracle.rs` and
  `governance/tests/port_oracle.rs` must pass unchanged against the SQL store. A test that needs a new call
  shape means the store leaked its implementation.
- **Hand edits are second class.** The export is for reading, diffing, and recovery. An edit made in the tree
  does nothing until `import`. That is the trade for a record that cannot be half-written.

## Reversal

`export`, stop, delete `~/.hades/mneme.db`, and the tree is the vault again: today's `FileVault` reads it,
the audit runs, the index renders. If the export is not lossless for every column above, this decision is
wrong and the round-trip test is the thing that says so.

## Evidence before any D5 code

1. **Round trip.** Seed a vault with today's `FileVault`: captures, filed notes, extras, unicode, a title
   containing a comma and a bracket. Import, export to a second directory, `diff -r` clean.
2. **Audit parity.** Same counts, same problems, same stale list from the tree and from the database.
3. **Oracle parity.** The two port suites pass against the SQL store without edits.
4. **Conflict.** Edit an exported body by hand, export again without `--force`: refused, file untouched.
5. **Crash.** Kill a writer mid-transaction, reopen, count: the previous state, no torn note.
6. **No fences in the record.** Import a tree with a fenced path: zero rows, one reported path.

## Checked before deciding

Against the system SQLite 3.45.1 on 2026-10-17, with the schema above in a scratch database: it builds; FTS5
indexes inserts, clears a replaced body and answers `match 'cache'`; the durable CHECK refuses a missing
summary and a missing provenance; the tag CHECKs refuse a third kind and an empty value; `json_valid` refuses
a non-JSON `extra`; the no-topic audit query finds a durable note with no tag; the stale fallback ages a note
by its dated slug when `captured` is null. And the trap: the same delete leaves two orphan tags on a
connection without `foreign_keys`, and zero with it. Nothing of the store has been implemented.

## Resolved (2026-10-17)

The three questions were put to the operator, who delegated the call: "make the best call based on our research and
continue on." The answers, and why.

1. **One dependency, inside `mneme` only.** `rusqlite` with bundled SQLite and FTS5, nothing else. FTS5 is
   the ranked retrieval the seam needs; a pure-Rust store without it means writing a tokenizer, an index and
   bm25, which is a second database with one user. Transactions are what turns the one-writer rule from a
   comment into a lock. The dependency does not leave the crate: `governance` and `harness` consume Mneme,
   not SQLite. `mneme/Cargo.toml` changes its comment from "nothing here worth a dependency" to naming this
   one exception and why. Bundled because the machine's 3.45.1 is the design reference, not a runtime
   requirement; the build gains a `cc` requirement the installer must check.
2. **Explicit import only.** A two-way export is sync, and sync needs conflict resolution, tombstones,
   rename detection and a clock over two writers. There is one user and one machine; the tree exists to be
   read, diffed and recovered. The hash refusal covers the one loss that matters, a hand edit overwritten by
   an export, and `--force` is the deliberate override. If a second writer ever appears it is the gateway
   through the database, where `BEGIN IMMEDIATE` already serializes it. The tree is never a writer.
3. **Beside the export.** `~/.hades/mneme.db`, not inside the vault root. The root is the portable, diffable
   surface: a database with `-wal` and `-shm` inside it would be walked by snapshots, copied by `cp -r`, and
   included in every `diff -r` round-trip check, and export would have to exclude three names by hand. Beside
   it, the root is exactly the notes, and state still lives under `~/.hades`. One-shot audit databases go to
   `~/.hades/tmp`.
