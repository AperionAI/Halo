//! Small shared helpers.

use std::path::Path;

/// Write `bytes` to `path` via a tmp file + rename, chmod `0600` on Unix.
///
/// Ported from `shield-standalone`'s `cloak::atomic_write_0600` -- the same
/// on-disk posture Shield uses for its identity key and cloak vault.
pub fn atomic_write_0600(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.flush()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
    }
    std::fs::rename(&tmp, path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Rough token estimate from character count. Providers bill by token, not
/// char; ~4 chars/token is the widely-used heuristic for English-ish text and
/// is honest enough for a pre-flight budget check. Actual billed tokens come
/// from the provider's usage block on the response and override this.
pub fn approx_tokens_from_chars(chars: usize) -> u64 {
    ((chars as f64) / 4.0).ceil() as u64
}

/// Input-side dollar delta between two prompt sizes. Used on Free to star
/// "Cut would have saved" when compression is not applied to the wire.
pub fn estimated_input_savings_usd(
    prices: &halo_common::pricing::PriceTable,
    model: &str,
    before_chars: usize,
    after_chars: usize,
) -> f64 {
    if after_chars >= before_chars {
        return 0.0;
    }
    let t0 = approx_tokens_from_chars(before_chars);
    let t1 = approx_tokens_from_chars(after_chars);
    let full = halo_common::pricing::estimate_cost_usd(prices, model, t0, 0, 0);
    let cut = halo_common::pricing::estimate_cost_usd(prices, model, t1, 0, 0);
    (full - cut).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimated_input_savings_zero_when_not_smaller() {
        let prices = halo_common::pricing::PriceTable::default();
        assert_eq!(estimated_input_savings_usd(&prices, "gpt-4o", 100, 100), 0.0);
        assert_eq!(estimated_input_savings_usd(&prices, "gpt-4o", 100, 200), 0.0);
    }

    #[test]
    fn estimated_input_savings_positive_when_shrunk() {
        let prices = halo_common::pricing::PriceTable::default();
        let v = estimated_input_savings_usd(&prices, "gpt-4o", 40_000, 20_000);
        assert!(v > 0.0);
    }
}
