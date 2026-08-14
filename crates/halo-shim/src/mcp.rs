//! MCP interception -- reuse Shield, don't rebuild.
//!
//! Shield is designed 1:1 (one instance wraps one upstream MCP server). Halo's
//! shim needs to front ALL of a user's configured MCP servers from a single
//! process: the runtime's MCP config is pointed at Halo, and Halo holds the
//! real server definitions. So Halo runs N stdio JSON-RPC client loops
//! internally -- one per registered server -- sharing ONE `CloakVault` and one
//! audit stream, rather than N separate OS processes.
//!
//! The security transforms at the seam are Shield's, used as a library:
//!   * `aperion_shield::CloakVault` -- resolve `{{cloak:NAME}}` placeholders on
//!     the outbound copy, scrub leaked secret values out of results before the
//!     agent sees them.
//!   * `aperion_shield::taint::scan_secrets` -- credential-shape detection, so
//!     a raw secret handed to a tool (uncloaked) or echoed back by a server is
//!     flagged for the audit log.

use crate::config::McpServerConfig;
use aperion_shield::taint::scan_secrets;
use aperion_shield::CloakVault;
use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{oneshot, Mutex};

/// What happened to one proxied MCP frame, for the audit trail. Carries no
/// secret values -- only counts and kinds.
#[derive(Debug, Default, Clone)]
pub struct SeamReport {
    pub method: String,
    pub tool: Option<String>,
    /// Placeholders resolved on the outbound copy.
    pub uncloaked: bool,
    /// Secret values scrubbed out of the result before the agent saw them.
    pub scrubbed: bool,
    /// Credential-shape kinds detected leaving toward the tool (uncloaked).
    pub outbound_secret_kinds: Vec<String>,
    /// Credential-shape kinds detected in the tool's result.
    pub inbound_secret_kinds: Vec<String>,
}

/// A single upstream stdio MCP server with request/response correlation.
struct McpServer {
    name: String,
    stdin: Mutex<ChildStdin>,
    pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>,
    next_id: AtomicI64,
}

impl McpServer {
    async fn spawn(cfg: &McpServerConfig) -> Result<Arc<Self>> {
        let mut child = Command::new(&cfg.command)
            .args(&cfg.args)
            .envs(&cfg.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("spawning MCP server '{}'", cfg.name))?;

        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
        let pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // Reader task: dispatch responses by id; drop notifications for v1.
        let pending_reader = pending.clone();
        let server_name = cfg.name.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<Value>(&line) {
                    if let Some(id) = v.get("id").and_then(|i| i.as_i64()) {
                        if let Some(tx) = pending_reader.lock().await.remove(&id) {
                            let _ = tx.send(v);
                        }
                    }
                }
            }
            // EOF or read error: server process is gone.
            tracing::warn!("MCP server '{server_name}' stdout closed");
        });

        // Send the standard MCP initialize handshake so servers that require
        // it are ready before the agent's first tool call.
        let srv = Arc::new(Self {
            name: cfg.name.clone(),
            stdin: Mutex::new(stdin),
            pending,
            next_id: AtomicI64::new(1),
        });
        let init = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "smartflow-halo", "version": env!("CARGO_PKG_VERSION") }
            }
        });
        let _ = srv.raw_request(init).await; // best-effort; ignore if unsupported.
        Ok(srv)
    }

    /// Send a frame and await the correlated response. Rewrites the id to a
    /// private counter and restores the caller's id on the way back.
    async fn raw_request(&self, mut frame: Value) -> Result<Value> {
        let original_id = frame.get("id").cloned();
        let our_id = self.next_id.fetch_add(1, Ordering::SeqCst);
        if let Some(obj) = frame.as_object_mut() {
            obj.insert("id".into(), Value::from(our_id));
        }

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(our_id, tx);

        let mut line = serde_json::to_string(&frame)?;
        line.push('\n');
        {
            let mut stdin = self.stdin.lock().await;
            stdin
                .write_all(line.as_bytes())
                .await
                .context("writing to MCP server stdin")?;
            stdin.flush().await.ok();
        }

        let timed_out = tokio::time::timeout(std::time::Duration::from_secs(120), rx).await;
        // On timeout the receiver is dropped without ever being resolved, which
        // would otherwise leak the `pending` entry forever (and let a very late
        // reply from the server incorrectly match a future request that happens
        // to reuse... it can't reuse this id since the counter is monotonic, but
        // the entry would sit in the map indefinitely). Always clean it up.
        let mut resp = match timed_out {
            Ok(inner) => {
                inner.map_err(|_| anyhow!("MCP server '{}' dropped the response", self.name))?
            }
            Err(_) => {
                self.pending.lock().await.remove(&our_id);
                return Err(anyhow!("MCP server '{}' timed out", self.name));
            }
        };

        if let (Some(obj), Some(id)) = (resp.as_object_mut(), original_id) {
            obj.insert("id".into(), id);
        }
        Ok(resp)
    }
}

