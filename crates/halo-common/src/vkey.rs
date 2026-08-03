//! Virtual key format.
//!
//! A virtual key is what the agent runtime is configured with instead of the
//! real provider key. Format: `sf_live_<agent>_<random>`. The shim maps it
//! back to the real provider credential (held only in the OS keychain).
//! Keeping the agent handle in the key means the shim can attribute spend and
//! apply per-agent budgets by parsing the key alone, without a lookup on the
//! hot path.

use serde::{Deserialize, Serialize};

/// Prefix marking a Halo-issued virtual key.
pub const VKEY_PREFIX: &str = "sf_live_";

/// Metadata persisted per issued virtual key. The real provider secret is
/// NEVER stored here -- it lives in the OS keychain, keyed by `agent_id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VirtualKeyRecord {
    pub agent_id: String,
    /// The full virtual key string handed to the agent.
    pub virtual_key: String,
    /// Which provider the real key behind this agent belongs to.
    pub provider: crate::telemetry::Provider,
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Set when the key has been revoked; a revoked key is rejected at ingress.
    #[serde(default)]
    pub revoked_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Optional custom base URL for an endpoint that speaks the same wire
    /// shape as `provider` but isn't the real `api.openai.com` /
    /// `api.anthropic.com` -- Groq/Together/Fireworks/a local vLLM/Ollama
    /// server for OpenAI-shaped traffic, or a Bedrock Anthropic-shape proxy
    /// for Anthropic-shaped traffic. `#[serde(default)]` keeps this backward
    /// compatible with records written before this field existed.
    #[serde(default)]
    pub base_url: Option<String>,
}

impl VirtualKeyRecord {
    pub fn is_active(&self) -> bool {
        self.revoked_at.is_none()
    }
}

/// Extract the agent handle from a virtual key, validating the prefix and
/// shape. Returns `None` for anything that isn't a well-formed Halo key.
pub fn parse_virtual_key(key: &str) -> Option<String> {
    let rest = key.strip_prefix(VKEY_PREFIX)?;
    // rest is "<agent>_<random>"; the random suffix has no underscores, so the
    // agent handle is everything before the final underscore.
    let idx = rest.rfind('_')?;
    let agent = &rest[..idx];
    let random = &rest[idx + 1..];
    if agent.is_empty() || random.is_empty() {
        return None;
    }
    Some(agent.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_agent_handle() {
        assert_eq!(
            parse_virtual_key("sf_live_researcher_ab12cd34").as_deref(),
            Some("researcher")
        );
    }

    #[test]
    fn agent_handle_may_contain_hyphen() {
        assert_eq!(
            parse_virtual_key("sf_live_data-team_xyz").as_deref(),
            Some("data-team")
        );
    }

    #[test]
    fn rejects_foreign_keys() {
        assert!(parse_virtual_key("sk-ant-api03-abc").is_none());
        assert!(parse_virtual_key("sf_live_").is_none());
        assert!(parse_virtual_key("sf_live_noagent").is_none());
    }
}
