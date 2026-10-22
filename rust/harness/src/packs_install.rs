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
use std::io::Read as _;
use std::path::Path;

use crate::packs_menu::{self, Row};

/// The public shelf. Raw files, so a fetch needs no API token and the URL a row names is the URL fetched.
pub const DEFAULT_BASE: &str =
    "https://raw.githubusercontent.com/GuideboardLabs/hades-agent-packs/main/";

/// A pack is a skill and a manifest. Four MiB is the fetch path's own body cap; a pack over it is not a pack.
const MAX_ARCHIVE: usize = 4 * 1024 * 1024;
const MAX_INDEX: usize = 256 * 1024;

/// Distinguishes two installations in the same process, where the clock alone could collide.
static STAGING: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

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
/// When the cached index was fetched, in seconds since the Unix epoch, so a menu can say how old it is.
pub fn cached_age(state: &Path) -> Option<u64> {
    let text = fs::read_to_string(cache_dir(state).join("INDEX.fetched")).ok()?;
    let at: u64 = text.trim().parse().ok()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(now.saturating_sub(at))
}

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
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let _ = fs::write(cache_dir(state).join("INDEX.fetched"), at.to_string());
    Ok(rows)
}

/// Fetch one pack by id, verify it, and either describe what installing it would do or do it.
///
/// `yes` is the difference between a plan and a write, and it is the only thing `yes` does: the digest is
/// checked, the plan is built and every preflight runs on both paths.
pub fn add(
    id: &str,
    expect: Option<&str>,
    home: &Path,
    state: &Path,
    yes: bool,
) -> Result<String, String> {
    let rows = rows(state, true)?;
    let row = rows.iter().find(|row| row.id == id).ok_or_else(|| {
        format!("`{id}` is not on the shelf. `hades pack find <words>` lists what is.")
    })?;
    // A digest the operator names is a pin on exact bytes, not a hint. If the shelf has moved on, the
    // installation is refused before anything is fetched, because the alternative is installing something
    // other than what was asked for and calling it a success.
    if let Some(expect) = expect
        && !expect.eq_ignore_ascii_case(&row.sha256)
    {
        return Err(format!(
            "refused: you asked for {expect}, and the shelf now names {} for `{id}` {}. Nothing was \
             fetched and nothing was written.",
            row.sha256, row.version
        ));
    }
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
    // Size first, then a bounded read: `fs::read` would allocate whatever the file happens to be, which is
    // the one thing a limit is for. The extra byte is what tells a file that grew after the check.
    let on_disk = fs::metadata(path)
        .map_err(|error| format!("could not stat {}: {error}", path.display()))?
        .len();
    if on_disk > MAX_ARCHIVE as u64 {
        return Err(format!(
            "refused: {} is {on_disk} bytes, over the {MAX_ARCHIVE}-byte ceiling for a pack. Nothing was \
             read.",
            path.display()
        ));
    }
    let mut bytes = Vec::new();
    let file = fs::File::open(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    file.take(MAX_ARCHIVE as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    if bytes.len() > MAX_ARCHIVE {
        return Err(format!(
            "refused: {} grew past the {MAX_ARCHIVE}-byte ceiling while it was being read. Nothing was \
             installed.",
            path.display()
        ));
    }
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
    // A staging file of this installation's own: `create_new` means two processes cannot write the same
    // path, and a name built from the process, the clock and a counter means they do not try.
    sweep_stale(&dir);
    let staging = dir.join(format!(
        "incoming-{}-{}-{}.hadepack",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0),
        STAGING.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let written = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staging)
        .and_then(|mut file| {
            std::io::Write::write_all(&mut file, bytes)?;
            file.sync_all()
        });
    if let Err(error) = written {
        let _ = fs::remove_file(&staging);
        return Err(format!("could not write {}: {error}", staging.display()));
    }
    // The parser reads the file, so the two failures are told apart: an archive that is not a pack says so,
    // and only bytes that changed under the parser are reported as a changed digest. Collapsing them would
    // have reported a truncated download as a race.
    let pack = match crate::pack::Pack::open(&staging) {
        Ok(pack) => pack,
        Err(error) => {
            let _ = fs::remove_file(&staging);
            return Err(format!(
                "refused: {origin} is not a readable pack: {error}. Nothing was installed."
            ));
        }
    };
    let again = fs::read(&staging)
        .map(|bytes| mneme::sha256::hex(&bytes))
        .unwrap_or_default();
    if again != digest {
        let _ = fs::remove_file(&staging);
        return Err(format!(
            "refused: the archive staged at {} is not the bytes that were verified ({digest}). Nothing was \
             installed.",
            staging.display()
        ));
    }
    let id = pack.manifest.id.clone();
    let version = pack.manifest.version.clone();
    // The cache name is the digest, not the id: an id may be namespaced (`author/tool`), which is not a
    // filename, and two versions of one pack are two different archives.
    let file = dir.join(format!("{digest}.hadepack"));
    fs::rename(&staging, &file)
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
            // A plan that wants a person means there is no default to apply. Saying so, with the identity
            // that needs the decision, is the difference between "this needs you" and a crash.
            "ask" | "review" | "refuse" => {
                return Err(format!(
                    "`{id}` needs a decision for {} `{}` ({}). Nothing was installed. Run `hades pack plan \
                     <file>` and `hades pack apply <file> --plan <decisions.json>` to choose.",
                    planned_row.kind.name(),
                    planned_row.identity,
                    planned_row.reason
                ));
            }
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
    // Nothing to do is a success, not a refusal. `pack apply` says "nothing to apply" because a person asked
    // it to change something; `pack add` asked for a pack to be present, and it already is.
    if decisions
        .iter()
        .all(|decision| decision.action == crate::pack_fit::Action::Skip)
    {
        text.push_str(&format!(
            "\nalready installed: {id} {version}, every item unchanged\n"
        ));
        return Ok(text);
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

/// Remove staging files whose process is gone. A `kill -9` during a write leaves one behind, and a directory
/// that grows by a few kilobytes on every interrupted install is a leak with a slow fuse. The pid is in the
/// name, so the check is whether that process still exists; a live install keeps its own.
fn sweep_stale(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(rest) = name.strip_prefix("incoming-") else {
            continue;
        };
        let Some(pid) = rest
            .split('-')
            .next()
            .and_then(|pid| pid.parse::<u32>().ok())
        else {
            continue;
        };
        if !Path::new("/proc").join(pid.to_string()).exists() {
            let _ = fs::remove_file(entry.path());
        }
    }
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
        let _env = crate::test_env::hold();
        // SAFETY: the variable is read once, in this test binary, before any fetch. Restored after.
        unsafe { std::env::set_var("HADES_PACKS_URL", "https://example.invalid/shelf") };
        assert_eq!(base().expect("a base"), "https://example.invalid/shelf/");
        unsafe { std::env::remove_var("HADES_PACKS_URL") };
    }

    #[test]
    fn a_plain_http_shelf_is_refused_unless_it_is_a_private_mirror() {
        let _env = crate::test_env::hold();
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
