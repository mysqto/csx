//! Real Anthropic Messages HTTP adapter.
//!
//! This is the ONLY place the crate calls the Anthropic Messages API. It
//! implements the [`crate::rag::ChatClient`] port with a blocking `ureq` POST to
//! `/v1/messages` (there is no official Anthropic Rust SDK, so the request is
//! built as raw JSON per the documented wire format). All RAG decision logic —
//! retrieval, context assembly, citation extraction — lives in [`crate::rag`]
//! behind the trait, so this file is excluded from coverage and driven by a
//! fake in tests.

use serde::Deserialize;
use serde_json::json;

use crate::error::{Error, Result};
use crate::rag::ChatClient;

/// Default Anthropic Messages endpoint.
const MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
/// API version header value.
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Default model for grounded answers.
const DEFAULT_MODEL: &str = "claude-opus-4-8";

/// [`ChatClient`] backed by the Anthropic Messages HTTP API.
#[derive(Debug, Clone)]
pub struct AnthropicChat {
    api_key: String,
    model: String,
    max_tokens: u32,
    url: String,
}

impl AnthropicChat {
    /// Build a chat client for `model` using `api_key`, hitting the default
    /// Messages endpoint with a 4096-token output cap.
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        AnthropicChat {
            api_key: api_key.into(),
            model: model.into(),
            max_tokens: 4096,
            url: MESSAGES_URL.to_string(),
        }
    }

    /// Build a chat client from the environment: `ANTHROPIC_API_KEY` and,
    /// optionally, `CSX_CHAT_MODEL` (defaulting to `claude-opus-4-8`). Fails if
    /// no key is set.
    pub fn from_env() -> Result<Self> {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .map_err(|_| Error::other("ANTHROPIC_API_KEY is not set"))?;
        let model = std::env::var("CSX_CHAT_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
        Ok(AnthropicChat::new(api_key, model))
    }

    /// Override the output token cap.
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// Override the endpoint URL (kept for completeness; unused in production).
    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.url = url.into();
        self
    }

    /// The model identifier this client requests.
    pub fn model(&self) -> &str {
        &self.model
    }
}

#[derive(Deserialize)]
struct MessagesResponse {
    content: Vec<ContentBlock>,
}

#[derive(Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: String,
}

impl ChatClient for AnthropicChat {
    fn complete(&self, system: &str, prompt: &str) -> Result<String> {
        let body = json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "system": system,
            "messages": [
                { "role": "user", "content": prompt }
            ],
        });
        let resp = ureq::post(&self.url)
            .set("x-api-key", &self.api_key)
            .set("anthropic-version", ANTHROPIC_VERSION)
            .set("Content-Type", "application/json")
            .send_json(body)
            .map_err(|e| Error::other(format!("anthropic request failed: {e}")))?;
        let parsed: MessagesResponse = resp
            .into_json()
            .map_err(|e| Error::other(format!("anthropic response decode failed: {e}")))?;

        // Concatenate the text blocks; ignore non-text blocks (e.g. thinking).
        let mut out = String::new();
        for block in parsed.content {
            if block.kind == "text" {
                out.push_str(&block.text);
            }
        }
        Ok(out)
    }
}
