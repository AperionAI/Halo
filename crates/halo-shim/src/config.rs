//! Configuration and on-disk layout.
//!
//! Everything Halo persists lives under a single base directory (default
//! `~/.halo/`), so it's trivial to back up, inspect, or delete. Real provider
//! secrets are the ONE thing that never lands here -- they go to the OS
//! keychain (see `keys.rs`).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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
    /// Free is the firewall (caps, kill switch, denylist, meter, 7-day
    /// history). Cache, compression, and prompt-cache injection are Cut.
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

    /// Refuse an MCP tool call *before* it is forwarded if the arguments
    /// contain uncloaked secret shapes (API keys, tokens). Default on. The
    /// audit log still records the kinds. Cloaked `{{cloak:NAME}}` placeholders
    /// are resolved after this check and are not a block.
    #[serde(default = "default_true")]
    pub mcp_block_uncloaked_secrets: bool,

    /// Route-tier failover: agent id -> backup agent id. On transport error
    /// or 502/503/504/529, Halo retries once with the backup agent's
    /// provider/key. Ignored on Free/Cut. No recursive hops.
    #[serde(default)]
    pub failover: BTreeMap<String, String>,

    /// Route-tier task-class routing. `by_class` maps a class (`chat`,
    /// `embedding`, `code`, or `X-Halo-Task-Class`) to another agent whose
    /// provider/key is used on the wire. `models` optionally rewrites the
    /// request model for that class (the GLM cheap lane). Spend still hits
    /// the inbound agent's cap.
    #[serde(default)]
    pub routing: RoutingConfig,

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

    /// Outbound egress policy. A built-in starter denylist always applies
    /// (cloud metadata + a few common exfil sinks); `denied_upstreams` adds
    /// to it. `allowed_upstreams` is still opt-in: empty = any non-denied
    /// host, non-empty = only those hosts (after the denylist). Enforced at
    /// dispatch time on every egress Halo itself initiates.
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
            mcp_block_uncloaked_secrets: true,
            failover: BTreeMap::new(),
            routing: RoutingConfig::default(),
            price_overrides: Vec::new(),
            dashboard: DashboardConfig::default(),
            egress: EgressConfig::default(),
            encrypt_at_rest: false,
        }
    }
}

/// Hosts the starter denylist always blocks, even if `denied_upstreams` is
/// empty. Cloud instance-metadata endpoints plus a short list of paste/exfil
/// sinks. Never includes `api.openai.com` / `api.anthropic.com`.
pub const STARTER_DENIED_UPSTREAMS: &[&str] = &[
    "169.254.169.254",
    "169.254.170.2",
    "fd00:ec2::254",
    "metadata.google.internal",
    ".webhook.site",
    ".requestbin.com",
    ".pipedream.net",
];

/// Provider hosts that a denylist entry must not be able to block. They can
/// still fail an allowlist (region-lock) if one is configured.
const NEVER_DENY: &[&str] = &["api.openai.com", "api.anthropic.com"];

/// Outbound egress policy. See the field doc on `Config::egress`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EgressConfig {
    /// Host names Halo is permitted to send requests to. Empty = allow any
    /// host that isn't denied. Matched case-insensitively; a rule beginning
    /// with `.` matches any subdomain (and the apex) of that suffix, e.g.
    /// `.example.com` matches `api.example.com` and `example.com`, but NOT
    /// `evil-example.com`. No scheme, no port, no path -- host only.
    #[serde(default)]
    pub allowed_upstreams: Vec<String>,
    /// Extra deny rules on top of [`STARTER_DENIED_UPSTREAMS`]. Deny wins
    /// over the allowlist. Empty extras do not disable the starter list.
    #[serde(default)]
    pub denied_upstreams: Vec<String>,
}

impl EgressConfig {
    /// Starter denylist plus any extra `denied_upstreams` (deduped).
    pub fn effective_denied(&self) -> Vec<String> {
        let mut out: Vec<String> = STARTER_DENIED_UPSTREAMS
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        for extra in &self.denied_upstreams {
            let extra = extra.trim();
            if extra.is_empty() {
                continue;
            }
            if !out.iter().any(|s| s.eq_ignore_ascii_case(extra)) {
                out.push(extra.to_string());
            }
        }
        out
    }

