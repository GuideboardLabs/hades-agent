//! `.hadepack`: a portable bundle of prompts, skills and cases.
//!
//! The format is deliberately plain: a deterministic tar.gz with a strict JSON manifest, a checksum file,
//! and a `files/` tree. Export never invents content; it copies what the state already has. Inspect and plan
//! read a pack without a model and without writing to the live state.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Map, Value, json};

use crate::cases::{self, Case};
use crate::harness_files::Files;
use crate::scratch;
use crate::skills;

pub const FORMAT: u32 = 1;
pub const MAX_MANIFEST: u64 = 64 * 1024;
pub const MAX_ENTRIES: usize = 4096;
pub const MAX_BYTES: u64 = 64 * 1024 * 1024;
/// Compressed bytes on disk. Checked from metadata before the file is read at all, so an archive that is
/// merely huge never becomes this process's memory.
pub const MAX_ARCHIVE: u64 = 128 * 1024 * 1024;
/// The `tar -t` listing, which is read before anything is extracted.
pub const MAX_LISTING: usize = 4 * 1024 * 1024;
/// Total bytes after decompression. A gzip header can claim any size, so this is counted from the stream.
pub const MAX_EXPANDED: u64 = 512 * 1024 * 1024;
/// How long extraction may take before the child is killed and the tree removed.
pub const MAX_EXTRACT_SECONDS: u64 = 20;

const KNOWN_ROLES: [&str; 5] = ["survey", "patch", "sweep", "verify", "record"];
/// Whether this build can supply a tool at all. The list lives with the surface that calls tools, so a tool
/// added there is a tool a pack may require; a second list here is how `browser` came to be rejected by a
/// runtime that ships it. `verify` is the one name a pack may require that is not a model-facing tool.
fn known_tool(name: &str) -> bool {
    crate::agent::TOOLS.contains(&name) || name == crate::VERIFY
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Skill,
    Harness,
    Case,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Skill => "skill",
            Kind::Harness => "harness",
            Kind::Case => "case",
        }
    }

    pub fn dir(self) -> &'static str {
        match self {
            Kind::Skill => "skills",
            Kind::Harness => "harness",
            Kind::Case => "cases",
        }
    }

    fn rank(self) -> u8 {
        match self {
            Kind::Skill => 0,
            Kind::Harness => 1,
            Kind::Case => 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provided {
    pub kind: Kind,
    pub identity: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Requires {
    pub hades: String,
    pub roles: Vec<String>,
    pub tools: Vec<String>,
    /// Claims the pack makes about the machine it will run on, checked by `hades pack check` before
    /// anything is installed. Optional: a pack that declares none is installed on the older terms,
    /// where a person reads the docs.
    pub assumptions: Vec<Assumption>,
}

/// One claim about the host: "8 GiB of VRAM", "port 8115 is free", "the endpoint answers".
///
/// The pack wrote these on the machine it was built on. `pack check` re-reads them here, and the
/// remedy travels with the claim so a false one arrives with what to do instead of a verdict alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assumption {
    pub id: String,
    pub claim: String,
    pub check: String,
    /// A scalar argument for the check, carried as text so the manifest stays plain JSON.
    pub arg: Option<String>,
    pub severity: Severity,
    pub resolution: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Results are meaningless until this is settled.
    Hard,
    /// Translate the pack's advice rather than stop.
    Soft,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provides {
    pub skills: Vec<Provided>,
    pub harnesses: Vec<Provided>,
    pub cases: Vec<Provided>,
}

impl Provides {
    pub fn all(&self) -> Vec<&Provided> {
        let mut items: Vec<&Provided> = self
            .skills
            .iter()
            .chain(self.harnesses.iter())
            .chain(self.cases.iter())
            .collect();
        items.sort_by(|a, b| {
            a.kind
                .rank()
                .cmp(&b.kind.rank())
                .then_with(|| a.identity.cmp(&b.identity))
        });
        items
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    pub network: bool,
    pub writes: Vec<String>,
    pub code: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub format: u32,
    pub id: String,
    pub version: String,
    pub title: String,
    pub summary: String,
    pub license: String,
    pub derivatives: String,
    pub requires: Requires,
    pub provides: Provides,
    pub capabilities: Capabilities,
    /// Words a person would search for. Optional, because most packs are found by their `id` and summary;
    /// it exists so a menu can be generated without a second file to keep in step with the manifest.
    pub keywords: Vec<String>,
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Manifest, String> {
        let value: Value = serde_json::from_str(text)
            .map_err(|error| format!("pack.json is not JSON: {error}"))?;
        let root = object(&value, "pack.json")?;
        keys(
            root,
            &[
                "format",
                "id",
                "version",
                "title",
                "summary",
                "license",
                "derivatives",
                "requires",
                "provides",
                "capabilities",
                "keywords",
            ],
            "pack.json",
        )?;
        for forbidden in ["roles", "floor", "cage", "policy"] {
            if root.contains_key(forbidden) {
                return Err(format!(
                    "pack.json may not declare `{forbidden}`; guardrails are not pack content"
                ));
            }
        }
        let format = root
            .get("format")
            .and_then(Value::as_u64)
            .ok_or_else(|| "pack.json `format` must be an integer".to_string())?
            as u32;
        if format != FORMAT {
            return Err(format!(
                "pack.json has format {format}; this build reads format {FORMAT}"
            ));
        }
        let id = string(root, "id", "pack.json")?;
        valid_id(&id)?;
        let version = string(root, "version", "pack.json")?;
        valid_version(&version)?;
        let derivatives = string(root, "derivatives", "pack.json")?;
        if derivatives != "allowed" && derivatives != "forbidden" {
            return Err("pack.json `derivatives` must be `allowed` or `forbidden`".to_string());
        }
        let requires = parse_requires(
            root.get("requires")
                .ok_or_else(|| "pack.json is missing `requires`".to_string())?,
        )?;
        let provides = parse_provides(
            root.get("provides")
                .ok_or_else(|| "pack.json is missing `provides`".to_string())?,
        )?;
        let capabilities = parse_capabilities(
            root.get("capabilities")
                .ok_or_else(|| "pack.json is missing `capabilities`".to_string())?,
        )?;
        let keywords = match root.get("keywords") {
            None => Vec::new(),
            Some(_) => strings(root, "keywords", "pack.json")?,
        };
        Ok(Manifest {
            format,
            id,
            version,
            title: string(root, "title", "pack.json")?,
            summary: string(root, "summary", "pack.json")?,
            license: string(root, "license", "pack.json")?,
            derivatives,
            requires,
            provides,
            capabilities,
            keywords,
        })
    }

    pub fn to_value(&self) -> Value {
        json!({
            "format": self.format,
            "id": self.id,
            "version": self.version,
            "title": self.title,
            "summary": self.summary,
            "license": self.license,
            "derivatives": self.derivatives,
            "requires": {
                "hades": self.requires.hades,
                "roles": self.requires.roles,
                "tools": self.requires.tools,
            },
            "provides": {
                "skills": self.provides.skills.iter().map(item_value).collect::<Vec<_>>(),
                "harnesses": self.provides.harnesses.iter().map(item_value).collect::<Vec<_>>(),
                "cases": self.provides.cases.iter().map(item_value).collect::<Vec<_>>(),
            },
            "capabilities": {
                "network": self.capabilities.network,
                "writes": self.capabilities.writes,
                "code": self.capabilities.code,
            },
            "keywords": self.keywords,
        })
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, String> {
        let mut text = serde_json::to_string_pretty(&self.to_value())
            .map_err(|error| format!("could not encode pack.json: {error}"))?;
        text.push('\n');
        if text.len() as u64 > MAX_MANIFEST {
            return Err(format!(
                "pack.json is {} bytes; the cap is {MAX_MANIFEST}",
                text.len()
            ));
        }
        Ok(text.into_bytes())
    }
}

fn item_value(item: &Provided) -> Value {
    match item.kind {
        Kind::Skill => json!({"name": item.identity, "path": item.path}),
        Kind::Harness => json!({"profile": item.identity, "path": item.path}),
        Kind::Case => json!({"id": item.identity, "path": item.path}),
    }
}

fn parse_requires(value: &Value) -> Result<Requires, String> {
    let map = object(value, "requires")?;
    keys(map, &["hades", "roles", "tools", "assumptions"], "requires")?;
    let hades = string(map, "hades", "requires")?;
    version_range(&hades)?;
    let assumptions = match map.get("assumptions") {
        Some(value) => parse_assumptions(value)?,
        None => Vec::new(),
    };
    Ok(Requires {
        hades,
        roles: strings(map, "roles", "requires")?,
        tools: strings(map, "tools", "requires")?,
        assumptions,
    })
}

fn parse_provides(value: &Value) -> Result<Provides, String> {
    let map = object(value, "provides")?;
    keys(map, &["skills", "harnesses", "cases"], "provides")?;
    let mut seen = BTreeSet::new();
    let skills = items(map, "skills", Kind::Skill, &mut seen)?;
    let harnesses = items(map, "harnesses", Kind::Harness, &mut seen)?;
    let cases = items(map, "cases", Kind::Case, &mut seen)?;
    Ok(Provides {
        skills,
        harnesses,
        cases,
    })
}

fn items(
    map: &Map<String, Value>,
    key: &str,
    kind: Kind,
    seen: &mut BTreeSet<(Kind, String)>,
) -> Result<Vec<Provided>, String> {
    let Some(value) = map.get(key) else {
        return Err(format!("provides is missing `{key}`"));
    };
    let array = value
        .as_array()
        .ok_or_else(|| format!("provides `{key}` must be an array"))?;
    let mut items = Vec::new();
    let key = match kind {
        Kind::Skill => "name",
        Kind::Harness => "profile",
        Kind::Case => "id",
    };
    for entry in array {
        let item = object(entry, &format!("provides.{key}"))?;
        keys(item, &[key, "path"], &format!("provides.{key}"))?;
        let identity = string(item, key, &format!("provides.{key}"))?;
        let path = string(item, "path", &format!("provides.{key}"))?;
        if identity.contains('/') {
            return Err(format!("provides.{key} `{identity}` is not a single name"));
        }
        valid_id(&identity)
            .map_err(|_| format!("provides.{key} `{identity}` is not a valid name"))?;
        let expected = format!("files/{}/{identity}", kind.dir());
        if path != expected {
            return Err(format!(
                "provides.{key} path `{path}` does not match identity `{identity}`; expected `{expected}`"
            ));
        }
        valid_path(&path)?;
        if !seen.insert((kind, identity.clone())) {
            return Err(format!("provides.{key} repeats `{identity}`"));
        }
        items.push(Provided {
            kind,
            identity,
            path,
        });
    }
    items.sort_by(|a, b| a.identity.cmp(&b.identity));
    Ok(items)
}

fn parse_capabilities(value: &Value) -> Result<Capabilities, String> {
    let map = object(value, "capabilities")?;
    keys(map, &["network", "writes", "code"], "capabilities")?;
    let network = map
        .get("network")
        .and_then(Value::as_bool)
        .ok_or_else(|| "capabilities `network` must be a boolean".to_string())?;
    let code = map
        .get("code")
        .and_then(Value::as_bool)
        .ok_or_else(|| "capabilities `code` must be a boolean".to_string())?;
    if code {
        return Err(
            "capabilities `code: true` is refused in format 1; code items are not a pack"
                .to_string(),
        );
    }
    let writes = strings(map, "writes", "capabilities")?;
    for write in &writes {
        if write != "vault" && write != "state" {
            return Err(format!(
                "capabilities `writes` names `{write}`; say `vault` or `state`"
            ));
        }
    }
    Ok(Capabilities {
        network,
        writes,
        code,
    })
}

fn object<'a>(value: &'a Value, context: &str) -> Result<&'a Map<String, Value>, String> {
    value
        .as_object()
        .ok_or_else(|| format!("{context} must be a JSON object"))
}

fn keys(map: &Map<String, Value>, allowed: &[&str], context: &str) -> Result<(), String> {
    for key in map.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(format!("{context} has unknown key `{key}`"));
        }
    }
    Ok(())
}

fn string(map: &Map<String, Value>, key: &str, context: &str) -> Result<String, String> {
    map.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("{context} `{key}` must be a string"))
}

