//! Jobs that run on their own: a standing goal file, a schedule, and the chat that receives the result.
//!
//! One job is one file under `<state>/jobs`, and the file is the record. Every scheduler tick reads it,
//! so a correction is the next run's behaviour instead of a change nobody notices until a restart. The
//! frontmatter holds the machine fields (`id`, `chat`, `schedule`, `mode`, `last`) and the statement of
//! intent (`inputs`, `outputs`, `done_when`, `stop_when`, `never`); the body is the goal.
//!
//! The old shape, a list in `<state>/cron.json`, is imported once by [`migrate`] when the gateway starts.
//! A file that will not parse is skipped and named in [`load`]'s problems: it does not run, and the
//! person who wrote it is the one who can fix it.
//!
//! Stated limit: a frontmatter field is one line. Multi-line detail belongs in the body, because the
//! frontmatter parser the vault already owns reads scalars and bracketed lists, and inventing a second
//! dialect for one file shape is how two parsers start to disagree.

use crate::mode::Mode;
use mneme::{Field, Frontmatter};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Job {
    pub id: u64,
    pub chat: i64,
    pub schedule: String,
    /// Read or act. A job file written before this field existed reads as `read`, which is the safe
    /// direction: an old job loses tools it may have used, and never gains one it was not given.
    pub mode: Mode,
    /// The body of the file: what the job is for.
    pub goal: String,
    /// What the job reads. One line, for the person as much as the model.
    pub inputs: String,
    /// What the answer is: a report, a digest, a list with links.
    pub outputs: String,
    /// The condition that says the job did its work.
    pub done_when: String,
    /// When to stop looking, so a research job does not wander.
    pub stop_when: String,
    /// The last calendar day this job may start (`YYYY-MM-DD`, empty = no end). `stop_when` is what the
    /// model is told; `until` is what the scheduler enforces, because a promise in prose is not a stop.
    pub until: String,
    /// What this job must not do, even in `act` mode.
    pub never: String,
    /// The epoch second the last run started.
    pub last: i64,
}

/// Where the standing goal files live.
pub fn dir(state: &Path) -> PathBuf {
    state.join("jobs")
}

/// The old single-file shape, imported once.
pub fn legacy_path(state: &Path) -> PathBuf {
    state.join("cron.json")
}

/// Every job file, and every file that would not parse. Sorted by id, so a listing reads the same way
/// twice. A missing directory is no jobs, not a failure: a fresh state directory is the normal case.
pub fn load(state: &Path) -> (Vec<Job>, Vec<String>) {
    let mut jobs = Vec::new();
    let mut problems = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir(state)) else {
        return (jobs, problems);
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|kind| kind == "md"))
        .collect();
    paths.sort();
    for path in paths {
        match read_file(&path) {
            Ok(job) => jobs.push(job),
            Err(said) => problems.push(format!("{}: {said}", relative(state, &path))),
        }
    }
    jobs.sort_by_key(|job| job.id);
    (jobs, problems)
}

fn read_file(path: &Path) -> Result<Job, String> {
    let text = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
    let (header, body) = Frontmatter::parse(&text);
    let id = scalar(&header, "id")
        .parse::<u64>()
        .map_err(|_| "no numeric `id`".to_string())?;
    let chat = scalar(&header, "chat")
        .parse::<i64>()
        .map_err(|_| "no numeric `chat`".to_string())?;
    let schedule = scalar(&header, "schedule");
    if schedule.is_empty() {
        return Err("no `schedule`".to_string());
    }
    let goal = body.trim().to_string();
    if goal.is_empty() {
        return Err("no goal in the body".to_string());
    }
    Ok(Job {
        id,
        chat,
        schedule,
        mode: Mode::parse(&scalar(&header, "mode")).unwrap_or(Mode::Read),
        goal,
        inputs: scalar(&header, "inputs"),
        outputs: scalar(&header, "outputs"),
        done_when: scalar(&header, "done_when"),
        stop_when: scalar(&header, "stop_when"),
        until: until(&scalar(&header, "until"))?,
        never: scalar(&header, "never"),
        last: scalar(&header, "last").parse::<i64>().unwrap_or(0),
    })
}

