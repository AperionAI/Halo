//! Anthropic `cache_control` breakpoint injection.
//!
//! Adapted from `src/prompt_cache_injector.rs`, and extended beyond its
//! system-prompt-only scope (confirmed via review: the main proxy covers
//! neither tool definitions nor message content) to also cover:
//!
//!   * **`tools` definitions.** Often the largest, most stable block in an
//!     agent's request -- unchanged turn over turn within a session, and
//!     Anthropic tool objects accept `cache_control` directly, same as a
//!     content block.
//!   * **The first message's attachment-shaped content.** Agent loops
//!     conventionally put a large, stable block -- a pasted document, a
//!     screenshot, a RAG context dump -- ahead of the per-turn question in
//!     the first user turn. That's exactly the "repetitive data or
//!     attachments" case worth a breakpoint: it recurs across many calls in
//!     the same session while the trailing question changes each time. The
//!     breakpoint is placed on the second-to-last block, not the last one --
//!     the last block is treated as the dynamic per-turn part and is
//!     deliberately excluded (a breakpoint that includes it would never
//!     reuse across two different questions; caught via live smoke test, see
//!     `stable_prefix_len` below).
//!
//! Anthropic allows up to 4 `cache_control` breakpoints per request; Halo
//! uses at most 3 (system, tools, first-message), leaving headroom for a
//! caller that already sets its own.
//!
//! The only other substantive change from the main proxy's version is
//! dropping the Redis-backed "seen count" for an in-memory counter -- v1 has
//! no shared L2 to justify Redis locally, and the injector only needs a
//! per-process repetition signal. OpenAI caches automatically, so there's
//! nothing to inject there; Halo only parses `cached_tokens` off the
//! response (see `ingress.rs`/`streaming.rs`).

use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const MIN_CACHEABLE_CHARS: usize = 4_000;
const REPETITION_CACHEABLE_CHARS: usize = 2_000;
const REPETITION_THRESHOLD: u32 = 3;
const MEMORY_TTL: Duration = Duration::from_secs(900); // 15 min

struct MemEntry {
    count: u32,
    first_seen: Instant,
}

/// Tracks how often each candidate block's hash has been seen this process,
/// and decides whether each qualifies for an Anthropic ephemeral cache
/// breakpoint. One instance is shared across all requests.
#[derive(Default)]
pub struct CacheControlInjector {
    seen: Mutex<HashMap<String, MemEntry>>,
}

pub struct Injected {
    pub body: String,
    /// How many cache_control breakpoints this call added (1-3).
    pub breakpoints: u32,
    /// Whether at least one breakpoint fired on repetition (seen 3+ times
    /// this process) rather than pure size -- i.e. this is a block Halo has
    /// concrete evidence is actually being reused, not just one that
    /// happens to be big on its first appearance.
    pub warm: bool,
}

impl CacheControlInjector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Inject `cache_control` breakpoints on the Anthropic system block,
    /// `tools` definitions, and the first message's attachment-shaped
    /// content, wherever each is large enough or smaller-but-repeated.
    /// Returns `None` when nothing changed.
    pub fn process_anthropic(&self, body: &str) -> Option<Injected> {
        let mut json: Value = serde_json::from_str(body).ok()?;
        let mut breakpoints = 0u32;
        let mut warm = false;

        if let Some(w) = self.maybe_inject(&mut json, "sys", system_char_len, hash_system, inject_system) {
            breakpoints += 1;
            warm |= w;
        }
        if let Some(w) = self.maybe_inject(&mut json, "tools", tools_char_len, hash_tools, inject_last_tool) {
            breakpoints += 1;
            warm |= w;
        }
        if let Some(w) = self.maybe_inject(
            &mut json,
            "msg0",
            first_message_char_len,
            hash_first_message,
            inject_first_message,
        ) {
            breakpoints += 1;
            warm |= w;
        }

        if breakpoints == 0 {
            return None;
        }
        Some(Injected {
            body: serde_json::to_string(&json).ok()?,
            breakpoints,
            warm,
        })
    }

    /// Shared decision + bookkeeping for one candidate block: measure it,
    /// track how often this exact block has recurred, decide whether it
    /// clears the size/repetition bar, and if so apply `inject`. Returns
    /// `Some(warm)` iff a breakpoint was actually added.
    fn maybe_inject(
        &self,
        json: &mut Value,
        namespace: &str,
        len_fn: impl Fn(&Value) -> usize,
        hash_fn: impl Fn(&Value) -> String,
        inject_fn: impl Fn(&mut Value) -> bool,
    ) -> Option<bool> {
        let len = len_fn(json);
        if len < REPETITION_CACHEABLE_CHARS {
            return None;
        }
        let count = self.increment(&format!("{namespace}:{}", hash_fn(json)));
        let warm = count > 1;
        let should = len >= MIN_CACHEABLE_CHARS || (len >= REPETITION_CACHEABLE_CHARS && count >= REPETITION_THRESHOLD);
        if !should {
            return None;
        }
        if inject_fn(json) {
            Some(warm)
        } else {
            None
        }
    }

    fn increment(&self, key: &str) -> u32 {
        let mut map = self.seen.lock().unwrap();
        map.retain(|_, e| e.first_seen.elapsed() < MEMORY_TTL);
        let e = map.entry(key.to_string()).or_insert(MemEntry {
            count: 0,
            first_seen: Instant::now(),
        });
        e.count += 1;
        e.count
    }
}

