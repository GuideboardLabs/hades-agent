//! The repo map: what a model should know exists, ranked, under a token budget.
//!
//! A model asked to change a repository it has never seen will guess at the shape of it, and a guess that
//! costs one turn is cheap only until it is wrong about which file owns the behaviour. So the map is built
//! from the tree itself: which files mention which other files, ranked, with each file's declarations named
//! and numbered so the model can ask for the right lines instead of the whole file.
//!
//! What the ranking is, honestly: a file that many other files mention is a file that many other files
//! depend on. A mention is found by looking for another file's stem in this file's text, weighted higher on
//! an import line than anywhere else. That is a heuristic and it says so: it is not a resolver, not an
//! import graph, and not an LSP. Its failure mode is a file that matters and is barely mentioned, which a
//! seed from the goal is there to catch.
//!
//! What it is not: an implementation of `.gitignore` (there is a fixed list of directories that are never
//! part of a repository's readable surface, and everything else is walked), and not a reader of data files
//! (a file over [`MAX_BYTES`], of an extension that is not in [`READABLE_EXTENSIONS`], or with a NUL byte
//! early in it, is not mapped at all).
//!
//! What it costs, measured on a 13k-file, 123 MB tree in release (`map_a_real_tree`): about two seconds,
//! nearly all of it reading and scanning the text. Three things that looked like they would be the problem
//! were measured and were not: reading only the head of each file (a fifth), tokenizing bytes rather than
//! characters (five per cent, and it costs non-ASCII identifiers), and the standard hasher on the mention
//! lookups (a fifth at most, since the scan is the larger half). The answer is not a faster scan of the same
//! bytes, it is to not scan them twice.
//!
//! The cache is that, measured on the same tree in release: `hades map` 2.262 s cold and 388.9 ms warm
//! (12,957 hits, 0 misses, a 17 MB file); a real `hades turn` over 12,959 files 3.00 s cold with 0 hits and
//! 12,959 misses, 1.38 s warm with 12,959 hits and 0 misses. The 389 ms that remains warm is the stat walk
//! itself, and it is the floor for this design: the cache saves the read, not the `stat`. Invalidation is
//! size plus modification time, the identity a file system gives away for free; a write that restores both
//! is missed, which on ext4 means a deliberate mtime restore.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read as _;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::context::{Section, estimate};
use crate::fault::Fault;

/// A file over this size is skipped. The map is for reading, and this is not a reader of data files.
pub const MAX_BYTES: u64 = 256 * 1024;

/// How much of a file is read at all. A file longer than this is still ranked and still listed, on what was
/// read, which is the part that declares things.
const SCAN_BYTES: usize = 64 * 1024;

/// How much of a file is looked through for mentions, which is the expensive half of building a map.
///
/// Measured on a real tree (`map_a_real_tree`): counting mentions over the whole of every file was 1.5s of a
/// 2.4s map, and the bytes are not evenly spread, so the tail is expensive and says little. What a file
/// depends on it says near the top: imports, uses, from-imports, #includes. Those lines are also the ones
/// weighted ten times over a mention in passing, so the signal that decides the ranking is in the window
/// that is kept.
const MENTION_BYTES: usize = 16 * 1024;

/// Declarations listed per file, so one enormous file cannot own the map. What was left out is counted on
/// the line, because a list that stops without saying so reads as a file that ends there.
const MAX_DEFS: usize = 40;

/// The share of a file's block that comes from the graph rather than from the seeds.
const DAMPING: f64 = 0.85;

/// Fixed passes, so the same tree ranks the same way every time. This is far past convergence for a tree of
/// this size, and a convergence test would be one more thing that can behave differently twice.
const PASSES: usize = 25;

/// A mention on an import line is a dependency; a mention anywhere else may be a coincidence in a comment.
const IMPORT_WEIGHT: f64 = 10.0;
const MENTION_WEIGHT: f64 = 1.0;

/// What a file named by the goal is worth, relative to the graph.
const SEED_WEIGHT: f64 = 10.0;

/// How the restart mass is split: half to the files the goal names, half spread evenly.
///
/// Both halves are load-bearing. All of it to the seeds and a goal that names one file erases the graph
/// that says which files the repository is built around; none of it and the seeds stop mattering after
/// enough passes, which is the bug this constant was added to fix.
const SEED_SHARE: f64 = 0.5;

/// The kinds of file a map is worth building from: source in the languages this reads, plus the prose and
/// configuration that tell a reader how the thing is put together.
///
/// An extension that is not here is not mapped at all. That is the point: a repository contains raw data,
/// archives and generated files that are none of them a map's business, and on a real tree they are most of
/// the bytes. Reading them costs seconds per turn and buys nothing, because a model does not need to be told
/// that `contributors/emails/0001.eml` exists.
pub const READABLE_EXTENSIONS: &[&str] = &[
    // source
    "rs",
    "py",
    "pyi",
    "ts",
    "tsx",
    "js",
    "jsx",
    "mjs",
    "cjs",
    "vue",
    "svelte",
    "astro",
    "go",
    "java",
    "kt",
    "kts",
    "scala",
    "rb",
    "php",
    "cs",
    "c",
    "h",
    "cc",
    "cpp",
    "cxx",
    "hpp",
    "hh",
    "m",
    "mm",
    "swift",
    "lua",
    "pl",
    "pm",
    "r",
    "jl",
    "dart",
    "ex",
    "exs",
    "erl",
    "hrl",
    "hs",
    "ml",
    "mli",
    "fs",
    "fsx",
    "clj",
    "cljs",
    "sh",
    "bash",
    "zsh",
    "fish",
    "ps1",
    "bat",
    "sql",
    "proto",
    "graphql",
    "gql",
    "tf",
    "tfvars",
    "nix",
    // markup, prose and configuration
    "html",
    "htm",
    "css",
    "scss",
    "sass",
    "less",
    "md",
    "markdown",
    "rst",
    "txt",
    "org",
    "tex",
    "toml",
    "yaml",
    "yml",
    "json",
    "json5",
    "ini",
    "cfg",
    "conf",
    "properties",
    "xml",
];

/// Files with no extension that are worth mapping anyway.
pub const READABLE_NAMES: &[&str] = &[
    "Makefile",
    "Dockerfile",
    "Justfile",
    "Rakefile",
    "Jenkinsfile",
];

