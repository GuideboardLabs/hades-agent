//! Search quality measured by the code that ships.
//!
//! The first version of this measurement lived in Python, and it reimplemented the fusion arithmetic, the
//! authority and coverage signals and the grader. That is two implementations of one behaviour: a number
//! produced by the copy says nothing about the product, and the copies had already drifted twice before this
//! module existed. Yahoo measured 0% against one parser and 53% against the other on the same recorded pages;
//! a set-joining bug in the Python scoring silently mixed the held-out queries into the tuning set.
//!
//! So the split is now: a query set names the canonical page, a recording holds what the sources said, and this
//! module scores the recording through [`crate::websearch::fuse`], the same function a live search calls. Fetching
//! lives in `search-eval fetch`, which drives [`crate::websearch::search`] itself.
//!
//! A recording is one JSON object per line:
//!
//! ```json
//! {"set":"docs","query":"sqlite pragma wal mode","source":"yahoo","status":"ok",
//!  "hits":[["Write-Ahead Logging","https://sqlite.org/wal.html"]]}
//! ```
//!
//! A query set is one object per line with `query`, `want` and `frags`, where `want` is the domain and a result
//! grades as correct only when it is on that domain *and* its URL carries one of the fragments. The domain test
//! alone was too generous: it scored a forum post as a hit for `pragma.html`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::websearch::{
    self, Answer, Fusion, Hit, Intent, State, fusion_for, intent_of, weights_for,
};

/// One line of a query set: the query, and what a correct answer looks like.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct GoldRow {
    pub query: String,
    /// The canonical page's domain, e.g. `sqlite.org`.
    pub want: String,
    /// Fragments of the canonical page's own URL; the answer is right when one of them is present.
    #[serde(default)]
    pub frags: Vec<String>,
    /// `docs`, `so` or `entity`. Defaults to `docs` so a one-column file still works.
    #[serde(default = "docs_set")]
    pub set: String,
}

fn docs_set() -> String {
    "docs".to_string()
}

/// One line of a recording: what one source answered for one query.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Recorded {
    pub query: String,
    pub source: String,
    #[serde(default = "docs_set")]
    pub set: String,
    #[serde(default = "ok_state")]
    pub status: String,
    #[serde(default)]
    pub hits: Vec<(String, String)>,
}

fn ok_state() -> String {
    "ok".to_string()
}

/// What one class scored.
#[derive(Debug, Clone, PartialEq)]
pub struct Score {
    pub set: String,
    pub asked: usize,
    pub answered: usize,
    pub hit1: usize,
    pub hit3: usize,
    pub reciprocal: f64,
}

impl Score {
    pub fn mrr(&self) -> f64 {
        if self.answered == 0 {
            0.0
        } else {
            self.reciprocal / self.answered as f64
        }
    }

    pub fn hit_at_1(&self) -> f64 {
        if self.answered == 0 {
            0.0
        } else {
            self.hit1 as f64 / self.answered as f64
        }
    }

    pub fn hit_at_3(&self) -> f64 {
        if self.answered == 0 {
            0.0
        } else {
            self.hit3 as f64 / self.answered as f64
        }
    }
}

/// The intent a gold class names.
fn class_of(set: &str) -> Intent {
    match set {
        "so" => Intent::Troubleshooting,
        "entity" => Intent::Entity,
        "general" => Intent::General,
        _ => Intent::Documentation,
    }
}

/// Grade a ranked list: 1-based rank of the canonical page, or `None` when it is not there.
pub fn grade(rows: &[(String, String)], want: &str, frags: &[String]) -> Option<usize> {
    for (index, (_, url)) in rows.iter().enumerate() {
        let host = websearch::domain(url);
        let on_domain = host == want
            || host.ends_with(&format!(".{want}"))
            || websearch::registrable(&host) == want;
        if !on_domain {
            continue;
        }
        let low = url.to_lowercase();
        if frags.iter().any(|frag| low.contains(&frag.to_lowercase())) {
            return Some(index + 1);
        }
    }
    None
}

/// Map a recorded source name onto the static name the weights table is keyed by.
///
/// A source nobody weighted is asked nothing, which is the same rule a live search follows: an unmeasured source
/// does not get to influence an order just because a recording happened to contain it.
fn known(source: &str) -> Option<&'static str> {
    match source {
        "yahoo" => Some("yahoo"),
        "yahoo-docs" => Some("yahoo-docs"),
        "keenable" => Some("keenable"),
        "bing" => Some("bing"),
        "mwmbl" => Some("mwmbl"),
        "hn" => Some("hn"),
        "stackexchange" => Some("stackexchange"),
        "wikipedia" => Some("wikipedia"),
        "exa" => Some("exa"),
        "parallel" => Some("parallel"),
        _ => None,
    }
}

