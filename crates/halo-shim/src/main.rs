//! Smartflow Halo shim -- the `halo` binary.

mod alert;
mod answer;
mod audit;
mod budget;
mod cache;
mod cache_control;
mod cachekey;
mod compress;
mod config;
mod dashboard;
mod egress;
mod embeddings;
mod ingress;
mod keys;
mod mcp;
mod openclaw;
mod registry;
mod report;
mod revocations;
mod semantic_cache;
mod service;
mod state;
mod streaming;
mod telemetry;
mod util;
mod vault;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use config::{Config, Paths};
use halo_common::telemetry::Provider;
use std::io::Write;
use std::sync::{Arc, Mutex};

#[derive(Parser)]
#[command(
    name = "halo",
    version,
    about = "Smartflow Halo -- local governance proxy for self-hosted agents"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start the local proxy (default). Point your agent runtime at it.
    Serve,
    /// Manage agents and their virtual keys.
    Agent {
        #[command(subcommand)]
        action: AgentCmd,
    },
    /// Show live spend by agent and current caps.
    Status,
    /// Local COGS / savings report (works fully offline).
    Report {
        /// Only include the last N hours (clamped to the tier history cap).
        #[arg(long)]
        hours: Option<i64>,
        /// `text` (default) or `json` for an exportable file.
        #[arg(long, value_enum, default_value_t = ReportFormat::Text)]
        format: ReportFormat,
        /// Write the report to this path instead of stdout.
        #[arg(long)]
        out: Option<std::path::PathBuf>,
    },
    /// Emergency stop: revoke an agent's key so the proxy refuses it at once.
    Kill {
        agent: String,
    },
    /// Manage the semantic cache's embedding API credential.
    Embeddings {
        #[command(subcommand)]
        action: EmbeddingsCmd,
    },
    /// Show or issue license entitlements.
    License {
        #[command(subcommand)]
        action: LicenseCmd,
    },
    /// Manage the local admin dashboard (http://127.0.0.1:8788 by default).
    Dashboard {
        #[command(subcommand)]
        action: DashboardCmd,
    },
    /// AI usage / governance registry: which agents exist, what they've
    /// touched, and what they've cost -- metadata-only, no prompt/response
    /// content. Works fully offline; no dashboard required.
    Registry {
        #[command(subcommand)]
        action: RegistryCmd,
    },
    /// Install or remove Halo as an always-on background service so it
    /// survives logout and reboot (macOS launchd; Linux systemd is a
    /// documented template for now). Run with sudo.
    Service {
        #[command(subcommand)]
        action: ServiceCmd,
    },
    /// Point an OpenClaw Gateway at this Halo install. Env vars do not
    /// work for OpenClaw -- this writes the field-verified config + auth
    /// patches. See docs/OPENCLAW_INTEGRATION.md.
    Openclaw {
        #[command(subcommand)]
        action: OpenclawCmd,
    },
}

