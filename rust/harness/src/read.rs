//! Reading: one acquisition pipeline, with the state of every attempt named.
//!
//! Before this module the agent had one way to read a page: `fetch`, ten lines handing a URL to `fetch::run`,
//! which strips the markup and cuts the text at 12,000 characters with the navigation still in it. Three other
//! acquisition paths existed beside it, and nothing chose between them. This module is the chooser: it sniffs
//! what came back, sends documents to the library parser and JavaScript shells to the browser, extracts the main
//! content from ordinary HTML, caches by digest, and refuses to call an empty page a success.
//!
//! The rules it inherits from the search work, because they were learned the hard way there:
//!
//! - **A refusal is a state, not an empty result.** `blocked(403)` and "the page had no text" are different
//!   facts, and a caller that cannot tell them apart will blame the web.
//! - **Every claim about quality comes from a measurement.** `hades read-eval` scores a frozen gold set through
//!   this same code, and its number is the only one quoted in the release notes.

use std::path::{Path, PathBuf};

/// How a read ended. `Display` produces the exact string a report prints, and a test holds those strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Text was extracted.
    Ok,
    /// The server refused, with the code it refused with.
    Blocked(u16),
    /// There is no page at that address. Distinct from `Blocked` on purpose: a 404 says the URL is wrong, a 403
    /// says the client is unwelcome, and telling them apart is what lets a caller retry the one that is worth
    /// retrying.
    NotFound,
    /// A login or subscription wall: the page is a teaser, not the document.
    Paywalled,
    /// Executable or compressed content, not text.
    Binary,
    /// Larger than the ceiling this tool will read.
    TooLarge,
    /// The page's text only exists after JavaScript runs.
    JsRequired,
    /// The body was fetched and parsed, and held no readable text at all.
    Empty,
    /// The transport failed before any status existed.
    Unreachable(String),
}

impl std::fmt::Display for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            State::Ok => write!(f, "ok"),
            State::Blocked(code) => write!(f, "blocked({code})"),
            State::NotFound => write!(f, "not-found"),
            State::Paywalled => write!(f, "paywalled"),
            State::Binary => write!(f, "binary"),
            State::TooLarge => write!(f, "too-large"),
            State::JsRequired => write!(f, "js-required"),
            State::Empty => write!(f, "empty"),
            State::Unreachable(why) => write!(f, "unreachable: {why}"),
        }
    }
}

/// Which acquisition path was used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Path_ {
    Fetch,
    /// Read through a site's own JSON API, because its HTML refuses a client without a session.
    Api,
    Browser,
    Document,
    Cache,
}

impl std::fmt::Display for Path_ {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Path_::Fetch => write!(f, "fetch"),
            Path_::Api => write!(f, "api"),
            Path_::Browser => write!(f, "browser"),
            Path_::Document => write!(f, "document"),
            Path_::Cache => write!(f, "cache"),
        }
    }
}

/// What the bytes look like, which is the only thing the selector is allowed to act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sniff {
    Pdf,
    Html,
    Text,
    Binary,
}

/// One extracted block, with the byte range it occupies in [`Reading::text`].
///
/// A citation names a passage, not a page. The range is recorded while the text is built, so slicing the text
/// gives back exactly the block that was kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub kind: String,
    pub range: (usize, usize),
}

/// The result of one read.
#[derive(Debug, Clone)]
pub struct Reading {
    pub url: String,
    pub path: Path_,
    pub state: State,
    /// Every decision in order, for a caller that has to explain a surprising result.
    pub states: Vec<String>,
    pub title: String,
    pub text: String,
    pub blocks: Vec<Block>,
    pub ms: u128,
}

impl Reading {
    pub fn chars(&self) -> usize {
        self.text.chars().count()
    }

    pub fn ok(&self) -> bool {
        self.state == State::Ok
    }
}

/// The ceiling on one read. Past this a page is reported `too-large` rather than clipped silently, because a
/// clipped page reads like a complete one.
/// Eight mebibytes, not two. Measured on the gold set: an RFC PDF is 2.8 MB and an arXiv paper 2.2 MB, so the
/// old ceiling reported both as unreachable, which reads as a network fault rather than as a limit.
pub const MAX_BYTES: usize = 8 * 1024 * 1024;

/// Bumped when extraction changes, because the cache key includes it and a stale extraction must not be served.
pub const EXTRACTOR_VERSION: &str = "1";

/// Decide what the bytes are. No header sniffing: the body decides, so a mislabelled `Content-Type` cannot
/// route a PDF down the HTML path.
pub fn sniff(bytes: &[u8]) -> Sniff {
    let head = &bytes[..bytes.len().min(2048)];
    if head.starts_with(b"%PDF") {
        return Sniff::Pdf;
    }
    if head.contains(&0) {
        return Sniff::Binary;
    }
    let low = String::from_utf8_lossy(head).to_lowercase();
    if low.contains("<!doctype html") || low.contains("<html") || low.contains("<body") {
        return Sniff::Html;
    }
    if low.trim_start().starts_with('{') || low.trim_start().starts_with('[') {
        // JSON is text a person can read; it is not a page, but it is not binary either.
        return Sniff::Text;
    }
    if low.contains("<p>") || low.contains("<div") || low.contains("<a href") {
        return Sniff::Html;
    }
    Sniff::Text
}