/// Fuse and grade a recording, per class, through the production [`websearch::fuse`].
pub fn score(
    gold: &[GoldRow],
    recorded: &[Recorded],
    shape_override: Option<Fusion>,
) -> Vec<Score> {
    score_report(gold, recorded, shape_override).0
}

/// The same scoring, plus every query whose live classification disagrees with its gold class.
///
/// Those two facts are worth keeping apart: a page ranked badly and a query filed under the wrong class need
/// different work, and a scorer that silently used its own guess would hide the second behind the first.
pub fn score_report(
    gold: &[GoldRow],
    recorded: &[Recorded],
    shape_override: Option<Fusion>,
) -> (Vec<Score>, Vec<String>) {
    score_with(gold, recorded, shape_override, false)
}

/// `live` decides where the class comes from: the gold label (a measurement of the ranking) or `intent_of` on
/// the query (a measurement of what production will actually do, classifier included).
pub fn score_with(
    gold: &[GoldRow],
    recorded: &[Recorded],
    shape_override: Option<Fusion>,
    live: bool,
) -> (Vec<Score>, Vec<String>) {
    let mut by_query: BTreeMap<(String, String), Vec<&Recorded>> = BTreeMap::new();
    for row in recorded {
        by_query
            .entry((row.set.clone(), row.query.clone()))
            .or_default()
            .push(row);
    }
    let gold_by_query: BTreeMap<(String, String), &GoldRow> = gold
        .iter()
        .map(|row| ((row.set.clone(), row.query.clone()), row))
        .collect();

    let mut misclassified: Vec<String> = Vec::new();
    let mut scores: BTreeMap<String, Score> = BTreeMap::new();
    for (key, rows) in &by_query {
        let Some(target) = gold_by_query.get(key) else {
            continue;
        };
        // The class comes from the gold label, not from re-deriving an intent at scoring time. A measurement
        // that lets the scorer guess the class is measuring the classifier as well as the ranking, and the two
        // failures need different fixes. Disagreement is counted and reported separately.
        let intent = if live {
            intent_of(&target.query)
        } else {
            class_of(&target.set)
        };
        if intent_of(&target.query) != class_of(&target.set) {
            misclassified.push(format!(
                "{} -> {:?} (gold {})",
                target.query,
                intent_of(&target.query),
                target.set
            ));
        }
        let weights = weights_for(intent);
        let shape = shape_override.unwrap_or_else(|| fusion_for(intent));
        let answers: Vec<Answer> = rows
            .iter()
            .filter_map(|row| {
                let source = known(&row.source)?;
                let hits = row
                    .hits
                    .iter()
                    .enumerate()
                    .map(|(index, (title, url))| Hit {
                        title: title.clone(),
                        url: url.clone(),
                        snippet: String::new(),
                        source,
                        rank: index + 1,
                    })
                    .collect();
                let state = if row.status == "ok" {
                    State::Ok
                } else {
                    State::Empty
                };
                Some(Answer {
                    source,
                    hits,
                    state,
                    ms: 0,
                    detail: String::new(),
                })
            })
            .collect();
        let fused = websearch::fuse(&target.query, &answers, &weights, shape, 10);
        let ranked: Vec<(String, String)> = fused
            .iter()
            .map(|row| (row.hit.title.clone(), row.hit.url.clone()))
            .collect();
        let entry = scores.entry(key.0.clone()).or_insert_with(|| Score {
            set: key.0.clone(),
            asked: 0,
            answered: 0,
            hit1: 0,
            hit3: 0,
            reciprocal: 0.0,
        });
        entry.asked += 1;
        if let Some(rank) = grade(&ranked, &target.want, &target.frags) {
            entry.answered += 1;
            entry.reciprocal += 1.0 / rank as f64;
            if rank == 1 {
                entry.hit1 += 1;
            }
            if rank <= 3 {
                entry.hit3 += 1;
            }
        }
    }
    (scores.into_values().collect(), misclassified)
}

/// A query set, read from a JSON-lines file.
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

