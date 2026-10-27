//! Providers: what an endpoint is, and which model names it can answer.
//!
//! A preset is a model this build ships a route for. A provider is a route the owner names, and it can
//! hold more than one model: `groq` with `llama-3.3-70b` and `qwen-2.5-72b`, one base URL and one key
//! between them. That is the shape Hermes uses, and the reason to copy it is arithmetic: a config that
//! names a provider once instead of one endpoint per model is a config that stays right when the model
//! list changes.
//!
//! The model is named `provider/model`, and a bare model name resolves against the providers that list
//! it. Nothing here reaches the network or reads a key: a provider describes where a key *would* come
//! from, and the value is resolved at call time by the key seam.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;

/// How a provider speaks. Only `chat_completions` is implemented, and saying so here is better than
/// accepting `anthropic_messages` and sending an OpenAI body to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiMode {
    /// OpenAI-compatible chat completions. The default, and the one every local server speaks.
    ChatCompletions,
    /// Anthropic messages: `/messages`, `x-api-key`, and a `content` array rather than `choices`.
    AnthropicMessages,
    /// OpenAI's responses API: `/responses`, `input` rather than `messages`.
    Responses,
}

impl ApiMode {
    fn parse(text: &str) -> Result<Self, String> {
        match text {
            "chat_completions" | "chat-completions" | "openai" | "chat" => Ok(Self::ChatCompletions),
            "anthropic_messages" | "anthropic" | "messages" | "claude" => Ok(Self::AnthropicMessages),
            "responses" | "openai_responses" | "codex" => Ok(Self::Responses),
            other => Err(format!(
                "api_mode `{other}` is not one this build speaks; say chat_completions, \
                 anthropic_messages or responses"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat_completions",
            Self::AnthropicMessages => "anthropic_messages",
            Self::Responses => "responses",
        }
    }

    /// The path a request is posted to, for a base URL that is a root rather than a full endpoint.
    fn path(self) -> &'static str {
        match self {
            Self::ChatCompletions => "/chat/completions",
            Self::AnthropicMessages => "/messages",
            Self::Responses => "/responses",
        }
    }
}

/// One named endpoint and the models it offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provider {
    pub id: String,
    /// The root of the API, `https://host/v1` or a full `/chat/completions` URL. A root is joined with
    /// the one path this build knows; a full URL is used as given, so a proxy that rewrites the path
    /// still works.
    pub base_url: String,
    pub api_mode: ApiMode,
    /// The *name* of the variable holding the key. The value is never in this struct.
    pub key_env: Option<String>,
    /// Model names this provider answers. Optional: a provider reached only as `id/model` needs none.
    pub models: Vec<String>,
    pub context: usize,
    /// Whether to ask the template for thinking. `None` sends nothing, which is what a cloud endpoint
    /// wants and what an unknown template needs.
    pub thinking: Option<bool>,
    /// Headers the provider needs on every call, for a gateway that routes by one. Values are literals:
    /// a header is not a secret, and a secret belongs in `key_env`.
    pub headers: std::collections::BTreeMap<String, String>,
}

impl Provider {
    /// The URL a call is posted to, for this provider's mode. A base URL that already names a full
    /// endpoint is used as given, so a proxy that rewrites the path still works.
    pub fn endpoint_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        let path = self.api_mode.path();
        if base.ends_with(path) {
            base.to_string()
        } else {
            format!("{base}{path}")
        }
    }

    /// The chat-completions URL, kept as a name for the common case.
    pub fn chat_url(&self) -> String {
        self.endpoint_url()
    }
}

/// The providers a config names, and the model resolution that follows from them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Registry {
    pub providers: BTreeMap<String, Provider>,
}

