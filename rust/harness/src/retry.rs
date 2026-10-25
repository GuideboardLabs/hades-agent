//! What happens when a scheduled job fails: one attempt record per try, a bounded retry with backoff,
//! and a key that names the occurrence every try belongs to.
//!
//! State lives beside the jobs but is not part of the goal file. `<state>/retry/<id>.json` is the pending
//! try; `<state>/attempts/<id>.jsonl` is what happened. The log is append-only and never rewritten, so a
//! person can see a flapping job instead of only its last message.
//!
//! The key, `<id>@<stamp>`, makes a retry visibly the same occurrence and groups its lines in the log. It
//! does not make an `act` job's side effects repeatable: if the first attempt already sent something, the
//! second cannot know. Keep an unattended job's steps idempotent, and prefer a `read` job for anything
//! that does not have to change the machine.

use crate::cron::Job;
use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Tries per occurrence, including the first. Two retries cover a transient endpoint or network fault
/// and are short enough that a person is not waiting on a broken job for a whole day.
pub const ATTEMPTS: u32 = 3;

/// A try that is waiting for its time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pending {
    pub window: String,
    /// The attempt that failed, so the next one is `attempt + 1`.
    pub attempt: u32,
    /// The epoch second the next try is allowed to start.
    pub at: i64,
    /// What the failed try said, carried into the next prompt.
    pub error: String,
}

/// One line of the attempt log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub window: String,
    pub attempt: u32,
    pub started: i64,
    pub finished: i64,
    /// `ok` or `failed`.
    pub outcome: String,
    pub detail: String,
}

/// What the scheduler should do with a job this tick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub window: String,
    pub attempt: u32,
    /// Empty on a first attempt; the previous failure on a retry.
    pub previous: String,
}

/// The occurrence key: the job and the second its first try started. Stable across its retries.
pub fn window(id: u64, now: i64) -> String {
    format!("{id}@{now}")
}

/// The wait after a failed attempt: thirty seconds, then two minutes. The third failure is the last.
pub fn backoff(attempt: u32) -> i64 {
    match attempt {
        1 => 30,
        _ => 120,
    }
}

/// Due first, then a pending retry whose time has come. A retry already at the bound is not run again:
/// the scheduler only writes a pending try below the bound, and a hand-edited one above it must not turn
/// into an unbounded loop.
pub fn step(job: &Job, pending: Option<&Pending>, now: i64) -> Option<Step> {
    if crate::cron::due(job, now) {
        return Some(Step {
            window: window(job.id, now),
            attempt: 1,
            previous: String::new(),
        });
    }
    // A pending try does not outlive the job's `until` date. `due` already refuses an expired job, but a
    // retry reaches this function on the other path, and running one after the cutoff would be a model
    // call the schedule promised not to make.
    if crate::cron::expired(job, now) {
        return None;
    }
    let pending = pending?;
    if pending.at > now || pending.attempt >= ATTEMPTS {
        return None;
    }
    Some(Step {
        window: pending.window.clone(),
        attempt: pending.attempt + 1,
        previous: pending.error.clone(),
    })
}

fn retry_dir(state: &Path) -> PathBuf {
    state.join("retry")
}

fn attempts_dir(state: &Path) -> PathBuf {
    state.join("attempts")
}

/// The pending try for a job, if one is waiting.
pub fn pending_path(state: &Path, id: u64) -> PathBuf {
    retry_dir(state).join(format!("{id}.json"))
}

/// The append-only record of tries for a job.
pub fn log_path(state: &Path, id: u64) -> PathBuf {
    attempts_dir(state).join(format!("{id}.jsonl"))
}

/// The pending try, or nothing. A file that will not read is no pending try: the job waits for its next
/// window rather than running on a state nobody can explain.
pub fn read(state: &Path, id: u64) -> Option<Pending> {
    let text = std::fs::read_to_string(pending_path(state, id)).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    Some(Pending {
        window: value.get("window")?.as_str()?.to_string(),
        attempt: value.get("attempt")?.as_u64()? as u32,
        at: value.get("at")?.as_i64()?,
        error: value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    })
}

