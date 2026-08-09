//! Configuration and on-disk layout.
//!
//! Everything Halo persists lives under a single base directory (default
//! `~/.halo/`), so it's trivial to back up, inspect, or delete. Real provider
//! secrets are the ONE thing that never lands here -- they go to the OS
//! keychain (see `keys.rs`).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Default loopback ingress address.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:8787";

/// User-editable configuration (`~/.halo/config.yaml`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Address the LLM-API ingress binds to. Loopback-only unless explicitly
    /// changed to a routable address.
    #[serde(default = "default_listen")]
    pub listen: String,

    /// Base URL of the relay to upload telemetry to. When absent, Halo runs
    /// fully local: budgets, cache, and `halo report` all work offline; only
    /// the hosted dashboard is unavailable.
    #[serde(default)]
    pub relay_url: Option<String>,

    /// Bearer token issued at device registration for the relay ingest
    /// endpoint.
    #[serde(default)]
    pub relay_token: Option<String>,

    /// Offline license key unlocking paid-tier features. Either the raw signed
    /// token itself, or a path to a file containing it. Absent / invalid /
    /// expired always resolves to the free tier -- it never blocks startup.
    /// The free tier (budgets, kill switch, exact cache, compression,
    /// prompt-cache injection, MCP cloak/taint, local audit + report) is fully
    /// functional forever without any key.
    #[serde(default)]
    pub license_key: Option<String>,

    /// Webhook URL that budget soft/hard-cap crossings POST to. Paid feature
    /// (`alerting`) -- ignored on the free tier.
    #[serde(default)]
    pub alert_webhook: Option<String>,

    #[serde(default)]
    pub budget: BudgetConfig,

    #[serde(default)]
    pub cache: CacheConfig,

    /// The embedding-similarity ("L2") cache. OFF by default -- unlike every
    /// other cache in Halo, this one spends real (if tiny) money on every
    /// lookup and every store, since it calls an embeddings API. Turning it
    /// on requires an explicit opt-in here AND a stored embedding API key
    /// (`halo embeddings set-key`).
    #[serde(default)]
    pub semantic_cache: SemanticCacheConfig,

    #[serde(default)]
    pub compression: CompressionConfig,

    /// Upstream MCP servers Halo fronts. The agent runtime's MCP config is
    /// pointed at Halo; Halo holds these real server definitions.
    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfig>,

    /// Per-model price overrides. The built-in table is a small, hand-maintained
    /// approximation (unlike LiteLLM's continuously-updated price file) and its
    /// fallback for an unrecognized model is a mid-tier guess -- fine most of the
    /// time, but it can meaningfully over- or under-estimate cost for a model
    /// that isn't in the table, which matters a lot for a budget/kill-switch
    /// product. Override here for anything the built-in table gets wrong.
    #[serde(default)]
    pub price_overrides: Vec<PriceOverride>,

    /// The local admin dashboard: a loopback-only web UI for viewing savings/
    /// logs and editing settings, bundled into the `halo` binary itself (no
    /// relay, no network, no account -- free tier). See `dashboard.rs`.
    #[serde(default)]
    pub dashboard: DashboardConfig,

    /// Outbound egress policy. Empty (default) = unrestricted, today's
    /// behavior. Non-empty = every egress Halo itself initiates (the LLM
    /// provider, the embeddings API, and the relay upload) is checked against
    /// this list first and hard-denied if the host isn't on it -- enforced at
    /// dispatch time, not just logged. Opt-in and additive: an existing
    /// install with no `egress:` block behaves exactly as before.
    #[serde(default)]
    pub egress: EgressConfig,

    /// Encrypt the content-bearing local stores (`cache.redb`'s response
    /// bodies, `semantic_cache.redb`'s stored answer text) at rest, keyed off
    /// `$HALO_VAULT_PASSPHRASE` (the same env var the credential fallback
    /// already uses). Off by default -- no behavior change for existing
    /// installs. When true and the passphrase is unset, `halo serve` refuses
    /// to start: this is the one case where a missing passphrase should
    /// block, because the operator explicitly asked for encryption.
    #[serde(default)]
    pub encrypt_at_rest: bool,
}

