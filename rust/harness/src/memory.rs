//! The Memory seam: what a turn is told before it acts, and what it files after.
//!
//! Lachesis answers one question, `recall`: which notes fit the goal and the budget. Clotho answers the
//! other, `record`: the durable note an accepted outcome leaves behind. The seam is a trait because a
//! turn must be runnable without a vault (a machine with no `hades init` yet), and because an oracle
//! that needs a file store to test the turn is a turn testing the store.
//!
//! Nothing here knows SQL: `SqliteStore::search` owns the query and `Curator::file` owns the rules a
//! durable note must pass. This module decides what a hit is worth in a prompt and what an outcome is.

use std::path::{Path, PathBuf};

use governance::{Curation, Curator, SystemTimestamp};
use mneme::{Area, Field, NoteStore, SqliteStore, Vault};
use mneme::{Clock, relative_posix, slugify};

use crate::context::estimate;

/// The most terms a goal contributes. A long goal is a paragraph, not a query.
const MAX_TERMS: usize = 12;

/// Words a goal shares with every note. Without this list, three characters is enough to make `the`
/// a search term and every note a hit; relevance that returns everything is not relevance.
const STOP_WORDS: &[&str] = &[
    "the", "and", "for", "with", "that", "this", "from", "into", "was", "were", "are", "but",
    "not", "you", "your", "its", "about", "where", "when", "what", "how", "why", "who", "will",
    "would", "can", "could", "should", "have", "has", "had", "they", "them", "then", "than",
    "there", "here", "all", "any", "some", "one", "two", "out", "use",
];

/// A note the recall kept, with what a prompt may cite.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub path: PathBuf,
    pub title: String,
    pub summary: Option<String>,
    pub provenance: Option<String>,
    pub topics: Vec<String>,
    /// The note that replaced this one. A hit carrying this is history: recall still shows it, after
    /// every current note, because a correction that cannot say what it corrected is a deletion.
    pub superseded_by: Option<String>,
}

impl Hit {
    /// One line, and the exact text the budget is charged for.
    pub fn line(&self) -> String {
        let mut line = format!("- {} ({}): ", self.path.display(), self.title);
        match &self.summary {
            Some(summary) if !summary.trim().is_empty() => line.push_str(summary.trim()),
            _ => line.push_str("(no summary)"),
        }
        if let Some(provenance) = &self.provenance
            && !provenance.trim().is_empty()
        {
            line.push_str(&format!(" [from: {}]", provenance.trim()));
        }
        if let Some(replaced_by) = &self.superseded_by
            && !replaced_by.trim().is_empty()
        {
            line.push_str(&format!(" [history, replaced by {}]", replaced_by.trim()));
        }
        line.push('\n');
        line
    }
}

/// Why a recall is shorter than a caller might expect. An empty answer is a fact, not a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecallReason {
    NoMatch,
    BudgetTooSmall,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Recall {
    pub hits: Vec<Hit>,
    pub reason: Option<RecallReason>,
    /// The estimate of `text`, which is the sum of the kept lines.
    pub tokens: usize,
    /// The section body: the kept lines, or one sentence saying why there are none.
    pub text: String,
}

impl Recall {
    pub fn is_empty(&self) -> bool {
        self.hits.is_empty()
    }
}

