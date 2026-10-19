//! The behaviours: append, walk the chain, fork, and compact as a view rather than a deletion.

use serde_json::{Value, json};

use crate::seams::{Clock, SessionStore};
use crate::types::{Entry, EntryId, Fault, Kind, SessionId};

/// One session, loaded. The store and the clock are injected at construction and never reached for
/// globally.
#[derive(Debug, Clone)]
pub struct Session<S: SessionStore + Clone, C: Clock + Clone> {
    id: SessionId,
    store: S,
    clock: C,
    entries: Vec<Entry>,
}

impl<S: SessionStore + Clone, C: Clock + Clone> Session<S, C> {
    /// Start a session with its meta root.
    pub fn create(store: S, clock: C, id: SessionId) -> Result<Self, Fault> {
        let root = Entry {
            id: EntryId(0),
            parent: None,
            kind: Kind::Meta,
            payload: json!({ "session_id": id.as_str() }),
            at: clock.now(),
        };
        store.create(&id, std::slice::from_ref(&root))?;
        Ok(Session {
            id,
            store,
            clock,
            entries: vec![root],
        })
    }

    /// Read an existing session back.
    pub fn load(store: S, clock: C, id: SessionId) -> Result<Self, Fault> {
        let entries = store.load(&id)?;
        Ok(Session {
            id,
            store,
            clock,
            entries,
        })
    }

    /// Build a session from an existing chain, writing it once, in order. This is what a fork uses:
    /// a named constructor rather than a private rewrite of someone else's file.
    pub fn from_chain(store: S, clock: C, id: SessionId, chain: &[Entry]) -> Result<Self, Fault> {
        store.create(&id, chain)?;
        Ok(Session {
            id,
            store,
            clock,
            entries: chain.to_vec(),
        })
    }

    pub fn id(&self) -> &SessionId {
        &self.id
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The last entry's id, or `None` for an empty log.
    pub fn head(&self) -> Option<EntryId> {
        self.entries.last().map(|entry| entry.id)
    }

    pub fn get(&self, id: EntryId) -> Result<&Entry, Fault> {
        self.entries
            .iter()
            .find(|entry| entry.id == id)
            .ok_or(Fault::UnknownEntry {
                session: self.id.0.clone(),
                id,
            })
    }

    /// The chain from the root to `at`, inclusive. `None` means the head, so resume is a read.
    pub fn path_to(&self, at: Option<EntryId>) -> Result<Vec<&Entry>, Fault> {
        let mut cursor = match at {
            Some(id) => Some(self.get(id)?),
            None => self.entries.last(),
        };
        let mut chain = Vec::new();
        while let Some(entry) = cursor {
            chain.push(entry);
            cursor = match entry.parent {
                Some(parent) => Some(self.get(parent)?),
                None => None,
            };
        }
        chain.reverse();
        Ok(chain)
    }

    /// What a reader should see: the latest compaction and everything after it, or the whole chain.
    /// The log keeps every entry, so this is a view and never a deletion.
    /// The entries a reader is shown after a cut.
    ///
    /// A compaction starts the view at the cut, with one exception: a `Plan` entry survives every cut. The
    /// plan is a standing instruction for the whole session, not history the run happened to have, and a
    /// model that cannot see the plan it was asked to write is a model that plans again or acts without one.
    /// The log still holds every entry; this changes what is read, never what is kept.
    pub fn view(&self) -> Vec<&Entry> {
        match self
            .entries
            .iter()
            .rposition(|entry| entry.kind == Kind::Compaction)
        {
            Some(index) => self
                .entries
                .iter()
                .enumerate()
                .filter(|(position, entry)| *position >= index || entry.kind == Kind::Plan)
                .map(|(_, entry)| entry)
                .collect(),
            None => self.entries.iter().collect(),
        }
    }

    /// Append one entry, linked to the current head.
    pub fn append(&mut self, kind: Kind, payload: Value) -> Result<EntryId, Fault> {
        let entry = Entry {
            id: EntryId(self.entries.len() as u64),
            parent: self.head(),
            kind,
            payload,
            at: self.clock.now(),
        };
        self.store.append(&self.id, &entry)?;
        let id = entry.id;
        self.entries.push(entry);
        Ok(id)
    }

    /// Record that everything up to `at` is covered by a summary. The covered entries stay.
    pub fn compact(&mut self, at: Option<EntryId>, summary: &str) -> Result<EntryId, Fault> {
        let at = match at {
            Some(id) => {
                self.get(id)?;
                id
            }
            None => self.head().ok_or(Fault::UnknownEntry {
                session: self.id.0.clone(),
                id: EntryId(0),
            })?,
        };
        let first = self
            .entries
            .first()
            .map(|entry| entry.id)
            .unwrap_or(EntryId(0));
        self.append(
            Kind::Compaction,
            json!({ "covers": [first.0, at.0], "summary": summary }),
        )
    }

    /// A fork id no session is using. Deterministic and readable, and never a clobber: the first
    /// candidate is the plain name, and each collision adds a suffix.
    fn fresh_fork_id(&self, at: EntryId) -> SessionId {
        let base = format!("{}-fork-{}", self.id.as_str(), at.0);
        let mut candidate = SessionId::new(base.clone());
        let mut suffix = 2;
        while self.store.exists(&candidate) {
            candidate = SessionId::new(format!("{base}-{suffix}"));
            suffix += 1;
        }
        candidate
    }

    /// A new session carrying the chain up to `at` verbatim, reusing ids, plus a meta entry that
    /// says where it came from. The source is never touched.
    pub fn fork(&self, at: Option<EntryId>) -> Result<Session<S, C>, Fault> {
        let source_head = self.head();
        let fork_at = match at {
            Some(id) => {
                self.get(id)?;
                id
            }
            None => source_head.ok_or(Fault::UnknownEntry {
                session: self.id.0.clone(),
                id: EntryId(0),
            })?,
        };
        let mut chain: Vec<Entry> = self.path_to(Some(fork_at))?.into_iter().cloned().collect();
        let new_id = self.fresh_fork_id(fork_at);
        let meta = Entry {
            id: EntryId(0),
            parent: None,
            kind: Kind::Meta,
            payload: json!({
                "session_id": new_id.as_str(),
                "forked_from": self.id.as_str(),
                "forked_at": fork_at.0,
                "source_head": source_head.map(|id| id.0),
            }),
            at: self.clock.now(),
        };
        if let Some(root) = chain.first_mut() {
            root.payload = meta.payload.clone();
        }
        Session::from_chain(self.store.clone(), self.clock.clone(), new_id, &chain)
    }
}