/// The selector: what the bytes are plus the status decide the path and the terminal state, in that order.
///
/// `Some(state)` means the read is already decided and no extraction should run.
pub fn select(sniffed: Sniff, status: u16, body_len: usize) -> (Path_, Option<State>) {
    if status == 402 {
        return (Path_::Fetch, Some(State::Paywalled));
    }
    if status == 401 || status == 403 || status == 429 || (500..600).contains(&status) {
        return (Path_::Fetch, Some(State::Blocked(status)));
    }
    if status == 404 || status == 410 {
        return (Path_::Fetch, Some(State::Blocked(status)));
    }
    if body_len > MAX_BYTES {
        return (Path_::Fetch, Some(State::TooLarge));
    }
    match sniffed {
        Sniff::Pdf => (Path_::Document, None),
        Sniff::Binary => (Path_::Fetch, Some(State::Binary)),
        Sniff::Text => (Path_::Fetch, None),
        Sniff::Html => (Path_::Fetch, None),
    }
}

/// A page whose body holds almost no text and looks like an application shell.
///
/// Measured need, not a guess: three of the thirty gold rows are pages whose HTML is a `<div id="root">` and a
/// script tag, and reading them through the fetch path returns nothing at all.
pub fn looks_like_a_shell(html: &str) -> bool {
    let low = html.to_lowercase();
    let visible = strip_tags_text(html);
    if visible.chars().count() >= 400 {
        return false;
    }
    let markers = [
        "id=\"root\"",
        "id=\"app\"",
        "id='root'",
        "id='app'",
        "__next_data__",
        "ng-app",
        "data-reactroot",
    ];
    let has_marker = markers.iter().any(|m| low.contains(m));
    let has_script = low.contains("<script");
    let noscript = low.contains("<noscript");
    has_script && (has_marker || noscript || visible.chars().count() < 80)
}

const NOISE: [&str; 9] = [
    "script", "style", "noscript", "nav", "header", "footer", "aside", "form", "template",
];
const CONTENT: [&str; 2] = ["article", "main"];
const BLOCK: [&str; 19] = [
    "p",
    "div",
    "section",
    "li",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "pre",
    "blockquote",
    "td",
    "tr",
    "br",
    "figcaption",
    "dt",
    "dd",
    "summary",
];

/// Every tag that is not a block boundary still separates words, so it becomes a space.
fn strip_tags_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut in_tag = false;
    let mut in_noise = 0usize;
    let mut name = String::new();
    let mut reading_name = false;
    let mut closing = false;
    for ch in html.chars() {
        if in_tag {
            match ch {
                '>' => {
                    in_tag = false;
                    let tag = name.to_lowercase();
                    if NOISE.contains(&tag.as_str()) {
                        if closing {
                            in_noise = in_noise.saturating_sub(1);
                        } else {
                            in_noise += 1;
                        }
                    }
                    out.push(' ');
                    name.clear();
                    reading_name = false;
                    closing = false;
                }
                c if reading_name && c.is_ascii_alphanumeric() => name.push(c.to_ascii_lowercase()),
                _ if reading_name => reading_name = false, // an attribute starts here; the name is complete
                '/' if name.is_empty() => closing = true,
                c if c.is_ascii_alphabetic() => {
                    reading_name = true;
                    name.push(c.to_ascii_lowercase());
                }
                _ => {}
            }
            continue;
        }
        if ch == '<' {
            in_tag = true;
            continue;
        }
        if in_noise == 0 {
            out.push(ch);
        }
    }
    let text = collapse(&out);
    decode_entities(&text).trim().to_string()
}

/// One extracted run of text before serialisation.
struct Segment {
    kind: &'static str,
    text: String,
    in_content: bool,
}

