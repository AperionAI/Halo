//! Smartflow Halo relay -- the `halo-relay` binary.
//!
//! Minimal telemetry ingest + a savings dashboard. Receives metadata only;
//! model traffic never transits here. One axum binary, one SQLite file.

mod counterfactual;
mod dashboard;
mod store;

use anyhow::Result;
use axum::{
    extract::{Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use clap::Parser;
use halo_common::telemetry::TelemetryBatch;
use std::sync::Arc;
use store::Store;

#[derive(Parser)]
#[command(name = "halo-relay", version, about = "Smartflow Halo relay -- telemetry ingest + savings dashboard")]
struct Cli {
    /// Address to bind.
    #[arg(long, env = "HALO_RELAY_BIND", default_value = "127.0.0.1:8080")]
    bind: String,
    /// SQLite database file.
    #[arg(long, env = "HALO_RELAY_DB", default_value = "halo-relay.db")]
    db: String,
    /// Shared bearer token required on the ingest/admin endpoints. When unset,
    /// they are OPEN (dev only -- always set a token in production).
    #[arg(long, env = "HALO_RELAY_TOKEN")]
    token: Option<String>,
    /// Additional bearer tokens (comma-separated), so different devices/seats
    /// can each carry their own token. Only honored when the relay's license
    /// (`--license`) entitles `multi_seat`; otherwise a warning is logged and
    /// they are ignored (only `--token` is accepted).
    #[arg(long, env = "HALO_RELAY_TOKENS", value_delimiter = ',')]
    tokens: Vec<String>,
    /// The relay's own license key (signed token or a path to one). Gates
    /// multi-seat tokens. Absent/invalid = single-token mode.
    #[arg(long, env = "HALO_RELAY_LICENSE")]
    license: Option<String>,
}

#[derive(Clone)]
struct AppState {
    store: Arc<Store>,
    /// Every accepted bearer token. Empty = auth disabled (dev only).
    tokens: Arc<Vec<String>>,
    /// Whether the relay license entitles the per-subject drill-down (the
    /// gated "per-channel/sub-agent cost attribution" paid feature).
    subject_attribution: bool,
}

impl AppState {
    /// True if the request carries an accepted bearer token, or auth is off.
    fn authorized(&self, headers: &HeaderMap) -> bool {
        if self.tokens.is_empty() {
            return true; // dev/open mode
        }
        let got = headers
            .get(header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        match got {
            Some(t) => self.tokens.iter().any(|expected| expected == t),
            None => false,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    let store = Arc::new(Store::open(&cli.db)?);

    let ent = resolve_license(cli.license.as_deref());
    let tokens = resolve_tokens(cli.token, cli.tokens, &ent);
    if tokens.is_empty() {
        tracing::warn!("no token set: ingest/admin endpoints are OPEN. Set HALO_RELAY_TOKEN in production.");
    } else {
        tracing::info!(count = tokens.len(), "relay auth active");
    }
    let subject_attribution = ent.has(halo_common::license::feature::SUBJECT_ATTRIBUTION);
    if subject_attribution {
        tracing::info!("subject_attribution licensed: per-channel/sub-agent drill-down enabled");
    }

    let state = AppState {
        store,
        tokens: Arc::new(tokens),
        subject_attribution,
    };

    let app = Router::new()
        .route("/", get(|| async { Html(dashboard::HTML) }))
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/summary", get(summary))
        .route("/api/revocations", get(list_revocations))
        .route("/api/features", get(features))
        .route("/v1/telemetry", post(ingest))
        .route("/v1/revocations", get(revocations_for_device))
        .route("/v1/kill", post(kill))
        .route("/v1/unkill", post(unkill))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&cli.bind).await?;
    tracing::info!("Halo relay on http://{}", cli.bind);
    println!("Halo relay on http://{}", cli.bind);
    axum::serve(listener, app).await?;
    Ok(())
}

/// Resolve the accepted token set from the single token, the extra tokens, and
/// the relay license. Extra tokens are honored only when the license entitles
/// `multi_seat`; otherwise they are dropped with a warning.
fn resolve_tokens(
    single: Option<String>,
    extra: Vec<String>,
    ent: &halo_common::Entitlements,
) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    if let Some(t) = single.filter(|s| !s.trim().is_empty()) {
        tokens.push(t);
    }
    let extra: Vec<String> = extra
        .into_iter()
        .map(|t| t.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if !extra.is_empty() {
        if ent.has(halo_common::license::feature::MULTI_SEAT) {
            tracing::info!(
                extra = extra.len(),
                org = ent.org.as_deref().unwrap_or("-"),
                "multi-seat licensed: accepting additional relay tokens"
            );
            tokens.extend(extra);
        } else {
            tracing::warn!(
                "HALO_RELAY_TOKENS ignored: relay license does not entitle `multi_seat` \
                 (status: {}). Only HALO_RELAY_TOKEN is accepted.",
                ent.status.label()
            );
        }
    }
    // De-dup while preserving order.
    let mut seen = std::collections::HashSet::new();
    tokens.retain(|t| seen.insert(t.clone()));
    tokens
}

/// Read the relay license (raw token or a path to one) into entitlements.
fn resolve_license(license: Option<&str>) -> halo_common::Entitlements {
    let key = license.map(|raw| {
        let trimmed = raw.trim();
        std::fs::read_to_string(trimmed)
            .map(|c| c.trim().to_string())
            .unwrap_or_else(|_| trimmed.to_string())
    });
    halo_common::Entitlements::from_license_key(key.as_deref())
}

#[derive(serde::Deserialize)]
struct WindowQuery {
    /// Hours to look back; 0 or absent means all time.
    #[serde(default)]
    hours: i64,
}

async fn summary(State(st): State<AppState>, Query(q): Query<WindowQuery>) -> Response {
    let since = if q.hours > 0 {
        chrono::Utc::now().timestamp() - q.hours * 3600
    } else {
        0
    };
    match st.store.summary(since) {
        Ok(mut s) => {
            // Per-subject drill-down is the gated paid feature; drop it from
            // the wire entirely when the relay isn't entitled.
            if !st.subject_attribution {
                s.strip_subjects();
            }
            Json(s).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("summary error: {e}"),
        )
            .into_response(),
    }
}

/// Feature flags the dashboard uses to decide what to render.
async fn features(State(st): State<AppState>) -> Response {
    Json(serde_json::json!({
        "subject_attribution": st.subject_attribution,
    }))
    .into_response()
}

async fn ingest(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(batch): Json<TelemetryBatch>,
) -> Response {
    if !st.authorized(&headers) {
        return (StatusCode::UNAUTHORIZED, "invalid or missing bearer token").into_response();
    }
    match st.store.insert_batch(&batch.device_id, &batch.events) {
        Ok(n) => (StatusCode::OK, Json(serde_json::json!({ "ingested": n }))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("ingest error: {e}"),
        )
            .into_response(),
    }
}

#[derive(serde::Deserialize)]
struct DeviceQuery {
    #[serde(default)]
    device_id: String,
}

/// Remote-kill list the shim polls. Auth required (same tokens as ingest).
async fn revocations_for_device(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<DeviceQuery>,
) -> Response {
    if !st.authorized(&headers) {
        return (StatusCode::UNAUTHORIZED, "invalid or missing bearer token").into_response();
    }
    match st.store.revoked_for(&q.device_id) {
        Ok(revoked) => Json(serde_json::json!({ "revoked": revoked })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("revocations error: {e}"))
            .into_response(),
    }
}

#[derive(serde::Deserialize)]
struct KillBody {
    agent_id: String,
    /// Omit (or "*") to revoke on every device.
    #[serde(default)]
    device_id: Option<String>,
}

fn kill_scope(b: &KillBody) -> Option<&str> {
    match b.device_id.as_deref() {
        Some("*") | Some("") | None => None,
        Some(d) => Some(d),
    }
}

/// Revoke an agent (fleet-wide or per-device). Admin endpoint, auth required.
async fn kill(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<KillBody>,
) -> Response {
    if !st.authorized(&headers) {
        return (StatusCode::UNAUTHORIZED, "invalid or missing bearer token").into_response();
    }
    match st.store.revoke(&body.agent_id, kill_scope(&body)) {
        Ok(()) => Json(serde_json::json!({ "revoked": body.agent_id })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("kill error: {e}")).into_response(),
    }
}

/// Lift a revocation. Admin endpoint, auth required.
async fn unkill(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<KillBody>,
) -> Response {
    if !st.authorized(&headers) {
        return (StatusCode::UNAUTHORIZED, "invalid or missing bearer token").into_response();
    }
    match st.store.unrevoke(&body.agent_id, kill_scope(&body)) {
        Ok(()) => Json(serde_json::json!({ "unrevoked": body.agent_id })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("unkill error: {e}")).into_response(),
    }
}

/// All revocations, for the dashboard's remote-kill panel. Read-only, and left
/// unauthenticated to match `/api/summary` (both expose only agent ids/costs,
/// no secrets); the mutating `/v1/kill` + `/v1/unkill` still require a token.
async fn list_revocations(State(st): State<AppState>) -> Response {
    match st.store.list_revocations() {
        Ok(list) => Json(list).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("list error: {e}")).into_response(),
    }
}
