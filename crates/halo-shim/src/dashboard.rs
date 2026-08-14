//! Local admin dashboard: a loopback-only web UI bundled into the `halo`
//! binary itself, distinct from `halo-relay`'s hosted, multi-device dashboard.
//!
//! Free tier, on by default. Read endpoints (savings, agents, config,
//! entitlements) require nothing beyond loopback access -- consistent with
//! everything else in the free tier being local-only and unconditional.
//! Endpoints that *mutate* state (revoke an agent, write config.yaml) require
//! a local bearer token generated on first use and never transmitted off the
//! machine (see `load_or_create_token`). This mirrors the relay dashboard's
//! kill-switch pattern (token gates writes, not reads) rather than inventing
//! a new auth model.
//!
//! Config writes update `config.yaml` on disk but do not hot-reload the
//! running process -- most fields (cache size, MCP servers, listen address)
//! are read once at `serve` startup and threaded into long-lived structures,
//! so honest behavior is "saved, restart to apply" rather than a half
//! hot-reload that silently misses some fields.

use crate::config::{Config, Paths};
use crate::registry;
use crate::report;
use crate::state::AppState;
use axum::extract::{Path as AxPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Json};
use axum::routing::{get, post};
use axum::Router;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub struct DashboardState {
    pub app: AppState,
    pub paths: Paths,
    pub token: String,
}

/// Load the local dashboard token, generating one on first use. 32 random
/// bytes, URL-safe base64, written `0600`. Losing/deleting the file just
/// mints a new one -- there is nothing durable to invalidate since it never
/// leaves this machine.
pub fn load_or_create_token(paths: &Paths) -> anyhow::Result<String> {
    let path = paths.dashboard_token();
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let t = existing.trim();
        if !t.is_empty() {
            return Ok(t.to_string());
        }
    }
    let mut raw = [0u8; 32];
    getrandom::getrandom(&mut raw).map_err(|e| anyhow::anyhow!("rng: {e}"))?;
    let token = base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, raw);
    crate::util::atomic_write_0600(&path, token.as_bytes())?;
    Ok(token)
}

fn authorized(state: &DashboardState, headers: &HeaderMap) -> bool {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| t == state.token)
}

pub fn router(state: Arc<DashboardState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/summary", get(summary))
        .route("/api/agents", get(agents))
        .route("/api/agents/:name/revoke", post(revoke_agent))
        .route("/api/config", get(get_config).post(post_config))
        .route("/api/entitlements", get(entitlements))
        .route("/api/registry", get(registry_json))
        .route("/api/registry.csv", get(registry_csv))
        .with_state(state)
}

async fn index() -> impl IntoResponse {
    Html(HTML)
}

#[derive(Serialize)]
struct SummaryResponse {
    total: RollupJson,
    by_agent: Vec<NamedRollup>,
    by_model: Vec<NamedRollup>,
    by_subject: Vec<NamedRollup>,
    by_task: Vec<NamedRollup>,
    by_hour: Vec<NamedRollup>,
    budget: BudgetJson,
    egress: EgressJson,
    ladder: String,
    max_history_hours: u64,
    clamped_hours: i64,
    upgrade: Option<UpgradeJson>,
}

#[derive(Serialize)]
struct UpgradeJson {
    to: String,
    price_usd: f64,
    saved_usd: f64,
    would_have_usd: f64,
    history_days: u64,
}

#[derive(Serialize)]
struct BudgetJson {
    soft_cap_usd: Option<f64>,
    hard_cap_usd: Option<f64>,
    window_hours: u64,
    spent_usd: f64,
    remaining_hard_usd: Option<f64>,
}

#[derive(Serialize)]
struct EgressJson {
    denied_upstreams: Vec<String>,
    allowed_upstreams: Vec<String>,
}

#[derive(Serialize)]
struct RollupJson {
    requests: u64,
    cache_hits: u64,
    semantic_hits: u64,
    actual_cost: f64,
    savings: f64,
    baseline_savings: f64,
    hit_savings: f64,
}

#[derive(Serialize)]
struct NamedRollup {
    name: String,
    #[serde(flatten)]
    rollup: RollupJson,
}

fn to_json(r: &report::AgentRollup) -> RollupJson {
    RollupJson {
        requests: r.requests,
        cache_hits: r.cache_hits,
        semantic_hits: r.semantic_hits,
        actual_cost: r.actual_cost,
        savings: r.savings(),
        baseline_savings: r.baseline_savings(),
        hit_savings: r.hit_savings(),
    }
}