/// One key as text. A bracketed list is joined rather than refused: a person who typed
/// `never: [no posts, no email]` meant both, and dropping the key because of its shape would be the
/// import trap the vault's parser exists to avoid.
fn scalar(header: &Frontmatter, key: &str) -> String {
    match header.extra.get(key) {
        Some(Field::Scalar(text)) => text.clone(),
        Some(Field::List(items)) => items.join(", "),
        None => String::new(),
    }
}

fn put(header: &mut Frontmatter, key: &str, value: String) {
    if !value.is_empty() {
        header.extra.insert(key.to_string(), Field::Scalar(value));
    }
}

/// Write one job file, named for its id and the first words of its goal. Returns the path, so the tool
/// can show a person the file to edit.
pub fn write(state: &Path, job: &Job) -> Result<PathBuf, String> {
    let folder = dir(state);
    std::fs::create_dir_all(&folder).map_err(|error| error.to_string())?;
    let first = job.goal.lines().next().unwrap_or("");
    let path = folder.join(format!("{}-{}.md", job.id, mneme::slugify(first, 6)));
    write_to(&path, job)?;
    Ok(path)
}

fn write_to(path: &Path, job: &Job) -> Result<(), String> {
    let mut header = Frontmatter::default();
    put(&mut header, "id", job.id.to_string());
    put(&mut header, "chat", job.chat.to_string());
    put(&mut header, "schedule", job.schedule.clone());
    put(&mut header, "mode", job.mode.name().to_string());
    put(&mut header, "last", job.last.to_string());
    put(&mut header, "inputs", job.inputs.clone());
    put(&mut header, "outputs", job.outputs.clone());
    put(&mut header, "done_when", job.done_when.clone());
    put(&mut header, "stop_when", job.stop_when.clone());
    put(&mut header, "until", job.until.clone());
    put(&mut header, "never", job.never.clone());
    let text = format!("{}\n{}\n", header.render(), job.goal.trim());
    std::fs::write(path, text).map_err(|error| format!("{}: {error}", path.display()))
}

/// The file for this id, if one is there.
pub fn path_of(state: &Path, id: u64) -> Result<PathBuf, String> {
    let Ok(entries) = std::fs::read_dir(dir(state)) else {
        return Err(format!("there is no job {id}"));
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|kind| kind != "md") {
            continue;
        }
        if read_file(&path).is_ok_and(|job| job.id == id) {
            return Ok(path);
        }
    }
    Err(format!("there is no job {id}"))
}

/// Write the last-run stamp before the work starts. The file is read again first, so a hand edit made
/// between the scheduler's load and this write is kept: the stamp changes `last` and nothing else.
pub fn stamp(state: &Path, id: u64, now: i64) -> Result<(), String> {
    let path = path_of(state, id)?;
    let mut job = read_file(&path)?;
    job.last = now;
    write_to(&path, &job)
}

/// Delete one job file. Removing a job is the person taking back work they asked for, so it is not a
/// floor action; the file's path is returned so the tool can say what was removed.
pub fn remove(state: &Path, id: u64) -> Result<String, String> {
    let path = path_of(state, id)?;
    std::fs::remove_file(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(relative(state, &path))
}

/// Cancel a standing job: the file goes, so it stops being due on the next tick, and the pending retry
/// beside it goes too, so a restart does not resume a job that no longer exists. The attempt log stays:
/// it is the record of what the job did, and cancelling is not undoing it.
///
/// One function for the CLI and the chat, because two call sites removing a file and forgetting the retry
/// is exactly the drift this item exists to stop. The stop is written to the activity ledger with the
/// actor who asked, the same way an allow is.
pub fn cancel(state: &Path, id: u64, by: &str) -> Result<String, String> {
    let path = path_of(state, id)?;
    let shown = relative(state, &path);
    std::fs::remove_file(&path).map_err(|error| format!("{}: {error}", path.display()))?;

    let retry = crate::retry::pending_path(state, id);
    if let Err(error) = std::fs::remove_file(&retry)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        crate::activity::stop(
            state,
            by,
            &format!("cancel: job {id}"),
            "the job file is gone but the pending retry could not be removed",
            &shown,
        );
        return Err(format!(
            "job {id} was removed but {} was not: {error}",
            retry.display()
        ));
    }

    crate::activity::stop(
        state,
        by,
        &format!("cancel: job {id}"),
        "a person cancelled the schedule; work it already did is not undone",
        &shown,
    );
    Ok(shown)
}

