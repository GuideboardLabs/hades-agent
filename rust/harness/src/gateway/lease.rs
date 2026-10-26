//! One gateway owns a state directory's background work.
//!
//! A scheduler and a heartbeat both scan the state directory globally: every due job, every open goal.
//! Two processes doing that against one directory run the same job twice and stamp each other's work,
//! because the stamp is a read followed by a write. A second surface therefore has to be told, not
//! assumed, and the record is a lease beside the offset.
//!
//! The lease is advisory: it stops a second gateway from starting by accident, and it cannot stop one
//! that ignores the file. That is enough for the failure this exists for (a stale unit, a second
//! terminal, a connector started beside `hades gateway`).

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

#[derive(Debug)]
pub struct Lease {
    path: PathBuf,
    token: String,
}

fn now() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_nanos())
        .unwrap_or(0)
}

impl Lease {
    /// Take the directory, or say who has it. A record naming this same process is this process's own
    /// earlier runtime (a test, or a `--once` run followed by a real one), and is replaced.
    pub fn take(state: &Path, surface: &str) -> Result<Lease, String> {
        let path = state.join("gateway").join("owner.json");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
        }
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(value) = serde_json::from_str::<Value>(&text)
        {
            let pid = value.get("pid").and_then(Value::as_u64).unwrap_or(0);
            let owner = value
                .get("surface")
                .and_then(Value::as_str)
                .unwrap_or("another surface")
                .to_string();
            if pid != 0
                && pid != u64::from(std::process::id())
                && Path::new(&format!("/proc/{pid}")).exists()
            {
                return Err(format!(
                    "{}: gateway pid {pid} ({owner}) already owns this state directory; stop it first, or give this surface its own `--state`",
                    path.display()
                ));
            }
        }
        let token = format!("{}-{}", std::process::id(), now());
        let record = json!({
            "pid": std::process::id(),
            "token": token,
            "surface": surface,
            "started": now(),
        });
        std::fs::write(&path, record.to_string())
            .map_err(|error| format!("{}: {error}", path.display()))?;
        Ok(Lease { path, token })
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        // Remove the record only when it is still ours: a lease taken over by the next process must not be
        // deleted by this one leaving.
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return;
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            return;
        };
        if value.get("token").and_then(Value::as_str) == Some(self.token.as_str()) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hades-lease-{name}-{}", now()));
        std::fs::create_dir_all(dir.join("gateway")).expect("a state dir");
        dir
    }

    #[test]
    fn a_second_runtime_is_refused_by_a_live_pid() {
        let dir = state("refused");
        let _held = Lease::take(&dir, "telegram").expect("the first lease");
        // The record names this process, which is treated as ours; a foreign live pid is what refuses.
        let record = json!({"pid": 1, "token": "other", "surface": "discord"});
        std::fs::write(dir.join("gateway").join("owner.json"), record.to_string()).expect("a record");
        let refused = Lease::take(&dir, "telegram").expect_err("a live owner");
        assert!(refused.contains("already owns this state"), "{refused}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dead_owner_is_replaced_and_a_released_lease_is_removed() {
        let dir = state("replaced");
        let record = json!({"pid": 999_999_999_u64, "token": "gone", "surface": "telegram"});
        std::fs::write(dir.join("gateway").join("owner.json"), record.to_string()).expect("a record");
        let held = Lease::take(&dir, "discord").expect("a dead owner is not an owner");
        assert!(dir.join("gateway").join("owner.json").exists());
        drop(held);
        assert!(!dir.join("gateway").join("owner.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
