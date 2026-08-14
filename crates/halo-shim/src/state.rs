//! Shared server state and the two side-effect helpers (audit + telemetry).

use crate::audit::AuditLog;
use crate::budget::Ledger;
use crate::cache::CacheStore;
use crate::cache_control::CacheControlInjector;
use crate::config::Config;
use crate::embeddings::EmbeddingClient;
use crate::keys::KeyStore;
use crate::mcp::McpManager;
use crate::revocations::RemoteRevocations;
use crate::semantic_cache::SemanticCacheStore;
use crate::telemetry::Telemetry;
use halo_common::pricing::{decompose_savings, estimate_cost_usd, PriceTable};
use halo_common::telemetry::{PolicyDecision, Provider, TelemetryEvent};
use halo_common::Entitlements;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    /// Resolved once at `serve` startup from `cfg.license_key`. Read-only; the
    /// single source of truth for every paid-feature gate. Free tier when
    /// absent/invalid/expired (never blocks startup).
    pub entitlements: Arc<Entitlements>,
    pub keys: Arc<KeyStore>,
    pub cache: Arc<CacheStore>,
    /// Always constructed (the redb file is cheap to open); gated at the call
    /// site by `cfg.semantic_cache.enabled` so "disabled" is a true no-op.
    pub semantic: Arc<SemanticCacheStore>,
    pub embedder: Arc<EmbeddingClient>,
    pub ledger: Arc<Ledger>,
    pub audit_log: Arc<Mutex<AuditLog>>,
    pub telem: Telemetry,
    pub injector: Arc<CacheControlInjector>,
    pub mcp: Option<Arc<McpManager>>,
    pub prices: Arc<PriceTable>,
    pub device_id: String,
    pub http: reqwest::Client,
    /// Best-effort remotely-revoked agents (paid `remote_kill`). Empty unless a
    /// relay + entitlement enable the poll loop; the local kill switch is
    /// always authoritative regardless of this.
    pub remote_revocations: RemoteRevocations,
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

    /// Fire a budget alert webhook if (and only if) the `alerting` feature is
    /// entitled and a webhook is configured. Fire-and-forget; never blocks.
    pub fn maybe_alert_budget(&self, agent: &str, verdict: &crate::budget::BudgetVerdict) {
        if !self.entitlements.has(halo_common::license::feature::CUT)
            && !self.entitlements.has(halo_common::license::feature::ALERTING)
        {
            return;
        }
        crate::alert::fire_budget_alert(
            &self.http,
            self.cfg.alert_webhook.as_deref(),
            &self.device_id,
            agent,
            verdict,
        );
    }

    /// The shared "a provider call finished" accounting step: compute actual +
    /// counterfactual cost, record spend, emit telemetry, and audit. Used by
    /// both the buffered response path and the streaming-completion path so
    /// the two can't drift apart on how a call is billed.
    #[allow(clippy::too_many_arguments)]
    pub async fn finalize_llm_call(&self, o: LlmOutcome) -> f64 {
        let actual_cost = match (o.decision, o.actual_cost_override) {
            // An explicit override always wins, regardless of decision: it
            // means the caller already knows the real cost (e.g. an
            // embedding provider's own accounting, which is authoritative --
            // for `Mock`/`Ollama` that's genuinely $0, and recomputing from
            // the model-name price table instead would wrongly charge the
            // embedding-fallback rate for a call that cost nothing).
            (_, Some(v)) => v,
            (PolicyDecision::CacheHit, None) => 0.0,
            (_, None) => estimate_cost_usd(&self.prices, &o.model, o.tokens_in, o.tokens_out, o.tokens_cached),
        };
        // Counterfactual: no provider cache discount and no compression --
        // what this would have cost without either optimization. Computed
        // via the shared decomposition helper so `halo report`/the relay can
        // later split this same gap into compression vs. provider-cache
        // portions from the raw token fields alone, without needing this
        // event to carry pre-split dollar amounts.
        let counterfactual_cost = decompose_savings(
            &self.prices,
            &o.model,
            o.tokens_in,
            o.tokens_out,
            o.tokens_cached,
            o.compression_ratio,
        )
        .counterfactual_cost;

        if o.record_spend && actual_cost > 0.0 {
            let _ = self.ledger.record(&o.agent, actual_cost);
        }

        // One terse line per completed request. At the default `info` level
        // (stderr) this makes traffic visible in the foreground in real time --
        // without it, `halo serve` prints a banner and then goes silent while
        // requests flow, which reads as "dead". Quiet it with `RUST_LOG=warn`.
        let cache_hit = matches!(
            o.decision,
            PolicyDecision::CacheHit | PolicyDecision::SemanticCacheHit
        );
        tracing::info!(
            agent = %o.agent,
            model = %o.model,
            tokens_in = o.tokens_in,
            tokens_out = o.tokens_out,
            cost = %format!("${actual_cost:.4}"),
            cache = if cache_hit { "hit" } else { "miss" },
            "request"
        );

        self.emit(TelemetryEvent {
            device_id: self.device_id.clone(),
            agent_id: o.agent.clone(),
            subject: o.subject.clone(),
            timestamp: chrono::Utc::now(),
            provider: o.provider,
            model: o.model.clone(),
            tokens_in: o.tokens_in,
            tokens_out: o.tokens_out,
            tokens_cached: o.tokens_cached,
            // Both hit kinds mean "the local machine served this, the
            // provider was never called for a completion" -- the relay's
            // fleet-wide savings aggregate treats both as pure savings. The
            // semantic hit's small real embedding cost is fully accounted for
            // locally (ledger + audit below); folding it into the relay's
            // cross-device aggregate too would be a rounding-level nicety,
            // not a correctness requirement (see docs/DESIGN_REVIEW.md).
            cache_hit,
            task_class: o.task_class.clone(),
            latency_ms: o.latency_ms,
            estimated_cost: actual_cost,
            counterfactual_cost,
            policy_decision: o.decision,
            compression_ratio: o.compression_ratio,
            error_class: o.error_class.clone(),
            shadow_savings_usd: o.shadow_savings_usd,
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
    /// Optional sub-identity (channel/sub-agent/thread) from `X-Halo-Subject`.
    pub subject: Option<String>,
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
    /// Used only for `PolicyDecision::SemanticCacheHit`: the real cost of the
    /// embedding-lookup call, decoupled from the served model's token price.
    pub actual_cost_override: Option<f64>,
    /// Free-tier shadow savings for this call. Zero when Cut is entitled.
    pub shadow_savings_usd: f64,
}
