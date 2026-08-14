//! Telemetry: append-only local log + batched, retryable upload to the relay.
//!
//! Two stores with distinct jobs:
//!   * `telemetry.jsonl` (local log) -- append-only on the hot path, NEVER
//!     cleared by upload. Pruned at `halo serve` start to the tier history
//!     window (Free 7d / Cut 30d / Route 90d). This is what `halo report`
//!     reads, so COGS/savings work fully offline even if the relay was never
//!     reachable. `audit.jsonl` is a hash chain and is never pruned here.
//!   * `spool/` (retry queue) -- batches that failed to upload, replayed on
//!     reconnect and deleted on success.
//!
//! TRUST INVARIANT: only `TelemetryEvent` (metadata) is ever written or
//! uploaded. No prompt/response text, no tool args, no vectors. The schema is
//! defined once in `halo-common` and published verbatim in the docs.

use crate::config::EgressConfig;
use anyhow::Result;
use halo_common::telemetry::{TelemetryBatch, TelemetryEvent};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Path on the relay that accepts a `TelemetryBatch`.
const INGEST_PATH: &str = "/v1/telemetry";

struct Inner {
    device_id: String,
    relay_url: Option<String>,
    relay_token: Option<String>,
    spool_dir: PathBuf,
    log_path: PathBuf,
    client: reqwest::Client,
    buffer: Mutex<Vec<TelemetryEvent>>,
    batch_size: usize,
    /// Checked before every relay upload, same policy as the LLM provider and
    /// embeddings egress. Defaults to unrestricted.
    egress: EgressConfig,
}

#[derive(Clone)]
pub struct Telemetry {
    inner: Arc<Inner>,
}

impl Telemetry {
    pub fn new(
        device_id: String,
        relay_url: Option<String>,
        relay_token: Option<String>,
        spool_dir: PathBuf,
        log_path: PathBuf,
    ) -> Self {
        Self::with_egress(device_id, relay_url, relay_token, spool_dir, log_path, EgressConfig::default())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_egress(
        device_id: String,
        relay_url: Option<String>,
        relay_token: Option<String>,
        spool_dir: PathBuf,
        log_path: PathBuf,
        egress: EgressConfig,
    ) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        Self {
            inner: Arc::new(Inner {
                device_id,
                relay_url,
                relay_token,
                spool_dir,
                log_path,
                client,
                buffer: Mutex::new(Vec::new()),
                batch_size: 32,
                egress,
            }),
        }
    }