fn strings(map: &Map<String, Value>, key: &str, context: &str) -> Result<Vec<String>, String> {
    let Some(value) = map.get(key) else {
        return Ok(Vec::new());
    };
    let array = value
        .as_array()
        .ok_or_else(|| format!("{context} `{key}` must be an array"))?;
    array
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("{context} `{key}` must contain strings"))
        })
        .collect()
}

pub fn valid_id(id: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > 128 || id.starts_with('/') || id.ends_with('/') {
        return Err(format!("`{id}` is not a pack id"));
    }
    for segment in id.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(format!(
                "`{id}` is not a pack id; it has an empty or dot segment"
            ));
        }
        if !segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
            || !segment
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric())
        {
            return Err(format!(
                "`{id}` is not a pack id; use letters, digits, `.`, `_`, `-` and `/`"
            ));
        }
    }
    Ok(())
}

pub fn valid_version(version: &str) -> Result<(), String> {
    parse_version(version).map(|_| ())
}

fn valid_path(path: &str) -> Result<(), String> {
    if !path.starts_with("files/") || path.starts_with('/') || path.contains('\\') {
        return Err(format!("`{path}` is not a path inside the pack"));
    }
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(format!("`{path}` has an empty or dot segment"));
        }
    }
    Ok(())
}

fn parse_version(text: &str) -> Result<(u64, u64, u64), String> {
    let parts: Vec<&str> = text.trim().split('.').collect();
    if parts.is_empty() || parts.len() > 3 || parts.iter().any(|part| part.is_empty()) {
        return Err(format!("`{text}` is not a version"));
    }
    let mut numbers = [0u64; 3];
    for (index, part) in parts.iter().enumerate() {
        if !part.chars().all(|c| c.is_ascii_digit()) {
            return Err(format!("`{text}` is not a version"));
        }
        numbers[index] = part
            .parse()
            .map_err(|_| format!("`{text}` is not a version"))?;
    }
    Ok((numbers[0], numbers[1], numbers[2]))
}

/// The Hades line a pack exported from this build was written against.
///
/// The upper bound is derived, not fixed. A hard-coded `<0.2` made every pack exported from 0.2.0
/// declare a range no build could satisfy, so the pack refused to install where it was made.
fn default_range() -> String {
    let current = env!("CARGO_PKG_VERSION");
    let mut parts = current.split('.');
    let major = parts.next().unwrap_or("0");
    let minor = parts.next().unwrap_or("0").parse::<u64>().unwrap_or(0);
    format!(">={current},<{major}.{}", minor + 1)
}

fn parse_assumptions(value: &Value) -> Result<Vec<Assumption>, String> {
    let array = value
        .as_array()
        .ok_or_else(|| "requires `assumptions` must be an array".to_string())?;
    let mut out: Vec<Assumption> = Vec::new();
    for (index, item) in array.iter().enumerate() {
        let context = format!("requires.assumptions[{index}]");
        let map = object(item, &context)?;
        keys(
            map,
            &["id", "claim", "check", "arg", "severity", "resolution"],
            &context,
        )?;
        let id = string(map, "id", &context)?;
        if id.trim().is_empty() {
            return Err(format!("{context} `id` must not be empty"));
        }
        if out.iter().any(|seen| seen.id == id) {
            return Err(format!("{context} repeats the id `{id}`"));
        }
        let claim = string(map, "claim", &context)?;
        if claim.trim().is_empty() {
            return Err(format!("{context} `claim` must say what was assumed"));
        }
        let check = string(map, "check", &context)?;
        let severity = match string(map, "severity", &context)?.as_str() {
            "hard" => Severity::Hard,
            "soft" => Severity::Soft,
            other => {
                return Err(format!(
                    "{context} `severity` is `{other}`; say `hard` or `soft`"
                ));
            }
        };
        let arg = match map.get("arg") {
            None | Some(Value::Null) => None,
            Some(Value::String(text)) => Some(text.clone()),
            Some(Value::Number(number)) => Some(number.to_string()),
            Some(Value::Bool(flag)) => Some(flag.to_string()),
            Some(_) => {
                return Err(format!(
                    "{context} `arg` must be a string, a number or a bool"
                ));
            }
        };
        let resolution = map
            .get("resolution")
            .and_then(Value::as_str)
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty());
        out.push(Assumption {
            id,
            claim,
            check,
            arg,
            severity,
            resolution,
        });
    }
    Ok(out)
}