/// A recording, read from one or more JSON-lines files and merged in argument order.
pub fn read_recorded(paths: &[PathBuf]) -> Result<Vec<Recorded>, String> {
    let mut rows = Vec::new();
    for path in paths {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        for (number, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with("//") {
                continue;
            }
            let row: Recorded = serde_json::from_str(line)
                .map_err(|error| format!("{}:{}: {error}", path.display(), number + 1))?;
            rows.push(row);
        }
    }
    Ok(rows)
}

/// Run the real search over a query set and write what each source said.
///
/// This is the half that must not be reimplemented: a recording produced by anything other than the shipped
/// [`crate::websearch::search`] measures that thing, not the product. The Python harness this replaces did its
/// own fetching and its own parsing, which is how Yahoo came to measure 0% in one place and 53% in another on
/// the same recorded pages.
pub fn fetch(
    gold: &[GoldRow],
    state: Option<&std::path::Path>,
    pause_ms: u64,
) -> Result<String, String> {
    let mut out = String::new();
    for (number, row) in gold.iter().enumerate() {
        if number > 0 && pause_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(pause_ms));
        }
        let report = crate::websearch::search(state, &row.query, 10);
        for answer in &report.answers {
            let hits: Vec<serde_json::Value> = answer
                .hits
                .iter()
                .map(|hit| serde_json::json!([hit.title, hit.url]))
                .collect();
            let record = serde_json::json!({
                "set": row.set,
                "query": row.query,
                "want": row.want,
                "frags": row.frags,
                "source": answer.source,
                "status": match answer.state {
                    State::Ok => "ok",
                    State::Empty => "empty",
                    State::Blocked => "blocked",
                    State::Error => "error",
                },
                "ms": answer.ms,
                "detail": answer.detail,
                "hits": hits,
            });
            out.push_str(&record.to_string());
            out.push('\n');
        }
        eprintln!(
            "  {:48} {:?} {} source(s)",
            if row.query.len() > 46 {
                &row.query[..46]
            } else {
                &row.query
            },
            report.intent,
            report.answers.len()
        );
    }
    Ok(out)
}

/// Every query's rank and the first result it ranked, for when a number needs explaining.
pub fn rank_report(gold: &[GoldRow], recorded: &[Recorded]) -> Vec<String> {
    rank_report_with(gold, recorded, false)
}

pub fn rank_report_with(gold: &[GoldRow], recorded: &[Recorded], live: bool) -> Vec<String> {
    let mut out = Vec::new();
    for target in gold {
        let rows: Vec<&Recorded> = recorded
            .iter()
            .filter(|row| row.set == target.set && row.query == target.query)
            .collect();
        if rows.is_empty() {
            out.push(format!("{:44} no recording", target.query));
            continue;
        }
        let intent = if live {
            intent_of(&target.query)
        } else {
            class_of(&target.set)
        };
        let weights = weights_for(intent);
        let shape = fusion_for(intent);
        let answers: Vec<Answer> = rows
            .iter()
            .filter_map(|row| {
                let source = known(&row.source)?;
                let hits = row
                    .hits
                    .iter()
                    .enumerate()
                    .map(|(index, (title, url))| Hit {
                        title: title.clone(),
                        url: url.clone(),
                        snippet: String::new(),
                        source,
                        rank: index + 1,
                    })
                    .collect();
                Some(Answer {
                    source,
                    hits,
                    state: State::Ok,
                    ms: 0,
                    detail: String::new(),
                })
            })
            .collect();
        let fused = websearch::fuse(&target.query, &answers, &weights, shape, 10);
        let ranked: Vec<(String, String)> = fused
            .iter()
            .map(|row| (row.hit.title.clone(), row.hit.url.clone()))
            .collect();
        let rank = grade(&ranked, &target.want, &target.frags);
        out.push(format!(
            "{:44} rank {:>4} of {:?}   top: {}",
            if target.query.len() > 42 {
                &target.query[..42]
            } else {
                &target.query
            },
            rank.map(|r| r.to_string())
                .unwrap_or_else(|| "-".to_string()),
            target.set,
            ranked
                .first()
                .map(|(_, url)| &url[..url.len().min(58)])
                .unwrap_or("nothing")
        ));
    }
    out
}

