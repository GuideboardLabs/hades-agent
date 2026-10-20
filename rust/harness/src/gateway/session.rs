//! One lock per session log, for the whole gateway.
//!
//! The session tree numbers an entry from the handle's own length, so two handles that load the same
//! log at the same moment compute the same next id. Two appends that both write entry `n` produce a log
//! the store refuses to load — permanently. A per-writer lock is therefore not a nicety: the load and
//! the append have to be one step.
//!
//! This module is that one step, and it is deliberately the only one. The child registry and the goal
//! store used to keep a private map each; two mutexes over one file is the same as none, because a
//! heartbeat continuation writes both a child event and a goal event. There is one table now, keyed on
//! the state directory as well as the session, so two gateways with different state directories never
//! wait on each other.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use session_tree::{FileStore, Kind, Session, SessionId, SystemClock};

/// A loaded session log. The type is named here so callers do not each spell out the store and clock.
pub type Log = Session<FileStore, SystemClock>;

type Locks = Mutex<BTreeMap<(PathBuf, String), Arc<Mutex<()>>>>;

fn table() -> &'static Locks {
    static LOCKS: std::sync::OnceLock<Locks> = std::sync::OnceLock::new();
    LOCKS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// The one lock for a session log. Every writer takes this before it loads the log, and holds it until
/// the append has returned.
pub fn lock(state: &Path, session: &str) -> Arc<Mutex<()>> {
    let mut locks = table()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    Arc::clone(
        locks
            .entry((state.to_path_buf(), session.to_string()))
            .or_default(),
    )
}

/// Open a session log, creating it when the file does not exist. A corrupt log is an error, not a
/// reason to overwrite it: append-only history is never clobbered.
pub fn open(state: &Path, session: &str) -> Result<Log, String> {
    let store = FileStore::new(state.join("sessions"));
    match Session::load(store.clone(), SystemClock, SessionId::new(session)) {
        Ok(log) => Ok(log),
        // A missing log is a new session. Any other fault is real and must not be papered over.
        Err(session_tree::Fault::Io { .. }) => {
            Session::create(store, SystemClock, SessionId::new(session))
                .map_err(|fault| fault.to_string())
        }
        Err(fault) => Err(fault.to_string()),
    }
}

/// Append one Meta entry under the session's lock. The payload carries its own `type` discriminator;
/// a reader that does not know the type ignores the entry, and the schema version does not move.
pub fn append_meta(state: &Path, session: &str, payload: Value) -> Result<(), String> {
    let lock = lock(state, session);
    let _held = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut log = open(state, session)?;
    log.append(Kind::Meta, payload)
        .map(|_| ())
        .map_err(|fault| fault.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn scratch(name: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or(0);
        let base = std::env::temp_dir().join(format!(
            "hades-session-lock-{name}-{}-{stamp}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("scratch");
        base
    }

    /// Two writers that load the same log and append under different mutexes compute the same next id
    /// and produce a log the store will not load again. One table, so the second waits.
    #[test]
    fn concurrent_appends_from_two_paths_leave_one_loadable_log() {
        let state = scratch("two-paths");
        let writers = 8;
        let each = 4;
        let mut threads = Vec::new();
        for index in 0..writers {
            let state = state.clone();
            threads.push(std::thread::spawn(move || {
                for turn in 0..each {
                    append_meta(
                        &state,
                        "shared-session",
                        json!({"type": if index % 2 == 0 { "child_result" } else { "goal" }, "n": turn}),
                    )
                    .expect("an append");
                }
            }));
        }
        for thread in threads {
            thread.join().expect("a writer");
        }

        let log = Session::load(
            FileStore::new(state.join("sessions")),
            SystemClock,
            SessionId::new("shared-session"),
        )
        .expect("one loadable log");
        // The create header is one entry, then one per append.
        assert_eq!(log.entries().len(), writers * each + 1, "no entry was lost");
        let ids: Vec<u64> = log.entries().iter().map(|entry| entry.id.0).collect();
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), ids.len(), "every id is unique: {ids:?}");
        assert_eq!(unique, (0..=ids.len() as u64 - 1).collect::<Vec<_>>());
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn one_state_directory_has_one_lock_and_another_has_its_own() {
        let first = Path::new("/state/a");
        let second = Path::new("/state/b");
        assert!(Arc::ptr_eq(&lock(first, "tg-7"), &lock(first, "tg-7")));
        assert!(!Arc::ptr_eq(&lock(first, "tg-7"), &lock(first, "tg-8")));
        assert!(!Arc::ptr_eq(&lock(first, "tg-7"), &lock(second, "tg-7")));
    }
}