fn sha256_hex(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    format!("{:x}", h.finalize())
}

/// Length of a single content block, generically across shapes: a `text`
/// block, or an `image`/`document` block's base64 `source.data` (the part
/// that actually drives token/byte cost; a `source.url` reference is tiny by
/// comparison and not worth a breakpoint on its own).
fn block_char_len(block: &Value) -> usize {
    if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
        return t.len();
    }
    if let Some(data) = block.pointer("/source/data").and_then(|v| v.as_str()) {
        return data.len();
    }
    0
}

fn system_char_len(json: &Value) -> usize {
    match json.get("system") {
        Some(Value::String(s)) => s.len(),
        Some(Value::Array(blocks)) => blocks.iter().map(block_char_len).sum(),
        _ => 0,
    }
}

fn hash_system(json: &Value) -> String {
    let s = match json.get("system") {
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
        None => return String::new(),
    };
    sha256_hex(&s)
}

/// Returns true if the body was modified.
fn inject_system(json: &mut Value) -> bool {
    let system = match json.get_mut("system") {
        Some(s) => s,
        None => return false,
    };
    match system {
        Value::String(s) => {
            let text = s.clone();
            *system = serde_json::json!([{
                "type": "text",
                "text": text,
                "cache_control": { "type": "ephemeral" }
            }]);
            true
        }
        Value::Array(blocks) => {
            if blocks.is_empty() {
                return false;
            }
            if blocks.last().and_then(|b| b.get("cache_control")).is_some() {
                return false;
            }
            if let Some(Value::Object(map)) = blocks.last_mut() {
                map.insert("cache_control".into(), serde_json::json!({ "type": "ephemeral" }));
                true
            } else {
                false
            }
        }
        _ => false,
    }
}

fn tools_char_len(json: &Value) -> usize {
    json.get("tools")
        .and_then(|t| t.as_array())
        .map(|arr| arr.iter().map(|t| serde_json::to_string(t).map(|s| s.len()).unwrap_or(0)).sum())
        .unwrap_or(0)
}

fn hash_tools(json: &Value) -> String {
    sha256_hex(&json.get("tools").map(|t| t.to_string()).unwrap_or_default())
}

/// Anthropic tool definitions carry `cache_control` the same way a content
/// block does; pinning the LAST tool in the list caches everything up to and
/// including it (Anthropic's documented breakpoint semantics).
fn inject_last_tool(json: &mut Value) -> bool {
    let tools = match json.get_mut("tools").and_then(|t| t.as_array_mut()) {
        Some(t) if !t.is_empty() => t,
        _ => return false,
    };
    let last = tools.last_mut().expect("checked non-empty above");
    if last.get("cache_control").is_some() {
        return false;
    }
    match last {
        Value::Object(map) => {
            map.insert("cache_control".into(), serde_json::json!({ "type": "ephemeral" }));
            true
        }
        _ => false,
    }
}

fn first_user_message(json: &Value) -> Option<&Value> {
    let msg = json.get("messages").and_then(|m| m.as_array())?.first()?;
    if msg.get("role").and_then(|r| r.as_str()) == Some("user") {
        Some(msg)
    } else {
        None
    }
}