fn version_range(range: &str) -> Result<(), String> {
    if range.trim().is_empty() {
        return Err("requires.hades is empty".to_string());
    }
    for part in range.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err(format!("requires.hades `{range}` has an empty comparator"));
        }
        let (operator, rest) = if let Some(rest) = part.strip_prefix(">=") {
            (">=", rest)
        } else if let Some(rest) = part.strip_prefix("<=") {
            ("<=", rest)
        } else if let Some(rest) = part.strip_prefix('>') {
            (">", rest)
        } else if let Some(rest) = part.strip_prefix('<') {
            ("<", rest)
        } else if let Some(rest) = part.strip_prefix('=') {
            ("=", rest)
        } else {
            ("=", part)
        };
        if rest.trim().is_empty() {
            return Err(format!(
                "requires.hades `{range}` has no version after `{operator}`"
            ));
        }
        parse_version(rest.trim())?;
    }
    Ok(())
}

pub fn range_ok(range: &str, current: &str) -> Result<bool, String> {
    version_range(range)?;
    let current = parse_version(current)?;
    for part in range.split(',') {
        let part = part.trim();
        let (operator, rest) = if let Some(rest) = part.strip_prefix(">=") {
            (">=", rest)
        } else if let Some(rest) = part.strip_prefix("<=") {
            ("<=", rest)
        } else if let Some(rest) = part.strip_prefix('>') {
            (">", rest)
        } else if let Some(rest) = part.strip_prefix('<') {
            ("<", rest)
        } else if let Some(rest) = part.strip_prefix('=') {
            ("=", rest)
        } else {
            ("=", part)
        };
        let wanted = parse_version(rest.trim())?;
        let yes = match operator {
            ">=" => current >= wanted,
            ">" => current > wanted,
            "<=" => current <= wanted,
            "<" => current < wanted,
            _ => current == wanted,
        };
        if !yes {
            return Ok(false);
        }
    }
    Ok(true)
}

// ── archive ──────────────────────────────────────────────────────────────────

pub struct Pack {
    root: PathBuf,
    pub manifest: Manifest,
    pub digest: String,
    pub bytes: u64,
}

impl Drop for Pack {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

impl Pack {
    pub fn open(file: &Path) -> Result<Pack, String> {
        ensure_gnu_tar()?;
        // The archive cap is a metadata check, so a 4 GB file is refused without being read.
        let on_disk = fs::metadata(file)
            .map_err(|error| format!("could not stat {}: {error}", file.display()))?
            .len();
        if on_disk > MAX_ARCHIVE {
            return Err(format!(
                "{} is {on_disk} bytes; the archive cap is {MAX_ARCHIVE}",
                file.display()
            ));
        }
        let digest = file_digest(file)?;
        let uncompressed = expanded_size(file)?;
        if uncompressed > MAX_EXPANDED {
            return Err(format!(
                "{} expands past {uncompressed} bytes; the cap is {MAX_EXPANDED}",
                file.display()
            ));
        }
        let names = capture_bounded(
            "tar",
            &["-tzf", &file.to_string_lossy()],
            MAX_LISTING,
            "the tar listing",
        )?;
        let types = capture_bounded(
            "tar",
            &["-tvzf", &file.to_string_lossy()],
            MAX_LISTING,
            "the tar listing",
        )?;
        validate_listing(&names, &types)?;
        let root = scratch::unique("pack-open");
        scratch::state_dir(&root)
            .map_err(|error| format!("could not make {}: {error}", root.display()))?;
        let cleanup = Cleanup(root.clone());
        let text = file.to_string_lossy().to_string();
        // Extraction runs bounded in time as well as in bytes: a nested archive or a device node can make
        // tar sit there, and a pack that never finishes is a pack that never gets refused.
        let mut child = Command::new("tar")
            .args([
                "-xzf",
                &text,
                "-C",
                &root.to_string_lossy(),
                "--same-permissions",
                "--no-same-owner",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("could not run tar: {error}"))?;
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(MAX_EXTRACT_SECONDS);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                Err(error) => return Err(format!("could not wait for tar: {error}")),
            }
        };
        let Some(status) = status else {
            return Err(format!(
                "{} did not extract within {MAX_EXTRACT_SECONDS}s; the tree was removed",
                file.display()
            ));
        };
        if !status.success() {
            return Err(format!("tar could not extract {}", file.display()));
        }
        set_mode(&root, 0o700)?;
        let files = walk_regular(&root)?;
        let bytes = files.iter().map(|(_, _, size)| *size).sum::<u64>();
        if bytes > MAX_BYTES {
            return Err(format!(
                "{} has {bytes} bytes after extraction; the cap is {MAX_BYTES}",
                file.display()
            ));
        }
        let manifest_path = root.join("pack.json");
        let manifest_bytes = fs::read(&manifest_path)
            .map_err(|error| format!("pack has no readable pack.json: {error}"))?;
        if manifest_bytes.len() as u64 > MAX_MANIFEST {
            return Err(format!(
                "pack.json is {} bytes; the cap is {MAX_MANIFEST}",
                manifest_bytes.len()
            ));
        }
        let manifest_text =
            String::from_utf8(manifest_bytes).map_err(|_| "pack.json is not UTF-8".to_string())?;
        let manifest = Manifest::parse(&manifest_text)?;
        for item in manifest.provides.all() {
            let path = root.join(&item.path);
            if !path.is_dir() {
                return Err(format!(
                    "pack is missing provided directory `{}`",
                    item.path
                ));
            }
        }
        verify_sums(&root, &files)?;
        std::mem::forget(cleanup);
        Ok(Pack {
            root,
            manifest,
            digest,
            bytes,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn inspect(&self) -> String {
        let items = self.manifest.provides.all();
        let mut counts = BTreeMap::new();
        for item in &items {
            *counts.entry(item.kind.name()).or_insert(0usize) += 1;
        }
        let mut text = format!(
            "pack: {} {}\ntitle: {}\nlicense: {} (derivatives: {})\nrequires: hades {}",
            self.manifest.id,
            self.manifest.version,
            self.manifest.title,
            self.manifest.license,
            self.manifest.derivatives,
            self.manifest.requires.hades
        );
        if !self.manifest.requires.roles.is_empty() {
            text.push_str(&format!(
                "; roles: {}",
                self.manifest.requires.roles.join(", ")
            ));
        }
        if !self.manifest.requires.tools.is_empty() {
            text.push_str(&format!(
                "; tools: {}",
                self.manifest.requires.tools.join(", ")
            ));
        }
        text.push('\n');
        let mut capabilities = Vec::new();
        if self.manifest.capabilities.network {
            capabilities.push("network".to_string());
        }
        if !self.manifest.capabilities.writes.is_empty() {
            capabilities.push(format!(
                "writes {}",
                self.manifest.capabilities.writes.join(", ")
            ));
        }
        capabilities.push(format!("code {}", self.manifest.capabilities.code));
        text.push_str(&format!("capabilities: {}\n", capabilities.join("; ")));
        text.push_str(&format!(
            "items: {} skill, {} harness, {} case\n",
            counts.get("skill").copied().unwrap_or(0),
            counts.get("harness").copied().unwrap_or(0),
            counts.get("case").copied().unwrap_or(0)
        ));
        text.push_str(&format!("bytes: {} (uncompressed)\n", self.bytes));
        text.push_str(&format!("sha256: {}\n", self.digest));
        text.push_str(&format!(
            "signature: {}\n",
            if self.root.join("signature").exists() {
                "present, not verified"
            } else {
                "unsigned"
            }
        ));
        let warnings = self.warnings();
        if warnings.is_empty() {
            text.push_str("warnings: none\n");
        } else {
            text.push_str(&format!("warnings: {}\n", warnings.join("; ")));
        }
        text
    }

    fn warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        if self.manifest.capabilities.network {
            warnings.push("asks for the network".to_string());
        }
        if self
            .manifest
            .capabilities
            .writes
            .iter()
            .any(|write| write == "vault")
        {
            warnings.push("writes the vault".to_string());
        }
        warnings
    }

    pub fn inspect_json(&self) -> Value {
        let items = self
            .manifest
            .provides
            .all()
            .into_iter()
            .map(|item| json!({"kind": item.kind.name(), "name": item.identity, "path": item.path}))
            .collect::<Vec<_>>();
        json!({
            "id": self.manifest.id,
            "version": self.manifest.version,
            "title": self.manifest.title,
            "summary": self.manifest.summary,
            "license": self.manifest.license,
            "derivatives": self.manifest.derivatives,
            "requires": {
                "hades": self.manifest.requires.hades,
                "roles": self.manifest.requires.roles,
                "tools": self.manifest.requires.tools,
            },
            "capabilities": {
                "network": self.manifest.capabilities.network,
                "writes": self.manifest.capabilities.writes,
                "code": self.manifest.capabilities.code,
            },
            "items": items,
            "bytes": self.bytes,
            "sha256": self.digest,
            "signature": if self.root.join("signature").exists() { "present, not verified" } else { "unsigned" },
            "warnings": self.warnings(),
        })
    }
}

fn validate_listing(names: &str, types: &str) -> Result<(), String> {
    let names: Vec<&str> = names.lines().filter(|line| !line.is_empty()).collect();
    let types: Vec<&str> = types.lines().filter(|line| !line.is_empty()).collect();
    if names.len() != types.len() {
        return Err("tar named a different number of entries than it typed".to_string());
    }
    if names.len() > MAX_ENTRIES {
        return Err(format!(
            "pack has {} entries; the cap is {MAX_ENTRIES}",
            names.len()
        ));
    }
    let mut seen = BTreeSet::new();
    for (name, line) in names.iter().zip(types.iter()) {
        let kind = line
            .as_bytes()
            .first()
            .copied()
            .ok_or_else(|| "empty tar entry".to_string())?;
        if kind != b'-' && kind != b'd' {
            return Err(format!(
                "pack contains a non-regular entry: {}",
                line.trim()
            ));
        }
        let name = name.strip_prefix("./").unwrap_or(name);
        if name.is_empty() || name == "." {
            if kind == b'd' {
                continue;
            }
            return Err("pack has a file with an empty name".to_string());
        }
        let check = name.trim_end_matches('/');
        if check.starts_with('/')
            || check
                .split('/')
                .any(|segment| segment == ".." || segment == "." || segment.is_empty())
            || check.contains('\\')
            || check.chars().any(|c| c == '\0' || c == '\n' || c == '\r')
        {
            return Err(format!("pack contains an unsafe path `{name}`"));
        }
        if kind == b'd' && !name.ends_with('/') {
            return Err(format!("pack directory `{name}` does not end with `/`"));
        }
        if !seen.insert(check.to_string()) {
            return Err(format!("pack repeats `{name}`"));
        }
    }
    Ok(())
}

/// The SHA-256 of a file, read in chunks. The archive cap bounds the size; this bounds the memory.
pub fn file_digest(file: &Path) -> Result<String, String> {
    use std::io::Read;
    let mut handle = fs::File::open(file)
        .map_err(|error| format!("could not read {}: {error}", file.display()))?;
    let mut digest = mneme::sha256::Digest::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = handle
            .read(&mut buffer)
            .map_err(|error| format!("could not read {}: {error}", file.display()))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(digest.finish())
}

/// How many bytes the archive expands to, counted from a decompression stream that stops one byte past the
/// cap. `gzip -l` reports what the trailer claims, and a trailer is written by whoever built the archive.
fn expanded_size(file: &Path) -> Result<u64, String> {
    use std::io::Read;
    let mut child = Command::new("gzip")
        .arg("-dc")
        .arg(file)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("could not run gzip: {error}"))?;
    let mut out = child
        .stdout
        .take()
        .ok_or_else(|| "gzip gave no output".to_string())?;
    let mut buffer = vec![0u8; 1 << 20];
    let mut total: u64 = 0;
    loop {
        let read = match out.read(&mut buffer) {
            Ok(read) => read,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "gzip could not decompress {}: {error}",
                    file.display()
                ));
            }
        };
        if read == 0 {
            break;
        }
        total += read as u64;
        if total > MAX_EXPANDED {
            // One byte past the cap is enough to refuse it; there is no reason to finish decompressing.
            let _ = child.kill();
            let _ = child.wait();
            return Ok(total);
        }
    }
    let _ = child.wait();
    Ok(total)
}

