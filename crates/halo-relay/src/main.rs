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
    /// Shared bearer token required on the ingest endpoint. When unset, ingest
    /// is open (dev only -- always set a token in production).
    #[arg(long, env = "HALO_RELAY_TOKEN")]
    token: Option<String>,
}

#[derive(Clone)]
struct AppState {
    store: Arc<Store>,
    token: Option<String>,
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
    if cli.token.is_none() {
        tracing::warn!("no --token set: telemetry ingest is OPEN. Set HALO_RELAY_TOKEN in production.");
    }
    let state = AppState {
        store,
        token: cli.token,
    };

    let app = Router::new()
        .route("/", get(|| async { Html(dashboard::HTML) }))
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/summary", get(summary))
        .route("/v1/telemetry", post(ingest))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&cli.bind).await?;
    tracing::info!("Halo relay on http://{}", cli.bind);
    println!("Halo relay on http://{}", cli.bind);
    axum::serve(listener, app).await?;
    Ok(())
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
        Ok(s) => Json(s).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("summary error: {e}"),
        )
            .into_response(),
    }
}

async fn ingest(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(batch): Json<TelemetryBatch>,
) -> Response {
    if let Some(expected) = &st.token {
        let got = headers
            .get(header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        if got != Some(expected.as_str()) {
            return (StatusCode::UNAUTHORIZED, "invalid or missing bearer token").into_response();
        }
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
