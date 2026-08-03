//! L2 semantic (embedding-similarity) cache.
//!
//! What this fixes relative to a naive "embed everything and kNN it"
//! approach (and relative to gaps found in the main Smartflow proxy's
//! semantic cache during Halo's design review):
//!
//!   1. **Always cosine re-checks a candidate before serving it.** Cheap
//!      keyword partitions (below) only *narrow the search*; they never
//!      decide a hit on their own. No "first key in the bucket wins".
//!   2. **Cross-provider/cross-model by design, not by accident.** The
//!      partition key deliberately excludes provider and model -- a stored
//!      answer is content, not a `(provider, model)`-shaped blob -- so a
//!      question answered once via Anthropic can serve a semantically
//!      similar question routed to OpenAI later. The response is always
//!      *re-rendered* into the requesting endpoint's own JSON shape
//!      (`answer::render_buffered` / `answer::render_stream`), never
//!      replayed as a raw stored HTTP body of a different shape.
//!   3. **Tool calls and structured output are excluded entirely.** See
//!      [`eligible_query`].
//!   4. **No local model.** Vectors come from an HTTP call to an embeddings
//!      API (`embeddings.rs`); this module only does arithmetic on the
//!      resulting `Vec<f32>`.
//!
//! Storage is a flat redb table scanned in full on every lookup, filtered to
//! the matching partition before the cosine check. That's a deliberate
//! simplicity choice, not an oversight: this is a *local, single-shim* cache
//! bounded by `max_entries` (default in the low hundreds), not a fleet-scale
//! vector index. Brute-force cosine over a few hundred small vectors is
//! sub-millisecond; pulling in an ANN crate (hnsw_rs et al.) to solve a
//! problem this small would be exactly the kind of unwarranted weight Halo
//! exists to avoid.

use crate::answer::AnswerExtract;
use anyhow::Result;
use halo_common::telemetry::Provider;
use redb::{Database, ReadableTable, ReadableTableMetadata, TableDefinition};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Arc;

const TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("semantic_entries");

/// Below this many normalized characters, an embedding call costs more in
/// latency/API spend than the exchange is worth caching, and short prompts
/// ("hi", "ok", "yes") are also the likeliest to collide across unrelated
/// contexts.
const MIN_QUERY_CHARS: usize = 12;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticEntry {
    pub embedding: Vec<f32>,
    pub partition: String,
    pub answer: AnswerExtract,
    pub origin_provider: Provider,
    pub origin_model: String,
    pub tokens_out: u64,
    pub created_at: i64,
}

pub struct SemanticCacheStore {
    db: Database,
    max_entries: u64,
}

impl SemanticCacheStore {
    pub fn open(path: &Path, max_entries: u64) -> Result<Arc<Self>> {
        let db = Database::create(path)?;
        {
            let w = db.begin_write()?;
            {
                let _ = w.open_table(TABLE)?;
            }
            w.commit()?;
        }
        Ok(Arc::new(Self {
            db,
            max_entries: max_entries.max(1),
        }))
    }

    /// Best match within `partition` at or above `threshold`, or `None`.
    /// Always re-checks cosine similarity against the live query vector --
    /// the partition only narrows candidates, it never substitutes for the
    /// similarity check.
    pub fn lookup(&self, partition: &str, query_vec: &[f32], threshold: f32) -> Result<Option<(SemanticEntry, f32)>> {
        let rtxn = self.db.begin_read()?;
        let table = rtxn.open_table(TABLE)?;
        let mut best: Option<(SemanticEntry, f32)> = None;
        for row in table.iter()? {
            let (_, v) = row?;
            let Ok(entry) = serde_json::from_slice::<SemanticEntry>(v.value()) else {
                continue;
            };
            if entry.partition != partition {
                continue;
            }
            let sim = cosine(query_vec, &entry.embedding);
            if sim >= threshold && best.as_ref().map(|(_, s)| sim > *s).unwrap_or(true) {
                best = Some((entry, sim));
            }
        }
        Ok(best)
    }

