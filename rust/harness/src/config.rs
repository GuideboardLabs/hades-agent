//! The config file: what a headless process reads at boot.
//!
//! The CLI can be told everything with flags. A daemon or a bot cannot: it is started by systemd or by a
//! supervisor with no arguments, so the values have to exist somewhere it can read before there is
//! anyone to ask. `~/.hades/config.json` is that file, and the CLI flags become overrides on top of it.
//!
//! It holds no secrets. A provider key and a bot token stay in `~/.hades/.env`, read by the key seam and
//! by nothing else: a token in the JSON is a token in the next backup.
//!
//! The model is named as a **preset id**, not as a bare model name. The endpoint, the key's name and the
//! window all come with the preset, and a bare name would leave a daemon holding an id it has nowhere to
//! send. `hades models` lists what this build ships.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

/// The file's name under the Hades home.
pub const NAME: &str = "config.json";

/// The schema this build reads. A file from a newer build is refused rather than half-read: a daemon
/// that guesses at a field it does not know is a daemon that runs with someone else's intentions.
pub const VERSION: u64 = 1;

/// What a run needs before it has any flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The account or server family the endpoint belongs to. Recorded so a receipt can say which one a
    /// run used without parsing a URL in three places.
    pub provider: String,
    /// A preset id from [`crate::model::presets`].
    pub model: String,
    /// The working tree a run is rooted in when no `--root` is given.
    pub root: PathBuf,
    /// The Mneme vault, for the audit and for the memory seam that comes later.
    pub vault: PathBuf,
    /// The MCP servers a chat may reach, and the tools a person has allowed on each.
    pub mcp: crate::mcp::McpConfig,
}

/// The config file under a Hades home.
pub fn path_in(hades_home: &Path) -> PathBuf {
    hades_home.join(NAME)
}

/// The provider a preset belongs to, from the endpoint it names.
///
/// Derived rather than declared in the preset so there is one list of presets and not two. The three
/// answers are the three shapes this build can actually reach: a local llama.cpp server, the cloud
/// account, and anything else speaking the same OpenAI-compatible shape.
pub fn provider_for(endpoint: &str) -> &'static str {
    if endpoint.contains("127.0.0.1") || endpoint.contains("localhost") {
        "llama.cpp"
    } else if endpoint.contains("ollama.com") {
        "ollama-cloud"
    } else {
        "openai-compatible"
    }
}

/// Read the file. A missing file is `None` and not an error: a fresh install has no config until
/// `hades init` writes one, and the commands that need it say so themselves.
pub fn load(path: &Path) -> Result<Option<Config>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    let value: Value =
        serde_json::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))?;
    let object = value
        .as_object()
        .ok_or_else(|| format!("{}: the config is not a JSON object", path.display()))?;

    let version = match object.get("version") {
        None => VERSION,
        Some(Value::Number(number)) => number
            .as_u64()
            .ok_or_else(|| format!("{}: `version` is not a whole number", path.display()))?,
        Some(_) => {
            return Err(format!(
                "{}: `version` is not a whole number",
                path.display()
            ));
        }
    };
    if version > VERSION {
        return Err(format!(
            "{}: written by a newer Hades (version {version}, this build reads {VERSION})",
            path.display()
        ));
    }

    Ok(Some(Config {
        provider: field(object, "provider", path)?,
        model: field(object, "model", path)?,
        root: PathBuf::from(field(object, "root", path)?),
        vault: PathBuf::from(field(object, "vault", path)?),
        mcp: crate::mcp::McpConfig::from_value(object.get("mcp"), path)?,
    }))
}

/// Write the file, through a temp name and a rename, so a process that boots while it is being written
/// reads either the old config or the new one and never half of either.
pub fn save(path: &Path, config: &Config) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    let value = json!({
        "version": VERSION,
        "provider": config.provider,
        "model": config.model,
        "root": config.root.to_string_lossy(),
        "vault": config.vault.to_string_lossy(),
        "mcp": config.mcp.to_value(),
    });
    let text = serde_json::to_string_pretty(&value)
        .map_err(|error| format!("the config could not be rendered: {error}"))?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, format!("{text}\n"))
        .map_err(|error| format!("{}: {error}", temporary.display()))?;
    std::fs::rename(&temporary, path).map_err(|error| {
        format!(
            "{}: the config was written to {} but not moved into place: {error}",
            path.display(),
            temporary.display()
        )
    })
}