#[derive(Subcommand)]
enum OpenclawCmd {
    /// Patch OpenClaw's config + auth store so traffic goes through Halo.
    Apply {
        /// Halo agent id from `halo agent add`.
        #[arg(long)]
        agent: String,
        /// OpenClaw home directory. Defaults to ~/.openclaw.
        #[arg(long)]
        home: Option<std::path::PathBuf>,
        /// OpenClaw runtime agent directory under agents/. Discovered if omitted.
        #[arg(long)]
        runtime_agent: Option<String>,
        /// Print the patched files and do not write.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum ServiceCmd {
    /// Generate the wrapper, passphrase file, log dir, and LaunchDaemon, then
    /// load it. Run with sudo.
    Install {
        /// User the service runs as (e.g. the agent runtime's service user).
        /// Defaults to $SUDO_USER, then root.
        #[arg(long)]
        user: Option<String>,
    },
    /// Unload and remove the service (leaves the data dir and passphrase in
    /// place). Run with sudo.
    Uninstall,
}

#[derive(Subcommand)]
enum RegistryCmd {
    /// Print the registry to stdout (or write it to `--out`).
    Export {
        #[arg(long, value_enum, default_value_t = RegistryFormat::Json)]
        format: RegistryFormat,
        #[arg(long)]
        out: Option<std::path::PathBuf>,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum RegistryFormat {
    Json,
    Csv,
}

#[derive(Clone, Copy, ValueEnum)]
enum ReportFormat {
    Text,
    Json,
}

#[derive(Subcommand)]
enum DashboardCmd {
    /// Print the local token required to change settings or revoke an agent
    /// from the dashboard. Read-only views need no token (loopback-only).
    Token {
        /// Discard the existing token and mint a new one.
        #[arg(long)]
        regenerate: bool,
    },
}

#[derive(Subcommand)]
enum LicenseCmd {
    /// Show the currently active tier, features, and expiry.
    Show,
    /// Issuer-only: mint a signed license key (Aperion internal). Requires the
    /// signing key -- the same 32-byte Ed25519 seed spec compass uses:
    /// `file:<path>`, `env:<VAR>`, `base64:<...>`, `hex:<...>`, or a bare
    /// base64/hex seed.
    Issue {
        #[arg(long)]
        org: String,
        /// Display label for the tier (`cut`, `route`, `govern`, or a legacy
        /// `pro`/`team` name). Empty `--feature` list fills Cut/Route/Govern
        /// defaults from the name.
        #[arg(long, default_value = "cut")]
        tier: String,
        #[arg(long, default_value_t = 1)]
        seats: u32,
        /// Repeatable feature flag (alerting, remote_kill,
        /// semantic_cache_unlimited, subject_attribution, multi_seat).
        #[arg(long = "feature")]
        features: Vec<String>,
        /// Days until the license expires.
        #[arg(long, default_value_t = 365)]
        days: i64,
        /// 32-byte Ed25519 signing-key seed spec (see above).
        #[arg(long)]
        signing_key: String,
    },
}

#[derive(Subcommand)]
enum EmbeddingsCmd {
    /// Store the embedding API key used by the semantic cache (OS keychain,
    /// same storage as agent provider keys). Only needed when
    /// `semantic_cache.provider: openai` in config.yaml; `ollama` talks to
    /// your own server with no key, `mock` needs nothing.
    SetKey {
        /// If omitted, read from $HALO_EMBEDDING_KEY or stdin.
        #[arg(long)]
        key: Option<String>,
    },
}

#[derive(Subcommand)]
enum AgentCmd {
    /// Register an agent: mint a virtual key mapped to a real provider key.
    Add {
        name: String,
        #[arg(long, value_enum)]
        provider: ProviderArg,
        /// Real provider key. If omitted, read from $HALO_PROVIDER_KEY or stdin.
        #[arg(long)]
        key: Option<String>,
        /// Custom base URL speaking the same wire shape as the chosen
        /// provider (Groq/Together/Fireworks/a local vLLM/Ollama server for
        /// --provider openai; a Bedrock Anthropic-shape proxy or local mock
        /// for --provider anthropic).
        #[arg(long)]
        base_url: Option<String>,
    },
    /// List registered agents.
    List,
    /// Revoke an agent's virtual key and delete its stored secret.
    Revoke { name: String },
}

#[derive(Copy, Clone, ValueEnum)]
enum ProviderArg {
    Anthropic,
    Openai,
}

impl From<ProviderArg> for Provider {
    fn from(p: ProviderArg) -> Self {
        match p {
            ProviderArg::Anthropic => Provider::Anthropic,
            ProviderArg::Openai => Provider::Openai,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let paths = Paths::resolve();
    paths.ensure().context("creating ~/.halo")?;
    // First `halo` invocation writes an armed default config (caps + starter
    // denylist) so a fresh install is a firewall without YAML hunting.
    let _ = Config::load_or_materialize(&paths.config())?;

    match cli.cmd.unwrap_or(Cmd::Serve) {
        Cmd::Serve => serve(paths).await,
        Cmd::Agent { action } => agent_cmd(paths, action),
        Cmd::Status => status(paths),
        Cmd::Report { hours, format, out } => report_cmd(paths, hours, format, out),
        Cmd::Kill { agent } => kill(paths, &agent),
        Cmd::Embeddings { action } => embeddings_cmd(paths, action),
        Cmd::License { action } => license_cmd(paths, action),
        Cmd::Dashboard { action } => dashboard_cmd(paths, action),
        Cmd::Registry { action } => registry_cmd(paths, action),
        Cmd::Service { action } => service_cmd(action),
        Cmd::Openclaw { action } => openclaw_cmd(paths, action),
    }
}

fn service_cmd(action: ServiceCmd) -> Result<()> {
    match action {
        ServiceCmd::Install { user } => service::install(user),
        ServiceCmd::Uninstall => service::uninstall(),
    }
}

fn openclaw_cmd(paths: Paths, action: OpenclawCmd) -> Result<()> {
    match action {
        OpenclawCmd::Apply {
            agent,
            home,
            runtime_agent,
            dry_run,
        } => {
            let cfg = Config::load(&paths.config())?;
            let ks = keys::KeyStore::new(paths);
            let rec = ks
                .records()?
                .into_iter()
                .find(|r| r.agent_id == agent && r.is_active())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "no active Halo agent '{agent}'. Register one first: \
                         `halo agent add {agent} --provider anthropic`"
                    )
                })?;
            if rec.provider != Provider::Anthropic {
                anyhow::bail!(
                    "halo openclaw apply currently patches the Anthropic provider only \
                     (agent '{agent}' is {}). Register an Anthropic agent, or patch OpenClaw by hand.",
                    rec.provider.as_str()
                );
            }
            let home = match home {
                Some(h) => h,
                None => dirs::home_dir()
                    .ok_or_else(|| anyhow::anyhow!("cannot resolve $HOME"))?
                    .join(".openclaw"),
            };
            let oc = openclaw::OpenclawPaths::resolve(home, runtime_agent.as_deref(), &agent)?;
            let base_url = format!("http://{}", cfg.listen);
            let result = openclaw::apply(&oc, &base_url, &rec.virtual_key, dry_run)?;
            if dry_run {
                println!("dry-run: would write {}", result.config.display());
                println!("{}", result.config_out);
                println!("dry-run: would write {}", result.auth.display());
                println!("{}", result.auth_out);
            } else {
                println!(
                    "Patched OpenClaw runtime agent '{}' to use Halo at {base_url}.",
                    result.runtime_agent
                );
                println!("  {}", result.config.display());
                println!("  {}", result.auth.display());
                println!(
                    "Restart the OpenClaw gateway, then verify with:\n  \
                     sudo lsof -nP -i -a -p <gateway-pid> -r2 | grep -E '8787|:443'"
                );
            }
        }
    }
    Ok(())
}

fn registry_cmd(paths: Paths, action: RegistryCmd) -> Result<()> {
    match action {
        RegistryCmd::Export { format, out } => {
            let cfg = Config::load(&paths.config())?;
            let ks = keys::KeyStore::new(paths.clone());
            let device_id = ks.device_id()?;
            let records = ks.records().unwrap_or_default();
            let telem = telemetry::Telemetry::new(
                device_id.clone(),
                None,
                None,
                paths.spool_dir(),
                paths.base.join("telemetry.jsonl"),
            );
            let events = telem.local_events();
            let entitlements = cfg.entitlements();
            let prices = cfg.price_table();
            let report =
                registry::build_registry(&records, &events, &prices, &cfg.mcp_servers, &entitlements, &device_id);
            let rendered = match format {
                RegistryFormat::Json => serde_json::to_string_pretty(&report)?,
                RegistryFormat::Csv => registry::agents_to_csv(&report),
            };
            match out {
                Some(path) => {
                    std::fs::write(&path, rendered)?;
                    eprintln!("wrote {}", path.display());
                }
                None => println!("{rendered}"),
            }
        }
    }
    Ok(())
}

fn dashboard_cmd(paths: Paths, action: DashboardCmd) -> Result<()> {
    match action {
        DashboardCmd::Token { regenerate } => {
            if regenerate {
                let _ = std::fs::remove_file(paths.dashboard_token());
            }
            let token = dashboard::load_or_create_token(&paths)?;
            println!("{token}");
            eprintln!(
                "\nUse this as the Bearer token / 'dashboard token' field to change \
                 settings or revoke an agent from the dashboard. It never leaves this \
                 machine; `--regenerate` mints a new one (invalidating the old)."
            );
        }
    }
    Ok(())
}

fn license_cmd(paths: Paths, action: LicenseCmd) -> Result<()> {
    match action {
        LicenseCmd::Show => {
            let cfg = Config::load(&paths.config())?;
            let ent = cfg.entitlements();
            println!("Smartflow Halo -- license");
            println!("Ladder:   {}", ent.ladder().as_str());
            println!("Tier:     {}", ent.tier_label);
            println!("Status:   {}", ent.status.label());
            println!("Org:      {}", ent.org.as_deref().unwrap_or("-"));
            if ent.seats > 0 {
                println!("Seats:    {}", ent.seats);
            }
            println!("Expires:  {}", ent.expires_at.as_deref().unwrap_or("-"));
            println!("History:  {}h max", ent.max_history_hours());
            println!("\nFeatures:");
            for f in halo_common::license::feature::ALL {
                let mark = if ent.has(f) { "on " } else { "off" };
                println!("  [{mark}] {f}");
            }
            if matches!(ent.ladder(), halo_common::Ladder::Free) {
                println!(
                    "\nFree is the firewall (caps, kill switch, denylist, 7-day history).\n\
                     Cache and compression stay on so the savings number is real.\n\
                     Cut ($50/mo) unlocks 30-day history. Paste a license key into\n\
                     `license_key` in ~/.halo/config.yaml. Stripe checkout is the next slice."
                );
            }
        }
        LicenseCmd::Issue {
            org,
            tier,
            seats,
            features,
            days,
            signing_key,
        } => {
            let seed = load_signing_seed(&signing_key)?;
            let now = chrono::Utc::now();
            let features = if features.is_empty() {
                halo_common::Entitlements::default_features_for_tier(&tier)
            } else {
                features
            };
            let claims = halo_common::LicenseClaims {
                org,
                tier,
                seats,
                features,
                issued_at: now.to_rfc3339(),
                expires_at: (now + chrono::Duration::days(days)).to_rfc3339(),
            };
            let key = halo_common::license::issue_from_seed(&claims, &seed);
            println!("{key}");
            eprintln!(
                "\nissued for '{}' ({}), {} feature(s), expires in {} days.\n\
                 Paste this into `license_key` in the customer's ~/.halo/config.yaml.",
                claims.org,
                claims.tier,
                claims.features.len(),
                days
            );
        }
    }
    Ok(())
}

/// Decode a 32-byte Ed25519 seed from a spec (`file:`, `env:`, `base64:`,
/// `hex:`, or bare base64/hex). Mirrors compass's seed handling so the issuer
/// workflow is identical across tools.
fn load_signing_seed(spec: &str) -> Result<[u8; 32]> {
    use base64::Engine;
    let raw = if let Some(rest) = spec.strip_prefix("file:") {
        std::fs::read_to_string(rest)
            .with_context(|| format!("reading signing key from {rest}"))?
            .trim()
            .to_string()
    } else if let Some(rest) = spec.strip_prefix("env:") {
        std::env::var(rest).with_context(|| format!("reading signing key from ${rest}"))?
    } else {
        spec.to_string()
    };
    let raw = raw.trim();
    let bytes = if let Some(h) = raw.strip_prefix("hex:") {
        hex::decode(h.trim())?
    } else if let Some(b) = raw.strip_prefix("base64:") {
        base64::engine::general_purpose::STANDARD.decode(b.trim())?
    } else if let Ok(b) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(raw) {
        b
    } else if let Ok(b) = base64::engine::general_purpose::STANDARD.decode(raw) {
        b
    } else {
        hex::decode(raw)?
    };
    if bytes.len() != 32 {
        anyhow::bail!("signing seed must be exactly 32 bytes, got {}", bytes.len());
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Ok(arr)
}

fn embeddings_cmd(paths: Paths, action: EmbeddingsCmd) -> Result<()> {
    let ks = keys::KeyStore::new(paths);
    match action {
        EmbeddingsCmd::SetKey { key } => {
            let secret = match key {
                Some(k) => k,
                None => match std::env::var("HALO_EMBEDDING_KEY") {
                    Ok(k) if !k.is_empty() => k,
                    _ => {
                        eprint!("Enter the embedding API key (input is read from stdin): ");
                        std::io::stderr().flush().ok();
                        let mut line = String::new();
                        std::io::stdin().read_line(&mut line)?;
                        if line.trim().is_empty() {
                            anyhow::bail!("no key provided");
                        }
                        line
                    }
                },
            };
            ks.store_secret(embeddings::EmbeddingClient::key_store_id(), secret.trim())?;
            println!(
                "Stored the embedding API key. Set `semantic_cache.enabled: true` in \
                 ~/.halo/config.yaml to turn on the semantic cache."
            );
        }
    }
    Ok(())
}

/// Pure gate for the free-tier agent-count cap, split out from `agent_cmd`
/// so it's testable without touching the filesystem/keychain. `active_count`
/// is the number of already-active agents *before* adding the new one.
fn check_agent_cap(active_count: usize, entitlements: &halo_common::Entitlements) -> Result<()> {
    if entitlements.has(halo_common::license::feature::MULTI_AGENT_UNLIMITED) {
        return Ok(());
    }
    let limit = halo_common::license::FREE_AGENT_LIMIT as usize;
    if active_count >= limit {
        anyhow::bail!(
            "free tier is limited to {limit} registered agents (you have {active_count}). \
             Revoke one with `halo agent revoke <name>`, or set `license_key` in \
             ~/.halo/config.yaml to lift the cap (`multi_agent_unlimited`)."
        );
    }
    Ok(())
}

fn agent_cmd(paths: Paths, action: AgentCmd) -> Result<()> {
    let cfg = config::Config::load(&paths.config()).unwrap_or_default();
    let listen = cfg.listen.clone();
    let cred_fallback = paths.cred_fallback();
    let ks = keys::KeyStore::new(paths);
    match action {
        AgentCmd::Add {
            name,
            provider,
            key,
            base_url,
        } => {
            let active_count = ks.records()?.iter().filter(|r| r.is_active()).count();
            check_agent_cap(active_count, &cfg.entitlements())?;
            let secret = match key {
                Some(k) => k,
                None => read_secret()?,
            };
            let (vkey, backend) = ks.issue(&name, provider.into(), secret.trim(), base_url.clone())?;
            println!("Registered agent '{name}'. Configure your runtime with:\n");
            match Provider::from(provider) {
                Provider::Anthropic => {
                    println!("  ANTHROPIC_API_KEY={vkey}");
                    println!("  ANTHROPIC_BASE_URL=http://{listen}");
                    if let Some(u) = &base_url {
                        println!("  (proxied through to {u} -- an Anthropic-shaped endpoint)");
                    }
                }
                _ => {
                    println!("  OPENAI_API_KEY={vkey}");
                    println!("  OPENAI_BASE_URL=http://{listen}/v1");
                    if let Some(u) = &base_url {
                        println!("  (proxied through to {u} -- an OpenAI-compatible endpoint)");
                    }
                }
            }
            match backend {
                keys::SecretBackend::Keychain => {
                    println!("\nThe real provider key is stored in your OS keychain, never on disk.");
                }
                keys::SecretBackend::EncryptedFile => {
                    println!(
                        "\nNo OS keychain was available (headless box / no GUI session), so the \
                         real provider key was sealed with Argon2id + XChaCha20-Poly1305 in \
                         {}, decryptable only with $HALO_VAULT_PASSPHRASE. It is never written \
                         in plaintext. Keep that passphrase set on every `halo serve` start -- \
                         see docs/HEADLESS.md.",
                        cred_fallback.display()
                    );
                }
            }
        }
        AgentCmd::List => {
            let recs = ks.records()?;
            if recs.is_empty() {
                println!("No agents registered. Add one with `halo agent add <name> --provider ...`.");
            }
            for r in recs {
                let status = if r.is_active() { "active" } else { "revoked" };
                let extra = r
                    .base_url
                    .as_deref()
                    .map(|u| format!("  base_url={u}"))
                    .unwrap_or_default();
                println!(
                    "{:<20} {:<10} {:<8} {}{}",
                    r.agent_id,
                    r.provider.as_str(),
                    status,
                    r.created_at.to_rfc3339(),
                    extra
                );
            }
        }
        AgentCmd::Revoke { name } => {
            ks.revoke(&name)?;
            println!("Revoked agent '{name}' and deleted its stored secret.");
        }
    }
    Ok(())
}

fn read_secret() -> Result<String> {
    if let Ok(k) = std::env::var("HALO_PROVIDER_KEY") {
        if !k.is_empty() {
            return Ok(k);
        }
    }
    eprint!("Enter the real provider key (input is read from stdin): ");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    if line.trim().is_empty() {
        anyhow::bail!("no key provided");
    }
    Ok(line)
}

fn status(paths: Paths) -> Result<()> {
    let cfg = Config::load(&paths.config())?;
    let ks = keys::KeyStore::new(paths.clone());
    let ledger = budget::Ledger::open(&paths.ledger(), cfg.budget.window_hours)?;

    println!("Smartflow Halo -- status");
    println!("Data dir: {}", paths.base.display());
    println!("Listen:  {}", cfg.listen);
    println!(
        "Relay:   {}",
        cfg.relay_url.as_deref().unwrap_or("(local-only mode)")
    );
    let recs = ks.records()?;
    let spend = ledger.spend_by_agent()?;
    let spent_global: f64 = spend.iter().map(|(_, c)| *c).sum();
    println!(
        "Caps:    global soft {:?}  hard {:?}  window {}h",
        cfg.budget.soft_cap_usd, cfg.budget.hard_cap_usd, cfg.budget.window_hours
    );
    match cfg.budget.hard_cap_usd {
        Some(hard) => {
            let remaining = (hard - spent_global).max(0.0);
            println!(
                "         spent ${spent_global:.4} this window, ${remaining:.4} left before the kill switch"
            );
            println!("         raise `budget.hard_cap_usd` in ~/.halo/config.yaml (or the dashboard) to lift it");
        }
        None => println!("         no hard cap set -- a runaway will bill until you add one"),
    }
    let denied = cfg.egress.effective_denied();
    println!(
        "Egress:  deny {} host(s) (starter + extras); {}",
        denied.len(),
        if cfg.egress.is_restricted() {
            format!("allowlist {} host(s)", cfg.egress.allowed_upstreams.len())
        } else {
            "allowlist unrestricted".to_string()
        }
    );
    for h in &denied {
        println!("         - {h}");
    }
    println!(
        "At rest: {}",
        if cfg.encrypt_at_rest { "encrypted (cache + semantic cache)" } else { "plaintext (encrypt_at_rest: false)" }
    );

    println!("\nAgents:");
    if recs.is_empty() {
        println!("  (none)");
    }
    for r in recs.iter().filter(|r| r.is_active()) {
        let s = spend
            .iter()
            .find(|(a, _)| a == &r.agent_id)
            .map(|(_, c)| *c)
            .unwrap_or(0.0);
        println!(
            "  {:<20} {:<10} spend ${:.4} (last {}h)",
            r.agent_id,
            r.provider.as_str(),
            s,
            cfg.budget.window_hours
        );
    }
    Ok(())
}

fn report_cmd(
    paths: Paths,
    hours: Option<i64>,
    format: ReportFormat,
    out: Option<std::path::PathBuf>,
) -> Result<()> {
    let cfg = Config::load(&paths.config())?;
    let log_path = paths.base.join("telemetry.jsonl");
    let json = matches!(format, ReportFormat::Json);

    // Path diagnostics go to stderr so `halo report --format json` is pipeable.
    let log = |s: &str| {
        if json {
            eprintln!("{s}");
        } else {
            println!("{s}");
        }
    };
    log(&format!("Data dir: {}", paths.base.display()));
    if !log_path.exists() {
        log(
            "\nNo telemetry log (telemetry.jsonl) exists in this directory yet -- Halo has \
             not recorded any requests here.\nIf you expected data, check that you're running \
             as the same user (and $HOME / $HALO_HOME) as `halo serve`.\n",
        );
    }

    let telem = telemetry::Telemetry::new(
        String::new(),
        None,
        None,
        paths.spool_dir(),
        log_path.clone(),
    );
    let events = telem.local_events();
    let hours = cfg.entitlements().clamp_history_hours(hours);
    log(&format!(
        "Window:   last {hours}h (tier max {}h)",
        cfg.entitlements().max_history_hours()
    ));
    let since = Some(chrono::Utc::now().timestamp() - hours * 3600);
    let r = report::build(&events, since, &cfg.price_table());
    if log_path.exists() && r.total.requests == 0 {
        log(
            "\nThe telemetry log here has no requests in the selected window. If you expected \
             data, confirm `halo serve` writes to this same directory (same user / $HALO_HOME).\n",
        );
    }
    let body = match format {
        ReportFormat::Text => report::render(&r),
        ReportFormat::Json => report::render_json(&r)?,
    };
    match out {
        Some(path) => {
            std::fs::write(&path, &body)?;
            eprintln!("wrote {}", path.display());
        }
        None => print!("{body}"),
    }
    Ok(())
}

fn kill(paths: Paths, agent: &str) -> Result<()> {
    let ks = keys::KeyStore::new(paths);
    ks.revoke(agent)
        .with_context(|| format!("killing agent '{agent}'"))?;
    println!("KILLED '{agent}': virtual key revoked. The proxy will refuse it immediately.");
    Ok(())
}

async fn serve(paths: Paths) -> Result<()> {
    let cfg = Config::load(&paths.config())?;
    let ks = Arc::new(keys::KeyStore::new(paths.clone()));
    let device_id = ks.device_id()?;

    // Resolve licensing once, up front. Never fails: absent/invalid/expired
    // degrades to the free tier so the proxy always starts.
    let entitlements = cfg.entitlements();
    match entitlements.tier {
        halo_common::Tier::Paid => tracing::info!(
            org = entitlements.org.as_deref().unwrap_or("-"),
            tier = %entitlements.tier_label,
            features = ?entitlements.features,
            "licensed: paid tier active"
        ),
        halo_common::Tier::Free => {
            tracing::info!(status = %entitlements.status.label(), "running on the free tier")
        }
    }

    // Encryption-at-rest is opt-in but, once requested, non-negotiable: a
    // missing passphrase is the one config problem that blocks startup,
    // because silently falling back to plaintext would violate exactly what
    // the operator asked for.
    let vault_passphrase = if cfg.encrypt_at_rest {
        let pass = std::env::var("HALO_VAULT_PASSPHRASE").ok().filter(|s| !s.is_empty());
        if pass.is_none() {
            anyhow::bail!(
                "encrypt_at_rest is true in config.yaml but $HALO_VAULT_PASSPHRASE is unset. \
                 Set it before starting `halo serve`, or set encrypt_at_rest: false."
            );
        }
        pass
    } else {
        None
    };
    if vault_passphrase.is_some() {
        tracing::info!("encrypt_at_rest is on: cache.redb and semantic_cache.redb content is sealed");
    }

    let cache = cache::CacheStore::open_with_encryption(
        &paths.cache(),
        cfg.cache.max_entries,
        cfg.cache.enabled,
        vault_passphrase.clone(),
    )?;
    // Free tier caps the semantic-cache working set; a license lifts it.
    let semantic_max = if entitlements.has(halo_common::license::feature::SEMANTIC_CACHE_UNLIMITED) {
        cfg.semantic_cache.max_entries
    } else {
        let ceiling = halo_common::license::FREE_SEMANTIC_CACHE_MAX_ENTRIES;
        // Only warn when the semantic cache is actually enabled -- an unused,
        // disabled feature carrying a too-high `max_entries` shouldn't spam a
        // warning on every start (the cap is applied either way, silently).
        if cfg.semantic_cache.enabled && cfg.semantic_cache.max_entries > ceiling {
            tracing::info!(
                configured = cfg.semantic_cache.max_entries,
                ceiling,
                "semantic_cache.max_entries capped to the free-tier ceiling \
                 (a license with `semantic_cache_unlimited` lifts it)"
            );
        }
        cfg.semantic_cache.max_entries.min(ceiling)
    };
    let semantic = semantic_cache::SemanticCacheStore::open_with_encryption(
        &paths.semantic_cache(),
        semantic_max,
        vault_passphrase.clone(),
    )?;
    if cfg.semantic_cache.enabled {
        tracing::info!(
            provider = %cfg.semantic_cache.provider,
            model = %cfg.semantic_cache.model,
            threshold = cfg.semantic_cache.similarity_threshold,
            "semantic cache enabled"
        );
    }
    let ledger = budget::Ledger::open(&paths.ledger(), cfg.budget.window_hours)?;
    let audit_log = Arc::new(Mutex::new(audit::AuditLog::open(
        &paths.audit(),
        &paths.audit_key(),
    )?));
    let telem = telemetry::Telemetry::with_egress(
        device_id.clone(),
        cfg.relay_url.clone(),
        cfg.relay_token.clone(),
        paths.spool_dir(),
        paths.base.join("telemetry.jsonl"),
        cfg.egress.clone(),
    );

    let mcp = if cfg.mcp_servers.is_empty() {
        None
    } else {
        match mcp::McpManager::start(&cfg.mcp_servers).await {
            Ok(m) => {
                tracing::info!("MCP servers online: {:?}", m.server_names());
                Some(Arc::new(m))
            }
            Err(e) => {
                tracing::error!("MCP manager failed to start: {e}");
                None
            }
        }
    };

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .unwrap_or_default();

    let prices = cfg.price_table();

    let embedder = Arc::new(embeddings::EmbeddingClient::with_egress(
        embeddings::EmbeddingProviderKind::parse(&cfg.semantic_cache.provider),
        cfg.semantic_cache.model.clone(),
        cfg.semantic_cache.base_url.clone(),
        http.clone(),
        cfg.egress.clone(),
    ));

    let listen = cfg.listen.clone();

    // Remote kill overlay: only active with a relay AND the `remote_kill`
    // entitlement. Everything else about the proxy is unchanged when it's off.
    let remote_revocations = revocations::RemoteRevocations::new();
    let remote_kill_on = entitlements.has(halo_common::license::feature::REMOTE_KILL)
        && cfg.relay_url.is_some();
    if remote_kill_on {
        let relay_url = cfg.relay_url.clone().expect("checked is_some");
        tracing::info!("remote kill enabled (best-effort; local kill switch remains authoritative)");
        tokio::spawn(revocations::poll_loop(
            http.clone(),
            relay_url,
            cfg.relay_token.clone(),
            device_id.clone(),
            remote_revocations.clone(),
        ));
    }

    let dashboard_cfg = cfg.dashboard.clone();

    let state = state::AppState {
        cfg: Arc::new(cfg),
        entitlements: Arc::new(entitlements),
        keys: ks,
        cache,
        semantic,
        embedder,
        ledger,
        audit_log,
        telem: telem.clone(),
        injector: Arc::new(cache_control::CacheControlInjector::new()),
        mcp,
        prices: Arc::new(prices),
        device_id,
        http,
        remote_revocations,
    };

    // Local admin dashboard: free tier, loopback-only, on by default. A
    // separate axum server/port from the LLM ingress on purpose (see
    // dashboard.rs). Never fails startup of the main proxy if the dashboard
    // port is unavailable -- it's a convenience surface, not core function.
    if dashboard_cfg.enabled {
        match dashboard::load_or_create_token(&paths) {
            Ok(token) => {
                let dstate = Arc::new(dashboard::DashboardState {
                    app: state.clone(),
                    paths: paths.clone(),
                    token,
                });
                let dlisten = dashboard_cfg.listen.clone();
                tracing::info!(
                    listen = %dlisten,
                    "dashboard listening (run `halo dashboard token` for the token needed to change settings)"
                );
                println!("Dashboard:            http://{dlisten}");
                tokio::spawn(async move {
                    match tokio::net::TcpListener::bind(&dlisten).await {
                        Ok(listener) => {
                            if let Err(e) = axum::serve(listener, dashboard::router(dstate)).await {
                                tracing::error!("dashboard server error: {e}");
                            }
                        }
                        Err(e) => tracing::error!("dashboard: failed to bind {dlisten}: {e}"),
                    }
                });
            }
            Err(e) => tracing::warn!("dashboard: failed to load/create local token, disabled: {e}"),
        }
    }

    // Background telemetry flush loop.
    {
        let telem = telem.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
            loop {
                tick.tick().await;
                telem.flush().await;
            }
        });
    }

