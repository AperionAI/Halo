//! LLM-API and MCP ingress.
//!
//! Loopback listeners the agent runtime points at instead of the real
//! provider. Anthropic-compatible (`/v1/messages`) and OpenAI-compatible
//! (`/v1/chat/completions`, `/v1/embeddings`), plus an MCP seam
//! (`/mcp/:server`). The request pipeline, in order:
//!
//!   virtual-key auth -> compression -> exact-match cache -> budget preflight
//!   (kill switch) -> provider forward with the real key -> usage/cost accounting
//!   -> cache store -> audit -> metadata telemetry.
//!
//! Streaming (`"stream": true`) requests are genuinely passed through byte by
//! byte as they arrive from the provider -- see [`stream_response`] -- rather
//! than buffered, which is table stakes for an interactive agent proxy.
//!
//! Provider API keys never leave this process's memory + the OS keychain.

use crate::answer::AnswerExtract;
use crate::budget::{BudgetVerdict, Caps};
use crate::cache::CacheEntry;
use crate::embeddings::EmbeddingProviderKind;
use crate::semantic_cache::SemanticEntry;
use crate::state::{AppState, LlmOutcome};
use crate::{answer, cachekey, compress, embeddings, semantic_cache, streaming, util};
use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::Response,
    routing::{get, post},
    Router,
};
use halo_common::pricing::estimate_cost_usd;
use halo_common::telemetry::{PolicyDecision, Provider};
use serde_json::Value;

/// Carries a semantic-cache-miss's already-computed query embedding forward
/// to the post-completion store step, so a request that misses the semantic
/// cache never pays for a second embedding call just to store its own
/// answer.
struct SemanticMissContext {
    partition: String,
    vector: Vec<f32>,
}

/// Which OpenAI/Anthropic surface this request hit.
#[derive(Clone, Copy)]
enum ApiKind {
    AnthropicMessages,
    OpenAiChat,
    OpenAiEmbeddings,
}

impl ApiKind {
    fn task_class(&self) -> &'static str {
        match self {
            ApiKind::AnthropicMessages | ApiKind::OpenAiChat => "chat",
            ApiKind::OpenAiEmbeddings => "embedding",
        }
    }
    fn upstream_path(&self) -> &'static str {
        match self {
            ApiKind::AnthropicMessages => "/v1/messages",
            ApiKind::OpenAiChat => "/v1/chat/completions",
            ApiKind::OpenAiEmbeddings => "/v1/embeddings",
        }
    }
    fn is_chat(&self) -> bool {
        !matches!(self, ApiKind::OpenAiEmbeddings)
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/messages", post(anthropic_messages))
        .route("/v1/chat/completions", post(openai_chat))
        .route("/v1/embeddings", post(openai_embeddings))
        .route("/mcp/:server", post(mcp_proxy))
        .with_state(state)
}

async fn anthropic_messages(State(st): State<AppState>, h: HeaderMap, body: String) -> Response {
    handle_llm(st, h, body, ApiKind::AnthropicMessages).await
}
async fn openai_chat(State(st): State<AppState>, h: HeaderMap, body: String) -> Response {
    handle_llm(st, h, body, ApiKind::OpenAiChat).await
}
async fn openai_embeddings(State(st): State<AppState>, h: HeaderMap, body: String) -> Response {
    handle_llm(st, h, body, ApiKind::OpenAiEmbeddings).await
}

fn json_response(code: StatusCode, ct: &str, body: String) -> Response {
    Response::builder()
        .status(code)
        .header(header::CONTENT_TYPE, ct)
        .body(Body::from(body))
        .unwrap()
}

fn error_response(code: StatusCode, msg: &str) -> Response {
    let body = serde_json::json!({ "error": { "message": msg, "type": "halo_error" } });
    json_response(code, "application/json", body.to_string())
}

fn extract_vkey(headers: &HeaderMap) -> Option<String> {
    if let Some(v) = headers.get("x-api-key").and_then(|h| h.to_str().ok()) {
        return Some(v.to_string());
    }
    if let Some(v) = headers.get(header::AUTHORIZATION).and_then(|h| h.to_str().ok()) {
        return Some(
            v.strip_prefix("Bearer ")
                .map(|s| s.to_string())
                .unwrap_or_else(|| v.to_string()),
        );
    }
    None
}

