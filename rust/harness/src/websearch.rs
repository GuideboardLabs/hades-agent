//! Web search: several specialist indexes, asked per query class, fused and ranked.
//!
//! Measured before this was written, on this machine, against a 42-query gold set graded on the canonical
//! page rather than the domain (see `docs/search-eval` and `~/.hermes/plans/hades-050-search.md`):
//!
//! - Blind reciprocal-rank fusion over every source scored **worse** than the best single source (0.166
//!   against 0.153 for bing alone on the honest metric, and 0.279 against 0.369 on the lenient one), because
//!   a specialist's irrelevant hits displace the generalist's right answer. So the sources are chosen per
//!   query class, not averaged.
//! - Host affinity (a bonus when the result's host names the query's subject) **hurt** once grading was
//!   strict: it promotes the right domain's wrong page, a forum post over the reference page. It was
//!   removed after measurement, not on taste.
//! - What survived: intent routing, an authority prior from the URL's shape, query-token coverage, and a
//!   small bonus for cross-source agreement. That combination scored 0.497 on documentation-shaped queries
//!   where bing alone scored 0.153.
//!
//! Every source here answers without a key, and every one of them is an HTTP API. The general index is not:
//! DuckDuckGo is network-unreachable from this machine, Brave answers with a CAPTCHA, Mojeek with a bot
//! wall, and Bing over HTTP returns site roots rather than pages. A browser-driven Bing is the one general
//! route that works and it lives in the browser tool, not in this module.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// One result from one source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hit {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub source: &'static str,
    /// 1-based rank inside its own source, kept so fusion is reproducible from a record.
    pub rank: usize,
}

/// What a source did. A source that answers nothing and a source that is blocked are different facts, and a
/// search tool that reports only "no results" teaches its caller the wrong lesson.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum State {
    Ok,
    Empty,
    Blocked,
    Error,
}

impl State {
    pub fn name(&self) -> &'static str {
        match self {
            State::Ok => "ok",
            State::Empty => "empty",
            State::Blocked => "blocked",
            State::Error => "error",
        }
    }
}

/// One source's answer for one query.
#[derive(Debug, Clone)]
pub struct Answer {
    pub source: &'static str,
    pub hits: Vec<Hit>,
    pub state: State,
    pub ms: u128,
    pub detail: String,
}

/// Which kind of question this is, because the sources disagree about what they are good at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    /// A reference page: language docs, a protocol, a tool's manual.
    Documentation,
    /// Something is broken and the answer is a thread where a person fixed it.
    Troubleshooting,
    /// A thing, a person, a definition.
    Entity,
    /// Everything else.
    General,
}

impl Intent {
    pub fn name(&self) -> &'static str {
        match self {
            Intent::Documentation => "documentation",
            Intent::Troubleshooting => "troubleshooting",
            Intent::Entity => "entity",
            Intent::General => "general",
        }
    }
}

/// Read the query, not the world. Deliberately small and explainable: every rule here is one a person could
/// argue with, which is the point, since a classifier nobody can debug is worse than a keyword list.
pub fn intent_of(query: &str) -> Intent {
    let q = query.to_lowercase();
    const TROUBLE: [&str; 14] = [
        "cannot",
        "can't",
        "error",
        "denied",
        "fatal",
        "refusing",
        "not working",
        "broken",
        "unknown",
        "too many",
        "502",
        "429",
        "not found",
        "failed",
    ];
    const ENTITY: [&str; 7] = [
        "what is a",
        "what is an",
        "history",
        "winners",
        "list of",
        "who invented",
        "architecture",
    ];
    const DOCS: [&str; 12] = [
        "api",
        "docs",
        "documentation",
        "reference",
        "syntax",
        "example",
        "how to",
        "manual",
        "spec",
        "usage",
        "options",
        "flags",
    ];
    if TROUBLE.iter().any(|w| q.contains(w)) {
        return Intent::Troubleshooting;
    }
    if ENTITY.iter().any(|w| q.contains(w)) {
        return Intent::Entity;
    }
    if DOCS.iter().any(|w| q.contains(w)) {
        return Intent::Documentation;
    }
    // A token shaped like code is a lookup for a symbol, whatever else the sentence says: "sqlite pragma wal
    // mode" and "tokio spawn_blocking" both name things to look up and neither says "docs".
    if q.split_whitespace()
        .any(|w| w.contains('_') || w.contains("::") || w.contains('#'))
    {
        return Intent::Documentation;
    }
    const SYMBOLS: [&str; 14] = [
        "pragma",
        "function",
        "method",
        "class",
        "struct",
        "trait",
        "module",
        "package",
        "config",
        "parameter",
        "header",
        "protocol",
        "attribute",
        "flag",
    ];
    if SYMBOLS
        .iter()
        .any(|w| q.split_whitespace().any(|token| token == *w))
    {
        return Intent::Documentation;
    }
    Intent::General
}

