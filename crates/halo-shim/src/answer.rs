//! Provider-agnostic extraction and re-rendering of a plain-text assistant
//! answer.
//!
//! Used by two features that both need to replay a previously-seen answer
//! under a *different* envelope than the one it was originally produced in:
//! serving a streaming request from a buffered exact-match cache entry, and
//! the semantic cache (whose entire point is that the request being served
//! may hit a different provider/model than the one that originally answered).
//!
//! The hard rule throughout this module: only ever capture/replay a clean
//! plain-text answer, never a tool call. A response containing a tool call is
//! a "do not cache" signal -- replaying stored text in its place would
//! silently drop the tool call and corrupt the agent's control flow. Every
//! extractor below is written to fail closed (return `None`) on anything it
//! isn't confident is plain text, rather than guess.

use halo_common::telemetry::Provider;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnswerExtract {
    pub text: String,
    /// Normalized: "stop" | "length" | "content_filter" | "other".
    pub finish_reason: String,
}

/// Extract a cacheable plain-text answer from a buffered (non-streamed) JSON
/// response body. `None` means "don't cache this" -- tool call, empty/refusal
/// shape, or a content-block type we don't understand.
pub fn from_buffered(provider: Provider, json: &Value) -> Option<AnswerExtract> {
    match provider {
        Provider::Anthropic => from_anthropic_buffered(json),
        _ => from_openai_buffered(json),
    }
}

fn from_openai_buffered(json: &Value) -> Option<AnswerExtract> {
    let choice = json.get("choices")?.as_array()?.first()?;
    let message = choice.get("message")?;
    if message
        .get("tool_calls")
        .map(|v| !v.is_null() && v.as_array().map(|a| !a.is_empty()).unwrap_or(true))
        .unwrap_or(false)
    {
        return None;
    }
    let text = message.get("content")?.as_str()?;
    if text.is_empty() {
        return None;
    }
    let finish = choice
        .get("finish_reason")
        .and_then(|f| f.as_str())
        .unwrap_or("stop");
    Some(AnswerExtract {
        text: text.to_string(),
        finish_reason: normalize_finish(finish),
    })
}

fn from_anthropic_buffered(json: &Value) -> Option<AnswerExtract> {
    let blocks = json.get("content")?.as_array()?;
    if blocks.is_empty() {
        return None;
    }
    let mut text = String::new();
    for b in blocks {
        match b.get("type").and_then(|t| t.as_str()) {
            Some("text") => text.push_str(b.get("text").and_then(|t| t.as_str()).unwrap_or("")),
            // tool_use, thinking, redacted_thinking, image, ...: not safe to
            // collapse into plain text. Bail out of the whole answer rather
            // than silently drop a block.
            _ => return None,
        }
    }
    if text.is_empty() {
        return None;
    }
    let stop = json
        .get("stop_reason")
        .and_then(|s| s.as_str())
        .unwrap_or("end_turn");
    Some(AnswerExtract {
        text,
        finish_reason: normalize_finish(stop),
    })
}

pub fn normalize_finish(s: &str) -> String {
    match s {
        "stop" | "end_turn" | "stop_sequence" => "stop",
        "length" | "max_tokens" => "length",
        "content_filter" => "content_filter",
        _ => "other",
    }
    .to_string()
}

fn short_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_string()
}

/// Render a stored answer as an OpenAI chat-completion JSON body, addressed
/// to whatever model name the *current* request asked for (which may differ
/// from the model that originally produced the text).
pub fn render_openai_chat(answer: &AnswerExtract, model: &str, tokens_in: u64, tokens_out: u64) -> Value {
    let finish = match answer.finish_reason.as_str() {
        "length" => "length",
        "content_filter" => "content_filter",
        _ => "stop",
    };
    serde_json::json!({
        "id": format!("chatcmpl-halo-{}", short_id()),
        "object": "chat.completion",
        "created": chrono::Utc::now().timestamp(),
        "model": model,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": answer.text},
            "finish_reason": finish
        }],
        "usage": {
            "prompt_tokens": tokens_in,
            "completion_tokens": tokens_out,
            "total_tokens": tokens_in + tokens_out
        }
    })
}

pub fn render_anthropic_message(answer: &AnswerExtract, model: &str, tokens_in: u64, tokens_out: u64) -> Value {
    let stop = match answer.finish_reason.as_str() {
        "length" => "max_tokens",
        _ => "end_turn",
    };
    serde_json::json!({
        "id": format!("msg_halo_{}", short_id()),
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": [{"type": "text", "text": answer.text}],
        "stop_reason": stop,
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": tokens_in, "output_tokens": tokens_out}
    })
}

/// Render as a single-shot SSE stream. Intentionally simple -- one content
/// chunk, not a simulated token-by-token stream. The goal is giving a
/// stream-expecting client a stream-shaped response it can parse correctly,
/// not reproducing generation pacing for an answer that isn't being
/// generated.
pub fn render_openai_stream_sse(answer: &AnswerExtract, model: &str, tokens_in: u64, tokens_out: u64) -> String {
    let id = format!("chatcmpl-halo-{}", short_id());
    let finish = match answer.finish_reason.as_str() {
        "length" => "length",
        "content_filter" => "content_filter",
        _ => "stop",
    };
    let delta_chunk = serde_json::json!({
        "id": id, "object": "chat.completion.chunk", "model": model,
        "choices": [{"index": 0, "delta": {"role": "assistant", "content": answer.text}, "finish_reason": Value::Null}]
    });
    let stop_chunk = serde_json::json!({
        "id": id, "object": "chat.completion.chunk", "model": model,
        "choices": [{"index": 0, "delta": {}, "finish_reason": finish}]
    });
    let usage_chunk = serde_json::json!({
        "id": id, "object": "chat.completion.chunk", "model": model, "choices": [],
        "usage": {"prompt_tokens": tokens_in, "completion_tokens": tokens_out, "total_tokens": tokens_in + tokens_out}
    });
    format!("data: {delta_chunk}\n\ndata: {stop_chunk}\n\ndata: {usage_chunk}\n\ndata: [DONE]\n\n")
}