fn first_user_message_mut(json: &mut Value) -> Option<&mut Value> {
    let msg = json.get_mut("messages").and_then(|m| m.as_array_mut())?.first_mut()?;
    if msg.get("role").and_then(|r| r.as_str()) == Some("user") {
        Some(msg)
    } else {
        None
    }
}

/// Requires >= 2 content blocks in the first user message: block conventions
/// like `[attachment, question]` put the stable, reusable part first and the
/// per-turn part last, so the LAST block is deliberately excluded from what
/// gets pinned -- caching through it would tie the breakpoint to content
/// that's expected to change every call, defeating the point (verified via
/// live smoke test: a breakpoint on the varying last block never gets reused
/// across turns; the fix is to cache everything through the second-to-last
/// block instead). A single-block first message has no separable stable
/// prefix and is skipped -- it's already covered by system/tools above if it
/// really is reused.
fn stable_prefix_len(blocks: &[Value]) -> usize {
    if blocks.len() < 2 {
        0
    } else {
        blocks.len() - 1
    }
}

fn first_message_char_len(json: &Value) -> usize {
    let Some(msg) = first_user_message(json) else {
        return 0;
    };
    let Some(Value::Array(blocks)) = msg.get("content") else {
        return 0;
    };
    let n = stable_prefix_len(blocks);
    blocks[..n].iter().map(block_char_len).sum()
}

fn hash_first_message(json: &Value) -> String {
    let Some(msg) = first_user_message(json) else {
        return String::new();
    };
    let Some(Value::Array(blocks)) = msg.get("content") else {
        return String::new();
    };
    let n = stable_prefix_len(blocks);
    if n == 0 {
        return String::new();
    }
    sha256_hex(&Value::Array(blocks[..n].to_vec()).to_string())
}

