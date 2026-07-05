//! Real Voyage embeddings HTTP adapter.
//!
//! This is the ONLY place the crate calls the Voyage embeddings API. It
//! implements the [`crate::embed::Embedder`] port with a blocking `ureq` POST
//! to `/v1/embeddings`; all decision logic that consumes embeddings (cosine,
//! fusion, storage, the RAG flow) lives in non-shim modules behind the trait,
//! so this file is excluded from coverage and driven by a fake in tests.

use serde::{Deserialize, Serialize};

use crate::embed::Embedder;
use crate::error::{Error, Result};

/// Default Voyage embeddings endpoint.
const VOYAGE_URL: &str = "https://api.voyageai.com/v1/embeddings";

/// [`Embedder`] backed by the Voyage embeddings HTTP API.
#[derive(Debug, Clone)]
pub struct VoyageEmbedder {
    api_key: String,
    model: String,
    url: String,
}

impl VoyageEmbedder {
    /// Build an embedder for `model` using `api_key`, hitting the default
    /// Voyage endpoint.
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        VoyageEmbedder {
            api_key: api_key.into(),
            model: model.into(),
            url: VOYAGE_URL.to_string(),
        }
    }

    /// Build an embedder from the environment: `VOYAGE_API_KEY` and, optionally,
    /// `CSX_EMBED_MODEL` (defaulting to `voyage-3`). Fails if no key is set.
    pub fn from_env() -> Result<Self> {
        let api_key = std::env::var("VOYAGE_API_KEY")
            .map_err(|_| Error::other("VOYAGE_API_KEY is not set"))?;
        let model = std::env::var("CSX_EMBED_MODEL").unwrap_or_else(|_| "voyage-3".to_string());
        Ok(VoyageEmbedder::new(api_key, model))
    }

    /// Override the endpoint URL (kept for completeness; unused in production).
    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.url = url.into();
        self
    }

    /// The model identifier this embedder requests.
    pub fn model(&self) -> &str {
        &self.model
    }
}

#[derive(Serialize)]
struct VoyageRequest<'a> {
    input: &'a [String],
    model: &'a str,
}

#[derive(Deserialize)]
struct VoyageResponse {
    data: Vec<VoyageDatum>,
}

#[derive(Deserialize)]
struct VoyageDatum {
    embedding: Vec<f32>,
    index: usize,
}

impl Embedder for VoyageEmbedder {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let body = VoyageRequest {
            input: texts,
            model: &self.model,
        };
        let resp = ureq::post(&self.url)
            .set("Authorization", &format!("Bearer {}", self.api_key))
            .set("Content-Type", "application/json")
            .send_json(&body)
            .map_err(|e| Error::other(format!("voyage request failed: {e}")))?;
        let parsed: VoyageResponse = resp
            .into_json()
            .map_err(|e| Error::other(format!("voyage response decode failed: {e}")))?;

        // Reorder by `index` so the output matches the input order regardless
        // of how the API returned them.
        let mut out: Vec<Vec<f32>> = vec![Vec::new(); texts.len()];
        for datum in parsed.data {
            if datum.index >= out.len() {
                return Err(Error::other(format!(
                    "voyage returned out-of-range index {}",
                    datum.index
                )));
            }
            out[datum.index] = datum.embedding;
        }
        Ok(out)
    }
}