    /// True if `host` is permitted. Deny (starter ∪ extras) wins; then the
    /// allowlist if configured; otherwise allow. `api.openai.com` and
    /// `api.anthropic.com` skip the denylist so a mis-edit can't cut the two
    /// default providers, but they still honor an allowlist.
    pub fn permits_host(&self, host: &str) -> bool {
        let host = normalize_host(host);
        if !is_never_deny(&host) && host_matches_any(&host, &self.effective_denied()) {
            return false;
        }
        if self.allowed_upstreams.is_empty() {
            return true;
        }
        host_matches_any(&host, &self.allowed_upstreams)
    }

    /// True if an allowlist is configured (used for `halo status` / dashboard).
    pub fn is_restricted(&self) -> bool {
        !self.allowed_upstreams.is_empty()
    }
}

impl Default for EgressConfig {
    fn default() -> Self {
        Self {
            allowed_upstreams: Vec::new(),
            // Copied into the first-run yaml so the operator can see what's
            // blocked. `effective_denied` still unions with the const, so
            // emptying this list does not open metadata hosts.
            denied_upstreams: STARTER_DENIED_UPSTREAMS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
        }
    }
}

fn normalize_host(host: &str) -> String {
    host.trim().trim_end_matches('.').to_ascii_lowercase()
}

fn is_never_deny(host: &str) -> bool {
    NEVER_DENY.contains(&host)
}

fn host_matches_any(host: &str, rules: &[String]) -> bool {
    rules.iter().any(|rule| host_matches_rule(host, rule))
}

