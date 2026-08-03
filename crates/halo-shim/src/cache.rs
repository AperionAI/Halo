//! Exact-match L1 response cache (redb-backed, hard entry cap).
//!
//! Deliberately exact-match only for v1 -- no embeddings, no semantic
//! similarity, no candle/HF/Ollama. The architecture analysis found cache
//! value is dominated by within-user repetition (fixed system prompts,
//! polling loops, repeated tool schemas), which exact-match already captures,
//! and the embedding path is the heaviest, least production-ready part of the
//! main proxy's cache stack.
//!
//! The entry cap is enforced from the first commit (a lesson-learned: the main
//! proxy's L1 grew unbounded and needed `METACACHE_L1_MAX_ENTRIES` bolted on
//! after the fact). When full we batch-evict the oldest ~10% so eviction cost
//! is amortized, not paid on every insert.

use anyhow::Result;
use redb::{Database, ReadableTable, ReadableTableMetadata, TableDefinition};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;

const TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("responses");

/// A cached upstream response plus the metadata needed to emit honest
/// telemetry on a hit (so a cache hit still counts toward savings).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntry {
    pub status: u16,
    pub content_type: String,
    pub body: String,
    pub model: String,
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// Unix seconds; used for oldest-first eviction.
    pub created_at: i64,
}

pub struct CacheStore {
    db: Database,
    max_entries: u64,
    enabled: bool,
}

impl CacheStore {
    pub fn open(path: &Path, max_entries: u64, enabled: bool) -> Result<Arc<Self>> {
        let db = Database::create(path)?;
        // Ensure the table exists so first read doesn't error.
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
            enabled,
        }))
    }

    pub fn get(&self, key: &str) -> Result<Option<CacheEntry>> {
        if !self.enabled {
            return Ok(None);
        }
        let rtxn = self.db.begin_read()?;
        let table = rtxn.open_table(TABLE)?;
        match table.get(key)? {
            Some(v) => Ok(serde_json::from_slice(v.value()).ok()),
            None => Ok(None),
        }
    }

    pub fn put(&self, key: &str, entry: &CacheEntry) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let bytes = serde_json::to_vec(entry)?;
        let wtxn = self.db.begin_write()?;
        {
            let mut table = wtxn.open_table(TABLE)?;

            let present = { table.get(key)?.is_some() };
            let len = table.len()?;

            if !present && len >= self.max_entries {
                let mut aged: Vec<(i64, String)> = Vec::new();
                {
                    for row in table.iter()? {
                        let (k, v) = row?;
                        let created = serde_json::from_slice::<CacheEntry>(v.value())
                            .map(|e| e.created_at)
                            .unwrap_or(0);
                        aged.push((created, k.value().to_string()));
                    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn entry(body: &str) -> CacheEntry {
        CacheEntry {
            status: 200,
            content_type: "application/json".into(),
            body: body.into(),
            model: "gpt-4o".into(),
            tokens_in: 10,
            tokens_out: 20,
            created_at: chrono::Utc::now().timestamp(),
        }
    }

    #[test]
    fn put_get_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let c = CacheStore::open(&tmp.path().join("c.redb"), 100, true).unwrap();
        c.put("k1", &entry("hello")).unwrap();
        let got = c.get("k1").unwrap().unwrap();
        assert_eq!(got.body, "hello");
        assert!(c.get("missing").unwrap().is_none());
    }

    #[test]
    fn enforces_entry_cap() {
        let tmp = TempDir::new().unwrap();
        let c = CacheStore::open(&tmp.path().join("c.redb"), 10, true).unwrap();
        for i in 0..50 {
            let mut e = entry(&format!("v{i}"));
            e.created_at = i as i64; // strictly increasing age
            c.put(&format!("k{i}"), &e).unwrap();
        }
        assert!(c.len().unwrap() <= 10, "cap must hold, got {}", c.len().unwrap());
        // Oldest should have been evicted; newest should still be present.
        assert!(c.get("k49").unwrap().is_some());
        assert!(c.get("k0").unwrap().is_none());
    }

    #[test]
    fn disabled_cache_is_noop() {
        let tmp = TempDir::new().unwrap();
        let c = CacheStore::open(&tmp.path().join("c.redb"), 100, false).unwrap();
        c.put("k1", &entry("hello")).unwrap();
        assert!(c.get("k1").unwrap().is_none());
    }
}
