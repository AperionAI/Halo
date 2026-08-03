//! Shared server state and the two side-effect helpers (audit + telemetry).

use crate::audit::AuditLog;
use crate::budget::Ledger;
use crate::cache::CacheStore;
use crate::cache_control::CacheControlInjector;
use crate::config::Config;
use crate::keys::KeyStore;
use crate::mcp::McpManager;
use crate::telemetry::Telemetry;
use halo_common::pricing::{estimate_cost_usd, PriceTable};
use halo_common::telemetry::{PolicyDecision, Provider, TelemetryEvent};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub keys: Arc<KeyStore>,
    pub cache: Arc<CacheStore>,
    pub ledger: Arc<Ledger>,
    pub audit_log: Arc<Mutex<AuditLog>>,
    pub telem: Telemetry,
    pub injector: Arc<CacheControlInjector>,
    pub mcp: Option<Arc<McpManager>>,
    pub prices: Arc<PriceTable>,
    pub device_id: String,
    pub http: reqwest::Client,
}

impl AppState {
    /// Record a telemetry event (append to local log + enqueue for upload).
    pub async fn emit(&self, event: TelemetryEvent) {
        self.telem.record(event).await;
    }

    /// Append a metadata-only entry to the hash-chained audit log. Never
    /// blocks on the network; a poisoned/locked log is logged and skipped so
    /// auditing can never take down the proxy hot path.
    pub fn audit(&self, event: serde_json::Value) {
        match self.audit_log.lock() {
            Ok(mut log) => {
                if let Err(e) = log.record(event) {
                    tracing::warn!("audit append failed: {e}");
                }
            }
            Err(_) => tracing::warn!("audit log mutex poisoned; skipping entry"),
        }
    }

    /// The shared "a provider call finished" accounting step: compute actual +
    /// counterfactual cost, record spend, emit telemetry, and audit. Used by
    /// both the buffered response path and the streaming-completion path so
    /// the two can't drift apart on how a call is billed.
    #[allow(clippy::too_many_arguments)]
    pub async fn finalize_llm_call(&self, o: LlmOutcome) -> f64 {
        let actual_cost = if o.decision == PolicyDecision::CacheHit {
            0.0
        } else {
            estimate_cost_usd(&self.prices, &o.model, o.tokens_in, o.tokens_out, o.tokens_cached)
        };
        // Counterfactual: no provider cache discount and no compression --
        // what this would have cost without either optimization.
        let uncompressed_in = if o.compression_ratio > 0.0 && o.compression_ratio < 1.0 {
            (o.tokens_in as f64 / o.compression_ratio).round() as u64
        } else {
            o.tokens_in
        };
        let counterfactual_cost = estimate_cost_usd(&self.prices, &o.model, uncompressed_in, o.tokens_out, 0);

        if o.record_spend && actual_cost > 0.0 {
            let _ = self.ledger.record(&o.agent, actual_cost);
        }

        self.emit(TelemetryEvent {
            device_id: self.device_id.clone(),
            agent_id: o.agent.clone(),
            timestamp: chrono::Utc::now(),
            provider: o.provider,
            model: o.model.clone(),
            tokens_in: o.tokens_in,
            tokens_out: o.tokens_out,
            tokens_cached: o.tokens_cached,
            cache_hit: o.decision == PolicyDecision::CacheHit,
            task_class: o.task_class.clone(),
            latency_ms: o.latency_ms,
            estimated_cost: actual_cost,
            counterfactual_cost,
            policy_decision: o.decision,
            compression_ratio: o.compression_ratio,
            error_class: o.error_class.clone(),
        })
        .await;

        self.audit(serde_json::json!({
            "kind": "llm_call",
            "agent": o.agent,
            "provider": o.provider.as_str(),
            "model": o.model,
            "decision": o.decision.as_str(),
            "cost": actual_cost,
            "streamed": o.streamed,
            "error_class": o.error_class,
        }));

        actual_cost
    }
}

/// Everything needed to bill and log one completed (or aborted) provider call.
pub struct LlmOutcome {
    pub agent: String,
    pub provider: Provider,
    pub model: String,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub tokens_cached: u64,
    pub task_class: String,
    pub latency_ms: u64,
    pub compression_ratio: f64,
    pub decision: PolicyDecision,
    pub error_class: String,
    /// Whether spend should be recorded against the ledger (false for cache
    /// hits and hard-blocked requests, which never called the provider).
    pub record_spend: bool,
    pub streamed: bool,
}