    let app = ingress::router(state);
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    tracing::info!("Smartflow Halo listening on http://{listen}");
    println!("Smartflow Halo listening on http://{listen}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown(telem))
        .await
        .context("server error")?;
    Ok(())
}

async fn shutdown(telem: telemetry::Telemetry) {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutting down; flushing telemetry");
    telem.flush().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_tier_allows_up_to_the_limit() {
        let free = halo_common::Entitlements::default();
        for n in 0..halo_common::license::FREE_AGENT_LIMIT as usize {
            assert!(check_agent_cap(n, &free).is_ok(), "expected {n} to be under the cap");
        }
    }

    #[test]
    fn free_tier_blocks_at_the_limit() {
        let free = halo_common::Entitlements::default();
        let limit = halo_common::license::FREE_AGENT_LIMIT as usize;
        assert!(check_agent_cap(limit, &free).is_err());
        assert!(check_agent_cap(limit + 5, &free).is_err());
    }

    #[test]
    fn licensed_multi_agent_unlimited_lifts_the_cap() {
        let now = chrono::Utc::now();
        let claims = halo_common::LicenseClaims {
            org: "acme".into(),
            tier: "pro".into(),
            seats: 1,
            features: vec![halo_common::license::feature::MULTI_AGENT_UNLIMITED.to_string()],
            issued_at: now.to_rfc3339(),
            expires_at: (now + chrono::Duration::days(30)).to_rfc3339(),
        };
        // Throwaway seed/keypair -- not Aperion's real signing key.
        let seed = [7u8; 32];
        let key = halo_common::license::issue_from_seed(&claims, &seed);
        let pubkey = halo_common::license::pubkey_b64url_from_seed(&seed);
        let vk = halo_common::license::pubkey_from_b64url(&pubkey).expect("valid pubkey");
        let ent = halo_common::Entitlements::verify_with_key(&key, &vk, now);
        assert!(check_agent_cap(1_000, &ent).is_ok());
    }
}