/// What an accepted run established, in the fields a durable note needs.
#[derive(Debug, Clone, PartialEq)]
pub struct OutcomeNote {
    pub goal: String,
    pub session: String,
    pub run: u64,
    pub attempt: usize,
    /// The note's title, one short line.
    pub title: String,
    /// One line. It is the note's summary and the reason the note exists.
    pub summary: String,
    pub constraints: Vec<String>,
    pub check: String,
    pub files: Vec<String>,
    pub topics: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Recorded {
    pub path: PathBuf,
}

/// A failure worth remembering: the goal, why it was abandoned, and the evidence a later run would need
/// to tell whether it is still a dead end. A dead end is not an outcome note: nothing was established.
#[derive(Debug, Clone, PartialEq)]
pub struct DeadEnd {
    pub goal: String,
    pub session: String,
    pub cause: String,
    pub attempts: u32,
    pub check_sha256: String,
    pub evidence: String,
    pub case_id: Option<String>,
}

impl DeadEnd {
    /// The identity a duplicate is folded into: the goal and the oracle that judged it. A new check
    /// means new evidence, which is exactly when a dead end is worth retrying.
    pub fn id(&self) -> String {
        let text = format!(
            "{}\0{}",
            self.goal.trim().to_lowercase(),
            self.check_sha256.trim()
        );
        mneme::sha256::hex(text.as_bytes())[..16].to_string()
    }
}

/// A turn's two questions. `recall` is answered before the prompt is built, `record` after the
/// outcome is accepted.
pub trait Memory {
    fn recall(&self, goal: &str, topics: &[String], budget: usize) -> Result<Recall, String>;
    fn record(&self, outcome: &OutcomeNote) -> Result<Recorded, String>;

    /// A failure, kept so the next run does not try the same thing again. The default refuses rather
    /// than silently dropping it: a memory that cannot keep a dead end should say so.
    fn record_dead_end(&self, _dead: &DeadEnd) -> Result<Recorded, String> {
        Err("this memory does not keep dead ends".to_string())
    }
}

/// The implementation over the record. Holds the curator and not just the vault, because a note that
/// is written without passing the curator's rules is not a memory, it is a file.
pub struct MnemeMemory {
    curator: Curator<SqliteStore, mneme::SystemClock, SystemTimestamp>,
    /// One append-only file per profile, beside the record rather than in it: a dead end is not a vault
    /// note, and filing one as research would put it on the knowledge graph as if it were a finding.
    dead_ends: PathBuf,
}

impl MnemeMemory {
    pub fn open(root: impl AsRef<Path>, db: impl AsRef<Path>) -> Result<Self, String> {
        let root = root.as_ref().to_path_buf();
        let dead_ends = db
            .as_ref()
            .parent()
            .map(|parent| parent.join("dead-ends.jsonl"))
            .unwrap_or_else(|| PathBuf::from("dead-ends.jsonl"));
        let store = SqliteStore::open(db, &root).map_err(|fault| fault.to_string())?;
        let vault = Vault::new(root, store, mneme::SystemClock);
        Ok(MnemeMemory {
            curator: Curator::new(vault, SystemTimestamp),
            dead_ends,
        })
    }

    pub fn dead_ends(&self) -> &Path {
        &self.dead_ends
    }

    pub fn vault(&self) -> &Vault<SqliteStore, mneme::SystemClock> {
        self.curator.vault()
    }