fn default_listen() -> String {
    DEFAULT_LISTEN.to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            relay_url: None,
            relay_token: None,
            license_key: None,
            alert_webhook: None,
            budget: BudgetConfig::default(),
            cache: CacheConfig::default(),
            semantic_cache: SemanticCacheConfig::default(),
            compression: CompressionConfig::default(),
            mcp_servers: Vec::new(),
            price_overrides: Vec::new(),
            dashboard: DashboardConfig::default(),
            egress: EgressConfig::default(),
            encrypt_at_rest: false,
        }
    }
}

/// Outbound egress allowlist. See the field doc on `Config::egress`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EgressConfig {
    /// Host names Halo is permitted to send requests to. Empty = allow all
    /// (unrestricted). Matched case-insensitively; a rule beginning with `.`
    /// matches any subdomain (and the apex) of that suffix, e.g. `.example.com`
    /// matches `api.example.com` and `example.com`, but NOT `evil-example.com`.
    /// No scheme, no port, no path -- host only.
    #[serde(default)]
    pub allowed_upstreams: Vec<String>,
}

impl EgressConfig {
    /// True if `host` is permitted -- either no policy is configured (allow
    /// all) or `host` matches an entry exactly or via a `.`-prefixed
    /// subdomain wildcard.
    pub fn permits_host(&self, host: &str) -> bool {
        if self.allowed_upstreams.is_empty() {
            return true;
        }
        let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
        self.allowed_upstreams.iter().any(|rule| {
            let rule = rule.trim().to_ascii_lowercase();
            match rule.strip_prefix('.') {
                Some(suffix) if !suffix.is_empty() => {
                    host == suffix || host.ends_with(&format!(".{suffix}"))
                }
                _ => host == rule,
            }
        })
    }

