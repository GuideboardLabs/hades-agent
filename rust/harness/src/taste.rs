//! The taste ledger: what a person actually picked, kept as one append-only line per answer.
//!
//! This is the smallest honest form of learning a preference. It does not infer a personality and it
//! never writes a rule on its own. Three things are read back from it:
//!
//! - a different option chosen repeatedly in one root can become the recommendation;
//! - five consecutive accepts of a choice's recommendation can stop that question being asked;
//! - two `Other` declines of the same family are offered to the person as a proposed `never` rule.
//!
//! Permissions are recorded but never auto-answered, and a preference from one root never applies to
//! another. Everything is visible with `hades taste`, and removable line by line.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::approval::Answer;
use crate::fault::Fault;
use crate::question::{Ask, AskKind};

/// One answered question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub at: String,
    pub root: String,
    pub kind: String,
    pub action: String,
    pub question: String,
    pub chosen: String,
    pub recommended: Option<String>,
    pub custom: Option<String>,
    pub option_keys: Vec<String>,
}

impl Entry {
    fn json(&self) -> Value {
        json!({
            "at": self.at,
            "root": self.root,
            "kind": self.kind,
            "action": self.action,
            "question": self.question,
            "chosen": self.chosen,
            "recommended": self.recommended,
            "custom": self.custom,
            "option_keys": self.option_keys,
        })
    }

    fn parse(value: &Value) -> Result<Entry, String> {
        let text = |key: &str| -> Result<String, String> {
            value[key]
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("entry.{key} is not a string"))
        };
        Ok(Entry {
            at: text("at")?,
            root: text("root")?,
            kind: text("kind")?,
            action: text("action")?,
            question: text("question")?,
            chosen: text("chosen")?,
            recommended: value["recommended"].as_str().map(str::to_string),
            custom: value["custom"].as_str().map(str::to_string),
            option_keys: value["option_keys"]
                .as_array()
                .map(|keys| {
                    keys.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
        })
    }
}

/// What the ledger currently says about one family in one root.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Learned {
    /// A key the person has chosen often enough to become the recommendation.
    pub recommendation: Option<String>,
    /// How many answers in a row agreed with the recommendation.
    pub streak: u32,
    /// Five agreeing answers on a choice: stop asking and take the recommendation.
    pub auto: bool,
}

pub fn path(state: &Path) -> PathBuf {
    state.join("taste.jsonl")
}

pub fn read(state: &Path) -> Result<Vec<Entry>, Fault> {
    let path = path(state);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(Fault::harness(format!("{}: {error}", path.display())));
        }
    };
    let mut entries = Vec::new();
    for (number, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line).map_err(|error| {
            Fault::harness(format!("{} line {}: {error}", path.display(), number + 1))
        })?;
        let entry = Entry::parse(&value).map_err(|error| {
            Fault::harness(format!("{} line {}: {error}", path.display(), number + 1))
        })?;
        entries.push(entry);
    }
    Ok(entries)
}

/// Append the answer to a question. A permission is recorded for the audit trail; a choice is what the
/// folds read.
pub fn record(state: &Path, root: &Path, ask: &Ask, answer: &Answer) -> Result<(), Fault> {
    let Some(chosen) = answer.option.clone() else {
        return Ok(());
    };
    let entry = Entry {
        at: answer.at.clone(),
        root: root.to_string_lossy().to_string(),
        kind: ask.family(),
        action: match ask.kind {
            AskKind::Permission => "permission",
            AskKind::Choice => "choice",
        }
        .to_string(),
        question: ask.prompt.clone(),
        chosen,
        recommended: ask.recommended().map(|choice| choice.key.clone()),
        custom: answer.note.clone(),
        option_keys: ask
            .options
            .iter()
            .map(|choice| choice.key.clone())
            .collect(),
    };
    let path = path(state);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| Fault::harness(format!("{}: {error}", parent.display())))?;
    }
    let line = entry.json().to_string();
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|error| Fault::harness(format!("{}: {error}", path.display())))?;
    writeln!(file, "{line}").map_err(|error| Fault::harness(format!("{}: {error}", path.display())))
}