    /// Capture a note, and when it corrects another, retire that one instead of leaving two truths in
    /// the record. The old note is marked `superseded_by` and the new one `supersedes`, both as vault
    /// relative paths, so either direction can be read from the files alone.
    ///
    /// The old path is checked before the new note is written: a correction that names a note which is
    /// not there leaves nothing behind to explain itself.
    pub fn remember(
        &self,
        text: &str,
        title: Option<&str>,
        supersedes: Option<&str>,
    ) -> Result<PathBuf, String> {
        let vault = self.vault();
        let old = match supersedes {
            Some(given) => {
                let path = inside(vault.root(), given)?;
                if !vault.store().exists(&path) {
                    return Err(format!("there is no note at {given}"));
                }
                Some(path)
            }
            None => None,
        };
        let new = vault
            .capture(text, title, None)
            .map_err(|fault| fault.to_string())?;
        let Some(old) = old else {
            return Ok(new);
        };
        let new_name = relative_posix(&new, vault.root());
        let old_name = relative_posix(&old, vault.root());

        let mut note = vault.read_note(&old).map_err(|fault| fault.to_string())?;
        note.frontmatter
            .extra
            .insert("superseded_by".to_string(), Field::Scalar(new_name));
        vault
            .write_note(&old, &note.frontmatter, &note.body)
            .map_err(|fault| fault.to_string())?;

        let mut fresh = vault.read_note(&new).map_err(|fault| fault.to_string())?;
        fresh
            .frontmatter
            .extra
            .insert("supersedes".to_string(), Field::Scalar(old_name));
        vault
            .write_note(&new, &fresh.frontmatter, &fresh.body)
            .map_err(|fault| fault.to_string())?;
        Ok(new)
    }
}

/// A path the model named, resolved under the vault root. Absolute paths and `..` are refused: a note the
/// model can point at is still a note inside the record.
fn inside(root: &Path, given: &str) -> Result<PathBuf, String> {
    let path = Path::new(given.trim());
    let climbs = path
        .components()
        .any(|part| matches!(part, std::path::Component::ParentDir));
    if given.trim().is_empty() || path.is_absolute() || climbs {
        return Err(format!("`{given}` is not a path inside the vault"));
    }
    Ok(root.join(path))
}

impl Memory for MnemeMemory {
    fn recall(&self, goal: &str, topics: &[String], budget: usize) -> Result<Recall, String> {
        let terms = terms(goal);
        // Dead ends are read first: a goal that already failed is the most useful thing a prompt can be
        // told, and it must survive a vault with no matching note.
        let dead = dead_end_text(&fold_dead_ends(&read_dead_ends(&self.dead_ends)), goal);
        let topics: Vec<String> = topics
            .iter()
            .map(|topic| topic.trim().to_lowercase())
            .filter(|topic| !topic.is_empty())
            .collect();
        let hits = self
            .vault()
            .store()
            .search(&terms, &topics)
            .map_err(|fault| fault.to_string())?;

        if hits.is_empty() {
            if !dead.is_empty() && estimate(&dead) <= budget {
                return Ok(Recall {
                    hits: Vec::new(),
                    reason: None,
                    tokens: estimate(&dead),
                    text: dead,
                });
            }
            return Ok(Recall {
                hits: Vec::new(),
                reason: Some(RecallReason::NoMatch),
                tokens: 0,
                text: "Nothing in the record matches this goal.".to_string(),
            });
        }

        let root = self.vault().root();
        let mut current: Vec<Hit> = Vec::new();
        let mut history: Vec<Hit> = Vec::new();
        for found in hits {
            let hit = Hit {
                path: PathBuf::from(relative_posix(&found.path, root)),
                title: found.title,
                summary: found.summary,
                provenance: found.provenance,
                topics: found.topics,
                superseded_by: found.superseded_by,
            };
            if hit.superseded_by.is_some() {
                history.push(hit);
            } else {
                current.push(hit);
            }
        }
        let mut kept = Vec::new();
        let mut text = String::new();
        let mut tokens = 0;
        // Current truth first, then history. A correction and the note it replaced usually share the
        // words that made both hits, so score alone can put the older, more specific one first; the
        // budget is spent on what is true now before it is spent on what used to be.
        for hit in current.into_iter().chain(history) {
            let line = hit.line();
            let cost = estimate(&line);
            if tokens + cost > budget {
                break;
            }
            tokens += cost;
            text.push_str(&line);
            kept.push(hit);
        }

        if kept.is_empty() {
            if !dead.is_empty() && estimate(&dead) <= budget {
                return Ok(Recall {
                    hits: Vec::new(),
                    reason: None,
                    tokens: estimate(&dead),
                    text: dead,
                });
            }
            return Ok(Recall {
                hits: Vec::new(),
                reason: Some(RecallReason::BudgetTooSmall),
                tokens: 0,
                text: "The memory budget is too small for one note.".to_string(),
            });
        }

        let dead_cost = estimate(&dead);
        if !dead.is_empty() && tokens + dead_cost <= budget {
            text.push('\n');
            text.push_str(&dead);
            tokens += dead_cost;
        }

        Ok(Recall {
            hits: kept,
            reason: None,
            tokens,
            text,
        })
    }

