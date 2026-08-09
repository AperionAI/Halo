//! Exact-match L1 response cache (redb-backed, hard entry cap).
//!
//! Exact-match, not similarity-based: the architecture analysis found cache
//! value is dominated by within-user repetition (fixed system prompts,
//! polling loops, repeated tool schemas), which exact-match already captures
//! without needing embeddings. (Genuine similarity-based reuse lives one
//! layer up, in `semantic_cache.rs`, which is careful about exactly the
//! failure modes a naive embedding cache runs into.)
//!
//! The entry cap is enforced from the first commit (a lesson-learned: the main
//! proxy's L1 grew unbounded and needed `METACACHE_L1_MAX_ENTRIES` bolted on
//! after the fact). When full we batch-evict the oldest ~10% so eviction cost
//! is amortized, not paid on every insert.

use crate::answer::AnswerExtract;
use crate::vault;
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
    /// Plain-text answer extracted at store time, if the response was a clean
    /// text completion (no tool call). Present so a hit on a *streaming*
    /// request can be replayed as a synthetic SSE stream even though `body`
    /// holds the original buffered JSON. `None` for entries written before
    /// this field existed, or for responses that weren't cleanly extractable
    /// -- in either case the entry still serves non-streaming hits from
    /// `body` exactly as before.
    #[serde(default)]
    pub answer: Option<AnswerExtract>,
}

/// On-disk envelope. `created_at` is always cleartext at this outer layer so
/// eviction never needs to decrypt every row just to age-sort them; `payload`
/// is either the plaintext `CacheEntry` JSON (`sealed: false`) or that same
/// JSON passed through `vault::seal` (`sealed: true`). Required fields (no
/// `#[serde(default)]`) so a pre-encryption-refactor row -- raw `CacheEntry`
/// JSON with no `sealed`/`payload` fields -- reliably fails to parse as this
/// type and falls through to the legacy-format path in `decode_entry`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredRecord {
    created_at: i64,
    sealed: bool,
    payload: Vec<u8>,
}

pub struct CacheStore {
    db: Database,
    max_entries: u64,
    enabled: bool,
    /// `Some(passphrase)` when `encrypt_at_rest` is on. New writes are
    /// sealed; reads transparently open sealed rows and fall back to
    /// plaintext for rows written before encryption was enabled.
    encrypt: Option<String>,
}

impl CacheStore {
    #[allow(dead_code)] // convenience wrapper used by tests; production always calls open_with_encryption
    pub fn open(path: &Path, max_entries: u64, enabled: bool) -> Result<Arc<Self>> {
        Self::open_with_encryption(path, max_entries, enabled, None)
    }

    pub fn open_with_encryption(
        path: &Path,
        max_entries: u64,
        enabled: bool,
        encrypt: Option<String>,
    ) -> Result<Arc<Self>> {
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
            encrypt,
        }))
    }

    pub fn get(&self, key: &str) -> Result<Option<CacheEntry>> {
        if !self.enabled {
            return Ok(None);
        }
        let rtxn = self.db.begin_read()?;
        let table = rtxn.open_table(TABLE)?;
        match table.get(key)? {
            Some(v) => self.decode_entry(v.value()),
            None => Ok(None),
        }
    }

    pub fn put(&self, key: &str, entry: &CacheEntry) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let entry_json = serde_json::to_vec(entry)?;
        let (sealed, payload) = match &self.encrypt {
            Some(pass) => (true, vault::seal(pass, &entry_json)?),
            None => (false, entry_json),
        };
        let bytes = serde_json::to_vec(&StoredRecord {
            created_at: entry.created_at,
            sealed,
            payload,
        })?;
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
                        aged.push((created_at_of(v.value()), k.value().to_string()));
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

    /// Decode a raw redb value into a `CacheEntry`, transparently unsealing
    /// if needed. Never errors on decode/decrypt failure -- returns `Ok(None)`
    /// so a corrupt row or an unreadable (wrong-passphrase) sealed row is
    /// treated as a miss, not a hard failure of the hot path.
    fn decode_entry(&self, raw: &[u8]) -> Result<Option<CacheEntry>> {
        if let Ok(rec) = serde_json::from_slice::<StoredRecord>(raw) {
            let json = if rec.sealed {
                match self.encrypt.as_deref().and_then(|p| vault::open(p, &rec.payload).ok()) {
                    Some(pt) => pt,
                    None => return Ok(None),
                }
            } else {
                rec.payload
            };
            return Ok(serde_json::from_slice(&json).ok());
        }
        // Legacy pre-encryption-refactor format: a bare `CacheEntry`.
        Ok(serde_json::from_slice(raw).ok())
    }

    #[allow(dead_code)] // used by tests and `halo status` diagnostics
    pub fn len(&self) -> Result<u64> {
        let rtxn = self.db.begin_read()?;
        let table = rtxn.open_table(TABLE)?;
        Ok(table.len()?)
    }
}