/// Run a program and keep at most `limit` bytes of its output. The listing of an archive is read before the
/// archive is extracted, so this is where a pack with a million entries is refused cheaply.
fn capture_bounded(
    program: &str,
    args: &[&str],
    limit: usize,
    label: &str,
) -> Result<String, String> {
    use std::io::Read;
    let mut child = Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not run {program}: {error}"))?;
    let mut out = child
        .stdout
        .take()
        .ok_or_else(|| format!("{program} gave no output"))?;
    let mut kept: Vec<u8> = Vec::new();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut overflow = false;
    loop {
        let read = out
            .read(&mut buffer)
            .map_err(|error| format!("could not read {program} output: {error}"))?;
        if read == 0 {
            break;
        }
        if kept.len() + read > limit {
            overflow = true;
            break;
        }
        kept.extend_from_slice(&buffer[..read]);
    }
    if overflow {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!(
            "{label} is over {limit} bytes; the pack is refused"
        ));
    }
    let output = child
        .wait_with_output()
        .map_err(|error| format!("could not wait for {program}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "{program} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(kept).map_err(|_| format!("{program} wrote non-UTF-8 output"))
}

fn verify_sums(root: &Path, files: &[(String, PathBuf, u64)]) -> Result<(), String> {
    let path = root.join("SHA256SUMS");
    let text = fs::read_to_string(&path)
        .map_err(|error| format!("pack has no readable SHA256SUMS: {error}"))?;
    let mut sums = BTreeMap::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let (digest, name) = line
            .split_once("  ")
            .ok_or_else(|| format!("SHA256SUMS line is not `<digest>  <path>`: {line}"))?;
        if digest.len() != 64 || !digest.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!("SHA256SUMS has a bad digest for `{name}`"));
        }
        let name = name.strip_prefix("./").unwrap_or(name);
        if sums
            .insert(name.to_string(), digest.to_ascii_lowercase())
            .is_some()
        {
            return Err(format!("SHA256SUMS repeats `{name}`"));
        }
    }
    let mut listed = BTreeSet::new();
    for (relative, path, _) in files {
        if relative == "SHA256SUMS" || relative == "signature" {
            continue;
        }
        let wanted = sums
            .get(relative)
            .ok_or_else(|| format!("SHA256SUMS does not list `{relative}`"))?;
        let bytes = fs::read(path)
            .map_err(|error| format!("could not read {}: {error}", path.display()))?;
        let found = mneme::sha256::hex(&bytes);
        if &found != wanted {
            return Err(format!("SHA256SUMS does not match `{relative}`"));
        }
        listed.insert(relative.clone());
    }
    for name in sums.keys() {
        if !listed.contains(name) {
            return Err(format!(
                "SHA256SUMS lists `{name}`, which is not a file in the pack"
            ));
        }
    }
    Ok(())
}

// ── export ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ExportOptions {
    pub id: String,
    pub version: String,
    pub from: PathBuf,
    pub title: String,
    pub summary: String,
    pub license: String,
    pub derivatives: String,
    pub network: bool,
    pub writes: Vec<String>,
    pub requires_roles: Vec<String>,
    pub requires_tools: Vec<String>,
    pub skills: Vec<String>,
    pub profiles: Vec<String>,
    pub cases: Vec<String>,
    pub keywords: Vec<String>,
    /// A skill folder that is not installed in `from`: the authoring case, where a person has a skill in a
    /// directory and wants to share it without seeding it into a state first.
    pub source: Option<PathBuf>,
    pub out: Option<PathBuf>,
}

