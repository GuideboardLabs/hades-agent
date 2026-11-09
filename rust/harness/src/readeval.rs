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
/// Whether any line of the text STARTS with the forbidden string.
///
/// Not `contains`. The review pulled the extracted text for every checked row and printed the match in context:
/// `Donate` matched "Please donate.", `Retrieved from` matched "can be retrieved from the remainder function",
/// `cookie` matched `$cookie_nocache`, `Talk` matched "The final talk at Kangrejos", `Categories:` matched "we divide
/// HTTP headers into two categories:". Thirteen of sixteen matches were ordinary prose, so the figure counted word
/// frequency. Chrome is placed at the start of a line; prose mentions the word inside one.
pub fn contains(text: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    text.lines().any(|line| line_is_chrome(line, needle))
}

/// Whether one line carries the marker as a LABEL rather than as a word opening a sentence.
///
/// Worked on the raw line, not the squashed one: whitespace and quotes are exactly the evidence that separates
/// "Retrieved from \"https://example.org/x\"" (a footer label) from "Retrieved from the remainder function of the
/// iterator" (prose). The tail is stripped of quoted segments and URL tokens before its punctuation is judged,
/// because a URL is full of full stops and a quoted citation is not a sentence.
fn line_is_chrome(line: &str, needle: &str) -> bool {
    // `Categories :` with a space before the colon is how Wikipedia renders it, so the marker is searched in a
    // compacted copy of the line rather than the literal one.
    let compact = line.replace(" :", ":").replace("  ", " ");
    let leading_trimmed =
        compact.trim_start_matches(|c: char| !c.is_alphanumeric() && c != '"' && c != '\'');
    let low = leading_trimmed.to_lowercase();
    let wanted = needle.trim().to_lowercase();
    let Some(at) = low.find(&wanted) else {
        return false;
    };
    // the marker must START the line, not merely appear in it
    if at != 0 {
        return false;
    }
    let rest = &leading_trimmed[wanted.len()..];
    let prose = tail_is_prose(rest);
    !prose
}