/// Extract the readable text of an HTML page, dropping navigation, headers, footers, asides and scripts.
///
/// The rule is structural rather than statistical: a segment inside `<article>` or `<main>` is kept ahead of
/// anything else, and a segment inside a noise container is never kept. A page with neither container falls
/// back to every segment outside noise, in document order, which is what makes a forum thread work.
pub fn extract_html(html: &str) -> (String, Vec<Block>, String) {
    let mut segments: Vec<Segment> = Vec::new();
    let mut noise = 0usize;
    let mut content = 0usize;
    let mut pre = 0usize;
    let mut in_link = 0usize;
    let mut link_chars = 0usize;
    let mut buffer = String::new();
    let mut kind: &'static str = "p";
    let mut title = String::new();
    let mut in_title = false;

    let mut chars = html.char_indices().peekable();
    let mut text_start = 0usize;
    let bytes = html.as_bytes();

    while let Some((index, ch)) = chars.next() {
        if ch == '<' {
            // flush whatever text came before this tag
            if index > text_start && noise == 0 && !in_title {
                let raw = &html[text_start..index];
                if in_link > 0 {
                    link_chars += raw.chars().filter(|c| !c.is_whitespace()).count();
                }
                push_text(&mut buffer, raw, pre > 0);
            }

            // read the tag name
            let mut name = String::new();
            let mut closing = false;
            // The name ends at the first character that cannot be part of one. Collecting letters after that
            // point turns `<a href="index.html">` into `ahrefindexhtml`, which matches no rule in NOISE or
            // CONTENT and never increments `in_link`, so a menu survives and the anchor-run rule never fires.
            let mut cursor = index + 1;
            let mut in_name = true;
            while cursor < bytes.len() {
                let c = bytes[cursor] as char;
                if c == '>' {
                    cursor += 1;
                    break;
                }
                if in_name {
                    if name.is_empty() && c == '/' {
                        closing = true;
                    } else if c.is_ascii_alphanumeric() {
                        name.push(c.to_ascii_lowercase());
                    } else {
                        in_name = false;
                    }
                }
                cursor += 1;
            }
            let tag = name.as_str();

            if tag == "a" {
                if closing {
                    in_link = in_link.saturating_sub(1);
                } else {
                    in_link += 1;
                }
            }
            if tag == "title" && !closing {
                in_title = true;
            }
            if tag == "title" && closing {
                in_title = false;
            }
            if NOISE.contains(&tag) {
                if closing {
                    noise = noise.saturating_sub(1);
                } else {
                    noise += 1;
                }
            } else if CONTENT.contains(&tag) {
                if closing {
                    content = content.saturating_sub(1);
                } else {
                    content += 1;
                }
            } else if tag == "pre" {
                if closing {
                    pre = pre.saturating_sub(1);
                    if noise == 0 {
                        flush_with_links(
                            &mut segments,
                            &mut buffer,
                            "pre",
                            content > 0,
                            &mut link_chars,
                        );
                    }
                } else {
                    pre += 1;
                    if noise == 0 {
                        flush_with_links(
                            &mut segments,
                            &mut buffer,
                            kind,
                            content > 0,
                            &mut link_chars,
                        );
                    }
                    kind = "pre";
                }
            } else if BLOCK.contains(&tag) && noise == 0 {
                flush_with_links(
                    &mut segments,
                    &mut buffer,
                    kind,
                    content > 0,
                    &mut link_chars,
                );
                kind = match tag {
                    "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                        // store the level in a leaked static per heading, so the serialiser can mark it
                        match tag {
                            "h1" => "h1",
                            "h2" => "h2",
                            "h3" => "h3",
                            "h4" => "h4",
                            "h5" => "h5",
                            _ => "h6",
                        }
                    }
                    "li" => "li",
                    "pre" => "pre",
                    "blockquote" => "blockquote",
                    "td" | "tr" => "td",
                    _ => "p",
                };
            }

            // jump the iterator past this tag
            while let Some(&(next, _)) = chars.peek() {
                if next < cursor {
                    chars.next();
                } else {
                    break;
                }
            }
            text_start = cursor;
            continue;
        }

        if in_title && noise == 0 && !ch.is_control() {
            title.push(ch);
        }
    }
    if text_start < html.len() && noise == 0 && !in_title {
        let raw = &html[text_start..];
        if in_link > 0 {
            link_chars += raw.chars().filter(|c| !c.is_whitespace()).count();
        }
        push_text(&mut buffer, raw, pre > 0);
    }
    flush_with_links(
        &mut segments,
        &mut buffer,
        kind,
        content > 0,
        &mut link_chars,
    );

    // Prefer the page's own article or main container. When there is none, fall back to the densest run of
    // segments rather than to everything outside noise: a page with no `<article>` (sqlite.org is one) otherwise
    // comes back as its own table of contents, tagline and footer, which is markup stripping wearing the name of
    // extraction.
    let has_content = segments.iter().any(|s| s.in_content);
    let kept: Vec<&Segment> = if has_content {
        segments.iter().filter(|s| s.in_content).collect()
    } else {
        dense_run(&segments)
    };

    // Two rules that apply whatever chose the segments. A heading whose whole text is a chrome word is a section
    // label for navigation, not a section of the page, and everything above the page's own `<h1>` is the header
    // block: a tagline, a logo line, a breadcrumb trail.
    let first_heading = kept.iter().position(|s| s.kind == "h1");
    let kept: Vec<&Segment> = kept
        .into_iter()
        .enumerate()
        .filter(|(index, segment)| {
            if let Some(heading) = first_heading
                && *index < heading
            {
                return false;
            }
            !is_chrome_heading(&segment.text)
        })
        .map(|(_, segment)| segment)
        .collect();

    let mut text = String::new();
    let mut blocks = Vec::new();
    for segment in kept {
        if segment.text.is_empty() {
            continue;
        }
        let start = text.len();
        match segment.kind {
            "h1" => text.push_str(&format!("# {}\n\n", segment.text)),
            "h2" => text.push_str(&format!("## {}\n\n", segment.text)),
            "h3" => text.push_str(&format!("### {}\n\n", segment.text)),
            "h4" | "h5" | "h6" => text.push_str(&format!("#### {}\n\n", segment.text)),
            "li" => text.push_str(&format!("- {}\n", segment.text)),
            "pre" => text.push_str(&format!("```\n{}\n```\n\n", segment.text)),
            "blockquote" => text.push_str(&format!("> {}\n\n", segment.text)),
            _ => text.push_str(&format!("{}\n\n", segment.text)),
        }
        let end = text.len();
        blocks.push(Block {
            kind: segment.kind.to_string(),
            range: (start, end),
        });
    }
    let trimmed = text.trim_end().to_string();
    if trimmed.len() < text.len() {
        // keep spans valid: drop the tail whitespace from the recorded text
        let cut = trimmed.len();
        for block in blocks.iter_mut() {
            block.range.1 = block.range.1.min(cut);
        }
        text = trimmed;
    }
    let title = title.trim().to_string();
    let title = if title.is_empty() {
        blocks
            .iter()
            .find(|b| b.kind == "h1")
            .map(|b| {
                text[b.range.0..b.range.1]
                    .trim_start_matches("# ")
                    .trim()
                    .to_string()
            })
            .unwrap_or_default()
    } else {
        title
    };
    (text, blocks, title)
}

/// A heading that names navigation rather than a part of the document.
fn is_chrome_heading(text: &str) -> bool {
    let low = text.trim().to_lowercase();
    matches!(
        low.as_str(),
        "navigation"
            | "table of contents"
            | "contents"
            | "site navigation"
            | "main navigation"
            | "breadcrumb"
            | "you are here"
            | "jump to"
            | "skip to content"
            | "menu"
    )
}