fn host_matches_rule(host: &str, rule: &str) -> bool {
    let rule = rule.trim().to_ascii_lowercase();
    match rule.strip_prefix('.') {
        Some(suffix) if !suffix.is_empty() => {
            host == suffix || host.ends_with(&format!(".{suffix}"))
        }
        _ => host == rule,
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RoutingConfig {
    /// task class -> agent id whose provider key is used on the wire.
    #[serde(default)]
    pub by_class: BTreeMap<String, String>,
    /// task class -> model name rewrite (e.g. `code: glm-4.7`).
    #[serde(default)]
    pub models: BTreeMap<String, String>,
    /// Lowest-cost / effort router. Route tier. Default `auto` with
    /// `efficient_agent: glm`; hops only fire if that agent exists.
    #[serde(default)]
    pub effort: EffortConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffortConfig {
    /// `off` / `auto` / `local` / `frontier`. Default auto on Route.
    #[serde(default = "default_effort_mode")]
    pub mode: String,
    /// Confidence bar for `auto`. Default 0.5.
    #[serde(default = "default_effort_threshold")]
    pub threshold: f64,
    /// Agent id whose key/provider is used on Efficient hops.
    #[serde(default = "default_efficient_agent")]
    pub efficient_agent: Option<String>,
    /// Agent id whose key/provider is used on Capable hops.
    /// None = keep the inbound agent for hard turns.
    #[serde(default)]
    pub capable_agent: Option<String>,
    /// Model rewrite on Efficient hops. Required when the cheap agent is a
    /// different provider (Claude → OpenAI glm). If unset, Halo picks a
    /// same-family cheap default from the hop provider.
    #[serde(default)]
    pub efficient_model: Option<String>,
    /// Model rewrite on Capable hops. None = keep the inbound model.
    #[serde(default)]
    pub capable_model: Option<String>,
}

fn default_effort_threshold() -> f64 {
    0.5
}

fn default_effort_mode() -> String {
    "auto".into()
}

fn default_efficient_agent() -> Option<String> {
    Some("glm".into())
}

impl Default for EffortConfig {
    fn default() -> Self {
        Self {
            mode: default_effort_mode(),
            threshold: default_effort_threshold(),
            efficient_agent: default_efficient_agent(),
            capable_agent: None,
            efficient_model: None,
            capable_model: None,
        }
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
    /// Fresh installs materialize $25; existing files without this field stay
    /// uncapped (`None`).
    #[serde(default)]
    pub soft_cap_usd: Option<f64>,
    /// Global hard cap in USD; refuse requests once exceeded. Enforced locally
    /// and always, even if the relay has never been reachable. Fresh installs
    /// materialize $50.
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
            soft_cap_usd: Some(25.0),
            hard_cap_usd: Some(50.0),
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

    /// First-run write: if `path` is missing, persist [`Config::default`]
    /// (armed $25/$50 caps, starter denylist in comments via the struct
    /// defaults) so a fresh install refuses a runaway without YAML hunting.
    /// Existing files are left untouched.
    pub fn load_or_materialize(path: &Path) -> anyhow::Result<Self> {
        if path.exists() {
            return Self::load(path);
        }
        let cfg = Config::default();
        let yaml = serde_yaml::to_string(&cfg)?;
        crate::util::atomic_write_0600(path, yaml.as_bytes())?;
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

    /// Write a signed license token to `license.key` (0600) and point
    /// `license_key` in config.yaml at that file. `raw` may be the token
    /// itself or a path to a file holding it.
    pub fn apply_license_token(paths: &Paths, raw: &str) -> anyhow::Result<std::path::PathBuf> {
        let token = resolve_license_token(raw)?;
        paths.ensure()?;
        let key_path = paths.base.join("license.key");
        crate::util::atomic_write_0600(&key_path, format!("{token}\n").as_bytes())?;
        let cfg_path = paths.config();
        let mut cfg = Self::load_or_materialize(&cfg_path)?;
        cfg.license_key = Some(key_path.to_string_lossy().into_owned());
        let yaml = serde_yaml::to_string(&cfg)?;
        crate::util::atomic_write_0600(&cfg_path, yaml.as_bytes())?;
        Ok(key_path)
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

fn resolve_license_token(raw: &str) -> anyhow::Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        anyhow::bail!("empty license token");
    }
    let path = Path::new(trimmed);
    if path.is_file() {
        let contents = std::fs::read_to_string(path)?;
        let token = contents.trim();
        if token.is_empty() {
            anyhow::bail!("license file is empty: {}", path.display());
        }
        return Ok(token.to_string());
    }
    Ok(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allow(rules: &[&str]) -> EgressConfig {
        EgressConfig {
            allowed_upstreams: rules.iter().map(|s| s.to_string()).collect(),
            denied_upstreams: Vec::new(),
        }
    }

    #[test]
    fn mcp_block_defaults_on_and_failover_empty() {
        let cfg: Config = serde_yaml::from_str("listen: 127.0.0.1:8787\n").unwrap();
        assert!(cfg.mcp_block_uncloaked_secrets);
        assert!(cfg.failover.is_empty());
        assert!(cfg.routing.by_class.is_empty());
        assert_eq!(cfg.routing.effort.mode, "auto");
        assert_eq!(cfg.routing.effort.efficient_agent.as_deref(), Some("glm"));
        assert!(cfg.routing.effort.efficient_model.is_none());
        assert!(cfg.routing.effort.capable_model.is_none());
    }

    #[test]
    fn routing_by_class_deserializes() {
        let cfg: Config = serde_yaml::from_str(
            "routing:\n  by_class:\n    code: glm\n  models:\n    code: glm-4.7\n  effort:\n    mode: auto\n    threshold: 0.5\n    efficient_agent: glm\n    efficient_model: glm-4.7\n",
        )
        .unwrap();
        assert_eq!(cfg.routing.by_class.get("code").unwrap(), "glm");
        assert_eq!(cfg.routing.models.get("code").unwrap(), "glm-4.7");
        assert_eq!(cfg.routing.effort.mode, "auto");
        assert_eq!(cfg.routing.effort.efficient_agent.as_deref(), Some("glm"));
        assert_eq!(
            cfg.routing.effort.efficient_model.as_deref(),
            Some("glm-4.7")
        );
    }

    #[test]
    fn empty_allowlist_permits_providers_but_not_metadata() {
        let e = EgressConfig {
            allowed_upstreams: Vec::new(),
            denied_upstreams: Vec::new(),
        };
        assert!(e.permits_host("api.anthropic.com"));
        assert!(e.permits_host("api.openai.com"));
        assert!(e.permits_host("anything.example.net"));
        assert!(!e.permits_host("169.254.169.254"));
        assert!(!e.is_restricted());
    }

    #[test]
    fn starter_denies_metadata_even_with_empty_extras() {
        let e = EgressConfig {
            allowed_upstreams: Vec::new(),
            denied_upstreams: Vec::new(),
        };
        assert!(!e.permits_host("169.254.169.254"));
        assert!(!e.permits_host("metadata.google.internal"));
        assert!(!e.permits_host("webhook.site"));
        assert!(!e.permits_host("foo.requestbin.com"));
    }

    #[test]
    fn custom_deny_blocks_extra_host() {
        let e = EgressConfig {
            allowed_upstreams: Vec::new(),
            denied_upstreams: vec!["evil.example.com".to_string()],
        };
        assert!(!e.permits_host("evil.example.com"));
        assert!(e.permits_host("api.anthropic.com"));
    }

    #[test]
    fn deny_wins_over_allowlist() {
        let e = EgressConfig {
            allowed_upstreams: vec!["169.254.169.254".to_string()],
            denied_upstreams: Vec::new(),
        };
        assert!(!e.permits_host("169.254.169.254"));
        assert!(!e.permits_host("api.openai.com"));
    }

    #[test]
    fn never_deny_openai_or_anthropic_via_denylist() {
        let e = EgressConfig {
            allowed_upstreams: Vec::new(),
            denied_upstreams: vec!["api.openai.com".into(), "api.anthropic.com".into()],
        };
        assert!(e.permits_host("api.openai.com"));
        assert!(e.permits_host("api.anthropic.com"));
    }

    #[test]
    fn exact_match_is_permitted_others_denied() {
        let e = allow(&["api.anthropic.com"]);
        assert!(e.is_restricted());
        assert!(e.permits_host("api.anthropic.com"));
        assert!(!e.permits_host("api.openai.com"));
    }

    #[test]
    fn dot_prefix_wildcard_matches_subdomain_and_apex() {
        let e = allow(&[".example.com"]);
        assert!(e.permits_host("example.com"));
        assert!(e.permits_host("api.example.com"));
        assert!(e.permits_host("deep.sub.example.com"));
    }

    #[test]
    fn wildcard_does_not_match_lookalike_suffix() {
        let e = allow(&[".example.com"]);
        assert!(!e.permits_host("evil-example.com"));
    }

    #[test]
    fn matching_is_case_insensitive_and_ignores_trailing_dot() {
        let e = allow(&["API.Anthropic.com"]);
        assert!(e.permits_host("api.anthropic.com."));
        assert!(e.permits_host("API.ANTHROPIC.COM"));
    }

    #[test]
    fn default_budget_caps_are_armed() {
        let b = BudgetConfig::default();
        assert_eq!(b.soft_cap_usd, Some(25.0));
        assert_eq!(b.hard_cap_usd, Some(50.0));
        assert_eq!(b.window_hours, 24);
    }

    #[test]
    fn existing_yaml_without_caps_stays_uncapped() {
        let cfg: Config = serde_yaml::from_str("listen: \"127.0.0.1:8787\"\n").unwrap();
        let with_window: Config = serde_yaml::from_str("budget:\n  window_hours: 24\n").unwrap();
        assert_eq!(with_window.budget.soft_cap_usd, None);
        assert_eq!(with_window.budget.hard_cap_usd, None);
        assert_eq!(cfg.budget.soft_cap_usd, Some(25.0));
    }

    #[test]
    fn load_or_materialize_writes_armed_defaults_once() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.yaml");
        let first = Config::load_or_materialize(&path).unwrap();
        assert!(path.exists());
        assert_eq!(first.budget.hard_cap_usd, Some(50.0));
        assert_eq!(first.budget.soft_cap_usd, Some(25.0));
        std::fs::write(&path, "budget:\n  hard_cap_usd: 9.0\n  window_hours: 24\n").unwrap();
        let second = Config::load_or_materialize(&path).unwrap();
        assert_eq!(second.budget.hard_cap_usd, Some(9.0));
        assert_eq!(second.budget.soft_cap_usd, None);
    }

    #[test]
    fn apply_license_token_writes_key_file_and_config_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let paths = Paths {
            base: dir.path().to_path_buf(),
        };
        let token = "halo.test-token-not-real";
        let key_path = Config::apply_license_token(&paths, token).unwrap();
        assert_eq!(key_path, paths.base.join("license.key"));
        assert_eq!(std::fs::read_to_string(&key_path).unwrap().trim(), token);
        let cfg = Config::load(&paths.config()).unwrap();
        assert_eq!(
            cfg.license_key.as_deref(),
            Some(key_path.to_str().unwrap())
        );
        assert_eq!(cfg.budget.hard_cap_usd, Some(50.0));
    }

    #[test]
    fn apply_license_token_reads_from_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let src = dir.path().join("from-checkout.txt");
        std::fs::write(&src, "  token-from-file  \n").unwrap();
        let paths = Paths {
            base: dir.path().join("halo-home"),
        };
        let key_path = Config::apply_license_token(&paths, src.to_str().unwrap()).unwrap();
        assert_eq!(std::fs::read_to_string(&key_path).unwrap().trim(), "token-from-file");
    }
}