    /// Record one event: append to the durable local log, and enqueue for
    /// upload. Never blocks the hot path on the network.
    pub async fn record(&self, event: TelemetryEvent) {
        if let Ok(line) = serde_json::to_string(&event) {
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.inner.log_path)
            {
                let _ = writeln!(f, "{line}");
            }
        }
        let mut buf = self.inner.buffer.lock().await;
        buf.push(event);
        let full = buf.len() >= self.inner.batch_size;
        drop(buf);
        if full {
            self.flush().await;
        }
    }

    /// Upload the in-memory buffer (and replay any spooled batches). Failures
    /// spool to disk for later. No-op when no relay is configured.
    pub async fn flush(&self) {
        let batch: Vec<TelemetryEvent> = {
            let mut buf = self.inner.buffer.lock().await;
            std::mem::take(&mut *buf)
        };

        let relay = match &self.inner.relay_url {
            Some(u) => u.clone(),
            None => return, // local-only mode: nothing to upload.
        };

        if !batch.is_empty()
            && self.upload(&relay, &batch).await.is_err() {
                self.spool(&batch);
            }
        // Best-effort replay of previously spooled batches.
        let _ = self.replay_spool(&relay).await;
    }

    async fn upload(&self, relay: &str, events: &[TelemetryEvent]) -> Result<()> {
        let url = format!("{}{}", relay.trim_end_matches('/'), INGEST_PATH);
        if let Err(host) = crate::egress::check_egress(&self.inner.egress, &url) {
            // Fail like any other upload failure -- caller spools to disk and
            // retries later. Logged once so a misconfigured allowlist is
            // diagnosable without silently losing telemetry forever.
            tracing::warn!("Halo egress policy denied relay upload to \"{host}\"; spooling locally");
            anyhow::bail!("egress policy denied relay upload to \"{host}\"");
        }
        let body = TelemetryBatch {
            device_id: self.inner.device_id.clone(),
            events: events.to_vec(),
        };
        let mut req = self.inner.client.post(&url).json(&body);
        if let Some(tok) = &self.inner.relay_token {
            req = req.bearer_auth(tok);
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("relay returned {}", resp.status());
        }
        Ok(())
    }

    fn spool(&self, events: &[TelemetryEvent]) {
        let _ = std::fs::create_dir_all(&self.inner.spool_dir);
        let name = format!(
            "{}-{}.json",
            chrono::Utc::now().timestamp_millis(),
            uuid::Uuid::new_v4().simple()
        );
        let path = self.inner.spool_dir.join(name);
        if let Ok(bytes) = serde_json::to_vec(events) {
            let _ = std::fs::write(path, bytes);
        }
    }

    async fn replay_spool(&self, relay: &str) -> Result<()> {
        let dir = match std::fs::read_dir(&self.inner.spool_dir) {
            Ok(d) => d,
            Err(_) => return Ok(()),
        };
        for entry in dir.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let events: Vec<TelemetryEvent> = match std::fs::read(&path)
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
            {
                Some(e) => e,
                None => {
                    let _ = std::fs::remove_file(&path); // corrupt: drop it.
                    continue;
                }
            };
            if self.upload(relay, &events).await.is_ok() {
                let _ = std::fs::remove_file(&path);
            } else {
                break; // relay down again; try the rest later.
            }
        }
        Ok(())
    }

    /// All locally-logged events, for `halo report` (works offline).
    pub fn local_events(&self) -> Vec<TelemetryEvent> {
        let raw = match std::fs::read_to_string(&self.inner.log_path) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        raw.lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    /// Drop events older than `hours` from `telemetry.jsonl`. Does not touch
    /// `audit.jsonl`. Returns how many lines were dropped. No-op if the file
    /// is missing or nothing is stale.
    pub fn prune_older_than_hours(&self, hours: u64) -> Result<u64> {
        let cutoff = chrono::Utc::now().timestamp() - (hours as i64).saturating_mul(3600);
        let raw = match std::fs::read_to_string(&self.inner.log_path) {
            Ok(r) => r,
            Err(_) => return Ok(0),
        };
        let mut kept = Vec::new();
        let mut dropped = 0u64;
        for line in raw.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<TelemetryEvent>(line) {
                Ok(e) if e.timestamp.timestamp() < cutoff => dropped += 1,
                _ => kept.push(line),
            }
        }
        if dropped == 0 {
            return Ok(0);
        }
        let mut body = kept.join("\n");
        if !body.is_empty() {
            body.push('\n');
        }
        crate::util::atomic_write_0600(&self.inner.log_path, body.as_bytes())?;
        Ok(dropped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use halo_common::telemetry::{PolicyDecision, Provider};

    fn ev(hours_ago: i64) -> TelemetryEvent {
        TelemetryEvent {
            device_id: "d".into(),
            agent_id: "a".into(),
            subject: None,
            timestamp: chrono::Utc::now() - chrono::Duration::hours(hours_ago),
            provider: Provider::Openai,
            model: "gpt-4o".into(),
            tokens_in: 1,
            tokens_out: 1,
            tokens_cached: 0,
            cache_hit: false,
            task_class: "chat".into(),
            latency_ms: 1,
            estimated_cost: 0.01,
            counterfactual_cost: 0.01,
            policy_decision: PolicyDecision::Allow,
            compression_ratio: 1.0,
            error_class: String::new(),
            shadow_savings_usd: 0.0,
        }
    }

    #[test]
    fn prune_drops_stale_keeps_recent() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("telemetry.jsonl");
        let t = Telemetry::new(
            "d".into(),
            None,
            None,
            dir.path().join("spool"),
            log.clone(),
        );
        let lines = [ev(48), ev(1)]
            .into_iter()
            .map(|e| serde_json::to_string(&e).unwrap())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(&log, lines).unwrap();
        let dropped = t.prune_older_than_hours(24).unwrap();
        assert_eq!(dropped, 1);
        assert_eq!(t.local_events().len(), 1);
    }
}
