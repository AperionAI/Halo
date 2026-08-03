//! Hash-chained local audit log.
//!
//! Adapted from `compass-standalone`'s `evidence/chain.rs` writer contract so
//! the files Halo produces are byte-compatible with Compass's verifier and
//! Smartflow's `audit-verifier`: strip `entry_hmac`, recursively sort object
//! keys, `serde_json::to_string`, HMAC-SHA256 with the key; each entry's
//! `prev_hash` equals the previous entry's `entry_hmac` ("genesis" for seq 1).
//!
//! This gives tamper-evidence today (append-only, chained) without the full
//! encrypted-escrow design -- a clean stepping stone. Secrets are NEVER
//! written here; only policy decisions and tool-call metadata.

use crate::util::atomic_write_0600;
use anyhow::{Context, Result};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

type HmacSha256 = Hmac<Sha256>;

pub struct AuditLog {
    path: PathBuf,
    key: Vec<u8>,
    next_seq: u64,
    prev: String,
}

impl AuditLog {
    /// Open (or start) the chain at `path`, loading/creating the HMAC key at
    /// `key_path`.
    pub fn open(path: &Path, key_path: &Path) -> Result<Self> {
        let key = load_or_create_key(key_path)?;
        let (next_seq, prev) = tail_state(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            key,
            next_seq,
            prev,
        })
    }

    /// Append an event. `event` is arbitrary metadata JSON (never secrets).
    pub fn record(&mut self, event: serde_json::Value) -> Result<()> {
        let mut entry = serde_json::json!({
            "seq": self.next_seq,
            "prev_hash": self.prev,
            "ts": chrono::Utc::now().to_rfc3339(),
            "event": event,
        });
        let hmac = hmac_hex(&self.key, &canonical_for_hmac(&entry));
        entry
            .as_object_mut()
            .unwrap()
            .insert("entry_hmac".into(), serde_json::json!(hmac));

        let line = serde_json::to_string(&entry)?;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .context("opening audit log")?;
        writeln!(f, "{line}")?;

        self.prev = hmac;
        self.next_seq += 1;
        Ok(())
    }
}

/// Read the last entry to resume the chain: returns (next_seq, prev_hash).
fn tail_state(path: &Path) -> Result<(u64, String)> {
    if !path.exists() {
        return Ok((1, "genesis".to_string()));
    }
    let raw = std::fs::read_to_string(path)?;
    let last = raw.lines().rfind(|l| !l.trim().is_empty());
    match last {
        None => Ok((1, "genesis".to_string())),
        Some(line) => {
            let v: serde_json::Value = serde_json::from_str(line)?;
            let seq = v.get("seq").and_then(|s| s.as_u64()).unwrap_or(0);
            let hmac = v
                .get("entry_hmac")
                .and_then(|h| h.as_str())
                .unwrap_or("genesis")
                .to_string();
            Ok((seq + 1, hmac))
        }
    }
}

fn load_or_create_key(key_path: &Path) -> Result<Vec<u8>> {
    if let Ok(bytes) = std::fs::read(key_path) {
        if bytes.len() >= 16 {
            return Ok(bytes);
        }
    }
    let mut key = [0u8; 32];
    getrandom::getrandom(&mut key).map_err(|e| anyhow::anyhow!("rng: {e}"))?;
    atomic_write_0600(key_path, &key).context("writing audit key")?;
    Ok(key.to_vec())
}

/// Canonicalize for HMAC -- identical contract to Compass/Smartflow.
fn canonical_for_hmac(entry: &serde_json::Value) -> String {
    let mut v = entry.clone();
    if let Some(obj) = v.as_object_mut() {
        obj.remove("entry_hmac");
    }
    serde_json::to_string(&sort_value(&v)).unwrap_or_default()
}

fn sort_value(v: &serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(m) => {
            let bt: BTreeMap<String, serde_json::Value> =
                m.iter().map(|(k, val)| (k.clone(), sort_value(val))).collect();
            serde_json::Value::Object(bt.into_iter().collect())
        }
        serde_json::Value::Array(a) => {
            serde_json::Value::Array(a.iter().map(sort_value).collect())
        }
        _ => v.clone(),
    }
}

fn hmac_hex(key: &[u8], msg: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(msg.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn chain_links_and_verifies() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("audit.jsonl");
        let key_path = tmp.path().join("audit-key");
        {
            let mut log = AuditLog::open(&path, &key_path).unwrap();
            log.record(serde_json::json!({"kind":"allow","agent":"a"})).unwrap();
            log.record(serde_json::json!({"kind":"cache_hit","agent":"a"})).unwrap();
        }
        // Reopen and append; seq must continue.
        {
            let mut log = AuditLog::open(&path, &key_path).unwrap();
            log.record(serde_json::json!({"kind":"block","agent":"b"})).unwrap();
        }

        let raw = std::fs::read_to_string(&path).unwrap();
        let entries: Vec<serde_json::Value> =
            raw.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(entries.len(), 3);

        // Verify linkage + HMAC exactly as the Compass verifier does.
        let key = std::fs::read(&key_path).unwrap();
        let mut prev = "genesis".to_string();
        for (i, e) in entries.iter().enumerate() {
            assert_eq!(e["seq"].as_u64().unwrap(), (i + 1) as u64);
            assert_eq!(e["prev_hash"].as_str().unwrap(), prev);
            let stored = e["entry_hmac"].as_str().unwrap();
            assert_eq!(hmac_hex(&key, &canonical_for_hmac(e)), stored);
            prev = stored.to_string();
        }
    }
}