    pub fn store(&self, key: &str, entry: &SemanticEntry) -> Result<()> {
        let bytes = serde_json::to_vec(entry)?;
        let wtxn = self.db.begin_write()?;
        {
            let mut table = wtxn.open_table(TABLE)?;
            let present = table.get(key)?.is_some();
            let len = table.len()?;
            if !present && len >= self.max_entries {
                let mut aged: Vec<(i64, String)> = Vec::new();
                for row in table.iter()? {
                    let (k, v) = row?;
                    let created = serde_json::from_slice::<SemanticEntry>(v.value())
                        .map(|e| e.created_at)
                        .unwrap_or(0);
                    aged.push((created, k.value().to_string()));
                }
                aged.sort_by_key(|(c, _)| *c);
                let target = (self.max_entries * 9 / 10).max(1);
                let remove_n = len.saturating_sub(target) + 1;
                for (_, k) in aged.into_iter().take(remove_n as usize) {
                    table.remove(k.as_str())?;
                }
            }
            table.insert(key, bytes.as_slice())?;
        }
        wtxn.commit()?;
        Ok(())
    }

    #[allow(dead_code)] // used by tests and `halo status` diagnostics
    pub fn len(&self) -> Result<u64> {
        let rtxn = self.db.begin_read()?;
        let table = rtxn.open_table(TABLE)?;
        Ok(table.len()?)
    }
}

/// Cosine similarity in [-1, 1]; 0 for mismatched lengths or a zero vector.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0f32;
    let mut na = 0f32;
    let mut nb = 0f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// A request judged eligible for the semantic cache: its normalized query
/// text (what gets embedded) and its partition key (what narrows the scan).
pub struct SemanticQuery {
    pub query_text: String,
    pub partition: String,
}

/// Decide whether a request may use the semantic cache at all, and if so,
/// compute what to embed and which partition to search/store into.
///
/// Returns `None` (skip the semantic cache entirely, fall through to a live
/// call) when:
///   * `tools`/`functions` are present -- a similar-but-not-identical prompt
///     with tool-calling enabled may need a *different* tool call; replaying
///     free text in its place is unsafe.
///   * `response_format` requests anything other than plain text (JSON mode,
///     JSON schema, etc.) -- a cached free-text answer may not conform.
///   * the message history has more than one user turn, or contains any
///     assistant/tool message -- i.e. this isn't a fresh, single-shot
///     question. Multi-turn semantic matching needs the full conversation
///     embedded and a materially higher bar for false positives; deferred
///     rather than shipped half-safe (see docs/DESIGN_REVIEW.md v1.2 notes).
///   * the normalized query is shorter than [`MIN_QUERY_CHARS`].
pub fn eligible_query(json: &Value) -> Option<SemanticQuery> {
    if has_nonempty_array(json, "tools") || has_nonempty_array(json, "functions") {
        return None;
    }
    if let Some(rf) = json.get("response_format") {
        let kind = rf.get("type").and_then(|t| t.as_str()).unwrap_or("text");
        if kind != "text" {
            return None;
        }
    }

    let messages = json.get("messages").and_then(|m| m.as_array())?;
    let mut system_text = json.get("system").map(crate::cachekey::collect_text).unwrap_or_default();
    let mut user_text: Option<String> = None;
    for m in messages {
        let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
        let content = m.get("content").cloned().unwrap_or(Value::Null);
        match role {
            "system" | "developer" => system_text.push_str(&crate::cachekey::collect_text(&content)),
            "user" => {
                if user_text.is_some() {
                    return None; // more than one user turn => multi-turn
                }
                user_text = Some(crate::cachekey::collect_text(&content));
            }
            // assistant / tool / anything else: this is a multi-turn thread
            // or a tool-result turn -- out of scope for v1.1.
            _ => return None,
        }
    }
    let user_text = user_text?;
    let normalized = crate::cachekey::normalize_query_text(&user_text);
    if normalized.chars().count() < MIN_QUERY_CHARS {
        return None;
    }

    let stage = classify_stage(&normalized);
    let intent = classify_intent(&normalized);
    let system_hash = short_hash(&crate::cachekey::normalize_query_text(&system_text));
    let partition = format!("{stage}:{intent}:{system_hash}");

    Some(SemanticQuery {
        query_text: normalized,
        partition,
    })
}

