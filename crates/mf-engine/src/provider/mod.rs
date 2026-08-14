//! Provider abstraction.
//!
//! Wire types here are provider-neutral; each implementation owns its own native codec and
//! normalises into [`StreamEvent`]. Adding a provider means adding one file that implements
//! [`AiProvider`].
//!
//! The trait uses `impl Future + Send` rather than `async_trait`, so calls are neither boxed nor
//! dyn-dispatched. Providers are a closed set dispatched by enum (plugin-supplied providers are
//! deliberately out of scope — a custom OpenAI-compatible endpoint covers that need with config).

pub mod anthropic;
pub mod openai;

use std::collections::BTreeMap;

use futures::stream::BoxStream;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};

use crate::proto::{ProviderId, StreamEvent};

pub trait AiProvider: Send + Sync {
    fn kind(&self) -> ProviderKind;
    fn capabilities(&self) -> Capabilities;

    fn models(&self) -> impl Future<Output = Result<Vec<ModelInfo>, ProviderError>> + Send;

    fn stream(
        &self,
        req: ChatRequest,
    ) -> impl Future<Output = Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError>> + Send;
}

/// The configured set of codecs, dispatched by [`ProviderKind`].
///
/// [`AiProvider`] returns `impl Future`, so it is not object-safe and cannot be `Box<dyn>`. That
/// is the right trade here — providers are a closed set — but it means the runtime choice has to
/// be an enum, and this is it.
pub enum AnyProvider {
    OpenAi(openai::OpenAi),
    Anthropic(anthropic::Anthropic),
}

impl AnyProvider {
    pub fn new(cfg: ProviderConfig, http: reqwest::Client) -> Result<Self, ProviderError> {
        match cfg.kind {
            ProviderKind::Anthropic => Ok(Self::Anthropic(anthropic::Anthropic::new(cfg, http))),
            // Ollama's `/v1` endpoint really is OpenAI's wire format, so it shares the codec
            // rather than getting a near-identical copy of it.
            ProviderKind::OpenAi | ProviderKind::Ollama => {
                Ok(Self::OpenAi(openai::OpenAi::new(cfg, http)))
            }
            // Falling through to the OpenAI codec would send Google a body it cannot parse and
            // then report the resulting 400 as if the user had misconfigured something.
            ProviderKind::Google => Err(ProviderError::Unsupported(
                "the Google codec is not implemented yet — use an OpenAI-compatible endpoint"
                    .into(),
            )),
        }
    }

    pub fn base_url(&self) -> &str {
        match self {
            Self::OpenAi(p) => p.base_url(),
            Self::Anthropic(p) => p.base_url(),
        }
    }
}

impl AiProvider for AnyProvider {
    fn kind(&self) -> ProviderKind {
        match self {
            Self::OpenAi(p) => p.kind(),
            Self::Anthropic(p) => p.kind(),
        }
    }

    fn capabilities(&self) -> Capabilities {
        match self {
            Self::OpenAi(p) => p.capabilities(),
            Self::Anthropic(p) => p.capabilities(),
        }
    }

    async fn models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        match self {
            Self::OpenAi(p) => p.models().await,
            Self::Anthropic(p) => p.models().await,
        }
    }

    async fn stream(
        &self,
        req: ChatRequest,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        match self {
            Self::OpenAi(p) => p.stream(req).await,
            Self::Anthropic(p) => p.stream(req).await,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ProviderKind {
    /// Also serves OpenRouter, DeepSeek, Groq, LM Studio and any OpenAI-compatible endpoint —
    /// their native API *is* this wire format, they differ only by base URL and headers.
    #[default]
    OpenAi,
    Anthropic,
    Google,
    Ollama,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub tools: bool,
    pub vision: bool,
    pub thinking: bool,
}

/// A configured provider entry. Note the key is a [`SecretString`]: its `Debug` prints
/// `[REDACTED]`, which is what stops the most common leak — a `#[derive(Debug)]` struct
/// reaching a log line.
#[derive(Clone)]
pub struct ProviderConfig {
    pub id: ProviderId,
    pub name: String,
    pub kind: ProviderKind,
    pub base_url: String,
    pub api_key: Option<SecretString>,
    pub org_id: Option<String>,
    pub headers: BTreeMap<String, String>,
}

impl std::fmt::Debug for ProviderConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderConfig")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "[REDACTED]"))
            .field("org_id", &self.org_id)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub context_window: Option<u32>,
}

#[derive(Debug, Clone, Default)]
pub struct ChatRequest {
    pub model: String,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSchema>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct Message {
    pub role: Role,
    pub content: Vec<Part>,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Self { role: Role::User, content: vec![Part::Text(text.into())] }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self { role: Role::Assistant, content: vec![Part::Text(text.into())] }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone)]
pub enum Part {
    Text(String),
    ToolCall { id: String, name: String, args: serde_json::Value },
    ToolResult { id: String, content: String, is_error: bool },
}

#[derive(Debug, Clone)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool's arguments, shipped verbatim to the provider.
    pub parameters: serde_json::Value,
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("http: {0}")]
    Http(String),
    #[error("{provider} returned {status}: {message}")]
    Api { provider: &'static str, status: u16, message: String },
    #[error("could not decode response: {0}")]
    Decode(String),
    #[error("{0}")]
    Unsupported(String),
}

impl From<reqwest::Error> for ProviderError {
    fn from(e: reqwest::Error) -> Self {
        // `without_url` matters: a provider URL can carry credentials in its query string, and
        // this error string ends up in logs and in the UI.
        ProviderError::Http(e.without_url().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::ProviderId;

    /// The single most likely way to leak a key is a `{:?}` on a config struct reaching a log
    /// line, so the redaction is asserted rather than assumed.
    #[test]
    fn debug_never_prints_the_api_key() {
        let cfg = ProviderConfig {
            id: ProviderId::new(),
            name: "test".into(),
            kind: ProviderKind::OpenAi,
            base_url: "https://api.openai.com/v1".into(),
            api_key: Some(SecretString::from("sk-SUPERSECRET123")),
            org_id: None,
            headers: Default::default(),
        };

        let rendered = format!("{cfg:?}");
        assert!(!rendered.contains("SUPERSECRET"), "key leaked into Debug: {rendered}");
        assert!(rendered.contains("REDACTED"), "{rendered}");
        // The non-secret fields still need to be useful for debugging.
        assert!(rendered.contains("api.openai.com"));
    }

    #[test]
    fn debug_distinguishes_a_missing_key_from_a_redacted_one() {
        let mut cfg = ProviderConfig {
            id: ProviderId::new(),
            name: "local".into(),
            kind: ProviderKind::OpenAi,
            base_url: "http://localhost:11434/v1".into(),
            api_key: None,
            org_id: None,
            headers: Default::default(),
        };
        assert!(format!("{cfg:?}").contains("None"), "an unset key must read as unset");

        cfg.api_key = Some(SecretString::from("k"));
        assert!(format!("{cfg:?}").contains("REDACTED"));
    }
}
