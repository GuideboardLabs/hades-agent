//! Reading quality, scored by the code that ships.
//!
//! The same shape as `searcheval`: a frozen gold set names the sentence that answers a question on a page, a
//! recording holds what the read pipeline returned, and this module grades both through [`crate::read::read`]
//! rather than through a copy of it. The number it prints is the only reading claim a release note may quote.
//!
//! A gold row:
//!
//! ```json
//! {"url":"https://sqlite.org/wal.html","class":"docs","expect":"ok",
//!  "question":"how do I enable WAL","fragments":["journal_mode=WAL"]}
//! ```

use std::path::PathBuf;

/// One line of the gold set.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct GoldRow {
    pub url: String,
    #[serde(default = "docs_class")]
    pub class: String,
    #[serde(default = "ok_expect")]
    pub expect: String,
    #[serde(default)]
    pub question: String,
    /// Text the extraction must contain, as a sentence copied from the body where possible. A single word scores
    /// `found` against a whole-page dump, which is how the first version of this set reported 86.7% while the
    /// extractor passed every site menu straight through.
    #[serde(default)]
    pub fragments: Vec<String>,
    /// Text the extraction must NOT contain: navigation, cookie banners, footers. This is the half of the claim
    /// that a substring search for content cannot make.
    #[serde(default)]
    pub absent: Vec<String>,
    /// Why this row is shaped the way it is, when it was changed after the set froze.
    #[serde(default)]
    pub note: String,
}

fn docs_class() -> String {
    "docs".to_string()
}

fn ok_expect() -> String {
    "ok".to_string()
}

/// What one class scored.
#[derive(Debug, Clone, PartialEq)]
pub struct Score {
    pub class: String,
    pub asked: usize,
    /// Rows whose extracted text contained the answering fragment.
    pub found: usize,
    /// Rows whose terminal state was the one the gold expected.
    pub stated: usize,
    /// Rows that carried an absence list and satisfied it. A row with no list is counted in `unchecked`, never
    /// here: `all()` over an empty iterator is true, so counting those as clean made fourteen rows pass without a
    /// check being made.
    pub clean: usize,
    /// Rows that carried no absence list at all, so nothing was asserted about their chrome.
    pub unchecked: usize,
    pub chars: usize,
    pub ms: u128,
    /// Mean share of lines that look like navigation, per the marker list. Printed because the release notes
    /// claim navigation is stripped and a claim with no number behind it is what the last review caught.
    pub boilerplate: f64,
}

impl Score {
    pub fn recall(&self) -> f64 {
        if self.asked == 0 {
            0.0
        } else {
            self.found as f64 / self.asked as f64
        }
    }

    pub fn states(&self) -> f64 {
        if self.asked == 0 {
            0.0
        } else {
            self.stated as f64 / self.asked as f64
        }
    }

    /// The proportion of rows whose body carried none of the strings the gold says must be absent.
    pub fn cleanliness(&self) -> f64 {
        if self.asked == 0 {
            0.0
        } else {
            self.clean as f64 / self.asked as f64
        }
    }

    /// Rows whose chrome nothing asserted, because the set has no absence list for them.
    pub fn unchecked(&self) -> usize {
        self.unchecked
    }
}

/// Whether the extracted text answers the question.
///
/// Case and whitespace are normalised, because a page that says `journal_mode=WAL` and a fragment that says
/// `journal_mode = WAL` are the same answer and a naive comparison would score it a miss. This is the same
/// correction the search specification needed for phrase assertions.
pub fn answers(text: &str, fragments: &[String]) -> bool {
    if fragments.is_empty() {
        return false;
    }
    let haystack = squash(text);
    fragments
        .iter()
        .any(|fragment| haystack.contains(&squash(fragment)))
}

/// Lowercase with every space removed.
///
/// Whitespace is the one difference between a page and a fragment that carries no meaning, and it appears in
/// exactly the places a reader does not notice: `journal_mode = WAL` and `journal_mode=WAL` are one answer.
/// Collapsing runs to a single space was not enough, because the difference here is a space beside punctuation,
/// so the comparison drops spaces entirely.
/// Whether the text contains a forbidden string, under the same normalisation as `answers`.
pub fn contains(text: &str, needle: &str) -> bool {
    !needle.is_empty() && squash(text).contains(&squash(needle))
}