#[derive(Deserialize)]
struct SummaryQuery {
    hours: Option<i64>,
}

async fn summary(
    State(st): State<Arc<DashboardState>>,
    axum::extract::Query(q): axum::extract::Query<SummaryQuery>,
) -> impl IntoResponse {
    let events = st.app.telem.local_events();
    let hours = st.app.entitlements.clamp_history_hours(q.hours);
    let since = Some(chrono::Utc::now().timestamp() - hours * 3600);
    let rep = report::build(&events, since, &st.app.prices);
    let spent = st.app.ledger.spend("").map(|(g, _)| g).unwrap_or(0.0);
    let hard = st.app.cfg.budget.hard_cap_usd;
    let ladder = st.app.entitlements.ladder();
    let upgrade = if matches!(ladder, halo_common::Ladder::Free) {
        Some(UpgradeJson {
            to: "cut".into(),
            price_usd: 50.0,
            saved_usd: rep.total.savings(),
            would_have_usd: rep.total.counterfactual_cost,
            history_days: 30,
        })
    } else {
        None
    };
    Json(SummaryResponse {
        total: to_json(&rep.total),
        by_agent: named(&rep.by_agent),
        by_model: named_nonempty(&rep.by_model),
        by_subject: named(&rep.by_subject),
        by_task: named(&rep.by_task),
        by_hour: named(&rep.by_hour),
        budget: BudgetJson {
            soft_cap_usd: st.app.cfg.budget.soft_cap_usd,
            hard_cap_usd: hard,
            window_hours: st.app.cfg.budget.window_hours,
            spent_usd: spent,
            remaining_hard_usd: hard.map(|h| (h - spent).max(0.0)),
        },
        egress: EgressJson {
            denied_upstreams: st.app.cfg.egress.effective_denied(),
            allowed_upstreams: st.app.cfg.egress.allowed_upstreams.clone(),
        },
        ladder: ladder.as_str().to_string(),
        max_history_hours: st.app.entitlements.max_history_hours(),
        clamped_hours: hours,
        upgrade,
    })
}

fn named(map: &std::collections::BTreeMap<String, report::AgentRollup>) -> Vec<NamedRollup> {
    map.iter()
        .map(|(k, v)| NamedRollup { name: k.clone(), rollup: to_json(v) })
        .collect()
}

fn named_nonempty(map: &std::collections::BTreeMap<String, report::AgentRollup>) -> Vec<NamedRollup> {
    map.iter()
        .filter(|(k, _)| !k.is_empty())
        .map(|(k, v)| NamedRollup { name: k.clone(), rollup: to_json(v) })
        .collect()
}

#[derive(Serialize)]
struct AgentRow {
    name: String,
    provider: String,
    status: String,
    spend_24h: f64,
}

async fn agents(State(st): State<Arc<DashboardState>>) -> impl IntoResponse {
    let recs = st.app.keys.records().unwrap_or_default();
    let spend = st.app.ledger.spend_by_agent().unwrap_or_default();
    let rows: Vec<AgentRow> = recs
        .into_iter()
        .map(|r| {
            let spend_24h = spend
                .iter()
                .find(|(a, _)| a == &r.agent_id)
                .map(|(_, c)| *c)
                .unwrap_or(0.0);
            let status = if r.is_active() { "active".into() } else { "revoked".into() };
            AgentRow {
                name: r.agent_id,
                provider: r.provider.as_str().to_string(),
                status,
                spend_24h,
            }
        })
        .collect();
    Json(rows)
}

