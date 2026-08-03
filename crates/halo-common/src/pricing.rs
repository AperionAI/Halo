//! Model price table for cost / counterfactual estimation.
//!
//! Prices are USD per 1,000,000 tokens and are APPROXIMATE published list
//! prices as of mid-2026. This table is intentionally data, not code: the
//! shim ships with these defaults and can override them from a local file so
//! a stale table never silently produces wrong numbers in the exact place
//! users trust Halo most (a lesson carried over from the main proxy, whose
//! cost attribution used a price table refreshed from the relay).

use serde::{Deserialize, Serialize};

/// Per-model pricing in USD per million tokens.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ModelPrice {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
    /// Price for cached input reads (usually a large discount). Falls back to
    /// `input_per_mtok` when a provider doesn't offer cached pricing.
    pub cached_input_per_mtok: f64,
}

impl ModelPrice {
    const fn new(input: f64, output: f64, cached: f64) -> Self {
        Self {
            input_per_mtok: input,
            output_per_mtok: output,
            cached_input_per_mtok: cached,
        }
    }
}

/// A resolvable table of model prices with a sane fallback.
#[derive(Debug, Clone)]
pub struct PriceTable {
    entries: Vec<(String, ModelPrice)>,
    fallback: ModelPrice,
    embedding_fallback: ModelPrice,
}

impl Default for PriceTable {
    fn default() -> Self {
        // Approximate mid-2026 list prices. Substring-matched (see `lookup`)
        // so dated model suffixes (e.g. "-20241022") still resolve.
        let entries = vec![
            ("claude-3-5-sonnet", ModelPrice::new(3.0, 15.0, 0.30)),
            ("claude-3-5-haiku", ModelPrice::new(0.80, 4.0, 0.08)),
            ("claude-3-opus", ModelPrice::new(15.0, 75.0, 1.50)),
            ("claude-3-haiku", ModelPrice::new(0.25, 1.25, 0.03)),
            ("claude-sonnet-4", ModelPrice::new(3.0, 15.0, 0.30)),
            ("gpt-4o-mini", ModelPrice::new(0.15, 0.60, 0.075)),
            ("gpt-4o", ModelPrice::new(2.50, 10.0, 1.25)),
            ("gpt-4-turbo", ModelPrice::new(10.0, 30.0, 10.0)),
            ("gpt-4", ModelPrice::new(30.0, 60.0, 30.0)),
            ("gpt-3.5-turbo", ModelPrice::new(0.50, 1.50, 0.50)),
            ("o1", ModelPrice::new(15.0, 60.0, 7.50)),
            ("o3-mini", ModelPrice::new(1.10, 4.40, 0.55)),
            // Embedding models (all-input, no output token component). Listed
            // explicitly so the semantic cache's lookup/store calls get a real
            // price instead of the chat fallback below, which would overcharge
            // a tiny embedding call by ~100-1000x.
            ("text-embedding-3-small", ModelPrice::new(0.02, 0.0, 0.02)),
            ("text-embedding-3-large", ModelPrice::new(0.13, 0.0, 0.13)),
            ("text-embedding-ada-002", ModelPrice::new(0.10, 0.0, 0.10)),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();

        Self {
            entries,
            // Conservative mid-tier fallback so an unknown model still yields
            // a plausible, non-zero estimate rather than $0.
            fallback: ModelPrice::new(3.0, 15.0, 0.30),
            // A separate, much cheaper fallback for unrecognized embedding
            // models (matched by name containing "embed") -- the chat fallback
            // above would badly overcharge a per-embedding-call cost, which
            // matters because every semantic-cache lookup and store makes one.
            embedding_fallback: ModelPrice::new(0.05, 0.0, 0.05),
        }
    }
}

impl PriceTable {
    /// Look up a model by longest-substring match against the table keys.
    pub fn lookup(&self, model: &str) -> ModelPrice {
        let m = model.to_lowercase();
        let mut best: Option<(&String, &ModelPrice)> = None;
        for (k, v) in &self.entries {
            if m.contains(k.as_str()) {
                match best {
                    Some((bk, _)) if bk.len() >= k.len() => {}
                    _ => best = Some((k, v)),
                }
            }
        }
        best.map(|(_, v)| *v).unwrap_or_else(|| {
            if m.contains("embed") {
                self.embedding_fallback
            } else {
                self.fallback
            }
        })
    }

    /// Override or add a model price (used when loading a local override file).
    pub fn set(&mut self, model: &str, price: ModelPrice) {
        if let Some(slot) = self.entries.iter_mut().find(|(k, _)| k == model) {
            slot.1 = price;
        } else {
            self.entries.push((model.to_string(), price));
        }
    }
}

/// Estimate cost in USD for a single call given token counts.
///
/// `cached_in` are counted at the cached rate; the remaining
/// `tokens_in - cached_in` at the normal input rate.
pub fn estimate_cost_usd(
    table: &PriceTable,
    model: &str,
    tokens_in: u64,
    tokens_out: u64,
    cached_in: u64,
) -> f64 {
    let p = table.lookup(model);
    let cached = cached_in.min(tokens_in);
    let fresh_in = tokens_in.saturating_sub(cached);
    let in_cost = (fresh_in as f64 / 1_000_000.0) * p.input_per_mtok;
    let cached_cost = (cached as f64 / 1_000_000.0) * p.cached_input_per_mtok;
    let out_cost = (tokens_out as f64 / 1_000_000.0) * p.output_per_mtok;
    in_cost + cached_cost + out_cost
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substring_match_resolves_dated_suffix() {
        let t = PriceTable::default();
        let p = t.lookup("claude-3-5-sonnet-20241022");
        assert_eq!(p.input_per_mtok, 3.0);
    }

    #[test]
    fn longest_match_wins_haiku_vs_sonnet() {
        let t = PriceTable::default();
        // "claude-3-5-haiku" must win over any shorter partial.
        let p = t.lookup("claude-3-5-haiku-latest");
        assert_eq!(p.output_per_mtok, 4.0);
    }

    #[test]
    fn cost_counts_cached_at_discount() {
        let t = PriceTable::default();
        // 1M input, all cached, on sonnet: cached rate 0.30 not 3.0.
        let c = estimate_cost_usd(&t, "claude-3-5-sonnet", 1_000_000, 0, 1_000_000);
        assert!((c - 0.30).abs() < 1e-9, "got {c}");
    }

    #[test]
    fn unknown_model_uses_fallback_not_zero() {
        let t = PriceTable::default();
        let c = estimate_cost_usd(&t, "some-future-model", 1_000_000, 0, 0);
        assert!(c > 0.0);
    }

    #[test]
    fn unknown_embedding_model_uses_cheap_fallback_not_chat_fallback() {
        let t = PriceTable::default();
        let c = estimate_cost_usd(&t, "some-future-embedding-model", 1_000_000, 0, 0);
        // Cheap embedding fallback (0.05/mtok), not the chat fallback (3.0/mtok)
        // which would overcharge a semantic-cache lookup by 60x.
        assert!((c - 0.05).abs() < 1e-9, "got {c}");
    }

    #[test]
    fn known_embedding_model_resolves_exactly() {
        let t = PriceTable::default();
        let c = estimate_cost_usd(&t, "text-embedding-3-small", 1_000_000, 0, 0);
        assert!((c - 0.02).abs() < 1e-9, "got {c}");
    }
}
