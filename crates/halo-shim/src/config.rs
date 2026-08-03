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

    #[serde(default)]
    pub budget: BudgetConfig,

    #[serde(default)]
    pub cache: CacheConfig,

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
            budget: BudgetConfig::default(),
            cache: CacheConfig::default(),
            compression: CompressionConfig::default(),
            mcp_servers: Vec::new(),
            price_overrides: Vec::new(),
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
pub struct CompressionConfig {
    /// Safe verbose-phrase reduction (multi-word -> shorter phrase, meaning
    /// preserved). On by default.
    #[serde(default = "default_true")]
    pub verbose_phrases: bool,
    /// Aggressive single-word/symbol abbreviations (e.g. "and" -> "&"). OFF by
    /// default: these can change meaning, and Halo's rule is never to degrade
    /// output silently.
    #[serde(default)]
    pub aggressive_abbreviations: bool,
    /// Inject Anthropic `cache_control` breakpoints on large/repeated system
    /// prompts.
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
}
