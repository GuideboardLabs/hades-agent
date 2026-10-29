//! Connector packs: a binary a surface ships, installed outside the base.
//!
//! A content pack is prompts, skills and cases; this is the other kind. The rules are stricter where it
//! matters and looser where it does not: the manifest is small and closed, every file in the tree is
//! hashed, nothing is executed before the digest has been shown and approved, and the installed binary is
//! hashed again immediately before it runs.
//!
//! The install root is state-relative (`<state>/connectors/<id>/<version>`), like every other artifact in
//! this project, so `--state` still isolates a test or a second instance.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

/// Manifest must stay small: a big one is a parsing surface and a place to hide a field.
const MAX_MANIFEST: usize = 64 * 1024;
const MAX_FILES: usize = 1024;
const MAX_EXPANDED: u64 = 192 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub format: u32,
    pub id: String,
    pub version: String,
    pub title: String,
    pub summary: String,
    pub license: String,
    pub protocol_min: u32,
    pub protocol_max: u32,
    pub requires_hades: Option<String>,
    pub platform: String,
    pub entrypoint: String,
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Manifest, String> {
        if text.len() > MAX_MANIFEST {
            return Err(format!(
                "connector.json is larger than {MAX_MANIFEST} bytes"
            ));
        }
        let value: Value = serde_json::from_str(text)
            .map_err(|error| format!("connector.json is not JSON: {error}"))?;
        let root = value
            .as_object()
            .ok_or_else(|| "connector.json is not an object".to_string())?;
        let allowed = [
            "format",
            "kind",
            "id",
            "version",
            "title",
            "summary",
            "license",
            "protocol",
            "requires_hades",
            "platform",
            "entrypoint",
            "config_schema",
            "signature",
            // The pack's own words and its own working animation. Optional, because a connector
            // that has nothing to say may ship neither, but a pack that carries them is still a
            // pack: they are content the surface installs, not code the harness runs.
            "prompt",
            "loading",
        ];
        for key in root.keys() {
            if !allowed.contains(&key.as_str()) {
                return Err(format!("connector.json has an unknown key `{key}`"));
            }
        }
        let text_of = |key: &str| {
            root.get(key)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| format!("connector.json needs a string `{key}`"))
        };
        let format = root
            .get("format")
            .and_then(Value::as_u64)
            .ok_or_else(|| "connector.json needs a numeric `format`".to_string())?;
        if format != 1 {
            return Err(format!("connector.json format {format} is not 1"));
        }
        let kind = text_of("kind")?;
        if kind != "connector" {
            return Err(format!(
                "connector.json kind is {kind:?}; this is not a connector"
            ));
        }
        let id = text_of("id")?;
        crate::pack::valid_id(&id)?;
        let version = text_of("version")?;
        crate::pack::valid_version(&version)?;
        let protocol = root
            .get("protocol")
            .and_then(Value::as_object)
            .ok_or_else(|| "connector.json needs a `protocol` object".to_string())?;
        let min = protocol
            .get("min")
            .and_then(Value::as_u64)
            .ok_or_else(|| "`protocol.min` must be an integer".to_string())?
            as u32;
        let max = protocol
            .get("max")
            .and_then(Value::as_u64)
            .ok_or_else(|| "`protocol.max` must be an integer".to_string())?
            as u32;
        if min > max {
            return Err(format!(
                "`protocol.min` {min} is above `protocol.max` {max}"
            ));
        }
        let requires_hades = match root.get("requires_hades") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                value
                    .as_object()
                    .and_then(|range| range.get("min"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| "`requires_hades` must be an object with `min`".to_string())?
                    .to_string(),
            ),
        };
        let platform = text_of("platform")?;
        if !platform.starts_with("linux") {
            return Err(format!(
                "connector platform {platform:?} is not supported here; this build runs {}",
                current_platform()
            ));
        }
        let entrypoint = text_of("entrypoint")?;
        valid_entrypoint(&entrypoint)?;
        Ok(Manifest {
            format: format as u32,
            id,
            version,
            title: text_of("title")?,
            summary: text_of("summary")?,
            license: text_of("license")?,
            protocol_min: min,
            protocol_max: max,
            requires_hades,
            platform,
            entrypoint,
        })
    }

    pub fn to_value(&self) -> Value {
        json!({
            "format": self.format,
            "kind": "connector",
            "id": self.id,
            "version": self.version,
            "title": self.title,
            "summary": self.summary,
            "license": self.license,
            "protocol": {"min": self.protocol_min, "max": self.protocol_max},
            "requires_hades": self.requires_hades.as_ref().map(|min| json!({"min": min})),
            "platform": self.platform,
            "entrypoint": self.entrypoint,
        })
    }
}

