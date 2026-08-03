//! `halo report` -- local COGS / savings view.
//!
//! Reads only the durable local telemetry log, so it works fully offline even
//! if the relay was never reachable. This is the same counterfactual math the
//! relay runs server-side, kept honest by operating on the exact metadata the
//! shim recorded.

use halo_common::pricing::{decompose_savings, PriceTable};
use halo_common::telemetry::{PolicyDecision, TelemetryEvent};
use std::collections::BTreeMap;

#[derive(Debug, Default, Clone)]
pub struct AgentRollup {
    pub requests: u64,
    /// Exact-match hits (identical, normalized prompt seen before).
    pub cache_hits: u64,
    /// Semantic hits: a *similar* prompt, possibly answered by a different
    /// provider originally. Tracked separately from `cache_hits` since it has
    /// a (tiny) real cost and a different reliability profile.
    pub semantic_hits: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub actual_cost: f64,
    pub counterfactual_cost: f64,
    /// Sum of [`halo_common::pricing::SavingsBreakdown::compression_savings`]
    /// across every event -- recomputed from each event's own
    /// tokens/compression_ratio, not trusted from a stored dollar figure.
    pub compression_savings: f64,
    /// Sum of `provider_cache_savings` -- the Anthropic/OpenAI prompt-cache
    /// discount, independent of whether Halo's own cache ever hit.
    pub provider_cache_savings: f64,
}

impl AgentRollup {
    pub fn savings(&self) -> f64 {
        (self.counterfactual_cost - self.actual_cost).max(0.0)
    }

    /// Savings that apply on every call, hit or not: compression +
    /// provider-native prompt caching. This is the floor a deployment still
    /// gets even at a 0% Halo cache-hit rate.
    pub fn baseline_savings(&self) -> f64 {
        self.compression_savings + self.provider_cache_savings
    }

    /// The remainder attributable specifically to a Halo L1/exact or L2/
    /// semantic cache hit (never calling the provider at all).
    pub fn hit_savings(&self) -> f64 {
        (self.savings() - self.baseline_savings()).max(0.0)
    }
}

#[derive(Debug, Default, Clone)]
pub struct Report {
    pub total: AgentRollup,
    pub by_agent: BTreeMap<String, AgentRollup>,
    pub by_model: BTreeMap<String, AgentRollup>,
    /// Per-subject (channel/sub-agent/thread) rollup -- only events that
    /// carried an `X-Halo-Subject` hint appear here. Empty when nothing set
    /// one, so a solo user never sees an empty section.
    pub by_subject: BTreeMap<String, AgentRollup>,
}

/// Roll up events optionally filtered to those on/after `since` (unix secs).
///
/// `prices` is used only to re-split each event's already-computed
/// actual/counterfactual gap into compression vs. provider-cache portions
/// (see [`halo_common::pricing::decompose_savings`]) -- it does not change
/// the event's own recorded `estimated_cost`/`counterfactual_cost`, so a
/// stale local `price_overrides` config can shift the baseline/hit split
/// slightly but never the headline "Estimated saved" total.
pub fn build(events: &[TelemetryEvent], since_secs: Option<i64>, prices: &PriceTable) -> Report {
    let mut report = Report::default();
    for e in events {
        if let Some(s) = since_secs {
            if e.timestamp.timestamp() < s {
                continue;
            }
        }
        let breakdown = decompose_savings(
            prices,
            &e.model,
            e.tokens_in,
            e.tokens_out,
            e.tokens_cached,
            e.compression_ratio,
        );
        let mut buckets: Vec<&mut AgentRollup> = vec![
            &mut report.total,
            report.by_agent.entry(e.agent_id.clone()).or_default(),
            report.by_model.entry(e.model.clone()).or_default(),
        ];
        if let Some(subj) = e.subject.as_ref().filter(|s| !s.is_empty()) {
            buckets.push(report.by_subject.entry(subj.clone()).or_default());
        }
        for bucket in buckets {
            accumulate(bucket, e, &breakdown);
        }
    }
    report
}

/// Fold one event into a rollup bucket. Shared so total/agent/model/subject
/// buckets can never drift on how a request is counted.
fn accumulate(
    bucket: &mut AgentRollup,
    e: &TelemetryEvent,
    breakdown: &halo_common::pricing::SavingsBreakdown,
) {
    bucket.requests += 1;
    match e.policy_decision {
        PolicyDecision::SemanticCacheHit => bucket.semantic_hits += 1,
        _ if e.cache_hit => bucket.cache_hits += 1,
        _ => {}
    }
    bucket.tokens_in += e.tokens_in;
    bucket.tokens_out += e.tokens_out;
    bucket.actual_cost += e.estimated_cost;
    bucket.counterfactual_cost += e.counterfactual_cost;
    bucket.compression_savings += breakdown.compression_savings;
    bucket.provider_cache_savings += breakdown.provider_cache_savings;
}