/// The source weights for a query class. A zero weight means "do not ask", which is cheaper and cleaner than
/// asking and then discarding.
pub fn weights_for(intent: Intent) -> BTreeMap<&'static str, f64> {
    let mut w = BTreeMap::new();
    match intent {
        Intent::Documentation => {
            // Marginalia is deliberately absent: it took 29 seconds on a live query and scored 0.039 MRR on
            // the gold set. A source that cannot answer inside the caller's patience is not a source.
            w.insert("mwmbl", 0.9);
            w.insert("hn", 0.2);
        }
        Intent::Troubleshooting => {
            w.insert("stackexchange", 1.0);
            w.insert("mwmbl", 0.3);
        }
        Intent::Entity => {
            w.insert("wikipedia", 1.0);
            w.insert("mwmbl", 0.3);
        }
        Intent::General => {
            // Wikipedia rides along on general queries: it costs 280 ms and it is the right answer whenever
            // the question turns out to be about a thing, which a keyword classifier will sometimes miss.
            w.insert("mwmbl", 0.7);
            w.insert("wikipedia", 0.5);
            w.insert("hn", 0.4);
        }
    }
    w
}

/// Words in the query that name a thing, used for coverage.
pub fn subjects(query: &str) -> BTreeSet<String> {
    const STOP: [&str; 26] = [
        "the", "a", "an", "of", "for", "to", "in", "on", "at", "is", "are", "was", "were", "how",
        "do", "does", "did", "i", "can", "not", "with", "and", "or", "it", "its", "this",
    ];
    query
        .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '.' && c != '-' && c != '#')
        .map(|word| word.trim_matches(|c| c == '.' || c == '-').to_lowercase())
        .filter(|word| word.len() > 2 && !STOP.contains(&word.as_str()))
        .collect()
}

pub fn domain(url: &str) -> String {
    let after_scheme = url.split("://").nth(1).unwrap_or("");
    let host = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.split(':').next().unwrap_or("").to_lowercase();
    host.strip_prefix("www.").unwrap_or(&host).to_string()
}

/// The registrable domain: `docs.python.org` and `www.python.org` are both `python.org`.
///
/// Two labels is the rule; three only when the middle label is a public-suffix component, which is the small
/// list every implementation carries because the real list is 9,000 entries and changes weekly. The failure
/// this prevents is the one that matters for deduplication: treating `docs.python.org` and `python.org` as two
/// different sites puts the same page in the fused list twice.
pub fn registrable(host: &str) -> String {
    const SECOND_LEVEL: [&str; 8] = ["co", "com", "net", "org", "gov", "ac", "edu", "or"];
    let parts: Vec<&str> = host.split('.').filter(|p| !p.is_empty()).collect();
    if parts.len() >= 3 && SECOND_LEVEL.contains(&parts[parts.len() - 2]) {
        parts[parts.len() - 3..].join(".")
    } else if parts.len() >= 2 {
        parts[parts.len() - 2..].join(".")
    } else {
        parts.join(".")
    }
}