/// `hades search-eval fetch --gold <set.jsonl> [--out <recording.jsonl>] [--state <dir>] [--pause-ms N]`
fn fetch_cmd(args: &[String]) -> i32 {
    let mut gold: Option<PathBuf> = None;
    let mut out: Option<PathBuf> = None;
    let mut state: Option<PathBuf> = None;
    let mut pause_ms: u64 = 250;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--gold" => {
                index += 1;
                gold = args.get(index).map(PathBuf::from);
            }
            "--out" => {
                index += 1;
                out = args.get(index).map(PathBuf::from);
            }
            "--state" => {
                index += 1;
                state = args.get(index).map(PathBuf::from);
            }
            "--pause-ms" => {
                index += 1;
                pause_ms = args
                    .get(index)
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(250);
            }
            "-h" | "--help" => {
                println!(
                    "hades search-eval fetch --gold <set.jsonl> [--out <recording.jsonl>] [--state <dir>]\n\
                     \n  Runs the shipped search over every query in the set and writes what each source said.\n  \
                     Without --state the browser source is skipped, which keeps the run offline and headless."
                );
                return 0;
            }
            other => {
                eprintln!("search-eval fetch: unknown argument `{other}`");
                return 2;
            }
        }
        index += 1;
    }
    let Some(gold_path) = gold else {
        eprintln!("search-eval fetch: --gold is required");
        return 2;
    };
    let gold_rows = match read_gold(&gold_path) {
        Ok(rows) => rows,
        Err(error) => {
            eprintln!("search-eval fetch: {error}");
            return 1;
        }
    };
    match fetch(&gold_rows, state.as_deref(), pause_ms) {
        Ok(body) => match out {
            Some(path) => {
                if let Err(error) = std::fs::write(&path, &body) {
                    eprintln!("search-eval fetch: {}: {error}", path.display());
                    return 1;
                }
                eprintln!("{} rows -> {}", body.lines().count(), path.display());
                0
            }
            None => {
                print!("{body}");
                0
            }
        },
        Err(error) => {
            eprintln!("search-eval fetch: {error}");
            1
        }
    }
}

