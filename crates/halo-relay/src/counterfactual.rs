//! Canonical, server-side savings computation.
//!
//! "The single most important thing on the relay" -- it's what makes the free
//! tier's COGS estimator credible rather than a marketing number. The relay
//! recomputes actual and counterfactual cost from the stored token metadata
//! using the SAME shared price table the shim uses (`halo_common::pricing`),
//! so the headline savings figure is canonical and not simply whatever the
//! shim reported.

use halo_common::pricing::{estimate_cost_usd, PriceTable};
use halo_common::telemetry::PolicyDecision;

/// Metadata needed to recompute one event's cost, canonically.
pub struct EventFacts<'a> {
    pub model: &'a str,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub tokens_cached: u64,
    pub compression_ratio: f64,
    pub cache_hit: bool,
    pub policy_decision: PolicyDecision,
    /// The shim's self-reported cost for this event. Ignored for every
    /// decision except `SemanticCacheHit` (see below) -- everything else is
    /// recomputed from token counts against the shared price table so a
    /// buggy/compromised shim can't just self-report inflated savings.
    pub reported_cost: f64,
}

/// Returns (actual_cost, counterfactual_cost).
///
/// * actual: what was really paid.
///   - An exact-match `CacheHit` is genuinely $0: no provider call happened.
///   - A `SemanticCacheHit` DID make one real call -- the embedding lookup --
///     whose price has nothing to do with the (possibly quite different)
///     served model's per-token rate, so it can't be recomputed from
///     `tokens_in`/`tokens_out`/`model` the way a normal call can. The shim's
///     `reported_cost` for this one decision is trusted rather than
///     recomputed, mirroring the same override the shim itself uses
///     internally (`LlmOutcome::actual_cost_override`). It's still a small,
///     auditable number (embedding calls are cheap), not an open-ended
///     self-report of the full completion cost.
///   - Everything else is recomputed from tokens against the canonical table.
/// * counterfactual: what it WOULD have cost with no cache discount and no
///   compression -- the compressed `tokens_in` is scaled back up by the
///   compression ratio, and cached tokens are billed at the full input rate.
pub fn canonical(prices: &PriceTable, f: &EventFacts) -> (f64, f64) {
    let actual = match f.policy_decision {
        PolicyDecision::CacheHit => 0.0,
        PolicyDecision::SemanticCacheHit => f.reported_cost,
        _ if f.cache_hit => 0.0,
        _ => estimate_cost_usd(prices, f.model, f.tokens_in, f.tokens_out, f.tokens_cached),
    };

    let uncompressed_in = if f.compression_ratio > 0.0 && f.compression_ratio < 1.0 {
        (f.tokens_in as f64 / f.compression_ratio).round() as u64
    } else {
        f.tokens_in
    };
    let counterfactual = estimate_cost_usd(prices, f.model, uncompressed_in, f.tokens_out, 0);

    (actual, counterfactual)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(model: &str, tokens_in: u64, tokens_out: u64, compression_ratio: f64, decision: PolicyDecision) -> EventFacts<'_> {
        EventFacts {
            model,
            tokens_in,
            tokens_out,
            tokens_cached: 0,
            compression_ratio,
            cache_hit: matches!(decision, PolicyDecision::CacheHit | PolicyDecision::SemanticCacheHit),
            policy_decision: decision,
            reported_cost: 0.0,
        }
    }

    #[test]
    fn cache_hit_is_pure_savings() {
        let p = PriceTable::default();
        let (a, c) = canonical(&p, &facts("gpt-4o", 1000, 500, 1.0, PolicyDecision::CacheHit));
        assert_eq!(a, 0.0);
        assert!(c > 0.0);
    }

    #[test]
    fn compression_widens_counterfactual() {
        let p = PriceTable::default();
        let (_, c) = canonical(
            &p,
            // sent half; would've sent 1000
            &facts("gpt-4o", 500, 0, 0.5, PolicyDecision::Allow),
        );
        let full = estimate_cost_usd(&p, "gpt-4o", 1000, 0, 0);
        assert!((c - full).abs() < 1e-9);
    }

    #[test]
    fn semantic_cache_hit_trusts_reported_embedding_cost_not_zero() {
        let p = PriceTable::default();
        let mut f = facts("claude-3-5-sonnet", 200, 50, 1.0, PolicyDecision::SemanticCacheHit);
        f.reported_cost = 0.0000135; // a real (tiny) embedding-lookup charge
        let (a, c) = canonical(&p, &f);
        assert_eq!(a, f.reported_cost);
        // Counterfactual (what a live completion would've cost) is still the
        // full completion price, not the tiny embedding price -- that's the
        // whole point of the savings figure.
        assert!(c > a);
    }

    #[test]
    fn semantic_cache_hit_is_not_forced_to_zero_like_exact_hit() {
        let p = PriceTable::default();
        let mut f = facts("gpt-4o", 200, 50, 1.0, PolicyDecision::SemanticCacheHit);
        f.reported_cost = 0.002;
        let (a, _) = canonical(&p, &f);
        assert_ne!(a, 0.0);
    }
}
