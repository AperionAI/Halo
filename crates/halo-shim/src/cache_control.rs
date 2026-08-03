//! Anthropic `cache_control` breakpoint injection.
//!
//! Adapted from `src/prompt_cache_injector.rs`. The only substantive change is
//! dropping the Redis-backed "seen count" for an in-memory counter -- v1 has no
//! shared L2 to justify Redis locally, and the injector only needs a per-
//! process repetition signal. OpenAI caches automatically, so there's nothing
//! to inject there; we only parse `cached_tokens` off the response elsewhere.

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

/// Tracks how often each system-prompt hash has been seen this process, and
/// decides whether to add an Anthropic ephemeral cache breakpoint.
#[derive(Default)]
pub struct CacheControlInjector {
    seen: Mutex<HashMap<String, MemEntry>>,
}

pub struct Injected {
    pub body: String,
    /// Whether the system block was seen before this request (warm cache).
    pub warm: bool,
}

impl CacheControlInjector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Inject `cache_control` on the Anthropic system block when it's large
    /// enough, or smaller but repeated. Returns `None` when nothing changed.
    pub fn process_anthropic(&self, body: &str) -> Option<Injected> {
        let mut json: Value = serde_json::from_str(body).ok()?;
        let sys_len = system_char_len(&json);
        if sys_len < REPETITION_CACHEABLE_CHARS {
            return None;
        }

        let hash = hash_system(&json);
        let count = self.increment(&hash);
        let warm = count > 1;

        let should = sys_len >= MIN_CACHEABLE_CHARS
            || (sys_len >= REPETITION_CACHEABLE_CHARS && count >= REPETITION_THRESHOLD);
        if !should {
            return None;
        }

        if !inject(&mut json) {
            return None;
        }
        Some(Injected {
            body: serde_json::to_string(&json).ok()?,
            warm,
        })
    }

    fn increment(&self, hash: &str) -> u32 {
        let mut map = self.seen.lock().unwrap();
        map.retain(|_, e| e.first_seen.elapsed() < MEMORY_TTL);
        let e = map.entry(hash.to_string()).or_insert(MemEntry {
            count: 0,
            first_seen: Instant::now(),
        });
        e.count += 1;
        e.count
    }
}

fn system_char_len(json: &Value) -> usize {
    match json.get("system") {
        Some(Value::String(s)) => s.len(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .map(|t| t.len())
            .sum(),
        _ => 0,
    }
}

fn hash_system(json: &Value) -> String {
    let s = match json.get("system") {
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
        None => return String::new(),
    };
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    format!("{:x}", h.finalize())
}

/// Returns true if the body was modified.
fn inject(json: &mut Value) -> bool {
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
                map.insert(
                    "cache_control".into(),
                    serde_json::json!({ "type": "ephemeral" }),
                );
                true
            } else {
                false
            }
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
    }

    #[test]
    fn small_system_never_injected() {
        let inj = CacheControlInjector::new();
        let body = r#"{"model":"claude-3-5-sonnet","system":"short","messages":[]}"#;
        assert!(inj.process_anthropic(body).is_none());
    }
}