pub fn export(options: &ExportOptions) -> Result<PathBuf, String> {
    ensure_gnu_tar()?;
    valid_id(&options.id)?;
    valid_version(&options.version)?;
    if options.derivatives != "allowed" && options.derivatives != "forbidden" {
        return Err("`--derivatives` must be `allowed` or `forbidden`".to_string());
    }
    for role in &options.requires_roles {
        if !KNOWN_ROLES.contains(&role.as_str()) {
            return Err(format!("`{role}` is not a role this build ships"));
        }
    }
    for tool in &options.requires_tools {
        if !known_tool(tool.as_str()) {
            return Err(format!("`{tool}` is not a tool this build ships"));
        }
    }
    for write in &options.writes {
        if write != "vault" && write != "state" {
            return Err(format!("`--writes {write}` must be `vault` or `state`"));
        }
    }
    if options.skills.is_empty() && options.profiles.is_empty() && options.cases.is_empty() {
        return Err("pack export needs at least one --skill, --profile or --case".to_string());
    }
    // `--from` is where installed skills, profiles and cases are looked up. A folder export reads none of
    // them, so it must not need a state directory to exist for a pack that was never installed.
    let needs_from =
        options.source.is_none() || !options.profiles.is_empty() || !options.cases.is_empty();
    if needs_from && !options.from.is_dir() {
        return Err(format!(
            "`--from {}` is not a directory",
            options.from.display()
        ));
    }
    let stage = scratch::unique("pack-stage");
    scratch::state_dir(&stage)
        .map_err(|error| format!("could not make {}: {error}", stage.display()))?;
    let _cleanup = Cleanup(stage.clone());
    set_mode(&stage, 0o755)?;
    let files_root = stage.join("files");
    make_dir(&files_root)?;

    let mut provided_skills = Vec::new();
    let skills_selected = unique(&options.skills);
    for name in &skills_selected {
        valid_id(name).map_err(|_| format!("`{name}` is not a valid skill name"))?;
        let dest = files_root.join("skills").join(name);
        make_dir(&dest)?;
        // A named folder wins, then the state resolver, which knows installed, proposed and built-in: a
        // built-in skill is the one most likely to be worth sharing and it was the one export could not see.
        let folder = options
            .source
            .clone()
            .filter(|dir| dir.file_name().and_then(|leaf| leaf.to_str()) == Some(name));
        if let Some(folder) = folder {
            if !folder.join("SKILL.md").is_file() {
                return Err(format!(
                    "`{}` is not a skill folder: it has no SKILL.md",
                    folder.display()
                ));
            }
            copy_tree(&folder, &dest)?;
        } else {
            let skill = skills::find(&options.from, name).ok_or_else(|| {
                format!(
                    "no skill called `{name}` under {}, installed, proposed or built in; pass --dir <folder> \
                     to export a folder that was never installed",
                    options.from.display()
                )
            })?;
            if skill.path.file_name().and_then(|name| name.to_str()) == Some("SKILL.md") {
                let root = skill
                    .path
                    .parent()
                    .ok_or_else(|| format!("skill `{name}` has no directory"))?;
                copy_tree(root, &dest)?;
            } else {
                copy_file(&skill.path, &dest.join("SKILL.md"))?;
            }
        }
        provided_skills.push(Provided {
            kind: Kind::Skill,
            identity: name.clone(),
            path: format!("files/skills/{name}"),
        });
    }

    let mut provided_harnesses = Vec::new();
    let profiles_selected = unique(&options.profiles);
    for profile in &profiles_selected {
        valid_id(profile).map_err(|_| format!("`{profile}` is not a valid profile name"))?;
        let plain = Path::new(profile)
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == profile);
        if !plain {
            return Err(format!("`{profile}` is not a plain profile directory name"));
        }
        let source = options.from.join("harness").join(profile);
        if !source.join("system.md").is_file() {
            return Err(format!(
                "no harness for `{profile}` at {}",
                source.display()
            ));
        }
        let dest = files_root.join("harness").join(profile);
        make_dir(&dest)?;
        copy_file(&source.join("system.md"), &dest.join("system.md"))?;
        let guidance = source.join("tool_guidance.md");
        if guidance.is_file() {
            copy_file(&guidance, &dest.join("tool_guidance.md"))?;
        }
        provided_harnesses.push(Provided {
            kind: Kind::Harness,
            identity: profile.clone(),
            path: format!("files/harness/{profile}"),
        });
    }

    let mut provided_cases = Vec::new();
    let profiles = cases::profiles(&options.from);
    let cases_selected = unique(&options.cases);
    for id in &cases_selected {
        valid_id(id).map_err(|_| format!("`{id}` is not a valid case name"))?;
        let mut found: Option<Case> = None;
        for profile in &profiles {
            if let Some(case) = cases::find(&options.from, profile, id)? {
                if found.is_some() {
                    return Err(format!("case `{id}` exists under more than one profile"));
                }
                found = Some(case);
            }
        }
        let case = found
            .ok_or_else(|| format!("no case called `{id}` under {}", options.from.display()))?;
        let tree = cases::pinned_tree(&options.from, &case)?;
        let dest = files_root.join("cases").join(id);
        make_dir(&dest)?;
        let case_json = serde_json::to_string_pretty(&case.to_value())
            .map_err(|error| format!("could not encode case `{id}`: {error}"))?;
        fs::write(dest.join("case.json"), format!("{case_json}\n"))
            .map_err(|error| format!("could not write case.json: {error}"))?;
        set_mode(&dest.join("case.json"), 0o644)?;
        copy_tree(&tree, &dest.join("tree"))?;
        provided_cases.push(Provided {
            kind: Kind::Case,
            identity: id.clone(),
            path: format!("files/cases/{id}"),
        });
    }

    provided_skills.sort_by(|a, b| a.identity.cmp(&b.identity));
    provided_harnesses.sort_by(|a, b| a.identity.cmp(&b.identity));
    provided_cases.sort_by(|a, b| a.identity.cmp(&b.identity));
    let manifest = Manifest {
        format: FORMAT,
        id: options.id.clone(),
        version: options.version.clone(),
        title: if options.title.trim().is_empty() {
            options.id.clone()
        } else {
            options.title.clone()
        },
        summary: options.summary.clone(),
        license: if options.license.trim().is_empty() {
            "NOASSERTION".to_string()
        } else {
            options.license.clone()
        },
        derivatives: options.derivatives.clone(),
        requires: Requires {
            assumptions: Vec::new(),
            hades: default_range(),
            roles: unique(&options.requires_roles),
            tools: unique(&options.requires_tools),
        },
        provides: Provides {
            skills: provided_skills,
            harnesses: provided_harnesses,
            cases: provided_cases,
        },
        capabilities: Capabilities {
            network: options.network,
            writes: unique(&options.writes),
            code: false,
        },
        keywords: unique(&options.keywords),
    };
    fs::write(stage.join("pack.json"), manifest.to_bytes()?)
        .map_err(|error| format!("could not write pack.json: {error}"))?;
    set_mode(&stage.join("pack.json"), 0o644)?;

    let all = walk_regular(&stage)?;
    let mut sums = Vec::new();
    for (relative, path, _) in &all {
        if relative == "SHA256SUMS" {
            continue;
        }
        let bytes = fs::read(path)
            .map_err(|error| format!("could not read {}: {error}", path.display()))?;
        sums.push((relative.clone(), mneme::sha256::hex(&bytes)));
    }
    sums.sort();
    let mut sum_text = String::new();
    for (name, digest) in sums {
        sum_text.push_str(&format!("{digest}  {name}\n"));
    }
    fs::write(stage.join("SHA256SUMS"), sum_text)
        .map_err(|error| format!("could not write SHA256SUMS: {error}"))?;
    set_mode(&stage.join("SHA256SUMS"), 0o644)?;
    make_every_directory_0755(&stage)?;

    let out = options.out.clone().unwrap_or_else(|| {
        let leaf = options.id.rsplit('/').next().unwrap_or("pack");
        PathBuf::from(format!("{leaf}-{}.hadepack", options.version))
    });
    if let Some(parent) = out.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .map_err(|error| format!("could not make {}: {error}", parent.display()))?;
    }
    write_archive(&stage, &out)?;
    Ok(out)
}