    fn record(&self, outcome: &OutcomeNote) -> Result<Recorded, String> {
        let day = self.vault().clock().today().to_iso();
        let session = match slugify(&outcome.session, 4) {
            name if name.is_empty() => "session".to_string(),
            name => name,
        };
        // A run number is not unique by itself: a resumed session accepts more than one turn on the same
        // day, and the caller's run is where the search starts, not a name to trust. The first free
        // `-rN` wins, so a second accepted turn files r2 instead of being thrown away as a repeat.
        let folder = self.vault().area_path(Area::Research);
        let mut run = outcome.run.max(1);
        let slug = loop {
            let candidate = format!("{day}-{session}-r{run}");
            if !self
                .vault()
                .store()
                .exists(&folder.join(format!("{candidate}.md")))
            {
                break candidate;
            }
            if run >= outcome.run.max(1) + 100 {
                return Err(format!(
                    "{day}-{session}: 100 run numbers are taken; the record is not being written"
                ));
            }
            run += 1;
        };
        let body = render_outcome(outcome);
        let curation = Curation {
            topics: outcome.topics.clone(),
            projects: Vec::new(),
            research: Vec::new(),
            from_project: None,
            summary: outcome.summary.clone(),
            provenance: format!(
                "session {} run {} attempt {}",
                outcome.session, run, outcome.attempt
            ),
        };
        let filed = self
            .curator
            .record(Area::Research, &outcome.title, &slug, &body, &curation)
            .map_err(|refusal| refusal.to_string())?;
        Ok(Recorded { path: filed })
    }

    fn record_dead_end(&self, dead: &DeadEnd) -> Result<Recorded, String> {
        use std::io::Write as _;
        if let Some(parent) = self.dead_ends.parent() {
            crate::scratch::state_dir(parent)
                .map_err(|error| format!("could not make {}: {error}", parent.display()))?;
        }
        let id = dead.id();
        let events = read_dead_ends(&self.dead_ends);
        let active = fold_dead_ends(&events);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_secs())
            .unwrap_or(0);
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.dead_ends)
            .map_err(|error| format!("could not open {}: {error}", self.dead_ends.display()))?;
        crate::scratch::private_file(&self.dead_ends)
            .map_err(|error| format!("could not restrict {}: {error}", self.dead_ends.display()))?;
        // Seen again is evidence about the same dead end, not a second one. Appending the fact keeps the
        // count without rewriting the record that explains it.
        if active.iter().any(|entry| entry.id == id) {
            writeln!(
                file,
                "{}",
                serde_json::json!({ "kind": "seen", "id": id, "at": now, "session": dead.session })
            )
            .map_err(|error| {
                format!("could not append to {}: {error}", self.dead_ends.display())
            })?;
            return Ok(Recorded {
                path: self.dead_ends.clone(),
            });
        }
        // The cap retires the oldest active entry with a record, never by deletion: an eviction that
        // erases the evidence turns a memory into a rumour.
        if active.len() >= DEAD_END_CAP
            && let Some(oldest) = active.first()
        {
            writeln!(
                file,
                "{}",
                serde_json::json!({ "kind": "retire", "id": oldest.id, "at": now })
            )
            .map_err(|error| format!("could not retire: {error}"))?;
        }
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "kind": "dead_end",
                "id": id,
                "goal": dead.goal,
                "session": dead.session,
                "cause": dead.cause,
                "attempts": dead.attempts,
                "check_sha256": dead.check_sha256,
                "evidence": dead.evidence,
                "case_id": dead.case_id,
                "at": now,
            })
        )
        .map_err(|error| format!("could not append to {}: {error}", self.dead_ends.display()))?;
        Ok(Recorded {
            path: self.dead_ends.clone(),
        })
    }
}

