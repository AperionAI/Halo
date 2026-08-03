//! Server-Sent-Events passthrough helpers.
//!
//! Real streaming pass-through is table stakes for an LLM proxy -- every
//! comparable gateway (LiteLLM, Portkey, Helicone, OpenRouter) forwards
//! provider SSE chunks to the client as they arrive rather than buffering the
//! full completion. Buffering would silently destroy time-to-first-token for
//! every interactive agent session and risk client-side read timeouts on long
//! completions, so this is core functionality, not a nice-to-have.
//!
//! Two small pieces make this work without pulling in a full SSE parser
//! crate:
//!   * [`ensure_openai_stream_usage`] -- OpenAI (and OpenAI-compatible) only
//!     emits a final usage-bearing chunk when the request opts in via
//!     `stream_options.include_usage`. Halo sets this automatically so token
//!     accounting works without the caller having to know about it.
//!   * [`extract_usage`] -- scans the accumulated SSE bytes once the stream
//!     ends and pulls out whatever usage info the provider sent, so telemetry
//!     is accurate without re-implementing a tokenizer.

use halo_common::telemetry::Provider;
use serde_json::Value;

/// Set `stream_options.include_usage = true` on an OpenAI-shaped streaming
/// request, unless the caller already specified `stream_options`. No-op for
/// non-streaming requests or bodies that aren't chat-completion shaped.
pub fn ensure_openai_stream_usage(json: &mut Value) {
    let is_stream = json.get("stream").and_then(|v| v.as_bool()) == Some(true);
    if !is_stream {
        return;
    }
    if json.get("stream_options").is_some() {
        return;
    }
    if let Some(obj) = json.as_object_mut() {
        obj.insert(
            "stream_options".into(),
            serde_json::json!({ "include_usage": true }),
        );
    }
}

/// Whether a (already-parsed) request body asked for a streamed response.
pub fn wants_stream(json: &Value) -> bool {
    json.get("stream").and_then(|v| v.as_bool()) == Some(true)
}

/// Best-effort usage extraction from a complete (or partially-received, if the
/// stream was aborted) SSE byte buffer. Returns
/// `(tokens_in, tokens_out, tokens_cached, model)`.
///
/// OpenAI-shaped streams: the last `data: {...}` line with a top-level
/// `usage` object (present because [`ensure_openai_stream_usage`] asked for
/// it) carries the final counts.
///
/// Anthropic streams always include usage without an opt-in: `message_start`
/// carries `input_tokens` (+ cache fields) and `message_delta` events carry
/// the running `output_tokens` -- we keep the last one seen.
pub fn extract_usage(
    provider: Provider,
    sse_text: &str,
    fallback_model: &str,
) -> (u64, u64, u64, String) {
    let mut tokens_in = 0u64;
    let mut tokens_out = 0u64;
    let mut tokens_cached = 0u64;
    let mut model = fallback_model.to_string();

    for line in sse_text.lines() {
        let line = line.trim();
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            continue;
        };

        if let Some(m) = v.get("model").and_then(|m| m.as_str()) {
            model = m.to_string();
        }

        match provider {
            Provider::Anthropic => {
                let kind = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match kind {
                    "message_start" => {
                        if let Some(u) = v.pointer("/message/usage") {
                            tokens_in = u.get("input_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
                            let cached = u
                                .get("cache_read_input_tokens")
                                .and_then(|x| x.as_u64())
                                .unwrap_or(0);
                            let created = u
                                .get("cache_creation_input_tokens")
                                .and_then(|x| x.as_u64())
                                .unwrap_or(0);
                            tokens_in += cached + created;
                            tokens_cached = cached;
                        }
                        if let Some(m) = v.pointer("/message/model").and_then(|m| m.as_str()) {
                            model = m.to_string();
                        }
                    }
                    "message_delta" => {
                        if let Some(out) = v.pointer("/usage/output_tokens").and_then(|x| x.as_u64()) {
                            tokens_out = out; // cumulative; last write wins.
                        }
                    }
                    _ => {}
                }
            }
            _ => {
                // OpenAI-compatible: only the final usage chunk (opted into via
                // ensure_openai_stream_usage) carries a top-level `usage`.
                if let Some(u) = v.get("usage") {
                    tokens_in = u.get("prompt_tokens").and_then(|x| x.as_u64()).unwrap_or(tokens_in);
                    tokens_out = u
                        .get("completion_tokens")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(tokens_out);
                    tokens_cached = u
                        .pointer("/prompt_tokens_details/cached_tokens")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(tokens_cached);
                }
            }
        }
    }

    (tokens_in, tokens_out, tokens_cached, model)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injects_usage_flag_only_when_streaming() {
        let mut j = serde_json::json!({"model":"gpt-4o","stream":true,"messages":[]});
        ensure_openai_stream_usage(&mut j);
        assert_eq!(j["stream_options"]["include_usage"], true);

        let mut j2 = serde_json::json!({"model":"gpt-4o","messages":[]});
        ensure_openai_stream_usage(&mut j2);
        assert!(j2.get("stream_options").is_none());
    }

    #[test]
    fn does_not_clobber_explicit_stream_options() {
        let mut j = serde_json::json!({
            "model":"gpt-4o","stream":true,
            "stream_options":{"include_usage":false},
            "messages":[]
        });
        ensure_openai_stream_usage(&mut j);
        assert_eq!(j["stream_options"]["include_usage"], false);
    }

    #[test]
    fn extracts_openai_final_usage_chunk() {
        let sse = concat!(
            "data: {\"id\":\"1\",\"model\":\"gpt-4o\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "data: {\"id\":\"1\",\"model\":\"gpt-4o\",\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\n",
            "data: [DONE]\n\n",
        );
        let (tin, tout, cached, model) = extract_usage(Provider::Openai, sse, "");
        assert_eq!((tin, tout, cached), (10, 5, 0));
        assert_eq!(model, "gpt-4o");
    }

    #[test]
    fn extracts_anthropic_start_and_delta() {
        let sse = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-3-5-sonnet\",\"usage\":{\"input_tokens\":20,\"cache_read_input_tokens\":5}}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":8}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":12}}\n\n",
        );
        let (tin, tout, cached, model) = extract_usage(Provider::Anthropic, sse, "");
        assert_eq!(tin, 25); // 20 input + 5 cache read
        assert_eq!(tout, 12); // last message_delta wins
        assert_eq!(cached, 5);
        assert_eq!(model, "claude-3-5-sonnet");
    }

    #[test]
    fn partial_buffer_from_aborted_stream_still_yields_partial_usage() {
        // Only message_start arrived before an abort -- output tokens are 0,
        // which is honest: we never got a delta.
        let sse = "data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-3-5-haiku\",\"usage\":{\"input_tokens\":30}}}\n\n";
        let (tin, tout, _, model) = extract_usage(Provider::Anthropic, sse, "fallback");
        assert_eq!(tin, 30);
        assert_eq!(tout, 0);
        assert_eq!(model, "claude-3-5-haiku");
    }

    #[test]
    fn wants_stream_detects_flag() {
        assert!(wants_stream(&serde_json::json!({"stream": true})));
        assert!(!wants_stream(&serde_json::json!({"stream": false})));
        assert!(!wants_stream(&serde_json::json!({})));
    }
}