async fn revoke_agent(
    State(st): State<Arc<DashboardState>>,
    headers: HeaderMap,
    AxPath(name): AxPath<String>,
) -> impl IntoResponse {
    if !authorized(&st, &headers) {
        return (StatusCode::UNAUTHORIZED, "missing/invalid dashboard token").into_response();
    }
    match st.app.keys.revoke(&name) {
        Ok(()) => Json(serde_json::json!({"revoked": name})).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

/// Editable config surface. Deliberately a subset of `Config`: real provider
/// keys never appear here (they live in the OS keychain, see `keys.rs`), and
/// `relay_token`/`license_key` are write-only from the dashboard (you can set
/// them, the GET response never echoes the current value back).
#[derive(Serialize, Deserialize)]
struct EditableConfig {
    soft_cap_usd: Option<f64>,
    hard_cap_usd: Option<f64>,
    window_hours: u64,
    cache_enabled: bool,
    cache_max_entries: u64,
    semantic_cache_enabled: bool,
    semantic_cache_provider: String,
    semantic_cache_threshold: f32,
    relay_url: Option<String>,
    relay_token_set: bool,
    license_key_set: bool,
    alert_webhook: Option<String>,
}

async fn get_config(State(st): State<Arc<DashboardState>>) -> impl IntoResponse {
    // Read fresh from disk, not the in-memory Arc<Config> captured at
    // startup, so a just-saved change shows up immediately even though it
    // won't take live effect until the next restart (see module docs).
    let owned = Config::load(&st.paths.config()).unwrap_or_else(|_| (*st.app.cfg).clone());
    let cfg = &owned;
    Json(EditableConfig {
        soft_cap_usd: cfg.budget.soft_cap_usd,
        hard_cap_usd: cfg.budget.hard_cap_usd,
        window_hours: cfg.budget.window_hours,
        cache_enabled: cfg.cache.enabled,
        cache_max_entries: cfg.cache.max_entries,
        semantic_cache_enabled: cfg.semantic_cache.enabled,
        semantic_cache_provider: cfg.semantic_cache.provider.clone(),
        semantic_cache_threshold: cfg.semantic_cache.similarity_threshold,
        relay_url: cfg.relay_url.clone(),
        relay_token_set: cfg.relay_token.is_some(),
        license_key_set: cfg.license_key.is_some(),
        alert_webhook: cfg.alert_webhook.clone(),
    })
}

/// Same shape as `EditableConfig` but every field optional -- a POST only
/// needs to carry what changed; omitted fields keep their on-disk value.
#[derive(Deserialize, Default)]
struct ConfigPatch {
    soft_cap_usd: Option<Option<f64>>,
    hard_cap_usd: Option<Option<f64>>,
    window_hours: Option<u64>,
    cache_enabled: Option<bool>,
    cache_max_entries: Option<u64>,
    semantic_cache_enabled: Option<bool>,
    semantic_cache_provider: Option<String>,
    semantic_cache_threshold: Option<f32>,
    relay_url: Option<Option<String>>,
    alert_webhook: Option<Option<String>>,
}

async fn post_config(
    State(st): State<Arc<DashboardState>>,
    headers: HeaderMap,
    Json(patch): Json<ConfigPatch>,
) -> impl IntoResponse {
    if !authorized(&st, &headers) {
        return (StatusCode::UNAUTHORIZED, "missing/invalid dashboard token").into_response();
    }
    // Re-read from disk (not the in-memory Arc<Config>) so a concurrent
    // manual edit of config.yaml isn't clobbered by a stale in-memory copy.
    let mut cfg = match Config::load(&st.paths.config()) {
        Ok(c) => c,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    if let Some(v) = patch.soft_cap_usd {
        cfg.budget.soft_cap_usd = v;
    }
    if let Some(v) = patch.hard_cap_usd {
        cfg.budget.hard_cap_usd = v;
    }
    if let Some(v) = patch.window_hours {
        cfg.budget.window_hours = v;
    }
    if let Some(v) = patch.cache_enabled {
        cfg.cache.enabled = v;
    }
    if let Some(v) = patch.cache_max_entries {
        cfg.cache.max_entries = v;
    }
    if let Some(v) = patch.semantic_cache_enabled {
        cfg.semantic_cache.enabled = v;
    }
    if let Some(v) = patch.semantic_cache_provider {
        cfg.semantic_cache.provider = v;
    }
    if let Some(v) = patch.semantic_cache_threshold {
        cfg.semantic_cache.similarity_threshold = v;
    }
    if let Some(v) = patch.relay_url {
        cfg.relay_url = v;
    }
    if let Some(v) = patch.alert_webhook {
        cfg.alert_webhook = v;
    }
    let yaml = match serde_yaml::to_string(&cfg) {
        Ok(y) => y,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    if let Err(e) = crate::util::atomic_write_0600(&st.paths.config(), yaml.as_bytes()) {
        return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
    }
    Json(serde_json::json!({"saved": true, "note": "restart `halo serve` to apply"}))
        .into_response()
}

async fn entitlements(State(st): State<Arc<DashboardState>>) -> impl IntoResponse {
    let e = &st.app.entitlements;
    Json(serde_json::json!({
        "tier": e.tier_label,
        "ladder": e.ladder().as_str(),
        "status": e.status.label(),
        "org": e.org,
        "seats": e.seats,
        "expires_at": e.expires_at,
        "max_history_hours": e.max_history_hours(),
        "features": halo_common::license::feature::ALL.iter().map(|f| (f.to_string(), e.has(f))).collect::<std::collections::BTreeMap<_,_>>(),
    }))
}

/// Build the AI usage / governance registry from live state -- same
/// aggregation `halo registry export` uses, just fed from `AppState` instead
/// of re-opening stores from disk. Unauthenticated read, same as the other
/// read endpoints on loopback (see module docs).
fn build_live_registry(st: &DashboardState) -> registry::RegistryReport {
    let records = st.app.keys.records().unwrap_or_default();
    let events = st.app.telem.local_events();
    registry::build_registry(
        &records,
        &events,
        &st.app.prices,
        &st.app.cfg.mcp_servers,
        &st.app.entitlements,
        &st.app.device_id,
    )
}

async fn registry_json(State(st): State<Arc<DashboardState>>) -> impl IntoResponse {
    Json(build_live_registry(&st))
}

async fn registry_csv(State(st): State<Arc<DashboardState>>) -> impl IntoResponse {
    let csv = registry::agents_to_csv(&build_live_registry(&st));
    let filename = format!("halo-ai-registry-{}.csv", chrono::Utc::now().format("%Y-%m-%d"));
    (
        [
            (axum::http::header::CONTENT_TYPE, "text/csv".to_string()),
            (
                axum::http::header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
        ],
        csv,
    )
}

const HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8"/>
<meta name="viewport" content="width=device-width, initial-scale=1"/>
<title>Smartflow Halo — Dashboard</title>
<style>
  :root { --bg:#0b1020; --card:#131a2e; --border:#26304a; --ink:#e7ecf7; --muted:#93a0bd; --teal:#39d3bb; --green:#4ade80; --amber:#fbbf24; }
  * { box-sizing: border-box; }
  body { margin:0; background:var(--bg); color:var(--ink); font:15px/1.5 -apple-system,Segoe UI,Roboto,sans-serif; }
  header { padding:24px 32px; border-bottom:1px solid var(--border); display:flex; justify-content:space-between; align-items:baseline; flex-wrap:wrap; gap:8px; }
  h1 { margin:0; font-size:20px; letter-spacing:.2px; }
  .sub { color:var(--muted); font-size:13px; }
  .tier-chip { font-size:12px; font-weight:700; padding:3px 10px; border-radius:12px; border:1px solid var(--teal); color:var(--teal); }
  main { max-width:1080px; margin:0 auto; padding:28px 32px 60px; }
  .cards { display:grid; grid-template-columns:repeat(4,1fr); gap:16px; margin-bottom:28px; }
  .card { background:var(--card); border:1px solid var(--border); border-radius:14px; padding:18px; }
  .card .label { color:var(--muted); font-size:12px; text-transform:uppercase; letter-spacing:.6px; }
  .card .value { font-size:24px; font-weight:650; margin-top:8px; }
  .card.savings .value { color:var(--green); }
  table { width:100%; border-collapse:collapse; background:var(--card); border:1px solid var(--border); border-radius:14px; overflow:hidden; margin-bottom:24px; }
  th,td { text-align:left; padding:10px 16px; border-bottom:1px solid var(--border); font-variant-numeric:tabular-nums; }
  th { color:var(--muted); font-size:12px; text-transform:uppercase; letter-spacing:.5px; }
  tr:last-child td { border-bottom:none; }
  td.num { text-align:right; }
  .money { color:var(--teal); }
  select,input { background:var(--card); color:var(--ink); border:1px solid var(--border); border-radius:8px; padding:6px 10px; }
  h2 { font-size:14px; color:var(--muted); text-transform:uppercase; letter-spacing:.6px; margin:28px 0 10px; }
  button { background:var(--card); color:var(--ink); border:1px solid var(--border); border-radius:8px; padding:6px 12px; cursor:pointer; }
  button.danger { border-color:#b4404a; color:#ff9ba3; }
  button:hover { border-color:var(--teal); }
  .row { display:flex; gap:10px; flex-wrap:wrap; align-items:center; margin-bottom:14px; }
  .form-grid { display:grid; grid-template-columns:repeat(2,1fr); gap:14px; background:var(--card); border:1px solid var(--border); border-radius:14px; padding:20px; margin-bottom:16px; }
  .form-grid label { display:flex; flex-direction:column; gap:4px; font-size:13px; color:var(--muted); }
  .form-grid input[type=checkbox] { width:16px; height:16px; align-self:flex-start; }
  .msg { color:var(--muted); font-size:13px; min-height:18px; }
  .hint { color:var(--muted); font-size:12px; margin-top:-4px; margin-bottom:16px; }
  code { background:#0a1020; padding:2px 6px; border-radius:5px; }
</style>
</head>
<body>
<header>
  <div><h1>Smartflow Halo</h1><div class="sub">Local admin dashboard — loopback only, nothing leaves this machine.</div></div>
  <span class="tier-chip" id="tierChip">—</span>
</header>
<main>
  <div class="row">
    Window:
    <select id="win" onchange="load()">
      <option value="24" selected>Last 24 hours</option>
      <option value="168">Last 7 days</option>
    </select>
  </div>
  <div class="card" id="upgradeCard" style="display:none;margin-bottom:20px">
    <div class="label">Your traffic, this window</div>
    <div class="value" id="upgradeLine">—</div>
    <div class="hint" id="upgradeHint" style="margin:8px 0 0"></div>
    <p style="margin:10px 0 0"><a id="upgradeBuy" href="https://deploy.langsmart.app/halo/buy/cut">Buy Cut — $50/mo</a>
      · <a href="https://deploy.langsmart.app/halo/buy/route">Buy Route — $100/mo</a></p>
  </div>
  <div class="cards">
    <div class="card savings"><div class="label">Estimated saved</div><div class="value" id="saved">—</div></div>
    <div class="card"><div class="label">Actual spend</div><div class="value money" id="spend">—</div></div>
    <div class="card"><div class="label">Requests</div><div class="value" id="reqs">—</div></div>
    <div class="card"><div class="label">Remaining budget</div><div class="value" id="remain">—</div></div>
  </div>
  <div class="hint" id="budgetHint">Hard cap is the kill switch. Raise <code>budget.hard_cap_usd</code> in Settings or <code>~/.halo/config.yaml</code>.</div>
  <div class="hint" id="egressHint"></div>

  <h2>Agents</h2>
  <div class="hint">Revoking an agent here requires the local dashboard token (run <code>halo dashboard token</code> in your terminal to reveal it).</div>
  <table><thead><tr><th>Agent</th><th>Provider</th><th>Status</th><th class="num">Spend (24h)</th><th></th></tr></thead><tbody id="agents"></tbody></table>

  <h2>By model</h2>
  <table><thead><tr><th>Model</th><th class="num">Requests</th><th class="num">Spend</th><th class="num">Saved</th></tr></thead><tbody id="models"></tbody></table>

  <h2>By task</h2>
  <table><thead><tr><th>Task</th><th class="num">Requests</th><th class="num">Spend</th><th class="num">Saved</th></tr></thead><tbody id="tasks"></tbody></table>

  <h2>By hour (UTC)</h2>
  <table><thead><tr><th>Hour</th><th class="num">Requests</th><th class="num">Spend</th></tr></thead><tbody id="hours"></tbody></table>

  <h2>AI usage registry</h2>
  <div class="hint">A governance/audit export: every agent, its provider and endpoint, and its lifetime request/spend/savings totals. Metadata only -- no prompt/response content. Read-only, no token needed.</div>
  <div class="row">
    <button onclick="downloadRegistry('json')">Download JSON</button>
    <button onclick="downloadRegistry('csv')">Download CSV</button>
  </div>
  <table><thead><tr><th>Agent</th><th>Provider</th><th>Endpoint</th><th>Status</th><th class="num">Requests</th><th class="num">Spend</th><th class="num">Saved</th></tr></thead><tbody id="registry"></tbody></table>

  <h2>Settings</h2>
  <div class="hint">Changes are written to <code>~/.halo/config.yaml</code> immediately but require restarting <code>halo serve</code> to take effect. Saving requires the local dashboard token.</div>
  <div class="form-grid" id="settingsForm">
    <label>Global soft cap (USD)<input id="s_soft" type="number" step="0.01" placeholder="unset"/></label>
    <label>Global hard cap (USD)<input id="s_hard" type="number" step="0.01" placeholder="unset"/></label>
    <label>Budget window (hours)<input id="s_window" type="number" step="1"/></label>
    <label>Relay URL (optional)<input id="s_relay" type="text" placeholder="https://relay.example.com"/></label>
    <label>Exact-match cache<input id="s_cache" type="checkbox"/></label>
    <label>Cache max entries<input id="s_cache_max" type="number" step="1"/></label>
    <label>Semantic cache<input id="s_semcache" type="checkbox"/></label>
    <label>Semantic similarity threshold<input id="s_semthresh" type="number" step="0.01" min="0" max="1"/></label>
  </div>
  <div class="row">
    <input id="d_token" type="password" placeholder="dashboard token" size="30"/>
    <button onclick="saveConfig()">Save settings</button>
  </div>
  <div class="msg" id="cfgMsg"></div>
</main>
<script>
const usd = n => '$' + (n||0).toFixed(4);
async function load() {
  const hours = document.getElementById('win').value;
  const r = await fetch('/api/summary?hours=' + hours);
  const d = await r.json();
  const t = d.total || {};
  document.getElementById('saved').textContent = usd(t.savings);
  document.getElementById('spend').textContent = usd(t.actual_cost);
  document.getElementById('reqs').textContent = (t.requests||0).toLocaleString();
  const capH = d.max_history_hours || 168;
  const win = document.getElementById('win');
  const wanted = [
    [24, 'Last 24 hours'],
    [168, 'Last 7 days'],
    [720, 'Last 30 days'],
    [2160, 'Last 90 days'],
  ].filter(([h]) => h <= capH);
  const cur = win.value;
  win.innerHTML = wanted.map(([h,l]) => `<option value="${h}">${l}</option>`).join('');
  win.value = wanted.some(([h]) => String(h) === cur) ? cur : String(wanted[0][0]);
  const up = d.upgrade;
  const card = document.getElementById('upgradeCard');
  if (up) {
    card.style.display = 'block';
    document.getElementById('upgradeLine').textContent =
      'Would have cost ' + usd(up.would_have_usd) + ' · Halo saved ' + usd(up.saved_usd);
    document.getElementById('upgradeHint').textContent =
      'Cut is $' + up.price_usd + '/mo and unlocks ' + up.history_days +
      '-day history. After checkout: halo license apply (paste the token), then halo license show.';
  } else {
    card.style.display = 'none';
  }
  const b = d.budget || {};
  const remainEl = document.getElementById('remain');
  if (b.hard_cap_usd == null) {
    remainEl.textContent = 'no cap';
  } else {
    remainEl.textContent = usd(b.remaining_hard_usd) + ' / ' + usd(b.hard_cap_usd);
  }
  document.getElementById('budgetHint').textContent =
    'Kill switch: $' + (b.hard_cap_usd ?? 'unset') + ' hard / $' + (b.soft_cap_usd ?? 'unset') +
    ' soft over ' + (b.window_hours||24) + 'h. Spent ' + usd(b.spent_usd) +
    ' this window. Raise budget.hard_cap_usd in Settings or ~/.halo/config.yaml.';
  const eg = d.egress || {};
  const denied = (eg.denied_upstreams||[]).join(', ') || '(none)';
  document.getElementById('egressHint').textContent = 'Active denylist: ' + denied;
  const fill = (id, rows, extra) => {
    const el = document.getElementById(id); el.innerHTML='';
    (rows||[]).forEach(m => { el.innerHTML += extra(m); });
  };
  fill('models', d.by_model, m => `<tr><td>${m.name}</td><td class="num">${m.requests}</td><td class="num money">${usd(m.actual_cost)}</td><td class="num">${usd(m.savings)}</td></tr>`);
  fill('tasks', d.by_task, m => `<tr><td>${m.name}</td><td class="num">${m.requests}</td><td class="num money">${usd(m.actual_cost)}</td><td class="num">${usd(m.savings)}</td></tr>`);
  fill('hours', d.by_hour, m => `<tr><td>${m.name}</td><td class="num">${m.requests}</td><td class="num money">${usd(m.actual_cost)}</td></tr>`);
}
async function loadAgents() {
  const r = await fetch('/api/agents');
  const list = await r.json();
  const tb = document.getElementById('agents'); tb.innerHTML = '';
  if (!list.length) { tb.innerHTML = '<tr><td colspan="5" style="color:var(--muted)">No agents registered. Run `halo agent add`.</td></tr>'; return; }
  list.forEach(a => {
    const btn = a.status === 'active'
      ? `<button class="danger" onclick="revoke('${a.name}')">Revoke</button>`
      : '<span style="color:var(--muted)">revoked</span>';
    tb.innerHTML += `<tr><td>${a.name}</td><td>${a.provider}</td><td>${a.status}</td><td class="num money">${usd(a.spend_24h)}</td><td>${btn}</td></tr>`;
  });
}
async function revoke(name) {
  const token = document.getElementById('d_token').value;
  if (!token) { alert('Enter the dashboard token first.'); return; }
  const r = await fetch(`/api/agents/${encodeURIComponent(name)}/revoke`, { method:'POST', headers:{'Authorization':'Bearer '+token} });
  if (r.ok) loadAgents(); else alert('Failed (' + r.status + '): check the token.');
}
async function loadEntitlements() {
  const r = await fetch('/api/entitlements');
  const e = await r.json();
  document.getElementById('tierChip').textContent = (e.ladder || e.tier) + ' · ' + e.status;
}
async function loadRegistry() {
  const r = await fetch('/api/registry');
  const reg = await r.json();
  const tb = document.getElementById('registry'); tb.innerHTML = '';
  if (!reg.agents || !reg.agents.length) { tb.innerHTML = '<tr><td colspan="7" style="color:var(--muted)">No agents registered yet.</td></tr>'; return; }
  reg.agents.forEach(a => {
    tb.innerHTML += `<tr><td>${a.name}</td><td>${a.provider}</td><td>${a.base_url_host || '(default)'}</td><td>${a.status}</td><td class="num">${a.requests}</td><td class="num money">${usd(a.spend_usd)}</td><td class="num">${usd(a.savings_usd)}</td></tr>`;
  });
}
function downloadRegistry(format) {
  const url = format === 'csv' ? '/api/registry.csv' : '/api/registry';
  if (format === 'csv') { window.location.href = url; return; }
  fetch(url).then(r => r.json()).then(data => {
    const blob = new Blob([JSON.stringify(data, null, 2)], { type: 'application/json' });
    const a = document.createElement('a');
    a.href = URL.createObjectURL(blob);
    a.download = 'halo-ai-registry-' + new Date().toISOString().slice(0,10) + '.json';
    a.click();
  });
}
async function loadConfig() {
  const r = await fetch('/api/config');
  const c = await r.json();
  document.getElementById('s_soft').value = c.soft_cap_usd ?? '';
  document.getElementById('s_hard').value = c.hard_cap_usd ?? '';
  document.getElementById('s_window').value = c.window_hours;
  document.getElementById('s_relay').value = c.relay_url ?? '';
  document.getElementById('s_cache').checked = c.cache_enabled;
  document.getElementById('s_cache_max').value = c.cache_max_entries;
  document.getElementById('s_semcache').checked = c.semantic_cache_enabled;
  document.getElementById('s_semthresh').value = c.semantic_cache_threshold;
}
function numOrNull(id) { const v = document.getElementById(id).value; return v === '' ? null : parseFloat(v); }
async function saveConfig() {
  const token = document.getElementById('d_token').value;
  const msg = document.getElementById('cfgMsg');
  if (!token) { msg.textContent = 'Enter the dashboard token first.'; return; }
  const body = {
    soft_cap_usd: numOrNull('s_soft'),
    hard_cap_usd: numOrNull('s_hard'),
    window_hours: parseInt(document.getElementById('s_window').value, 10),
    relay_url: document.getElementById('s_relay').value || null,
    cache_enabled: document.getElementById('s_cache').checked,
    cache_max_entries: parseInt(document.getElementById('s_cache_max').value, 10),
    semantic_cache_enabled: document.getElementById('s_semcache').checked,
    semantic_cache_threshold: parseFloat(document.getElementById('s_semthresh').value),
  };
  try {
    const r = await fetch('/api/config', { method:'POST', headers:{'Content-Type':'application/json','Authorization':'Bearer '+token}, body: JSON.stringify(body) });
    const d = await r.json();
    msg.textContent = r.ok ? 'Saved. Restart `halo serve` to apply.' : `Failed (${r.status}): check the token.`;
  } catch(e) { msg.textContent = 'Request failed: ' + e; }
}
load(); loadAgents(); loadEntitlements(); loadConfig(); loadRegistry();
</script>
</body>
</html>"#;