pub fn render_anthropic_stream_sse(answer: &AnswerExtract, model: &str, tokens_in: u64, tokens_out: u64) -> String {
    let stop = match answer.finish_reason.as_str() {
        "length" => "max_tokens",
        _ => "end_turn",
    };
    let message_start = serde_json::json!({
        "type": "message_start",
        "message": {
            "id": format!("msg_halo_{}", short_id()), "type": "message", "role": "assistant",
            "model": model, "content": [], "stop_reason": Value::Null, "stop_sequence": Value::Null,
            "usage": {"input_tokens": tokens_in, "output_tokens": 0}
        }
    });
    let block_start = serde_json::json!({
        "type": "content_block_start", "index": 0,
        "content_block": {"type": "text", "text": ""}
    });
    let block_delta = serde_json::json!({
        "type": "content_block_delta", "index": 0,
        "delta": {"type": "text_delta", "text": answer.text}
    });
    let block_stop = serde_json::json!({"type": "content_block_stop", "index": 0});
    let message_delta = serde_json::json!({
        "type": "message_delta",
        "delta": {"stop_reason": stop, "stop_sequence": Value::Null},
        "usage": {"output_tokens": tokens_out}
    });
    let message_stop = serde_json::json!({"type": "message_stop"});
    format!(
        "event: message_start\ndata: {message_start}\n\n\
         event: content_block_start\ndata: {block_start}\n\n\
         event: content_block_delta\ndata: {block_delta}\n\n\
         event: content_block_stop\ndata: {block_stop}\n\n\
         event: message_delta\ndata: {message_delta}\n\n\
         event: message_stop\ndata: {message_stop}\n\n"
    )
}

/// Dispatch to the right envelope for a buffered response.
pub fn render_buffered(provider: Provider, answer: &AnswerExtract, model: &str, tokens_in: u64, tokens_out: u64) -> Value {
    match provider {
        Provider::Anthropic => render_anthropic_message(answer, model, tokens_in, tokens_out),
        _ => render_openai_chat(answer, model, tokens_in, tokens_out),
    }
}

/// Dispatch to the right SSE envelope for a streaming request.
pub fn render_stream(provider: Provider, answer: &AnswerExtract, model: &str, tokens_in: u64, tokens_out: u64) -> String {
    match provider {
        Provider::Anthropic => render_anthropic_stream_sse(answer, model, tokens_in, tokens_out),
        _ => render_openai_stream_sse(answer, model, tokens_in, tokens_out),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_plain_text_extracts() {
        let j = serde_json::json!({
            "choices": [{"message": {"role":"assistant","content":"hi there"}, "finish_reason":"stop"}]
        });
        let a = from_buffered(Provider::Openai, &j).unwrap();
        assert_eq!(a.text, "hi there");
        assert_eq!(a.finish_reason, "stop");
    }

    #[test]
    fn openai_tool_call_is_never_cached() {
        let j = serde_json::json!({
            "choices": [{"message": {"role":"assistant","content": Value::Null, "tool_calls":[{"id":"1"}]}, "finish_reason":"tool_calls"}]
        });
        assert!(from_buffered(Provider::Openai, &j).is_none());
    }

    #[test]
    fn anthropic_plain_text_extracts() {
        let j = serde_json::json!({
            "content": [{"type":"text","text":"hello"}],
            "stop_reason": "end_turn"
        });
        let a = from_buffered(Provider::Anthropic, &j).unwrap();
        assert_eq!(a.text, "hello");
        assert_eq!(a.finish_reason, "stop");
    }

    #[test]
    fn anthropic_tool_use_is_never_cached() {
        let j = serde_json::json!({
            "content": [{"type":"text","text":"let me check"}, {"type":"tool_use","id":"t1","name":"lookup","input":{}}],
            "stop_reason": "tool_use"
        });
        assert!(from_buffered(Provider::Anthropic, &j).is_none());
    }

    #[test]
    fn round_trip_openai_render_is_extractable_shape() {
        let a = AnswerExtract { text: "42".into(), finish_reason: "stop".into() };
        let rendered = render_openai_chat(&a, "gpt-4o", 10, 3);
        let back = from_buffered(Provider::Openai, &rendered).unwrap();
        assert_eq!(back.text, "42");
    }

    #[test]
    fn round_trip_anthropic_render_is_extractable_shape() {
        let a = AnswerExtract { text: "42".into(), finish_reason: "stop".into() };
        let rendered = render_anthropic_message(&a, "claude-3-5-sonnet", 10, 3);
        let back = from_buffered(Provider::Anthropic, &rendered).unwrap();
        assert_eq!(back.text, "42");
    }

    #[test]
    fn stream_sse_contains_full_text_and_done() {
        let a = AnswerExtract { text: "hello world".into(), finish_reason: "stop".into() };
        let sse = render_openai_stream_sse(&a, "gpt-4o", 5, 2);
        assert!(sse.contains("hello world"));
        assert!(sse.ends_with("data: [DONE]\n\n"));

        let sse2 = render_anthropic_stream_sse(&a, "claude-3-5-sonnet", 5, 2);
        assert!(sse2.contains("hello world"));
        assert!(sse2.contains("message_stop"));
    }
}
