//! AI usage / governance registry: a metadata-only "what's running, on what,
//! at what cost" export -- the evidence pack a compliance/audit reviewer (or
//! a City-of-Austin-Resolution-55-style AI-inventory mandate) asks for.
//!
//! Pure aggregation over data Halo already tracks -- no new instrumentation,
//! no prompt/response content, and MCP server `env` (which can hold secrets)
//! is never included. Consistent with the metadata-only trust invariant in
//! `docs/TELEMETRY_SCHEMA.md`.

use crate::config::McpServerConfig;
use crate::report;
use halo_common::pricing::PriceTable;
use halo_common::telemetry::TelemetryEvent;
use halo_common::vkey::VirtualKeyRecord;
use halo_common::Entitlements;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct RegistryAgent {
    pub name: String,
    pub provider: String,
    /// "active" | "revoked".
    pub status: String,
    /// Host only (no path/query), when the agent uses a non-default
    /// `base_url` override. `None` for the provider's own default endpoint.
    pub base_url_host: Option<String>,
    pub created_at: String,
    pub requests: u64,
    pub spend_usd: f64,
    pub savings_usd: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RegistryMcp {
    pub name: String,
    /// Executable only -- never `env`, which can hold secrets.
    pub command: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RegistryReport {
    pub generated_at: String,
    pub halo_version: String,
    pub device_id: String,
    pub tier: String,
    pub agents: Vec<RegistryAgent>,
    pub mcp_servers: Vec<RegistryMcp>,
}

/// Compose the registry from existing sources: virtual-key records (agents),
/// the local telemetry log (spend/savings/requests, via the same rollup
/// `halo report` uses), configured MCP servers, and the resolved
/// entitlements. No I/O of its own -- callers (CLI, dashboard) gather the
/// inputs so this stays trivially unit-testable.
pub fn build_registry(
    records: &[VirtualKeyRecord],
    events: &[TelemetryEvent],
    prices: &PriceTable,
    mcp_servers: &[McpServerConfig],
    entitlements: &Entitlements,
    device_id: &str,
) -> RegistryReport {
    let rollup = report::build(events, None, prices);
    let agents = records
        .iter()
        .map(|r| {
            let roll = rollup.by_agent.get(&r.agent_id);
            RegistryAgent {
                name: r.agent_id.clone(),
                provider: r.provider.as_str().to_string(),
                status: if r.is_active() { "active".to_string() } else { "revoked".to_string() },
                base_url_host: r.base_url.as_deref().and_then(host_of),
                created_at: r.created_at.to_rfc3339(),
                requests: roll.map(|x| x.requests).unwrap_or(0),
                spend_usd: roll.map(|x| x.actual_cost).unwrap_or(0.0),
                savings_usd: roll.map(|x| x.savings()).unwrap_or(0.0),
            }
        })
        .collect();
    let mcp = mcp_servers
        .iter()
        .map(|m| RegistryMcp {
            name: m.name.clone(),
            command: m.command.clone(),
        })
        .collect();
    RegistryReport {
        generated_at: chrono::Utc::now().to_rfc3339(),
        halo_version: env!("CARGO_PKG_VERSION").to_string(),
        device_id: device_id.to_string(),
        tier: entitlements.tier_label.clone(),
        agents,
        mcp_servers: mcp,
    }
}

fn host_of(url: &str) -> Option<String> {
    reqwest::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_string))
}

/// One row per agent, RFC 4180-ish (quotes doubled, fields with a comma/
/// quote/newline wrapped in quotes). No external `csv` crate -- the schema
/// is small and fixed, and Halo prefers zero extra dependencies for a
/// one-off export like this.
pub fn agents_to_csv(report: &RegistryReport) -> String {
    let mut out = String::new();
    out.push_str("name,provider,status,base_url_host,created_at,requests,spend_usd,savings_usd\n");
    for a in &report.agents {
        let fields = [
            a.name.as_str(),
            a.provider.as_str(),
            a.status.as_str(),
            a.base_url_host.as_deref().unwrap_or(""),
            a.created_at.as_str(),
            &a.requests.to_string(),
            &format!("{:.6}", a.spend_usd),
            &format!("{:.6}", a.savings_usd),
        ];
        out.push_str(&fields.iter().map(|f| csv_escape(f)).collect::<Vec<_>>().join(","));
        out.push('\n');
    }
    out
}

