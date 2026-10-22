//! Fetching a pack from the shelf.
//!
//! Two things are checked before anything is read as a pack: the index has to parse, and the archive has to
//! hash to the digest the index published. The digest is computed over the bytes that arrived, not over a
//! header or a filename, so a truncated download, a swapped archive and a redirected URL all fail the same
//! way and none of them reaches the manifest parser.
//!
//! The base URL is a constant and may only be replaced by `HADES_PACKS_URL`, which is for a mirror you run
//! yourself. `HADES_PACKS_ALLOW_PRIVATE=1` exists for that mirror; the default refuses private addresses,
//! because "fetch this and run it" is not a sentence to point at a laptop.

use std::fs;
use std::path::Path;

use crate::packs_menu::{self, Row};

/// The public shelf. Raw files, so a fetch needs no API token and the URL a row names is the URL fetched.
pub const DEFAULT_BASE: &str = "https://raw.githubusercontent.com/GuideboardLabs/packs/main/";

/// A pack is a skill and a manifest. Four MiB is the fetch path's own body cap; a pack over it is not a pack.
const MAX_ARCHIVE: usize = 4 * 1024 * 1024;
const MAX_INDEX: usize = 256 * 1024;

pub fn base() -> Result<String, String> {
    let base = match std::env::var("HADES_PACKS_URL") {
        Ok(value) if !value.trim().is_empty() => value.trim().to_string(),
        _ => DEFAULT_BASE.to_string(),
    };
    if !base.starts_with("https://") && !allow_private() {
        return Err(format!(
            "refused: the pack base `{base}` is not https, and this build only fetches a shelf over TLS"
        ));
    }
    Ok(if base.ends_with('/') {
        base
    } else {
        format!("{base}/")
    })
}

fn allow_private() -> bool {
    matches!(
        std::env::var("HADES_PACKS_ALLOW_PRIVATE").as_deref(),
        Ok("1") | Ok("true")
    )
}

fn cache_dir(state: &Path) -> std::path::PathBuf {
    state.join("packs")
}

/// The cached index, when this machine has read the shelf before. `pack menu` is offline by default, so a
/// command that only lists what exists does not open a socket to say nothing new.
pub fn cached(state: &Path) -> Option<Vec<Row>> {
    let text = fs::read_to_string(cache_dir(state).join("INDEX.json")).ok()?;
    packs_menu::parse(&text).ok()
}

/// The shelf: the cached index when there is one, otherwise a fetch. `refresh` always fetches, which is what
/// makes a new pack visible without a new binary.
pub fn rows(state: &Path, refresh: bool) -> Result<Vec<Row>, String> {
    let path = cache_dir(state).join("INDEX.json");
    if !refresh
        && let Ok(text) = fs::read_to_string(&path)
        && let Ok(rows) = packs_menu::parse(&text)
    {
        return Ok(rows);
    }
    let url = format!("{}INDEX.json", base()?);
    let text = crate::fetch::run_raw(&url, MAX_INDEX, allow_private())
        .map_err(|error| format!("could not read the pack index at {url}: {error}"))?;
    let rows = packs_menu::parse(&text).map_err(|error| format!("{url}: {error}"))?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)
            .map_err(|error| format!("could not make {}: {error}", dir.display()))?;
    }
    fs::write(&path, &text)
        .map_err(|error| format!("could not cache the index at {}: {error}", path.display()))?;
    Ok(rows)
}

/// Fetch one pack by id, verify it, and either describe what installing it would do or do it.
///
/// `yes` is the difference between a plan and a write, and it is the only thing `yes` does: the digest is
/// checked, the plan is built and every preflight runs on both paths.
pub fn add(id: &str, home: &Path, state: &Path, yes: bool) -> Result<String, String> {
    let rows = rows(state, true)?;
    let row = rows.iter().find(|row| row.id == id).ok_or_else(|| {
        format!("`{id}` is not on the shelf. `hades pack find <words>` lists what is.")
    })?;
    // A row may name an absolute URL: that is how a pack whose bytes are not in the public repository is
    // published. The digest is the same commitment either way, so the check does not care where it came from.
    let url = if row.archive.starts_with("https://") || row.archive.starts_with("http://") {
        row.archive.clone()
    } else {
        format!("{}{}", base()?, row.archive)
    };
    let bytes = crate::fetch::bytes(&url, MAX_ARCHIVE, allow_private())
        .map_err(|error| format!("could not fetch {url}: {error}"))?;
    install(&bytes, &url, &row.sha256, home, state, yes)
}