/// Optional `X-Halo-Subject` sub-identity for cost attribution. Trimmed,
/// length-capped (defence against a client stuffing content into it -- this is
/// metadata, never content), and empty -> `None`.
fn extract_subject(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-halo-subject")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(128).collect())
}

/// Extract (tokens_in, tokens_out, tokens_cached) from a non-streamed
/// provider response body.
fn parse_usage(provider: Provider, json: &Value) -> (u64, u64, u64) {
    let u = match json.get("usage") {
        Some(u) => u,
        None => return (0, 0, 0),
    };
    match provider {
        Provider::Anthropic => {
            let cin = u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            let out = u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            let cached = u
                .get("cache_read_input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            // Anthropic reports fresh input separately from cache reads; total
            // input billed = input_tokens + cache reads + cache creation.
            let created = u
                .get("cache_creation_input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            (cin + cached + created, out, cached)
        }
        _ => {
            let cin = u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            let out = u
                .get("completion_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let cached = u
                .pointer("/prompt_tokens_details/cached_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            (cin, out, cached)
        }
    }
}

/// Resolve the upstream base URL: an agent-specific override (for
/// OpenAI-compatible third parties -- Groq, Together, a local vLLM/Ollama
/// server) wins, else the provider's default.
fn provider_base(provider: Provider, override_url: Option<&str>) -> String {
    if let Some(u) = override_url {
        return u.trim_end_matches('/').to_string();
    }
    match provider {
        Provider::Anthropic => "https://api.anthropic.com".to_string(),
        _ => "https://api.openai.com".to_string(),
    }
}

fn failover_status(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 502 | 503 | 504 | 529)
}

struct BackupHop {
    agent_id: String,
    provider: Provider,
    key: String,
    url: String,
}

/// Route-tier: one backup agent, no recursive hops. None on Free/Cut or
/// when the map/key/egress check fails.
fn route_backup(st: &AppState, agent: &str, kind: ApiKind) -> Option<BackupHop> {
    if !st.entitlements.has(halo_common::license::feature::ROUTE) {
        return None;
    }
    let backup_id = st.cfg.failover.get(agent)?;
    if backup_id.is_empty() || backup_id == agent {
        return None;
    }
    let rec = st
        .keys
        .records()
        .ok()?
        .into_iter()
        .find(|r| r.agent_id == *backup_id && r.is_active())?;
    let key = st.keys.get_secret(&rec.agent_id).ok()?;
    let url = format!(
        "{}{}",
        provider_base(rec.provider, rec.base_url.as_deref()),
        kind.upstream_path()
    );
    if crate::egress::check_egress(&st.cfg.egress, &url).is_err() {
        return None;
    }
    Some(BackupHop {
        agent_id: rec.agent_id,
        provider: rec.provider,
        key,
        url,
    })
}

async fn send_upstream(
    st: &AppState,
    headers: &HeaderMap,
    provider: Provider,
    real_key: &str,
    url: &str,
    outbound: &str,
) -> Result<reqwest::Response, reqwest::Error> {
    let mut req = st
        .http
        .post(url)
        .header(header::CONTENT_TYPE, "application/json")
        .body(outbound.to_string());
    req = match provider {
        Provider::Anthropic => {
            let ver = headers
                .get("anthropic-version")
                .and_then(|h| h.to_str().ok())
                .unwrap_or("2023-06-01");
            req.header("x-api-key", real_key)
                .header("anthropic-version", ver)
        }
        _ => req.header(header::AUTHORIZATION, format!("Bearer {real_key}")),
    };
    req.send().await
}

async fn handle_llm(st: AppState, headers: HeaderMap, body: String, kind: ApiKind) -> Response {
    // 1. Virtual-key auth.
    let vkey = match extract_vkey(&headers) {
        Some(v) => v,
        None => return error_response(StatusCode::UNAUTHORIZED, "missing API key"),
    };
    let record = match st.keys.resolve(&vkey) {
        Ok(Some(r)) => r,
        Ok(None) => {
            return error_response(
                StatusCode::UNAUTHORIZED,
                "unrecognized or revoked Halo virtual key",
            )
        }
        Err(e) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let agent = record.agent_id.clone();
    // Best-effort remote kill: an operator revoked this agent from the relay.
    // The always-local key revocation above is the primary control; this only
    // ever adds to it.
    if st.remote_revocations.is_revoked(&agent) {
        return error_response(
            StatusCode::UNAUTHORIZED,
            "agent remotely revoked (killed from the relay)",
        );
    }
    let mut provider = record.provider;
    let provider_str = provider.as_str();
    // Optional sub-identity hint (channel/sub-agent/thread) for cost
    // attribution when one process/key fans out across many channels.
    let subject = extract_subject(&headers);
    let original: Option<Value> = serde_json::from_str(&body).ok();
    let model = original
        .as_ref()
        .and_then(|j| j.get("model").and_then(|m| m.as_str()))
        .unwrap_or_default()
        .to_string();
    let is_stream = kind.is_chat()
        && original
            .as_ref()
            .map(streaming::wants_stream)
            .unwrap_or(false);

    // 2. Compression (chat only). Cut applies it to the wire. Free still
    // computes the ratio so the dashboard can star "Cut would have saved".
    let cut = st.entitlements.has(halo_common::license::feature::CUT);
    let mut compression_ratio = 1.0f64;
    let mut shadow_savings = 0.0f64;
    let mut outbound = body.clone();
    if kind.is_chat() {
        let c = compress::compress_body(
            &outbound,
            st.cfg.compression.verbose_phrases,
            st.cfg.compression.aggressive_abbreviations,
            st.cfg.compression.whitespace,
        );
        compression_ratio = c.ratio;
        if cut {
            if let Some(b) = c.body {
                outbound = b;
            }
            if matches!(kind, ApiKind::AnthropicMessages) && st.cfg.compression.anthropic_cache_control {
                if let Some(inj) = st.injector.process_anthropic(&outbound) {
                    tracing::debug!(
                        breakpoints = inj.breakpoints,
                        warm = inj.warm,
                        "injected anthropic cache_control breakpoint(s)"
                    );
                    outbound = inj.body;
                }
            }
        } else if let Some(ref compressed) = c.body {
            if c.ratio > 0.0 && c.ratio < 1.0 {
                shadow_savings += crate::util::estimated_input_savings_usd(
                    &st.prices,
                    &model,
                    outbound.len(),
                    compressed.len(),
                );
            }
        }
        // OpenAI-compatible streams only carry a final usage chunk if asked
        // for it; ask automatically so telemetry doesn't silently go blind.
        if is_stream && matches!(provider, Provider::Openai | Provider::Other) {
            if let Ok(mut j) = serde_json::from_str::<Value>(&outbound) {
                streaming::ensure_openai_stream_usage(&mut j);
                outbound = j.to_string();
            }
        }
    }

    // 3. Exact-match cache lookup (on the original request shape). The key is
    // independent of the `stream` flag (see `cachekey::request_cache_key`),
    // so a hit here can come from either a prior streamed or buffered call.
    let cache_key = if kind.is_chat() {
        cachekey::request_cache_key(provider_str, &body)
    } else {
        None
    };
    if let Some(key) = &cache_key {
        if let Ok(Some(entry)) = st.cache.get(key) {
            // A streaming request can only be served from an entry that
            // captured a replayable plain-text answer. Entries without one
            // (written before this field existed, or a response that wasn't
            // safely extractable as plain text) fall through to a live call
            // rather than serving the wrong shape.
            if !is_stream || entry.answer.is_some() {
                if cut {
                    let response = if is_stream {
                        let a = entry.answer.as_ref().expect("checked above");
                        let sse = answer::render_stream(
                            provider,
                            a,
                            &entry.model,
                            entry.tokens_in,
                            entry.tokens_out,
                        );
                        Response::builder()
                            .status(StatusCode::OK)
                            .header(header::CONTENT_TYPE, "text/event-stream")
                            .body(Body::from(sse))
                            .unwrap()
                    } else {
                        json_response(
                            StatusCode::from_u16(entry.status).unwrap_or(StatusCode::OK),
                            &entry.content_type,
                            entry.body.clone(),
                        )
                    };
                    st.finalize_llm_call(LlmOutcome {
                        agent: agent.clone(),
                        subject: subject.clone(),
                        provider,
                        model: entry.model.clone(),
                        tokens_in: entry.tokens_in,
                        tokens_out: entry.tokens_out,
                        tokens_cached: 0,
                        task_class: kind.task_class().into(),
                        latency_ms: 0,
                        compression_ratio: 1.0,
                        decision: PolicyDecision::CacheHit,
                        error_class: String::new(),
                        record_spend: false,
                        streamed: is_stream,
                        actual_cost_override: None,
                        shadow_savings_usd: 0.0,
                    })
                    .await;
                    return response;
                }
                // Free: do not serve the hit. Count what Cut would have saved
                // (the whole provider call) and fall through.
                shadow_savings = estimate_cost_usd(
                    &st.prices,
                    &entry.model,
                    entry.tokens_in,
                    entry.tokens_out,
                    0,
                );
            }
        }
    }

    // 4. Budget preflight (kill switch). Conservative output allowance so one
    // non-streamed request can't overshoot a hard cap. Streaming requests get
    // the SAME pre-flight check (this is the primary enforcement point in
    // both cases) plus a coarse mid-stream stop-loss below, since a single
    // long-running generation can't be pre-charged for its eventual size.
    let approx_in = util::approx_tokens_from_chars(outbound.len());
    let projected = estimate_cost_usd(&st.prices, &model, approx_in, 1024, 0);
    let (gs, gh) = (st.cfg.budget.soft_cap_usd, st.cfg.budget.hard_cap_usd);
    let over = if cut {
        st.cfg.budget.per_agent.iter().find(|a| a.agent_id == agent)
    } else {
        None
    };
    let caps = Caps {
        global_soft: gs,
        global_hard: gh,
        agent_soft: over.and_then(|a| a.soft_cap_usd),
        agent_hard: over.and_then(|a| a.hard_cap_usd),
    };
    let verdict = st
        .ledger
        .check(&agent, projected, caps)
        .unwrap_or(BudgetVerdict::Allow);
    if let BudgetVerdict::HardBlock { scope, spent, cap } = &verdict {
        st.maybe_alert_budget(&agent, &verdict);
        st.finalize_llm_call(LlmOutcome {
            agent: agent.clone(),
            subject: subject.clone(),
            provider,
            model: model.clone(),
            tokens_in: 0,
            tokens_out: 0,
            tokens_cached: 0,
            task_class: kind.task_class().into(),
            latency_ms: 0,
            compression_ratio,
            decision: PolicyDecision::BudgetBlocked,
            error_class: String::new(),
            record_spend: false,
            streamed: false,
            actual_cost_override: None,
            shadow_savings_usd: 0.0,
        })
        .await;
        return error_response(
            StatusCode::PAYMENT_REQUIRED,
            &format!(
                "Halo hard budget cap reached ({scope}: ${spent:.2} spent, ${cap:.2} cap). \
                 Request refused locally. Raise the cap in ~/.halo/config.yaml or run `halo status`."
            ),
        );
    }
    let soft_warned = matches!(verdict, BudgetVerdict::SoftWarn { .. });
    if soft_warned {
        st.maybe_alert_budget(&agent, &verdict);
    }

    // 4.5 Semantic (embedding-similarity) cache. Only reached once the
    // hard-cap check above has passed, so an already-blocked agent never
    // spends even the tiny embedding-lookup cost. Chat-only -- an
    // /v1/embeddings call has nothing to semantically match against.
    //
    // The embedding call's cost is billed the moment it's made (below),
    // independent of whether it turns out to be a hit or a miss: the spend
    // already happened by the time we know which. A hit folds that cost into
    // the single `SemanticCacheHit` telemetry event via `actual_cost_override`;
    // a miss gets its own small `task_class: "embedding"` event immediately,
    // and the resulting vector is carried forward in `semantic_miss` so
    // storing this request's own answer afterward never pays for a second
    // embedding call.
    let mut semantic_miss: Option<SemanticMissContext> = None;
    if kind.is_chat() && st.cfg.semantic_cache.enabled && cut {
        if let Some(orig) = &original {
            if let Some(sq) = semantic_cache::eligible_query(orig) {
                let embed_key = if matches!(st.embedder.kind, EmbeddingProviderKind::Openai) {
                    st.keys.get_secret(embeddings::EmbeddingClient::key_store_id()).ok()
                } else {
                    None
                };
                let embed_started = std::time::Instant::now();
                match st.embedder.embed(&sq.query_text, embed_key.as_deref(), &st.prices).await {
                    Ok(er) => match st.semantic.lookup(&sq.partition, &er.vector, st.cfg.semantic_cache.similarity_threshold) {
                        Ok(Some((entry, similarity))) => {
                            let tokens_in = util::approx_tokens_from_chars(outbound.len());
                            let response = if is_stream {
                                let sse = answer::render_stream(provider, &entry.answer, &model, tokens_in, entry.tokens_out);
                                Response::builder()
                                    .status(StatusCode::OK)
                                    .header(header::CONTENT_TYPE, "text/event-stream")
                                    .body(Body::from(sse))
                                    .unwrap()
                            } else {
                                let json = answer::render_buffered(provider, &entry.answer, &model, tokens_in, entry.tokens_out);
                                json_response(StatusCode::OK, "application/json", json.to_string())
                            };
                            st.audit(serde_json::json!({
                                "kind": "semantic_cache_hit",
                                "agent": agent,
                                "similarity": similarity,
                                "partition": sq.partition,
                                "origin_provider": entry.origin_provider.as_str(),
                                "origin_model": entry.origin_model,
                                "serving_provider": provider.as_str(),
                                "serving_model": model,
                            }));
                            st.finalize_llm_call(LlmOutcome {
                                agent: agent.clone(),
                                subject: subject.clone(),
                                provider,
                                model: model.clone(),
                                tokens_in,
                                tokens_out: entry.tokens_out,
                                tokens_cached: 0,
                                task_class: kind.task_class().into(),
                                latency_ms: embed_started.elapsed().as_millis() as u64,
                                compression_ratio,
                                decision: PolicyDecision::SemanticCacheHit,
                                error_class: String::new(),
                                record_spend: true,
                                streamed: is_stream,
                                actual_cost_override: Some(er.cost_usd),
                                shadow_savings_usd: 0.0,
                            })
                            .await;
                            return response;
                        }
                        Ok(None) => {
                            // Bill the lookup embedding call now -- the spend
                            // already happened, independent of what the live
                            // completion below does next. `actual_cost_override`
                            // trusts the embedding provider's own cost
                            // (correctly $0 for `mock`/`ollama`) instead of
                            // recomputing from the model-name price table.
                            st.finalize_llm_call(LlmOutcome {
                                agent: agent.clone(),
                                subject: subject.clone(),
                                provider,
                                model: st.embedder.model.clone(),
                                tokens_in: er.tokens,
                                tokens_out: 0,
                                tokens_cached: 0,
                                task_class: "embedding".into(),
                                latency_ms: embed_started.elapsed().as_millis() as u64,
                                compression_ratio: 1.0,
                                decision: PolicyDecision::Allow,
                                error_class: String::new(),
                                record_spend: er.cost_usd > 0.0,
                                streamed: false,
                                actual_cost_override: Some(er.cost_usd),
                                shadow_savings_usd: 0.0,
                            })
                            .await;
                            semantic_miss = Some(SemanticMissContext {
                                partition: sq.partition,
                                vector: er.vector,
                            });
                        }
                        Err(e) => tracing::warn!("semantic cache lookup failed: {e}"),
                    },
                    Err(e) => tracing::debug!("semantic cache embed skipped: {e}"),
                }
            }
        }
    }

    // 5. Forward to the real provider with the real key.
    let real_key = match st.keys.get_secret(&agent) {
        Ok(k) => k,
        Err(e) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let base = provider_base(provider, record.base_url.as_deref());
    let url = format!("{base}{}", kind.upstream_path());
    if let Err(denied_host) = crate::egress::check_egress(&st.cfg.egress, &url) {
        st.finalize_llm_call(LlmOutcome {
            agent: agent.clone(),
            subject: subject.clone(),
            provider,
            model: model.clone(),
            tokens_in: 0,
            tokens_out: 0,
            tokens_cached: 0,
            task_class: kind.task_class().into(),
            latency_ms: 0,
            compression_ratio,
            decision: PolicyDecision::EgressDenied,
            error_class: "egress_denied".to_string(),
            record_spend: false,
            streamed: false,
            actual_cost_override: None,
            shadow_savings_usd: 0.0,
        })
        .await;
        return error_response(
            StatusCode::FORBIDDEN,
            &format!(
                "Halo egress policy denied this request: \"{denied_host}\" is blocked by \
                 the denylist (egress.denied_upstreams / starter) or is not on \
                 egress.allowed_upstreams in ~/.halo/config.yaml."
            ),
        );
    }
    let started = std::time::Instant::now();
    let mut resp = match send_upstream(&st, &headers, provider, &real_key, &url, &outbound).await {
        Ok(r) => r,
        Err(e) => {
            if let Some(fb) = route_backup(&st, &agent, kind) {
                match send_upstream(&st, &headers, fb.provider, &fb.key, &fb.url, &outbound).await {
                    Ok(r2) => {
                        st.audit(serde_json::json!({
                            "kind": "failover",
                            "from_agent": agent,
                            "to_agent": fb.agent_id,
                            "reason": if e.is_timeout() { "timeout" } else { "transport" },
                        }));
                        provider = fb.provider;
                        r2
                    }
                    Err(e2) => {
                        let err_class = if e2.is_timeout() { "timeout" } else { "transport" };
                        st.finalize_llm_call(LlmOutcome {
                            agent: agent.clone(),
                            subject: subject.clone(),
                            provider,
                            model: model.clone(),
                            tokens_in: 0,
                            tokens_out: 0,
                            tokens_cached: 0,
                            task_class: kind.task_class().into(),
                            latency_ms: started.elapsed().as_millis() as u64,
                            compression_ratio,
                            decision: PolicyDecision::Allow,
                            error_class: err_class.into(),
                            record_spend: false,
                            streamed: false,
                            actual_cost_override: None,
                            shadow_savings_usd: 0.0,
                        })
                        .await;
                        return error_response(
                            StatusCode::BAD_GATEWAY,
                            &format!("upstream error: {e2}"),
                        );
                    }
                }
            } else {
                let err_class = if e.is_timeout() { "timeout" } else { "transport" };
                st.finalize_llm_call(LlmOutcome {
                    agent: agent.clone(),
                    subject: subject.clone(),
                    provider,
                    model: model.clone(),
                    tokens_in: 0,
                    tokens_out: 0,
                    tokens_cached: 0,
                    task_class: kind.task_class().into(),
                    latency_ms: started.elapsed().as_millis() as u64,
                    compression_ratio,
                    decision: PolicyDecision::Allow,
                    error_class: err_class.into(),
                    record_spend: false,
                    streamed: false,
                    actual_cost_override: None,
                    shadow_savings_usd: 0.0,
                })
                .await;
                return error_response(StatusCode::BAD_GATEWAY, &format!("upstream error: {e}"));
            }
        }
    };

    if failover_status(resp.status()) {
        if let Some(fb) = route_backup(&st, &agent, kind) {
            match send_upstream(&st, &headers, fb.provider, &fb.key, &fb.url, &outbound).await {
                Ok(r2) => {
                    st.audit(serde_json::json!({
                        "kind": "failover",
                        "from_agent": agent,
                        "to_agent": fb.agent_id,
                        "reason": format!("http_{}", resp.status().as_u16()),
                    }));
                    provider = fb.provider;
                    resp = r2;
                }
                Err(_) => {}
            }
        }
    }

    let status = resp.status();

    // 5b. Genuine streaming passthrough -- forward bytes as they arrive
    // instead of buffering the full completion. Accounting happens once the
    // stream ends, off the hot path, via the spawned task in
    // `stream_response`.
    if is_stream && status.is_success() {
        return stream_response(
            st,
            agent,
            subject,
            provider,
            model,
            kind.task_class().to_string(),
            compression_ratio,
            soft_warned,
            caps,
            resp,
            started,
            cache_key,
            semantic_miss,
            shadow_savings,
        )
        .await;
    }

    let ct = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    let resp_body = resp.text().await.unwrap_or_default();
    let latency_ms = started.elapsed().as_millis() as u64;

    // 6. Usage + cost accounting.
    let parsed: Option<Value> = serde_json::from_str(&resp_body).ok();
    let (tokens_in, tokens_out, tokens_cached) = parsed
        .as_ref()
        .map(|j| parse_usage(provider, j))
        .unwrap_or((0, 0, 0));
    let effective_model = parsed
        .as_ref()
        .and_then(|j| j.get("model").and_then(|m| m.as_str()))
        .map(str::to_string)
        .unwrap_or(model.clone());

    let decision = if soft_warned {
        PolicyDecision::SoftCapWarn
    } else {
        PolicyDecision::Allow
    };
    let error_class = if status.is_success() {
        String::new()
    } else {
        format!("http_{}", status.as_u16())
    };

    st.finalize_llm_call(LlmOutcome {
        agent: agent.clone(),
        subject: subject.clone(),
        provider,
        model: effective_model.clone(),
        tokens_in,
        tokens_out,
        tokens_cached,
        task_class: kind.task_class().into(),
        latency_ms,
        compression_ratio,
        decision,
        error_class,
        record_spend: status.is_success(),
        streamed: false,
        actual_cost_override: None,
        shadow_savings_usd: shadow_savings,
    })
    .await;

    // 7. Store in cache on a clean success. `answer_extract` is `None` for
    // tool-call responses (or anything else not safely summarizable as plain
    // text) -- those still get an exact-match entry (for a byte-identical
    // future non-streaming request) but never seed the semantic cache and
    // never let a future *streaming* request replay this exact-match entry.
    if status.is_success() {
        let answer_extract: Option<AnswerExtract> = parsed.as_ref().and_then(|j| answer::from_buffered(provider, j));
        if let Some(key) = &cache_key {
            let entry = CacheEntry {
                status: status.as_u16(),
                content_type: ct.clone(),
                body: resp_body.clone(),
                model: effective_model.clone(),
                tokens_in,
                tokens_out,
                created_at: chrono::Utc::now().timestamp(),
                answer: answer_extract.clone(),
            };
            let _ = st.cache.put(key, &entry);
        }
        if let (Some(ctx), Some(ans)) = (&semantic_miss, &answer_extract) {
            let se = SemanticEntry {
                embedding: ctx.vector.clone(),
                partition: ctx.partition.clone(),
                answer: ans.clone(),
                origin_provider: provider,
                origin_model: effective_model,
                tokens_out,
                created_at: chrono::Utc::now().timestamp(),
            };
            let _ = st.semantic.store(&uuid::Uuid::new_v4().to_string(), &se);
        }
    }

    json_response(
        StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::OK),
        &ct,
        resp_body,
    )
}

/// Stream the upstream response to the client byte-for-byte as it arrives,
/// while a background task accumulates it for post-hoc usage/cost accounting
/// and a coarse runaway-cost stop-loss.
///
/// Accounting can only happen after the fact for a streamed response (we
/// don't know the final token count until the provider says so), which is
/// also true of every comparable proxy -- the pre-flight check above is the
/// primary enforcement point. This stop-loss is a backstop against a
/// pathological runaway generation, not the main guarantee.
#[allow(clippy::too_many_arguments)]
async fn stream_response(
    st: AppState,
    agent: String,
    subject: Option<String>,
    provider: Provider,
    fallback_model: String,
    task_class: String,
    compression_ratio: f64,
    soft_warned: bool,
    caps: Caps,
    mut upstream: reqwest::Response,
    started: std::time::Instant,
    cache_key: Option<String>,
    semantic_miss: Option<SemanticMissContext>,
    shadow_savings_usd: f64,
) -> Response {
    let ct = upstream
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("text/event-stream")
        .to_string();

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<axum::body::Bytes, std::io::Error>>(32);

    tokio::spawn(async move {
        let mut acc: Vec<u8> = Vec::new();
        let mut abort_reason: Option<String> = None;

        loop {
            match upstream.chunk().await {
                Ok(Some(bytes)) => {
                    acc.extend_from_slice(&bytes);
                    if tx.send(Ok(bytes)).await.is_err() {
                        break; // client disconnected; stop pulling from upstream.
                    }
                    // Coarse stop-loss: trip only when we're WAY past the hard cap
                    // (3x) so the char/4 approximation and provider framing
                    // overhead can't false-positive on a normal request.
                    if let Some(hard) = caps.agent_hard.or(caps.global_hard) {
                        if hard > 0.0 {
                            let approx_out = util::approx_tokens_from_chars(acc.len());
                            let approx_cost =
                                estimate_cost_usd(&st.prices, &fallback_model, 0, approx_out, 0);
                            if approx_cost > hard * 3.0 {
                                abort_reason = Some(format!(
                                    "runaway_stream_over_hard_cap(~${approx_cost:.2}_vs_${hard:.2})"
                                ));
                                tracing::warn!(
                                    "halo: aborting runaway stream for agent '{agent}': {}",
                                    abort_reason.as_deref().unwrap_or("")
                                );
                                break;
                            }
                        }
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!("halo: stream read error for agent '{agent}': {e}");
                    abort_reason = Some("transport".to_string());
                    break;
                }
            }
        }
        drop(tx); // closes the response body to the client.

        let text = String::from_utf8_lossy(&acc);
        let result = streaming::extract_stream_result(provider, &text, &fallback_model);
        let (tokens_in, tokens_out, tokens_cached, model) =
            (result.tokens_in, result.tokens_out, result.tokens_cached, result.model.clone());
        let decision = if abort_reason.is_some() {
            PolicyDecision::BudgetBlocked
        } else if soft_warned {
            PolicyDecision::SoftCapWarn
        } else {
            PolicyDecision::Allow
        };
        st.finalize_llm_call(LlmOutcome {
            agent: agent.clone(),
            subject: subject.clone(),
            provider,
            model: model.clone(),
            tokens_in,
            tokens_out,
            tokens_cached,
            task_class,
            latency_ms: started.elapsed().as_millis() as u64,
            compression_ratio,
            decision,
            error_class: abort_reason.clone().unwrap_or_default(),
            record_spend: true, // bill whatever was actually streamed, even if aborted.
            streamed: true,
            actual_cost_override: None,
            shadow_savings_usd,
        })
        .await;

        // Feed the exact-match and semantic caches from a cleanly-finished
        // stream, exactly like the buffered path does. Never on an abort --
        // partial/truncated content must not be replayed as a complete
        // answer later.
        if abort_reason.is_none() {
            if let Some(ans) = &result.answer {
                if let Some(key) = &cache_key {
                    let body = answer::render_buffered(provider, ans, &model, tokens_in, tokens_out).to_string();
                    let entry = CacheEntry {
                        status: 200,
                        content_type: "application/json".into(),
                        body,
                        model: model.clone(),
                        tokens_in,
                        tokens_out,
                        created_at: chrono::Utc::now().timestamp(),
                        answer: Some(ans.clone()),
                    };
                    let _ = st.cache.put(key, &entry);
                }
                if let Some(ctx) = &semantic_miss {
                    let se = SemanticEntry {
                        embedding: ctx.vector.clone(),
                        partition: ctx.partition.clone(),
                        answer: ans.clone(),
                        origin_provider: provider,
                        origin_model: model,
                        tokens_out,
                        created_at: chrono::Utc::now().timestamp(),
                    };
                    let _ = st.semantic.store(&uuid::Uuid::new_v4().to_string(), &se);
                }
            }
        }
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, ct)
        .body(Body::from_stream(stream))
        .unwrap()
}

async fn mcp_proxy(
    State(st): State<AppState>,
    Path(server): Path<String>,
    body: String,
) -> Response {
    let mgr = match &st.mcp {
        Some(m) => m.clone(),
        None => return error_response(StatusCode::NOT_FOUND, "no MCP servers configured"),
    };
    let frame: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, &format!("invalid JSON-RPC: {e}")),
    };

    match mgr
        .proxy(&server, frame, st.cfg.mcp_block_uncloaked_secrets)
        .await
    {
        Ok((resp, report)) => {
            st.audit(serde_json::json!({
                "kind": "mcp_call",
                "server": server,
                "method": report.method,
                "tool": report.tool,
                "uncloaked": report.uncloaked,
                "scrubbed": report.scrubbed,
                "outbound_secret_kinds": report.outbound_secret_kinds,
                "inbound_secret_kinds": report.inbound_secret_kinds,
                "blocked": false,
            }));
            json_response(StatusCode::OK, "application/json", resp.to_string())
        }
        Err(e) => {
            let msg = e.to_string();
            if msg.starts_with("MCP blocked") {
                st.audit(serde_json::json!({
                    "kind": "mcp_blocked",
                    "server": server,
                    "reason": msg,
                }));
                error_response(StatusCode::FORBIDDEN, &msg)
            } else {
                error_response(StatusCode::BAD_GATEWAY, &format!("MCP error: {e}"))
            }
        }
    }
}
