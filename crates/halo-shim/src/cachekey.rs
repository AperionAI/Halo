//! Cache-key normalization.
//!
//! Ported from `src/cache_key_utils.rs` (the CJK-safe normalization). Halo
//! only needs the exact-match path, so this is the normalize + canonical-hash
//! logic, dropped of the multimodal/Redis-key helpers that don't apply.

use sha2::{Digest, Sha256};

/// Trailing punctuation stripped during normalization -- ASCII and
/// fullwidth/CJK forms, so "什么是AI？" and "什么是AI" hash the same, mirroring
/// "What is AI?" in English.
const TRAILING_PUNCTUATION: &[char] = &[
    '?', '!', '.', ',', ';', ':', '？', '！', '。', '，', '；', '：', '、',
];

/// Lowercase, trim, strip trailing punctuation (ASCII + fullwidth), and
/// collapse whitespace runs (including U+3000) to a single ASCII space.
pub fn normalize_query_text(text: &str) -> String {
    let mut n = text.to_lowercase();
    n = n.trim().trim_end_matches(TRAILING_PUNCTUATION).to_string();
    n.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Build a stable exact-match cache key from a chat/messages request body.
///
/// Canonicalizes the request-defining fields (model, system, and each
/// message's role + normalized text) into one string and SHA-256s it.
/// Sampling params (temperature, top_p, etc.) are intentionally excluded --
/// they rarely change the answer and including them shreds hit rate. Returns
/// `None` for bodies we shouldn't cache (streaming, tool-result turns, or
/// unparseable JSON) so the caller falls through to a live provider call.
pub fn request_cache_key(provider: &str, body: &str) -> Option<String> {
    let json: serde_json::Value = serde_json::from_str(body).ok()?;

    // Never serve a streaming request from a buffered cache entry.
    if json.get("stream").and_then(|v| v.as_bool()) == Some(true) {
        return None;
    }

    let model = json.get("model").and_then(|m| m.as_str()).unwrap_or("");
    let mut parts: Vec<String> = vec![format!("provider={provider}"), format!("model={model}")];

    // System (Anthropic top-level string/array, or OpenAI system messages).
    if let Some(sys) = json.get("system") {
        parts.push(format!("system={}", collect_text(sys)));
    }

    let mut saw_tool = false;
    if let Some(messages) = json.get("messages").and_then(|m| m.as_array()) {
        for m in messages {
            let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
            if role == "tool" {
                saw_tool = true;
            }
            let content = m.get("content").map(collect_text).unwrap_or_default();
            parts.push(format!("{role}:{}", normalize_query_text(&content)));
        }
    }

    // Tool-augmented turns are highly context-dependent; don't exact-match them.
    if saw_tool {
        return None;
    }

    let composite = parts.join("\n");
    let mut hasher = Sha256::new();
    hasher.update(composite.as_bytes());
    Some(format!("{:x}", hasher.finalize()))
}

/// Flatten a content value (string, or array of `{type,text}` blocks) to text.
fn collect_text(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(arr) => arr
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()).map(str::to_string))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_punctuation_and_case() {
        assert_eq!(normalize_query_text("What is Redis?"), "what is redis");
        assert_eq!(normalize_query_text("什么是Redis？"), "什么是redis");
    }

    #[test]
    fn same_prompt_differing_whitespace_same_key() {
        let a = request_cache_key(
            "openai",
            r#"{"model":"gpt-4o","messages":[{"role":"user","content":"Hello  world"}]}"#,
        );
        let b = request_cache_key(
            "openai",
            r#"{"model":"gpt-4o","messages":[{"role":"user","content":"hello world"}]}"#,
        );
        assert!(a.is_some());
        assert_eq!(a, b);
    }

    #[test]
    fn different_model_different_key() {
        let a = request_cache_key(
            "openai",
            r#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}"#,
        );
        let b = request_cache_key(
            "openai",
            r#"{"model":"gpt-4o-mini","messages":[{"role":"user","content":"hi"}]}"#,
        );
        assert_ne!(a, b);
    }

    #[test]
    fn streaming_not_cacheable() {
        let k = request_cache_key(
            "openai",
            r#"{"model":"gpt-4o","stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
        );
        assert!(k.is_none());
    }
}