fn has_nonempty_array(json: &Value, field: &str) -> bool {
    json.get(field)
        .and_then(|v| v.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false)
}

fn short_hash(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    format!("{:x}", h.finalize())[..12].to_string()
}

/// Coarse conversational stage. English-keyword-based like the main proxy's
/// classifier -- a cheap partition, not the safety mechanism (cosine
/// similarity is). CJK/non-English queries fall through to "primary", which
/// is safe (just a bigger candidate set within the same intent bucket) not
/// incorrect.
fn classify_stage(q: &str) -> &'static str {
    let t = q.trim();
    if t.len() < 24
        && (t.starts_with("hi") || t.starts_with("hello") || t.starts_with("hey") || t.starts_with("thanks") || t.starts_with("thank you"))
    {
        return "greeting";
    }
    if t.starts_with("can you clarify")
        || t.starts_with("what do you mean")
        || t.starts_with("could you explain that")
        || t.starts_with("sorry i meant")
    {
        return "clarification";
    }
    if t.starts_with("continue") || t.starts_with("go on") || t.starts_with("keep going") {
        return "continuation";
    }
    "primary"
}

/// Coarse query intent, same rationale as [`classify_stage`].
fn classify_intent(q: &str) -> &'static str {
    if q.contains(" vs ") || q.contains(" versus ") || q.contains("compare") || q.contains("difference between") {
        return "comparison";
    }
    if q.starts_with("what is") || q.starts_with("what are") || q.starts_with("define") || q.contains("what does") {
        return "definition";
    }
    if q.contains("error")
        || q.contains("doesn't work")
        || q.contains("not working")
        || q.contains(" fix ")
        || q.starts_with("fix ")
        || q.contains("debug")
        || q.contains(" bug")
    {
        return "troubleshooting";
    }
    if q.starts_with("how do i") || q.starts_with("how to") || q.starts_with("how can i") {
        return "instruction";
    }
    if q.contains("should i") || q.contains("recommend") || q.contains("which is better") || q.contains("best way") {
        return "recommendation";
    }
    if q.contains("evaluate") || q.contains("review this") || q.contains("assess") || q.contains("critique") {
        return "evaluation";
    }
    "general"
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn entry(text: &str, partition: &str, embedding: Vec<f32>) -> SemanticEntry {
        SemanticEntry {
            embedding,
            partition: partition.into(),
            answer: AnswerExtract {
                text: text.into(),
                finish_reason: "stop".into(),
            },
            origin_provider: Provider::Openai,
            origin_model: "gpt-4o".into(),
            tokens_out: 10,
            created_at: chrono::Utc::now().timestamp(),
        }
    }

    #[test]
    fn cosine_identical_vectors_is_one() {
        let v = vec![1.0, 2.0, 3.0];
        assert!((cosine(&v, &v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_orthogonal_is_zero() {
        assert!((cosine(&[1.0, 0.0], &[0.0, 1.0])).abs() < 1e-6);
    }

    #[test]
    fn lookup_never_returns_below_threshold_even_same_partition() {
        let tmp = TempDir::new().unwrap();
        let store = SemanticCacheStore::open(&tmp.path().join("s.redb"), 100).unwrap();
        store.store("k1", &entry("hi", "primary:general:abc", vec![1.0, 0.0])).unwrap();
        // Orthogonal query vector in the same partition: must NOT hit despite
        // being "the only candidate in the bucket" -- this is the fix for the
        // main proxy's "first bucket match wins" flaw.
        let got = store.lookup("primary:general:abc", &[0.0, 1.0], 0.90).unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn lookup_hits_above_threshold_in_same_partition() {
        let tmp = TempDir::new().unwrap();
        let store = SemanticCacheStore::open(&tmp.path().join("s.redb"), 100).unwrap();
        store.store("k1", &entry("hi", "primary:general:abc", vec![1.0, 0.0])).unwrap();
        let got = store.lookup("primary:general:abc", &[0.99, 0.01], 0.90).unwrap();
        assert!(got.is_some());
    }

    #[test]
    fn lookup_ignores_other_partitions() {
        let tmp = TempDir::new().unwrap();
        let store = SemanticCacheStore::open(&tmp.path().join("s.redb"), 100).unwrap();
        store.store("k1", &entry("hi", "primary:general:abc", vec![1.0, 0.0])).unwrap();
        let got = store.lookup("primary:troubleshooting:abc", &[1.0, 0.0], 0.90).unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn eligible_rejects_tools() {
        let j = serde_json::json!({
            "messages": [{"role":"user","content":"what is the weather in nyc today"}],
            "tools": [{"type":"function","function":{"name":"get_weather"}}]
        });
        assert!(eligible_query(&j).is_none());
    }

    #[test]
    fn eligible_rejects_json_mode() {
        let j = serde_json::json!({
            "messages": [{"role":"user","content":"give me a json list of colors"}],
            "response_format": {"type": "json_object"}
        });
        assert!(eligible_query(&j).is_none());
    }

    #[test]
    fn eligible_rejects_multiturn_history() {
        let j = serde_json::json!({
            "messages": [
                {"role":"user","content":"what is redis"},
                {"role":"assistant","content":"an in-memory data store"},
                {"role":"user","content":"and what about postgres"}
            ]
        });
        assert!(eligible_query(&j).is_none());
    }

    #[test]
    fn eligible_rejects_short_prompts() {
        let j = serde_json::json!({"messages": [{"role":"user","content":"hi"}]});
        assert!(eligible_query(&j).is_none());
    }

    #[test]
    fn eligible_accepts_single_turn_with_system() {
        let j = serde_json::json!({
            "messages": [
                {"role":"system","content":"You are a helpful assistant."},
                {"role":"user","content":"What is the capital of France?"}
            ]
        });
        let q = eligible_query(&j).unwrap();
        assert_eq!(q.query_text, "what is the capital of france");
    }

    #[test]
    fn eligible_accepts_anthropic_shape_with_top_level_system() {
        let j = serde_json::json!({
            "system": "You are a helpful assistant.",
            "messages": [{"role":"user","content":"What is the capital of France?"}]
        });
        let q = eligible_query(&j).unwrap();
        assert!(q.partition.contains("definition"));
    }

    #[test]
    fn different_system_prompt_yields_different_partition() {
        let a = serde_json::json!({
            "system": "You are a pirate.",
            "messages": [{"role":"user","content":"What is the capital of France?"}]
        });
        let b = serde_json::json!({
            "system": "You are a formal assistant.",
            "messages": [{"role":"user","content":"What is the capital of France?"}]
        });
        let qa = eligible_query(&a).unwrap();
        let qb = eligible_query(&b).unwrap();
        assert_ne!(qa.partition, qb.partition);
    }

    #[test]
    fn same_system_prompt_same_partition_regardless_of_provider_or_model() {
        // The partition intentionally has no provider/model component -- this
        // is what makes cross-provider reuse possible.
        let a = serde_json::json!({
            "model": "gpt-4o",
            "system": "You are a helpful assistant.",
            "messages": [{"role":"user","content":"What is the capital of France?"}]
        });
        let b = serde_json::json!({
            "model": "claude-3-5-sonnet",
            "system": "You are a helpful assistant.",
            "messages": [{"role":"user","content":"What is the capital of France?"}]
        });
        let qa = eligible_query(&a).unwrap();
        let qb = eligible_query(&b).unwrap();
        assert_eq!(qa.partition, qb.partition);
    }
}
