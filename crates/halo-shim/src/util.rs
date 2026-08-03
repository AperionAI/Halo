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
