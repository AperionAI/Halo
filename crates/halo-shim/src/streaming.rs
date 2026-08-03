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
//!   * [`extract_stream_result`] -- scans the accumulated SSE bytes once the
//!     stream ends and pulls out whatever usage info the provider sent (so
//!     telemetry is accurate without re-implementing a tokenizer), plus --
//!     when it's safe to do so -- the assistant's full text answer, so a
//!     streamed completion can feed the exact-match and semantic caches
//!     exactly like a buffered one does.

use crate::answer::{self, AnswerExtract};
use halo_common::telemetry::Provider;
use serde_json::Value;

/// Everything worth knowing once a streamed response has finished (or was
/// aborted): billing metadata plus, if it's safe to do so, a replayable
/// plain-text answer for the exact-match and semantic caches.
#[derive(Debug, Clone)]
pub struct StreamResult {
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub tokens_cached: u64,
    pub model: String,
    /// `None` if the stream contained a tool call anywhere, produced no text,
    /// or was aborted before any content arrived -- any of which means "don't
    /// cache this", not "guess".
    pub answer: Option<AnswerExtract>,
}

/// Scans a complete (or partially-received, if the stream was aborted) SSE
/// byte buffer for usage plus, when safe, a replayable text answer.
pub fn extract_stream_result(provider: Provider, sse_text: &str, fallback_model: &str) -> StreamResult {
    let mut tokens_in = 0u64;
    let mut tokens_out = 0u64;
    let mut tokens_cached = 0u64;
    let mut model = fallback_model.to_string();
    let mut text = String::new();
    let mut saw_tool_call = false;
    let mut finish_reason = "stop".to_string();

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
                    "content_block_start"
                        if v.pointer("/content_block/type").and_then(|t| t.as_str()) == Some("tool_use") =>
                    {
                        saw_tool_call = true;
                    }
                    "content_block_delta"
                        if v.pointer("/delta/type").and_then(|t| t.as_str()) == Some("text_delta") =>
                    {
                        if let Some(t) = v.pointer("/delta/text").and_then(|t| t.as_str()) {
                            text.push_str(t);
                        }
                    }
                    "message_delta" => {
                        if let Some(out) = v.pointer("/usage/output_tokens").and_then(|x| x.as_u64()) {
                            tokens_out = out;
                        }
                        if let Some(sr) = v.pointer("/delta/stop_reason").and_then(|s| s.as_str()) {
                            if sr == "tool_use" {
                                saw_tool_call = true;
                            }
                            finish_reason = sr.to_string();
                        }
                    }
                    _ => {}
                }
            }
            _ => {
                if let Some(choice) = v.get("choices").and_then(|c| c.as_array()).and_then(|a| a.first()) {
                    if let Some(delta) = choice.get("delta") {
                        let has_tools = delta
                            .get("tool_calls")
                            .map(|tc| !tc.is_null() && tc.as_array().map(|a| !a.is_empty()).unwrap_or(true))
                            .unwrap_or(false);
                        if has_tools {
                            saw_tool_call = true;
                        }
                        if let Some(c) = delta.get("content").and_then(|c| c.as_str()) {
                            text.push_str(c);
                        }
                    }
                    if let Some(fr) = choice.get("finish_reason").and_then(|f| f.as_str()) {
                        finish_reason = fr.to_string();
                    }
                }
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

    let answer = if saw_tool_call || text.is_empty() {
        None
    } else {
        Some(AnswerExtract {
            text,
            finish_reason: answer::normalize_finish(&finish_reason),
        })
    };

    StreamResult {
        tokens_in,
        tokens_out,
        tokens_cached,
        model,
        answer,
    }
}

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
        let r = extract_stream_result(Provider::Openai, sse, "");
        assert_eq!((r.tokens_in, r.tokens_out, r.tokens_cached), (10, 5, 0));
        assert_eq!(r.model, "gpt-4o");
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
        let r = extract_stream_result(Provider::Anthropic, sse, "");
        assert_eq!(r.tokens_in, 25); // 20 input + 5 cache read
        assert_eq!(r.tokens_out, 12); // last message_delta wins
        assert_eq!(r.tokens_cached, 5);
        assert_eq!(r.model, "claude-3-5-sonnet");
    }

    #[test]
    fn partial_buffer_from_aborted_stream_still_yields_partial_usage() {
        // Only message_start arrived before an abort -- output tokens are 0,
        // which is honest: we never got a delta.
        let sse = "data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-3-5-haiku\",\"usage\":{\"input_tokens\":30}}}\n\n";
        let r = extract_stream_result(Provider::Anthropic, sse, "fallback");
        assert_eq!(r.tokens_in, 30);
        assert_eq!(r.tokens_out, 0);
        assert_eq!(r.model, "claude-3-5-haiku");
    }

    #[test]
    fn wants_stream_detects_flag() {
        assert!(wants_stream(&serde_json::json!({"stream": true})));
        assert!(!wants_stream(&serde_json::json!({"stream": false})));
        assert!(!wants_stream(&serde_json::json!({})));
    }

    #[test]
    fn stream_result_accumulates_openai_text() {
        let sse = concat!(
            "data: {\"id\":\"1\",\"model\":\"gpt-4o\",\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"Hel\"}}]}\n\n",
            "data: {\"id\":\"1\",\"model\":\"gpt-4o\",\"choices\":[{\"delta\":{\"content\":\"lo\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"id\":\"1\",\"model\":\"gpt-4o\",\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\n",
            "data: [DONE]\n\n",
        );
        let r = extract_stream_result(Provider::Openai, sse, "");
        assert_eq!((r.tokens_in, r.tokens_out), (10, 5));
        let a = r.answer.expect("should have a cacheable answer");
        assert_eq!(a.text, "Hello");
        assert_eq!(a.finish_reason, "stop");
    }

    #[test]
    fn stream_result_openai_tool_call_is_never_cached() {
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"let me check\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"1\"}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
        );
        let r = extract_stream_result(Provider::Openai, sse, "gpt-4o");
        assert!(r.answer.is_none());
    }

    #[test]
    fn stream_result_accumulates_anthropic_text() {
        let sse = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-3-5-sonnet\",\"usage\":{\"input_tokens\":20}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi there\"}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\n",
        );
        let r = extract_stream_result(Provider::Anthropic, sse, "");
        assert_eq!((r.tokens_in, r.tokens_out), (20, 3));
        let a = r.answer.expect("should have a cacheable answer");
        assert_eq!(a.text, "Hi there");
        assert_eq!(a.finish_reason, "stop");
    }

    #[test]
    fn stream_result_anthropic_tool_use_is_never_cached() {
        let sse = concat!(
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"lookup\"}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":1}}\n\n",
        );
        let r = extract_stream_result(Provider::Anthropic, sse, "claude-3-5-sonnet");
        assert!(r.answer.is_none());
    }
}