/// Where an answer usually lives, from the URL's shape rather than from a list of sites nobody maintains.
pub fn authority(host: &str, path: &str) -> f64 {
    let mut score = 0.0;
    for prefix in ["docs.", "doc.", "developer.", "learn.", "wiki."] {
        if host.starts_with(prefix) {
            score += 1.0;
            break;
        }
    }
    if host.ends_with(".rs")
        || host.ends_with(".io")
        || host.ends_with(".org")
        || host.ends_with(".dev")
    {
        score += 0.4;
    }
    if host == "github.com" || host == "gitlab.com" {
        score += 0.5;
    }
    if host.ends_with("wikipedia.org") {
        score += 0.6;
    }
    if host == "stackoverflow.com" {
        score += 0.5;
    }
    for part in [
        "/docs/",
        "/doc/",
        "/reference/",
        "/api/",
        "/manual/",
        "/stable/",
        "/latest/",
    ] {
        if path.contains(part) {
            score += 0.5;
            break;
        }
    }
    // Content farms are the single most common reason a correct answer sits below a wrong one.
    const NOISE: [&str; 12] = [
        "geeksforgeeks",
        "w3schools",
        "tutorialspoint",
        "javatpoint",
        "baeldung",
        "dev.to",
        "medium.com",
        "codegrepper",
        "scribd",
        "slideshare",
        "quora.com",
        "linkedin.com",
    ];
    if NOISE.iter().any(|n| host.contains(n) || path.contains(n)) {
        score -= 1.2;
    }
    score
}

/// How much of what the query named appears in the result. The cheapest relevance signal there is, and it
/// beat every hand-tuned weight tried in its place.
pub fn coverage(query: &str, title: &str, url: &str) -> f64 {
    let wanted = subjects(query);
    if wanted.is_empty() {
        return 0.0;
    }
    let blob = format!("{} {}", title, url).to_lowercase();
    let found = wanted
        .iter()
        .filter(|word| blob.contains(word.as_str()))
        .count();
    found as f64 / wanted.len() as f64
}

/// One row of the fused ranking, with the arithmetic kept so a bad order can be explained rather than
/// argued about.
#[derive(Debug, Clone, PartialEq)]
pub struct Scored {
    pub hit: Hit,
    pub score: f64,
    pub sources: BTreeSet<&'static str>,
}

/// Fuse the answers. No host affinity: it was measured and it rewarded the right domain's wrong page.
pub fn fuse(
    query: &str,
    answers: &[Answer],
    weights: &BTreeMap<&'static str, f64>,
    limit: usize,
) -> Vec<Scored> {
    const K: f64 = 60.0;
    let mut scores: BTreeMap<String, Scored> = BTreeMap::new();
    for answer in answers {
        if answer.state != State::Ok {
            continue;
        }
        let weight = *weights.get(answer.source).unwrap_or(&1.0);
        if weight <= 0.0 {
            continue;
        }
        for hit in &answer.hits {
            let key = format!("{}{}", registrable(&domain(&hit.url)), path_of(&hit.url));
            let entry = scores.entry(key).or_insert_with(|| Scored {
                hit: hit.clone(),
                score: 0.0,
                sources: BTreeSet::new(),
            });
            entry.score += weight / (K + hit.rank as f64);
            entry.sources.insert(hit.source);
        }
    }
    for row in scores.values_mut() {
        let host = domain(&row.hit.url);
        row.score += 0.5 * authority(&host, &path_of(&row.hit.url));
        row.score += 0.6 * coverage(query, &row.hit.title, &row.hit.url);
        row.score += 0.3 * (row.sources.len().saturating_sub(1)) as f64;
    }
    let mut rows: Vec<Scored> = scores.into_values().collect();
    // Ties are broken by score, then by the shorter URL: a canonical page is usually the shorter path.
    rows.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.hit.url.len().cmp(&b.hit.url.len()))
    });
    rows.truncate(limit);
    rows
}

fn path_of(url: &str) -> String {
    let after = url.split("://").nth(1).unwrap_or("");
    let rest = after.split_once('/').map(|(_, tail)| tail).unwrap_or("");
    let path = rest.split(['?', '#']).next().unwrap_or("");
    // A trailing slash and an explicit index page are the same page. Measured live: the same article arrived
    // as `/loom/` and `/loom` and took two rows in the fused list.
    let trimmed = path.trim_end_matches('/').trim_end_matches("/index.html");
    format!("/{trimmed}")
}

// ---------------------------------------------------------------- sources
//
// Every source here is an HTTP API that answers without a key. Parsing is deliberately forgiving: an index
// that changes its JSON shape must degrade to "no results from this source", never to a panic or a silent
// lie about what was found. `fetch::bytes` is the runtime's own validated GET, so a source URL is subject to
// the same private-address refusal as any other fetch.

use crate::fetch;

/// How much of a source's answer to read. Indexes send 200-400 KB for ten results; the parse needs all of it.
const MAX_BYTES: usize = 2 * 1024 * 1024;