/// Fold the last ten answers for one family in one root.
pub fn learn(entries: &[Entry], root: &str, family: &str) -> Learned {
    let mine: Vec<&Entry> = entries
        .iter()
        .filter(|entry| entry.root == root && entry.kind == family)
        .collect();
    let recent = &mine[mine.len().saturating_sub(10)..];
    if recent.is_empty() {
        return Learned::default();
    }
    let current = recent.last().and_then(|entry| entry.recommended.clone());
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for entry in recent {
        *counts.entry(entry.chosen.as_str()).or_default() += 1;
    }
    let mut learned = Learned::default();
    if let Some((best, count)) = counts.iter().max_by_key(|(_, count)| **count) {
        let current_count = current
            .as_deref()
            .and_then(|key| counts.get(key).copied())
            .unwrap_or(0);
        if *count >= 3 && *count >= current_count + 3 && Some(*best) != current.as_deref() {
            learned.recommendation = Some((*best).to_string());
        }
    }
    let mut streak = 0;
    for entry in recent.iter().rev() {
        if entry.recommended.as_deref() == Some(entry.chosen.as_str()) {
            streak += 1;
        } else {
            break;
        }
    }
    learned.streak = streak;
    if streak >= 5 && recent.iter().all(|entry| entry.action == "choice") {
        learned.auto = true;
        learned.recommendation = current;
    }
    learned
}

/// Every (root, family) the ledger knows, newest last.
pub fn families(entries: &[Entry]) -> Vec<(String, String)> {
    let mut seen = Vec::new();
    for entry in entries {
        let pair = (entry.root.clone(), entry.kind.clone());
        if !seen.contains(&pair) {
            seen.push(pair);
        }
    }
    seen
}

/// Two `Other` declines of the same family are a proposal, never a rule.
pub fn deny_proposals(entries: &[Entry]) -> Vec<(String, String, usize)> {
    let mut counts: BTreeMap<(String, String), usize> = BTreeMap::new();
    for entry in entries {
        if entry.action == "choice" && entry.chosen == "other" {
            *counts
                .entry((entry.root.clone(), entry.kind.clone()))
                .or_default() += 1;
        }
    }
    counts
        .into_iter()
        .filter(|(_, count)| *count >= 2)
        .map(|((root, kind), count)| (root, kind, count))
        .collect()
}