/// The platform this build can run. A connector that names another one is refused at install rather than
/// at spawn, where the error is a kernel message instead of a sentence.
pub fn current_platform() -> String {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        other => other,
    };
    format!("linux-{arch}")
}

/// `bin/name`, one segment, nothing that can climb out. A prefix check is not enough: `bin/../x` starts
/// with `bin/`.
fn valid_entrypoint(path: &str) -> Result<(), String> {
    if path.contains('\\') || path.starts_with('/') {
        return Err(format!("entrypoint {path:?} is not a relative path"));
    }
    let parts: Vec<&str> = path.split('/').collect();
    for part in &parts {
        if part.is_empty() || *part == "." || *part == ".." {
            return Err(format!(
                "entrypoint {path:?} has a path segment that climbs"
            ));
        }
    }
    if parts.len() != 2 || parts[0] != "bin" {
        return Err(format!("entrypoint {path:?} must be `bin/<name>`"));
    }
    Ok(())
}

fn digest_of(path: &Path) -> Result<String, String> {
    use std::io::Read;
    let mut handle = std::fs::File::open(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let mut digest = mneme::sha256::Digest::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = handle
            .read(&mut buffer)
            .map_err(|error| format!("could not read {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(digest.finish())
}

/// Where a connector lives. State-relative, like packs and sessions, so `--state` still isolates.
pub fn root(state: &Path) -> PathBuf {
    state.join("connectors")
}

pub fn installed(state: &Path, id: &str, version: &str) -> PathBuf {
    root(state).join(id).join(version)
}

fn index_path(state: &Path) -> PathBuf {
    root(state).join("index.json")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub id: String,
    pub version: String,
    pub title: String,
    pub platform: String,
    /// The digest of the entrypoint at install time. `resolve` compares it with the file it is about to
    /// run, so a tree edited after the install is not executed.
    pub live_digest: String,
}

impl Row {
    fn to_value(&self) -> Value {
        json!({
            "id": self.id,
            "version": self.version,
            "title": self.title,
            "platform": self.platform,
            "live_digest": self.live_digest,
        })
    }

    fn from_value(value: &Value) -> Option<Row> {
        Some(Row {
            id: value.get("id")?.as_str()?.to_string(),
            version: value.get("version")?.as_str()?.to_string(),
            title: value.get("title")?.as_str()?.to_string(),
            platform: value.get("platform")?.as_str()?.to_string(),
            live_digest: value.get("live_digest")?.as_str()?.to_string(),
        })
    }
}

pub fn rows(state: &Path) -> Vec<Row> {
    let Ok(text) = std::fs::read_to_string(index_path(state)) else {
        return Vec::new();
    };
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    items.iter().filter_map(Row::from_value).collect()
}

fn write_rows(state: &Path, rows: &[Row]) -> Result<(), String> {
    let path = index_path(state);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    let items: Vec<Value> = rows.iter().map(Row::to_value).collect();
    std::fs::write(&path, Value::Array(items).to_string())
        .map_err(|error| format!("{}: {error}", path.display()))
}

/// Install a prepared directory. Every file except `SHA256SUMS` must be listed there and match; the
/// entrypoint must be the file the manifest names. Nothing in the tree may be a symlink.
pub fn install_dir(state: &Path, source: &Path, manifest: &Manifest) -> Result<PathBuf, String> {
    let staging = root(state).join(format!(
        ".staging-{}-{}",
        manifest.id,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&staging).map_err(|error| format!("{}: {error}", staging.display()))?;
    copy_tree(source, &staging)?;
    let result = validate_tree(&staging, manifest).and_then(|digest| {
        let target = installed(state, &manifest.id, &manifest.version);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("{}: {error}", parent.display()))?;
        }
        std::fs::rename(&staging, &target)
            .map_err(|error| format!("{}: {error}", target.display()))?;
        let mut all = rows(state);
        all.retain(|row| row.id != manifest.id);
        all.push(Row {
            id: manifest.id.clone(),
            version: manifest.version.clone(),
            title: manifest.title.clone(),
            platform: manifest.platform.clone(),
            live_digest: digest,
        });
        write_rows(state, &all)?;
        Ok(target)
    });
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    result
}

fn copy_tree(source: &Path, target: &Path) -> Result<(), String> {
    let entries =
        std::fs::read_dir(source).map_err(|error| format!("{}: {error}", source.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        let path = entry.path();
        let name = entry.file_name();
        let to = target.join(&name);
        let meta = std::fs::symlink_metadata(&path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        if meta.file_type().is_symlink() {
            return Err(format!(
                "{} is a symlink; a connector tree may not carry one",
                path.display()
            ));
        }
        if meta.is_dir() {
            std::fs::create_dir_all(&to).map_err(|error| format!("{}: {error}", to.display()))?;
            copy_tree(&path, &to)?;
        } else if meta.is_file() {
            std::fs::copy(&path, &to).map_err(|error| format!("{}: {error}", to.display()))?;
        } else {
            return Err(format!(
                "{} is not a regular file or directory",
                path.display()
            ));
        }
    }
    Ok(())
}

/// Walk the staged tree, hash every file, and compare it with `SHA256SUMS`. Returns the entrypoint digest.
fn validate_tree(staging: &Path, manifest: &Manifest) -> Result<String, String> {
    let sums_path = staging.join("SHA256SUMS");
    let sums = std::fs::read_to_string(&sums_path).map_err(|error| {
        format!(
            "{}: {error} (a connector pack needs one)",
            sums_path.display()
        )
    })?;
    let mut listed: BTreeMap<String, String> = BTreeMap::new();
    for line in sums.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (digest, name) = line
            .split_once("  ")
            .or_else(|| line.split_once(' '))
            .ok_or_else(|| format!("SHA256SUMS line does not read: {line:?}"))?;
        if digest.len() != 64 || !digest.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!(
                "SHA256SUMS has a digest that is not sha256: {digest:?}"
            ));
        }
        let name = name.trim_start_matches("./");
        if name.starts_with('/') || name.contains("..") {
            return Err(format!(
                "SHA256SUMS names a path outside the pack: {name:?}"
            ));
        }
        if listed
            .insert(name.to_string(), digest.to_string())
            .is_some()
        {
            return Err(format!("SHA256SUMS lists {name} twice"));
        }
    }
    let mut seen = 0usize;
    let mut entry_digest = None;
    let mut stack = vec![staging.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).map_err(|error| error.to_string())? {
            let entry = entry.map_err(|error| error.to_string())?;
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
            if meta.file_type().is_symlink() {
                return Err(format!("{} is a symlink; refused", path.display()));
            }
            if meta.is_dir() {
                stack.push(path);
                continue;
            }
            if !meta.is_file() {
                return Err(format!("{} is not a regular file", path.display()));
            }
            let name = path
                .strip_prefix(staging)
                .map_err(|_| "a path outside the staging tree".to_string())?
                .to_string_lossy()
                .to_string();
            if name == "SHA256SUMS" {
                continue;
            }
            seen += 1;
            if seen > MAX_FILES {
                return Err(format!(
                    "a connector pack may hold at most {MAX_FILES} files"
                ));
            }
            if meta.len() > MAX_EXPANDED {
                return Err(format!("{} is larger than the pack cap", name));
            }
            let digest = digest_of(&path)?;
            let Some(want) = listed.remove(&name) else {
                return Err(format!("{name} is in the pack but not in SHA256SUMS"));
            };
            if want != digest {
                return Err(format!("{name} does not match its SHA256SUMS entry"));
            }
            if name == manifest.entrypoint {
                entry_digest = Some(digest);
            }
        }
    }
    if let Some(name) = listed.keys().next() {
        return Err(format!("SHA256SUMS names {name}, which is not in the pack"));
    }
    let entry = staging.join(&manifest.entrypoint);
    let meta = std::fs::symlink_metadata(&entry)
        .map_err(|_| format!("the entrypoint {} is missing", manifest.entrypoint))?;
    if !meta.is_file() {
        return Err(format!(
            "the entrypoint {} is not a regular file",
            manifest.entrypoint
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&entry, std::fs::Permissions::from_mode(0o755))
            .map_err(|error| format!("{}: {error}", entry.display()))?;
    }
    entry_digest.ok_or_else(|| {
        format!(
            "the entrypoint {} is not in SHA256SUMS",
            manifest.entrypoint
        )
    })
}

/// Unpack a `.tar.gz` into staging, with a listing pass that refuses anything that could climb or link
/// out, then hand the tree to [`install_dir`]. The listing pass is the guard; the hash walk is the proof.
pub fn install_tar(state: &Path, archive: &Path) -> Result<(PathBuf, Manifest), String> {
    let listing = Command::new("tar")
        .args(["-tzf", &archive.display().to_string()])
        .output()
        .map_err(|error| format!("could not run tar: {error}"))?;
    if !listing.status.success() {
        return Err(String::from_utf8_lossy(&listing.stderr).trim().to_string());
    }
    let names = String::from_utf8_lossy(&listing.stdout);
    if names.trim().is_empty() {
        return Err("the archive is empty".to_string());
    }
    for name in names.lines() {
        let name = name.trim();
        if name.is_empty() || name.starts_with('/') || name.contains("..") || name.contains('\\') {
            return Err(format!("the archive names {name:?}, which is refused"));
        }
    }
    let staging = root(state).join(format!(
        ".unpack-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&staging).map_err(|error| error.to_string())?;
    let status = Command::new("tar")
        .args([
            "-xzf",
            &archive.display().to_string(),
            "-C",
            &staging.display().to_string(),
            "--no-same-owner",
            "--no-same-permissions",
        ])
        .status()
        .map_err(|error| format!("could not run tar: {error}"))?;
    if !status.success() {
        let _ = std::fs::remove_dir_all(&staging);
        return Err("tar could not unpack the connector".to_string());
    }
    // A pack may wrap everything in one directory; that is the tree if it holds connector.json.
    let mut source = staging.clone();
    if !source.join("connector.json").exists()
        && let Ok(entries) = std::fs::read_dir(&source)
    {
        let dirs: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        if dirs.len() == 1 && dirs[0].join("connector.json").exists() {
            source = dirs[0].clone();
        }
    }
    let text = std::fs::read_to_string(source.join("connector.json"))
        .map_err(|error| format!("connector.json: {error}"))?;
    let manifest = Manifest::parse(&text)?;
    let result = install_dir(state, &source, &manifest);
    let _ = std::fs::remove_dir_all(&staging);
    result.map(|path| (path, manifest))
}

/// Whether an archive is a connector pack, decided by looking, never by asking the caller. `pack add`
/// takes both kinds and the file itself says which one it is.
pub fn looks_like_connector(archive: &Path) -> bool {
    Command::new("tar")
        .args(["-tzf", &archive.display().to_string()])
        .output()
        .map(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .any(|line| line.trim_end_matches('/').ends_with("connector.json"))
        })
        .unwrap_or(false)
}

/// The digest of one file, for a caller that pinned a value and wants to compare.
pub fn digest_for(path: &Path) -> Result<String, String> {
    digest_of(path)
}

/// The installed binary for an id, after checking that the file still hashes to what was approved.
pub fn resolve(state: &Path, id: &str) -> Result<(Manifest, PathBuf), String> {
    let rows = rows(state);
    let row = rows
        .iter()
        .find(|row| row.id == id)
        .ok_or_else(|| format!("no connector `{id}` is installed"))?;
    let dir = installed(state, &row.id, &row.version);
    let text = std::fs::read_to_string(dir.join("connector.json"))
        .map_err(|error| format!("{}: {error}", dir.display()))?;
    let manifest = Manifest::parse(&text)?;
    let binary = dir.join(&manifest.entrypoint);
    let digest = digest_of(&binary)?;
    if digest != row.live_digest {
        return Err(format!(
            "{id} {version} does not match the digest it was installed with; refusing to run it",
            version = row.version
        ));
    }
    Ok((manifest, binary))
}

pub fn remove(state: &Path, id: &str) -> Result<usize, String> {
    let before = rows(state);
    let mut kept = before.clone();
    kept.retain(|row| row.id != id);
    if kept.len() == before.len() {
        return Err(format!("no connector `{id}` is installed"));
    }
    let dir = root(state).join(id);
    let removed = std::fs::remove_dir_all(&dir).is_ok();
    write_rows(state, &kept)?;
    Ok(if removed { 1 } else { 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hades-connector-{name}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("a scratch dir");
        dir
    }

    fn manifest(id: &str) -> Manifest {
        Manifest {
            format: 1,
            id: id.to_string(),
            version: "0.1.0".to_string(),
            title: "Test".to_string(),
            summary: "A test connector".to_string(),
            license: "MIT".to_string(),
            protocol_min: 1,
            protocol_max: 1,
            requires_hades: None,
            platform: current_platform(),
            entrypoint: "bin/test".to_string(),
        }
    }

    /// A tree with a binary-looking entrypoint and a correct SHA256SUMS.
    fn pack(name: &str) -> (PathBuf, Manifest) {
        let dir = scratch(name);
        std::fs::create_dir_all(dir.join("bin")).expect("bin");
        std::fs::write(dir.join("bin/test"), "#!/bin/sh\necho ok\n").expect("entry");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.join("bin/test"), std::fs::Permissions::from_mode(0o755))
                .expect("mode");
        }
        std::fs::write(dir.join("README.md"), "read me\n").expect("readme");
        let manifest = manifest("test");
        let text = std::fs::read_to_string(dir.join("bin/test")).expect("entry");
        let digest = {
            let mut d = mneme::sha256::Digest::new();
            d.update(text.as_bytes());
            d.finish()
        };
        let readme = {
            let mut d = mneme::sha256::Digest::new();
            d.update(b"read me\n");
            d.finish()
        };
        let manifest_text = manifest.to_value().to_string();
        let manifest_digest = {
            let mut d = mneme::sha256::Digest::new();
            d.update(manifest_text.as_bytes());
            d.finish()
        };
        std::fs::write(
            dir.join("SHA256SUMS"),
            format!("{digest}  bin/test\n{readme}  README.md\n{manifest_digest}  connector.json\n"),
        )
        .expect("sums");
        std::fs::write(dir.join("connector.json"), manifest_text).expect("manifest");
        (dir, manifest)
    }

    #[test]
    fn a_good_tree_installs_and_resolves() {
        let state = scratch("good-state");
        let (dir, manifest) = pack("good");
        let installed_at = install_dir(&state, &dir, &manifest).expect("installed");
        assert!(installed_at.join("bin/test").exists());
        let (read, binary) = resolve(&state, "test").expect("resolved");
        assert_eq!(read.version, "0.1.0");
        assert_eq!(binary, installed_at.join("bin/test"));
        let _ = std::fs::remove_dir_all(&state);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_missing_from_the_sums_is_refused() {
        let state = scratch("unlisted-state");
        let (dir, manifest) = pack("unlisted");
        std::fs::write(dir.join("extra.sh"), "echo hi\n").expect("extra");
        let refused = install_dir(&state, &dir, &manifest).expect_err("unlisted");
        assert!(refused.contains("not in SHA256SUMS"), "{refused}");
        let _ = std::fs::remove_dir_all(&state);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_tampered_binary_is_not_run() {
        let state = scratch("tamper-state");
        let (dir, manifest) = pack("tamper");
        let installed_at = install_dir(&state, &dir, &manifest).expect("installed");
        std::fs::write(installed_at.join("bin/test"), "echo changed\n").expect("tamper");
        let refused = resolve(&state, "test").expect_err("tampered");
        assert!(refused.contains("does not match the digest"), "{refused}");
        let _ = std::fs::remove_dir_all(&state);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_manifest_cannot_climb_out_of_bin() {
        let text = r#"{"format":1,"kind":"connector","id":"x","version":"1.0.0","title":"x","summary":"x","license":"MIT","protocol":{"min":1,"max":1},"platform":"linux-x86_64","entrypoint":"bin/../x"}"#;
        let refused = Manifest::parse(text).expect_err("climb");
        assert!(refused.contains("climbs"), "{refused}");
    }

    #[test]
    fn a_manifest_with_an_unknown_key_is_refused() {
        let text = r#"{"format":1,"kind":"connector","id":"x","version":"1.0.0","title":"x","summary":"x","license":"MIT","protocol":{"min":1,"max":1},"platform":"linux-x86_64","entrypoint":"bin/x","token_env":"OPENAI_API_KEY"}"#;
        let refused = Manifest::parse(text).expect_err("unknown");
        assert!(refused.contains("unknown key"), "{refused}");
    }
}