/// How many dead ends one profile keeps before the oldest is retired.
pub const DEAD_END_CAP: usize = 200;

/// One live dead-end entry, as recall reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeadEntry {
    pub id: String,
    pub goal: String,
    pub cause: String,
    pub attempts: u32,
    pub evidence: String,
    pub seen: usize,
    pub path: PathBuf,
}

pub fn read_dead_ends(path: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// Fold the append-only events into the live set. `seen` counts repeats, a `retire` removes an entry,
/// and the order is insertion order so "oldest" means what it says.
pub fn fold_dead_ends(events: &[serde_json::Value]) -> Vec<DeadEntry> {
    let mut live: Vec<DeadEntry> = Vec::new();
    for event in events {
        let kind = event
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let id = event
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        match kind {
            "dead_end" => live.push(DeadEntry {
                id: id.to_string(),
                goal: event
                    .get("goal")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                cause: event
                    .get("cause")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                attempts: event
                    .get("attempts")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0) as u32,
                evidence: event
                    .get("evidence")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                seen: 1,
                path: PathBuf::new(),
            }),
            "seen" => {
                if let Some(entry) = live.iter_mut().find(|entry| entry.id == id) {
                    entry.seen += 1;
                }
            }
            "retire" => live.retain(|entry| entry.id != id),
            _ => {}
        }
    }
    live
}

/// The matching dead ends, newest last, as the lines a prompt is given. Empty when none match.
pub fn dead_end_text(entries: &[DeadEntry], goal: &str) -> String {
    let wanted = terms(goal);
    let matches: Vec<&DeadEntry> = entries
        .iter()
        .filter(|entry| {
            let have = terms(&entry.goal);
            wanted.iter().any(|term| have.contains(term))
        })
        .collect();
    if matches.is_empty() {
        return String::new();
    }
    let mut text = String::from("Tried and failed; do not retry unless the evidence changed:\n");
    for entry in matches {
        text.push_str(&format!(
            "- {} ({} after {} attempt(s), seen {}x): {}\n",
            entry.goal,
            entry.cause,
            entry.attempts,
            entry.seen,
            entry.evidence.replace('\n', " ")
        ));
    }
    text
}

/// A goal as terms: alphanumeric runs of three or more characters, lowercased, deduplicated, capped.
/// This is the only place user text becomes a query, and it never produces an operator.
pub fn terms(goal: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for word in goal.split(|character: char| !character.is_alphanumeric()) {
        let word = word.to_lowercase();
        if word.chars().count() < 3 || STOP_WORDS.contains(&word.as_str()) || out.contains(&word) {
            continue;
        }
        out.push(word);
        if out.len() == MAX_TERMS {
            break;
        }
    }
    out
}

