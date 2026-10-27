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

use std::collections::BTreeMap;
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
    /// Per-profile model overrides, by shipped profile id. A profile with no entry inherits `model`.
    pub profile_models: BTreeMap<String, String>,
    /// The MCP servers a chat may reach, and the tools a person has allowed on each.
    pub mcp: crate::mcp::McpConfig,
    /// Endpoints this owner named, so a model that is not a shipped preset is still choosable by id.
    pub endpoints: BTreeMap<String, Endpoint>,
    /// Toolchains under the home directory, named explicitly. Discovery never walks the home, so a rustup,
    /// nvm, pyenv or uv install there is unreadable inside the cage unless it is listed here.
    pub runtime_paths: Vec<PathBuf>,
    /// How research may reach outside the machine.
    pub research: Research,
    /// An OpenAI-compatible endpoint for the image tool. Absent, the tool starts a local model.
    pub vision: Option<Vision>,
}

/// A remote vision endpoint, in the same shape as a named model endpoint: a URL, a model id, and the
/// *name* of the variable holding the key. A config file is copied and backed up, so it never holds a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vision {
    pub url: String,
    pub model: String,
    pub key_env: Option<String>,
}

/// The research seam. `external_fallback` is off unless a person turns it on, because the fallback sends the
/// query to a third party: a local SearXNG that answers badly is a bad answer, not permission to tell Jina
/// and DuckDuckGo what was asked.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Research {
    pub external_fallback: bool,
}

impl Config {
    /// The model this config asks a named profile to run, if it asks for one.
    pub fn profile_model(&self, profile: &str) -> Option<&str> {
        self.profile_models.get(profile).map(String::as_str)
    }
}

/// A named endpoint the owner added, so a non-shipped model can be chosen by name.
///
/// The key is named by its *variable*, never by its value: a config file is copied into a backup and a
/// secret in it is a secret in the backup. `context_tokens` is required because the window decides when a
/// conversation is compressed, and a guessed window is a run that fails at the provider instead of here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub url: String,
    pub model: String,
    /// The environment variable or `.env` name holding the key, if the endpoint needs one.
    pub key_env: Option<String>,
    pub context_tokens: usize,
    /// Whether the endpoint accepts a `reasoning` field. Recorded rather than assumed.
    pub thinking: bool,
}

impl Endpoint {
    /// A URL has to say https or http, and a key is named, not pasted. Both refusals are here rather than
    /// at the call site because a bad endpoint is discovered at three in the morning otherwise.
    fn parse(name: &str, value: &Value, path: &Path) -> Result<Endpoint, String> {
        let object = value
            .as_object()
            .ok_or_else(|| format!("{}: `endpoints.{name}` is not an object", path.display()))?;
        let url = object
            .get("url")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|url| url.starts_with("http://") || url.starts_with("https://"))
            .ok_or_else(|| {
                format!(
                    "{}: `endpoints.{name}.url` must start with http:// or https://",
                    path.display()
                )
            })?
            .to_string();
        let model = object
            .get("model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .ok_or_else(|| format!("{}: `endpoints.{name}.model` is missing", path.display()))?
            .to_string();
        let key_env = match object.get("key_env") {
            None | Some(Value::Null) => None,
            Some(Value::String(text)) => {
                let text = text.trim();
                let valid = !text.is_empty()
                    && text.chars().enumerate().all(|(at, ch)| {
                        ch == '_'
                            || ch.is_ascii_uppercase()
                            || ch.is_ascii_lowercase()
                            || (at > 0 && ch.is_ascii_digit())
                    });
                if !valid {
                    return Err(format!(
                        "{}: `endpoints.{name}.key_env` is a variable name, not a key",
                        path.display()
                    ));
                }
                Some(text.to_string())
            }
            Some(_) => {
                return Err(format!(
                    "{}: `endpoints.{name}.key_env` is not a string",
                    path.display()
                ));
            }
        };
        let context_tokens = object
            .get("context_tokens")
            .and_then(Value::as_u64)
            .filter(|tokens| *tokens > 0)
            .ok_or_else(|| {
                format!(
                    "{}: `endpoints.{name}.context_tokens` must be a positive number",
                    path.display()
                )
            })? as usize;
        let thinking = object
            .get("thinking")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Ok(Endpoint {
            url,
            model,
            key_env,
            context_tokens,
            thinking,
        })
    }
}