/// `created_at` for eviction age-sorting, readable without decrypting the
/// (possibly sealed) payload -- new-format rows carry it in cleartext at the
/// `StoredRecord` layer; legacy rows fall back to decoding the bare entry.
fn created_at_of(raw: &[u8]) -> i64 {
    if let Ok(rec) = serde_json::from_slice::<StoredRecord>(raw) {
        return rec.created_at;
    }
    serde_json::from_slice::<CacheEntry>(raw).map(|e| e.created_at).unwrap_or(0)
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
            answer: None,
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

    #[test]
    fn encrypted_store_roundtrips() {
        let tmp = TempDir::new().unwrap();
        let c = CacheStore::open_with_encryption(
            &tmp.path().join("c.redb"),
            100,
            true,
            Some("correct horse battery staple".to_string()),
        )
        .unwrap();
        c.put("k1", &entry("sensitive response body")).unwrap();
        let got = c.get("k1").unwrap().unwrap();
        assert_eq!(got.body, "sensitive response body");
    }

    #[test]
    fn encrypted_store_is_unreadable_with_wrong_passphrase() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("c.redb");
        {
            let c = CacheStore::open_with_encryption(&path, 100, true, Some("right-pass".to_string())).unwrap();
            c.put("k1", &entry("sensitive")).unwrap();
        }
        // Re-open the same file with a different passphrase (simulating a
        // config change / stolen file without the real key).
        let c2 = CacheStore::open_with_encryption(&path, 100, true, Some("wrong-pass".to_string())).unwrap();
        assert!(c2.get("k1").unwrap().is_none(), "wrong passphrase must not decrypt");
    }

    #[test]
    fn eviction_still_works_when_encrypted() {
        let tmp = TempDir::new().unwrap();
        let c = CacheStore::open_with_encryption(
            &tmp.path().join("c.redb"),
            10,
            true,
            Some("pass".to_string()),
        )
        .unwrap();
        for i in 0..50 {
            let mut e = entry(&format!("v{i}"));
            e.created_at = i as i64;
            c.put(&format!("k{i}"), &e).unwrap();
        }
        assert!(c.len().unwrap() <= 10);
        assert!(c.get("k49").unwrap().is_some());
        assert!(c.get("k0").unwrap().is_none());
    }

    #[test]
    fn pre_encryption_plaintext_row_is_still_readable_after_enabling_encryption() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("c.redb");
        // Write a row the old way: no encryption configured.
        {
            let c = CacheStore::open(&path, 100, true).unwrap();
            c.put("k1", &entry("written before encryption existed")).unwrap();
        }
        // Re-open with encryption on -- the old plaintext row must still read.
        let c2 = CacheStore::open_with_encryption(&path, 100, true, Some("pass".to_string())).unwrap();
        let got = c2.get("k1").unwrap().unwrap();
        assert_eq!(got.body, "written before encryption existed");
        // A subsequent write from the now-encrypted store seals normally.
        c2.put("k2", &entry("written after encryption enabled")).unwrap();
        assert_eq!(c2.get("k2").unwrap().unwrap().body, "written after encryption enabled");
    }
}