fn unique(values: &[String]) -> Vec<String> {
    let mut values = values.to_vec();
    values.sort();
    values.dedup();
    values
}

fn write_archive(stage: &Path, out: &Path) -> Result<(), String> {
    let mut tar = Command::new("tar")
        .args([
            "--sort=name",
            "--mtime=@0",
            "--owner=0",
            "--group=0",
            "--numeric-owner",
            "-C",
            &stage.to_string_lossy(),
            "-cf",
            "-",
            ".",
        ])
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not run tar: {error}"))?;
    let stdout = tar
        .stdout
        .take()
        .ok_or_else(|| "tar gave no stdout".to_string())?;
    let file = fs::File::create(out)
        .map_err(|error| format!("could not create {}: {error}", out.display()))?;
    let mut gzip = Command::new("gzip")
        .arg("-n")
        .stdin(Stdio::from(stdout))
        .stdout(Stdio::from(file))
        .spawn()
        .map_err(|error| format!("could not run gzip: {error}"))?;
    let tar_status = tar.wait().map_err(|error| format!("tar failed: {error}"))?;
    let gzip_status = gzip
        .wait()
        .map_err(|error| format!("gzip failed: {error}"))?;
    if !tar_status.success() || !gzip_status.success() {
        let _ = fs::remove_file(out);
        return Err(format!("packaging {} failed", out.display()));
    }
    Ok(())
}

fn ensure_gnu_tar() -> Result<(), String> {
    let version = capture("tar", &["--version"])?;
    if version.contains("GNU tar") {
        Ok(())
    } else {
        Err(if cfg!(target_env = "musl") {
            "pack install and export need GNU tar for a deterministic archive; this build does not bundle one, and \
             busybox tar does not produce the same bytes. Install it (`apk add tar`) and run the command \
             again. Nothing was installed."
                .to_string()
        } else {
            "pack install and export need GNU tar for a deterministic archive; `rust/package.sh` already requires it"
                .to_string()
        })
    }
}

fn capture(program: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|error| format!("could not run {program}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "{program} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout).map_err(|_| format!("{program} wrote non-UTF-8 output"))
}

struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn make_dir(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path)
        .map_err(|error| format!("could not make {}: {error}", path.display()))?;
    set_mode(path, 0o755)
}

fn set_mode(path: &Path, mode: u32) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .map_err(|error| format!("could not set mode on {}: {error}", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

fn make_every_directory_0755(root: &Path) -> Result<(), String> {
    let mut entries = Vec::new();
    for entry in
        fs::read_dir(root).map_err(|error| format!("could not read {}: {error}", root.display()))?
    {
        let entry = entry.map_err(|error| format!("could not read {}: {error}", root.display()))?;
        entries.push(entry.path());
    }
    entries.sort();
    for path in entries {
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("could not stat {}: {error}", path.display()))?;
        if metadata.is_dir() {
            set_mode(&path, 0o755)?;
            make_every_directory_0755(&path)?;
        }
    }
    Ok(())
}

pub(crate) fn copy_file(from: &Path, to: &Path) -> Result<(), String> {
    if let Some(parent) = to.parent() {
        make_dir(parent)?;
    }
    let metadata = fs::symlink_metadata(from)
        .map_err(|error| format!("could not stat {}: {error}", from.display()))?;
    if !metadata.is_file() {
        return Err(format!("`{}` is not a regular file", from.display()));
    }
    fs::copy(from, to).map_err(|error| format!("could not copy {}: {error}", from.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode() & 0o777;
        set_mode(to, mode)?;
    }
    Ok(())
}

pub(crate) fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    make_dir(to)?;
    let mut entries = Vec::new();
    for entry in
        fs::read_dir(from).map_err(|error| format!("could not read {}: {error}", from.display()))?
    {
        let entry = entry.map_err(|error| format!("could not read {}: {error}", from.display()))?;
        entries.push(entry.path());
    }
    entries.sort();
    for path in entries {
        let relative = path
            .strip_prefix(from)
            .map_err(|_| format!("{} is not under {}", path.display(), from.display()))?;
        let target = to.join(relative);
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("could not stat {}: {error}", path.display()))?;
        if metadata.is_dir() {
            copy_tree(&path, &target)?;
        } else if metadata.is_file() {
            copy_file(&path, &target)?;
        } else {
            return Err(format!(
                "`{}` is a link or special file; packs carry regular files only",
                path.display()
            ));
        }
    }
    Ok(())
}

pub(crate) fn walk_regular(root: &Path) -> Result<Vec<(String, PathBuf, u64)>, String> {
    let mut found = Vec::new();
    walk(root, root, &mut found)?;
    found.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(found)
}

fn walk(root: &Path, at: &Path, out: &mut Vec<(String, PathBuf, u64)>) -> Result<(), String> {
    let mut entries = Vec::new();
    for entry in
        fs::read_dir(at).map_err(|error| format!("could not read {}: {error}", at.display()))?
    {
        let entry = entry.map_err(|error| format!("could not read {}: {error}", at.display()))?;
        entries.push(entry.path());
    }
    entries.sort();
    for path in entries {
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("could not stat {}: {error}", path.display()))?;
        if metadata.is_dir() {
            walk(root, &path, out)?;
        } else if metadata.is_file() {
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            out.push((relative, path.clone(), metadata.len()));
        } else {
            return Err(format!(
                "`{}` is a link or special file; packs carry regular files only",
                path.display()
            ));
        }
    }
    Ok(())
}

// ── plan ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Row {
    pub class: String,
    pub kind: Kind,
    pub identity: String,
    pub digest: String,
    pub score: f64,
    pub shared: Vec<String>,
    pub default_action: String,
    pub reason: String,
}

impl Row {
    pub fn to_value(&self) -> Value {
        json!({
            "class": self.class,
            "kind": self.kind.name(),
            "identity": self.identity,
            "digest": self.digest,
            "score": self.score,
            "shared": self.shared,
            "default": self.default_action,
            "reason": self.reason,
        })
    }
}

struct CatalogItem {
    kind: Kind,
    identity: String,
    digest: String,
    text: String,
}

struct PackItem {
    kind: Kind,
    identity: String,
    digest: String,
    text: String,
}

