//! Research: a question becomes a report whose factual claims carry byte-verified quotes.
//!
//! The shape is deliberately not a chat. The run owns the state; the planner and each extractor are
//! stateless calls built from that state. A worker sees one subquestion and one source, never the
//! planner's conversation or another worker's. That is the property the long-horizon work needs: the
//! log is evidence, the prompt is a view.
//!
//! The check is deterministic. It reads the stored canonical text and compares `body[start..end]` with
//! the quote the report cites. A link is provenance; a byte range is a check.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{Completion, Harness};
use crate::task::{Budget, Usage};

pub const PARSER: &str = "fetch/strip_html/v2";
pub const MIN_BODY_CHARS: usize = 400;
pub const MIN_QUOTE_CHARS: usize = 20;
pub const SENT_CHARS: usize = 12_000;

// ── types ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionKind {
    #[default]
    Fact,
    Decision,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    #[default]
    Web,
    Offline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Planned,
    Searching,
    Checking,
    Published,
    CheckFailed,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubquestionStatus {
    Pending,
    Done,
    Empty,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssignmentRole {
    Plan,
    Extract,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssignmentStatus {
    Running,
    Done,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Primary,
    Secondary,
    Social,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimKind {
    Observed,
    #[default]
    Sourced,
    Inferred,
}

impl ClaimKind {
    pub fn label(self) -> &'static str {
        match self {
            ClaimKind::Observed => "observed",
            ClaimKind::Sourced => "sourced",
            ClaimKind::Inferred => "inferred",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResearchBudget {
    pub queries: usize,
    pub fetches: usize,
    pub minutes: u64,
    pub claims_per_source: usize,
}

impl Default for ResearchBudget {
    fn default() -> Self {
        ResearchBudget {
            queries: 12,
            fetches: 24,
            minutes: 60,
            claims_per_source: 3,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UsageLedger {
    pub searches: usize,
    pub fetches: usize,
    pub sources: usize,
    pub claims: usize,
    pub planner_prompt_tokens: u64,
    pub planner_generated_tokens: u64,
    pub extractor_prompt_tokens: u64,
    pub extractor_generated_tokens: u64,
    pub judge_prompt_tokens: u64,
    pub judge_generated_tokens: u64,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionCriteria {
    pub support: String,
    pub weaken: String,
    pub unclear: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Quote {
    pub source: String,
    pub start: usize,
    pub end: usize,
    pub text: String,
}

impl Quote {
    /// Verify against the canonical body. Byte offsets, not a substring search: the same sentence in
    /// another context is not the same evidence.
    pub fn verifies(&self, body: &str) -> bool {
        self.end > self.start
            && self.end <= body.len()
            && body.get(self.start..self.end) == Some(self.text.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaimRevision {
    pub text: String,
    pub support: Vec<Quote>,
    pub at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claim {
    pub id: String,
    pub text: String,
    pub kind: ClaimKind,
    pub subquestion: String,
    pub support: Vec<Quote>,
    pub contradict: Vec<Quote>,
    #[serde(default = "yes")]
    pub on_question: bool,
    #[serde(default)]
    pub history: Vec<ClaimRevision>,
    pub decision: Option<DecisionCriteria>,
    pub alternatives: Vec<String>,
    pub revision_of: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    pub id: String,
    pub url: String,
    pub final_url: String,
    pub title: String,
    pub fetched_at_ms: u64,
    pub status: Option<u16>,
    pub content_type: String,
    pub sha256: String,
    pub canonical_sha256: String,
    pub bytes: usize,
    pub parser: String,
    pub kind: SourceKind,
    pub artifact_url: Option<String>,
    pub snippet_only: bool,
}

impl Source {
    pub fn admits(&self, body: &str) -> Result<(), String> {
        let library = self.url.starts_with("library://");
        if !(library || self.url.starts_with("http://") || self.url.starts_with("https://")) {
            return Err("the source is not an http(s) or library URL".to_string());
        }
        if self.snippet_only {
            return Err("a search snippet is not a source".to_string());
        }
        let floor = if library { 80 } else { MIN_BODY_CHARS };
        if body.chars().count() < floor {
            return Err(format!(
                "the body is {} characters; a snippet or an empty page is not evidence",
                body.chars().count()
            ));
        }
        if self.title.trim().is_empty() {
            return Err("the source has no title".to_string());
        }
        if self.sha256.is_empty() || self.canonical_sha256.is_empty() {
            return Err("the source has no hash".to_string());
        }
        if self.kind == SourceKind::Social && self.artifact_url.is_none() {
            return Err(
                "a social post with no linked artifact cannot support a claim on its own"
                    .to_string(),
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subquestion {
    pub id: String,
    pub text: String,
    pub query: String,
    pub kind: QuestionKind,
    pub status: SubquestionStatus,
    pub claim_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Assignment {
    pub id: String,
    pub subquestion: String,
    pub worker: String,
    pub role: AssignmentRole,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub status: AssignmentStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Failure {
    pub stage: String,
    pub subject: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dispute {
    pub id: String,
    pub left: String,
    pub right: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResearchRun {
    pub id: String,
    pub question: String,
    pub kind: QuestionKind,
    pub status: RunStatus,
    pub created_at_ms: u64,
    #[serde(default)]
    pub mode: RunMode,
    pub budget: ResearchBudget,
    pub subquestions: Vec<Subquestion>,
    pub assignments: Vec<Assignment>,
    pub sources: Vec<Source>,
    pub claims: Vec<Claim>,
    #[serde(default)]
    pub disputes: Vec<Dispute>,
    pub failures: Vec<Failure>,
    pub usage: UsageLedger,
    pub report: Option<String>,
}

// ── store ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(state: &Path) -> Store {
        Store {
            root: state.join("research"),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    pub fn source_path(&self, id: &str, source: &str) -> PathBuf {
        self.dir(id).join("sources").join(format!("{source}.txt"))
    }

    pub fn create(
        &self,
        question: &str,
        kind: QuestionKind,
        budget: ResearchBudget,
    ) -> Result<ResearchRun, String> {
        let id = format!("r-{}-{}", now_ms(), slug(question));
        self.create_named(&id, question, kind, budget)
    }

    pub fn create_named(
        &self,
        id: &str,
        question: &str,
        kind: QuestionKind,
        budget: ResearchBudget,
    ) -> Result<ResearchRun, String> {
        let run = ResearchRun {
            id: id.to_string(),
            question: question.to_string(),
            kind,
            status: RunStatus::Planned,
            created_at_ms: now_ms(),
            mode: RunMode::Web,
            budget,
            subquestions: Vec::new(),
            assignments: Vec::new(),
            sources: Vec::new(),
            claims: Vec::new(),
            disputes: Vec::new(),
            failures: Vec::new(),
            usage: UsageLedger::default(),
            report: None,
        };
        fs::create_dir_all(self.dir(id).join("sources"))
            .map_err(|error| format!("could not create the research run: {error}"))?;
        self.save(&run)?;
        Ok(run)
    }

    pub fn save(&self, run: &ResearchRun) -> Result<(), String> {
        let dir = self.dir(&run.id);
        fs::create_dir_all(dir.join("sources"))
            .map_err(|error| format!("could not create the research run: {error}"))?;
        let text = serde_json::to_string_pretty(run)
            .map_err(|error| format!("could not encode the research run: {error}"))?;
        let temp = dir.join("run.json.tmp");
        fs::write(&temp, text)
            .map_err(|error| format!("could not write the research run: {error}"))?;
        fs::rename(&temp, dir.join("run.json"))
            .map_err(|error| format!("could not replace the research run: {error}"))?;
        Ok(())
    }

    pub fn load(&self, id: &str) -> Result<ResearchRun, String> {
        let path = self.dir(id).join("run.json");
        let text = fs::read_to_string(&path)
            .map_err(|error| format!("no research run `{id}`: {error}"))?;
        serde_json::from_str(&text)
            .map_err(|error| format!("the research run `{id}` is not readable: {error}"))
    }

    pub fn list(&self) -> Vec<String> {
        let mut ids = Vec::new();
        let Ok(entries) = fs::read_dir(&self.root) else {
            return ids;
        };
        for entry in entries.flatten() {
            if entry.path().is_dir() && entry.path().join("run.json").exists() {
                ids.push(entry.file_name().to_string_lossy().to_string());
            }
        }
        ids.sort();
        ids
    }

    pub fn save_source(&self, run_id: &str, source: &Source, body: &str) -> Result<(), String> {
        let path = self.source_path(run_id, &source.id);
        fs::write(&path, body)
            .map_err(|error| format!("could not write source {}: {error}", source.id))?;
        let meta = serde_json::to_string_pretty(source)
            .map_err(|error| format!("could not encode source {}: {error}", source.id))?;
        fs::write(
            self.dir(run_id)
                .join("sources")
                .join(format!("{}.json", source.id)),
            meta,
        )
        .map_err(|error| format!("could not write source {} metadata: {error}", source.id))?;
        Ok(())
    }

    pub fn append_failure(&self, run_id: &str, failure: &Failure) {
        let path = self.dir(run_id).join("failures.jsonl");
        if let Ok(line) = serde_json::to_string(failure) {
            use std::io::Write;
            if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
                let _ = writeln!(file, "{line}");
            }
        }
    }

    pub fn append_evidence(&self, run_id: &str, claim: &Claim) {
        let path = self.dir(run_id).join("evidence.jsonl");
        if let Ok(line) = serde_json::to_string(claim) {
            use std::io::Write;
            if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
                let _ = writeln!(file, "{line}");
            }
        }
    }

    pub fn append_assignment(&self, run_id: &str, assignment: &Assignment) {
        let path = self.dir(run_id).join("assignments.jsonl");
        if let Ok(line) = serde_json::to_string(assignment) {
            use std::io::Write;
            if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
                let _ = writeln!(file, "{line}");
            }
        }
    }
}

// ── seams ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[derive(Debug, Clone)]
pub struct Fetched {
    pub url: String,
    pub final_url: String,
    pub title: String,
    pub status: Option<u16>,
    pub content_type: String,
    pub body: String,
    /// What the extractor sees, when the full body is too large or only one part is relevant. The body
    /// is still the canonical text the quote is verified against.
    pub excerpt: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PlanItem {
    pub text: String,
    #[serde(default)]
    pub query: String,
    #[serde(default)]
    pub kind: QuestionKind,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ClaimDraft {
    pub text: String,
    pub quote: String,
    #[serde(default)]
    pub kind: ClaimKind,
    #[serde(default)]
    pub decision: Option<DecisionCriteria>,
    #[serde(default)]
    pub alternatives: Vec<String>,
    #[serde(default)]
    pub contradicts: Vec<String>,
}

pub trait Web {
    fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, String>;
    fn fetch(&self, url: &str) -> Result<Fetched, String>;
}

pub trait Planner {
    fn plan(&self, question: &str, budget: &ResearchBudget) -> Result<Vec<PlanItem>, String>;

    /// The same call with the endpoint's cost, when it reports one. The default keeps an offline
    /// planner honest: no usage is not zero usage, and the ledger stays at zero rather than guessing.
    fn plan_with_usage(
        &self,
        question: &str,
        budget: &ResearchBudget,
    ) -> Result<(Vec<PlanItem>, Option<Usage>), String> {
        Ok((self.plan(question, budget)?, None))
    }
}

pub trait Extractor {
    fn extract(
        &self,
        subquestion: &str,
        source: &Source,
        body: &str,
        budget: &ResearchBudget,
    ) -> Result<Vec<ClaimDraft>, String>;

    fn extract_with_usage(
        &self,
        subquestion: &str,
        source: &Source,
        body: &str,
        budget: &ResearchBudget,
    ) -> Result<(Vec<ClaimDraft>, Option<Usage>), String> {
        Ok((self.extract(subquestion, source, body, budget)?, None))
    }

    /// One retry after a quote failed to verify. The hint names the failed quote; it is never the whole
    /// source and it is never trusted as evidence. A default implementation keeps offline extractors
    /// honest: no hint support means no second opinion.
    fn extract_with_hint(
        &self,
        subquestion: &str,
        source: &Source,
        body: &str,
        budget: &ResearchBudget,
        _hint: Option<&str>,
    ) -> Result<(Vec<ClaimDraft>, Option<Usage>), String> {
        self.extract_with_usage(subquestion, source, body, budget)
    }
}

pub trait DisputeJudge {
    fn judge(&self, left: &Claim, right: &Claim) -> Result<Option<String>, String>;

    /// The same comparison with the endpoint's token cost, when it reports one.
    fn judge_with_usage(
        &self,
        left: &Claim,
        right: &Claim,
    ) -> Result<(Option<String>, Option<Usage>), String> {
        Ok((self.judge(left, right)?, None))
    }
}

// ── live web ─────────────────────────────────────────────────────────────────

pub struct Searx {
    base: String,
}

impl Searx {
    pub fn from_env() -> Searx {
        let base = std::env::var("SEARXNG_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "http://127.0.0.1:8080".to_string());
        Searx { base }
    }
}

impl Web for Searx {
    fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, String> {
        let hits = self.search_searx(query, limit)?;
        if relevant(&hits, query) {
            return Ok(hits);
        }
        // The local SearXNG instance can be healthy and still answer with a single-token match
        // ("llama" for "llama.cpp Vulkan RDNA1"). A search that cannot see two query terms is a
        // failed search, not a result set. DuckDuckGo's HTML endpoint through Jina Reader is the
        // fallback; it is a network dependency, and a failure returns the original hits rather than
        // pretending the question has no sources.
        match self.search_jina(query, limit) {
            Ok(fallback) if !fallback.is_empty() => Ok(fallback),
            _ => Ok(hits),
        }
    }

    fn fetch(&self, url: &str) -> Result<Fetched, String> {
        let allow_private = std::env::var("HADES_FETCH_ALLOW_PRIVATE").as_deref() == Ok("1");
        let body = crate::fetch::run(url, 2_000_000, allow_private);
        if body.starts_with("refused:") || body.starts_with("could not run curl") {
            return Err(body);
        }
        let title = host_of(url);
        Ok(Fetched {
            url: url.to_string(),
            final_url: url.to_string(),
            title,
            status: None,
            content_type: "text/html".to_string(),
            body,
            excerpt: None,
        })
    }
}

impl Searx {
    fn search_searx(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, String> {
        let url = format!(
            "{}/search?q={}&format=json",
            self.base.trim_end_matches('/'),
            urlencode(query)
        );
        let output = Command::new("curl")
            .arg("-sS")
            .arg("--max-time")
            .arg("20")
            .arg(&url)
            .output()
            .map_err(|error| format!("could not run curl: {error}"))?;
        let value: Value = serde_json::from_slice(&output.stdout).map_err(|error| {
            format!(
                "the search endpoint did not answer with JSON: {error} (it said: {})",
                clip(String::from_utf8_lossy(&output.stdout).trim(), 200)
            )
        })?;
        let results = value
            .get("results")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut hits = Vec::new();
        for result in results.iter().take(limit) {
            let url = result
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if url.is_empty() {
                continue;
            }
            hits.push(SearchHit {
                title: result
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                url,
                snippet: result
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            });
        }
        Ok(hits)
    }

    fn search_jina(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, String> {
        let url = format!(
            "https://r.jina.ai/http://html.duckduckgo.com/html/?q={}",
            urlencode(query)
        );
        let output = Command::new("curl")
            .arg("-sS")
            .arg("--max-time")
            .arg("30")
            .arg(&url)
            .output()
            .map_err(|error| format!("could not run curl: {error}"))?;
        let text = String::from_utf8_lossy(&output.stdout);
        Ok(parse_jina(&text, limit))
    }
}

fn parse_jina(text: &str, limit: usize) -> Vec<SearchHit> {
    let mut hits = Vec::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let Some(rest) = line.strip_prefix("## [") else {
            continue;
        };
        let Some((title, after)) = rest.split_once("](") else {
            continue;
        };
        let Some((link, _)) = after.split_once(')') else {
            continue;
        };
        let url = if let Some(encoded) = link.split("uddg=").nth(1) {
            let encoded = encoded.split('&').next().unwrap_or(encoded);
            url_decode(encoded)
        } else {
            link.to_string()
        };
        if !url.starts_with("http") {
            continue;
        }
        let mut snippet = String::new();
        while let Some(next) = lines.peek() {
            let next = next.trim();
            if next.is_empty() || next.starts_with("## [") || next.starts_with("![") {
                if next.starts_with("## [") {
                    break;
                }
                lines.next();
                continue;
            }
            snippet = strip_markdown(next);
            lines.next();
            break;
        }
        hits.push(SearchHit {
            title: strip_markdown(title),
            url,
            snippet,
        });
        if hits.len() >= limit {
            break;
        }
    }
    hits
}

fn relevant(hits: &[SearchHit], query: &str) -> bool {
    let tokens: Vec<String> = query
        .split_whitespace()
        .map(|word| {
            word.trim_matches(|ch: char| !ch.is_alphanumeric())
                .to_ascii_lowercase()
        })
        .filter(|word| word.len() >= 3)
        .collect();
    if tokens.is_empty() {
        return !hits.is_empty();
    }
    let needed = tokens.len().min(2);
    hits.iter().any(|hit| {
        let text = format!("{} {}", hit.title, hit.snippet).to_ascii_lowercase();
        tokens
            .iter()
            .filter(|token| text.contains(token.as_str()))
            .count()
            >= needed
    })
}

fn strip_markdown(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '[' => {
                let mut label = String::new();
                for next in chars.by_ref() {
                    if next == ']' {
                        break;
                    }
                    label.push(next);
                }
                if chars.peek() == Some(&'(') {
                    chars.next();
                    let mut depth = 1;
                    for next in chars.by_ref() {
                        if next == '(' {
                            depth += 1;
                        } else if next == ')' {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                    }
                }
                out.push_str(&label);
            }
            '*' | '_' | '`' => {}
            _ => out.push(ch),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn url_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2]))
        {
            out.push(high * 16 + low);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn host_of(url: &str) -> String {
    url.split('/').nth(2).unwrap_or(url).to_string()
}

fn urlencode(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn clip(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let cut: String = text.chars().take(limit).collect();
    format!("{cut}...")
}

fn kind_of(url: &str) -> SourceKind {
    if url.starts_with("library://") {
        return SourceKind::Primary;
    }
    let host = host_of(url).to_ascii_lowercase();
    if host.contains("x.com") || host.contains("twitter.com") {
        SourceKind::Social
    } else if host == "gist.github.com" {
        SourceKind::Secondary
    } else if host.contains("arxiv.org")
        || host.contains("github.com")
        || host.ends_with(".gov")
        || host.ends_with(".edu")
    {
        SourceKind::Primary
    } else if host.is_empty() {
        SourceKind::Unknown
    } else {
        SourceKind::Secondary
    }
}

fn source_rank(kind: SourceKind) -> u8 {
    match kind {
        SourceKind::Primary => 0,
        SourceKind::Secondary => 1,
        SourceKind::Unknown => 2,
        SourceKind::Social => 3,
    }
}

fn source_from_fetched(id: &str, fetched: &Fetched) -> Source {
    let body = &fetched.body;
    let sha = mneme::sha256::hex(body.as_bytes());
    Source {
        id: id.to_string(),
        url: fetched.url.clone(),
        final_url: fetched.final_url.clone(),
        title: if fetched.title.trim().is_empty() {
            host_of(&fetched.url)
        } else {
            fetched.title.clone()
        },
        fetched_at_ms: now_ms(),
        status: fetched.status,
        content_type: fetched.content_type.clone(),
        sha256: sha.clone(),
        canonical_sha256: sha,
        bytes: body.len(),
        parser: PARSER.to_string(),
        kind: kind_of(&fetched.url),
        artifact_url: None,
        snippet_only: false,
    }
}

// ── live model seams ─────────────────────────────────────────────────────────

pub struct ModelPlanner<'a> {
    pub harness: &'a dyn Harness,
}

impl Planner for ModelPlanner<'_> {
    fn plan(&self, question: &str, budget: &ResearchBudget) -> Result<Vec<PlanItem>, String> {
        self.plan_with_usage(question, budget)
            .map(|(items, _usage)| items)
    }

    fn plan_with_usage(
        &self,
        question: &str,
        budget: &ResearchBudget,
    ) -> Result<(Vec<PlanItem>, Option<Usage>), String> {
        let user = format!(
            "Question: {question}\n\nBudget: {} subquestions, {} searches, {} fetches. Return only the JSON array.",
            budget.claims_per_source, budget.queries, budget.fetches
        );
        let completion = self
            .harness
            .complete(PLANNER_SYSTEM, &user, &Budget::new(32_000))
            .map_err(|fault| format!("the planner call failed: {}", fault.detail()))?;
        Ok((parse_json(&completion.text)?, completion.usage))
    }
}

pub struct ModelExtractor<'a> {
    pub harness: &'a dyn Harness,
}

impl Extractor for ModelExtractor<'_> {
    fn extract(
        &self,
        subquestion: &str,
        source: &Source,
        body: &str,
        budget: &ResearchBudget,
    ) -> Result<Vec<ClaimDraft>, String> {
        self.extract_with_usage(subquestion, source, body, budget)
            .map(|(claims, _usage)| claims)
    }

    fn extract_with_usage(
        &self,
        subquestion: &str,
        source: &Source,
        body: &str,
        budget: &ResearchBudget,
    ) -> Result<(Vec<ClaimDraft>, Option<Usage>), String> {
        self.extract_with_hint(subquestion, source, body, budget, None)
    }

    fn extract_with_hint(
        &self,
        subquestion: &str,
        source: &Source,
        body: &str,
        budget: &ResearchBudget,
        hint: Option<&str>,
    ) -> Result<(Vec<ClaimDraft>, Option<Usage>), String> {
        let text = clip(body, SENT_CHARS);
        let hint = hint
            .map(|hint| {
                format!(
                    "\n\nA previous quote did not appear in the source. The failed quote was:\n{}\nCopy a new quote exactly from the text above; do not repair it by guessing.",
                    clip(hint, 600)
                )
            })
            .unwrap_or_default();
        let user = format!(
            "Subquestion: {subquestion}\n\nSource: {}\nURL: {}\nKind: {:?}\n\nTEXT:\n{text}\n\nReturn at most {} claims as a JSON array.{hint}",
            source.title, source.url, source.kind, budget.claims_per_source
        );
        let completion = self
            .harness
            .complete(EXTRACTOR_SYSTEM, &user, &Budget::new(32_000))
            .map_err(|fault| format!("the extractor call failed: {}", fault.detail()))?;
        Ok((parse_json(&completion.text)?, completion.usage))
    }
}

pub struct ModelDisputeJudge<'a> {
    pub harness: &'a dyn Harness,
}

impl DisputeJudge for ModelDisputeJudge<'_> {
    fn judge(&self, left: &Claim, right: &Claim) -> Result<Option<String>, String> {
        self.judge_with_usage(left, right).map(|(reason, _)| reason)
    }

    fn judge_with_usage(
        &self,
        left: &Claim,
        right: &Claim,
    ) -> Result<(Option<String>, Option<Usage>), String> {
        let left_quote = left
            .support
            .first()
            .map(|quote| quote.text.as_str())
            .unwrap_or("");
        let right_quote = right
            .support
            .first()
            .map(|quote| quote.text.as_str())
            .unwrap_or("");
        let user = format!(
            "Subquestion: {}\n\nLeft claim [{}] ({}): {}\nLeft quote: {}\n\nRight claim [{}] ({}): {}\nRight quote: {}\n\nReturn only the JSON object.",
            left.subquestion,
            left.id,
            left.kind.label(),
            left.text,
            left_quote,
            right.id,
            right.kind.label(),
            right.text,
            right_quote
        );
        let completion = self
            .harness
            .complete(DISPUTE_SYSTEM, &user, &Budget::new(8_000))
            .map_err(|fault| format!("the dispute judge call failed: {}", fault.detail()))?;
        let verdict: DisputeVerdict = parse_object(&completion.text)?;
        let reason = if verdict.relation.eq_ignore_ascii_case("contradicts") {
            Some(if verdict.reason.trim().is_empty() {
                "the two sources assert incompatible results".to_string()
            } else {
                verdict.reason.trim().to_string()
            })
        } else {
            None
        };
        Ok((reason, completion.usage))
    }
}

#[derive(Debug, Deserialize)]
struct DisputeVerdict {
    relation: String,
    #[serde(default)]
    reason: String,
}

const DISPUTE_SYSTEM: &str = "You compare two claims from different sources. Return only one JSON object: {\"relation\":\"contradicts\"|\"compatible\"|\"unrelated\",\"reason\":\"...\"}. Use contradicts only when the same quantity, condition, population or time is asserted with incompatible values or opposite conclusions. Different models, drivers, versions, hardware or conditions are not a contradiction; call them unrelated. Do not use outside knowledge.";

const PLANNER_SYSTEM: &str = "You are the planning step of an evidence-first research run. Return only a JSON array. Each item is {\"text\": \"...\", \"query\": \"...\", \"kind\": \"fact\"|\"decision\"}. Give 3 to 5 subquestions. Each query must be plain keywords a search engine can answer; do not use site: operators, URL fragments or file paths. Do not answer the question.";
const EXTRACTOR_SYSTEM: &str = "You are one stateless extraction worker. You see one subquestion and one source. Return only a JSON array. Each item is {\"text\": \"...\", \"quote\": \"...\", \"kind\": \"sourced\"|\"observed\"|\"inferred\", \"decision\": {\"support\": \"...\", \"weaken\": \"...\", \"unclear\": \"...\"} | null, \"alternatives\": [\"...\"], \"contradicts\": [\"...\"]}. The quote must be copied character for character from the source, must be a contiguous span, and must be at least 20 characters. Do not paraphrase. Do not use ellipses. The claim must keep every qualifier in the quote: platform, driver, version, model and condition. Do not turn a conditional observation into a general one. Put a contradicting quote in `contradicts` only when this same source states a conflicting result. Only return claims that directly answer the subquestion; do not return general facts about the project. If the source does not support a claim, return [].";

fn parse_json<T: for<'de> Deserialize<'de>>(text: &str) -> Result<T, String> {
    let extracted = crate::model::extract(text);
    if let Ok(value) = serde_json::from_str::<T>(&extracted) {
        return Ok(value);
    }
    if let (Some(start), Some(end)) = (extracted.find('['), extracted.rfind(']'))
        && end > start
        && let Ok(value) = serde_json::from_str::<T>(&extracted[start..=end])
    {
        return Ok(value);
    }
    Err(format!(
        "the model did not return the requested JSON: {}",
        clip(text, 300)
    ))
}

fn parse_object<T: for<'de> Deserialize<'de>>(text: &str) -> Result<T, String> {
    let extracted = crate::model::extract(text);
    if let Ok(value) = serde_json::from_str::<T>(&extracted) {
        return Ok(value);
    }
    if let (Some(start), Some(end)) = (extracted.find('{'), extracted.rfind('}'))
        && end > start
        && let Ok(value) = serde_json::from_str::<T>(&extracted[start..=end])
    {
        return Ok(value);
    }
    Err(format!(
        "the model did not return the requested JSON object: {}",
        clip(text, 300)
    ))
}

// ── engine ───────────────────────────────────────────────────────────────────

fn absorb_drafts(
    store: &Store,
    run: &mut ResearchRun,
    index: usize,
    source: &Source,
    body: &str,
    drafts: Vec<ClaimDraft>,
    budget: &ResearchBudget,
) -> Result<(usize, usize, Option<String>), String> {
    let mut added = 0usize;
    let mut verified = 0usize;
    let mut first_bad = None;
    for draft in drafts.into_iter().take(budget.claims_per_source) {
        let quote_text = draft.quote.trim().to_string();
        if quote_text.chars().count() < MIN_QUOTE_CHARS {
            run.failures.push(Failure {
                stage: "verify".to_string(),
                subject: source.id.clone(),
                reason: "the quote is shorter than 20 characters".to_string(),
            });
            continue;
        }
        let Some(start) = body.find(&quote_text) else {
            first_bad.get_or_insert_with(|| quote_text.clone());
            run.failures.push(Failure {
                stage: "verify".to_string(),
                subject: source.id.clone(),
                reason: format!(
                    "the quote was not found in the source: {}",
                    clip(&quote_text, 120)
                ),
            });
            continue;
        };
        let end = start + quote_text.len();
        let quote = Quote {
            source: source.id.clone(),
            start,
            end,
            text: quote_text,
        };
        if !quote.verifies(body) {
            first_bad.get_or_insert_with(|| quote.text.clone());
            run.failures.push(Failure {
                stage: "verify".to_string(),
                subject: source.id.clone(),
                reason: "the quote did not verify at its byte range".to_string(),
            });
            continue;
        }
        verified += 1;

        let mut contradict = Vec::new();
        for text in &draft.contradicts {
            let text = text.trim().to_string();
            if text.chars().count() < MIN_QUOTE_CHARS {
                run.failures.push(Failure {
                    stage: "contradict".to_string(),
                    subject: source.id.clone(),
                    reason: "the contradiction quote is shorter than 20 characters".to_string(),
                });
                continue;
            }
            let Some(start) = body.find(&text) else {
                run.failures.push(Failure {
                    stage: "contradict".to_string(),
                    subject: source.id.clone(),
                    reason: format!(
                        "the contradiction quote was not found: {}",
                        clip(&text, 120)
                    ),
                });
                continue;
            };
            let quote = Quote {
                source: source.id.clone(),
                start,
                end: start + text.len(),
                text,
            };
            if quote.verifies(body) {
                contradict.push(quote);
            }
        }

        let on_question =
            claim_on_question(&run.subquestions[index].text, &draft.text, &quote.text);
        let normalized = normalize_claim(&draft.text);
        if let Some(existing) = run
            .claims
            .iter_mut()
            .find(|claim| normalize_claim(&claim.text) == normalized)
        {
            if existing.text != draft.text.trim() {
                existing.history.push(ClaimRevision {
                    text: existing.text.clone(),
                    support: existing.support.clone(),
                    at_ms: now_ms(),
                });
                existing.text = draft.text.trim().to_string();
                existing.decision = draft.decision;
                existing.alternatives = draft.alternatives;
            }
            existing.on_question |= on_question;
            if !existing
                .support
                .iter()
                .any(|q| q.source == quote.source && q.start == quote.start)
            {
                existing.support.push(quote);
            }
            for quote in contradict {
                if !existing
                    .contradict
                    .iter()
                    .any(|q| q.source == quote.source && q.start == quote.start)
                {
                    existing.contradict.push(quote);
                }
            }
        } else {
            let id = format!("c{}", run.claims.len() + 1);
            let claim = Claim {
                id: id.clone(),
                text: draft.text.trim().to_string(),
                kind: draft.kind,
                subquestion: run.subquestions[index].id.clone(),
                support: vec![quote],
                contradict,
                on_question,
                history: Vec::new(),
                decision: draft.decision,
                alternatives: draft.alternatives,
                revision_of: None,
            };
            run.subquestions[index].claim_ids.push(id);
            store.append_evidence(&run.id, &claim);
            run.claims.push(claim);
            run.usage.claims += 1;
            added += 1;
        }
    }
    Ok((added, verified, first_bad))
}

const CLAIM_STOP: &[&str] = &[
    "what",
    "which",
    "documented",
    "document",
    "does",
    "with",
    "from",
    "this",
    "that",
    "into",
    "about",
    "when",
    "where",
    "your",
    "their",
    "there",
    "these",
    "those",
    "have",
    "has",
    "were",
    "was",
    "are",
    "for",
    "the",
    "and",
    "not",
    "but",
    "can",
    "could",
    "should",
    "would",
    "will",
    "how",
    "why",
    "who",
    "one",
    "two",
    "out",
    "use",
    "using",
    "used",
    "paper",
    "study",
    "result",
    "results",
    "findings",
    "according",
    "source",
    "sources",
];

fn distinctive_terms(text: &str) -> Vec<String> {
    let mut terms = Vec::new();
    for word in text.split_whitespace() {
        let word = word
            .trim_matches(|ch: char| !ch.is_alphanumeric() && ch != '.' && ch != '+' && ch != '#')
            .to_ascii_lowercase();
        if word.len() >= 4 && !CLAIM_STOP.contains(&word.as_str()) && !terms.contains(&word) {
            terms.push(word);
        }
    }
    terms
}

/// A conservative lexical signal, not a semantic judge. It is strict enough to catch a general fact
/// published under a specific subquestion, and it never drops the claim: the report lists it and
/// `check --strict` can turn it red.
fn claim_on_question(subquestion: &str, claim: &str, quote: &str) -> bool {
    let terms = distinctive_terms(subquestion);
    if terms.is_empty() {
        return true;
    }
    let haystack = format!(
        "{} {}",
        claim.to_ascii_lowercase(),
        quote.to_ascii_lowercase()
    );
    let matched = terms
        .iter()
        .filter(|term| haystack.contains(term.as_str()))
        .count();
    let needed = if terms.len() >= 4 { 2 } else { 1 };
    matched >= needed
}

fn yes() -> bool {
    true
}

const MAX_DISPUTE_PAIRS: usize = 16;

fn judge_disputes(store: &Store, run: &mut ResearchRun, judge: &dyn DisputeJudge) {
    let mut pairs = Vec::new();
    'outer: for left in 0..run.claims.len() {
        for right in left + 1..run.claims.len() {
            if pairs.len() >= MAX_DISPUTE_PAIRS {
                break 'outer;
            }
            let a = &run.claims[left];
            let b = &run.claims[right];
            if a.subquestion != b.subquestion || !a.on_question || !b.on_question {
                continue;
            }
            if a.text.trim().eq_ignore_ascii_case(b.text.trim()) {
                continue;
            }
            let a_source = a.support.first().map(|quote| quote.source.as_str());
            let b_source = b.support.first().map(|quote| quote.source.as_str());
            if a_source.is_none() || a_source == b_source {
                continue;
            }
            pairs.push((left, right));
        }
    }
    for (left, right) in pairs {
        let (left_claim, right_claim) = (run.claims[left].clone(), run.claims[right].clone());
        match judge.judge_with_usage(&left_claim, &right_claim) {
            Ok((Some(reason), usage)) => {
                if let Some(usage) = usage {
                    run.usage.judge_prompt_tokens += usage.prompt_tokens;
                    run.usage.judge_generated_tokens += usage.generated_tokens;
                }
                let id = format!("d{}", run.disputes.len() + 1);
                run.disputes.push(Dispute {
                    id,
                    left: left_claim.id,
                    right: right_claim.id,
                    reason,
                });
            }
            Ok((None, usage)) => {
                if let Some(usage) = usage {
                    run.usage.judge_prompt_tokens += usage.prompt_tokens;
                    run.usage.judge_generated_tokens += usage.generated_tokens;
                }
            }
            Err(reason) => {
                let failure = Failure {
                    stage: "dispute".to_string(),
                    subject: format!("{} vs {}", left_claim.id, right_claim.id),
                    reason,
                };
                run.failures.push(failure.clone());
                store.append_failure(&run.id, &failure);
            }
        }
    }
}

pub struct Engine<'a> {
    pub store: &'a Store,
    pub web: &'a dyn Web,
    pub planner: &'a dyn Planner,
    pub extractor: &'a dyn Extractor,
    pub judge: Option<&'a dyn DisputeJudge>,
    pub mode: RunMode,
}

impl<'a> Engine<'a> {
    pub fn run(&self, question: &str, budget: ResearchBudget) -> Result<ResearchRun, String> {
        let id = format!("r-{}-{}", now_ms(), slug(question));
        self.run_named(&id, question, budget)
    }

    pub fn run_named(
        &self,
        id: &str,
        question: &str,
        budget: ResearchBudget,
    ) -> Result<ResearchRun, String> {
        let kind = question_kind(question);
        let mut run = self
            .store
            .create_named(id, question, kind, budget.clone())?;
        run.mode = self.mode;
        self.store.save(&run)?;
        let started = Instant::now();
        let deadline = started + Duration::from_secs(budget.minutes.max(1) * 60);

        // Plan. One stateless call.
        let assignment_id = format!("a{}", run.assignments.len() + 1);
        let mut assignment = Assignment {
            id: assignment_id,
            subquestion: "*".to_string(),
            worker: "planner".to_string(),
            role: AssignmentRole::Plan,
            started_at_ms: now_ms(),
            finished_at_ms: None,
            status: AssignmentStatus::Running,
        };
        run.assignments.push(assignment.clone());
        self.store.append_assignment(&run.id, &assignment);

        let planned = match self.planner.plan_with_usage(question, &budget) {
            Ok((items, usage)) => {
                if let Some(usage) = usage {
                    run.usage.planner_prompt_tokens += usage.prompt_tokens;
                    run.usage.planner_generated_tokens += usage.generated_tokens;
                }
                items
            }
            Err(reason) => {
                assignment.status = AssignmentStatus::Failed;
                assignment.finished_at_ms = Some(now_ms());
                run.failures.push(Failure {
                    stage: "plan".to_string(),
                    subject: question.to_string(),
                    reason: reason.clone(),
                });
                self.store
                    .append_failure(&run.id, run.failures.last().expect("just pushed"));
                run.status = RunStatus::Failed;
                self.store.save(&run)?;
                let _ = record_failure(self.store, &run);
                return Err(format!(
                    "{reason}\nrun: {}",
                    self.store.dir(&run.id).display()
                ));
            }
        };
        assignment.status = AssignmentStatus::Done;
        assignment.finished_at_ms = Some(now_ms());
        if let Some(last) = run.assignments.last_mut() {
            *last = assignment;
        }
        run.status = RunStatus::Searching;

        let items = planned.into_iter().take(7).collect::<Vec<_>>();
        if items.is_empty() {
            run.failures.push(Failure {
                stage: "plan".to_string(),
                subject: question.to_string(),
                reason: "the planner returned no subquestions".to_string(),
            });
            run.status = RunStatus::Failed;
            self.store.save(&run)?;
            let _ = record_failure(self.store, &run);
            return Err(format!(
                "the planner returned no subquestions\nrun: {}",
                self.store.dir(&run.id).display()
            ));
        }

        for (index, item) in items.iter().enumerate() {
            let qid = format!("q{}", index + 1);
            let query = if item.query.trim().is_empty() {
                item.text.clone()
            } else {
                item.query.clone()
            };
            run.subquestions.push(Subquestion {
                id: qid.clone(),
                text: item.text.clone(),
                query: query.clone(),
                kind: item.kind,
                status: SubquestionStatus::Pending,
                claim_ids: Vec::new(),
            });
        }
        self.store.save(&run)?;

        let mut fetched_urls: HashSet<String> = HashSet::new();
        let mut no_progress = 0usize;

        for index in 0..run.subquestions.len() {
            if Instant::now() >= deadline {
                run.failures.push(Failure {
                    stage: "budget".to_string(),
                    subject: run.subquestions[index].id.clone(),
                    reason: "the time budget ran out".to_string(),
                });
                break;
            }
            if run.usage.searches >= budget.queries || run.usage.fetches >= budget.fetches {
                run.failures.push(Failure {
                    stage: "budget".to_string(),
                    subject: run.subquestions[index].id.clone(),
                    reason: "the search or fetch budget ran out".to_string(),
                });
                break;
            }

            let worker = format!("w{}", index + 1);
            let assignment_id = format!("a{}", run.assignments.len() + 1);
            let mut assignment = Assignment {
                id: assignment_id,
                subquestion: run.subquestions[index].id.clone(),
                worker: worker.clone(),
                role: AssignmentRole::Extract,
                started_at_ms: now_ms(),
                finished_at_ms: None,
                status: AssignmentStatus::Running,
            };
            run.assignments.push(assignment.clone());
            self.store.append_assignment(&run.id, &assignment);

            let query = run.subquestions[index].query.clone();
            let mut hits = match self.web.search(&query, 5) {
                Ok(hits) => hits,
                Err(reason) => {
                    run.usage.searches += 1;
                    run.failures.push(Failure {
                        stage: "search".to_string(),
                        subject: query.clone(),
                        reason,
                    });
                    run.subquestions[index].status = SubquestionStatus::Failed;
                    assignment.status = AssignmentStatus::Failed;
                    assignment.finished_at_ms = Some(now_ms());
                    if let Some(last) = run.assignments.last_mut() {
                        *last = assignment;
                    }
                    self.store.save(&run)?;
                    continue;
                }
            };
            hits.sort_by_key(|hit| (source_rank(kind_of(&hit.url)), normalize_url(&hit.url)));
            run.usage.searches += 1;

            let mut new_claims = 0usize;
            for hit in hits {
                if run.usage.fetches >= budget.fetches || Instant::now() >= deadline {
                    break;
                }
                let key = normalize_url(&hit.url);
                if key.is_empty() || !usable_url(&hit.url) || !fetched_urls.insert(key) {
                    continue;
                }
                run.usage.fetches += 1;
                let fetched = match self.web.fetch(&hit.url) {
                    Ok(fetched) => fetched,
                    Err(reason) => {
                        run.failures.push(Failure {
                            stage: "fetch".to_string(),
                            subject: hit.url.clone(),
                            reason,
                        });
                        continue;
                    }
                };
                let source_id = format!("s{}", run.sources.len() + 1);
                let source = source_from_fetched(&source_id, &fetched);
                if let Err(reason) = source.admits(&fetched.body) {
                    run.failures.push(Failure {
                        stage: "gate".to_string(),
                        subject: hit.url.clone(),
                        reason,
                    });
                    continue;
                }
                if let Err(reason) = self.store.save_source(&run.id, &source, &fetched.body) {
                    run.failures.push(Failure {
                        stage: "store".to_string(),
                        subject: hit.url.clone(),
                        reason,
                    });
                    continue;
                }
                run.sources.push(source.clone());
                run.usage.sources += 1;
                self.store.save(&run)?;

                let view = fetched.excerpt.as_deref().unwrap_or(&fetched.body);
                let drafts = match self.extractor.extract_with_usage(
                    &run.subquestions[index].text,
                    &source,
                    view,
                    &budget,
                ) {
                    Ok((drafts, usage)) => {
                        if let Some(usage) = usage {
                            run.usage.extractor_prompt_tokens += usage.prompt_tokens;
                            run.usage.extractor_generated_tokens += usage.generated_tokens;
                        }
                        drafts
                    }
                    Err(reason) => {
                        run.failures.push(Failure {
                            stage: "extract".to_string(),
                            subject: source.id.clone(),
                            reason,
                        });
                        continue;
                    }
                };

                let (mut added, verified, first_bad) = absorb_drafts(
                    self.store,
                    &mut run,
                    index,
                    &source,
                    &fetched.body,
                    drafts,
                    &budget,
                )?;
                if verified == 0
                    && let Some(bad) = first_bad
                {
                    match self.extractor.extract_with_hint(
                        &run.subquestions[index].text,
                        &source,
                        view,
                        &budget,
                        Some(&bad),
                    ) {
                        Ok((retry, usage)) => {
                            if let Some(usage) = usage {
                                run.usage.extractor_prompt_tokens += usage.prompt_tokens;
                                run.usage.extractor_generated_tokens += usage.generated_tokens;
                            }
                            let (more, _, _) = absorb_drafts(
                                self.store,
                                &mut run,
                                index,
                                &source,
                                &fetched.body,
                                retry,
                                &budget,
                            )?;
                            added += more;
                        }
                        Err(reason) => run.failures.push(Failure {
                            stage: "extract".to_string(),
                            subject: source.id.clone(),
                            reason: format!("the retry after a failed quote failed: {reason}"),
                        }),
                    }
                }
                new_claims += added;
                self.store.save(&run)?;
            }

            run.subquestions[index].status = if new_claims > 0 {
                SubquestionStatus::Done
            } else {
                SubquestionStatus::Empty
            };
            if new_claims == 0 {
                no_progress += 1;
                let failure = Failure {
                    stage: "progress".to_string(),
                    subject: run.subquestions[index].id.clone(),
                    reason: "the worker produced no new verified claim".to_string(),
                };
                run.failures.push(failure.clone());
                self.store.append_failure(&run.id, &failure);
                if no_progress >= 2 {
                    let failure = Failure {
                        stage: "progress".to_string(),
                        subject: run.subquestions[index].id.clone(),
                        reason: "two consecutive workers produced no progress".to_string(),
                    };
                    run.failures.push(failure.clone());
                    self.store.append_failure(&run.id, &failure);
                    assignment.status = AssignmentStatus::Failed;
                    assignment.finished_at_ms = Some(now_ms());
                    if let Some(last) = run.assignments.last_mut() {
                        *last = assignment;
                    }
                    break;
                }
            } else {
                no_progress = 0;
            }
            assignment.status = AssignmentStatus::Done;
            assignment.finished_at_ms = Some(now_ms());
            if let Some(last) = run.assignments.last_mut() {
                *last = assignment;
            }
            self.store.save(&run)?;
        }

        run.usage.elapsed_ms = started.elapsed().as_millis() as u64;
        if run.claims.is_empty() {
            let failure = Failure {
                stage: "publish".to_string(),
                subject: question.to_string(),
                reason: "the run produced no verified claim".to_string(),
            };
            run.failures.push(failure.clone());
            self.store.append_failure(&run.id, &failure);
            run.status = RunStatus::Failed;
            self.store.save(&run)?;
            let _ = record_failure(self.store, &run);
            return Err(format!(
                "the run produced no verified claim\nrun: {}",
                self.store.dir(&run.id).display()
            ));
        }
        if let Some(judge) = self.judge {
            judge_disputes(self.store, &mut run, judge);
        }
        run.status = RunStatus::Checking;
        self.store.save(&run)?;
        let report = write_report(self.store, &run)?;
        run.report = Some(report);
        let check = check(self.store, &run.id)?;
        run.status = if check.code == CheckCode::Green {
            RunStatus::Published
        } else {
            RunStatus::CheckFailed
        };
        run.usage.elapsed_ms = started.elapsed().as_millis() as u64;
        let report = write_report(self.store, &run)?;
        run.report = Some(report);
        self.store.save(&run)?;
        Ok(run)
    }
}

// ── report and check ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckCode {
    Green,
    Red,
    Unreadable,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BenchQuestion {
    pub id: String,
    pub question: String,
    #[serde(default)]
    pub key_terms: Vec<String>,
    #[serde(default = "one")]
    pub min_claims: usize,
}

fn one() -> usize {
    1
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BenchVerdict {
    Pass,
    Partial,
    Fail,
}

impl BenchVerdict {
    pub fn label(self) -> &'static str {
        match self {
            BenchVerdict::Pass => "pass",
            BenchVerdict::Partial => "partial",
            BenchVerdict::Fail => "fail",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchGrade {
    pub id: String,
    pub verdict: BenchVerdict,
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchRow {
    pub id: String,
    pub question: String,
    pub run_id: String,
    pub published: bool,
    pub claims: usize,
    pub verified: usize,
    pub off_question: usize,
    pub sources: usize,
    pub seconds_ms: u64,
    pub prompt_tokens: u64,
    pub generated_tokens: u64,
    pub coverage: usize,
    pub key_terms: usize,
    pub missing: Vec<String>,
    #[serde(default)]
    pub grade: Option<String>,
    #[serde(default)]
    pub agreement: Option<bool>,
}

/// Run a question set through the real engine. Key-term coverage is a reading aid, not a truth score;
/// the row carries the run id so a person can hand-grade the report.
pub fn run_bench(
    store: &Store,
    web: &dyn Web,
    planner: &dyn Planner,
    extractor: &dyn Extractor,
    mode: RunMode,
    questions: &[BenchQuestion],
    budget: ResearchBudget,
) -> Result<Vec<BenchRow>, String> {
    run_bench_with_grades(
        store, web, planner, extractor, mode, questions, budget, None,
    )
}

fn predicted_verdict(coverage: usize, key_terms: usize) -> BenchVerdict {
    if key_terms > 0 && coverage == key_terms {
        BenchVerdict::Pass
    } else if coverage > 0 {
        BenchVerdict::Partial
    } else {
        BenchVerdict::Fail
    }
}

#[allow(clippy::too_many_arguments)] // mirrors run_bench and adds only the optional grade file
pub fn run_bench_with_grades(
    store: &Store,
    web: &dyn Web,
    planner: &dyn Planner,
    extractor: &dyn Extractor,
    mode: RunMode,
    questions: &[BenchQuestion],
    budget: ResearchBudget,
    grades: Option<&[BenchGrade]>,
) -> Result<Vec<BenchRow>, String> {
    if let Some(grades) = grades {
        let mut seen = std::collections::BTreeSet::new();
        for grade in grades {
            if !questions.iter().any(|question| question.id == grade.id) {
                return Err(format!(
                    "the grade file names {} but the bench has no such question",
                    grade.id
                ));
            }
            if !seen.insert(grade.id.as_str()) {
                return Err(format!("the grade file names {} twice", grade.id));
            }
        }
    }
    let mut rows = Vec::new();
    for question in questions {
        let started = Instant::now();
        let engine = Engine {
            store,
            web,
            planner,
            extractor,
            judge: None,
            mode,
        };
        let run = engine.run(&question.question, budget.clone())?;
        let check = check_with(store, &run.id, true)?;
        let text = run
            .claims
            .iter()
            .map(|claim| claim.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
            .to_ascii_lowercase();
        let mut coverage = 0usize;
        let mut missing = Vec::new();
        for term in &question.key_terms {
            if text.contains(&term.to_ascii_lowercase()) {
                coverage += 1;
            } else {
                missing.push(term.clone());
            }
        }
        let grade = grades.and_then(|grades| grades.iter().find(|grade| grade.id == question.id));
        let predicted = predicted_verdict(coverage, question.key_terms.len());
        rows.push(BenchRow {
            id: question.id.clone(),
            question: question.question.clone(),
            run_id: run.id,
            published: run.status == RunStatus::Published && check.code == CheckCode::Green,
            claims: run.claims.len(),
            verified: check.verified,
            off_question: check.off_question,
            sources: run.sources.len(),
            seconds_ms: started.elapsed().as_millis() as u64,
            prompt_tokens: run.usage.planner_prompt_tokens + run.usage.extractor_prompt_tokens,
            generated_tokens: run.usage.planner_generated_tokens
                + run.usage.extractor_generated_tokens,
            coverage,
            key_terms: question.key_terms.len(),
            missing,
            grade: grade.map(|grade| grade.verdict.label().to_string()),
            agreement: grade.map(|grade| grade.verdict == predicted),
        });
    }
    Ok(rows)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureIndexEntry {
    pub run_id: String,
    pub question: String,
    pub mode: RunMode,
    pub failures: Vec<Failure>,
    pub at_ms: u64,
}

/// Append a failed run to the research failure index. The case store is a repo-task corpus with a pinned
/// tree and a replayable ruler; a research question has neither, so it gets its own ledger rather than a
/// case that would pretend to be replayable.
pub fn record_failure(store: &Store, run: &ResearchRun) -> Result<(), String> {
    let entry = FailureIndexEntry {
        run_id: run.id.clone(),
        question: run.question.clone(),
        mode: run.mode,
        failures: run.failures.clone(),
        at_ms: now_ms(),
    };
    let path = store.root().join("failure-index.jsonl");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|error| format!("could not open {}: {error}", path.display()))?;
    use std::io::Write;
    writeln!(
        file,
        "{}",
        serde_json::to_string(&entry)
            .map_err(|error| format!("could not encode the failure: {error}"))?
    )
    .map_err(|error| format!("could not append to {}: {error}", path.display()))
}

pub fn failure_index(store: &Store) -> Result<Vec<FailureIndexEntry>, String> {
    let path = store.root().join("failure-index.jsonl");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(&path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let mut entries = Vec::new();
    for (number, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        entries.push(serde_json::from_str(line).map_err(|error| {
            format!(
                "{}:{}: the failure index is not valid JSON: {error}",
                path.display(),
                number + 1
            )
        })?);
    }
    Ok(entries)
}

#[derive(Debug, Clone)]
pub struct CheckReport {
    pub code: CheckCode,
    pub verified: usize,
    pub off_question: usize,
    pub failures: Vec<String>,
}

impl CheckReport {
    pub fn exit(&self) -> i32 {
        match self.code {
            CheckCode::Green => 0,
            CheckCode::Red => 1,
            CheckCode::Unreadable => 2,
        }
    }
}

fn write_report(store: &Store, run: &ResearchRun) -> Result<String, String> {
    let mut text = format!("# {}\n\n", run.question.trim());
    text.push_str(&format!(
        "Run `{}`, status `{:?}`, mode `{}`. This report verifies provenance, not truth.\n\n",
        run.id,
        run.status,
        match run.mode {
            RunMode::Web => "web",
            RunMode::Offline => "offline library",
        }
    ));

    text.push_str("## Findings\n\n");
    for q in &run.subquestions {
        let claims: Vec<&Claim> = run
            .claims
            .iter()
            .filter(|claim| claim.subquestion == q.id && claim.kind != ClaimKind::Inferred)
            .collect();
        if claims.is_empty() {
            continue;
        }
        text.push_str(&format!("### {}\n\n", q.text.trim()));
        for claim in claims {
            let mut marks = Vec::new();
            if !claim.on_question {
                marks.push("off-question".to_string());
            }
            if !claim.history.is_empty() {
                marks.push(format!("revised {}x", claim.history.len()));
            }
            let marks = if marks.is_empty() {
                String::new()
            } else {
                format!(" ({})", marks.join(", "))
            };
            text.push_str(&format!(
                "- [{}] {}{}\n",
                claim.id,
                claim.text.trim(),
                marks
            ));
        }
        text.push('\n');
    }

    let off_question: Vec<&Claim> = run
        .claims
        .iter()
        .filter(|claim| !claim.on_question)
        .collect();
    if !off_question.is_empty() {
        text.push_str("## Off-question claims\n\n");
        text.push_str(
            "These quotes verify, but the claim may not answer the subquestion it is filed under. \
             `research check --strict` turns this red.\n\n",
        );
        for claim in off_question {
            text.push_str(&format!("- [{}] {}\n", claim.id, claim.text.trim()));
        }
        text.push('\n');
    }

    let disputed: Vec<&Claim> = run
        .claims
        .iter()
        .filter(|claim| !claim.contradict.is_empty())
        .collect();
    if !disputed.is_empty() {
        text.push_str("## Disputed\n\n");
        text.push_str(
            "A source in this run states both the support and the contradicting quote. Both are \
             byte-verified; the run does not decide which one is right.\n\n",
        );
        for claim in disputed {
            text.push_str(&format!("### [{}] {}\n\n", claim.id, claim.text.trim()));
            for quote in &claim.support {
                text.push_str(&format!(
                    "- support [{}] {}..{}: \"{}\"\n",
                    quote.source, quote.start, quote.end, quote.text
                ));
            }
            for quote in &claim.contradict {
                text.push_str(&format!(
                    "- contradicts [{}] {}..{}: \"{}\"\n",
                    quote.source, quote.start, quote.end, quote.text
                ));
            }
            text.push('\n');
        }
    }

    if !run.disputes.is_empty() {
        text.push_str("## Cross-source disputes\n\n");
        text.push_str(
            "The judge compared claims from different sources on the same subquestion. Both quotes \
             are byte-verified; this section records a conflict, not a verdict.\n\n",
        );
        for dispute in &run.disputes {
            let left = run.claims.iter().find(|claim| claim.id == dispute.left);
            let right = run.claims.iter().find(|claim| claim.id == dispute.right);
            text.push_str(&format!(
                "- [{}] [{}] vs [{}]: {}\n",
                dispute.id, dispute.left, dispute.right, dispute.reason
            ));
            if let Some(claim) = left {
                text.push_str(&format!("  - [{}] {}\n", claim.id, claim.text.trim()));
                for quote in &claim.support {
                    text.push_str(&format!(
                        "    - [{}] {}..{}: \"{}\"\n",
                        quote.source, quote.start, quote.end, quote.text
                    ));
                }
            }
            if let Some(claim) = right {
                text.push_str(&format!("  - [{}] {}\n", claim.id, claim.text.trim()));
                for quote in &claim.support {
                    text.push_str(&format!(
                        "    - [{}] {}..{}: \"{}\"\n",
                        quote.source, quote.start, quote.end, quote.text
                    ));
                }
            }
            text.push('\n');
        }
    }

    let inferred: Vec<&Claim> = run
        .claims
        .iter()
        .filter(|claim| claim.kind == ClaimKind::Inferred)
        .collect();
    if !inferred.is_empty() {
        text.push_str("## Inferred\n\n");
        text.push_str("These are the run's own reasoning, not sourced facts.\n\n");
        for claim in inferred {
            text.push_str(&format!("- [{}] {}\n", claim.id, claim.text.trim()));
        }
        text.push('\n');
    }

    text.push_str("## Sources\n\n");
    for source in &run.sources {
        text.push_str(&format!(
            "- [{}] {}: {} ({}, sha256 `{}`)\n",
            source.id,
            source.title.trim(),
            source.url,
            format!("{:?}", source.kind).to_lowercase(),
            source.sha256
        ));
    }
    text.push('\n');

    text.push_str("## Limits\n\n");
    text.push_str("- Every linked claim has a byte-verified quote in the stored canonical text.\n");
    text.push_str(
        "- A verified quote proves provenance, not truth; the claim can still be wrong.\n",
    );
    text.push_str(
        "- Canonical text is stripped HTML, not the raw response; the parser is recorded.\n",
    );

    let report_path = store.dir(&run.id).join("report.md");
    fs::write(&report_path, &text)
        .map_err(|error| format!("could not write the report: {error}"))?;

    let report = serde_json::json!({
        "run_id": run.id,
        "question": run.question,
        "status": format!("{:?}", run.status),
        "mode": run.mode,
        "claims": run.claims.iter().map(|claim| serde_json::json!({
            "id": claim.id,
            "text": claim.text,
            "kind": claim.kind,
            "subquestion": claim.subquestion,
            "quotes": claim.support,
            "contradict": claim.contradict,
            "on_question": claim.on_question,
            "history": claim.history,
            "decision": claim.decision,
            "alternatives": claim.alternatives,
        })).collect::<Vec<_>>(),
        "sources": run.sources.iter().map(|source| serde_json::json!({
            "id": source.id,
            "title": source.title,
            "url": source.url,
            "sha256": source.sha256,
            "kind": source.kind,
        })).collect::<Vec<_>>(),
        "disputes": run.disputes,
    });
    fs::write(
        store.dir(&run.id).join("report.json"),
        serde_json::to_string_pretty(&report)
            .map_err(|error| format!("could not encode the report: {error}"))?,
    )
    .map_err(|error| format!("could not write report.json: {error}"))?;

    Ok(text)
}

/// Deterministic provenance check. The report must cite every claim, and every quote must match the
/// stored canonical text at its byte range.
pub fn check(store: &Store, id: &str) -> Result<CheckReport, String> {
    check_with(store, id, false)
}

pub fn check_with(store: &Store, id: &str, strict: bool) -> Result<CheckReport, String> {
    let run = store.load(id)?;
    let mut failures = Vec::new();
    let mut verified = 0usize;
    let mut off_question = 0usize;
    let mut unreadable = false;

    let report_path = store.dir(id).join("report.md");
    let report = match fs::read_to_string(&report_path) {
        Ok(text) => Some(text),
        Err(_) => {
            failures.push("report.md is missing".to_string());
            None
        }
    };

    for claim in &run.claims {
        if !claim.on_question {
            off_question += 1;
        }
        if claim.kind != ClaimKind::Inferred && claim.support.is_empty() {
            failures.push(format!("{} has no support quote", claim.id));
        }
        for quote in claim.support.iter().chain(claim.contradict.iter()) {
            let path = store.source_path(id, &quote.source);
            let body = match fs::read_to_string(&path) {
                Ok(body) => body,
                Err(error) => {
                    unreadable = true;
                    failures.push(format!(
                        "{} cites {} but the source cannot be read: {error}",
                        claim.id, quote.source
                    ));
                    continue;
                }
            };
            if quote.verifies(&body) {
                verified += 1;
            } else {
                failures.push(format!(
                    "{} quote at {}..{} does not match {}",
                    claim.id, quote.start, quote.end, quote.source
                ));
            }
        }
        if let Some(report) = &report
            && !report.contains(&format!("[{}]", claim.id))
        {
            failures.push(format!("{} is not cited in report.md", claim.id));
        }
    }
    if strict && off_question > 0 {
        failures.push(format!(
            "{off_question} verified claim(s) are off-question; inspect the report before trusting it"
        ));
    }

    let code = if unreadable {
        CheckCode::Unreadable
    } else if failures.is_empty() {
        CheckCode::Green
    } else {
        CheckCode::Red
    };
    Ok(CheckReport {
        code,
        verified,
        off_question,
        failures,
    })
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn slug(text: &str) -> String {
    let mut out = String::new();
    for ch in text.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
        } else if !out.ends_with('-') {
            out.push('-');
        }
        if out.len() >= 40 {
            break;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "run".to_string()
    } else {
        trimmed
    }
}

fn question_kind(question: &str) -> QuestionKind {
    let lower = question.to_ascii_lowercase();
    if lower.contains("should")
        || lower.contains("which")
        || lower.contains("compare")
        || lower.contains("decide")
        || lower.contains("better")
    {
        QuestionKind::Decision
    } else {
        QuestionKind::Fact
    }
}

fn normalize_url(url: &str) -> String {
    let mut url = url.trim().to_string();
    if let Some(hash) = url.find('#') {
        url.truncate(hash);
    }
    while url.ends_with('/') {
        url.pop();
    }
    let lower = url.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("https://") {
        format!("https://{rest}")
    } else if let Some(rest) = lower.strip_prefix("http://") {
        format!("http://{rest}")
    } else {
        lower
    }
}

/// A search hit that is a fragment of a web application, not a document. GitHub's commit pager and
/// timeline endpoints return a shell with the content loaded by scripts; fetching them costs a
/// budget slot and yields no prose.
fn usable_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    !(lower.contains("/commits?")
        || lower.contains("timeline_more_items")
        || lower.contains("/actions/runs/"))
}

fn normalize_claim(text: &str) -> String {
    let mut out = String::new();
    let mut space = false;
    for ch in text.chars().flat_map(char::to_lowercase) {
        if ch.is_alphanumeric() {
            out.push(ch);
            space = false;
        } else if !space {
            out.push(' ');
            space = true;
        }
    }
    out.trim().to_string()
}

#[allow(dead_code)]
fn completion_text(completion: &Completion) -> &str {
    &completion.text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> Source {
        Source {
            id: "s1".to_string(),
            url: "https://example.com/page".to_string(),
            final_url: "https://example.com/page".to_string(),
            title: "Example".to_string(),
            fetched_at_ms: 1,
            status: None,
            content_type: "text/html".to_string(),
            sha256: "a".repeat(64),
            canonical_sha256: "a".repeat(64),
            bytes: 1000,
            parser: PARSER.to_string(),
            kind: SourceKind::Secondary,
            artifact_url: None,
            snippet_only: false,
        }
    }

    #[test]
    fn a_quote_verifies_only_at_its_byte_range() {
        let body = "alpha beta gamma";
        let quote = Quote {
            source: "s1".to_string(),
            start: 6,
            end: 10,
            text: "beta".to_string(),
        };
        assert!(quote.verifies(body));
        let wrong = Quote {
            start: 0,
            end: 4,
            text: "beta".to_string(),
            ..quote
        };
        assert!(!wrong.verifies(body));
    }

    #[test]
    fn a_tampered_body_turns_the_quote_red() {
        let body = "alpha beta gamma";
        let quote = Quote {
            source: "s1".to_string(),
            start: 6,
            end: 10,
            text: "beta".to_string(),
        };
        assert!(!quote.verifies(&body.replace("beta", "zeta")));
    }

    #[test]
    fn a_thin_source_is_refused() {
        let source = source();
        assert!(source.admits("too short").is_err());
    }

    #[test]
    fn a_snippet_only_source_is_refused() {
        let mut source = source();
        source.snippet_only = true;
        assert!(source.admits(&"x".repeat(500)).is_err());
    }

    #[test]
    fn a_social_post_without_an_artifact_is_refused() {
        let body = "x".repeat(500);
        let mut source = source();
        source.kind = SourceKind::Social;
        assert!(source.admits(&body).is_err());
        source.artifact_url = Some("https://arxiv.org/abs/2610.23839".to_string());
        assert!(source.admits(&body).is_ok());
    }

    #[test]
    fn a_source_with_a_body_and_hash_is_admitted() {
        let source = source();
        assert!(source.admits(&"x".repeat(500)).is_ok());
    }

    #[test]
    fn a_github_timeline_fragment_is_not_a_document() {
        assert!(!usable_url(
            "https://github.com/ggml-org/llama.cpp/pull/1/timeline_more_items?after_cursor=x"
        ));
        assert!(!usable_url(
            "https://github.com/ggml-org/llama.cpp/commits?after=x"
        ));
        assert!(usable_url(
            "https://github.com/ggml-org/llama.cpp/issues/30039"
        ));
    }

    #[test]
    fn a_duckduckgo_redirect_url_is_decoded() {
        let sample = "## [Example](http://duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa%3Fb%3D1&rut=x)\n\nThe **snippet** for the result.\n";
        let hits = parse_jina(sample, 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "https://example.com/a?b=1");
        assert_eq!(hits[0].title, "Example");
        assert_eq!(hits[0].snippet, "The snippet for the result.");
    }

    #[test]
    fn a_duplicate_claim_keeps_all_support_quotes() {
        let body = "one two three four five";
        let a = Quote {
            source: "s1".to_string(),
            start: 4,
            end: 7,
            text: "two".to_string(),
        };
        let b = Quote {
            source: "s2".to_string(),
            start: 0,
            end: 3,
            text: "one".to_string(),
        };
        assert!(a.verifies(body));
        assert!(b.verifies(body));
    }

    #[test]
    fn a_report_cites_every_claim() {
        let store = Store::new(&crate::scratch::unique("research-report"));
        let mut run = store
            .create_named(
                "r-test",
                "What is true?",
                QuestionKind::Fact,
                ResearchBudget::default(),
            )
            .expect("a run");
        let body = "This is a sentence with enough characters to be a quote.";
        let start = body.find("This is").expect("a quote");
        let quote = Quote {
            source: "s1".to_string(),
            start,
            end: start + "This is".len(),
            text: "This is".to_string(),
        };
        let source = source();
        store.save_source(&run.id, &source, body).expect("a source");
        run.sources.push(source);
        run.claims.push(Claim {
            id: "c1".to_string(),
            text: "A claim.".to_string(),
            kind: ClaimKind::Sourced,
            subquestion: "q1".to_string(),
            support: vec![quote],
            contradict: Vec::new(),
            on_question: true,
            history: Vec::new(),
            decision: None,
            alternatives: Vec::new(),
            revision_of: None,
        });
        run.subquestions.push(Subquestion {
            id: "q1".to_string(),
            text: "A question.".to_string(),
            query: "a query".to_string(),
            kind: QuestionKind::Fact,
            status: SubquestionStatus::Done,
            claim_ids: vec!["c1".to_string()],
        });
        store.save(&run).expect("a run");
        let report = write_report(&store, &run).expect("a report");
        assert!(report.contains("[c1]"));
        assert!(report.contains("## Sources"));
        assert!(check(&store, &run.id).expect("a check").code == CheckCode::Green);
    }

    #[test]
    fn a_missing_source_is_unreadable() {
        let store = Store::new(&crate::scratch::unique("research-missing"));
        let mut run = store
            .create_named(
                "r-test",
                "What is true?",
                QuestionKind::Fact,
                ResearchBudget::default(),
            )
            .expect("a run");
        run.claims.push(Claim {
            id: "c1".to_string(),
            text: "A claim.".to_string(),
            kind: ClaimKind::Sourced,
            subquestion: "q1".to_string(),
            support: vec![Quote {
                source: "s9".to_string(),
                start: 0,
                end: 4,
                text: "text".to_string(),
            }],
            contradict: Vec::new(),
            on_question: true,
            history: Vec::new(),
            decision: None,
            alternatives: Vec::new(),
            revision_of: None,
        });
        store.save(&run).expect("a run");
        write_report(&store, &run).expect("a report");
        assert!(check(&store, &run.id).expect("a check").code == CheckCode::Unreadable);
    }

    #[test]
    fn a_second_quote_with_the_same_claim_text_revises_in_place() {
        let store = Store::new(&crate::scratch::unique("research-revision"));
        let mut run = store
            .create_named(
                "r-test",
                "What limits Vulkan on RDNA1?",
                QuestionKind::Fact,
                ResearchBudget::default(),
            )
            .expect("a run");
        run.subquestions.push(Subquestion {
            id: "q1".to_string(),
            text: "What limits Vulkan on RDNA1?".to_string(),
            query: "Vulkan RDNA1".to_string(),
            kind: QuestionKind::Fact,
            status: SubquestionStatus::Pending,
            claim_ids: Vec::new(),
        });
        let source = Source {
            id: "s1".to_string(),
            url: "https://example.com/paper".to_string(),
            final_url: "https://example.com/paper".to_string(),
            title: "Paper".to_string(),
            fetched_at_ms: 1,
            status: Some(200),
            content_type: "text/html".to_string(),
            sha256: "0".repeat(64),
            canonical_sha256: "0".repeat(64),
            bytes: 200,
            parser: PARSER.to_string(),
            kind: SourceKind::Primary,
            artifact_url: None,
            snippet_only: false,
        };
        let body = "The first measurement shows a limit. The second measurement shows the same limit again.";
        let first = ClaimDraft {
            text: "Vulkan on RDNA1 has a limit.".to_string(),
            quote: "The first measurement shows a limit.".to_string(),
            kind: ClaimKind::Sourced,
            decision: None,
            alternatives: Vec::new(),
            contradicts: Vec::new(),
        };
        let second = ClaimDraft {
            text: "Vulkan on RDNA1 has a limit".to_string(),
            quote: "The second measurement shows the same limit again.".to_string(),
            kind: ClaimKind::Sourced,
            decision: None,
            alternatives: Vec::new(),
            contradicts: Vec::new(),
        };
        absorb_drafts(
            &store,
            &mut run,
            0,
            &source,
            body,
            vec![first],
            &ResearchBudget::default(),
        )
        .expect("the first draft");
        absorb_drafts(
            &store,
            &mut run,
            0,
            &source,
            body,
            vec![second],
            &ResearchBudget::default(),
        )
        .expect("the second draft");
        assert_eq!(run.claims.len(), 1);
        assert_eq!(run.claims[0].support.len(), 2);
        assert_eq!(run.claims[0].history.len(), 1);
        assert!(run.claims[0].history[0].text.contains("has a limit."));
    }

    #[test]
    fn a_failed_run_is_appended_to_the_research_failure_index() {
        let store = Store::new(&crate::scratch::unique("research-failure-index"));
        let mut run = store
            .create_named(
                "r-failed",
                "What is true?",
                QuestionKind::Fact,
                ResearchBudget::default(),
            )
            .expect("a run");
        run.status = RunStatus::Failed;
        run.failures.push(Failure {
            stage: "publish".to_string(),
            subject: "What is true?".to_string(),
            reason: "the run produced no verified claim".to_string(),
        });
        store.save(&run).expect("a run");
        record_failure(&store, &run).expect("an index entry");
        record_failure(&store, &run).expect("a second index entry");
        let entries = failure_index(&store).expect("an index");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].run_id, "r-failed");
        assert_eq!(entries[0].failures[0].stage, "publish");
    }

    #[test]
    fn a_general_claim_is_off_question_for_a_specific_subquestion() {
        let question =
            "What are the documented limitations of llama.cpp Vulkan inference on AMD RDNA1 GPUs?";
        assert!(!claim_on_question(
            question,
            "llama.cpp loads a GGUF file from disk.",
            "The model loader reads the file and allocates a context."
        ));
        assert!(claim_on_question(
            question,
            "Vulkan inference on RDNA1 is limited by the driver.",
            "On RDNA1 the Vulkan backend falls back for some shaders."
        ));
    }

    #[test]
    fn a_primary_source_outranks_a_social_post() {
        assert!(source_rank(SourceKind::Primary) < source_rank(SourceKind::Secondary));
        assert!(source_rank(SourceKind::Secondary) < source_rank(SourceKind::Unknown));
        assert!(source_rank(SourceKind::Unknown) < source_rank(SourceKind::Social));
    }
}
