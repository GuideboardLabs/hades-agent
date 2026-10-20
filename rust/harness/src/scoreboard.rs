//! The scoreboard half of the router: what our own finished runs say about each profile.
//!
//! The design's rule is *rules first, then our own scoreboard*. The rules say what kind of work a goal
//! sounds like; the scoreboard says how that kind of work has actually gone. This module is the second
//! half, and it is deliberately small: one append-only line per finished walk, a pass rate and a mean
//! time per profile, and a choice that abstains until three runs exist.
//!
//! Why three: a profile that passed once is not evidence, and a router that chases a single good run
//! oscillates. Why pass rate before time: a cheap wrong answer is not a bargain. Why the failures are
//! recorded too: a scoreboard of passes only makes every profile look perfect.

use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

/// The fewest finished runs a profile needs before it may be chosen.
pub const MIN_RUNS: usize = 3;

/// One finished walk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub profile: String,
    pub passed: bool,
    pub attempts: u32,
    pub millis: u64,
    /// What the endpoint reported for this run, in the two halves it reports: prompt and generated are kept
    /// apart so a total can be checked against the model's own numbers rather than trusted as one.
    pub prompt_tokens: u64,
    pub generated_tokens: u64,
    pub at: u64,
}

/// What the record says about one profile.
#[derive(Debug, Clone, PartialEq)]
pub struct Standing {
    pub profile: String,
    pub runs: usize,
    pub passes: usize,
    pub mean_ms: u64,
    pub prompt_tokens: u64,
    pub generated_tokens: u64,
}

impl Standing {
    pub fn rate(&self) -> f64 {
        if self.runs == 0 {
            0.0
        } else {
            self.passes as f64 / self.runs as f64
        }
    }

    /// What the profile's runs spent in total, as the endpoints reported it.
    pub fn tokens(&self) -> u64 {
        self.prompt_tokens + self.generated_tokens
    }

    pub fn line(&self) -> String {
        format!(
            "{:<10} {:>3} run(s)  {:>3}/{:<3}  {:>5.1}%  {:>6} ms mean  {:>7} tokens{}",
            self.profile,
            self.runs,
            self.passes,
            self.runs,
            self.rate() * 100.0,
            self.mean_ms,
            self.tokens(),
            if self.runs < MIN_RUNS {
                "  (too few to choose)"
            } else {
                ""
            }
        )
    }
}

/// Append one record. A failure to write is the caller's to ignore: a scoreboard is advice, and losing a
/// row must never turn a finished run into a failed one.
pub fn record(state: &Path, record: &Record) -> std::io::Result<()> {
    use std::io::Write as _;
    std::fs::create_dir_all(state)?;
    let path = state.join("scoreboard.jsonl");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(
        file,
        "{}",
        json!({
            "profile": record.profile,
            "passed": record.passed,
            "attempts": record.attempts,
            "millis": record.millis,
            "prompt_tokens": record.prompt_tokens,
            "generated_tokens": record.generated_tokens,
            "at": record.at,
        })
    )
}

/// Read every record. A corrupt line is skipped rather than fatal: the scoreboard is not the ledger.
pub fn load(state: &Path) -> Vec<Record> {
    let text = std::fs::read_to_string(state.join("scoreboard.jsonl")).unwrap_or_default();
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|entry| {
            Some(Record {
                profile: entry.get("profile")?.as_str()?.to_string(),
                passed: entry.get("passed")?.as_bool()?,
                attempts: entry.get("attempts")?.as_u64()? as u32,
                millis: entry.get("millis")?.as_u64()?,
                // A row written before spend was recorded is a row that reported nothing, which is zero,
                // not a corrupt line: the scoreboard must keep reading its own history.
                prompt_tokens: entry
                    .get("prompt_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                generated_tokens: entry
                    .get("generated_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                at: entry.get("at")?.as_u64()?,
            })
        })
        .collect()
}

/// One standing per profile, ordered by name so the table is stable between runs.
pub fn standings(records: &[Record]) -> Vec<Standing> {
    #[derive(Default)]
    struct Tally {
        runs: usize,
        passes: usize,
        millis: u64,
        prompt_tokens: u64,
        generated_tokens: u64,
    }
    let mut by_profile: BTreeMap<String, Tally> = BTreeMap::new();
    for record in records {
        let tally = by_profile.entry(record.profile.clone()).or_default();
        tally.runs += 1;
        tally.passes += usize::from(record.passed);
        tally.millis += record.millis;
        tally.prompt_tokens += record.prompt_tokens;
        tally.generated_tokens += record.generated_tokens;
    }
    by_profile
        .into_iter()
        .map(|(profile, tally)| Standing {
            profile,
            runs: tally.runs,
            passes: tally.passes,
            mean_ms: tally.millis / tally.runs.max(1) as u64,
            prompt_tokens: tally.prompt_tokens,
            generated_tokens: tally.generated_tokens,
        })
        .collect()
}

/// The profile the scoreboard would pick, or none when no profile has enough runs. Best pass rate first;
/// on a tie, the lower mean time; on that tie, the name, so the choice is never random.
pub fn choose(records: &[Record]) -> Option<Standing> {
    let mut eligible: Vec<Standing> = standings(records)
        .into_iter()
        .filter(|standing| standing.runs >= MIN_RUNS)
        .collect();
    eligible.sort_by(|a, b| {
        b.rate()
            .partial_cmp(&a.rate())
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.mean_ms.cmp(&b.mean_ms))
            .then(a.profile.cmp(&b.profile))
    });
    eligible.into_iter().next()
}

