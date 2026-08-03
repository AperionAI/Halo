//! `halo report` -- local COGS / savings view.
//!
//! Reads only the durable local telemetry log, so it works fully offline even
//! if the relay was never reachable. This is the same counterfactual math the
//! relay runs server-side, kept honest by operating on the exact metadata the
//! shim recorded.

use halo_common::telemetry::TelemetryEvent;
use std::collections::BTreeMap;

#[derive(Debug, Default, Clone)]
pub struct AgentRollup {
    pub requests: u64,
    pub cache_hits: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub actual_cost: f64,
    pub counterfactual_cost: f64,
}

impl AgentRollup {
    pub fn savings(&self) -> f64 {
        (self.counterfactual_cost - self.actual_cost).max(0.0)
    }
}

#[derive(Debug, Default, Clone)]
pub struct Report {
    pub total: AgentRollup,
    pub by_agent: BTreeMap<String, AgentRollup>,
    pub by_model: BTreeMap<String, AgentRollup>,
}

/// Roll up events optionally filtered to those on/after `since` (unix secs).
pub fn build(events: &[TelemetryEvent], since_secs: Option<i64>) -> Report {
    let mut report = Report::default();
    for e in events {
        if let Some(s) = since_secs {
            if e.timestamp.timestamp() < s {
                continue;
            }
        }
        for bucket in [
            &mut report.total,
            report.by_agent.entry(e.agent_id.clone()).or_default(),
            report.by_model.entry(e.model.clone()).or_default(),
        ] {
            bucket.requests += 1;
            if e.cache_hit {
                bucket.cache_hits += 1;
            }
            bucket.tokens_in += e.tokens_in;
            bucket.tokens_out += e.tokens_out;
            bucket.actual_cost += e.estimated_cost;
            bucket.counterfactual_cost += e.counterfactual_cost;
        }
    }
    report
}

/// Render a compact text report for the CLI.
pub fn render(report: &Report) -> String {
    let mut out = String::new();
    let t = &report.total;
    let hit_rate = if t.requests > 0 {
        100.0 * t.cache_hits as f64 / t.requests as f64
    } else {
        0.0
    };
    out.push_str("Smartflow Halo -- local savings report\n");
    out.push_str("======================================\n");
    out.push_str(&format!("Requests:        {}\n", t.requests));
    out.push_str(&format!("Cache hits:      {} ({hit_rate:.1}%)\n", t.cache_hits));
    out.push_str(&format!("Tokens in/out:   {} / {}\n", t.tokens_in, t.tokens_out));
    out.push_str(&format!("Actual spend:    {}\n", fmt_usd(t.actual_cost)));
    out.push_str(&format!(
        "Would-have cost: {}  (no cache, no compression)\n",
        fmt_usd(t.counterfactual_cost)
    ));
    out.push_str(&format!("Estimated saved: {}\n", fmt_usd(t.savings())));

    if !report.by_agent.is_empty() {
        out.push_str("\nBy agent:\n");
        for (agent, r) in &report.by_agent {
            out.push_str(&format!(
                "  {agent:<20} spend {}  saved {}  ({} reqs, {} hits)\n",
                fmt_usd(r.actual_cost),
                fmt_usd(r.savings()),
                r.requests,
                r.cache_hits
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
        let r = build(&events, None);
        assert_eq!(r.total.requests, 3);
        assert_eq!(r.total.cache_hits, 1);
        assert!((r.total.savings() - 0.13).abs() < 1e-9);
        assert_eq!(r.by_agent.len(), 2);
    }
}