/// `hades search-eval --gold <set.jsonl> --hits <recording.jsonl> [--hits ...]`
pub fn cmd(args: &[String]) -> i32 {
    if args.first().map(String::as_str) == Some("fetch") {
        return fetch_cmd(&args[1..]);
    }
    let mut gold: Option<PathBuf> = None;
    let mut hits: Vec<PathBuf> = Vec::new();
    let mut json = false;
    let mut verbose = false;
    let mut live = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--gold" => {
                index += 1;
                gold = args.get(index).map(PathBuf::from);
            }
            "--hits" => {
                index += 1;
                if let Some(path) = args.get(index) {
                    hits.push(PathBuf::from(path));
                }
            }
            "--json" => json = true,
            "--verbose" => verbose = true,
            "--live-class" => live = true,
            "-h" | "--help" => {
                println!(
                    "hades search-eval --gold <set.jsonl> --hits <recording.jsonl> [--hits ...] [--json]\n\
                     \n  Scores a recording through the same fusion a live search uses: hit@1, hit@3 and MRR\n  \
                     per query class, where a hit means the canonical page is on the named domain and its URL\n  \
                     carries one of the set's fragments."
                );
                return 0;
            }
            other => {
                eprintln!("search-eval: unknown argument `{other}`");
                return 2;
            }
        }
        index += 1;
    }
    let Some(gold_path) = gold else {
        eprintln!("search-eval: --gold is required");
        return 2;
    };
    if hits.is_empty() {
        eprintln!("search-eval: at least one --hits recording is required");
        return 2;
    }
    let gold_rows = match read_gold(&gold_path) {
        Ok(rows) => rows,
        Err(error) => {
            eprintln!("search-eval: {error}");
            return 1;
        }
    };
    let recorded = match read_recorded(&hits) {
        Ok(rows) => rows,
        Err(error) => {
            eprintln!("search-eval: {error}");
            return 1;
        }
    };
    let (scores, misclassified) = score_with(&gold_rows, &recorded, None, live);
    if verbose {
        for line in rank_report_with(&gold_rows, &recorded, live) {
            eprintln!("{line}");
        }
    }
    if json {
        let body = scores
            .iter()
            .map(|row| {
                format!(
                    "{{\"set\":\"{}\",\"asked\":{},\"answered\":{},\"hit1\":{},\"hit3\":{},\"mrr\":{:.4}}}",
                    row.set,
                    row.asked,
                    row.answered,
                    row.hit1,
                    row.hit3,
                    row.mrr()
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        println!("{body}");
        return 0;
    }
    if !misclassified.is_empty() {
        println!(
            "note: {} query/queries classify differently live than their gold class:",
            misclassified.len()
        );
        for line in &misclassified {
            println!("  {line}");
        }
        println!();
    }
    println!(
        "{:<8} {:>5} {:>5} {:>6} {:>6} {:>6}",
        "class", "asked", "found", "hit@1", "hit@3", "mrr"
    );
    for row in &scores {
        println!(
            "{:<8} {:>5} {:>5} {:>5.0}% {:>5.0}% {:>6.3}",
            row.set,
            row.asked,
            row.answered,
            row.hit_at_1() * 100.0,
            row.hit_at_3() * 100.0,
            row.mrr()
        );
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recording(set: &str, query: &str, source: &str, hits: &[(&str, &str)]) -> Recorded {
        Recorded {
            query: query.to_string(),
            source: source.to_string(),
            set: set.to_string(),
            status: "ok".to_string(),
            hits: hits
                .iter()
                .map(|(t, u)| (t.to_string(), u.to_string()))
                .collect(),
        }
    }

    fn gold(set: &str, query: &str, want: &str, frags: &[&str]) -> GoldRow {
        GoldRow {
            query: query.to_string(),
            want: want.to_string(),
            frags: frags.iter().map(|f| f.to_string()).collect(),
            set: set.to_string(),
        }
    }

    #[test]
    fn a_hit_needs_the_domain_and_the_page() {
        // The domain alone graded a forum post as the answer for `pragma.html`, which is how a forum thread beat
        // the reference page for a whole release without anyone noticing.
        let rows = vec![
            (
                "forum".to_string(),
                "https://sqlite.org/forum/info/a1ca".to_string(),
            ),
            ("wal".to_string(), "https://sqlite.org/wal.html".to_string()),
        ];
        assert_eq!(grade(&rows, "sqlite.org", &["wal".to_string()]), Some(2));
        assert_eq!(grade(&rows, "sqlite.org", &["pragma".to_string()]), None);
        assert_eq!(grade(&rows, "example.org", &["wal".to_string()]), None);
    }

    #[test]
    fn a_recording_scores_through_the_production_fusion() {
        // One query, two sources, the canonical page first only in one of them. The fused order must put it on
        // top and the score must say hit@1.
        let gold = vec![gold(
            "docs",
            "sqlite pragma wal mode",
            "sqlite.org",
            &["wal"],
        )];
        let recorded = vec![
            recording(
                "docs",
                "sqlite pragma wal mode",
                "yahoo",
                &[
                    ("WAL", "https://sqlite.org/wal.html"),
                    ("Blog", "https://blog.example/sqlite"),
                ],
            ),
            recording(
                "docs",
                "sqlite pragma wal mode",
                "mwmbl",
                &[("Blog", "https://blog.example/sqlite")],
            ),
        ];
        let scores = score(&gold, &recorded, None);
        assert_eq!(scores.len(), 1);
        assert_eq!(scores[0].asked, 1);
        assert_eq!(scores[0].hit1, 1);
        assert_eq!(scores[0].mrr(), 1.0);
    }

    #[test]
    fn a_query_outside_the_gold_set_is_not_scored() {
        // A recording holds whatever was fetched. Scoring a query the set does not name would invent an answer
        // key out of the first row it saw, which is how the Python harness mixed two sets together.
        let gold = vec![gold("docs", "a", "a.example", &["a"])];
        let recorded = vec![
            recording("docs", "a", "yahoo", &[("A", "https://a.example/a")]),
            recording("docs", "b", "yahoo", &[("B", "https://b.example/b")]),
        ];
        let scores = score(&gold, &recorded, None);
        assert_eq!(scores[0].asked, 1);
    }

    #[test]
    fn a_source_without_a_weight_is_not_counted() {
        // An unmeasured source must not influence an order just because a recording contains it.
        let gold = vec![gold(
            "docs",
            "sqlite pragma wal mode",
            "sqlite.org",
            &["wal"],
        )];
        let recorded = vec![recording(
            "docs",
            "sqlite pragma wal mode",
            "some-unweighted-engine",
            &[("WAL", "https://sqlite.org/wal.html")],
        )];
        let scores = score(&gold, &recorded, None);
        assert_eq!(
            scores[0].answered, 0,
            "nothing was asked, so nothing can be found"
        );
    }
}