/// The reason recorded when the scoreboard made the choice; it names the evidence, not just the winner.
pub fn reason(standing: &Standing) -> String {
    format!(
        "no rule matched, so the scoreboard picked the best pass rate ({} of {} runs, {} ms mean)",
        standing.passes, standing.runs, standing.mean_ms
    )
}

/// The table `hades scoreboard` prints.
pub fn report(records: &[Record]) -> String {
    if records.is_empty() {
        return "no finished runs yet: the scoreboard is written by `hades run`\n".to_string();
    }
    let mut text = format!(
        "profile    runs    passes   rate      mean           tokens{}\n",
        if records.len() == 1 {
            " (1 record)"
        } else {
            ""
        }
    );
    for standing in standings(records) {
        text.push_str(&standing.line());
        text.push('\n');
    }
    match choose(records) {
        Some(best) => text.push_str(&format!(
            "\nwould pick: {} ({:.0}% over {} runs)\n",
            best.profile,
            best.rate() * 100.0,
            best.runs
        )),
        None => text.push_str(&format!(
            "\nwould pick: nothing yet, because no profile has {MIN_RUNS} finished runs\n"
        )),
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(profile: &str, passed: bool, millis: u64) -> Record {
        spent(profile, passed, millis, 0, 0)
    }

    fn spent(profile: &str, passed: bool, millis: u64, prompt: u64, generated: u64) -> Record {
        Record {
            profile: profile.to_string(),
            passed,
            attempts: 1,
            millis,
            prompt_tokens: prompt,
            generated_tokens: generated,
            at: 0,
        }
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = crate::scratch::root().join(format!("scoreboard-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_fixture_run_adds_to_the_totals_without_moving_the_pass_rate() {
        let state = scratch("spend");
        // Three finished runs, each reporting what the endpoint said. The spend is written and read back
        // through the file the scoreboard actually uses, not held in the test's own arithmetic.
        for (passed, prompt, generated) in [(true, 1000u64, 500u64), (false, 200, 50), (true, 0, 0)]
        {
            record(&state, &spent("sweep", passed, 100, prompt, generated)).expect("a row");
        }
        let records = load(&state);
        assert_eq!(records.len(), 3, "{records:#?}");
        let mut standings = standings(&records);
        let standing = standings.remove(0);
        assert_eq!(standing.runs, 3);
        assert_eq!(standing.passes, 2);
        assert_eq!(standing.rate(), 2.0 / 3.0);
        assert_eq!(standing.prompt_tokens, 1200);
        assert_eq!(standing.generated_tokens, 550);
        assert_eq!(standing.tokens(), 1750);
        let table = report(&records);
        assert!(table.contains("1750 tokens"), "{table}");
        // The rate the table states is still the pass count over the run count, not a spend-weighted one.
        assert!(table.contains("66.7%"), "{table}");
    }

    #[test]
    fn a_profile_needs_three_runs_before_it_is_chosen() {
        let records = vec![row("sweep", true, 10), row("sweep", true, 20)];
        assert!(choose(&records).is_none());
        let mut records = records;
        records.push(row("sweep", false, 30));
        let best = choose(&records).expect("three runs");
        assert_eq!(best.profile, "sweep");
        assert_eq!(best.runs, 3);
    }

    #[test]
    fn pass_rate_beats_a_faster_worse_profile() {
        let records = vec![
            row("fast", true, 10),
            row("fast", false, 10),
            row("fast", false, 10),
            row("slow", true, 900),
            row("slow", true, 900),
            row("slow", true, 900),
        ];
        let best = choose(&records).expect("both eligible");
        assert_eq!(best.profile, "slow", "{best:?}");
    }

    #[test]
    fn a_tie_goes_to_the_lower_mean_time() {
        let records = vec![
            row("a", true, 500),
            row("a", true, 500),
            row("a", true, 500),
            row("b", true, 100),
            row("b", true, 100),
            row("b", true, 100),
        ];
        assert_eq!(choose(&records).expect("both eligible").profile, "b");
    }

    #[test]
    fn the_reason_names_the_evidence() {
        let standing = choose(&[
            row("sweep", true, 100),
            row("sweep", true, 100),
            row("sweep", false, 100),
        ])
        .expect("three runs");
        let said = reason(&standing);
        assert!(said.contains("2 of 3 runs"), "{said}");
        assert!(said.contains("100 ms"), "{said}");
    }
}
