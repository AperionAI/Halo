//! Embedding provider abstraction for the semantic cache.
//!
//! Deliberately API-call-only. Every provider here makes an HTTP request to
//! an already-running service and returns a vector -- Halo never loads, runs,
//! or ships a model itself (no candle/hf-hub/onnx/tokenizers). That's the
//! same "no model in the process" line Halo draws for exact-match caching and
//! compression, just extended to cover this feature too:
//!   * `OpenAI` calls the real OpenAI embeddings API.
//!   * `Ollama` calls a user-supplied, already-running Ollama server's
//!     embeddings endpoint -- Halo doesn't start Ollama, just talks to it.
//!   * `Mock` is deterministic (hash-based) and free, for tests/offline dev.

use anyhow::{anyhow, Context, Result};
use halo_common::pricing::{estimate_cost_usd, PriceTable};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EmbeddingProviderKind {
    #[default]
    Openai,
    Ollama,
    Mock,
}

impl EmbeddingProviderKind {
    /// Parses config strings ("openai"/"ollama"/"mock"); an unrecognized
    /// value falls back to OpenAI rather than failing startup over a typo.
    pub fn parse(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "ollama" => EmbeddingProviderKind::Ollama,
            "mock" => EmbeddingProviderKind::Mock,
            _ => EmbeddingProviderKind::Openai,
        }
    }
}

/// One embedding call's result: the vector plus what it cost, so callers can
/// bill it honestly rather than assuming it's free.
pub struct EmbedResult {
    pub vector: Vec<f32>,
    pub tokens: u64,
    pub cost_usd: f64,
}

pub struct EmbeddingClient {
    pub kind: EmbeddingProviderKind,
    pub model: String,
    /// Only used for `Ollama` (points at the user's own server) or to
    /// override the OpenAI base for an OpenAI-compatible embeddings endpoint.
    pub base_url: Option<String>,
    http: reqwest::Client,
}

const RESERVED_KEY_ID: &str = "__embeddings__";

impl EmbeddingClient {
    pub fn new(kind: EmbeddingProviderKind, model: String, base_url: Option<String>, http: reqwest::Client) -> Self {
        Self {
            kind,
            model,
            base_url,
            http,
        }
    }

    /// Reserved [`KeyStore`](crate::keys::KeyStore) id under which the
    /// embedding API key is stored -- reuses the same keychain-first,
    /// encrypted-file-fallback storage as provider agent credentials rather
    /// than inventing a second secret store.
    pub fn key_store_id() -> &'static str {
        RESERVED_KEY_ID
    }

    pub async fn embed(&self, text: &str, api_key: Option<&str>, prices: &PriceTable) -> Result<EmbedResult> {
        match self.kind {
            EmbeddingProviderKind::Mock => Ok(mock_embed(text)),
            EmbeddingProviderKind::Openai => self.embed_openai(text, api_key, prices).await,
            EmbeddingProviderKind::Ollama => self.embed_ollama(text).await,
        }
    }

    async fn embed_openai(&self, text: &str, api_key: Option<&str>, prices: &PriceTable) -> Result<EmbedResult> {
        let key = api_key.ok_or_else(|| {
            anyhow!(
                "semantic cache is enabled with provider=openai but no embedding API key is \
                 stored; run `halo embeddings set-key` first"
            )
        })?;
        let base = self.base_url.as_deref().unwrap_or("https://api.openai.com");
        let url = format!("{}/v1/embeddings", base.trim_end_matches('/'));
        let resp = self
            .http
            .post(&url)
            .bearer_auth(key)
            .json(&serde_json::json!({ "model": self.model, "input": text }))
            .send()
            .await
            .context("calling embeddings API")?;
        let status = resp.status();
        let body: serde_json::Value = resp.json().await.context("parsing embeddings response")?;
        if !status.is_success() {
            return Err(anyhow!("embeddings API returned {status}: {body}"));
        }
        let vector: Vec<f32> = body
            .pointer("/data/0/embedding")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow!("embeddings response missing data[0].embedding"))?
            .iter()
            .filter_map(|x| x.as_f64())
            .map(|x| x as f32)
            .collect();
        let tokens = body
            .pointer("/usage/total_tokens")
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
        let cost_usd = estimate_cost_usd(prices, &self.model, tokens, 0, 0);
        Ok(EmbedResult {
            vector,
            tokens,
            cost_usd,
        })
    }

    async fn embed_ollama(&self, text: &str) -> Result<EmbedResult> {
        let base = self
            .base_url
            .as_deref()
            .unwrap_or("http://localhost:11434");
        let url = format!("{}/api/embeddings", base.trim_end_matches('/'));
        let resp = self
            .http
            .post(&url)
            .json(&serde_json::json!({ "model": self.model, "prompt": text }))
            .send()
            .await
            .context("calling local Ollama embeddings endpoint")?;
        let status = resp.status();
        let body: serde_json::Value = resp.json().await.context("parsing Ollama embeddings response")?;
        if !status.is_success() {
            return Err(anyhow!("Ollama embeddings endpoint returned {status}: {body}"));
        }
        let vector: Vec<f32> = body
            .get("embedding")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow!("Ollama response missing 'embedding'"))?
            .iter()
            .filter_map(|x| x.as_f64())
            .map(|x| x as f32)
            .collect();
        // Self-hosted: no metered per-token charge to account for.
        Ok(EmbedResult {
            vector,
            tokens: 0,
            cost_usd: 0.0,
        })
    }
}

/// Deterministic, dependency-free pseudo-embedding for tests and offline dev.
/// Not semantically meaningful beyond "similar strings get similar vectors
/// often enough to exercise the caching logic" -- never use in production.
fn mock_embed(text: &str) -> EmbedResult {
    const DIM: usize = 64;
    let norm = text.to_lowercase();
    let words: Vec<&str> = norm.split_whitespace().collect();
    let mut v = vec![0f32; DIM];
    for w in &words {
        let mut hasher_state: u64 = 1469598103934665603;
        for b in w.bytes() {
            hasher_state ^= b as u64;
            hasher_state = hasher_state.wrapping_mul(1099511628211);
        }
        let idx = (hasher_state as usize) % DIM;
        v[idx] += 1.0;
    }
    let norm_len: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_len > 0.0 {
        for x in v.iter_mut() {
            *x /= norm_len;
        }
    }
    EmbedResult {
        vector: v,
        tokens: words.len() as u64,
        cost_usd: 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_provider_is_deterministic_and_free() {
        let c = EmbeddingClient::new(EmbeddingProviderKind::Mock, "mock".into(), None, reqwest::Client::new());
        let p = PriceTable::default();
        let a = c.embed("what is redis", None, &p).await.unwrap();
        let b = c.embed("what is redis", None, &p).await.unwrap();
        assert_eq!(a.vector, b.vector);
        assert_eq!(a.cost_usd, 0.0);
    }

    #[tokio::test]
    async fn mock_provider_similar_text_yields_similar_vector() {
        let c = EmbeddingClient::new(EmbeddingProviderKind::Mock, "mock".into(), None, reqwest::Client::new());
        let p = PriceTable::default();
        let a = c.embed("what is the capital of france", None, &p).await.unwrap();
        let b = c.embed("what's the capital of France?", None, &p).await.unwrap();
        let cos = crate::semantic_cache::cosine(&a.vector, &b.vector);
        assert!(cos > 0.5, "expected high similarity, got {cos}");
    }
}
