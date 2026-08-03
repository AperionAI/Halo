//! Budget alerting webhooks (paid feature `alerting`).
//!
//! When a budget soft/hard cap is crossed, POST a small JSON event to a
//! configured webhook. Strictly fire-and-forget: alerting can never slow down
//! or block the request hot path, and a webhook that's down/slow must not
//! affect proxy behaviour. Metadata only -- same trust invariant as telemetry:
//! no prompt/response text ever leaves the box.

use crate::budget::BudgetVerdict;
use serde_json::json;

/// The kind of cap crossing, for the webhook payload's `event` field.
fn verdict_fields(v: &BudgetVerdict) -> Option<(&'static str, &str, f64, f64)> {
    match v {
        BudgetVerdict::SoftWarn { scope, spent, cap } => {
            Some(("budget.soft_cap", scope, *spent, *cap))
        }
        BudgetVerdict::HardBlock { scope, spent, cap } => {
            Some(("budget.hard_cap", scope, *spent, *cap))
        }
        BudgetVerdict::Allow => None,
    }
}

/// Fire a budget alert without blocking the caller. No-op when `webhook` is
/// `None` or the verdict isn't a crossing. Callers gate on entitlement before
/// calling this so a free-tier install never hits the network here.
pub fn fire_budget_alert(
    client: &reqwest::Client,
    webhook: Option<&str>,
    device_id: &str,
    agent: &str,
    verdict: &BudgetVerdict,
) {
    let url = match webhook {
        Some(u) if !u.trim().is_empty() => u.trim().to_string(),
        _ => return,
    };
    let (event, scope, spent, cap) = match verdict_fields(verdict) {
        Some(f) => f,
        None => return,
    };

    let payload = json!({
        "event": event,
        "device_id": device_id,
        "agent_id": agent,
        "scope": scope,
        "spent_usd": spent,
        "cap_usd": cap,
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "source": "smartflow-halo",
    });
    let client = client.clone();
    let scope = scope.to_string();
    tokio::spawn(async move {
        let res = client
            .post(&url)
            .json(&payload)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await;
        match res {
            Ok(r) if r.status().is_success() => {}
            Ok(r) => tracing::warn!(status = %r.status(), scope, "budget alert webhook non-2xx"),
            Err(e) => tracing::warn!(error = %e, scope, "budget alert webhook failed"),
        }
    });
}
