//! Best-effort remote kill (paid feature `remote_kill`).
//!
//! An operator can revoke an agent from the relay; the shim pulls that list on
//! a slow poll and refuses matching agents at ingress. This is deliberately a
//! *convenience overlay*, never the primary control: the always-local hard-cap
//! kill switch (`budget.rs`) and `halo kill` (local key revocation) work with
//! zero network and are never gated. If the relay is unreachable, the local
//! controls still fully protect the user -- remote kill just can't add to them.

use std::collections::HashSet;
use std::sync::{Arc, RwLock};

/// Relay endpoint returning the current revoked-agent list for a device.
const REVOCATIONS_PATH: &str = "/v1/revocations";
/// How often to refresh. Slow on purpose -- this is a backstop, not a hot path.
const POLL_INTERVAL_SECS: u64 = 30;

/// Shared, cheaply-cloneable set of remotely-revoked agent ids.
#[derive(Clone, Default)]
pub struct RemoteRevocations {
    inner: Arc<RwLock<HashSet<String>>>,
}

impl RemoteRevocations {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ingress check. A poisoned lock fails open (returns `false`): a broken
    /// overlay must never wedge the proxy, and the local controls still apply.
    pub fn is_revoked(&self, agent: &str) -> bool {
        self.inner
            .read()
            .map(|s| s.contains(agent))
            .unwrap_or(false)
    }

    fn replace(&self, ids: HashSet<String>) {
        if let Ok(mut w) = self.inner.write() {
            *w = ids;
        }
    }
}

#[derive(serde::Deserialize)]
struct RevocationsResponse {
    #[serde(default)]
    revoked: Vec<String>,
}

/// Poll the relay for the revoked-agent list forever, updating `store`. Only
/// spawned when a relay is configured AND the `remote_kill` feature is
/// entitled. Every failure is swallowed (logged at debug) -- the overlay just
/// keeps whatever it last knew.
pub async fn poll_loop(
    client: reqwest::Client,
    relay_url: String,
    relay_token: Option<String>,
    device_id: String,
    store: RemoteRevocations,
) {
    let url = format!("{}{}", relay_url.trim_end_matches('/'), REVOCATIONS_PATH);
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(POLL_INTERVAL_SECS));
    loop {
        tick.tick().await;
        let mut req = client.get(&url).query(&[("device_id", &device_id)]);
        if let Some(tok) = &relay_token {
            req = req.bearer_auth(tok);
        }
        match req.send().await {
            Ok(resp) if resp.status().is_success() => {
                match resp.json::<RevocationsResponse>().await {
                    Ok(body) => {
                        let set: HashSet<String> = body.revoked.into_iter().collect();
                        if !set.is_empty() {
                            tracing::info!(count = set.len(), "remote kill: refreshed revoked agents");
                        }
                        store.replace(set);
                    }
                    Err(e) => tracing::debug!(error = %e, "remote kill: bad revocations body"),
                }
            }
            Ok(resp) => tracing::debug!(status = %resp.status(), "remote kill: relay non-2xx"),
            Err(e) => tracing::debug!(error = %e, "remote kill: poll failed"),
        }
    }
}