fn squash(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Lines that are almost always chrome. A page whose body still carries these was stripped of markup, not of
/// everything that is not the article.
pub const NAV_MARKERS: &[&str] = &[
    "skip to content",
    "sign in",
    "sign up",
    "log in",
    "subscribe",
    "newsletter",
    "cookie",
    "privacy policy",
    "terms of service",
    "all rights reserved",
    "report a bug",
    "show source",
    "previous topic",
    "next topic",
    "table of contents",
    "advertise",
    "download the app",
];

/// The proportion of lines that look like navigation, for the boilerplate claim.
///
/// A line counts as boilerplate when it is short and carries no sentence: link rows, menu labels and menu
/// separators. The list of markers is a fixture-scale approximation, and the release notes say so.
pub fn boilerplate_ratio(text: &str, markers: &[&str]) -> f64 {
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if lines.is_empty() {
        return 0.0;
    }
    let hits = lines
        .iter()
        .filter(|line| {
            let low = line.to_lowercase();
            markers
                .iter()
                .any(|marker| low.contains(&marker.to_lowercase()))
        })
        .count();
    hits as f64 / lines.len() as f64
}

/// Grade a set of readings, grouped by class.
pub fn score(gold: &[GoldRow], readings: &[(GoldRow, crate::read::Reading)]) -> Vec<Score> {
    let mut scores: std::collections::BTreeMap<String, Score> = std::collections::BTreeMap::new();
    for (row, reading) in readings {
        let entry = scores.entry(row.class.clone()).or_insert_with(|| Score {
            class: row.class.clone(),
            asked: 0,
            found: 0,
            stated: 0,
            clean: 0,
            unchecked: 0,
            chars: 0,
            ms: 0,
            boilerplate: 0.0,
        });
        entry.asked += 1;
        entry.chars += reading.chars();
        entry.boilerplate += boilerplate_ratio(&reading.text, NAV_MARKERS);
        entry.ms += reading.ms;
        if answers(&reading.text, &row.fragments) {
            entry.found += 1;
        }
        if reading.state.to_string() == row.expect {
            entry.stated += 1;
        }
        // `all()` over an empty iterator is true, so a row with no absence list must count as unchecked rather
        // than clean. Fourteen of thirty rows had an empty list and every one of them passed for free.
        if row.absent.is_empty() {
            entry.unchecked += 1;
        } else if row
            .absent
            .iter()
            .all(|forbidden| !contains(&reading.text, forbidden))
        {
            entry.clean += 1;
        }
    }
    for score in scores.values_mut() {
        score.boilerplate /= score.asked.max(1) as f64;
    }
    let _ = gold;
    scores.into_values().collect()
}

/// A gold set, read from JSON lines.
pub fn read_gold(path: &std::path::Path) -> Result<Vec<GoldRow>, String> {
    let text =
        std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut rows = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        let row: GoldRow = serde_json::from_str(line)
            .map_err(|error| format!("{}:{}: {error}", path.display(), number + 1))?;
        rows.push(row);
    }
    Ok(rows)
}