/// Directories that are never part of a repository's readable surface, and whose files are never ranked.
pub const SKIPPED_DIRS: &[&str] = &[
    ".git",
    ".cargo",
    "target",
    "node_modules",
    "__pycache__",
    ".venv",
    "venv",
    "dist",
    "build",
    ".mypy_cache",
    ".pytest_cache",
    "site-packages",
];

/// A declaration worth naming: where it is, and the line as written.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Definition {
    line: usize,
    text: String,
}

impl Definition {
    /// A declaration as a cache remembered it.
    pub(crate) fn at(line: usize, text: &str) -> Definition {
        Definition {
            line,
            text: text.to_string(),
        }
    }

    pub fn line(&self) -> usize {
        self.line
    }

    pub fn text(&self) -> &str {
        &self.text
    }
}

/// One file in the map: where it ranks, what it declares, and what did not fit.
#[derive(Debug, Clone, PartialEq)]
pub struct File {
    path: String,
    rank: f64,
    seeded: bool,
    definitions: Vec<Definition>,
    omitted: usize,
    bytes: u64,
}

impl File {
    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn rank(&self) -> f64 {
        self.rank
    }

    /// Whether the goal named this file.
    pub fn seeded(&self) -> bool {
        self.seeded
    }

    pub fn definitions(&self) -> &[Definition] {
        &self.definitions
    }

    /// How many declarations were left off this file's list for room.
    pub fn omitted(&self) -> usize {
        self.omitted
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

/// The tree, ranked. Shared by the cold build and the cached one, so that a warm map is the same map: two
/// paths to a ranking would eventually disagree, and the disagreement would show up as a map that changes
/// when nothing changed.
fn ranked(sources: Vec<Source>, seeds: &[String]) -> Map {
    let edges = edges(&sources);
    let seeded = matched(&sources, seeds);
    let ranks = rank(&edges, &seeded, sources.len());

    let named: Vec<bool> = (0..sources.len())
        .map(|index| in_seeds(index, &seeded))
        .collect();
    let mut files: Vec<File> = sources
        .iter()
        .zip(ranks)
        .enumerate()
        .map(|(index, (source, rank))| File {
            path: source.path.clone(),
            rank,
            seeded: named[index],
            omitted: 0,
            bytes: source.bytes,
            definitions: source.definitions.clone(),
        })
        .collect();

    // A file the goal names comes first, then the graph's order, then the path so that an equal rank is
    // broken the same way every run. The goal's files lead because the map is read from the top and the
    // budget stops it partway: a file the caller asked about is the last thing that should fall off the
    // page. Everything below that is the graph, which is the part that says how the repository is built.
    files.sort_by(|a, b| {
        b.seeded
            .cmp(&a.seeded)
            .then_with(|| {
                b.rank
                    .partial_cmp(&a.rank)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.path.cmp(&b.path))
    });

    let walked = sources.len();
    Map { files, walked }
}

/// The tree, ranked.
#[derive(Debug, Clone, PartialEq)]
pub struct Map {
    files: Vec<File>,
    walked: usize,
}

impl Map {
    /// Build the map for a tree. `seeds` are names the caller already knows matter, usually the words of
    /// the goal: a file named by the goal starts near the top whatever the graph thinks of it.
    pub fn build(root: &Path, seeds: &[String]) -> Result<Map, Fault> {
        Ok(ranked(walked(root)?, seeds))
    }

    /// The same map, from a scan that is remembered between builds.
    ///
    /// A file whose size and modification time are unchanged is not read again; its declarations and the
    /// names it mentions come out of `cache`. The ranking is not cached: the seeds come from the goal, and a
    /// remembered rank would be an answer to a different question.
    pub fn build_cached(root: &Path, seeds: &[String], cache: &mut Cache) -> Result<Map, Fault> {
        Ok(ranked(walked_cached(root, cache)?, seeds))
    }

    /// Every file that was walked, in rank order. Built for a caller that wants the ranks, not the text.
    pub fn files(&self) -> &[File] {
        &self.files
    }

    /// How many files were walked, including the ones that did not fit into the render.
    pub fn walked(&self) -> usize {
        self.walked
    }

    /// The map as text: a contiguous run from the top of the ranking, up to `tokens`.
    ///
    /// A file is included with as many of its declarations as fit, never fewer than its header and never
    /// more than [`MAX_DEFS`] of them, and what was left off is counted on the line. The map stops at the
    /// first file that does not fit rather than skipping it, so it is always a prefix of the ranking: a map
    /// that leaves a file out and then lists things below it reads as the whole list, which is the lie this
    /// file exists to avoid. The footer says how much of the tree is on the page.
    pub fn render(&self, tokens: usize) -> String {
        if self.files.is_empty() || tokens == 0 {
            return String::new();
        }
        let mut out = String::new();
        let mut used = 0usize;
        let mut shown = 0usize;

        for file in &self.files {
            let room = tokens.saturating_sub(used);
            if room == 0 {
                break;
            }
            // The cap is the room that is left, and how many declarations a file shows is capped separately
            // ([`MAX_DEFS`]): a map lists names so a model can ask for the right lines, and a file that owns
            // the whole map is not a map.
            let (block, omitted) = block(file, room);
            let cost = estimate(&block);
            if cost > room {
                break;
            }
            used += cost;
            shown += 1;
            out.push_str(&block);
            if omitted > 0 {
                out.push_str(&format!("  ... and {omitted} more in this file\n"));
            }
        }

        if out.is_empty() {
            return String::new();
        }
        out.push_str(&format!(
            "map: {shown} of {} files, ranked by what mentions them; {} did not fit\n",
            self.walked,
            self.files.len() - shown
        ));
        out
    }
}

/// The map as a context section, if there is room for one.
///
/// Returns `None` rather than an empty section when the tree is empty or the room is too small for a map
/// worth reading: a section that says nothing still costs its heading and still displaces something real.
pub fn section(root: &Path, goal: &str, tokens: usize) -> Result<Option<Section>, Fault> {
    let mut cold = Cache::new();
    section_cached(root, goal, tokens, &mut cold)
}

/// The map section, from a cache the caller keeps across turns. This is the entry point a turn uses: the one
/// above exists so that a caller with nowhere to keep a cache does not have to invent one.
pub fn section_cached(
    root: &Path,
    goal: &str,
    tokens: usize,
    cache: &mut Cache,
) -> Result<Option<Section>, Fault> {
    let map = Map::build_cached(root, &seed_words(goal), cache)?;
    let text = map.render(tokens);
    if text.is_empty() {
        return Ok(None);
    }
    Ok(Some(Section::new("repo map", text)))
}

/// The words of a goal that could name a file. Short words and punctuation are not names.
pub fn seed_words(goal: &str) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    for word in goal.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-' || c == '.')) {
        let word = word.trim_matches('.').to_lowercase();
        if word.len() >= 3 && !words.contains(&word) {
            words.push(word);
        }
    }
    words
}

/// One file as the walk found it: where it is, and the identity that says whether it changed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Seen {
    path: String,
    stem: String,
    bytes: u64,
    seconds: u64,
    nanos: u32,
}