/// The optional `endpoints` map: named endpoints that are not shipped presets.
fn endpoints(value: Option<&Value>, path: &Path) -> Result<BTreeMap<String, Endpoint>, String> {
    let Some(value) = value else {
        return Ok(BTreeMap::new());
    };
    let object = value.as_object().ok_or_else(|| {
        format!(
            "{}: `endpoints` is not an object of name to endpoint",
            path.display()
        )
    })?;
    let mut out = BTreeMap::new();
    for (name, endpoint) in object {
        if crate::model::preset(name).is_some() {
            return Err(format!(
                "{}: `endpoints.{name}` shadows a shipped preset; pick another name",
                path.display()
            ));
        }
        out.insert(name.clone(), Endpoint::parse(name, endpoint, path)?);
    }
    Ok(out)
}

/// A model id is a shipped preset or a named endpoint. Anything else is refused by name, here, because the
/// alternative is a run that starts and then cannot reach anything.
fn check_model(
    model: &str,
    endpoints: &BTreeMap<String, Endpoint>,
    path: &Path,
    field: &str,
) -> Result<(), String> {
    if crate::model::preset(model).is_none() && !endpoints.contains_key(model) {
        let mut known = endpoints.keys().cloned().collect::<Vec<_>>();
        known.extend(
            crate::model::presets()
                .iter()
                .map(|preset| preset.id.to_string()),
        );
        return Err(format!(
            "{}: `{field}` names model `{model}`, which is neither a preset nor an endpoint; try {}",
            path.display(),
            known.join(", ")
        ));
    }
    Ok(())
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

    let endpoints = endpoints(object.get("endpoints"), path)?;
    let model = field(object, "model", path)?;
    check_model(&model, &endpoints, path, "model")?;

    Ok(Some(Config {
        provider: field(object, "provider", path)?,
        model,
        root: PathBuf::from(field(object, "root", path)?),
        vault: PathBuf::from(field(object, "vault", path)?),
        profile_models: profile_models(object.get("profile_models"), &endpoints, path)?,
        mcp: crate::mcp::McpConfig::from_value(object.get("mcp"), path)?,
        endpoints,
        runtime_paths: runtime_paths(object.get("runtime_paths"), path)?,
        research: research(object.get("research"), path)?,
        vision: vision(object.get("vision"), path)?,
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
        "profile_models": config.profile_models,
        "mcp": config.mcp.to_value(),
        "research": {
            "external_fallback": config.research.external_fallback,
        },
        "vision": config.vision.as_ref().map(|vision| json!({
            "url": vision.url,
            "model": vision.model,
            "key_env": vision.key_env,
        })),
        "runtime_paths": config
            .runtime_paths
            .iter()
            .map(|path| path.to_string_lossy().to_string())
            .collect::<Vec<String>>(),
        "endpoints": config.endpoints.iter().map(|(name, endpoint)| {
            let mut value = json!({
                "url": endpoint.url,
                "model": endpoint.model,
                "context_tokens": endpoint.context_tokens,
                "thinking": endpoint.thinking,
            });
            if let Some(key) = &endpoint.key_env {
                value["key_env"] = json!(key);
            }
            (name.clone(), value)
        }).collect::<Map<String, Value>>(),
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

/// The optional `research` section. Unknown keys are refused: a misspelled switch that is silently ignored
/// is a setting the owner believes is on.
fn research(value: Option<&Value>, path: &Path) -> Result<Research, String> {
    let Some(value) = value else {
        return Ok(Research::default());
    };
    let object = value
        .as_object()
        .ok_or_else(|| format!("{}: `research` is not an object", path.display()))?;
    for key in object.keys() {
        if key != "external_fallback" {
            return Err(format!(
                "{}: `research.{key}` is not a setting this build knows",
                path.display()
            ));
        }
    }
    let external_fallback = match object.get("external_fallback") {
        None => false,
        Some(Value::Bool(flag)) => *flag,
        Some(_) => {
            return Err(format!(
                "{}: `research.external_fallback` is not true or false",
                path.display()
            ));
        }
    };
    Ok(Research { external_fallback })
}

/// The optional `runtime_paths` list: toolchains under the home directory that the cage may read.
///
/// Each entry is a path to a directory. A relative path is refused because it would resolve against
/// whatever directory the process happened to start in; a path equal to or above the home directory is
/// refused here as well as in the cage, so the error arrives where the person wrote it rather than as a
/// silent non-grant at the first shell call.
fn runtime_paths(value: Option<&Value>, path: &Path) -> Result<Vec<PathBuf>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let list = value
        .as_array()
        .ok_or_else(|| format!("{}: `runtime_paths` is not a list", path.display()))?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut kept: Vec<PathBuf> = Vec::new();
    for entry in list {
        let text = entry
            .as_str()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .ok_or_else(|| {
                format!(
                    "{}: `runtime_paths` holds something that is not a path",
                    path.display()
                )
            })?;
        let candidate = PathBuf::from(text);
        if !candidate.is_absolute() {
            return Err(format!(
                "{}: `runtime_paths` entry `{text}` is relative; a toolchain is named by its absolute path",
                path.display()
            ));
        }
        if let Some(home) = &home
            && (candidate == *home || home.starts_with(&candidate))
        {
            return Err(format!(
                "{}: `runtime_paths` entry `{text}` is the home directory or above it; name the toolchain, not the home",
                path.display()
            ));
        }
        if !kept.contains(&candidate) {
            kept.push(candidate);
        }
    }
    crate::cage::declare_toolchains(kept.clone());
    Ok(kept)
}

/// The optional `vision` object: where the image tool sends its question instead of starting a local model.
///
/// A URL has to be explicit, and a key is named rather than pasted, for the same reason a named endpoint
/// does: a config file is backed up and a secret in it is a secret in the backup.
fn vision(value: Option<&Value>, path: &Path) -> Result<Option<Vision>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let object = value
        .as_object()
        .ok_or_else(|| format!("{}: `vision` is not an object", path.display()))?;
    let url = object
        .get("url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|url| url.starts_with("http://") || url.starts_with("https://"))
        .ok_or_else(|| {
            format!(
                "{}: `vision.url` must start with http:// or https://",
                path.display()
            )
        })?
        .to_string();
    let model = object
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .ok_or_else(|| format!("{}: `vision.model` is missing", path.display()))?
        .to_string();
    let key_env = match object.get("key_env") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) if !text.trim().is_empty() => Some(text.trim().to_string()),
        Some(_) => {
            return Err(format!(
                "{}: `vision.key_env` must be a variable name, not a value",
                path.display()
            ));
        }
    };
    Ok(Some(Vision {
        url,
        model,
        key_env,
    }))
}

/// The optional `profile_models` map. Every key must name a shipped profile and every value a preset
/// this build knows: a typo that is silently ignored is a setting the owner believes is on.
fn profile_models(
    value: Option<&Value>,
    endpoints: &BTreeMap<String, Endpoint>,
    path: &Path,
) -> Result<BTreeMap<String, String>, String> {
    let Some(value) = value else {
        return Ok(BTreeMap::new());
    };
    let object = value.as_object().ok_or_else(|| {
        format!(
            "{}: `profile_models` is not an object of profile id to preset",
            path.display()
        )
    })?;
    let profiles = crate::profile::shipped()
        .iter()
        .map(|profile| profile.id.as_str().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let presets = crate::model::presets()
        .iter()
        .map(|preset| preset.id)
        .collect::<Vec<_>>()
        .join(", ");
    let mut out = BTreeMap::new();
    for (profile, model) in object {
        if crate::profile::named(profile).is_none() {
            return Err(format!(
                "{}: `profile_models.{profile}` names no shipped profile; try {profiles}",
                path.display()
            ));
        }
        let model = model
            .as_str()
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .ok_or_else(|| {
                format!(
                    "{}: `profile_models.{profile}` is not a preset id",
                    path.display()
                )
            })?;
        if crate::model::preset(model).is_none() && !endpoints.contains_key(model) {
            return Err(format!(
                "{}: `profile_models.{profile}` names model `{model}`, which is neither a preset nor an \
                 endpoint; try {presets}",
                path.display()
            ));
        }
        out.insert(profile.clone(), model.to_string());
    }
    Ok(out)
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
            profile_models: BTreeMap::new(),
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
            runtime_paths: Vec::new(),
            research: Research::default(),
            endpoints: BTreeMap::new(),
            vision: None,
        }
    }

    #[test]
    fn vision_is_optional_and_a_bad_url_is_refused() {
        let dir = scratch("vision");
        let path = path_in(&dir);
        let base = r#"{"version":1,"provider":"ollama-cloud","model":"deepseek-v4.1-flash","root":"/r","vault":"/v""#;
        std::fs::write(
            &path,
            format!(
                "{base},\"vision\":{{\"url\":\"https://example.test/v1/chat/completions\",\"model\":\"vl-8b\",\"key_env\":\"VISION_KEY\"}}}}"
            ),
        )
        .expect("a config");
        let config = load(&path).expect("load").expect("a config");
        let vision = config.vision.expect("a vision endpoint");
        assert_eq!(vision.model, "vl-8b");
        assert_eq!(vision.key_env.as_deref(), Some("VISION_KEY"));

        std::fs::write(
            &path,
            format!("{base},\"vision\":{{\"url\":\"ftp://nope\",\"model\":\"m\"}}}}"),
        )
        .expect("a config");
        let error = load(&path).expect_err("a non-http url is refused");
        assert!(error.contains("vision.url"), "{error}");
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
            r#"{"version":1,"provider":"ollama-cloud","model":"deepseek-v4.1-flash","root":"/r"}"#,
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
    fn profile_models_round_trip_and_are_validated() {
        let dir = scratch("profile-models");
        let path = dir.join("config.json");
        let mut config = sample();
        config
            .profile_models
            .insert("fix".to_string(), "deepseek-v4.1-flash".to_string());
        save(&path, &config).expect("save");
        assert_eq!(load(&path).expect("load"), Some(config));

        std::fs::write(
            &path,
            r#"{"version":1,"provider":"ollama-cloud","model":"deepseek-v4.1-flash","root":"/r","vault":"/v","profile_models":{"nope":"deepseek-v4.1-flash"}}"#,
        )
        .expect("write");
        let said = load(&path).expect_err("an unknown profile is not ignored");
        assert!(said.contains("names no shipped profile"), "{said}");
        assert!(said.contains("fix"), "{said}");

        std::fs::write(
            &path,
            r#"{"version":1,"provider":"ollama-cloud","model":"deepseek-v4.1-flash","root":"/r","vault":"/v","profile_models":{"fix":"not-a-preset"}}"#,
        )
        .expect("write");
        let said = load(&path).expect_err("an unknown preset is not ignored");
        assert!(said.contains("neither a preset nor an endpoint"), "{said}");
        assert!(said.contains("not-a-preset"), "{said}");
    }

    #[test]
    fn a_misspelled_mcp_key_is_refused_rather_than_ignored() {
        let dir = scratch("mcp-typo");
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            r#"{"version":1,"provider":"ollama-cloud","model":"deepseek-v4.1-flash","root":"/r","vault":"/v",
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
            r#"{"version":1,"provider":"ollama-cloud","model":"deepseek-v4.1-flash","root":"/r","vault":"/v"}"#,
        )
        .expect("write");
        let config = load(&path).expect("load").expect("a config");
        assert!(config.mcp.servers.is_empty());
    }

    #[test]
    fn a_declared_toolchain_survives_a_round_trip() {
        let dir = scratch("toolchain-round-trip");
        let path = dir.join("config.json");
        let toolchain = dir.join("toolchain");
        std::fs::create_dir_all(&toolchain).expect("a toolchain");
        let mut config = Config {
            provider: "local".into(),
            model: crate::model::presets()[0].id.to_string(),
            root: dir.clone(),
            vault: dir.join("vault"),
            profile_models: Default::default(),
            mcp: crate::mcp::McpConfig::default(),
            endpoints: Default::default(),
            runtime_paths: vec![toolchain.clone()],
            research: Research::default(),
            vision: None,
        };
        save(&path, &config).expect("a save");
        let read = load(&path).expect("a read").expect("a config");
        assert_eq!(read.runtime_paths, vec![toolchain.clone()]);
        // The load is what arms the cage's declared list, so the path has to be there afterwards.
        assert!(crate::cage::declared().contains(&toolchain));
        config.runtime_paths.clear();
        save(&path, &config).expect("a save");
    }

    #[test]
    fn research_fallback_is_off_by_default_and_a_misspelling_is_refused() {
        let dir = scratch("research-section");
        let path = dir.join("config.json");
        let mut config = sample();
        config.root = dir.clone();
        config.vault = dir.join("vault");
        save(&path, &config).expect("a save");
        let read = load(&path).expect("a read").expect("a config");
        assert!(!read.research.external_fallback);

        // A turned-on switch survives, and a key this build does not know is an error rather than a setting
        // the owner believes is on.
        config.research.external_fallback = true;
        save(&path, &config).expect("a save");
        assert!(
            load(&path)
                .expect("a read")
                .expect("a config")
                .research
                .external_fallback
        );

        let text = std::fs::read_to_string(&path).expect("the file");
        std::fs::write(&path, text.replace("external_fallback", "external_fallbak"))
            .expect("a write");
        let error = load(&path).expect_err("an unknown key is refused");
        assert!(
            error.contains("is not a setting this build knows"),
            "{error}"
        );
    }

    #[test]
    fn a_relative_or_home_toolchain_is_refused_by_name() {
        let dir = scratch("toolchain-refused");
        let text = std::fs::read_to_string(dir.join("config.json")).unwrap_or_default();
        let _ = text;
        let path = dir.join("config.json");
        let base = || Config {
            provider: "local".into(),
            model: crate::model::presets()[0].id.to_string(),
            root: dir.clone(),
            vault: dir.join("vault"),
            profile_models: Default::default(),
            mcp: crate::mcp::McpConfig::default(),
            endpoints: Default::default(),
            runtime_paths: Vec::new(),
            research: Research::default(),
            vision: None,
        };
        let mut config = base();
        config.runtime_paths = vec![PathBuf::from("relative/toolchain")];
        save(&path, &config).expect("a save");
        let error = load(&path).expect_err("a relative path is refused");
        assert!(error.contains("is relative"), "{error}");

        let home = PathBuf::from(std::env::var_os("HOME").expect("a home"));
        let mut config = base();
        config.runtime_paths = vec![home];
        save(&path, &config).expect("a save");
        let error = load(&path).expect_err("the home directory is refused");
        assert!(error.contains("home directory or above"), "{error}");

        let mut config = base();
        config.runtime_paths.clear();
        save(&path, &config).expect("a clean save");
    }

    #[test]
    fn an_endpoint_is_a_valid_model_id() {
        let dir = scratch("endpoint");
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            r#"{"version":1,"provider":"openai-compatible","model":"friend-vllm","root":"/r","vault":"/v",
                "endpoints":{"friend-vllm":{"url":"http://100.64.0.2:8000/v1","model":"Qwen3.8-27B-FP8","key_env":"FRIEND_KEY","context_tokens":32000}}}"#,
        )
        .expect("write");
        let config = load(&path).expect("load").expect("a config");
        let endpoint = &config.endpoints["friend-vllm"];
        assert_eq!(endpoint.url, "http://100.64.0.2:8000/v1");
        assert_eq!(endpoint.model, "Qwen3.8-27B-FP8");
        assert_eq!(endpoint.key_env.as_deref(), Some("FRIEND_KEY"));
        assert_eq!(endpoint.context_tokens, 32000);
    }

    #[test]
    fn an_unknown_endpoint_is_refused_by_name() {
        let dir = scratch("endpoint-missing");
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            r#"{"version":1,"provider":"openai-compatible","model":"ghost","root":"/r","vault":"/v"}"#,
        )
        .expect("write");
        let said = load(&path).expect_err("an endpoint that does not exist is not a model");
        assert!(said.contains("ghost"), "{said}");
        assert!(said.contains("endpoint"), "{said}");
    }

    #[test]
    fn a_pasted_key_is_refused_and_only_a_variable_name_is_kept() {
        let dir = scratch("endpoint-key");
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            r#"{"version":1,"provider":"openai-compatible","model":"e","root":"/r","vault":"/v",
                "endpoints":{"e":{"url":"https://x.example/v1","model":"m","key_env":"sk-live-abc123","context_tokens":8000}}}"#,
        )
        .expect("write");
        let said = load(&path).expect_err("a secret in the config is a secret in the backup");
        assert!(said.contains("variable name"), "{said}");
        assert!(!said.contains("sk-live"), "the key was echoed back: {said}");
    }

    #[test]
    fn an_endpoint_without_a_window_is_refused() {
        let dir = scratch("endpoint-window");
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            r#"{"version":1,"provider":"openai-compatible","model":"e","root":"/r","vault":"/v",
                "endpoints":{"e":{"url":"https://x.example/v1","model":"m"}}}"#,
        )
        .expect("write");
        let said = load(&path).expect_err("a guessed window is a wrong window");
        assert!(said.contains("context_tokens"), "{said}");
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