/// A path as it is shown to a person: relative to the state directory, never the whole machine path.
pub fn relative(state: &Path, path: &Path) -> String {
    path.strip_prefix(state)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// Import the old `<state>/cron.json` list as one file per job. Idempotent: a job whose file already
/// exists is left alone, and the JSON is renamed only after every entry has a file.
pub fn migrate(state: &Path) -> Result<usize, String> {
    let old = legacy_path(state);
    if !old.exists() {
        return Ok(0);
    }
    let text = std::fs::read_to_string(&old).map_err(|error| error.to_string())?;
    let parsed: Value =
        serde_json::from_str(&text).map_err(|error| format!("{}: {error}", old.display()))?;
    let jobs: Vec<Job> = parsed
        .as_array()
        .map(|items| items.iter().filter_map(legacy_item).collect())
        .unwrap_or_default();
    let mut written = 0;
    for job in &jobs {
        if path_of(state, job.id).is_ok() {
            continue;
        }
        write(state, job)?;
        written += 1;
    }
    std::fs::rename(&old, state.join("cron.json.migrated"))
        .map_err(|error| format!("could not retire {}: {error}", old.display()))?;
    Ok(written)
}

fn legacy_item(item: &Value) -> Option<Job> {
    Some(Job {
        id: item.get("id")?.as_u64()?,
        chat: item.get("chat")?.as_i64()?,
        schedule: item.get("when")?.as_str()?.to_string(),
        mode: item
            .get("mode")
            .and_then(Value::as_str)
            .and_then(Mode::parse)
            .unwrap_or(Mode::Read),
        goal: item.get("task")?.as_str()?.to_string(),
        inputs: String::new(),
        outputs: String::new(),
        done_when: String::new(),
        stop_when: String::new(),
        until: String::new(),
        never: String::new(),
        last: item.get("last").and_then(Value::as_i64).unwrap_or(0),
    })
}

pub fn next_id(jobs: &[Job]) -> u64 {
    jobs.iter().map(|job| job.id).max().unwrap_or(0) + 1
}

/// Whether a job should run now. `last` is the epoch second it last started, so an interval is measured
/// from the run, not from the clock.
pub fn due(job: &Job, now: i64) -> bool {
    if expired(job, now) {
        return false;
    }
    let when = job.schedule.trim().to_lowercase();
    if let Some(rest) = when.strip_prefix("every ") {
        return match interval(rest) {
            Some(seconds) => now - job.last >= seconds,
            None => false,
        };
    }
    if let Some(rest) = when.strip_prefix("daily ") {
        let Some(at) = daily(rest) else {
            return false;
        };
        // Today's occurrence, plus yesterday's if the machine was asleep through it.
        return (now >= at && job.last < at) || (now < at && job.last < at - 86_400);
    }
    false
}

/// `30m`, `6h`, `90s`.
fn interval(text: &str) -> Option<i64> {
    let text = text.trim();
    let (number, unit) = text.split_at(text.len().checked_sub(1)?);
    let value: i64 = number.trim().parse().ok()?;
    if value <= 0 {
        return None;
    }
    match unit {
        "s" => Some(value),
        "m" => Some(value * 60),
        "h" => Some(value * 3_600),
        _ => None,
    }
}

/// `07:30` as the epoch second of that time today, in local time. `date` knows the zone; this crate does
/// not carry a calendar library for one line of arithmetic.
fn daily(text: &str) -> Option<i64> {
    let text = text.trim();
    let (hour, minute) = text.split_once(':')?;
    let hour: u32 = hour.trim().parse().ok()?;
    let minute: u32 = minute.trim().parse().ok()?;
    if hour > 23 || minute > 59 {
        return None;
    }
    let stamp = format!("{hour:02}:{minute:02}");
    let output = Command::new("date")
        .arg("-d")
        .arg(&stamp)
        .arg("+%s")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

/// An `until` value: empty, or a real `YYYY-MM-DD`. Refused at load rather than treated as expired,
/// because a typo that silently stops a job is the failure nobody sees.
fn until(text: &str) -> Result<String, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(String::new());
    }
    match day_start(text) {
        Some(_) => Ok(text.to_string()),
        None => Err(format!("`until {text}` is not a date (YYYY-MM-DD)")),
    }
}