/// Add a pack that arrived as a file: the sharing case, where there is no shelf to ask.
///
/// Without a published digest there is nothing to compare against, so the file's own sha256 is printed for
/// the person to check against what the sender said. `expect` is that check when they have one, and a
/// mismatch is refused before the archive is read, exactly as a shelf row is.
pub fn add_file(
    path: &Path,
    expect: Option<&str>,
    home: &Path,
    state: &Path,
    yes: bool,
) -> Result<String, String> {
    let bytes =
        fs::read(path).map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let digest = mneme::sha256::hex(&bytes);
    if let Some(expect) = expect
        && !expect.eq_ignore_ascii_case(&digest)
    {
        return Err(format!(
            "refused: {digest} does not match the digest you gave ({expect}). The archive was not read and \
             nothing was written."
        ));
    }
    let said = path.display().to_string();
    let text = install(&bytes, &said, &digest, home, state, yes)?;
    Ok(format!("sha256: {digest}\n{text}"))
}

fn install(
    bytes: &[u8],
    origin: &str,
    digest: &str,
    home: &Path,
    state: &Path,
    yes: bool,
) -> Result<String, String> {
    let found = mneme::sha256::hex(bytes);
    if found != digest {
        return Err(format!(
            "refused: {origin} hashes to {found}, and {digest} was published. The archive was not read and \
             nothing was written."
        ));
    }

    let dir = cache_dir(state).join("archives");
    fs::create_dir_all(&dir)
        .map_err(|error| format!("could not make {}: {error}", dir.display()))?;
    // Read it from a copy in the cache: `Pack::open` takes a file, and the cache is where a pack lives
    // between being checked and being installed.
    let incoming = dir.join("incoming.hadepack");
    fs::write(&incoming, bytes)
        .map_err(|error| format!("could not write {}: {error}", incoming.display()))?;
    let pack = crate::pack::Pack::open(&incoming)?;
    let id = pack.manifest.id.clone();
    let version = pack.manifest.version.clone();
    let file = dir.join(format!("{id}-{version}.hadepack"));
    fs::rename(&incoming, &file)
        .map_err(|error| format!("could not keep {}: {error}", file.display()))?;

    let planned = crate::pack::plan(&pack, state)?;
    let mut text = format!("{id} {version} from {origin}\n");
    text.push_str(&render(&planned));
    if !yes {
        text.push_str(
            "\nnothing was installed; run the same command with --yes to apply these decisions\n",
        );
        return Ok(text);
    }
    let mut decisions = Vec::new();
    for planned_row in &planned {
        let action = match planned_row.default_action.as_str() {
            "install" => crate::pack_fit::Action::Install,
            "replace" => crate::pack_fit::Action::Replace,
            "skip" => crate::pack_fit::Action::Skip,
            other => {
                return Err(format!(
                    "the plan has an action this build does not know: `{other}`"
                ));
            }
        };
        decisions.push(crate::pack_fit::Decision {
            kind: planned_row.kind,
            identity: planned_row.identity.clone(),
            action,
        });
    }
    let receipt = crate::pack_fit::apply(&pack, home, state, &decisions, true)?;
    text.push_str(&format!(
        "\napplied: {} item(s), {} skipped\n",
        receipt.items.len(),
        receipt.skipped
    ));
    for item in &receipt.items {
        text.push_str(&format!(
            "  {} {} -> {}\n",
            item.kind.name(),
            item.identity,
            item.live
        ));
    }
    Ok(text)
}

fn render(rows: &[crate::pack::Row]) -> String {
    let mut text = format!(
        "{:<11} {:<8} {:<30} {}\n",
        "class", "kind", "identity", "default"
    );
    for row in rows {
        text.push_str(&format!(
            "{:<11} {:<8} {:<30} {} ({})\n",
            row.class,
            row.kind.name(),
            row.identity,
            row.default_action,
            row.reason
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_base_without_a_slash_still_joins() {
        // SAFETY: the variable is read once, in this test binary, before any fetch. Restored after.
        unsafe { std::env::set_var("HADES_PACKS_URL", "https://example.invalid/shelf") };
        assert_eq!(base().expect("a base"), "https://example.invalid/shelf/");
        unsafe { std::env::remove_var("HADES_PACKS_URL") };
    }

    #[test]
    fn a_plain_http_shelf_is_refused_unless_it_is_a_private_mirror() {
        // SAFETY: as above; this test restores both variables it touches.
        unsafe {
            std::env::set_var("HADES_PACKS_URL", "http://example.invalid/shelf");
            std::env::remove_var("HADES_PACKS_ALLOW_PRIVATE");
        }
        let said = base().expect_err("http is not a shelf");
        assert!(said.contains("not https"), "{said}");
        unsafe {
            std::env::set_var("HADES_PACKS_ALLOW_PRIVATE", "1");
        }
        assert!(base().is_ok());
        unsafe {
            std::env::remove_var("HADES_PACKS_ALLOW_PRIVATE");
            std::env::remove_var("HADES_PACKS_URL");
        }
    }
}