/// Owns all configured MCP servers and the shared cloak vault.
pub struct McpManager {
    servers: HashMap<String, Arc<McpServer>>,
    cloak: CloakVault,
}

impl McpManager {
    /// Spawn every configured server. Reuses Shield's home-dir cloak vault so
    /// secrets registered with `aperion-shield` are honored here too.
    pub async fn start(configs: &[McpServerConfig]) -> Result<Self> {
        let mut servers = HashMap::new();
        for cfg in configs {
            match McpServer::spawn(cfg).await {
                Ok(srv) => {
                    servers.insert(cfg.name.clone(), srv);
                }
                Err(e) => tracing::error!("failed to start MCP server '{}': {e}", cfg.name),
            }
        }
        Ok(Self {
            servers,
            cloak: CloakVault::load(true),
        })
    }

    pub fn server_names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.servers.keys().cloned().collect();
        v.sort();
        v
    }

    /// Proxy one JSON-RPC frame to the named server, applying cloak/taint at
    /// the seam. Returns the (possibly scrubbed) response plus a metadata-only
    /// `SeamReport` for the audit log.
    ///
    /// When `block_uncloaked` is true (the default), uncloaked secret shapes
    /// in tool arguments refuse the call *before* it is forwarded.
    pub async fn proxy(
        &self,
        server: &str,
        frame: Value,
        block_uncloaked: bool,
    ) -> Result<(Value, SeamReport)> {
        let srv = self
            .servers
            .get(server)
            .ok_or_else(|| anyhow!("unknown MCP server '{server}'"))?;

        let mut report = SeamReport {
            method: frame
                .get("method")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string(),
            tool: frame
                .pointer("/params/name")
                .and_then(|n| n.as_str())
                .map(str::to_string),
            ..Default::default()
        };

        // Outbound: detect raw (uncloaked) secret shapes in the arguments the
        // agent is trying to hand the tool -- the leak we most want to catch.
        if let Some(args) = frame.pointer("/params/arguments") {
            let text = args.to_string();
            let hits = scan_secrets(&text);
            if !hits.is_empty() {
                report.outbound_secret_kinds =
                    hits.into_iter().map(|m| m.kind.to_string()).collect();
            }
        }

        if block_uncloaked && !report.outbound_secret_kinds.is_empty() {
            let kinds = report.outbound_secret_kinds.join(", ");
            return Err(anyhow!(
                "MCP blocked: uncloaked secret shape(s) in tool arguments ({kinds}). \
                 Use {{{{cloak:NAME}}}} placeholders."
            ));
        }

        // Resolve {{cloak:NAME}} placeholders on the copy we forward upstream.
        let outbound = match self.cloak.uncloak_request(&frame) {
            Some(resolved) => {
                report.uncloaked = true;
                serde_json::from_str(&resolved).unwrap_or(frame.clone())
            }
            None => frame.clone(),
        };

        let resp = srv.raw_request(outbound).await?;

        // Inbound: detect credential shapes echoed back by the server...
        if let Some(result) = resp.get("result") {
            let hits = scan_secrets(&result.to_string());
            if !hits.is_empty() {
                report.inbound_secret_kinds =
                    hits.into_iter().map(|m| m.kind.to_string()).collect();
            }
        }
        // ...and scrub any of OUR registered secret values back to placeholders
        // before the agent (and its model context) ever sees them.
        let scrubbed = match self.cloak.scrub_response(&resp) {
            Some(s) => {
                report.scrubbed = true;
                serde_json::from_str(&s).unwrap_or(resp)
            }
            None => resp,
        };

        Ok((scrubbed, report))
    }
}