/// What reading a file produced. Beside [`Seen`] this is everything a map needs from one file, and nothing
/// in here can be learned from a stat: that is the point of keeping it separate.
#[derive(Debug, Clone, Default, PartialEq)]
struct Recall {
    definitions: Vec<Definition>,
    /// The stems this file names, and the weight it names them at. Stems rather than indexes, because a cache
    /// outlives one list of files, and an index is a position in a list that the next scan may reorder.
    mentions: Vec<(String, f64)>,
}

/// One file's place in a map: what it is, and what reading it said.
struct Source {
    path: String,
    stem: String,
    bytes: u64,
    definitions: Vec<Definition>,
    mentions: Vec<(String, f64)>,
}

/// The scan, remembered: per file, its identity on disk beside what reading it produced.
///
/// The ranking is deliberately never cached. A goal changes the seeds, the graph is the answer to that goal,
/// and a remembered rank would be an answer to a question nobody asked this time.
///
/// The cache knows nothing about a root: paths are relative to the tree that was walked, so a cache saved
/// beside one state directory and loaded against a different `--root` would be a cache of the wrong tree. The
/// caller owns that pairing; [`Cache::save`] writes what [`Cache::load`] reads and no more. Keep it outside
/// the tree it describes: a cache is a file, and a file inside the tree is a file the next scan reads.
#[derive(Debug, Clone, Default)]
pub struct Cache {
    files: BTreeMap<String, (Seen, Recall)>,
    hits: u64,
    misses: u64,
}

impl Cache {
    pub fn new() -> Cache {
        Cache::default()
    }

    /// Files whose content was not read again.
    pub fn hits(&self) -> u64 {
        self.hits
    }

    /// Files that were read.
    pub fn misses(&self) -> u64 {
        self.misses
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Read a cache from disk.
    ///
    /// This fails rather than returning an empty cache, because the two are different facts and the caller is
    /// the one that can decide: a daemon starting on a corrupted cache should say so and run cold, and a test
    /// asserting a warm second build should not be handed a silent miss.
    pub fn load(path: &Path) -> Result<Cache, Fault> {
        let text = std::fs::read_to_string(path).map_err(|error| {
            Fault::harness(format!("{} could not be read: {error}", path.display()))
        })?;
        let whole: Value = serde_json::from_str(&text).map_err(|error| {
            Fault::harness(format!("{} is not a cache: {error}", path.display()))
        })?;
        let listing = whole
            .get("files")
            .and_then(Value::as_object)
            .ok_or_else(|| Fault::harness(format!("{} has no files table", path.display())))?;

        let mut cache = Cache::new();
        for (path_key, entry) in listing {
            let bytes = entry.get("bytes").and_then(Value::as_u64).unwrap_or(0);
            let seconds = entry.get("seconds").and_then(Value::as_u64).unwrap_or(0);
            let nanos = entry.get("nanos").and_then(Value::as_u64).unwrap_or(0) as u32;
            let mut recall = Recall::default();
            if let Some(list) = entry.get("definitions").and_then(Value::as_array) {
                for pair in list {
                    let line = pair.get(0).and_then(Value::as_u64).unwrap_or(0) as usize;
                    let text = pair.get(1).and_then(Value::as_str).unwrap_or_default();
                    recall.definitions.push(Definition::at(line, text));
                }
            }
            if let Some(list) = entry.get("mentions").and_then(Value::as_array) {
                for pair in list {
                    let stem = pair.get(0).and_then(Value::as_str).unwrap_or_default();
                    let weight = pair.get(1).and_then(Value::as_f64).unwrap_or(0.0);
                    if !stem.is_empty() {
                        recall.mentions.push((stem.to_string(), weight));
                    }
                }
            }
            let seen = Seen {
                path: path_key.clone(),
                stem: stem_of(path_key),
                bytes,
                seconds,
                nanos,
            };
            cache.files.insert(path_key.clone(), (seen, recall));
        }
        Ok(cache)
    }

    /// Write the cache, by temp file and rename, so a crash in the middle leaves the previous cache whole
    /// rather than a half-written file that reads as a corrupted one.
    pub fn save(&self, path: &Path) -> Result<(), Fault> {
        let mut files = serde_json::Map::new();
        for (key, (seen, recall)) in &self.files {
            let definitions: Vec<Value> = recall
                .definitions
                .iter()
                .map(|definition| json!([definition.line(), definition.text()]))
                .collect();
            let mentions: Vec<Value> = recall
                .mentions
                .iter()
                .map(|(stem, weight)| json!([stem, weight]))
                .collect();
            files.insert(
                key.clone(),
                json!({
                    "bytes": seen.bytes,
                    "seconds": seen.seconds,
                    "nanos": seen.nanos,
                    "definitions": definitions,
                    "mentions": mentions,
                }),
            );
        }
        let whole = json!({ "v": 1, "files": Value::Object(files) });
        let text = serde_json::to_string(&whole)
            .map_err(|error| Fault::harness(format!("the cache did not serialize: {error}")))?;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                Fault::harness(format!("{} could not be made: {error}", parent.display()))
            })?;
        }
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, text.as_bytes()).map_err(|error| {
            Fault::harness(format!(
                "{} could not be written: {error}",
                temporary.display()
            ))
        })?;
        std::fs::rename(&temporary, path).map_err(|error| {
            Fault::harness(format!(
                "{} could not be put in place: {error}",
                path.display()
            ))
        })
    }

    /// Whether a file as the current scan found it is the file this cache remembers.
    fn remembers(&self, seen: &Seen) -> Option<&Recall> {
        self.files
            .get(&seen.path)
            .filter(|(was, _)| {
                was.bytes == seen.bytes && was.seconds == seen.seconds && was.nanos == seen.nanos
            })
            .map(|(_, recall)| recall)
    }
}

