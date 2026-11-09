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

    /// Rows that carried an absence list, so a check was actually made.
    pub fn checked(&self) -> usize {
        self.asked - self.unchecked
    }

    /// The proportion of CHECKED rows whose body carried none of the strings the gold says must be absent.
    ///
    /// The denominator is the rows that were checked, not all rows: dividing by `asked` let fourteen unchecked rows
    /// dilute a figure the notes then quoted as if it were a measurement of every row.
    pub fn cleanliness(&self) -> f64 {
        if self.checked() == 0 {
            0.0
        } else {
            self.clean as f64 / self.checked() as f64
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

/// Write the measured totals to a file, so a later check can compare against a measurement rather than a memory.
fn save(path: &std::path::Path, scores: &[Score]) -> Result<(), String> {
    let rows: Vec<serde_json::Value> = scores
        .iter()
        .map(|score| {
            serde_json::json!({
                "class": score.class,
                "rows": score.asked,
                "found": score.found,
                "stated": score.stated,
                "clean": score.clean,
                "checked": score.checked(),
                "unchecked": score.unchecked,
                "recall": format!("{:.1}%", score.recall() * 100.0),
                "states": format!("{:.0}%", score.states() * 100.0),
            })
        })
        .collect();
    let document = serde_json::json!({ "classes": rows });
    std::fs::write(
        path,
        serde_json::to_string_pretty(&document).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("{}: {e}", path.display()))
}

/// Compare the newest release section's measurement table against a saved measurement.
///
/// The defect this closes: 0.6.2 shipped notes claiming cleanliness 100% while the binary reported 63.3%, because
/// the table column the figure came from never existed and nothing compared the two. This runs offline, from the
/// files, so the gate can call it on every build. A note that disagrees with the recorded measurement fails.
fn check_notes(notes: &std::path::Path, baseline: &std::path::Path) -> Result<(), String> {
    let baseline_text =
        std::fs::read_to_string(baseline).map_err(|e| format!("{}: {e}", baseline.display()))?;
    let saved: serde_json::Value =
        serde_json::from_str(&baseline_text).map_err(|e| format!("{}: {e}", baseline.display()))?;
    let notes_text =
        std::fs::read_to_string(notes).map_err(|e| format!("{}: {e}", notes.display()))?;
    // The newest section is the first `## ` header in the file. Searching for "\n## " found the SECOND section
    // when the file opens with a header, which is how a tampered row passed the check the first time it ran.
    let start = notes_text
        .lines()
        .scan(0usize, |offset, line| {
            let at = *offset;
            *offset += line.len() + 1;
            Some((at, line))
        })
        .find(|(_, line)| line.starts_with("## "))
        .map(|(at, _)| at)
        .unwrap_or(0);
    let newest = &notes_text[start..];
    let newest = match newest[3..].find("\n## ") {
        Some(end) => &newest[..end + 3],
        None => newest,
    };
    let mut problems = Vec::new();
    for class in saved
        .get("classes")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default()
    {
        let name = class.get("class").and_then(|v| v.as_str()).unwrap_or("");
        let found = class.get("found").and_then(|v| v.as_u64()).unwrap_or(0);
        let rows = class.get("rows").and_then(|v| v.as_u64()).unwrap_or(0);
        // a table row that names the class must state the same found/rows
        if let Some(line) = newest.lines().find(|line| {
            let low = line.to_lowercase();
            low.starts_with("| ") && low.contains(&name.to_lowercase())
        }) {
            let cells: Vec<&str> = line
                .split('|')
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .collect();
            if cells.len() >= 3 {
                let noted_rows: u64 = cells[1].parse().unwrap_or(u64::MAX);
                let noted_found: u64 = cells[2].parse().unwrap_or(u64::MAX);
                if noted_rows != rows || noted_found != found {
                    problems.push(format!(
                        "{name}: notes say {noted_found} of {noted_rows}, the measurement says {found} of {rows}"
                    ));
                }
            }
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("; "))
    }
}

/// `hades read-eval --gold <set.jsonl> [--state <dir>] [--refresh] [--json]`
pub fn cmd(args: &[String]) -> i32 {
    let mut gold: Option<PathBuf> = None;
    let mut state: Option<PathBuf> = None;
    let mut refresh = false;
    let mut json = false;
    let mut limit = usize::MAX;
    let mut save_path: Option<PathBuf> = None;
    let mut notes_path: Option<PathBuf> = None;
    let mut baseline_path: Option<PathBuf> = None;
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
            "--save" => {
                index += 1;
                save_path = args.get(index).map(PathBuf::from);
            }
            "--check-notes" => {
                index += 1;
                notes_path = args.get(index).map(PathBuf::from);
                index += 1;
                baseline_path = args.get(index).map(PathBuf::from);
            }
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
    // A check needs no gold set and no network: it compares two files.
    if let (Some(notes), Some(baseline)) = (notes_path.as_ref(), baseline_path.as_ref()) {
        return match check_notes(notes, baseline) {
            Ok(()) => {
                println!("ok   the newest release notes match the saved measurement");
                0
            }
            Err(why) => {
                eprintln!("notes disagree with the measurement: {why}");
                1
            }
        };
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
    if let Some(path) = save_path.as_ref() {
        if let Err(why) = save(path, &scores) {
            eprintln!("read-eval: {why}");
            return 1;
        }
        eprintln!("saved the measurement to {}", path.display());
    }
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
    let stated: usize = scores.iter().map(|s| s.stated).sum();
    let clean: usize = scores.iter().map(|s| s.clean).sum();
    let checked: usize = scores.iter().map(|s| s.checked()).sum();
    let unchecked: usize = scores.iter().map(|s| s.unchecked).sum();
    if asked > 0 {
        // The all-row is printed, not computed by whoever writes the notes. A figure the tool does not print is a
        // figure the notes can invent, and 0.6.2 invented one.
        println!(
            "\n{:<10} {:>5} {:>6} {:>7} {:>7} {:>6} {:>9}",
            "all",
            asked,
            found,
            format!("{:.0}%", 100.0 * stated as f64 / asked as f64),
            format!("{:.1}%", 100.0 * found as f64 / asked as f64),
            format!("{clean}/{checked}"),
            unchecked
        );
        println!(
            "overall recall {:.1}% over {asked} rows",
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
    fn a_release_note_that_disagrees_with_the_measurement_fails() {
        // The 0.6.2 defect, made impossible: notes claiming a figure the measurement does not contain must fail.
        // Falsified by hand first (a digit was changed, the check failed, it was reverted), then locked here.
        let dir = std::env::temp_dir().join(format!("read-eval-notes-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let baseline = dir.join("baseline.json");
        std::fs::write(
            &baseline,
            r#"{"classes":[{"class":"forum","rows":4,"found":3,"stated":3,"clean":0,"checked":0,"unchecked":4}]}"#,
        )
        .unwrap();

        let honest = dir.join("honest.md");
        std::fs::write(
            &honest,
            "## 0.6.4\n\n| class | rows | found |\n|---|---|---|\n| forum | 4 | 3 |\n",
        )
        .unwrap();
        assert!(
            check_notes(&honest, &baseline).is_ok(),
            "an honest table must pass"
        );

        let lying = dir.join("lying.md");
        std::fs::write(
            &lying,
            "## 0.6.4\n\n| class | rows | found |\n|---|---|---|\n| forum | 4 | 4 |\n",
        )
        .unwrap();
        let why = check_notes(&lying, &baseline).unwrap_err();
        assert!(
            why.contains("forum"),
            "the failure must name the class: {why}"
        );
        assert!(
            why.contains("3 of 4"),
            "the failure must state the measurement: {why}"
        );

        // the newest section is the one checked, not the second one in the file
        let two_sections = dir.join("two.md");
        std::fs::write(
            &two_sections,
            "## 0.6.4\n\n| forum | 4 | 9 |\n## 0.6.3\n\n| forum | 4 | 3 |\n",
        )
        .unwrap();
        assert!(
            check_notes(&two_sections, &baseline).is_err(),
            "the newest section is the one compared"
        );
        let _ = std::fs::remove_dir_all(&dir);
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