impl Registry {
    /// Read `providers` from a config value. Absent is an empty registry, not an error: a file with only
    /// presets is a valid file. Present but wrong is refused with the field named.
    pub fn parse(value: Option<&Value>, path: &Path) -> Result<Self, String> {
        let Some(value) = value else {
            return Ok(Self::default());
        };
        let object = value.as_object().ok_or_else(|| {
            format!(
                "{}: `providers` is not an object of name to provider",
                path.display()
            )
        })?;
        let mut providers = BTreeMap::new();
        for (id, entry) in object {
            if id.trim().is_empty()
                || !id
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
            {
                return Err(format!(
                    "{}: `providers.{id}` is not a usable name; letters, digits, dash and underscore",
                    path.display()
                ));
            }
            providers.insert(id.clone(), Provider::parse(id, entry, path)?);
        }
        Ok(Self { providers })
    }

    pub fn get(&self, id: &str) -> Option<&Provider> {
        self.providers.get(id)
    }

    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    /// A model name to a provider and the model's own name.
    ///
    /// `provider/model` is explicit and wins. A bare name is looked for in every provider's list, in id
    /// order, so the answer does not depend on the order the file happened to be written in.
    pub fn resolve(&self, name: &str) -> Option<(&Provider, String)> {
        if let Some((id, model)) = name.split_once('/')
            && !model.is_empty()
            && let Some(provider) = self.providers.get(id)
        {
            return Some((provider, model.to_string()));
        }
        self.providers
            .values()
            .find(|provider| provider.models.iter().any(|model| model == name))
            .map(|provider| (provider, name.to_string()))
    }

    /// Every model name this registry can reach, for a menu or a refusal.
    pub fn known(&self) -> Vec<String> {
        let mut known = Vec::new();
        for provider in self.providers.values() {
            for model in &provider.models {
                known.push(format!("{}/{model}", provider.id));
            }
        }
        known
    }
}