/// Record the try that comes next. Written to a temporary file and renamed, so a crash leaves the old
/// state whole rather than half a document.
pub fn again(state: &Path, id: u64, step: &Step, at: i64, error: &str) -> Result<(), String> {
    let folder = retry_dir(state);
    std::fs::create_dir_all(&folder).map_err(|error| error.to_string())?;
    let path = pending_path(state, id);
    let temporary = path.with_extension("json.tmp");
    let body = json!({
        "window": step.window,
        "attempt": step.attempt,
        "at": at,
        "error": clip(error, 500)
    });
    let text = serde_json::to_string_pretty(&body).map_err(|error| error.to_string())?;
    std::fs::write(&temporary, text + "\n").map_err(|error| error.to_string())?;
    std::fs::rename(&temporary, &path).map_err(|error| error.to_string())
}

/// The occurrence is over: done, or failed for the last time.
pub fn done(state: &Path, id: u64) {
    let _ = std::fs::remove_file(pending_path(state, id));
}

/// Append one try to the log. The log is the durable part: the pending file is gone after the last try,
/// and the message in the chat is only the last line of the story.
pub fn record(state: &Path, id: u64, entry: &Entry) -> Result<(), String> {
    let folder = attempts_dir(state);
    std::fs::create_dir_all(&folder).map_err(|error| error.to_string())?;
    let body = json!({
        "window": entry.window,
        "attempt": entry.attempt,
        "started": entry.started,
        "finished": entry.finished,
        "outcome": entry.outcome,
        "detail": clip(&entry.detail, 500)
    });
    let mut line = serde_json::to_string(&body).map_err(|error| error.to_string())?;
    line.push('\n');
    let path = log_path(state, id);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    file.write_all(line.as_bytes())
        .map_err(|error| format!("{}: {error}", path.display()))
}

/// Every try in the log, oldest first. A line that will not parse is skipped: one bad append must not
/// hide the tries around it.
pub fn entries(state: &Path, id: u64) -> Vec<Entry> {
    let Ok(text) = std::fs::read_to_string(log_path(state, id)) else {
        return Vec::new();
    };
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(parse)
        .collect()
}

fn parse(line: &str) -> Option<Entry> {
    let value: Value = serde_json::from_str(line).ok()?;
    Some(Entry {
        window: value.get("window")?.as_str()?.to_string(),
        attempt: value.get("attempt")?.as_u64()? as u32,
        started: value.get("started")?.as_i64()?,
        finished: value.get("finished")?.as_i64()?,
        outcome: value.get("outcome")?.as_str()?.to_string(),
        detail: value
            .get("detail")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    })
}

/// The last try in the log, which is what `hades jobs` shows.
pub fn latest(state: &Path, id: u64) -> Option<Entry> {
    entries(state, id).pop()
}