/// The scheduler's check for an `until` value, for callers that build a job rather than read one.
pub fn check_until(text: &str) -> Result<String, String> {
    until(text)
}

/// The start of a local calendar day, from a strict `YYYY-MM-DD`. `date` knows the zone; this crate
/// still does not carry a calendar library.
fn day_start(text: &str) -> Option<i64> {
    let text = text.trim();
    let mut parts = text.split('-');
    let (year, month, day) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some()
        || year.len() != 4
        || month.len() != 2
        || day.len() != 2
        || text.len() != 10
    {
        return None;
    }
    let output = Command::new("date")
        .arg("-d")
        .arg(text)
        .arg("+%s")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

/// Whether the job's last day is behind us. The last day itself is included: `until: 2026-11-01` means
/// the 1st may still run, and the 2nd may not.
fn expired(job: &Job, now: i64) -> bool {
    if job.until.is_empty() {
        return false;
    }
    match day_start(&job.until) {
        Some(start) => now >= start + 86_400,
        // Load refuses an unreadable date, so this is only reachable for a hand-built Job. Stopping is the
        // safe direction.
        None => true,
    }
}

/// A schedule is valid when it parses and names a real interval or time.
pub fn valid(when: &str) -> bool {
    let when = when.trim().to_lowercase();
    if let Some(rest) = when.strip_prefix("every ") {
        return interval(rest).is_some();
    }
    if let Some(rest) = when.strip_prefix("daily ") {
        return daily(rest).is_some();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/scratch");
        let dir = base.join(format!("cron-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch state directory");
        dir
    }

    fn job(schedule: &str, last: i64) -> Job {
        Job {
            id: 1,
            chat: 7,
            schedule: schedule.to_string(),
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

    #[test]
    fn a_job_stops_after_its_last_day() {
        let mut job = job("daily 07:30", 0);
        job.until = "2026-10-01".to_string();
        let on_the_day = at("2026-10-01 12:00");
        let after = at("2026-10-02 00:01");
        assert!(due(&job, on_the_day), "the last day still runs");
        assert!(!due(&job, after), "the day after does not");
    }

    #[test]
    fn an_until_that_is_not_a_date_is_refused_at_load() {
        let state = scratch("bad-until");
        std::fs::create_dir_all(dir(&state)).expect("the jobs folder");
        std::fs::write(
            dir(&state).join("4-bad.md"),
            "---\nid: 4\nchat: 7\nschedule: every 6h\nuntil: someday\n---\n\nreport\n",
        )
        .expect("a job file");
        let (jobs, problems) = load(&state);
        assert!(jobs.is_empty());
        assert!(problems[0].contains("until"), "{problems:?}");
        let _ = std::fs::remove_dir_all(&state);
    }

    fn at(text: &str) -> i64 {
        let output = Command::new("date")
            .arg("-d")
            .arg(text)
            .arg("+%s")
            .output()
            .expect("date");
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .expect("epoch seconds")
    }

    #[test]
    fn every_field_survives_the_file() {
        let state = scratch("round-trip");
        let job = Job {
            id: 2,
            chat: 9,
            schedule: "every 6h".to_string(),
            mode: Mode::Act,
            goal: "Summarise the inbox".to_string(),
            inputs: "the notes from yesterday".to_string(),
            outputs: "a digest with links".to_string(),
            done_when: "five items, each with a link".to_string(),
            stop_when: "one pass per source".to_string(),
            until: "2026-11-01".to_string(),
            never: "do not post anything".to_string(),
            last: 5,
        };
        let path = write(&state, &job).expect("the file is written");
        assert_eq!(path.file_name().unwrap(), "2-summarise-the-inbox.md");
        let (jobs, problems) = load(&state);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(jobs, vec![job]);
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn a_correction_to_the_file_is_what_the_next_load_sees() {
        let state = scratch("corrected");
        write(&state, &job("every 6h", 0)).expect("the file is written");
        let path = dir(&state).join("1-report.md");
        let text = std::fs::read_to_string(&path).expect("the file");
        std::fs::write(&path, text.replace("report", "report on the X feed")).expect("the edit");
        let (jobs, problems) = load(&state);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(jobs[0].goal, "report on the X feed");
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn a_stamp_keeps_a_hand_edit_made_after_the_load() {
        let state = scratch("stamp");
        write(&state, &job("every 6h", 0)).expect("the file is written");
        // The scheduler loaded the old body; a person corrects it before the stamp is written.
        let path = dir(&state).join("1-report.md");
        let text = std::fs::read_to_string(&path).expect("the file");
        std::fs::write(&path, text.replace("report", "report only what changed"))
            .expect("the edit");
        stamp(&state, 1, 99).expect("the stamp");
        let (jobs, _) = load(&state);
        assert_eq!(jobs[0].last, 99);
        assert_eq!(jobs[0].goal, "report only what changed");
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn a_file_that_will_not_parse_is_named_and_not_run() {
        let state = scratch("broken");
        std::fs::create_dir_all(dir(&state)).expect("the jobs folder");
        std::fs::write(
            dir(&state).join("9-broken.md"),
            "---\nchat: 7\nschedule: every 6h\n---\n\nno id here\n",
        )
        .expect("a broken file");
        let (jobs, problems) = load(&state);
        assert!(jobs.is_empty());
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("jobs/9-broken.md"), "{problems:?}");
        assert!(problems[0].contains("id"), "{problems:?}");
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn the_old_list_becomes_files_once() {
        let state = scratch("migrate");
        std::fs::write(
            legacy_path(&state),
            r#"[{"id":3,"chat":7,"when":"every 6h","task":"report","last":0}]"#,
        )
        .expect("the old list");
        assert_eq!(migrate(&state).expect("the migration"), 1);
        let (jobs, problems) = load(&state);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].schedule, "every 6h");
        assert_eq!(jobs[0].goal, "report");
        assert!(!legacy_path(&state).exists());
        assert!(state.join("cron.json.migrated").exists());
        assert_eq!(migrate(&state).expect("nothing left to do"), 0);
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn removing_a_job_removes_its_file() {
        let state = scratch("remove");
        write(&state, &job("every 6h", 0)).expect("the file is written");
        let path = remove(&state, 1).expect("the removal");
        assert_eq!(path, "jobs/1-report.md");
        assert!(load(&state).0.is_empty());
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn an_interval_is_measured_from_the_run() {
        assert!(!due(&job("every 30m", 1_000), 1_000 + 1_799));
        assert!(due(&job("every 30m", 1_000), 1_000 + 1_800));
    }

    #[test]
    fn a_daily_job_runs_once_a_day() {
        let now = daily("07:30").unwrap();
        assert!(due(&job("daily 07:30", now - 86_400), now + 5));
        assert!(!due(&job("daily 07:30", now + 1), now + 5));
    }

    #[test]
    fn nonsense_is_not_a_schedule() {
        assert!(!valid("every soon"));
        assert!(!valid("daily 25:00"));
        assert!(!valid("whenever"));
        assert!(valid("every 6h"));
        assert!(valid("daily 07:30"));
    }
}