fn inject_first_message(json: &mut Value) -> bool {
    let Some(msg) = first_user_message_mut(json) else {
        return false;
    };
    let blocks = match msg.get_mut("content").and_then(|c| c.as_array_mut()) {
        Some(b) if b.len() >= 2 => b,
        _ => return false,
    };
    // Pin the last block of the STABLE prefix (second-to-last overall), not
    // the array's literal last block, which is the dynamic per-turn part.
    let idx = blocks.len() - 2;
    let target = &mut blocks[idx];
    if target.get("cache_control").is_some() {
        return false;
    }
    match target {
        Value::Object(map) => {
            map.insert("cache_control".into(), serde_json::json!({ "type": "ephemeral" }));
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn big_system() -> String {
        let sys = "x".repeat(5000);
        format!(r#"{{"model":"claude-3-5-sonnet","system":"{sys}","messages":[]}}"#)
    }

    #[test]
    fn large_system_injected_immediately() {
        let inj = CacheControlInjector::new();
        let out = inj.process_anthropic(&big_system()).unwrap();
        assert!(out.body.contains("cache_control"));
        assert!(out.body.contains("ephemeral"));
        assert_eq!(out.breakpoints, 1);
    }

    #[test]
    fn small_system_never_injected() {
        let inj = CacheControlInjector::new();
        let body = r#"{"model":"claude-3-5-sonnet","system":"short","messages":[]}"#;
        assert!(inj.process_anthropic(body).is_none());
    }

    #[test]
    fn large_tools_block_gets_its_own_breakpoint_on_last_tool() {
        let inj = CacheControlInjector::new();
        let desc = "d".repeat(4500);
        let body = serde_json::json!({
            "model": "claude-3-5-sonnet",
            "tools": [
                {"name": "search", "description": desc},
                {"name": "fetch", "description": "short"}
            ],
            "messages": [{"role": "user", "content": "hi there, what's up today"}]
        })
        .to_string();
        let out = inj.process_anthropic(&body).unwrap();
        let v: Value = serde_json::from_str(&out.body).unwrap();
        assert!(v["tools"][1].get("cache_control").is_some(), "breakpoint must land on the LAST tool");
        assert!(v["tools"][0].get("cache_control").is_none());
    }

    #[test]
    fn small_tools_block_not_injected() {
        let inj = CacheControlInjector::new();
        let body = serde_json::json!({
            "model": "claude-3-5-sonnet",
            "tools": [{"name": "search", "description": "short"}],
            "messages": []
        })
        .to_string();
        assert!(inj.process_anthropic(&body).is_none());
    }

    #[test]
    fn large_first_message_attachment_gets_breakpoint_on_stable_prefix_not_the_question() {
        let inj = CacheControlInjector::new();
        let doc = "y".repeat(4500);
        let body = serde_json::json!({
            "model": "claude-3-5-sonnet",
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": doc},
                    {"type": "text", "text": "what does this say"}
                ]}
            ]
        })
        .to_string();
        let out = inj.process_anthropic(&body).unwrap();
        let v: Value = serde_json::from_str(&out.body).unwrap();
        let blocks = v["messages"][0]["content"].as_array().unwrap();
        // The breakpoint must land on the STABLE block (the attachment), not
        // the dynamic last block (the question) -- a breakpoint on the part
        // that changes every call would never actually get reused.
        assert!(blocks[0].get("cache_control").is_some(), "breakpoint must land on the stable attachment block");
        assert!(blocks[1].get("cache_control").is_none(), "must NOT land on the varying trailing block");
    }

    #[test]
    fn single_block_first_message_has_no_separable_stable_prefix() {
        // No second block to treat as "the dynamic part" -- skip rather than
        // guess, even if the sole block is large.
        let inj = CacheControlInjector::new();
        let doc = "y".repeat(4500);
        let body = serde_json::json!({
            "model": "claude-3-5-sonnet",
            "messages": [{"role": "user", "content": [{"type": "text", "text": doc}]}]
        })
        .to_string();
        assert!(inj.process_anthropic(&body).is_none());
    }

    #[test]
    fn plain_string_first_message_has_no_attachment_to_pin() {
        let inj = CacheControlInjector::new();
        let body = serde_json::json!({
            "model": "claude-3-5-sonnet",
            "messages": [{"role": "user", "content": "just a normal short question"}]
        })
        .to_string();
        assert!(inj.process_anthropic(&body).is_none());
    }

    #[test]
    fn image_attachment_measured_by_base64_data_len() {
        let inj = CacheControlInjector::new();
        let data = "z".repeat(4200);
        let body = serde_json::json!({
            "model": "claude-3-5-sonnet",
            "messages": [
                {"role": "user", "content": [
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": data}},
                    {"type": "text", "text": "describe this screenshot"}
                ]}
            ]
        })
        .to_string();
        let out = inj.process_anthropic(&body).unwrap();
        assert!(out.body.contains("cache_control"));
    }

    #[test]
    fn small_repeated_first_message_becomes_warm_after_threshold() {
        let inj = CacheControlInjector::new();
        let ctx = "c".repeat(2500); // above repetition floor, below unconditional floor
        let body = serde_json::json!({
            "model": "claude-3-5-sonnet",
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": ctx},
                    {"type": "text", "text": "question one"}
                ]}
            ]
        })
        .to_string();
        assert!(inj.process_anthropic(&body).is_none(), "1st sighting: below repetition threshold");
        assert!(inj.process_anthropic(&body).is_none(), "2nd sighting: still below threshold");
        let out = inj.process_anthropic(&body).unwrap();
        assert!(out.warm, "3rd sighting: repetition threshold met, should be reported warm");
    }

    #[test]
    fn multiple_blocks_can_all_fire_in_one_request() {
        let inj = CacheControlInjector::new();
        let sys = "s".repeat(5000);
        let desc = "d".repeat(5000);
        let doc = "y".repeat(5000);
        let body = serde_json::json!({
            "model": "claude-3-5-sonnet",
            "system": sys,
            "tools": [{"name": "search", "description": desc}],
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": doc},
                    {"type": "text", "text": "question"}
                ]}
            ]
        })
        .to_string();
        let out = inj.process_anthropic(&body).unwrap();
        assert_eq!(out.breakpoints, 3);
    }

    #[test]
    fn already_present_cache_control_is_not_duplicated() {
        let inj = CacheControlInjector::new();
        let sys = "x".repeat(5000);
        let body = serde_json::json!({
            "model": "claude-3-5-sonnet",
            "system": [{"type": "text", "text": sys, "cache_control": {"type": "ephemeral"}}],
            "messages": []
        })
        .to_string();
        assert!(inj.process_anthropic(&body).is_none());
    }
}