/// The stem a path is known by, which is what other files name it with.
fn stem_of(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

/// Walk the tree, in a sorted order, skipping what a repository does not read.
///
/// Nothing is opened here. What a file says is read in [`read`], and only for the files a cache does not
/// already remember: the whole point of separating the stat pass from the read pass is that a warm map pays
/// for neither.
///
/// A symlink is never followed, so a link back up the tree cannot turn this into a loop. An entry that
/// cannot be read is skipped rather than fatal: a permissions problem in one corner of a tree is not a
/// reason to refuse a turn.
fn scan(root: &Path) -> Result<Vec<Seen>, Fault> {
    let mut files = Vec::new();
    let mut dirs = vec![root.to_path_buf()];

    while let Some(dir) = dirs.pop() {
        // A directory that cannot be read is skipped, not fatal. The map ranks what it can read, and one
        // unreadable subdirectory is ordinary: a root-owned service directory inside /tmp, a private build
        // cache, a mount that went away. Failing the whole map there would fail the whole turn, because the
        // map is built while the context is built, and a turn that cannot build context cannot run at all.
        let Ok(listing) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut entries: Vec<PathBuf> = listing
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .collect();
        entries.sort();

        for path in entries {
            let name = match path.file_name().and_then(|name| name.to_str()) {
                Some(name) => name.to_string(),
                None => continue,
            };
            let kind = match std::fs::symlink_metadata(&path) {
                Ok(meta) => meta,
                Err(_) => continue,
            };
            if kind.file_type().is_symlink() {
                continue;
            }
            if kind.is_dir() {
                if name.starts_with('.') || SKIPPED_DIRS.contains(&name.as_str()) {
                    continue;
                }
                dirs.push(path);
                continue;
            }
            if !kind.is_file() || name.starts_with('.') || kind.len() > MAX_BYTES {
                continue;
            }
            if !is_readable(&path) {
                continue;
            }
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let (seconds, nanos) = kind
                .modified()
                .ok()
                .and_then(|when| when.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|age| (age.as_secs(), age.subsec_nanos()))
                .unwrap_or((0, 0));
            files.push(Seen {
                path: relative,
                stem: stem_of(&path.to_string_lossy()),
                bytes: kind.len(),
                seconds,
                nanos,
            });
        }
    }

    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

/// Read one file and keep what a map needs from it.
///
/// Only the part that is read is read: a map scans for names and declarations near the top of a file, and
/// pulling a whole large file into memory to throw most of it away is how a walk turns into a stall. `None`
/// when the file cannot be read now, which is the same answer the walk gives a file it cannot open.
fn read(root: &Path, seen: &Seen, stems: &HashSet<String>) -> Option<Recall> {
    let path = root.join(&seen.path);
    let mut file = std::fs::File::open(&path).ok()?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(SCAN_BYTES as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    if is_binary(&bytes) {
        return None;
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Some(Recall {
        definitions: definitions(&text, &seen.path),
        mentions: named(&text, stems),
    })
}

/// The tree, read without help from a cache.
fn walked(root: &Path) -> Result<Vec<Source>, Fault> {
    if !root.is_dir() {
        return Err(Fault::harness(format!(
            "{} is not a directory to map",
            root.display()
        )));
    }
    let seen = scan(root)?;
    let stems = stems_of(&seen);
    Ok(seen
        .iter()
        .filter_map(|file| {
            read(root, file, &stems).map(|recall| Source {
                path: file.path.clone(),
                stem: file.stem.clone(),
                bytes: file.bytes,
                definitions: recall.definitions,
                mentions: recall.mentions,
            })
        })
        .collect())
}

/// The tree, read only where the cache does not already know the answer.
///
/// `cache.files` is replaced wholesale at the end, so a file that is gone is forgotten and a file that is new
/// is read. A file whose size and modification time are unchanged is not opened at all.
fn walked_cached(root: &Path, cache: &mut Cache) -> Result<Vec<Source>, Fault> {
    if !root.is_dir() {
        return Err(Fault::harness(format!(
            "{} is not a directory to map",
            root.display()
        )));
    }
    let seen = scan(root)?;
    let stems = stems_of(&seen);
    let mut kept: BTreeMap<String, (Seen, Recall)> = BTreeMap::new();
    let mut sources = Vec::with_capacity(seen.len());

    for file in seen {
        // Cloned out of the cache before the counters move: a borrow of `cache.files` and a write to
        // `cache.hits` are the same cache, and the compiler is right to refuse them at once.
        let remembered = cache.remembers(&file).cloned();
        let recall = match remembered {
            Some(recall) => {
                cache.hits += 1;
                recall
            }
            None => {
                cache.misses += 1;
                let Some(recall) = read(root, &file, &stems) else {
                    continue;
                };
                recall
            }
        };
        kept.insert(file.path.clone(), (file.clone(), recall.clone()));
        sources.push(Source {
            path: file.path,
            stem: file.stem,
            bytes: file.bytes,
            definitions: recall.definitions,
            mentions: recall.mentions,
        });
    }

    cache.files = kept;
    Ok(sources)
}

/// Every stem a file in this walk is known by. Only stems long enough to be a name: a three-letter token is
/// the shortest thing the walk will accept as a mention.
fn stems_of(files: &[Seen]) -> HashSet<String> {
    files
        .iter()
        .filter(|file| file.stem.len() >= 3)
        .map(|file| file.stem.clone())
        .collect()
}

/// Whether this is the kind of file a map reads, by extension or by the few names that carry their own kind.
fn is_readable(path: &Path) -> bool {
    if path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| READABLE_NAMES.contains(&name))
    {
        return true;
    }
    path.extension()
        .map(|extension| extension.to_string_lossy().to_lowercase())
        .is_some_and(|extension| READABLE_EXTENSIONS.contains(&extension.as_str()))
}

/// The first thing a reader of a source file needs to know about a file it cannot read.
fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|byte| *byte == 0)
}

/// The stems a text names, and the weight it names them at. A mention twenty kilobytes into a file is not
/// evidence of a dependency, so the scan stops at [`MENTION_BYTES`].
///
/// This is the half of the work that a cache remembers: it needs the text, and the text is the expensive part.
fn named(text: &str, stems: &HashSet<String>) -> Vec<(String, f64)> {
    let mut weights: HashMap<String, f64> = HashMap::new();
    let mut seen = 0usize;
    for line in text.lines() {
        if seen >= MENTION_BYTES {
            break;
        }
        seen += line.len() + 1;
        let importing = is_import_line(line);
        for token in line.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-')) {
            if token.len() < 3 {
                continue;
            }
            // Lowercasing every token allocates once per token, which on a real tree is millions of
            // allocations to answer a question a scan of the token's own bytes answers first: source is
            // written in lowercase, so only the tokens that are not get copied.
            let lowered;
            let token: &str = if token.bytes().any(|byte| byte.is_ascii_uppercase()) {
                lowered = token.to_lowercase();
                &lowered
            } else {
                token
            };
            if !stems.contains(token) {
                continue;
            }
            let weight = if importing {
                IMPORT_WEIGHT
            } else {
                MENTION_WEIGHT
            };
            *weights.entry(token.to_string()).or_insert(0.0) += weight;
        }
    }
    let mut pairs: Vec<(String, f64)> = weights.into_iter().collect();
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    pairs
}