/// Render a compact text report for the CLI.
pub fn render(report: &Report) -> String {
    let mut out = String::new();
    let t = &report.total;
    let hit_rate = if t.requests > 0 {
        100.0 * (t.cache_hits + t.semantic_hits) as f64 / t.requests as f64
    } else {
        0.0
    };
    out.push_str("Smartflow Halo -- local savings report\n");
    out.push_str("======================================\n");
    out.push_str(&format!("Requests:        {}\n", t.requests));
    out.push_str(&format!(
        "Cache hits:      {} exact + {} semantic ({hit_rate:.1}% total)\n",
        t.cache_hits, t.semantic_hits
    ));
    out.push_str(&format!("Tokens in/out:   {} / {}\n", t.tokens_in, t.tokens_out));
    out.push_str(&format!("Actual spend:    {}\n", fmt_usd(t.actual_cost)));
    out.push_str(&format!(
        "Would-have cost: {}  (no cache, no compression, no provider prompt-cache)\n",
        fmt_usd(t.counterfactual_cost)
    ));
    out.push_str(&format!("Estimated saved: {}\n", fmt_usd(t.savings())));
    out.push_str(&format!(
        "  of which baseline (compression {} + provider cache {}): {}  -- applies even at 0% hit rate\n",
        fmt_usd(t.compression_savings),
        fmt_usd(t.provider_cache_savings),
        fmt_usd(t.baseline_savings())
    ));
    out.push_str(&format!(
        "  of which from Halo cache hits (exact/semantic):        {}\n",
        fmt_usd(t.hit_savings())
    ));

    if !report.by_agent.is_empty() {
        out.push_str("\nBy agent:\n");
        for (agent, r) in &report.by_agent {
            out.push_str(&format!(
                "  {agent:<20} spend {}  saved {}  ({} reqs, {} exact + {} semantic hits)\n",
                fmt_usd(r.actual_cost),
                fmt_usd(r.savings()),
                r.requests,
                r.cache_hits,
                r.semantic_hits
            ));
        }
    }
    if !report.by_model.is_empty() {
        out.push_str("\nBy model:\n");
        for (model, r) in &report.by_model {
            if model.is_empty() {
                continue;
            }
            out.push_str(&format!(
                "  {model:<28} spend {}  saved {}\n",
                fmt_usd(r.actual_cost),
                fmt_usd(r.savings())
            ));
        }
    }
    if !report.by_subject.is_empty() {
        out.push_str("\nBy subject (channel / sub-agent):\n");
        for (subject, r) in &report.by_subject {
            out.push_str(&format!(
                "  {subject:<28} spend {}  saved {}  ({} reqs)\n",
                fmt_usd(r.actual_cost),
                fmt_usd(r.savings()),
                r.requests
            ));
        }
    }
    out
}

/// Show 4 decimals for normal amounts, but widen to 6 for sub-cent spend so a
/// real (if tiny) cost never misleadingly renders as `$0.0000`.
fn fmt_usd(v: f64) -> String {
    if v.abs() < 0.0001 && v != 0.0 {
        format!("${v:.6}")
    } else {
        format!("${v:.4}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use halo_common::telemetry::{PolicyDecision, Provider};

    fn ev(agent: &str, model: &str, actual: f64, counter: f64, hit: bool) -> TelemetryEvent {
        TelemetryEvent {
            device_id: "d".into(),
            agent_id: agent.into(),
            subject: None,
            timestamp: chrono::Utc::now(),
            provider: Provider::Openai,
            model: model.into(),
            tokens_in: 100,
            tokens_out: 50,
            tokens_cached: 0,
            cache_hit: hit,
            task_class: "chat".into(),
            latency_ms: 10,
            estimated_cost: actual,
            counterfactual_cost: counter,
            policy_decision: if hit {
                PolicyDecision::CacheHit
            } else {
                PolicyDecision::Allow
            },
            compression_ratio: 1.0,
            error_class: String::new(),
        }
    }

    #[test]
    fn rollup_sums_and_savings() {
        let events = vec![
            ev("a", "gpt-4o", 0.10, 0.10, false),
            ev("a", "gpt-4o", 0.0, 0.10, true),
            ev("b", "gpt-4o-mini", 0.02, 0.05, false),
        ];
        let r = build(&events, None, &PriceTable::default());
        assert_eq!(r.total.requests, 3);
        assert_eq!(r.total.cache_hits, 1);
        assert!((r.total.savings() - 0.13).abs() < 1e-9);
        assert_eq!(r.by_agent.len(), 2);
    }

    #[test]
    fn by_subject_only_populated_when_hint_present() {
        let mut with_subj = ev("a", "gpt-4o", 0.10, 0.20, false);
        with_subj.subject = Some("slack:general".into());
        let without = ev("a", "gpt-4o", 0.10, 0.20, false);

        let r = build(&[with_subj, without], None, &PriceTable::default());
        // Only the one event with a subject shows up under by_subject.
        assert_eq!(r.by_subject.len(), 1);
        let s = &r.by_subject["slack:general"];
        assert_eq!(s.requests, 1);
        // ...but both are counted in the totals.
        assert_eq!(r.total.requests, 2);
    }

    #[test]
    fn baseline_savings_split_from_hit_savings() {
        let prices = PriceTable::default();
        // A live (non-hit) call with real compression + provider cache
        // discount baked into its token fields.
        let mut live = ev("a", "gpt-4o", 0.0, 0.0, false);
        live.tokens_in = 500;
        live.tokens_out = 200;
        live.tokens_cached = 100;
        live.compression_ratio = 0.5;
        live.estimated_cost = halo_common::pricing::estimate_cost_usd(&prices, "gpt-4o", 500, 200, 100);
        live.counterfactual_cost = halo_common::pricing::estimate_cost_usd(&prices, "gpt-4o", 1000, 200, 0);

        // A genuine Halo cache hit: zero cost, no compression/provider-cache
        // fields set on its own account.
        let hit = ev("a", "gpt-4o", 0.0, 0.05, true);

        let r = build(&[live, hit], None, &prices);
        let a = &r.by_agent["a"];
        assert!(a.compression_savings > 0.0, "expected nonzero compression savings");
        assert!(a.provider_cache_savings > 0.0, "expected nonzero provider-cache savings");
        assert!(a.hit_savings() > 0.0, "expected nonzero hit savings from the cache-hit event");
        assert!((a.baseline_savings() + a.hit_savings() - a.savings()).abs() < 1e-9);
    }
}
