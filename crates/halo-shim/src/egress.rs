//! Outbound egress denylist + allowlist enforcement.
//!
//! One tiny, shared check called at every place Halo itself initiates a
//! network request: the LLM provider dispatch, the embeddings API call, and
//! the relay telemetry upload. Deliberately NOT a generic HTTP-client
//! wrapper -- three explicit call sites are easier to audit than a shared
//! client that could silently grow a fourth egress path nobody checks.
//!
//! This is a proxy-side, fail-closed control: even a fully automated,
//! prompt-injected, or misconfigured agent cannot make Halo reach a host
//! on the denylist (or, if an allowlist is set, a host that isn't on it),
//! because the check happens before the request leaves the process.

use crate::config::EgressConfig;

/// Extract the host from `url` and check it against `cfg`. `Ok(())` if
/// permitted. `Err` carries the offending host (or the raw url, if it
/// couldn't even be parsed) for the caller to fold into an error message
/// / audit entry.
pub fn check_egress(cfg: &EgressConfig, url: &str) -> Result<(), String> {
    let host = reqwest::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_string));
    match host {
        Some(h) if cfg.permits_host(&h) => Ok(()),
        Some(h) => Err(h),
        None => Err(url.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EgressConfig;

    fn cfg(allow: &[&str], deny: &[&str]) -> EgressConfig {
        EgressConfig {
            allowed_upstreams: allow.iter().map(|s| s.to_string()).collect(),
            denied_upstreams: deny.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn starter_denies_metadata_host() {
        let e = cfg(&[], &[]);
        let err = check_egress(&e, "http://169.254.169.254/latest/meta-data").unwrap_err();
        assert_eq!(err, "169.254.169.254");
    }

    #[test]
    fn custom_deny() {
        let e = cfg(&[], &["evil.example.com"]);
        assert!(check_egress(&e, "https://evil.example.com/v1").is_err());
        assert!(check_egress(&e, "https://api.anthropic.com/v1/messages").is_ok());
    }

    #[test]
    fn allowlist_still_works() {
        let e = cfg(&["api.anthropic.com"], &[]);
        assert!(check_egress(&e, "https://api.anthropic.com/v1/messages").is_ok());
        assert!(check_egress(&e, "https://api.openai.com/v1/embeddings").is_err());
    }

    #[test]
    fn empty_extra_rules_do_not_open_metadata() {
        let e = cfg(&[], &[]);
        assert!(check_egress(&e, "http://169.254.169.254/").is_err());
        assert!(check_egress(&e, "https://metadata.google.internal/").is_err());
        assert!(check_egress(&e, "https://api.openai.com/v1/embeddings").is_ok());
        assert!(check_egress(&e, "https://api.anthropic.com/v1/messages").is_ok());
    }

    #[test]
    fn no_allowlist_allows_any_non_denied_url() {
        let e = cfg(&[], &[]);
        assert!(check_egress(&e, "https://api.anthropic.com/v1/messages").is_ok());
    }

    #[test]
    fn allowed_host_passes() {
        let e = cfg(&["api.anthropic.com"], &[]);
        assert!(check_egress(&e, "https://api.anthropic.com/v1/messages").is_ok());
    }

    #[test]
    fn disallowed_host_is_denied_with_the_host_in_the_error() {
        let e = cfg(&["api.anthropic.com"], &[]);
        let err = check_egress(&e, "https://evil.example.com/v1/messages").unwrap_err();
        assert_eq!(err, "evil.example.com");
    }

    #[test]
    fn malformed_url_is_denied() {
        let e = cfg(&["api.anthropic.com"], &[]);
        assert!(check_egress(&e, "not a url").is_err());
    }

    #[test]
    fn wildcard_subdomain_rule_permits_matching_provider_endpoint() {
        let e = cfg(&[".openai.com"], &[]);
        assert!(check_egress(&e, "https://api.openai.com/v1/embeddings").is_ok());
    }
}