/// One string field, trimmed and required. Every field is required: a config that names three of four
/// things is a config whose fourth was lost, and guessing it is how a run ends up in the wrong place.
fn field(object: &Map<String, Value>, key: &str, path: &Path) -> Result<String, String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            format!(
                "{}: `{key}` is missing; run `hades init` to write the file again",
                path.display()
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = scratch_base().join(format!("hades-config-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        dir
    }

    fn scratch_base() -> PathBuf {
        let base = crate::scratch::root()
            .canonicalize()
            .unwrap_or_else(|_| crate::scratch::root());
        std::fs::create_dir_all(&base).expect("the scratch base");
        base
    }

    fn sample() -> Config {
        Config {
            provider: "ollama-cloud".to_string(),
            model: "deepseek-v4.1-flash".to_string(),
            root: PathBuf::from("/work/tree"),
            vault: PathBuf::from("/work/tree/.hades/mneme"),
            mcp: crate::mcp::McpConfig::from_value(
                Some(&json!({
                    "servers": {
                        "echo": {
                            "command": "python3",
                            "args": ["server.py"],
                            "allow": ["echo"],
                            "timeout_s": 10
                        }
                    }
                })),
                Path::new("sample"),
            )
            .expect("the sample mcp section"),
        }
    }

    #[test]
    fn a_missing_file_is_not_a_config_and_not_an_error() {
        let dir = scratch("missing");
        assert_eq!(load(&dir.join("config.json")).expect("load"), None);
    }

    #[test]
    fn the_file_round_trips_exactly() {
        let dir = scratch("round-trip");
        let path = path_in(&dir);
        save(&path, &sample()).expect("save");
        assert_eq!(load(&path).expect("load"), Some(sample()));
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("\"version\": 1"), "{text}");
        assert!(text.ends_with('\n'), "{text}");
    }

    #[test]
    fn a_field_that_was_lost_is_named_rather_than_guessed() {
        let dir = scratch("missing-field");
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            r#"{"version":1,"provider":"ollama-cloud","model":"x","root":"/r"}"#,
        )
        .expect("write");
        let said = load(&path).expect_err("a vault is required");
        assert!(said.contains("`vault` is missing"), "{said}");
        assert!(said.contains("hades init"), "{said}");
    }

    #[test]
    fn a_file_from_a_newer_build_is_refused() {
        let dir = scratch("newer");
        let path = dir.join("config.json");
        std::fs::write(&path, r#"{"version":99}"#).expect("write");
        let said = load(&path).expect_err("newer is not readable");
        assert!(said.contains("newer Hades"), "{said}");
    }

    #[test]
    fn a_misspelled_mcp_key_is_refused_rather_than_ignored() {
        let dir = scratch("mcp-typo");
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            r#"{"version":1,"provider":"ollama-cloud","model":"x","root":"/r","vault":"/v",
                "mcp":{"servers":{"echo":{"command":"true","alow":["echo"]}}}}"#,
        )
        .expect("write");
        let said = load(&path).expect_err("a typo is not a silent empty allowlist");
        assert!(said.contains("alow"), "{said}");
        assert!(said.contains("allow"), "{said}");
    }

    #[test]
    fn an_empty_mcp_section_is_an_empty_registry() {
        let dir = scratch("mcp-empty");
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            r#"{"version":1,"provider":"ollama-cloud","model":"x","root":"/r","vault":"/v"}"#,
        )
        .expect("write");
        let config = load(&path).expect("load").expect("a config");
        assert!(config.mcp.servers.is_empty());
    }

    #[test]
    fn the_provider_is_derived_from_what_the_preset_actually_reaches() {
        assert_eq!(provider_for("http://127.0.0.1:8110/v1"), "llama.cpp");
        assert_eq!(provider_for("https://ollama.com/v1"), "ollama-cloud");
        assert_eq!(
            provider_for("https://someone.example/v1"),
            "openai-compatible"
        );
    }
}