/// Remove one line by its 0-based index, or all of them, rewriting the file atomically.
pub fn forget(state: &Path, index: Option<usize>) -> Result<usize, Fault> {
    let mut entries = read(state)?;
    let removed = match index {
        Some(index) => {
            if index >= entries.len() {
                return Err(Fault::harness(format!(
                    "there is no taste line {index}; {} lines are recorded",
                    entries.len()
                )));
            }
            entries.remove(index);
            1
        }
        None => {
            let count = entries.len();
            entries.clear();
            count
        }
    };
    let path = path(state);
    let text = entries
        .iter()
        .map(|entry| entry.json().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&path, if text.is_empty() { text } else { text + "\n" })
        .map_err(|error| Fault::harness(format!("{}: {error}", path.display())))?;
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::Scope;
    use crate::policy::Decision;

    fn ask() -> Ask {
        let mut ask = Ask::choice(
            "Where should the queue live?",
            "both fit",
            true,
            vec![
                crate::question::Choice::new(
                    "postgres",
                    "Postgres",
                    "a server",
                    crate::question::Effect::Free,
                )
                .recommended(),
                crate::question::Choice::new(
                    "sqlite",
                    "SQLite",
                    "one file",
                    crate::question::Effect::Free,
                ),
            ],
        );
        ask.family = "storage".into();
        ask
    }

    fn answer(chosen: &str, note: Option<&str>) -> Answer {
        Answer {
            decision: Decision::Allow,
            scope: Scope::Once,
            by: "test".into(),
            at: "2026-10-20T12:00:00Z".into(),
            option: Some(chosen.into()),
            note: note.map(str::to_string),
        }
    }

    fn dir(name: &str) -> PathBuf {
        let dir = crate::scratch::root().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        dir
    }

    fn seed(state: &Path, root: &str, chosen: &[&str]) {
        for key in chosen {
            record(state, Path::new(root), &ask(), &answer(key, None)).expect("a recorded answer");
        }
    }

    #[test]
    fn three_consistent_overrides_change_the_recommendation() {
        let dir = dir("taste-three");
        seed(&dir, "/p", &["sqlite", "sqlite", "sqlite"]);
        let learned = learn(&read(&dir).expect("the ledger"), "/p", "storage");
        assert_eq!(learned.recommendation.as_deref(), Some("sqlite"));
        assert!(!learned.auto);
    }

    #[test]
    fn two_overrides_do_not() {
        let dir = dir("taste-two");
        seed(&dir, "/p", &["sqlite", "sqlite"]);
        let learned = learn(&read(&dir).expect("the ledger"), "/p", "storage");
        assert_eq!(learned.recommendation, None);
    }

    #[test]
    fn five_accepts_of_a_choice_stop_asking() {
        let dir = dir("taste-five");
        seed(&dir, "/p", &["postgres"; 5]);
        let learned = learn(&read(&dir).expect("the ledger"), "/p", "storage");
        assert!(
            learned.auto,
            "five accepts convert the choice into an assumption"
        );
        assert_eq!(learned.recommendation.as_deref(), Some("postgres"));
    }

    #[test]
    fn permissions_never_auto_assume() {
        let dir = dir("taste-permission");
        let permission = Ask::permission("Delete build?", "it cannot be undone");
        for _ in 0..5 {
            record(
                &dir,
                Path::new("/p"),
                &permission,
                &answer("allow-once", None),
            )
            .expect("a recorded answer");
        }
        let learned = learn(&read(&dir).expect("the ledger"), "/p", "permission");
        assert!(!learned.auto, "a permission is never silenced");
    }

    #[test]
    fn a_preference_from_one_root_does_not_leak_into_another() {
        let dir = dir("taste-root");
        seed(&dir, "/one", &["sqlite", "sqlite", "sqlite"]);
        let entries = read(&dir).expect("the ledger");
        assert_eq!(
            learn(&entries, "/one", "storage").recommendation.as_deref(),
            Some("sqlite")
        );
        assert_eq!(learn(&entries, "/two", "storage").recommendation, None);
    }

    #[test]
    fn an_override_reopens_a_silenced_choice() {
        let dir = dir("taste-reopen");
        seed(&dir, "/p", &["postgres"; 6]);
        let entries = read(&dir).expect("the ledger");
        assert!(learn(&entries, "/p", "storage").auto);
        seed(&dir, "/p", &["sqlite"]);
        let entries = read(&dir).expect("the ledger");
        let learned = learn(&entries, "/p", "storage");
        assert!(!learned.auto, "one override reopens the question");
    }

    #[test]
    fn two_other_declines_are_only_a_proposal() {
        let dir = dir("taste-decline");
        record(
            &dir,
            Path::new("/p"),
            &ask(),
            &answer("other", Some("use redis")),
        )
        .expect("a decline");
        record(
            &dir,
            Path::new("/p"),
            &ask(),
            &answer("other", Some("use redis")),
        )
        .expect("a decline");
        let proposals = deny_proposals(&read(&dir).expect("the ledger"));
        assert_eq!(proposals, vec![("/p".into(), "storage".into(), 2)]);
    }

    #[test]
    fn forget_removes_only_that_line() {
        let dir = dir("taste-forget");
        seed(&dir, "/p", &["sqlite", "postgres", "sqlite"]);
        assert_eq!(forget(&dir, Some(1)).expect("forget one"), 1);
        let entries = read(&dir).expect("the ledger");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].chosen, "sqlite");
        assert_eq!(entries[1].chosen, "sqlite");
        assert_eq!(forget(&dir, None).expect("forget all"), 2);
        assert!(read(&dir).expect("the ledger").is_empty());
    }
}
