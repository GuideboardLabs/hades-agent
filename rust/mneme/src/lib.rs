//! Mneme: the second brain that ships with Hades Agent.
//!
//! The name is the elder Muse of memory, not a product. It is a second system's memory, with its own
//! vault and its own state; nothing here reads the running install's notes.
//!
//! A port of the Python spec at `hades/vault.py`. The tests in `tests/port_oracle.rs` mirror the
//! Python suite, and the port counts as done when those behaviours hold here unchanged.
//!
//! Six areas, topics as notes, frontmatter as the graph:
//!
//! ```text
//! <vault>/
//!     charter.md            who the agent is, who the user is, what the work is for
//!     TAXONOMY.md           the 26 fixed categories, generated
//!     INDEX.md              generated from frontmatter, never hand edited
//!     00 Inbox/             captures land here and only here
//!     10 Outbox/            work that leaves the machine, and messages for other agents
//!     20 Research/          findings, filed under their topic
//!     30 Projects/          hubs, one folder each, with research
//!     40 Products/          projects that stopped moving, frozen as their own thing
//!     50 Topics/            the spine: category / topic / subtopic
//! ```
//!
//! The rules are enforced here rather than asked for. A note is **durable** only with a summary, at
//! least one topic and provenance, and without a topic it cannot leave the Inbox, so the graph has
//! no orphans by construction. The Inbox is a transit zone and a human review queue, so candidates
//! past their welcome are reported as stale rather than quietly kept. Fenced paths are never read
//! and never indexed.
//!
//! **The vault is a directory. It is not the corpus.** The live vault on disk also contains material that is
//! deliberately outside the knowledge graph (family, health, personal profiles), and a walk that equates the
//! two reads them. So this crate takes the *directory* because that is where the notes are, and every walk
//! applies the fence list: [`Vault::iter_notes`] and [`Vault::walk`] are the corpus, and a path is never the
//! corpus by itself. When the two need distinguishing in prose, the directory is "the vault" and the notes
//! outside the fences are the knowledge graph. Nothing in this crate may call the directory the whole thing.
//!
//! Two seams and nothing else, because varying parameters are values rather than seams:
//! [`seams::NoteStore`] touches the disk and [`seams::Clock`] supplies the day. Everything else is a
//! type, and a rule that can become a type does: the area, the day, the frontmatter, the note and
//! the fault.
//!
//! A fenced path is a **record, not a defect**: the audit prints it so a person can see the boundary is where
//! it should be, and does not fail for it. See `governance::report::AuditReport::fenced`.
//!
//! One deliberate deviation from the Python, and it is the one the port earns: an unknown area has
//! no counterpart to `UnknownArea` here, because [`Area`] is an enum. A string area is refused by
//! [`Area::parse`] at the boundary, so it cannot reach a write at all.

pub mod area;
pub mod charter;
pub mod clock;
pub mod date;
pub mod fault;
pub mod fence;
pub mod frontmatter;
pub mod index;
pub mod note;
pub mod permit;
pub mod refs;
pub mod seams;
pub mod sha256;
pub mod sqlite;
pub mod store;
pub mod topic;
pub mod vault;

pub use area::Area;
pub use charter::Charter;
pub use clock::SystemClock;
pub use date::Date;
pub use fault::Fault;
pub use fence::fenced;
pub use frontmatter::{Field, Frontmatter};
pub use index::INDEX_NAME;
pub use note::{Note, relative_posix, slugify};
pub use permit::{Holder, WritePermit};
pub use refs::ResearchRef;
pub use seams::{Clock, NoteStore};
pub use sqlite::{ExportReport, ImportReport, SearchHit, SqliteStore};
pub use store::FileVault;
pub use topic::Category;
pub use vault::{Filing, Vault};

/// A capture or a filed note older than this many days is reported as stale, never dropped.
pub const INBOX_EXPIRY_DAYS: i64 = 14;