fn clip(text: &str, limit: usize) -> String {
    let mut cut: String = text.chars().take(limit).collect();
    if text.chars().count() > limit {
        cut.push_str(" [cut]");
    }
    cut
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mode::Mode;

    fn job(last: i64) -> Job {
        Job {
            id: 2,
            chat: 7,
            schedule: "every 6h".to_string(),
            mode: Mode::Read,
            goal: "report".to_string(),
            inputs: String::new(),
            outputs: String::new(),
            done_when: String::new(),
            stop_when: String::new(),
            until: String::new(),
            never: String::new(),
            last,
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = crate::scratch::root().join(format!("retry-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch state directory");
        dir
    }

    #[test]
    fn a_due_job_starts_a_new_occurrence() {
        let now = 6 * 3_600;
        let step = step(&job(0), None, now).expect("due");
        assert_eq!(step.attempt, 1);
        assert_eq!(step.window, format!("2@{now}"));
        assert!(step.previous.is_empty());
    }

    #[test]
    fn a_pending_try_waits_for_its_time_and_then_becomes_the_next_attempt() {
        let pending = Pending {
            window: "2@1000".to_string(),
            attempt: 1,
            at: 1_030,
            error: "endpoint refused".to_string(),
        };
        assert_eq!(step(&job(1_000), Some(&pending), 1_029), None);
        let next = step(&job(1_000), Some(&pending), 1_030).expect("the retry");
        assert_eq!(next.attempt, 2);
        assert_eq!(next.window, "2@1000", "the same occurrence");
        assert_eq!(next.previous, "endpoint refused");
    }

    fn job_until(last: i64, until: &str) -> Job {
        Job {
            until: until.to_string(),
            ..job(last)
        }
    }

    #[test]
    fn a_pending_retry_does_not_outlive_the_until_date() {
        // A try was saved before midnight; the process restarts after it. The scheduler must not resume
        // the retry, even though its time has come.
        let start = crate::cron::day_start("2026-10-24").expect("a date");
        let now = start + 86_400 + 60;
        let pending = Pending {
            window: "2@1".to_string(),
            attempt: 1,
            at: 0,
            error: "endpoint refused".to_string(),
        };
        assert_eq!(step(&job_until(now - 60, "2026-10-24"), Some(&pending), now), None);
    }

    #[test]
    fn a_pending_retry_still_runs_before_the_until_date() {
        // The same pending try, an hour before midnight: the retry is allowed, and it keeps its occurrence.
        let start = crate::cron::day_start("2026-10-24").expect("a date");
        let now = start + 3_600;
        let pending = Pending {
            window: "2@1".to_string(),
            attempt: 1,
            at: 0,
            error: "endpoint refused".to_string(),
        };
        let next = step(&job_until(now - 60, "2026-10-24"), Some(&pending), now).expect("the retry");
        assert_eq!(next.attempt, 2);
        assert_eq!(next.window, "2@1");
    }

    #[test]
    fn a_persisted_retry_is_refused_after_a_restart_past_until() {
        // The recovery case: the pending try is on disk before the cutoff, the process restarts after
        // midnight, and the scheduler must make no model call.
        let state = scratch("until-restart");
        let start = crate::cron::day_start("2026-10-24").expect("a date");
        again(
            &state,
            2,
            &Step {
                window: "2@1".to_string(),
                attempt: 1,
                previous: String::new(),
            },
            0,
            "endpoint refused",
        )
        .expect("the pending try");
        let reread = read(&state, 2).expect("persisted across the restart");
        let now = start + 86_400 + 60;
        assert_eq!(step(&job_until(now - 60, "2026-10-24"), Some(&reread), now), None);
    }

    #[test]
    fn a_pending_try_at_the_bound_is_not_run_again() {
        let pending = Pending {
            window: "2@1000".to_string(),
            attempt: ATTEMPTS,
            at: 0,
            error: "no".to_string(),
        };
        assert_eq!(step(&job(1_000), Some(&pending), 10_000), None);
    }

    #[test]
    fn a_new_window_outranks_a_pending_retry() {
        // The interval has elapsed, so the job is due again. A stale pending try must not replace it.
        let pending = Pending {
            window: "2@1000".to_string(),
            attempt: 1,
            at: 9_999_999,
            error: "old".to_string(),
        };
        let next = step(&job(1_000), Some(&pending), 1_000 + 6 * 3_600).expect("due");
        assert_eq!(next.attempt, 1);
        assert_ne!(next.window, pending.window);
    }

    #[test]
    fn the_pending_try_round_trips_and_is_removed_when_the_occurrence_ends() {
        let state = scratch("pending");
        let step = Step {
            window: "2@1000".to_string(),
            attempt: 1,
            previous: String::new(),
        };
        again(&state, 2, &step, 1_030, "endpoint refused").expect("the pending try");
        let read_back = read(&state, 2).expect("the file");
        assert_eq!(read_back.attempt, 1);
        assert_eq!(read_back.at, 1_030);
        assert_eq!(read_back.error, "endpoint refused");
        done(&state, 2);
        assert_eq!(read(&state, 2), None);
    }

    #[test]
    fn the_log_keeps_every_try_in_order() {
        let state = scratch("log");
        record(
            &state,
            2,
            &Entry {
                window: "2@1000".to_string(),
                attempt: 1,
                started: 1_000,
                finished: 1_010,
                outcome: "failed".to_string(),
                detail: "timed out".to_string(),
            },
        )
        .expect("first");
        record(
            &state,
            2,
            &Entry {
                window: "2@1000".to_string(),
                attempt: 2,
                started: 1_040,
                finished: 1_050,
                outcome: "ok".to_string(),
                detail: String::new(),
            },
        )
        .expect("second");
        let latest = latest(&state, 2).expect("the last line");
        assert_eq!(latest.attempt, 2);
        assert_eq!(latest.outcome, "ok");
        assert_eq!(latest.window, "2@1000");
        let all = entries(&state, 2);
        assert_eq!(all.len(), 2, "{all:?}");
        assert_eq!(all[0].attempt, 1);
        assert_eq!(all[0].outcome, "failed");
        assert_eq!(all[0].detail, "timed out");
        assert_eq!(all[1].attempt, 2);
    }

    #[test]
    fn the_waits_are_short_and_bounded() {
        assert_eq!(backoff(1), 30);
        assert_eq!(backoff(2), 120);
        assert_eq!(ATTEMPTS, 3);
    }
}
