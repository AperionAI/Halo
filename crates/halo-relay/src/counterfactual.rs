//! Canonical, server-side savings computation.
//!
//! "The single most important thing on the relay" -- it's what makes the free
//! tier's COGS estimator credible rather than a marketing number. The relay
//! recomputes actual and counterfactual cost from the stored token metadata
//! using the SAME shared price table the shim uses (`halo_common::pricing`),
//! so the headline savings figure is canonical and not simply whatever the
//! shim reported.

use halo_common::pricing::{estimate_cost_usd, PriceTable};

/// Metadata needed to recompute one event's cost, canonically.
pub struct EventFacts<'a> {
    pub model: &'a str,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub tokens_cached: u64,
    pub compression_ratio: f64,
    pub cache_hit: bool,
}

/// Returns (actual_cost, counterfactual_cost).
///
/// * actual: what was really paid -- $0 on a cache hit (never called the
///   provider), else input at the (partly cached) rate plus output.
/// * counterfactual: what it WOULD have cost with no cache discount and no
///   compression -- the compressed `tokens_in` is scaled back up by the
///   compression ratio, and cached tokens are billed at the full input rate.
pub fn canonical(prices: &PriceTable, f: &EventFacts) -> (f64, f64) {
    let actual = if f.cache_hit {
        0.0
    } else {
        estimate_cost_usd(prices, f.model, f.tokens_in, f.tokens_out, f.tokens_cached)
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

    #[test]
    fn cache_hit_is_pure_savings() {
        let p = PriceTable::default();
        let (a, c) = canonical(
            &p,
            &EventFacts {
                model: "gpt-4o",
                tokens_in: 1000,
                tokens_out: 500,
                tokens_cached: 0,
                compression_ratio: 1.0,
                cache_hit: true,
            },
        );
        assert_eq!(a, 0.0);
        assert!(c > 0.0);
    }

    #[test]
    fn compression_widens_counterfactual() {
        let p = PriceTable::default();
        let (_, c) = canonical(
            &p,
            &EventFacts {
                model: "gpt-4o",
                tokens_in: 500,
                tokens_out: 0,
                tokens_cached: 0,
                compression_ratio: 0.5, // sent half; would've sent 1000
                cache_hit: false,
            },
        );
        let full = estimate_cost_usd(&p, "gpt-4o", 1000, 0, 0);
        assert!((c - full).abs() < 1e-9);
    }
}