fn tail_is_prose(rest: &str) -> bool {
    let mut visible = String::new();
    let mut in_quote = false;
    for c in rest.chars() {
        if c == '"' || c == '\'' {
            in_quote = !in_quote;
            continue;
        }
        if !in_quote {
            visible.push(c);
        }
    }
    // a URL token is not a sentence: drop anything with a scheme or a slash
    let words: Vec<&str> = visible
        .split_whitespace()
        .filter(|word| !word.contains("://") && !word.contains('/'))
        .collect();
    let tail = words.join(" ");
    let tail = tail.trim_start_matches(|c: char| !c.is_alphanumeric());
    if tail.contains('.') || tail.contains('?') || tail.contains('!') {
        return true;
    }
    // A label tail is a few words. "the remainder function of the iterator" is 39 characters and is prose, so the
    // bound is 25: "https://example.org/x" is dropped as a URL token before this, and "Database algorithms" fits.
    tail.chars().count() > 25
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
        // A row whose reading produced no text at all cannot be clean: no assertion was exercised, because there
        // was nothing to assert against. The pdf class scored 2/2 clean while one of those rows never resolved.
        if row.absent.is_empty() || reading.text.trim().is_empty() {
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
                "cleanliness": format!("{:.0}%", score.cleanliness() * 100.0),
                "boiler": format!("{:.0}%", score.boilerplate * 100.0),
            })
        })
        .collect();
    let asked: usize = scores.iter().map(|s| s.asked).sum();
    let found: usize = scores.iter().map(|s| s.found).sum();
    let stated: usize = scores.iter().map(|s| s.stated).sum();
    let clean: usize = scores.iter().map(|s| s.clean).sum();
    let checked: usize = scores.iter().map(|s| s.checked()).sum();
    let unchecked: usize = scores.iter().map(|s| s.unchecked).sum();
    let boiler: f64 =
        scores.iter().map(|s| s.boilerplate).sum::<f64>() / scores.len().max(1) as f64;
    // The aggregate is saved as a class named `all`, so it is a guarded row like any other. Leaving it out of the
    // baseline left `5/16` and `73.3%` unguarded, and the invented figure in 0.6.2 lived in exactly that row.
    let mut rows = rows;
    rows.push(serde_json::json!({
        "class": "all",
        "rows": asked,
        "found": found,
        "stated": stated,
        "clean": clean,
        "checked": checked,
        "unchecked": unchecked,
        "recall": format!("{:.1}%", 100.0 * found as f64 / asked.max(1) as f64),
        "states": format!("{:.0}%", 100.0 * stated as f64 / asked.max(1) as f64),
        "cleanliness": format!("{clean}/{checked}"),
        "boiler": format!("{:.0}%", boiler * 100.0),
    }));
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

    // The newest section is the first `## ` header in the file. Searching for "\n## " found the SECOND section when
    // the file opens with a header, which is how a tampered row passed the check the first time it ran.
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

    let classes = saved
        .get("classes")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    // A guard that fails open on absence is not a guard: an empty baseline, or a notes section with no table, used
    // to report ok. Both are failures now.
    if classes.is_empty() {
        return Err("the saved measurement has no rows, so nothing was checked".to_string());
    }
    let mut problems = Vec::new();

    // Shape one: a row in the notes that the measurement does not record is unguarded.
    let recorded: Vec<String> = classes
        .iter()
        .filter_map(|c| c.get("class").and_then(|v| v.as_str()).map(str::to_string))
        .collect();
    // The separator has to be recognised by its CELLS, not by the line: `| mystery | --- | 999 |` is a data row with
    // a dashed second cell, and skipping any line containing `---` let it through as a recorded class.
    let is_separator = |line: &str| {
        line.trim_start().starts_with("| ")
            && line
                .split('|')
                .map(str::trim)
                .filter(|cell| !cell.is_empty())
                .all(|cell| !cell.is_empty() && cell.chars().all(|c| c == '-' || c == ':'))
    };
    let table_rows: Vec<String> = newest
        .lines()
        .filter(|line| line.trim_start().starts_with("| ") && !is_separator(line))
        .filter_map(|line| {
            line.split('|')
                .nth(1)
                .map(|cell| cell.trim().replace('*', ""))
        })
        .filter(|cell| !cell.is_empty() && cell != "class")
        .collect();
    for row in &table_rows {
        if !recorded.iter().any(|name| name == row) {
            problems.push(format!(
                "{row}: the notes have a row the measurement does not record"
            ));
        }
    }

    for class in &classes {
        let name = class.get("class").and_then(|v| v.as_str()).unwrap_or("");
        // Shape three: a class that matches more than one row means the first is taken and the rest are unguarded.
        let matches: Vec<&str> = newest
            .lines()
            .filter(|line| {
                let trimmed = line.trim_start();
                trimmed.starts_with("| ")
                    && trimmed
                        .split('|')
                        .nth(1)
                        .map(|cell| cell.trim().replace('*', "") == name)
                        .unwrap_or(false)
            })
            .collect();
        if matches.len() > 1 {
            problems.push(format!(
                "{name}: the notes table has {} rows for this class",
                matches.len()
            ));
            continue;
        }
        let Some(line) = matches.first().copied() else {
            problems.push(format!("{name}: the notes table has no row for this class"));
            continue;
        };
        let cells: Vec<String> = line
            .split('|')
            .map(|cell| cell.trim().to_string())
            .filter(|cell| !cell.is_empty())
            .collect();
        // Compared BY POSITION, not by presence. Presence-checking let a mutation through: changing the docs
        // cleanliness from 100% to 99% still found a "100%" in the row, in the state or recall column. The table
        // has a fixed column order, so cell i is compared to the value for column i:
        //   class | rows | found | state% | recall | clean | unchecked | boiler
        let expected = [
            class.get("rows").map(|v| v.to_string()).unwrap_or_default(),
            class
                .get("found")
                .map(|v| v.to_string())
                .unwrap_or_default(),
            class
                .get("states")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            class
                .get("recall")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            class
                .get("cleanliness")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            class
                .get("unchecked")
                .map(|v| v.to_string())
                .unwrap_or_default(),
            class
                .get("boiler")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        ];
        let labels = [
            "rows",
            "found",
            "state%",
            "recall",
            "clean",
            "unchecked",
            "boiler",
        ];
        // cells[0] is the class name; the rest are the columns, in order
        let present: Vec<String> = cells.iter().skip(1).map(|c| c.replace('*', "")).collect();
        if present.len() > expected.len() {
            problems.push(format!(
                "{name}: the row has {} columns, the measurement records {}",
                present.len() + 1,
                expected.len() + 1
            ));
            continue;
        }
        if present.len() < expected.len() {
            problems.push(format!(
                "{name}: the row has {} columns, the measurement records {}",
                present.len() + 1,
                expected.len() + 1
            ));
            continue;
        }
        for (index, (want, label)) in expected.iter().zip(labels).enumerate() {
            if want.is_empty() || want == "null" {
                continue;
            }
            if present.get(index) != Some(want) {
                problems.push(format!(
                    "{name}: {label} is {:?} in the notes, {want:?} in the measurement",
                    present.get(index).cloned().unwrap_or_default()
                ));
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
    let mut emit_table = false;
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
            "--emit-table" => emit_table = true,
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
    if emit_table {
        // The release notes carry this table. Printing it here means the notes are a copy of a command's output
        // rather than a transcription of one, which is the defect that put a 4 on 0.6.8.
        println!("| class | rows | found | state% | recall | clean | unchecked | boiler |");
        println!("|---|---|---|---|---|---|---|---|");
        for score in &scores {
            println!(
                "| {} | {} | {} | {:.0}% | {:.1}% | {:.0}% | {} | {:.0}% |",
                score.class,
                score.asked,
                score.found,
                score.states() * 100.0,
                score.recall() * 100.0,
                score.cleanliness() * 100.0,
                score.unchecked,
                score.boilerplate * 100.0
            );
        }
        let asked: usize = scores.iter().map(|s| s.asked).sum();
        let found: usize = scores.iter().map(|s| s.found).sum();
        let stated: usize = scores.iter().map(|s| s.stated).sum();
        let clean: usize = scores.iter().map(|s| s.clean).sum();
        let checked: usize = scores.iter().map(|s| s.checked()).sum();
        let unchecked: usize = scores.iter().map(|s| s.unchecked).sum();
        let boiler = scores.iter().map(|s| s.boilerplate).sum::<f64>() / scores.len().max(1) as f64;
        println!(
            "| all | {} | {} | {:.0}% | {:.1}% | {}/{} | {} | {:.0}% |",
            asked,
            found,
            100.0 * stated as f64 / asked.max(1) as f64,
            100.0 * found as f64 / asked.max(1) as f64,
            clean,
            checked,
            unchecked,
            boiler * 100.0
        );
        return 0;
    }
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
        // The all row is printed, not computed by whoever writes the notes. A figure the tool does not print is a
        // figure the notes can invent, and 0.6.2 invented one. Every cell here is the same kind of cell as the class
        // rows above it, boiler included, and it is the row `--check-notes` must guard hardest.
        let boiler = scores.iter().map(|s| s.boilerplate).sum::<f64>() / scores.len().max(1) as f64;
        println!(
            "{:<10} {:>5} {:>6} {:>7} {:>7} {:>6} {:>9} {:>7}",
            "all",
            asked,
            found,
            format!("{:.0}%", 100.0 * stated as f64 / asked as f64),
            format!("{:.1}%", 100.0 * found as f64 / asked as f64),
            format!("{clean}/{checked}"),
            unchecked,
            format!("{:.0}%", boiler * 100.0)
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
        // The 0.6.2 defect, made impossible, and then the 0.6.4 weakness too: the first version of this test let a
        // mutation through because it compared by presence. Cells are compared by position now, so changing one
        // percentage is caught even when the same digits appear in another column.
        let dir = std::env::temp_dir().join(format!("read-eval-notes-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let baseline = dir.join("baseline.json");
        std::fs::write(
            &baseline,
            r#"{"classes":[
                {"class":"forum","rows":4,"found":3,"states":"75%","recall":"75.0%","cleanliness":"0%","unchecked":4,"boiler":"0%"},
                {"class":"all","rows":4,"found":3,"states":"75%","recall":"75.0%","cleanliness":"0/4","unchecked":4,"boiler":"0%"}]}"#,
        )
        .unwrap();
        let header = "| class | rows | found | state% | recall | clean | unchecked | boiler |\n|---|---|---|---|---|---|---|---|\n";
        let honest_row = "| forum | 4 | 3 | 75% | 75.0% | 0% | 4 | 0% |\n";
        let all_row = "| all | 4 | 3 | 75% | 75.0% | 0/4 | 4 | 0% |\n";

        let honest = dir.join("honest.md");
        std::fs::write(
            &honest,
            format!("## 1.0.0\n\n{header}{honest_row}{all_row}"),
        )
        .unwrap();
        assert!(
            check_notes(&honest, &baseline).is_ok(),
            "an honest table must pass: {:?}",
            check_notes(&honest, &baseline)
        );

        // one percentage changed, while the same digits still appear in the state and recall columns
        let lying = dir.join("lying.md");
        std::fs::write(
            &lying,
            format!(
                "## 1.0.0\n\n{header}{}{all_row}",
                honest_row.replace("| 0% |", "| 99% |")
            ),
        )
        .unwrap();
        let why = check_notes(&lying, &baseline).unwrap_err();
        assert!(
            why.contains("forum"),
            "the failure must name the class: {why}"
        );

        // a row that is simply absent is a failure, not a pass
        let missing = dir.join("missing.md");
        std::fs::write(&missing, format!("## 1.0.0\n\n{header}{all_row}")).unwrap();
        assert!(
            check_notes(&missing, &baseline).is_err(),
            "a missing row must fail"
        );

        // an empty baseline checked nothing, so it cannot report ok
        let empty = dir.join("empty.json");
        std::fs::write(&empty, r#"{"classes":[]}"#).unwrap();
        assert!(
            check_notes(&honest, &empty).is_err(),
            "an empty measurement must fail"
        );

        // the newest section is the one compared, not the second one in the file
        let two = dir.join("two.md");
        std::fs::write(
            &two,
            format!(
                "## 2.0.0\n\n{header}{}{header}{honest_row}",
                honest_row.replace("| 3 |", "| 9 |")
            ),
        )
        .unwrap();
        assert!(
            check_notes(&two, &baseline).is_err(),
            "the newest section is the one compared"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_marker_must_be_a_label_not_a_word_that_opens_a_sentence() {
        // The distinction the sixth review asked for: a nav label ends, or continues into a short tail with no
        // sentence punctuation. Prose runs on.
        assert!(
            contains("Show source", "Show source"),
            "a bare label matches"
        );
        assert!(
            contains("- Show Source", "Show source"),
            "a bulleted label matches"
        );
        assert!(
            contains("Retrieved from \"https://example.org/x\"", "Retrieved from"),
            "a short tail is a label"
        );
        assert!(contains("Categories: - Database algorithms", "Categories:"));

        // prose that happens to begin with the marker is not chrome
        assert!(
            !contains(
                "Retrieved from the remainder function of the iterator",
                "Retrieved from"
            ),
            "a sentence that opens with the marker is prose"
        );
        assert!(!contains("Please donate.", "Donate"));
        assert!(!contains(
            "The final talk at Kangrejos was about the removal of the old API",
            "The final talk"
        ));
    }

    #[test]
    fn a_marker_inside_a_bullet_or_a_quote_still_counts_as_chrome() {
        // The two strings the 0.6.5 release claimed to catch and did not.
        assert!(
            contains("- Show Source", "Show Source"),
            "a bulleted marker must match"
        );
        assert!(
            contains("            Cookie\" as a special case", "Cookie"),
            "a marker after leading space and a quote must match"
        );
        assert!(contains(
            "Retrieved from \"https://example.org/x\"",
            "Retrieved from"
        ));
        assert!(contains(
            "Categories : - Database algorithms",
            "Categories:"
        ));

        // prose must not match, which is the half the earlier check got wrong
        assert!(!contains("Please donate.", "Donate"));
        assert!(!contains("proxy_cache_bypass $cookie_nocache", "cookie"));
        assert!(!contains("The final talk at Kangrejos 2026", "Talk"));
        assert!(!contains(
            "we divide HTTP headers into two categories:",
            "Categories:"
        ));
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