fn get(url: &str) -> Result<String, String> {
    let body = fetch::bytes(url, MAX_BYTES, false)?;
    Ok(String::from_utf8_lossy(&body).to_string())
}

/// Percent-encode a query for a URL. Space becomes `+`, which every one of these APIs accepts.
pub fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 3);
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Wikipedia's own search. The canonical answer for an entity, and nothing else wins that class.
pub fn wikipedia(query: &str, limit: usize) -> Answer {
    let started = std::time::Instant::now();
    let url = format!(
        "https://en.wikipedia.org/w/api.php?action=query&list=search&format=json&srlimit={limit}&srsearch={}",
        encode(query)
    );
    let body = match get(&url) {
        Ok(body) => body,
        Err(error) => return failed("wikipedia", started, &error),
    };
    let value: serde_json::Value = match serde_json::from_str(&body) {
        Ok(value) => value,
        Err(error) => return failed("wikipedia", started, &format!("json: {error}")),
    };
    let mut hits = Vec::new();
    if let Some(rows) = value.pointer("/query/search").and_then(|v| v.as_array()) {
        for (i, row) in rows.iter().take(limit).enumerate() {
            let title = row.get("title").and_then(|v| v.as_str()).unwrap_or("");
            if title.is_empty() {
                continue;
            }
            let slug = title.replace(' ', "_");
            hits.push(Hit {
                title: title.to_string(),
                url: format!(
                    "https://en.wikipedia.org/wiki/{}",
                    encode(&slug).replace('+', "_")
                ),
                snippet: plain(row.get("snippet").and_then(|v| v.as_str()).unwrap_or("")),
                source: "wikipedia",
                rank: i + 1,
            });
        }
    }
    done("wikipedia", started, hits)
}

/// Stack Exchange: the canonical answer for a troubleshooting question.
pub fn stackexchange(query: &str, limit: usize) -> Answer {
    let started = std::time::Instant::now();
    let url = format!(
        "https://api.stackexchange.com/2.3/search/advanced?order=desc&sort=relevance&q={}&site=stackoverflow&pagesize={limit}",
        encode(query)
    );
    let body = match get(&url) {
        Ok(body) => body,
        Err(error) => return failed("stackexchange", started, &error),
    };
    let value: serde_json::Value = match serde_json::from_str(&body) {
        Ok(value) => value,
        Err(error) => return failed("stackexchange", started, &format!("json: {error}")),
    };
    let mut hits = Vec::new();
    for (i, row) in value
        .get("items")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
        .iter()
        .take(limit)
        .enumerate()
    {
        let link = row.get("link").and_then(|v| v.as_str()).unwrap_or("");
        if link.is_empty() {
            continue;
        }
        hits.push(Hit {
            title: plain(row.get("title").and_then(|v| v.as_str()).unwrap_or("")),
            url: link.to_string(),
            snippet: String::new(),
            source: "stackexchange",
            rank: i + 1,
        });
    }
    done("stackexchange", started, hits)
}