impl Provider {
    fn parse(id: &str, value: &Value, path: &Path) -> Result<Self, String> {
        let object = value.as_object().ok_or_else(|| {
            format!(
                "{}: `providers.{id}` is not an object",
                path.display()
            )
        })?;
        let base_url = object
            .get("base_url")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|url| url.starts_with("http://") || url.starts_with("https://"))
            .ok_or_else(|| {
                format!(
                    "{}: `providers.{id}.base_url` must start with http:// or https://",
                    path.display()
                )
            })?
            .to_string();
        if base_url.contains(char::is_whitespace) {
            return Err(format!(
                "{}: `providers.{id}.base_url` has whitespace in it",
                path.display()
            ));
        }
        let api_mode = match object.get("api_mode") {
            None => ApiMode::ChatCompletions,
            Some(Value::String(text)) => ApiMode::parse(text).map_err(|said| {
                format!("{}: `providers.{id}.{said}`", path.display())
            })?,
            Some(other) => {
                return Err(format!(
                    "{}: `providers.{id}.api_mode` is {other}; it is a string",
                    path.display()
                ));
            }
        };
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
                        "{}: `providers.{id}.key_env` is a variable name, not a key",
                        path.display()
                    ));
                }
                Some(text.to_string())
            }
            Some(_) => {
                return Err(format!(
                    "{}: `providers.{id}.key_env` is not a string",
                    path.display()
                ));
            }
        };
        let models = match object.get("models") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => {
                let mut models = Vec::new();
                for item in items {
                    let model = item
                        .as_str()
                        .map(str::trim)
                        .filter(|model| !model.is_empty())
                        .ok_or_else(|| {
                            format!(
                                "{}: `providers.{id}.models` holds something that is not a name",
                                path.display()
                            )
                        })?;
                    models.push(model.to_string());
                }
                models
            }
            Some(_) => {
                return Err(format!(
                    "{}: `providers.{id}.models` is a list of names",
                    path.display()
                ));
            }
        };
        let context = object
            .get("context_length")
            .or_else(|| object.get("context"))
            .and_then(Value::as_u64)
            .filter(|tokens| *tokens > 0)
            .unwrap_or(32_768) as usize;
        let thinking = object.get("thinking").and_then(Value::as_bool);
        let headers = match object.get("headers") {
            None | Some(Value::Null) => std::collections::BTreeMap::new(),
            Some(Value::Object(map)) => {
                let mut headers = std::collections::BTreeMap::new();
                for (name, value) in map {
                    let text = value.as_str().ok_or_else(|| {
                        format!(
                            "{}: `providers.{id}.headers.{name}` is a string",
                            path.display()
                        )
                    })?;
                    headers.insert(name.clone(), text.to_string());
                }
                headers
            }
            Some(_) => {
                return Err(format!(
                    "{}: `providers.{id}.headers` is an object of header name to value",
                    path.display()
                ));
            }
        };
        Ok(Self {
            id: id.to_string(),
            base_url,
            api_mode,
            key_env,
            models,
            context,
            thinking,
            headers,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    fn parse(value: Value) -> Result<Registry, String> {
        Registry::parse(Some(&value), &PathBuf::from("config.json"))
    }

    #[test]
    fn a_provider_holds_several_models_behind_one_url_and_one_key() {
        let registry = parse(json!({
            "groq": {
                "base_url": "https://api.groq.com/openai/v1",
                "api_mode": "chat_completions",
                "key_env": "GROQ_API_KEY",
                "models": ["llama-3.3-70b", "qwen-2.5-72b"],
                "context_length": 131072
            }
        }))
        .expect("a provider");
        let (provider, model) = registry.resolve("groq/qwen-2.5-72b").expect("explicit");
        assert_eq!(provider.id, "groq");
        assert_eq!(model, "qwen-2.5-72b");
        assert_eq!(
            provider.chat_url(),
            "https://api.groq.com/openai/v1/chat/completions"
        );
        assert_eq!(provider.key_env.as_deref(), Some("GROQ_API_KEY"));
        assert_eq!(provider.context, 131_072);
        // A bare name resolves through the list, and the same name from another provider does not.
        let (provider, model) = registry.resolve("llama-3.3-70b").expect("bare");
        assert_eq!(provider.id, "groq");
        assert_eq!(model, "llama-3.3-70b");
        assert!(registry.resolve("llama-3.3-8b").is_none());
    }

    #[test]
    fn a_full_completions_url_is_used_as_given() {
        let registry = parse(json!({
            "proxy": {"base_url": "https://proxy.example/v1/chat/completions"}
        }))
        .expect("a provider");
        let (provider, _) = registry.resolve("proxy/any-model").expect("explicit");
        assert_eq!(
            provider.chat_url(),
            "https://proxy.example/v1/chat/completions"
        );
    }

    #[test]
    fn the_three_modes_parse_and_a_fourth_is_refused_by_name() {
        let registry = parse(json!({
            "a": {"base_url": "https://api.example/v1", "api_mode": "anthropic_messages"},
            "b": {"base_url": "https://api.example/v1", "api_mode": "responses"}
        }))
        .expect("two modes");
        assert_eq!(
            registry.get("a").expect("a").endpoint_url(),
            "https://api.example/v1/messages"
        );
        assert_eq!(
            registry.get("b").expect("b").endpoint_url(),
            "https://api.example/v1/responses"
        );
        let refused = parse(json!({
            "gemini": {"base_url": "https://api.example/v1", "api_mode": "generate_content"}
        }))
        .expect_err("refused");
        assert!(refused.contains("generate_content"), "{refused}");
        assert!(refused.contains("anthropic_messages"), "{refused}");
    }

    #[test]
    fn a_key_is_a_variable_name_and_a_url_has_to_be_a_url() {
        let refused = parse(json!({
            "x": {"base_url": "api.example/v1"}
        }))
        .expect_err("no scheme");
        assert!(refused.contains("base_url"), "{refused}");
        let refused = parse(json!({
            "x": {"base_url": "https://api.example/v1", "key_env": "sk-live-abc"}
        }))
        .expect_err("a key, not a name");
        assert!(refused.contains("key_env"), "{refused}");
    }
}