/// Which files mention which. An edge is a mention of another file's stem, resolved against the files that
/// are in this walk. A file naming itself is not evidence that anything depends on it: left in, a self-edge
/// lets a file that merely contains its own name collect its own rank and climb the map on nothing.
fn edges(sources: &[Source]) -> Vec<Vec<(usize, f64)>> {
    let mut by_stem: HashMap<&str, usize> = HashMap::new();
    for (index, source) in sources.iter().enumerate() {
        if source.stem.len() >= 3 {
            by_stem.entry(source.stem.as_str()).or_insert(index);
        }
    }

    sources
        .iter()
        .enumerate()
        .map(|(index, source)| {
            let mut weights: HashMap<usize, f64> = HashMap::new();
            for (stem, weight) in &source.mentions {
                let Some(&target) = by_stem.get(stem.as_str()) else {
                    continue;
                };
                if target == index {
                    continue;
                }
                *weights.entry(target).or_insert(0.0) += weight;
            }
            // Sorted so an equal-weight tie resolves the same way every build: a map whose order depends on
            // a hash seed is a map that changes while nothing changed.
            let mut pairs: Vec<(usize, f64)> = weights.into_iter().collect();
            pairs.sort_by_key(|pair| pair.0);
            pairs
        })
        .collect()
}

/// A line that names another file as a dependency rather than in passing.
fn is_import_line(line: &str) -> bool {
    let line = line.trim_start();
    for prefix in [
        "use ", "mod ", "pub mod", "import ", "from ", "require(", "require ", "include",
        "#include", "using ",
    ] {
        if line.starts_with(prefix) {
            return true;
        }
    }
    false
}

/// The files the caller's seeds name, with the weight a seed is worth.
fn matched(sources: &[Source], seeds: &[String]) -> Vec<(usize, f64)> {
    let mut found = Vec::new();
    for (index, source) in sources.iter().enumerate() {
        let path = source.path.to_lowercase();
        let mut weight = 0.0;
        for seed in seeds {
            let names_it = seed == &source.stem
                || path.contains(seed.as_str())
                || (seed.len() >= 5 && source.stem.contains(seed.as_str()));
            if names_it {
                weight += SEED_WEIGHT;
            }
        }
        if weight > 0.0 {
            found.push((index, weight));
        }
    }
    found
}

/// Whether a file was named by one of the seeds.
fn in_seeds(index: usize, seeds: &[(usize, f64)]) -> bool {
    seeds.iter().any(|(seeded, _)| *seeded == index)
}

/// Weighted pagerank over the mention graph, with the goal's files as the restart distribution.
///
/// The seeds are not a bonus added at the end, and they are not only where the walk starts: they are where
/// the walk *restarts* on every pass (`(1 - d) * p`), which is what keeps a file the goal names near the top
/// of a graph that has never heard of it. Using them only to set the first vector looks right and is not:
/// after enough passes the walk has forgotten where it began, which is how one version of this ranked a
/// central file above the file the goal named.
fn rank(edges: &[Vec<(usize, f64)>], seeds: &[(usize, f64)], count: usize) -> Vec<f64> {
    if count == 0 {
        return Vec::new();
    }
    // The restart distribution: the goal's files share half of it, evenly spread is the other half.
    let mut restart = vec![(1.0 - SEED_SHARE) / count as f64; count];
    let seed_total: f64 = seeds.iter().map(|(_, weight)| weight).sum();
    if seed_total > 0.0 {
        for (index, weight) in seeds {
            restart[*index] += SEED_SHARE * (weight / seed_total);
        }
    } else {
        for value in restart.iter_mut() {
            *value += SEED_SHARE / count as f64;
        }
    }

    let mut ranks = restart.clone();
    let out_weight: Vec<f64> = edges
        .iter()
        .map(|out| out.iter().map(|(_, weight)| weight).sum())
        .collect();

    for _ in 0..PASSES {
        let mut next: Vec<f64> = restart
            .iter()
            .map(|share| (1.0 - DAMPING) * share)
            .collect();
        for (from, out) in edges.iter().enumerate() {
            if out.is_empty() || out_weight[from] == 0.0 {
                // A file that mentions nothing passes its rank on evenly, the same way a page with no links
                // does not hold what arrives at it.
                let share = DAMPING * ranks[from] / count as f64;
                for value in next.iter_mut() {
                    *value += share;
                }
                continue;
            }
            for (to, weight) in out {
                next[*to] += DAMPING * ranks[from] * (weight / out_weight[from]);
            }
        }
        ranks = next;
    }
    ranks
}