/// `hades read-eval --gold <set.jsonl> [--state <dir>] [--refresh] [--json]`
pub fn cmd(args: &[String]) -> i32 {
    let mut gold: Option<PathBuf> = None;
    let mut state: Option<PathBuf> = None;
    let mut refresh = false;
    let mut json = false;
    let mut limit = usize::MAX;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--gold" => {
                index += 1;
                gold = args.get(index).map(PathBuf::from);
            }
            "--state" => {
                index += 1;
                state = args.get(index).map(PathBuf::from);
            }
            "--refresh" => refresh = true,
            "--json" => json = true,
            "--limit" => {
                index += 1;
                limit = args
                    .get(index)
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(usize::MAX);
            }
            "-h" | "--help" => {
                println!(
                    "hades read-eval --gold <set.jsonl> [--state <dir>] [--refresh] [--limit N] [--json]\n\
                     \n  Reads every URL in the set through the shipped pipeline and reports, per class, how\n  \
                     many extracted texts contained the answering fragment and how many ended in the state the\n  \
                     set expected. A row whose text is missing scores as a miss, never as a pass."
                );
                return 0;
            }
            other => {
                eprintln!("read-eval: unknown argument `{other}`");
                return 2;
            }
        }
        index += 1;
    }
    let Some(gold_path) = gold else {
        eprintln!("read-eval: --gold is required");
        return 2;
    };
    let rows = match read_gold(&gold_path) {
        Ok(rows) => rows,
        Err(error) => {
            eprintln!("read-eval: {error}");
            return 1;
        }
    };
    let mut readings: Vec<(GoldRow, crate::read::Reading)> = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        if i >= limit {
            break;
        }
        let reading = crate::read::read(state.as_deref(), &row.url, refresh, false);
        eprintln!(
            "  {:52} {:12} {:>7} chars  {} ms  {}",
            if row.url.len() > 50 {
                &row.url[..50]
            } else {
                &row.url
            },
            reading.state.to_string(),
            reading.chars(),
            reading.ms,
            if answers(&reading.text, &row.fragments) {
                "found"
            } else {
                "MISS"
            }
        );
        readings.push((row.clone(), reading));
    }
    let scores = score(&rows, &readings);
    if json {
        for row in &scores {
            println!(
                "{{\"class\":\"{}\",\"asked\":{},\"found\":{},\"stated\":{},\"clean\":{},\"chars\":{},\"ms\":{},\"recall\":{:.3},\"states\":{:.3},\"cleanliness\":{:.3},\"boilerplate\":{:.3}}}",
                row.class,
                row.asked,
                row.found,
                row.stated,
                row.clean,
                row.chars,
                row.ms,
                row.recall(),
                row.states(),
                row.cleanliness(),
                row.boilerplate
            );
        }
        return 0;
    }
    println!(
        "{:<10} {:>5} {:>6} {:>7} {:>7} {:>6} {:>9} {:>7}",
        "class", "rows", "found", "state%", "recall", "clean", "unchecked", "boiler"
    );
    for row in &scores {
        println!(
            "{:<10} {:>5} {:>6} {:>6.0}% {:>6.0}% {:>5.0}% {:>9} {:>6.0}%",
            row.class,
            row.asked,
            row.found,
            row.states() * 100.0,
            row.recall() * 100.0,
            row.cleanliness() * 100.0,
            row.unchecked(),
            row.boilerplate * 100.0
        );
    }
    let asked: usize = scores.iter().map(|s| s.asked).sum();
    let found: usize = scores.iter().map(|s| s.found).sum();
    if asked > 0 {
        println!(
            "\noverall recall {:.1}% over {asked} rows",
            100.0 * found as f64 / asked as f64
        );
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(class: &str, expect: &str, fragments: &[&str]) -> GoldRow {
        GoldRow {
            url: "https://example.org/x".to_string(),
            class: class.to_string(),
            expect: expect.to_string(),
            question: "q".to_string(),
            fragments: fragments.iter().map(|f| f.to_string()).collect(),
            absent: Vec::new(),
            note: String::new(),
        }
    }

    fn reading(state: crate::read::State, text: &str) -> crate::read::Reading {
        crate::read::Reading {
            url: "https://example.org/x".to_string(),
            path: crate::read::Path_::Fetch,
            state,
            states: Vec::new(),
            title: String::new(),
            text: text.to_string(),
            blocks: Vec::new(),
            ms: 5,
        }
    }

    #[test]
    fn a_fragment_matches_across_whitespace_and_case() {
        assert!(answers(
            "PRAGMA journal_mode = WAL;",
            &["journal_mode=WAL".to_string()]
        ));
        assert!(answers(
            "The WAL Journal Mode",
            &["wal journal".to_string()]
        ));
        assert!(!answers("nothing here", &["journal_mode=WAL".to_string()]));
        assert!(
            !answers("anything", &[]),
            "an empty fragment list cannot pass"
        );
    }

    #[test]
    fn a_missing_page_scores_as_a_miss_not_a_pass() {
        // The row that would otherwise pass by absence: no text, and no fragment to find in it.
        let gold = vec![row("docs", "ok", &["journal_mode=WAL"])];
        let readings = vec![(gold[0].clone(), reading(crate::read::State::Empty, ""))];
        let scores = score(&gold, &readings);
        assert_eq!(scores[0].found, 0);
        assert_eq!(
            scores[0].stated, 0,
            "an empty page must not satisfy an expected ok"
        );
        assert_eq!(scores[0].recall(), 0.0);
    }

    #[test]
    fn a_named_refusal_satisfies_a_row_that_expected_one() {
        let gold = vec![row("news", "blocked(403)", &["text"])];
        let readings = vec![(
            gold[0].clone(),
            reading(crate::read::State::Blocked(403), ""),
        )];
        let scores = score(&gold, &readings);
        assert_eq!(scores[0].stated, 1);
        assert_eq!(scores[0].found, 0, "a refusal is not an answer");
    }

    #[test]
    fn boilerplate_is_counted_by_line() {
        let text = "Documentation\nDownload\nBlog\nThe real answer is here and it is a sentence.";
        let ratio = boilerplate_ratio(text, &["documentation", "download", "blog"]);
        assert!((ratio - 0.75).abs() < 0.01, "got {ratio}");
    }
}