    /// True if any allowlist is configured at all (used for `halo status` /
    /// dashboard display of the current egress policy).
    pub fn is_restricted(&self) -> bool {
        !self.allowed_upstreams.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardConfig {
    /// On by default -- it's loopback-only and free, so there's no reason to
    /// make a user opt in. Set false to disable entirely.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Separate port from `listen` (the LLM-facing ingress) on purpose: the
    /// dashboard is an admin surface, the proxy is the hot path, and mixing
    /// them on one router would mean every dashboard route has to be proven
    /// safe against the same threat model as untrusted agent traffic.
    #[serde(default = "default_dashboard_listen")]
    pub listen: String,
}

fn default_dashboard_listen() -> String {
    "127.0.0.1:8788".to_string()
}

impl Default for DashboardConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            listen: default_dashboard_listen(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetConfig {
    /// Global soft cap in USD over the rolling window; warn but keep serving.
    #[serde(default)]
    pub soft_cap_usd: Option<f64>,
    /// Global hard cap in USD; refuse requests once exceeded. Enforced locally
    /// and always, even if the relay has never been reachable.
    #[serde(default)]
    pub hard_cap_usd: Option<f64>,
    /// Per-agent overrides.
    #[serde(default)]
    pub per_agent: Vec<AgentBudget>,
    /// Rolling window length in hours the caps apply over.
    #[serde(default = "default_window_hours")]
    pub window_hours: u64,
}

fn default_window_hours() -> u64 {
    24
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            soft_cap_usd: None,
            hard_cap_usd: None,
            per_agent: Vec::new(),
            window_hours: default_window_hours(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentBudget {
    pub agent_id: String,
    #[serde(default)]
    pub soft_cap_usd: Option<f64>,
    #[serde(default)]
    pub hard_cap_usd: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Hard cap on stored entries -- a lesson-learned from the main proxy's
    /// unbounded L1 cache. Enforced from day one, not patched in later.
    #[serde(default = "default_cache_max")]
    pub max_entries: u64,
}

fn default_cache_max() -> u64 {
    10_000
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_entries: default_cache_max(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticCacheConfig {
    /// Off by default -- see the field doc on `Config::semantic_cache`.
    #[serde(default)]
    pub enabled: bool,
    /// "openai" | "ollama" | "mock". Never a local model.
    #[serde(default = "default_embedding_provider")]
    pub provider: String,
    #[serde(default = "default_embedding_model")]
    pub model: String,
    /// Required for "ollama" (points at your own already-running server);
    /// optional override for "openai" (e.g. an OpenAI-compatible proxy).
    #[serde(default)]
    pub base_url: Option<String>,
    /// Minimum cosine similarity to serve a candidate. High by default: a
    /// false-positive semantic hit is a wrong answer served silently, so this
    /// errs conservative. 0.90-0.95 is the typical production range for
    /// short-form Q&A embeddings.
    #[serde(default = "default_similarity_threshold")]
    pub similarity_threshold: f32,
    /// Hard cap on stored entries, same rationale as `CacheConfig::max_entries`.
    #[serde(default = "default_semantic_cache_max")]
    pub max_entries: u64,
}

fn default_embedding_provider() -> String {
    "openai".to_string()
}
fn default_embedding_model() -> String {
    "text-embedding-3-small".to_string()
}
fn default_similarity_threshold() -> f32 {
    0.93
}
fn default_semantic_cache_max() -> u64 {
    2_000
}

impl Default for SemanticCacheConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: default_embedding_provider(),
            model: default_embedding_model(),
            base_url: None,
            similarity_threshold: default_similarity_threshold(),
            max_entries: default_semantic_cache_max(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompressionConfig {
    /// Safe verbose-phrase reduction (multi-word -> shorter phrase, meaning
    /// preserved). On by default.
    #[serde(default = "default_true")]
    pub verbose_phrases: bool,
    /// Collapse 3+ blank lines to 2 and strip trailing line whitespace. Never
    /// touches leading whitespace/indentation, so it can't corrupt
    /// indentation-sensitive pasted content. On by default -- meaning cannot
    /// change either way.
    #[serde(default = "default_true")]
    pub whitespace: bool,
    /// Aggressive single-word/symbol abbreviations (e.g. "and" -> "&"). OFF by
    /// default: these can change meaning, and Halo's rule is never to degrade
    /// output silently.
    #[serde(default)]
    pub aggressive_abbreviations: bool,
    /// Inject Anthropic `cache_control` breakpoints on large/repeated system
    /// prompts, tool definitions, and the first message's attachment-shaped
    /// content blocks (documents/images/large text).
    #[serde(default = "default_true")]
    pub anthropic_cache_control: bool,
}

fn default_true() -> bool {
    true
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            verbose_phrases: true,
            whitespace: true,
            aggressive_abbreviations: false,
            anthropic_cache_control: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceOverride {
    /// Substring matched against the model name, same rule as the built-in
    /// table (longest match wins).
    pub model: String,
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
    /// Defaults to `input_per_mtok` (no cache discount) when omitted.
    #[serde(default)]
    pub cached_input_per_mtok: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Logical name the agent references this server by.
    pub name: String,
    /// Executable to spawn for a stdio MCP server.
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment for the spawned server process.
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
}

/// Resolves and owns the on-disk layout. All paths derive from one base dir.
#[derive(Debug, Clone)]
pub struct Paths {
    pub base: PathBuf,
}

impl Paths {
    /// Resolve the base directory: `$HALO_HOME` if set, else `~/.halo`, else
    /// `./.halo` as a last resort.
    pub fn resolve() -> Self {
        let base = std::env::var_os("HALO_HOME")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join(".halo")))
            .unwrap_or_else(|| PathBuf::from(".halo"));
        Self { base }
    }

    pub fn ensure(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.base)?;
        std::fs::create_dir_all(self.spool_dir())?;
        Ok(())
    }

    pub fn config(&self) -> PathBuf {
        self.base.join("config.yaml")
    }
    pub fn state(&self) -> PathBuf {
        self.base.join("state.json")
    }
    pub fn vkeys(&self) -> PathBuf {
        self.base.join("vkeys.json")
    }
    pub fn ledger(&self) -> PathBuf {
        self.base.join("ledger.redb")
    }
    pub fn cache(&self) -> PathBuf {
        self.base.join("cache.redb")
    }
    pub fn semantic_cache(&self) -> PathBuf {
        self.base.join("semantic_cache.redb")
    }
    pub fn audit(&self) -> PathBuf {
        self.base.join("audit.jsonl")
    }
    pub fn audit_key(&self) -> PathBuf {
        self.base.join("audit-key")
    }
    pub fn spool_dir(&self) -> PathBuf {
        self.base.join("spool")
    }
    pub fn cred_fallback(&self) -> PathBuf {
        self.base.join("cred-fallback.json")
    }
    /// Local bearer token gating dashboard *mutations* (settings writes,
    /// agent revoke). Generated on first use; never transmitted anywhere but
    /// the loopback dashboard itself.
    pub fn dashboard_token(&self) -> PathBuf {
        self.base.join("dashboard-token")
    }
}

impl Config {
    /// Load config from the given path, or return defaults if it doesn't exist.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if !path.exists() {
            return Ok(Config::default());
        }
        let raw = std::fs::read_to_string(path)?;
        let cfg: Config = serde_yaml::from_str(&raw)?;
        Ok(cfg)
    }

    /// Resolve the active entitlements from `license_key`. The value may be the
    /// signed token itself or a path to a file holding it; a path that exists
    /// is read, otherwise the string is treated as the token. Infallible: any
    /// problem degrades to the free tier (see `halo_common::license`).
    pub fn entitlements(&self) -> halo_common::Entitlements {
        let key = self.license_key.as_deref().map(|raw| {
            let trimmed = raw.trim();
            match std::fs::read_to_string(trimmed) {
                Ok(contents) => contents.trim().to_string(),
                Err(_) => trimmed.to_string(),
            }
        });
        halo_common::Entitlements::from_license_key(key.as_deref())
    }

    /// Build the effective price table: built-in defaults with
    /// `price_overrides` applied on top. Shared by `serve` (live billing)
    /// and `report`/`status` (offline recompute) so both use identically
    /// priced numbers.
    pub fn price_table(&self) -> halo_common::pricing::PriceTable {
        let mut prices = halo_common::pricing::PriceTable::default();
        for o in &self.price_overrides {
            prices.set(
                &o.model,
                halo_common::pricing::ModelPrice {
                    input_per_mtok: o.input_per_mtok,
                    output_per_mtok: o.output_per_mtok,
                    cached_input_per_mtok: o.cached_input_per_mtok.unwrap_or(o.input_per_mtok),
                },
            );
        }
        prices
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_allowlist_permits_everything() {
        let e = EgressConfig::default();
        assert!(e.permits_host("api.anthropic.com"));
        assert!(e.permits_host("anything.example.net"));
        assert!(!e.is_restricted());
    }

    #[test]
    fn exact_match_is_permitted_others_denied() {
        let e = EgressConfig {
            allowed_upstreams: vec!["api.anthropic.com".to_string()],
        };
        assert!(e.is_restricted());
        assert!(e.permits_host("api.anthropic.com"));
        assert!(!e.permits_host("api.openai.com"));
    }

    #[test]
    fn dot_prefix_wildcard_matches_subdomain_and_apex() {
        let e = EgressConfig {
            allowed_upstreams: vec![".example.com".to_string()],
        };
        assert!(e.permits_host("example.com"));
        assert!(e.permits_host("api.example.com"));
        assert!(e.permits_host("deep.sub.example.com"));
    }

    #[test]
    fn wildcard_does_not_match_lookalike_suffix() {
        let e = EgressConfig {
            allowed_upstreams: vec![".example.com".to_string()],
        };
        // "evil-example.com" ends with "example.com" as a raw string but is
        // NOT a subdomain of it -- must not match.
        assert!(!e.permits_host("evil-example.com"));
    }

    #[test]
    fn matching_is_case_insensitive_and_ignores_trailing_dot() {
        let e = EgressConfig {
            allowed_upstreams: vec!["API.Anthropic.com".to_string()],
        };
        assert!(e.permits_host("api.anthropic.com."));
        assert!(e.permits_host("API.ANTHROPIC.COM"));
    }
}