/// The declarations in a file, by extension. An unknown extension contributes no declarations, and the file
/// still appears in the map: that it exists and is mentioned is worth knowing on its own.
fn definitions(text: &str, path: &str) -> Vec<Definition> {
    let extension = Path::new(path)
        .extension()
        .map(|extension| extension.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let mut found = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.len() < 3
            || trimmed.starts_with("//")
            || trimmed.starts_with('#') && extension != "md"
        {
            continue;
        }
        if declares(trimmed, &extension) {
            found.push(Definition {
                line: index + 1,
                text: short(trimmed),
            });
        }
    }
    found
}

/// Whether a trimmed line declares something, in the dialects this build reads.
fn declares(line: &str, extension: &str) -> bool {
    let rest = unqualified(line);
    match extension {
        "rs" => {
            for keyword in [
                "fn ",
                "struct ",
                "enum ",
                "trait ",
                "impl ",
                "impl<",
                "mod ",
                "type ",
                "const ",
                "static ",
                "macro_rules!",
            ] {
                if rest.starts_with(keyword) {
                    return true;
                }
            }
            false
        }
        "py" => rest.starts_with("def ") || rest.starts_with("class "),
        "md" => line.starts_with('#'),
        _ => false,
    }
}

/// Strip what comes before the word that says what kind of thing this is: visibility, and the modifiers
/// that can precede a declaration without changing what it declares.
fn unqualified(line: &str) -> &str {
    let mut rest = line;
    loop {
        let before = rest;
        for prefix in [
            "async ", "unsafe ", "extern ", "default ", "export ", "declare ",
        ] {
            if let Some(tail) = rest.strip_prefix(prefix) {
                rest = tail.trim_start();
            }
        }
        if let Some(tail) = rest.strip_prefix("pub ") {
            rest = tail.trim_start();
        } else if rest.starts_with("pub(")
            && let Some(close) = rest.find(')')
        {
            rest = rest[close + 1..].trim_start();
        }
        if rest == before {
            return rest;
        }
    }
}

/// A declaration line as it goes into the map: one line, and not an essay.
fn short(text: &str) -> String {
    let cut: String = text.chars().take(120).collect();
    if cut.chars().count() < text.chars().count() {
        format!("{cut} ...")
    } else {
        cut
    }
}

/// One file's block, and how many of its declarations were left off for room.
fn block(file: &File, per_file: usize) -> (String, usize) {
    let mut out = format!("{}:", file.path);
    let mut kept = 0usize;
    let mut used = estimate(&out);
    for definition in &file.definitions {
        if kept >= MAX_DEFS {
            break;
        }
        let line = format!("\n  {:>5}  {}", definition.line, definition.text);
        let cost = estimate(&line);
        if used + cost > per_file {
            break;
        }
        used += cost;
        kept += 1;
        out.push_str(&line);
    }
    out.push('\n');
    (out, file.definitions.len() - kept)
}

#[cfg(test)]
mod tests {

    /// A scratch base outside the source tree: the build directory when cargo names one, the temp
    /// directory otherwise. Never `<manifest>/../target`, which belongs to the build and, on a machine
    /// with a nearly full root filesystem, is the wrong disk to fill.
    fn scratch_base() -> std::path::PathBuf {
        crate::scratch::root()
    }

    use super::*;