pub fn plan(pack: &Pack, state: &Path) -> Result<Vec<Row>, String> {
    let mut catalog = catalog(state)?;
    let mut pack_items = pack_items(pack)?;
    catalog.sort_by(|a, b| {
        a.kind
            .rank()
            .cmp(&b.kind.rank())
            .then_with(|| a.identity.cmp(&b.identity))
    });
    pack_items.sort_by(|a, b| {
        a.kind
            .rank()
            .cmp(&b.kind.rank())
            .then_with(|| a.identity.cmp(&b.identity))
    });
    let current = env!("CARGO_PKG_VERSION");
    let mut rows = Vec::new();
    for item in &pack_items {
        let mut incompatibility = None;
        if !range_ok(&pack.manifest.requires.hades, current)? {
            incompatibility = Some(format!(
                "requires hades {}; this is {current}",
                pack.manifest.requires.hades
            ));
        }
        for role in &pack.manifest.requires.roles {
            if !KNOWN_ROLES.contains(&role.as_str()) {
                incompatibility = Some(format!("requires unknown role `{role}`"));
            }
        }
        for tool in &pack.manifest.requires.tools {
            if !known_tool(tool.as_str()) {
                incompatibility = Some(format!("requires unknown tool `{tool}`"));
            }
        }
        if let Some(reason) = incompatibility {
            rows.push(Row {
                class: "incompatible".to_string(),
                kind: item.kind,
                identity: item.identity.clone(),
                digest: item.digest.clone(),
                score: 0.0,
                shared: Vec::new(),
                default_action: "refuse".to_string(),
                reason,
            });
            continue;
        }
        let same: Vec<&CatalogItem> = catalog
            .iter()
            .filter(|known| known.kind == item.kind && known.identity == item.identity)
            .collect();
        if let Some(known) = same.first() {
            // The installed copy of a skill carries the installer's own frontmatter (status, defaults), so its
            // digest can never equal the archive's. The catalog is the record of what was installed and from
            // which archive, so "already installed, and still as installed" is asked of it rather than guessed
            // from the text. Without this, installing the same pack twice was a conflict and `pack add` could
            // not be re-run.
            let recorded = crate::pack_fit::read_catalog(state).unwrap_or_default();
            let from_this_archive = recorded.iter().any(|row| {
                row.kind == item.kind
                    && row.identity == item.identity
                    && row.pack_digest == pack.digest
                    && row.live_digest == known.digest
            });
            let class = if known.digest == item.digest || from_this_archive {
                "duplicate"
            } else {
                "conflict"
            };
            rows.push(Row {
                class: class.to_string(),
                kind: item.kind,
                identity: item.identity.clone(),
                digest: item.digest.clone(),
                score: 0.0,
                shared: Vec::new(),
                default_action: if class == "duplicate" { "skip" } else { "ask" }.to_string(),
                reason: if class == "duplicate" {
                    "same identity and digest".to_string()
                } else {
                    "same identity, different digest".to_string()
                },
            });
            continue;
        }
        let mut best: Option<(f64, Vec<String>, &CatalogItem)> = None;
        for known in catalog.iter().filter(|known| known.kind == item.kind) {
            let (score, shared) = similarity(&item.text, &known.text);
            if best.as_ref().is_none_or(|(best, _, _)| score > *best) {
                best = Some((score, shared, known));
            }
        }
        if let Some((score, shared, known)) = best
            && score >= 0.60
        {
            let note = if score >= 0.85 {
                " (near-duplicate)"
            } else {
                ""
            };
            rows.push(Row {
                class: "redundant".to_string(),
                kind: item.kind,
                identity: item.identity.clone(),
                digest: item.digest.clone(),
                score: round3(score),
                shared,
                default_action: "review".to_string(),
                reason: format!("similar to `{}` at {score:.3}{note}", known.identity),
            });
            continue;
        }
        rows.push(Row {
            class: "new".to_string(),
            kind: item.kind,
            identity: item.identity.clone(),
            digest: item.digest.clone(),
            score: 0.0,
            shared: Vec::new(),
            default_action: "install".to_string(),
            reason: "no item with this identity".to_string(),
        });
    }
    Ok(rows)
}

fn catalog(state: &Path) -> Result<Vec<CatalogItem>, String> {
    let mut items = Vec::new();
    for skill in skills::installed(state) {
        let (text, digest) =
            if skill.path.file_name().and_then(|name| name.to_str()) == Some("SKILL.md") {
                let root = skill.path.parent().unwrap_or(&skill.path);
                (tree_text(root)?, digest_tree(root)?)
            } else {
                (skill_text(&skill.path)?, digest_file(&skill.path)?)
            };
        items.push(CatalogItem {
            kind: Kind::Skill,
            identity: skill.name,
            digest,
            text,
        });
    }
    let harness_root = state.join("harness");
    if let Ok(listing) = fs::read_dir(&harness_root) {
        let mut dirs: Vec<PathBuf> = listing
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        dirs.sort();
        for dir in dirs {
            let Some(name) = dir.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let files = Files::load(Some(&dir.to_string_lossy()), name)?;
            let text = format!("{}\n{}", files.system, files.guidance);
            items.push(CatalogItem {
                kind: Kind::Harness,
                identity: name.to_string(),
                digest: digest_tree(&dir)?,
                text,
            });
        }
    }
    for profile in cases::profiles(state) {
        for case in cases::load(state, &profile) {
            let value = case.to_value();
            let text = format!("{}\n{}", case.goal, case.check);
            items.push(CatalogItem {
                kind: Kind::Case,
                identity: case.id.clone(),
                digest: digest_case(&value, &case.tree),
                text,
            });
        }
    }
    Ok(items)
}

fn pack_items(pack: &Pack) -> Result<Vec<PackItem>, String> {
    let mut items = Vec::new();
    for provided in pack.manifest.provides.all() {
        let path = pack.root.join(&provided.path);
        match provided.kind {
            Kind::Skill => items.push(PackItem {
                kind: Kind::Skill,
                identity: provided.identity.clone(),
                digest: digest_tree(&path)?,
                text: tree_text(&path)?,
            }),
            Kind::Harness => {
                let mut text = fs::read_to_string(path.join("system.md"))
                    .map_err(|error| format!("could not read harness: {error}"))?;
                if let Ok(guidance) = fs::read_to_string(path.join("tool_guidance.md")) {
                    text.push('\n');
                    text.push_str(&guidance);
                }
                items.push(PackItem {
                    kind: Kind::Harness,
                    identity: provided.identity.clone(),
                    digest: digest_tree(&path)?,
                    text,
                });
            }
            Kind::Case => {
                let case_text = fs::read_to_string(path.join("case.json"))
                    .map_err(|error| format!("could not read case.json: {error}"))?;
                let value: Value = serde_json::from_str(&case_text)
                    .map_err(|error| format!("case.json is not JSON: {error}"))?;
                let case = Case::from_value(&value).ok_or_else(|| {
                    format!("case `{}` has no readable fields", provided.identity)
                })?;
                if provided.identity != case.id {
                    return Err(format!(
                        "case `{}` carries id `{}` in case.json; the manifest and the case must agree",
                        provided.identity, case.id
                    ));
                }
                if !cases::valid_id(&case.id) {
                    return Err(format!("`{}` is not a case id", case.id));
                }
                let expected = cases::case_id(&case.profile, &case.goal, &case.check, &case.tree);
                if case.id != expected {
                    return Err(format!(
                        "case `{}` is not the id of its profile, goal, check and tree (expected {expected})",
                        case.id
                    ));
                }
                let tree = path.join("tree");
                let tree_digest = cases::tree_digest(&tree)?;
                if tree_digest != case.tree {
                    return Err(format!(
                        "case `{}` tree changed: recorded {}, found {tree_digest}",
                        provided.identity, case.tree
                    ));
                }
                items.push(PackItem {
                    kind: Kind::Case,
                    identity: provided.identity.clone(),
                    digest: digest_case(&value, &tree_digest),
                    text: format!("{}\n{}", case.goal, case.check),
                });
            }
        }
    }
    Ok(items)
}

fn skill_text(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|error| format!("could not read {}: {error}", path.display()))
}

fn tree_text(root: &Path) -> Result<String, String> {
    let mut text = String::new();
    for (relative, file, _) in walk_regular(root)? {
        let body = fs::read_to_string(&file)
            .map_err(|error| format!("could not read {}: {error}", file.display()))?;
        text.push_str(&relative);
        text.push('\n');
        text.push_str(&body);
        text.push('\n');
    }
    Ok(text)
}

fn digest_file(path: &Path) -> Result<String, String> {
    let bytes =
        fs::read(path).map_err(|error| format!("could not read {}: {error}", path.display()))?;
    Ok(mneme::sha256::hex(&bytes))
}

pub(crate) fn digest_tree(root: &Path) -> Result<String, String> {
    let files = walk_regular(root)?;
    let mut text = String::new();
    for (relative, path, _) in files {
        let bytes = fs::read(&path)
            .map_err(|error| format!("could not read {}: {error}", path.display()))?;
        text.push_str(&relative);
        text.push('\0');
        text.push_str(&mneme::sha256::hex(&bytes));
        text.push('\n');
    }
    Ok(mneme::sha256::hex(text.as_bytes()))
}

fn digest_case(value: &Value, tree: &str) -> String {
    let text = format!("{}\n{tree}", value);
    mneme::sha256::hex(text.as_bytes())
}