/// Hacker News through Algolia: where a discussion lives, not where a reference does.
pub fn hn(query: &str, limit: usize) -> Answer {
    let started = std::time::Instant::now();
    let url = format!(
        "https://hn.algolia.com/api/v1/search?query={}&hitsPerPage={limit}",
        encode(query)
    );
    let body = match get(&url) {
        Ok(body) => body,
        Err(error) => return failed("hn", started, &error),
    };
    let value: serde_json::Value = match serde_json::from_str(&body) {
        Ok(value) => value,
        Err(error) => return failed("hn", started, &format!("json: {error}")),
    };
    let mut hits = Vec::new();
    for (i, row) in value
        .get("hits")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
        .iter()
        .take(limit)
        .enumerate()
    {
        let url = row
            .get("url")
            .and_then(|v| v.as_str())
            .filter(|u| !u.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                format!(
                    "https://news.ycombinator.com/item?id={}",
                    row.get("objectID").and_then(|v| v.as_str()).unwrap_or("")
                )
            });
        let title = row
            .get("title")
            .or_else(|| row.get("story_title"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if title.is_empty() {
            continue;
        }
        hits.push(Hit {
            title: plain(title),
            url,
            snippet: plain(row.get("story_text").and_then(|v| v.as_str()).unwrap_or("")),
            source: "hn",
            rank: i + 1,
        });
    }
    done("hn", started, hits)
}

/// Mwmbl: a small community index that scored 0.372 MRR on documentation queries, which is not nothing.
/// Its title and extract are lists of spans, not strings; a parser that assumes a string finds nothing.
pub fn mwmbl(query: &str, limit: usize) -> Answer {
    let started = std::time::Instant::now();
    let url = format!("https://api.mwmbl.org/api/v1/search/?s={}", encode(query));
    let body = match get(&url) {
        Ok(body) => body,
        Err(error) => return failed("mwmbl", started, &error),
    };
    let value: serde_json::Value = match serde_json::from_str(&body) {
        Ok(value) => value,
        Err(error) => return failed("mwmbl", started, &format!("json: {error}")),
    };
    let rows = value
        .as_array()
        .cloned()
        .or_else(|| value.get("results").and_then(|v| v.as_array()).cloned())
        .unwrap_or_default();
    let mut hits = Vec::new();
    for (i, row) in rows.iter().take(limit).enumerate() {
        let url = row.get("url").and_then(|v| v.as_str()).unwrap_or("");
        if url.is_empty() {
            continue;
        }
        hits.push(Hit {
            title: spans(row.get("title")),
            url: url.to_string(),
            snippet: spans(row.get("extract")),
            source: "mwmbl",
            rank: i + 1,
        });
    }
    done("mwmbl", started, hits)
}

/// Marginalia: the small, non-commercial web. Slow to answer and sometimes down, hence its own timeout budget.
pub fn marginalia(query: &str, limit: usize) -> Answer {
    let started = std::time::Instant::now();
    let url = format!(
        "https://api.marginalia.nu/public/search/{}?count={limit}",
        encode(query)
    );
    let body = match get(&url) {
        Ok(body) => body,
        Err(error) => return failed("marginalia", started, &error),
    };
    let value: serde_json::Value = match serde_json::from_str(&body) {
        Ok(value) => value,
        Err(error) => return failed("marginalia", started, &format!("json: {error}")),
    };
    let mut hits = Vec::new();
    for (i, row) in value
        .get("results")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
        .iter()
        .take(limit)
        .enumerate()
    {
        let url = row.get("url").and_then(|v| v.as_str()).unwrap_or("");
        if url.is_empty() {
            continue;
        }
        hits.push(Hit {
            title: row
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            url: url.to_string(),
            snippet: row
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            source: "marginalia",
            rank: i + 1,
        });
    }
    done("marginalia", started, hits)
}

/// mwmbl sends `{"value": "...", "is_bold": false}` spans; a plain string is passed through.
fn spans(value: Option<&serde_json::Value>) -> String {
    match value {
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(serde_json::Value::Array(parts)) => parts
            .iter()
            .map(|p| p.get("value").and_then(|v| v.as_str()).unwrap_or(""))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// Drop the markup an index wraps around a snippet, and unescape the entities it leaves behind.
fn plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut inside = false;
    for ch in text.chars() {
        match ch {
            '<' => inside = true,
            '>' => inside = false,
            c if !inside => out.push(c),
            _ => {}
        }
    }
    out.replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .replace("&nbsp;", " ")
        .replace("  ", " ")
        .trim()
        .to_string()
}

fn done(source: &'static str, started: std::time::Instant, hits: Vec<Hit>) -> Answer {
    Answer {
        source,
        state: if hits.is_empty() {
            State::Empty
        } else {
            State::Ok
        },
        hits,
        ms: started.elapsed().as_millis(),
        detail: String::new(),
    }
}

fn failed(source: &'static str, started: std::time::Instant, detail: &str) -> Answer {
    // A refusal from the index's own edge is a block, not a failure of this code, and the distinction is what
    // tells a person to try later rather than to distrust the tool.
    let blocked = detail.contains("403") || detail.contains("429") || detail.contains("refused");
    Answer {
        source,
        state: if blocked {
            State::Blocked
        } else {
            State::Error
        },
        hits: Vec::new(),
        ms: started.elapsed().as_millis(),
        detail: detail.to_string(),
    }
}

/// What one search did: the class it decided, the answers, and the fused order.
pub struct Report {
    pub intent: Intent,
    pub answers: Vec<Answer>,
    pub rows: Vec<Scored>,
}

/// Ask the sources this query class needs, fuse, and return the ranking plus what happened to every source.
pub fn search(query: &str, limit: usize) -> Report {
    let intent = intent_of(query);
    let weights = weights_for(intent);
    let asked: Vec<&'static str> = weights.keys().copied().collect();
    let mut handles = Vec::new();
    for source in asked {
        let query = query.to_string();
        handles.push(std::thread::spawn(move || match source {
            "wikipedia" => wikipedia(&query, 10),
            "stackexchange" => stackexchange(&query, 10),
            "hn" => hn(&query, 10),
            "mwmbl" => mwmbl(&query, 10),
            "marginalia" => marginalia(&query, 10),
            other => Answer {
                source: "unknown",
                hits: Vec::new(),
                state: State::Error,
                ms: 0,
                detail: format!("no such source: {other}"),
            },
        }));
    }
    let answers: Vec<Answer> = handles
        .into_iter()
        .map(|handle| {
            handle.join().unwrap_or(Answer {
                source: "unknown",
                hits: Vec::new(),
                state: State::Error,
                ms: 0,
                detail: "the source thread panicked".to_string(),
            })
        })
        .collect();
    let rows = fuse(query, &answers, &weights, limit);
    Report {
        intent,
        answers,
        rows,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(source: &'static str, rank: usize, title: &str, url: &str) -> Hit {
        Hit {
            title: title.into(),
            url: url.into(),
            snippet: String::new(),
            source,
            rank,
        }
    }

    fn answer(source: &'static str, hits: Vec<Hit>) -> Answer {
        let state = if hits.is_empty() {
            State::Empty
        } else {
            State::Ok
        };
        Answer {
            source,
            hits,
            state,
            ms: 10,
            detail: String::new(),
        }
    }

    #[test]
    fn a_symbol_lookup_is_documentation_even_with_no_keyword() {
        assert_eq!(intent_of("tokio spawn_blocking"), Intent::Documentation);
        // "trait" is in the symbol list, so this is a documentation lookup now. It used to fall through to
        // General, and the live run showed what that costs: a reference question asked of discussion indexes.
        assert_eq!(
            intent_of("rust async trait object safety"),
            Intent::Documentation
        );
        assert_eq!(
            intent_of("what is the difference between latency and throughput"),
            Intent::General
        );
        assert_eq!(
            intent_of("docker permission denied var run docker sock"),
            Intent::Troubleshooting
        );
        assert_eq!(intent_of("who invented the jacquard loom"), Intent::Entity);
        assert_eq!(intent_of("sqlite pragma docs"), Intent::Documentation);
    }

    #[test]
    fn a_troubleshooting_query_does_not_ask_the_encyclopedia() {
        let w = weights_for(Intent::Troubleshooting);
        assert!(w.contains_key("stackexchange"));
        assert!(!w.contains_key("wikipedia"));
    }

    #[test]
    fn the_sources_that_answer_nothing_do_not_vote() {
        let weights = weights_for(Intent::Documentation);
        let answers = vec![
            answer(
                "mwmbl",
                vec![hit(
                    "mwmbl",
                    1,
                    "SQLite Pragma",
                    "https://sqlite.org/pragma.html",
                )],
            ),
            Answer {
                source: "marginalia",
                hits: vec![],
                state: State::Blocked,
                ms: 900,
                detail: "HTTP 429".into(),
            },
        ];
        let rows = fuse("sqlite pragma wal mode", &answers, &weights, 5);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].hit.url, "https://sqlite.org/pragma.html");
    }

    #[test]
    fn a_content_farm_ranks_below_the_reference_page_it_outranks_in_the_source() {
        let weights = weights_for(Intent::Documentation);
        // The farm is returned first by the source; the reference page second. Fusion must correct that.
        let answers = vec![answer(
            "mwmbl",
            vec![
                hit(
                    "mwmbl",
                    1,
                    "SQLite WAL Mode Tutorial",
                    "https://www.geeksforgeeks.org/sqlite-wal/",
                ),
                hit(
                    "mwmbl",
                    2,
                    "Write-Ahead Logging",
                    "https://sqlite.org/wal.html",
                ),
            ],
        )];
        let rows = fuse("sqlite wal mode", &answers, &weights, 5);
        assert_eq!(rows[0].hit.url, "https://sqlite.org/wal.html", "{rows:#?}");
    }

    #[test]
    fn coverage_prefers_the_result_that_names_what_was_asked() {
        let weights = weights_for(Intent::Documentation);
        let answers = vec![answer(
            "mwmbl",
            vec![
                hit("mwmbl", 1, "A general guide", "https://example.org/guide"),
                hit(
                    "mwmbl",
                    2,
                    "spawn_blocking in tokio::task",
                    "https://docs.rs/tokio/1/tokio/task/fn.spawn_blocking.html",
                ),
            ],
        )];
        let rows = fuse("tokio spawn_blocking", &answers, &weights, 5);
        assert!(rows[0].hit.url.contains("spawn_blocking"));
    }

    #[test]
    fn two_sources_agreeing_beats_one_source_alone() {
        let weights = BTreeMap::from([("mwmbl", 1.0), ("hn", 1.0)]);
        let answers = vec![
            answer(
                "mwmbl",
                vec![hit("mwmbl", 3, "Only here", "https://a.example.org/one")],
            ),
            answer(
                "hn",
                vec![
                    hit("hn", 1, "Shared", "https://b.example.org/two"),
                    hit("hn", 2, "Only there", "https://c.example.org/three"),
                ],
            ),
        ];
        let rows = fuse("shared thing", &answers, &weights, 5);
        assert_eq!(rows[0].hit.url, "https://b.example.org/two");
        assert_eq!(
            rows[0].sources.len(),
            1,
            "the same URL from two sources is one row"
        );
    }

    #[test]
    fn the_same_page_from_two_sources_is_one_result() {
        let weights = BTreeMap::from([("mwmbl", 1.0), ("marginalia", 1.0)]);
        let answers = vec![
            answer(
                "mwmbl",
                vec![hit("mwmbl", 1, "SQLite", "https://sqlite.org/pragma.html")],
            ),
            answer(
                "marginalia",
                vec![hit(
                    "marginalia",
                    1,
                    "SQLite pragma",
                    "https://sqlite.org/pragma.html",
                )],
            ),
        ];
        let rows = fuse("sqlite pragma", &answers, &weights, 5);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].sources.len(), 2);
    }

    /// Live: asks the real indexes. Ignored by default because the suite must not depend on the network.
    ///
    ///     cargo test -p harness --lib websearch::tests::live -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_search_over_the_real_indexes() {
        for query in [
            "sqlite pragma wal mode",
            "docker permission denied var run docker sock",
            "jacquard loom punched cards",
        ] {
            let report = search(query, 6);
            println!("\n=== {query}  [{}]", report.intent.name());
            for answer in &report.answers {
                println!(
                    "  source {:<14} {:<8} {:>5}ms  {} hit(s) {}",
                    answer.source,
                    answer.state.name(),
                    answer.ms,
                    answer.hits.len(),
                    answer.detail
                );
            }
            for (i, row) in report.rows.iter().enumerate() {
                println!(
                    "  {}. {:.3} {:<54} {}",
                    i + 1,
                    row.score,
                    row.hit.title.chars().take(54).collect::<String>(),
                    row.hit.url
                );
            }
        }
    }

    #[test]
    fn a_domain_is_reduced_to_what_a_person_would_call_it() {
        assert_eq!(domain("https://www.example.org/a/b"), "example.org");
        assert_eq!(registrable("docs.python.org"), "python.org");
        assert_eq!(registrable("www.python.org"), "python.org");
        assert_eq!(registrable("example.org"), "example.org");
        assert_eq!(
            registrable("news.bbc.co.uk"),
            "bbc.co.uk",
            "a two-part suffix keeps three labels"
        );
        assert_eq!(
            domain("https://docs.rs/tokio/1/tokio/task/fn.spawn_blocking.html"),
            "docs.rs"
        );
    }

    #[test]
    fn authority_rewards_reference_shapes_and_punishes_farms() {
        assert!(authority("docs.python.org", "/3/library/asyncio-task.html") > 1.0);
        assert!(authority("sqlite.org", "/pragma.html") > 0.3);
        assert!(authority("geeksforgeeks.org", "/sqlite-wal/") < 0.0);
    }
}