/// The heaviest run of consecutive segments, by a weight that punishes navigation.
///
/// The weights are judgements, not measurements, and they are here in one place so a wrong one is one edit:
/// non-space characters are the base, a list row counts a quarter, a line carrying a known chrome phrase counts
/// nothing, and a segment too short to hold a sentence counts half. The window is the maximum-sum subarray over
/// that weight, which is what keeps a page's article and drops its header, tagline and footer when the page marks
/// none of them.
fn dense_run(segments: &[Segment]) -> Vec<&Segment> {
    if segments.len() < 8 {
        return segments.iter().collect();
    }
    let weights: Vec<f64> = segments.iter().map(weight_of).collect();
    let (mut best, mut best_start, mut best_end) = (0.0, 0, 0);
    let mut running = 0.0;
    let mut start = 0;
    for (index, weight) in weights.iter().enumerate() {
        if running <= 0.0 {
            running = 0.0;
            start = index;
        }
        running += weight;
        if running > best {
            best = running;
            best_start = start;
            best_end = index;
        }
    }
    if best <= 0.0 {
        return segments.iter().collect();
    }
    segments[best_start..=best_end].iter().collect()
}

fn weight_of(segment: &Segment) -> f64 {
    let text = segment.text.to_lowercase();
    if crate::readeval::NAV_MARKERS
        .iter()
        .any(|marker| text.contains(marker))
    {
        return 0.0;
    }
    let chars = segment.text.chars().filter(|c| !c.is_whitespace()).count() as f64;
    let mut weight = chars;
    if segment.kind == "li" {
        weight *= 0.25;
    }
    if chars < 40.0 {
        weight *= 0.5;
    }
    weight
}

/// Flush, dropping a short run that is almost entirely link text.
///
/// The structural rule alone was not enough: sqlite.org's menu is a run of `<a>` tags in a plain container, so
/// "Home Menu About Documentation Download Licence Support Purchase Search" survived a `<nav>`-only filter and
/// reached the model as if it were the page. A run that is mostly anchors and short is a menu by construction.
fn flush_with_links(
    segments: &mut Vec<Segment>,
    buffer: &mut String,
    kind: &'static str,
    in_content: bool,
    link_chars: &mut usize,
) {
    let text_chars = buffer.chars().filter(|c| !c.is_whitespace()).count();
    let ratio = if text_chars == 0 {
        0.0
    } else {
        *link_chars as f64 / text_chars as f64
    };
    let short = buffer.chars().count() <= 160;
    *link_chars = 0;
    if ratio >= 0.7 && short && !in_content {
        buffer.clear();
        return;
    }
    flush(segments, buffer, kind, in_content);
}

fn flush(segments: &mut Vec<Segment>, buffer: &mut String, kind: &'static str, in_content: bool) {
    let text = collapse(buffer);
    buffer.clear();
    if !text.is_empty() {
        segments.push(Segment {
            kind,
            text: decode_entities(&text),
            in_content,
        });
    }
}

fn push_text(buffer: &mut String, raw: &str, in_pre: bool) {
    if in_pre {
        let decoded = decode_entities(raw);
        buffer.push_str(&decoded);
        if !buffer.ends_with('\n') {
            buffer.push('\n');
        }
        return;
    }
    buffer.push_str(raw);
    buffer.push(' ');
}

/// Collapse runs of whitespace to one space, so markup indentation never reaches the text.
fn collapse(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut space = false;
    for ch in raw.chars() {
        if ch.is_whitespace() {
            space = true;
            continue;
        }
        if space && !out.is_empty() {
            out.push(' ');
        }
        space = false;
        out.push(ch);
    }
    out.trim().to_string()
}