fn csv_escape(field: &str) -> String {
    if field.contains(',') || field.contains('"') || field.contains('\n') {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use halo_common::telemetry::{PolicyDecision, Provider};

    fn rec(agent_id: &str, active: bool) -> VirtualKeyRecord {
        VirtualKeyRecord {
            agent_id: agent_id.to_string(),
            virtual_key: format!("sf_live_{agent_id}_abc"),
            provider: Provider::Anthropic,
            created_at: chrono::Utc::now(),
            revoked_at: if active { None } else { Some(chrono::Utc::now()) },
            base_url: None,
        }
    }

    fn ev(agent_id: &str) -> TelemetryEvent {
        TelemetryEvent {
            device_id: "dev1".into(),
            agent_id: agent_id.into(),
            subject: None,
            timestamp: chrono::Utc::now(),
            provider: Provider::Anthropic,
            model: "claude-3-5-sonnet".into(),
            tokens_in: 100,
            tokens_out: 50,
            tokens_cached: 0,
            cache_hit: false,
            task_class: "chat".into(),
            latency_ms: 10,
            estimated_cost: 0.05,
            counterfactual_cost: 0.05,
            policy_decision: PolicyDecision::Allow,
            compression_ratio: 1.0,
            error_class: String::new(),
        }
    }

    fn free_entitlements() -> Entitlements {
        Entitlements::default()
    }

    #[test]
    fn build_registry_aggregates_agents_and_spend() {
        let records = vec![rec("researcher", true), rec("old-agent", false)];
        let events = vec![ev("researcher"), ev("researcher")];
        let prices = PriceTable::default();
        let report = build_registry(&records, &events, &prices, &[], &free_entitlements(), "dev1");

        assert_eq!(report.device_id, "dev1");
        assert_eq!(report.tier, "free");
        assert_eq!(report.agents.len(), 2);
        let researcher = report.agents.iter().find(|a| a.name == "researcher").unwrap();
        assert_eq!(researcher.status, "active");
        assert_eq!(researcher.requests, 2);
        assert!(researcher.spend_usd > 0.0);
        let old = report.agents.iter().find(|a| a.name == "old-agent").unwrap();
        assert_eq!(old.status, "revoked");
        assert_eq!(old.requests, 0);
    }

    #[test]
    fn mcp_servers_never_carry_env() {
        let mcp = vec![McpServerConfig {
            name: "fs".to_string(),
            command: "npx".to_string(),
            args: vec!["mcp-server-fs".to_string()],
            env: std::collections::BTreeMap::from([("SECRET_TOKEN".to_string(), "super-secret".to_string())]),
        }];
        let report = build_registry(&[], &[], &PriceTable::default(), &mcp, &free_entitlements(), "dev1");
        assert_eq!(report.mcp_servers.len(), 1);
        assert_eq!(report.mcp_servers[0].command, "npx");
        // The secret-leak guard: serialize the whole report and confirm the
        // secret value never appears anywhere in it.
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("super-secret"), "MCP env leaked into the registry export");
        assert!(!json.contains("SECRET_TOKEN"), "MCP env key leaked into the registry export");
    }

    #[test]
    fn csv_has_stable_header_and_escapes_special_chars() {
        let mut records = vec![rec("agent,with,commas", true)];
        records[0].base_url = Some("https://\"quoted\".example.com".to_string());
        let report = build_registry(&records, &[], &PriceTable::default(), &[], &free_entitlements(), "dev1");
        let csv = agents_to_csv(&report);
        let mut lines = csv.lines();
        assert_eq!(
            lines.next().unwrap(),
            "name,provider,status,base_url_host,created_at,requests,spend_usd,savings_usd"
        );
        let row = lines.next().unwrap();
        assert!(row.starts_with("\"agent,with,commas\","), "comma-containing field must be quoted, got: {row}");
    }

    #[test]
    fn empty_registry_has_empty_collections_not_errors() {
        let report = build_registry(&[], &[], &PriceTable::default(), &[], &free_entitlements(), "dev1");
        assert!(report.agents.is_empty());
        assert!(report.mcp_servers.is_empty());
        let csv = agents_to_csv(&report);
        assert_eq!(csv.lines().count(), 1); // header only
    }
}