fn round3(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

fn similarity(left: &str, right: &str) -> (f64, Vec<String>) {
    let left = shingles(left);
    let right = shingles(right);
    if left.is_empty() || right.is_empty() {
        return (0.0, Vec::new());
    }
    let shared: Vec<String> = left.intersection(&right).cloned().collect();
    let union = left.union(&right).count();
    let score = if union == 0 {
        0.0
    } else {
        shared.len() as f64 / union as f64
    };
    let mut shown = shared;
    shown.truncate(3);
    (score, shown)
}

fn shingles(text: &str) -> BTreeSet<String> {
    let normalized = text
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { ' ' })
        .collect::<String>();
    let tokens: Vec<&str> = normalized.split_whitespace().collect();
    if tokens.is_empty() {
        return BTreeSet::new();
    }
    if tokens.len() < 5 {
        return BTreeSet::from([tokens.join(" ")]);
    }
    tokens.windows(5).map(|window| window.join(" ")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_version_range_accepts_and_refuses_the_expected_builds() {
        assert!(range_ok(">=0.0.19,<0.2", "0.0.19").expect("range"));
        assert!(!range_ok(">=0.0.19,<0.2", "0.2.0").expect("range"));
        assert!(!range_ok(">=9", "0.0.19").expect("range"));
        assert!(version_range(">=0.0.19,").is_err());
        assert!(version_range(">=x").is_err());
    }

    #[test]
    fn the_range_an_export_declares_admits_the_build_that_wrote_it() {
        let current = env!("CARGO_PKG_VERSION");
        let range = default_range();
        assert!(
            range_ok(&range, current).expect("range"),
            "a pack exported from {current} declares {range}, which must admit {current}"
        );
        // The next minor line is outside it, which is what the upper bound is for.
        let next = default_range_next_minor_for_test();
        assert!(!range_ok(&range, &next).expect("range"));
    }

    fn default_range_next_minor_for_test() -> String {
        let current = env!("CARGO_PKG_VERSION");
        let mut parts = current.split('.');
        let major = parts.next().unwrap_or("0").to_string();
        let minor = parts.next().unwrap_or("0").parse::<u64>().unwrap_or(0);
        format!("{major}.{}.0", minor + 1)
    }

    #[test]
    fn an_assumption_without_a_severity_is_refused_by_name() {
        let value: Value =
            serde_json::from_str(r#"[{"id":"x","claim":"something","check":"gpu_present"}]"#)
                .expect("json");
        let error = parse_assumptions(&value).expect_err("no severity");
        assert!(error.contains("severity"), "{error}");
    }

    #[test]
    fn an_assumption_carries_its_check_argument_and_remedy() {
        let value: Value = serde_json::from_str(
            r#"[{"id":"vram","claim":"8 GiB","check":"vram_at_least","arg":8,
                 "severity":"hard","resolution":"recompute the offload"}]"#,
        )
        .expect("json");
        let assumptions = parse_assumptions(&value).expect("assumptions");
        assert_eq!(assumptions.len(), 1);
        assert_eq!(assumptions[0].arg.as_deref(), Some("8"));
        assert_eq!(assumptions[0].severity, Severity::Hard);
        assert_eq!(
            assumptions[0].resolution.as_deref(),
            Some("recompute the offload")
        );
    }

    #[test]
    fn a_listing_rejects_an_escape_a_link_and_a_bare_root() {
        assert!(validate_listing("./\n./pack.json\n", "d\n-\n").is_ok());
        assert!(validate_listing("../escape\n", "-\n").is_err());
        assert!(validate_listing("files/./escape\n", "-\n").is_err());
        assert!(validate_listing("files/link\n", "l\n").is_err());
        assert!(validate_listing("files/\n", "d\n").is_ok());
    }

    #[test]
    fn every_tool_the_runtime_ships_is_a_tool_a_pack_may_require() {
        // `browser` was rejected by a runtime that ships it, because this list was a second copy of the tool
        // registry. The copy is gone; this test is what keeps the two from drifting apart again.
        for tool in crate::agent::TOOLS {
            assert!(
                known_tool(tool),
                "`{tool}` is a tool this build ships and a pack cannot require it"
            );
        }
        assert!(known_tool(crate::VERIFY));
        assert!(!known_tool("teleport"));
    }

    #[test]
    fn a_manifest_accepts_a_harness_profile_key() {
        let text = r#"{
            "format": 1,
            "id": "demo/one",
            "version": "1.0.0",
            "title": "Demo",
            "summary": "",
            "license": "MIT",
            "derivatives": "forbidden",
            "requires": {"hades": ">=0.0.19,<0.2", "roles": [], "tools": []},
            "provides": {
                "skills": [],
                "harnesses": [{"profile": "research", "path": "files/harness/research"}],
                "cases": []
            },
            "capabilities": {"network": false, "writes": [], "code": false}
        }"#;
        let manifest = Manifest::parse(text).expect("manifest");
        assert_eq!(manifest.provides.harnesses[0].identity, "research");
    }

    #[test]
    fn a_case_json_keeps_budget_tokens() {
        let value = json!({
            "kind": "case",
            "id": "research-abc123def456",
            "goal": "Find the value",
            "check": "test -f run.sh",
            "profile": "research",
            "bounds": {"attempts": 2, "minutes": 5, "budget_tokens": 123, "width": 1},
            "tree": "abc"
        });
        let case = Case::from_value(&value).expect("case");
        assert_eq!(case.bounds.tokens, Some(123));
        assert_eq!(case.to_value()["bounds"]["budget_tokens"], 123);
        assert_eq!(
            Case::from_value(&case.to_value()).expect("round trip"),
            case
        );
    }

    #[test]
    fn similar_text_scores_high_and_unrelated_text_scores_low() {
        let a = (0..80)
            .map(|index| format!("token{index}"))
            .collect::<Vec<_>>()
            .join(" ");
        let b = a.replacen("token40", "changed40", 1);
        let (high, shared) = similarity(&a, &b);
        assert!(high > 0.85, "{high}");
        assert!(!shared.is_empty());
        let (low, _) = similarity(&a, "a recipe for bread with flour water and salt");
        assert!(low < 0.60, "{low}");
    }

    #[test]
    fn keywords_are_optional_and_survive_a_round_trip() {
        let bare = r#"{"format":1,"id":"demo","version":"1.0.0","title":"Demo","summary":"s",
            "license":"MIT","derivatives":"allowed",
            "requires":{"hades":">=0.1.0","roles":[],"tools":[]},
            "provides":{"skills":[],"harnesses":[],"cases":[]},
            "capabilities":{"network":false,"writes":[],"code":false}}"#;
        assert!(
            Manifest::parse(bare)
                .expect("a pack without keywords")
                .keywords
                .is_empty()
        );

        let tagged = bare.replace(
            r#""capabilities""#,
            r#""keywords":["git","review"],"capabilities""#,
        );
        let manifest = Manifest::parse(&tagged).expect("a pack with keywords");
        assert_eq!(manifest.keywords, vec!["git", "review"]);
        let again = Manifest::parse(&manifest.to_value().to_string()).expect("its own output");
        assert_eq!(again.keywords, vec!["git", "review"]);
    }

    #[test]
    fn a_manifest_refuses_unknown_keys_and_code() {
        let valid = r#"{"format":1,"id":"a/b","version":"1.0.0","title":"t","summary":"","license":"MIT","derivatives":"forbidden","requires":{"hades":">=0.0.19,<0.2","roles":[],"tools":[]},"provides":{"skills":[],"harnesses":[],"cases":[]},"capabilities":{"network":false,"writes":[],"code":false}}"#;
        assert!(Manifest::parse(valid).is_ok());
        assert!(
            Manifest::parse(&valid.replace("\"format\":1", "\"format\":1,\"extra\":1")).is_err()
        );
        assert!(Manifest::parse(&valid.replace("\"code\":false", "\"code\":true")).is_err());
        assert!(Manifest::parse(&valid.replace(">=0.0.19,<0.2", ">=0.0.19,")).is_err());
    }
}