    /// A tree on disk, written from a list of (path, text). The map reads the filesystem, so its tests read
    /// one too: a fixture object would be a second implementation of "a tree".
    fn tree(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let root = scratch_base().join(format!("hades-map-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (path, text) in files {
            let full = root.join(path);
            std::fs::create_dir_all(full.parent().expect("a parent")).expect("a directory");
            std::fs::write(&full, text).expect("a file");
        }
        std::fs::create_dir_all(&root).expect("the root");
        root
    }

    fn paths(map: &Map) -> Vec<String> {
        map.files().iter().map(|file| file.path.clone()).collect()
    }

    /// The claim P3 rests on: a second build reads nothing, and produces the same map.
    #[test]
    fn a_cache_makes_the_second_build_read_nothing() {
        let root = tree(
            "cache-hit",
            &[
                ("a.rs", "mod b;\npub fn a() {}\n"),
                ("b.rs", "pub fn b() {}\n"),
                ("notes.md", "a and b\n"),
            ],
        );
        let seeds = seed_words("a");
        let mut cache = Cache::new();

        let cold = Map::build_cached(&root, &seeds, &mut cache).expect("a cold map");
        let misses = cache.misses();
        assert_eq!(cache.hits(), 0, "a fresh cache hits nothing");
        assert_eq!(misses, 3, "three files were read");
        assert_eq!(cache.len(), 3);

        let warm = Map::build_cached(&root, &seeds, &mut cache).expect("a warm map");
        assert_eq!(cache.misses(), misses, "nothing was read again");
        assert_eq!(cache.hits(), 3, "every file came out of the cache");
        assert_eq!(cold, warm, "a warm map is the same map");
    }

    /// What invalidates: content that moved the mtime, and a file that is gone.
    #[test]
    fn a_changed_file_is_read_again_and_a_deleted_one_is_forgotten() {
        let root = tree(
            "cache-invalidate",
            &[
                ("a.rs", "pub fn a() {}\n"),
                ("b.rs", "pub fn b() {}\n"),
                ("c.rs", "pub fn c() {}\n"),
            ],
        );
        let seeds = seed_words("a");
        let mut cache = Cache::new();
        Map::build_cached(&root, &seeds, &mut cache).expect("a cold map");
        let misses = cache.misses();

        // A rewrite through a real write moves the mtime, which is the identity a cache watches.
        std::fs::write(root.join("a.rs"), "pub fn a() {}\npub fn a_again() {}\n").expect("a write");
        std::fs::remove_file(root.join("b.rs")).expect("a delete");

        let changed = Map::build_cached(&root, &seeds, &mut cache).expect("a second map");
        assert_eq!(cache.misses(), misses + 1, "the rewritten file was read");
        assert_eq!(cache.hits(), 1, "the untouched file was not");
        assert_eq!(cache.len(), 2, "the deleted file is forgotten");
        assert!(
            paths(&changed).iter().all(|path| path != "b.rs"),
            "a deleted file is not in the map: {changed:?}"
        );
        assert!(!cache.is_empty());
    }

    /// A cache on disk makes a fresh process warm, which is the whole reason the daemon holds it.
    #[test]
    fn a_saved_cache_makes_a_new_one_warm() {
        let root = tree(
            "cache-round-trip",
            &[
                ("a.rs", "mod b;\npub fn a() {}\n"),
                ("b.rs", "pub fn b() {}\n"),
            ],
        );
        let seeds = seed_words("a");

        let mut saved = Cache::new();
        let cold = Map::build_cached(&root, &seeds, &mut saved).expect("a cold map");
        // Beside the tree, never inside it: a cache is a file, and a file in the tree is a file the next
        // scan reads. This test found that by putting one there and watching the warm build miss it.
        let path = root.with_extension("repomap.json");
        saved.save(&path).expect("the cache is written");

        let mut loaded = Cache::load(&path).expect("the cache is read");
        assert_eq!(loaded.len(), 2);
        let _ = std::fs::remove_file(&path);
        assert_eq!(loaded.hits(), 0, "a loaded cache starts having hit nothing");
        let warm = Map::build_cached(&root, &seeds, &mut loaded).expect("a warm map");
        assert_eq!(loaded.misses(), 0, "a loaded cache reads nothing");
        assert_eq!(cold, warm, "the map survived the round trip");
    }

    /// A cache that will not read is an error, not a silent cold start: the caller decides, and a test
    /// asserting warmth must not be handed a quiet miss.
    #[test]
    fn a_cache_that_will_not_read_is_an_error() {
        let root = tree("cache-broken", &[("a.rs", "pub fn a() {}\n")]);
        let path = root.join("repomap.json");
        std::fs::write(&path, "{\"files\": 3}").expect("a broken cache");
        assert!(Cache::load(&path).is_err(), "{:?}", Cache::load(&path));
        assert!(Cache::load(&root.join("nothing.json")).is_err());
    }

    /// A warm map is only worth anything if a turn can reach it, so this pins the section path too.
    #[test]
    fn a_section_from_a_warm_cache_says_the_same_thing() {
        let root = tree(
            "cache-section",
            &[
                ("alpha.rs", "pub fn alpha() {}\n"),
                ("beta.rs", "pub fn beta() {}\n"),
            ],
        );
        let mut cache = Cache::new();
        let cold = section_cached(&root, "alpha", 2000, &mut cache)
            .expect("a section")
            .expect("one with a map");
        let warm = section_cached(&root, "alpha", 2000, &mut cache)
            .expect("a section")
            .expect("one with a map");
        assert_eq!(cold.text(), warm.text());
        assert_eq!(cache.misses(), 2);
        assert_eq!(cache.hits(), 2);
    }

    /// Where the time goes on a real tree, cold and then warm. Ignored by default because it reads a large
    /// repository; run it with `cargo test -p harness --release -- --ignored --nocapture map_a_real_tree`
    /// and set `HADES_MAP_ROOT` to the tree to map.
    #[test]
    #[ignore]
    fn map_a_real_tree() {
        let root =
            PathBuf::from(std::env::var("HADES_MAP_ROOT").unwrap_or_else(|_| ".".to_string()));
        let seeds = seed_words("repomap cache daemon");

        let mut cache = Cache::new();
        let start = std::time::Instant::now();
        let cold = Map::build_cached(&root, &seeds, &mut cache).expect("a cold map");
        let cold_time = start.elapsed();
        let cold_hits = cache.hits();
        let cold_misses = cache.misses();

        let start = std::time::Instant::now();
        let warm = Map::build_cached(&root, &seeds, &mut cache).expect("a warm map");
        let warm_time = start.elapsed();

        println!(
            "cold {:?}  {} files  {} hits {} misses  |  warm {:?}  {} hits {} misses  |  same map {}",
            cold_time,
            cold.walked(),
            cold_hits,
            cold_misses,
            warm_time,
            cache.hits() - cold_hits,
            cache.misses() - cold_misses,
            cold == warm,
        );
        assert_eq!(cold, warm, "a warm map is the same map");
    }

    #[test]
    fn a_file_many_others_mention_outranks_a_file_none_do() {
        let root = tree(
            "rank",
            &[
                ("core.rs", "pub fn core() {}\n"),
                ("a.rs", "use crate::core;\npub fn a() { core() }\n"),
                ("b.rs", "use crate::core;\npub fn b() { core() }\n"),
                ("c.rs", "use crate::core;\npub fn c() { core() }\n"),
                ("orphan.rs", "pub fn nobody_mentions_this() {}\n"),
            ],
        );
        let map = Map::build(&root, &[]).expect("a map");
        let order = paths(&map);
        assert_eq!(order[0], "core.rs", "{order:?}");
        assert_eq!(order.last().expect("a last"), "orphan.rs", "{order:?}");
    }

    #[test]
    fn a_file_the_goal_names_starts_at_the_top() {
        // Nothing mentions the notes file, and the graph would put it last. The goal names it, so the goal
        // wins: the ranking is a starting point for the model, not a verdict about what matters.
        let root = tree(
            "seeds",
            &[
                ("core.rs", "pub fn core() {}\n"),
                ("a.rs", "use crate::core;\n"),
                ("notes-about-the-parser.md", "# the parser\n"),
            ],
        );
        let flat = Map::build(&root, &[]).expect("a map");
        assert_eq!(
            paths(&flat).last().expect("a last"),
            "notes-about-the-parser.md"
        );

        let seeded = Map::build(&root, &["parser".to_string()]).expect("a map");
        assert_eq!(paths(&seeded)[0], "notes-about-the-parser.md");
    }

    #[test]
    fn an_import_counts_for_more_than_a_mention_in_prose() {
        // The same tree twice, differing only in how the referencing file mentions its dependency: once as an
        // import, once in a comment. The file being mentioned is the same file, so the difference in rank is
        // the weight and nothing else.
        let importing = tree(
            "weight-import",
            &[
                ("imported.rs", "pub fn imported() {}\n"),
                ("mentioned.rs", "pub fn mentioned() {}\n"),
                ("referrer.rs", "use crate::imported;\n// mentioned\n"),
            ],
        );
        let map = Map::build(&importing, &[]).expect("a map");
        let rank_of = |path: &str| {
            map.files()
                .iter()
                .find(|file| file.path == path)
                .expect("a file")
                .rank
        };
        assert!(
            rank_of("imported.rs") > rank_of("mentioned.rs"),
            "both are named by the same file, so the difference is the weight: {} vs {}",
            rank_of("imported.rs"),
            rank_of("mentioned.rs")
        );
    }

    #[test]
    fn declarations_are_named_and_numbered() {
        let root = tree(
            "defs",
            &[(
                "lib.rs",
                "// a comment\n\npub struct Map {\n    files: usize,\n}\n\nasync fn build() {}\n\nimpl Map {\n    pub fn new() -> Self { todo!() }\n}\n",
            )],
        );
        let map = Map::build(&root, &[]).expect("a map");
        let file = &map.files()[0];
        let named: Vec<(usize, String)> = file
            .definitions()
            .iter()
            .map(|definition| (definition.line(), definition.text().to_string()))
            .collect();
        assert_eq!(
            named,
            vec![
                (3, "pub struct Map {".to_string()),
                (7, "async fn build() {}".to_string()),
                (9, "impl Map {".to_string()),
                (10, "pub fn new() -> Self { todo!() }".to_string()),
            ],
            "a comment is not a declaration and a nested one still is"
        );
    }

    #[test]
    fn python_and_markdown_declare_their_own_way() {
        let root = tree(
            "dialects",
            &[
                (
                    "tool.py",
                    "import os\n\ndef run():\n    pass\n\nclass Thing:\n    def nested(self):\n        pass\n",
                ),
                ("notes.md", "# a heading\n\nnot a heading\n## another\n"),
            ],
        );
        let map = Map::build(&root, &[]).expect("a map");
        let by_path = |path: &str| {
            map.files()
                .iter()
                .find(|file| file.path == path)
                .expect("a file")
                .definitions()
                .iter()
                .map(|definition| definition.text().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            by_path("tool.py"),
            vec![
                "def run():".to_string(),
                "class Thing:".to_string(),
                "def nested(self):".to_string()
            ]
        );
        assert_eq!(
            by_path("notes.md"),
            vec!["# a heading".to_string(), "## another".to_string()]
        );
    }

    #[test]
    fn what_a_repository_does_not_read_is_not_mapped() {
        let root = tree(
            "skips",
            &[
                ("src/a.rs", "pub fn a() {}\n"),
                ("target/debug/build.rs", "pub fn build() {}\n"),
                ("node_modules/pkg/index.js", "function thing() {}\n"),
                (".git/config", "[core]\n"),
                (".hidden.rs", "pub fn hidden() {}\n"),
            ],
        );
        std::fs::write(root.join("big.rs"), "x".repeat(300 * 1024)).expect("a big file");
        std::fs::write(root.join("blob.bin"), [0u8, 1, 2, 3, 4]).expect("a binary file");

        let map = Map::build(&root, &[]).expect("a map");
        assert_eq!(paths(&map), vec!["src/a.rs".to_string()]);
        assert_eq!(map.walked(), 1, "the walk counts what it kept");
    }

    #[test]
    fn a_render_keeps_whole_files_and_says_what_it_left_out() {
        let root = tree(
            "budget",
            &[
                ("first.rs", "pub fn first() {}\npub fn second() {}\n"),
                ("second.rs", "pub fn third() {}\n"),
                ("third.rs", "pub fn fourth() {}\n"),
            ],
        );
        let map = Map::build(&root, &["first".to_string()]).expect("a map");
        let full = map.render(100_000);
        assert!(full.contains("first.rs:"), "{full}");
        assert!(full.contains("second.rs:"), "{full}");
        assert!(full.contains("third.rs:"), "{full}");
        assert!(full.contains("3 of 3 files"), "{full}");

        let tight = map.render(12);
        assert!(tight.contains("first.rs:"), "{tight}");
        assert!(tight.contains("1 of 3 files"), "{tight}");
        assert!(
            tight.lines().all(|line| !line.contains("third.rs")),
            "the file that did not fit is not half in the map: {tight}"
        );
    }

    #[test]
    fn a_long_file_list_says_how_many_declarations_it_left_off() {
        let mut text = String::new();
        for index in 0..80 {
            text.push_str(&format!("pub fn function_{index}() {{}}\n"));
        }
        let root = tree("many", &[("many.rs", text.as_str())]);
        let map = Map::build(&root, &[]).expect("a map");
        let rendered = map.render(20_000);
        assert!(rendered.contains("and 40 more in this file"), "{rendered}");
        assert!(!rendered.contains("function_79"), "{rendered}");
    }

    #[test]
    fn there_is_no_map_for_a_tree_with_nothing_to_map() {
        let root = tree("empty", &[]);
        let map = Map::build(&root, &[]).expect("a map");
        assert!(map.files().is_empty());
        assert_eq!(map.render(1000), "");
        assert_eq!(
            section(&root, "do the thing", 1000).expect("no fault"),
            None
        );
    }

    #[test]
    fn a_tree_that_is_not_a_directory_is_refused() {
        let missing = scratch_base().join("hades-map-does-not-exist");
        let error = Map::build(&missing, &[]).expect_err("no tree");
        assert!(
            error.detail().contains("not a directory"),
            "{}",
            error.detail()
        );
    }

    #[test]
    fn the_same_tree_ranks_the_same_way_twice() {
        let root = tree(
            "stable",
            &[
                ("a.rs", "use crate::b;\n"),
                ("b.rs", "use crate::a;\n"),
                ("c.rs", "use crate::a;\nuse crate::b;\n"),
            ],
        );
        let first = Map::build(&root, &[]).expect("a map");
        let second = Map::build(&root, &[]).expect("a map");
        assert_eq!(first, second, "a fixed number of passes, so no run drifts");
    }

    #[test]
    fn a_goal_seeds_the_words_that_could_name_a_file() {
        let words = seed_words("fix the failing test in turn_oracle.rs, please");
        assert!(words.contains(&"turn_oracle.rs".to_string()), "{words:?}");
        assert!(words.contains(&"failing".to_string()), "{words:?}");
        assert!(!words.contains(&"in".to_string()), "{words:?}");
        assert!(!words.contains(&"rs".to_string()), "{words:?}");
    }

    #[test]
    fn the_section_is_named_for_the_model() {
        let root = tree("section", &[("a.rs", "pub fn a() {}\n")]);
        let section = section(&root, "change a", 500)
            .expect("no fault")
            .expect("a section");
        assert_eq!(section.name(), "repo map");
        assert!(section.text().contains("a.rs:"), "{}", section.text());
    }
}