/// The note's body, bounded. A durable note is a citation, not a transcript: the session log keeps the
/// transcript, and this keeps what the next run must know.
fn render_outcome(outcome: &OutcomeNote) -> String {
    let mut body = String::new();
    body.push_str(&format!("Goal: {}\n\n", outcome.goal.trim()));
    if !outcome.constraints.is_empty() {
        body.push_str("Accepted:\n");
        for constraint in &outcome.constraints {
            body.push_str(&format!("- {}\n", constraint.trim()));
        }
        body.push('\n');
    }
    if !outcome.check.trim().is_empty() {
        body.push_str(&format!("Check: {}\n", outcome.check.trim()));
    }
    if !outcome.files.is_empty() {
        body.push_str("\nFiles:\n");
        for file in &outcome.files {
            body.push_str(&format!("- {}\n", file.trim()));
        }
    }
    body.push_str(&format!(
        "\nSession {}, run {}, attempt {}.\n",
        outcome.session, outcome.run, outcome.attempt
    ));
    if body.chars().count() > 4_000 {
        body.truncate(4_000);
        body.push_str("\n[truncated at 4000 characters]\n");
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terms_are_lowercase_deduplicated_and_capped() {
        assert_eq!(terms("Fix THE And-Gate, and the gate"), vec!["fix", "gate"]);
        assert_eq!(terms("where is the lantern"), vec!["lantern"]);
        assert!(terms("a b ab !!!").is_empty());
        let many = (0..20)
            .map(|index| format!("word{index}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(terms(&many).len(), MAX_TERMS);
    }

    #[test]
    fn a_hit_line_is_the_charged_text() {
        let hit = Hit {
            path: PathBuf::from("20 Research/x.md"),
            title: "X".to_string(),
            summary: Some("does a thing".to_string()),
            provenance: Some("run 1".to_string()),
            topics: vec![],
            superseded_by: None,
        };
        let line = hit.line();
        assert_eq!(line, "- 20 Research/x.md (X): does a thing [from: run 1]\n");
        assert_eq!(estimate(&line), line.chars().count().div_ceil(4));
    }

    #[test]
    fn a_history_line_says_what_replaced_it() {
        let hit = Hit {
            path: PathBuf::from("00 Inbox/x.md"),
            title: "X".to_string(),
            summary: Some("the port is 8101".to_string()),
            provenance: None,
            topics: vec![],
            superseded_by: Some("00 Inbox/y.md".to_string()),
        };
        assert_eq!(
            hit.line(),
            "- 00 Inbox/x.md (X): the port is 8101 [history, replaced by 00 Inbox/y.md]\n"
        );
    }

    #[test]
    fn a_path_outside_the_vault_is_refused() {
        let root = Path::new("/vault");
        assert!(inside(root, "00 Inbox/x.md").is_ok());
        assert!(inside(root, "/etc/passwd").is_err());
        assert!(inside(root, "../x.md").is_err());
        assert!(inside(root, "00 Inbox/../../x.md").is_err());
        assert!(inside(root, "   ").is_err());
    }

    #[test]
    fn a_correction_retires_the_note_it_replaces() {
        let dir = crate::scratch::root().join(format!("memory-supersede-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch vault");
        // Canonicalized: the store compares the note's parent to the root, and a `..` in the root makes
        // the same folder compare unequal, which would send the note to a plain file instead of a row.
        let dir = std::fs::canonicalize(&dir).expect("a real path");
        let memory = MnemeMemory::open(&dir, dir.join("mneme.db")).expect("a store");
        let old = memory
            .vault()
            .capture("the port is 8101", Some("Port"), None)
            .expect("an old note");
        let old_name = relative_posix(&old, memory.vault().root());
        let fresh = memory
            .remember("the port is 8102", Some("Port, corrected"), Some(&old_name))
            .expect("a correction");
        let fresh_name = relative_posix(&fresh, memory.vault().root());

        let recall = memory.recall("port", &[], 10_000).expect("a recall");
        assert_eq!(recall.hits.len(), 2, "{}", recall.text);
        assert_eq!(recall.hits[0].path, PathBuf::from(&fresh_name));
        assert_eq!(recall.hits[1].path, PathBuf::from(&old_name));
        assert_eq!(
            recall.hits[1].superseded_by.as_deref(),
            Some(fresh_name.as_str())
        );
        assert_eq!(recall.hits[0].superseded_by, None);
        assert!(
            recall
                .text
                .lines()
                .last()
                .unwrap()
                .contains("history, replaced by"),
            "{}",
            recall.text
        );

        // The budget is spent on what is true now: room for one line keeps the correction, not history.
        let one = estimate(&recall.hits[0].line());
        let tight = memory.recall("port", &[], one).expect("a tight recall");
        assert_eq!(tight.hits.len(), 1, "{}", tight.text);
        assert_eq!(tight.hits[0].path, PathBuf::from(&fresh_name));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