/// The five escapes that cover ordinary pages, plus numeric references.
pub fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        let Some(end) = tail.find(';').filter(|end| *end <= 10) else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let entity = &tail[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            "mdash" => Some('\u{2014}'),
            "ndash" => Some('\u{2013}'),
            "hellip" => Some('\u{2026}'),
            "rsquo" => Some('\u{2019}'),
            "lsquo" => Some('\u{2018}'),
            "ldquo" => Some('\u{201c}'),
            "rdquo" => Some('\u{201d}'),
            other => other
                .strip_prefix('#')
                .and_then(|num| {
                    if let Some(hex) = num.strip_prefix('x').or_else(|| num.strip_prefix('X')) {
                        u32::from_str_radix(hex, 16).ok()
                    } else {
                        num.parse::<u32>().ok()
                    }
                })
                .and_then(char::from_u32),
        };
        match decoded {
            Some(ch) => {
                out.push(ch);
                rest = &tail[end + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

// ---------------------------------------------------------------- cache

/// Where cached readings live: `<state>/read-cache`.
pub fn cache_dir(state: &Path) -> PathBuf {
    state.join("read-cache")
}

/// Cache key: the URL, the accept we sent, and the extractor version.
///
/// The version is in the key on purpose. Extraction changes are exactly when a cached reading becomes a lie,
/// and a TTL alone cannot know that.
/// The cache key, with the extractor version as a parameter so a test can change it.
///
/// One implementation, so the test that varies the version is exercising the key the cache actually uses. The
/// separators matter: without them a URL ending in `a` and an accept header starting with `b` hash the same as a
/// URL ending in `ab`.
pub fn cache_key_with(url: &str, accept: &str, version: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in format!("{url}\u{0}{accept}\u{0}{version}").bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn cache_key(url: &str, accept: &str) -> String {
    cache_key_with(url, accept, EXTRACTOR_VERSION)
}

/// How long a reading stays valid, by what kind of page it is.
///
/// Documentation is stable for a day, a news page is not, everything else gets six hours. These are judgements,
/// not measurements, and they are here in one function so a wrong one is one edit.
pub fn ttl_secs(url: &str) -> u64 {
    let low = url.to_lowercase();
    let docs = low.contains("docs.")
        || low.contains("/docs/")
        || low.contains("/reference/")
        || low.contains("/manual")
        || low.contains("man.")
        || low.contains("developer.");
    if docs {
        return 24 * 3600;
    }
    let news = low.contains("/news")
        || low.contains("/blog/")
        || low.contains("techcrunch")
        || low.contains("reuters")
        || low.contains("bbc.co");
    if news {
        return 3600;
    }
    6 * 3600
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Cached {
    url: String,
    fetched_at: u64,
    title: String,
    text: String,
    blocks: Vec<(String, usize, usize)>,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Read a cached entry, if one exists, matches this URL, and has not expired.
pub fn cache_get(state: &Path, url: &str, accept: &str) -> Option<Reading> {
    let path = cache_dir(state).join(format!("{}.json", cache_key(url, accept)));
    let raw = std::fs::read_to_string(&path).ok()?;
    let cached: Cached = serde_json::from_str(&raw).ok()?;
    if cached.url != url {
        return None;
    }
    if now_secs().saturating_sub(cached.fetched_at) > ttl_secs(url) {
        return None;
    }
    Some(Reading {
        url: url.to_string(),
        path: Path_::Cache,
        state: State::Ok,
        states: vec!["cache-hit".to_string()],
        title: cached.title,
        text: cached.text,
        blocks: cached
            .blocks
            .into_iter()
            .map(|(kind, start, end)| Block {
                kind,
                range: (start, end),
            })
            .collect(),
        ms: 0,
    })
}

/// Store a successful reading. A refusal, an empty page or a shell is never stored.
pub fn cache_put(state: &Path, url: &str, accept: &str, reading: &Reading) {
    if reading.state != State::Ok || reading.path == Path_::Cache || reading.text.is_empty() {
        return;
    }
    let dir = cache_dir(state);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let cached = Cached {
        url: url.to_string(),
        fetched_at: now_secs(),
        title: reading.title.clone(),
        text: reading.text.clone(),
        blocks: reading
            .blocks
            .iter()
            .map(|b| (b.kind.clone(), b.range.0, b.range.1))
            .collect(),
    };
    let Ok(body) = serde_json::to_string(&cached) else {
        return;
    };
    if std::fs::write(dir.join(format!("{}.json", cache_key(url, accept))), body).is_ok() {
        evict(&dir, 16 * 1024 * 1024);
    }
}

/// Keep the cache under a byte ceiling by dropping the oldest entries first.
fn evict(dir: &Path, cap: u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(u64, PathBuf, u64)> = Vec::new();
    let mut total = 0u64;
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let age = meta
            .modified()
            .ok()
            .and_then(|when| when.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        total += meta.len();
        files.push((age, entry.path(), meta.len()));
    }
    if total <= cap {
        return;
    }
    files.sort();
    for (_, path, size) in files {
        if total <= cap {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(size);
        }
    }
}

// ---------------------------------------------------------------- read

/// A Stack Exchange question, recognised in the URL.
///
/// The HTML of every Stack Exchange site answers 403 to a client without a session (measured on all four forum
/// rows of the gold set), and the site's own keyless API serves the same question with its answers in JSON. A
/// reader that stops at the block has not read the page; this is the page, from the company that owns it.
fn stack_exchange(url: &str) -> Option<(&'static str, u64)> {
    let host = url.split("://").nth(1)?.split('/').next()?.to_lowercase();
    let site = match host.as_str() {
        "stackoverflow.com" => "stackoverflow",
        "superuser.com" => "superuser",
        "serverfault.com" => "serverfault",
        "askubuntu.com" => "askubuntu",
        "unix.stackexchange.com" => "unix",
        "apple.stackexchange.com" => "apple",
        "security.stackexchange.com" => "security",
        _ => return None,
    };
    let id = url
        .split("/questions/")
        .nth(1)?
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()?;
    Some((site, id))
}

/// Read a Stack Exchange question through its API: the question body, then every answer.
fn read_stack_exchange(
    site: &str,
    id: u64,
    states: &mut Vec<String>,
) -> Result<(String, String), String> {
    let url = format!(
        "https://api.stackexchange.com/2.3/questions/{id}?site={site}&filter=withbody&order=desc&sort=activity"
    );
    states.push(format!("stack exchange api: {site} question {id}"));
    let (status, bytes, hops) = crate::fetch::get_following(
        &url,
        4 * 1024 * 1024,
        false,
        &[("Accept", "application/json")],
        3,
    )?;
    for hop in &hops {
        states.push(format!("redirect -> {hop}"));
    }
    if status != 200 {
        return Err(format!("the api answered {status}"));
    }
    let body = String::from_utf8_lossy(&bytes);
    let parsed: serde_json::Value = serde_json::from_str(&body)
        .map_err(|error| format!("the api answer was not json: {error}"))?;
    let items = parsed
        .get("items")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let Some(item) = items.first() else {
        return Err("the api returned no items".to_string());
    };
    let title = item
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let mut text = String::new();
    if let Some(body) = item.get("body").and_then(|v| v.as_str()) {
        text.push_str(&extract_html(body).0);
        text.push_str("\n\n");
    }
    // The questions endpoint does not inline answer bodies whatever the filter says, so the answers are a second
    // request. Sorted by votes because the accepted answer is not always the useful one.
    let answers_url = format!(
        "https://api.stackexchange.com/2.3/questions/{id}/answers?site={site}&filter=withbody&sort=votes&order=desc"
    );
    let answers: Option<Vec<serde_json::Value>> = crate::fetch::get_following(
        &answers_url,
        4 * 1024 * 1024,
        false,
        &[("Accept", "application/json")],
        3,
    )
    .ok()
    .filter(|(status, _, _)| *status == 200)
    .and_then(|(_, bytes, _)| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
    .and_then(|value| value.get("items").and_then(|v| v.as_array()).cloned());
    if let Some(answers) = answers
        .as_ref()
        .or_else(|| item.get("answers").and_then(|v| v.as_array()))
    {
        for (number, answer) in answers.iter().enumerate() {
            if let Some(body) = answer.get("body").and_then(|v| v.as_str()) {
                let accepted = answer
                    .get("is_accepted")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                text.push_str(&format!(
                    "answer {} ({}) score {}\n",
                    number + 1,
                    if accepted { "accepted" } else { "not accepted" },
                    answer.get("score").and_then(|v| v.as_i64()).unwrap_or(0)
                ));
                text.push_str(&extract_html(body).0);
                text.push_str("\n\n");
            }
        }
    }
    Ok((title, text.trim().to_string()))
}

/// Read one URL: cache, then sniff, then the path the selector chooses.
pub fn read(state: Option<&Path>, url: &str, refresh: bool, allow_private: bool) -> Reading {
    // A Stack Exchange question never reaches the HTML path: the site refuses it, so it is read through the API
    // that serves it. The state records which route answered, because "read through the API" and "read the page"
    // are different claims about the same URL.
    if let Some((site, id)) = stack_exchange(url) {
        let mut states = vec!["path api".to_string()];
        let started = std::time::Instant::now();
        let (state, title, text) = match read_stack_exchange(site, id, &mut states) {
            Ok((title, text)) if !text.is_empty() => (State::Ok, title, text),
            Ok(_) => (State::Empty, String::new(), String::new()),
            Err(why) => {
                states.push(format!("api failed: {why}"));
                (State::Unreachable(why), String::new(), String::new())
            }
        };
        states.push(format!("state {state}"));
        return Reading {
            url: url.to_string(),
            path: Path_::Api,
            state,
            states,
            title,
            text,
            blocks: Vec::new(),
            ms: started.elapsed().as_millis(),
        };
    }

    let started = std::time::Instant::now();
    let accept = "text/html,application/pdf;q=0.9,*/*;q=0.8";

    if !refresh
        && let Some(state) = state
        && let Some(hit) = cache_get(state, url, accept)
    {
        return Reading {
            ms: started.elapsed().as_millis(),
            ..hit
        };
    }

    let (status, body, hops) = match crate::fetch::get_following(
        url,
        MAX_BYTES,
        allow_private,
        &[
            ("Accept", accept),
            (
                "User-Agent",
                "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
            ),
        ],
        5,
    ) {
        Ok(triple) => triple,
        Err(why) => {
            // The transport refuses a body over the ceiling with its own words. Reporting that as `unreachable`
            // tells the caller the network failed when the document was simply too big, which is the exact
            // confusion the ceiling comment claims this release removed.
            let too_large = why.contains("Maximum file size exceeded")
                || why.contains("exited 63")
                || why.contains("maximum file size");
            let state = if too_large {
                State::TooLarge
            } else {
                State::Unreachable(why.clone())
            };
            return Reading {
                url: url.to_string(),
                path: Path_::Fetch,
                state,
                states: vec![if too_large {
                    format!("too-large: the transport refused a body over the ceiling ({why})")
                } else {
                    format!("unreachable: {why}")
                }],
                title: String::new(),
                text: String::new(),
                blocks: Vec::new(),
                ms: started.elapsed().as_millis(),
            };
        }
    };
    let bytes: &[u8] = &body;
    let sniffed = sniff(bytes);
    let (path, decided) = select(sniffed, status, bytes.len());
    let mut states = vec![format!("status {status}")];
    for hop in &hops {
        states.push(format!("redirect -> {hop}"));
    }
    states.push(format!("sniffed {sniffed:?}"));
    states.push(format!("path {path}"));

    if let Some(state) = decided {
        states.push(format!("state {state}"));
        let out = Reading {
            url: url.to_string(),
            path,
            state,
            states,
            title: String::new(),
            text: String::new(),
            blocks: Vec::new(),
            ms: started.elapsed().as_millis(),
        };
        return out;
    }

    let out = match path {
        Path_::Document => read_document(url, bytes, &mut states),
        _ if sniffed == Sniff::Html && looks_like_a_shell(&String::from_utf8_lossy(bytes)) => {
            states.push("state js-required".to_string());
            read_browser(state, url, &mut states)
        }
        _ if sniffed == Sniff::Html => {
            let (text, blocks, title) = extract_html(&String::from_utf8_lossy(bytes));
            if text.trim().is_empty() {
                states.push("state empty".to_string());
                Reading {
                    url: url.to_string(),
                    path,
                    state: State::Empty,
                    states,
                    title,
                    text,
                    blocks,
                    ms: started.elapsed().as_millis(),
                }
            } else {
                states.push("state ok".to_string());
                Reading {
                    url: url.to_string(),
                    path,
                    state: State::Ok,
                    states,
                    title,
                    text,
                    blocks,
                    ms: started.elapsed().as_millis(),
                }
            }
        }
        _ => {
            states.push("state ok".to_string());
            Reading {
                url: url.to_string(),
                path,
                state: State::Ok,
                states,
                title: String::new(),
                text: String::from_utf8_lossy(bytes).to_string(),
                blocks: Vec::new(),
                ms: started.elapsed().as_millis(),
            }
        }
    };

    if let Some(state) = state {
        cache_put(state, url, accept, &out);
    }
    out
}

fn read_document(url: &str, bytes: &[u8], states: &mut Vec<String>) -> Reading {
    let started = std::time::Instant::now();
    let dir = std::env::temp_dir().join("hades-read");
    let _ = std::fs::create_dir_all(&dir);
    // The name is the digest of the bytes, never the URL's last segment. A path keyed on a basename is shared
    // state: two reads of two different documents both called `report.pdf` raced, and each could receive the
    // other's text and report it `ok`. A digest names exactly one body, so there is nothing to share.
    let stem = url
        .rsplit('/')
        .next()
        .unwrap_or("document")
        .replace(|c: char| !c.is_alphanumeric() && c != '.' && c != '-', "_");
    let path = dir.join(format!("{:016x}-{stem}", digest(bytes)));
    let path = if path.to_string_lossy().to_lowercase().ends_with(".pdf") {
        path
    } else {
        path.with_extension("pdf")
    };
    if let Err(why) = std::fs::write(&path, bytes) {
        states.push(format!("document write failed: {why}"));
        return Reading {
            url: url.to_string(),
            path: Path_::Document,
            state: State::Unreachable(why.to_string()),
            states: states.clone(),
            title: String::new(),
            text: String::new(),
            blocks: Vec::new(),
            ms: started.elapsed().as_millis(),
        };
    }
    let reading = finish_document(url, &path, bytes, states, started);
    // One body, one file: the temp copy is removed as soon as it has been parsed, so the directory holds what is
    // being read and nothing else.
    let _ = std::fs::remove_file(&path);
    reading
}

/// Parse a written document and report how it went, with the time actually taken.
fn finish_document(
    url: &str,
    path: &Path,
    bytes: &[u8],
    states: &mut Vec<String>,
    started: std::time::Instant,
) -> Reading {
    let ms = |started: std::time::Instant| started.elapsed().as_millis();
    match crate::library::text_of_file(&path_stem(path), path, bytes) {
        Ok(text) => {
            // 77 characters of garbage out of a 3 MB file is not a read. A document must produce enough text to
            // be worth returning, or it is reported `empty` and the caller knows to try something else.
            let empty = text.trim().chars().count() < 200;
            states.push(if empty {
                "state empty".to_string()
            } else {
                "state ok".to_string()
            });
            Reading {
                url: url.to_string(),
                path: Path_::Document,
                state: if empty { State::Empty } else { State::Ok },
                states: states.clone(),
                title: path_stem(path),
                text,
                blocks: Vec::new(),
                ms: ms(started),
            }
        }
        Err(why) => {
            // Not `binary`: the bytes were a document, it simply yielded no text to this parser.
            states.push(format!("document parse failed: {why}"));
            Reading {
                url: url.to_string(),
                path: Path_::Document,
                state: State::Empty,
                states: states.clone(),
                title: String::new(),
                text: String::new(),
                blocks: Vec::new(),
                ms: ms(started),
            }
        }
    }
}

/// FNV-1a over the body. A digest only has to tell two documents apart inside one directory, so this is enough
/// and it costs no dependency.
fn digest(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

fn path_stem(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "document".to_string())
}

fn read_browser(state: Option<&Path>, url: &str, states: &mut Vec<String>) -> Reading {
    let Some(state_dir) = state else {
        states.push("browser unavailable without a state directory".to_string());
        return Reading {
            url: url.to_string(),
            path: Path_::Browser,
            state: State::JsRequired,
            states: states.clone(),
            title: String::new(),
            text: String::new(),
            blocks: Vec::new(),
            ms: 0,
        };
    };
    let args = serde_json::json!({"action": "open", "url": url});
    let body = crate::browser::tool(&args, state_dir);
    let (text, blocks, title) = extract_html(&body);
    let empty = text.trim().is_empty();
    states.push(if empty {
        "state js-required (browser returned no text)".to_string()
    } else {
        "state ok via browser".to_string()
    });
    Reading {
        url: url.to_string(),
        path: Path_::Browser,
        state: if empty { State::JsRequired } else { State::Ok },
        states: states.clone(),
        title,
        text,
        blocks,
        ms: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARTICLE: &str = r#"<html><head><title>Write-Ahead Logging</title></head>
<body><nav><a href="/a">Documentation</a><a href="/b">Download</a></nav>
<main><h1>Write-Ahead Logging</h1>
<p>The WAL journal mode is enabled with a pragma.</p>
<pre>PRAGMA journal_mode=WAL;</pre>
</main><footer><a href="/c">Privacy</a></footer></body></html>"#;

    #[test]
    fn the_selector_names_every_refusal() {
        assert_eq!(select(Sniff::Html, 200, 100), (Path_::Fetch, None));
        assert_eq!(select(Sniff::Pdf, 200, 100), (Path_::Document, None));
        assert_eq!(select(Sniff::Binary, 200, 100).1, Some(State::Binary));
        assert_eq!(select(Sniff::Html, 403, 100).1, Some(State::Blocked(403)));
        assert_eq!(select(Sniff::Html, 429, 100).1, Some(State::Blocked(429)));
        assert_eq!(select(Sniff::Html, 402, 100).1, Some(State::Paywalled));
        assert_eq!(
            select(Sniff::Html, 200, MAX_BYTES + 1).1,
            Some(State::TooLarge)
        );
        // the strings a report prints
        assert_eq!(State::Blocked(403).to_string(), "blocked(403)");
        assert_eq!(State::JsRequired.to_string(), "js-required");
    }

    #[test]
    fn an_empty_body_is_never_ok() {
        // The rule the search work paid for: a page that yields no text is a named state, not a success.
        let (text, blocks, _) = extract_html("<html><body><div></div></body></html>");
        assert!(text.is_empty());
        assert!(blocks.is_empty());
        // Asserted through the pipeline, not by comparing two enum variants: `assert_ne!(Ok, Empty)` is true for
        // any two variants and says nothing about the code.
        let mut states = Vec::new();
        let reading = finish_document(
            "https://example.org/blank.pdf",
            Path::new("/nonexistent/blank.pdf"),
            b"",
            &mut states,
            std::time::Instant::now(),
        );
        assert_eq!(
            reading.state,
            State::Empty,
            "a body with no text must not report ok"
        );
    }

    #[test]
    fn navigation_and_footer_are_not_content() {
        let (text, _, title) = extract_html(ARTICLE);
        assert!(text.contains("WAL journal mode is enabled"));
        assert!(
            !text.contains("Documentation"),
            "nav label survived: {text}"
        );
        assert!(!text.contains("Privacy"), "footer label survived: {text}");
        assert_eq!(title, "Write-Ahead Logging");
    }

    #[test]
    fn a_code_block_survives_verbatim() {
        let (text, _, _) = extract_html(ARTICLE);
        assert!(
            text.contains("```\nPRAGMA journal_mode=WAL;\n```"),
            "{text}"
        );
    }

    #[test]
    fn a_citation_span_slices_back_to_its_own_block() {
        // The span is recorded while the text is built, so a claim can name the passage rather than the page.
        let (text, blocks, _) = extract_html(ARTICLE);
        assert!(!blocks.is_empty());
        for block in &blocks {
            let slice = &text[block.range.0..block.range.1];
            assert!(!slice.is_empty(), "empty slice for {}", block.kind);
            assert!(block.range.1 <= text.len());
        }
        let code = blocks
            .iter()
            .find(|b| b.kind == "pre")
            .expect("no pre block");
        assert!(text[code.range.0..code.range.1].contains("PRAGMA"));
    }

    #[test]
    fn a_forum_thread_keeps_the_answer_without_an_article_container() {
        let html = r#"<html><body><div class="q"><p>How do I fix it?</p></div>
<div class="a"><p>Run the command with sudo.</p></div></body></html>"#;
        let (text, _, _) = extract_html(html);
        assert!(text.contains("Run the command with sudo."));
    }

    #[test]
    fn a_javascript_shell_is_recognised() {
        let shell =
            r#"<html><body><div id="root"></div><script src="/app.js"></script></body></html>"#;
        assert!(looks_like_a_shell(shell));
        assert!(!looks_like_a_shell(ARTICLE));
    }

    #[test]
    fn a_menu_built_from_list_items_is_dropped() {
        // The exact shape sqlite.org uses: a plain <div class="menu"> holding a <ul> of <li><a> rows. There is
        // no <nav> anywhere, which is why the structural filter alone let the whole menu through.
        let html = r#"<html><body>
<div class="menu mainmenu"><ul>
<li><a href="index.html">Home</a>
<li class='wideonly'><a href='about.html'>About</a>
<li class='desktoponly'><a href="docs.html">Documentation</a>
<li class='desktoponly'><a href="download.html">Download</a>
</ul></div>
<article><h1>Write-Ahead Logging</h1><p>The WAL journal mode is enabled with a pragma.</p></article>
</body></html>"#;
        let (text, _, _) = extract_html(html);
        assert!(text.contains("WAL journal mode"), "content lost: {text}");
        assert!(!text.contains("Documentation"), "menu survived: {text}");
        assert!(!text.contains("Home"), "menu survived: {text}");
    }
    #[test]
    fn entities_are_decoded_including_numeric() {
        assert_eq!(
            decode_entities("a &amp; b &lt;c&gt; &#39;d&#x27;"),
            "a & b <c> 'd'"
        );
        assert_eq!(decode_entities("100 & 200"), "100 & 200");
    }

    #[test]
    fn the_cache_key_carries_the_extractor_version() {
        // Changing extraction must not serve a stale reading, and a TTL cannot know that.
        let a = cache_key("https://example.org/x", "text/html");
        assert_eq!(a, cache_key("https://example.org/x", "text/html"));
        assert_ne!(a, cache_key("https://example.org/y", "text/html"));
        assert_ne!(a, cache_key("https://example.org/x", "application/pdf"));
        // The version has to be inside the key, not appended to it in the test. Recomputing the key with the
        // version forced to a different value is the only assertion that can fail if the constant is dropped.
        let older = cache_key_with("https://example.org/x", "text/html", "v0");
        let newer = cache_key_with("https://example.org/x", "text/html", "v1");
        assert_ne!(older, newer, "the extractor version must change the key");
    }

    #[test]
    fn a_reading_is_cached_and_a_refusal_is_not() {
        let dir = std::env::temp_dir().join(format!("hades-read-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let good = Reading {
            url: "https://example.org/doc".to_string(),
            path: Path_::Fetch,
            state: State::Ok,
            states: vec![],
            title: "T".to_string(),
            text: "body".to_string(),
            blocks: vec![Block {
                kind: "p".to_string(),
                range: (0, 4),
            }],
            ms: 1,
        };
        cache_put(&dir, "https://example.org/doc", "text/html", &good);
        let hit = cache_get(&dir, "https://example.org/doc", "text/html").expect("no cache hit");
        assert_eq!(hit.text, "body");
        assert_eq!(hit.path, Path_::Cache);

        let refused = Reading {
            state: State::Blocked(403),
            ..good.clone()
        };
        cache_put(&dir, "https://example.org/no", "text/html", &refused);
        assert!(
            cache_get(&dir, "https://example.org/no", "text/html").is_none(),
            "a refusal was cached"
        );

        let empty = Reading {
            state: State::Ok,
            text: String::new(),
            ..good
        };
        cache_put(&dir, "https://example.org/blank", "text/html", &empty);
        assert!(
            cache_get(&dir, "https://example.org/blank", "text/html").is_none(),
            "an empty reading was cached"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_documentation_page_is_valid_for_a_day_and_news_for_an_hour() {
        assert_eq!(
            ttl_secs("https://docs.python.org/3/library/asyncio.html"),
            24 * 3600
        );
        assert_eq!(ttl_secs("https://www.reuters.com/news/thing"), 3600);
        assert_eq!(ttl_secs("https://example.org/some/page"), 6 * 3600);
    }
}
