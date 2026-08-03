//! Smartflow Halo shim -- the `halo` binary.

mod audit;
mod budget;
mod cache;
mod cache_control;
mod cachekey;
mod compress;
mod config;
mod ingress;
mod keys;
mod mcp;
mod report;
mod state;
mod streaming;
mod telemetry;
mod util;

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
        /// Only include the last N hours.
        #[arg(long)]
        hours: Option<i64>,
    },
    /// Emergency stop: revoke an agent's key so the proxy refuses it at once.
    Kill {
        agent: String,
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
        /// Custom OpenAI-compatible base URL (Groq, Together, Fireworks, a
        /// local vLLM/Ollama server, ...). Ignored for --provider anthropic.
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

    match cli.cmd.unwrap_or(Cmd::Serve) {
        Cmd::Serve => serve(paths).await,
        Cmd::Agent { action } => agent_cmd(paths, action),
        Cmd::Status => status(paths),
        Cmd::Report { hours } => report_cmd(paths, hours),
        Cmd::Kill { agent } => kill(paths, &agent),
    }
}

fn agent_cmd(paths: Paths, action: AgentCmd) -> Result<()> {
    let listen = config::Config::load(&paths.config())
        .map(|c| c.listen)
        .unwrap_or_else(|_| "127.0.0.1:8787".to_string());
    let ks = keys::KeyStore::new(paths);
    match action {
        AgentCmd::Add {
            name,
            provider,
            key,
            base_url,
        } => {
            let secret = match key {
                Some(k) => k,
                None => read_secret()?,
            };
            if base_url.is_some() && matches!(provider, ProviderArg::Anthropic) {
                eprintln!("warning: --base-url is ignored for --provider anthropic");
            }
            let effective_base_url = if matches!(provider, ProviderArg::Openai) {
                base_url.clone()
            } else {
                None
            };
            let vkey = ks.issue(&name, provider.into(), secret.trim(), effective_base_url.clone())?;
            println!("Registered agent '{name}'. Configure your runtime with:\n");
            match Provider::from(provider) {
                Provider::Anthropic => {
                    println!("  ANTHROPIC_API_KEY={vkey}");
                    println!("  ANTHROPIC_BASE_URL=http://{listen}");
                }
                _ => {
                    println!("  OPENAI_API_KEY={vkey}");
                    println!("  OPENAI_BASE_URL=http://{listen}/v1");
                    if let Some(u) = &effective_base_url {
                        println!("  (proxied through to {u} -- an OpenAI-compatible endpoint)");
                    }
                }
            }
            println!("\nThe real provider key is stored in your OS keychain, never on disk.");
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
    println!("Listen:  {}", cfg.listen);
    println!(
        "Relay:   {}",
        cfg.relay_url.as_deref().unwrap_or("(local-only mode)")
    );
    println!(
        "Caps:    global soft {:?}  hard {:?}  window {}h",
        cfg.budget.soft_cap_usd, cfg.budget.hard_cap_usd, cfg.budget.window_hours
    );

    let recs = ks.records()?;
    let spend = ledger.spend_by_agent()?;
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

fn report_cmd(paths: Paths, hours: Option<i64>) -> Result<()> {
    let telem = telemetry::Telemetry::new(
        String::new(),
        None,
        None,
        paths.spool_dir(),
        paths.base.join("telemetry.jsonl"),
    );
    let events = telem.local_events();
    let since = hours.map(|h| chrono::Utc::now().timestamp() - h * 3600);
    let r = report::build(&events, since);
    print!("{}", report::render(&r));
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

    let cache = cache::CacheStore::open(&paths.cache(), cfg.cache.max_entries, cfg.cache.enabled)?;
    let ledger = budget::Ledger::open(&paths.ledger(), cfg.budget.window_hours)?;
    let audit_log = Arc::new(Mutex::new(audit::AuditLog::open(
        &paths.audit(),
        &paths.audit_key(),
    )?));
    let telem = telemetry::Telemetry::new(
        device_id.clone(),
        cfg.relay_url.clone(),
        cfg.relay_token.clone(),
        paths.spool_dir(),
        paths.base.join("telemetry.jsonl"),
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

    let mut prices = halo_common::pricing::PriceTable::default();
    for o in &cfg.price_overrides {
        prices.set(
            &o.model,
            halo_common::pricing::ModelPrice {
                input_per_mtok: o.input_per_mtok,
                output_per_mtok: o.output_per_mtok,
                cached_input_per_mtok: o.cached_input_per_mtok.unwrap_or(o.input_per_mtok),
            },
        );
    }

    let listen = cfg.listen.clone();
    let state = state::AppState {
        cfg: Arc::new(cfg),
        keys: ks,
        cache,
        ledger,
        audit_log,
        telem: telem.clone(),
        injector: Arc::new(cache_control::CacheControlInjector::new()),
        mcp,
        prices: Arc::new(prices),
        device_id,
        http,
    };

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
