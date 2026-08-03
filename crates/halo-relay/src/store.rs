//! SQLite-backed telemetry store + summary aggregation.
//!
//! One file, via rusqlite's bundled amalgamation (no system libsqlite3). A
//! single connection behind a mutex is plenty at v1 scale and keeps ops cost
//! near zero -- no Postgres, no Redis, no Mongo.

use crate::counterfactual::{canonical, EventFacts};
use anyhow::Result;
use halo_common::pricing::{decompose_savings, PriceTable};
use halo_common::telemetry::{PolicyDecision, TelemetryEvent};
use rusqlite::{params, Connection};
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::Mutex;

pub struct Store {
    conn: Mutex<Connection>,
    prices: PriceTable,
}

impl Store {
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                device_id TEXT NOT NULL,
                agent_id TEXT NOT NULL,
                subject TEXT,
                ts INTEGER NOT NULL,
                provider TEXT NOT NULL,
                model TEXT NOT NULL,
                tokens_in INTEGER NOT NULL,
                tokens_out INTEGER NOT NULL,
                tokens_cached INTEGER NOT NULL,
                cache_hit INTEGER NOT NULL,
                task_class TEXT NOT NULL,
                latency_ms INTEGER NOT NULL,
                estimated_cost REAL NOT NULL,
                counterfactual_cost REAL NOT NULL,
                policy_decision TEXT NOT NULL,
                compression_ratio REAL NOT NULL,
                error_class TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_events_ts ON events(ts);
            CREATE INDEX IF NOT EXISTS idx_events_device ON events(device_id);
            -- Remote-kill list the shim polls (paid `remote_kill`). device_id
            -- '*' means fleet-wide; a specific id scopes to one device.
            CREATE TABLE IF NOT EXISTS revocations (
                device_id TEXT NOT NULL,
                agent_id TEXT NOT NULL,
                ts INTEGER NOT NULL,
                PRIMARY KEY (device_id, agent_id)
            );",
        )?;
        // Migrate pre-existing DBs that were created before the subject column.
        // Ignore the "duplicate column" error on already-migrated files.
        let _ = conn.execute("ALTER TABLE events ADD COLUMN subject TEXT", []);
        Ok(Self {
            conn: Mutex::new(conn),
            prices: PriceTable::default(),
        })
    }

    pub fn insert_batch(&self, device_id: &str, events: &[TelemetryEvent]) -> Result<usize> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO events (device_id, agent_id, subject, ts, provider, model, tokens_in,
                    tokens_out, tokens_cached, cache_hit, task_class, latency_ms, estimated_cost,
                    counterfactual_cost, policy_decision, compression_ratio, error_class)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
            )?;
            for e in events {
                // Trust the batch envelope's device_id over any client-set field.
                stmt.execute(params![
                    device_id,
                    e.agent_id,
                    e.subject,
                    e.timestamp.timestamp(),
                    e.provider.as_str(),
                    e.model,
                    e.tokens_in as i64,
                    e.tokens_out as i64,
                    e.tokens_cached as i64,
                    e.cache_hit as i64,
                    e.task_class,
                    e.latency_ms as i64,
                    e.estimated_cost,
                    e.counterfactual_cost,
                    e.policy_decision.as_str(),
                    e.compression_ratio,
                    e.error_class,
                ])?;
            }
        }
        tx.commit()?;
        Ok(events.len())
    }

    /// Mark an agent as revoked. `device_id` `None` (or `"*"`) revokes it on
    /// every device that polls; a specific id scopes it to one device.
    pub fn revoke(&self, agent_id: &str, device_id: Option<&str>) -> Result<()> {
        let dev = device_id.unwrap_or("*");
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO revocations (device_id, agent_id, ts) VALUES (?1,?2,?3)",
            params![dev, agent_id, chrono::Utc::now().timestamp()],
        )?;
        Ok(())
    }

    /// Lift a revocation. Matches the same `device_id` scoping used to set it.
    pub fn unrevoke(&self, agent_id: &str, device_id: Option<&str>) -> Result<()> {
        let dev = device_id.unwrap_or("*");
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM revocations WHERE device_id = ?1 AND agent_id = ?2",
            params![dev, agent_id],
        )?;
        Ok(())
    }

    /// Agent ids revoked for a given device: its device-scoped entries plus any
    /// fleet-wide (`*`) ones. This is what the shim's poll loop reads.
    pub fn revoked_for(&self, device_id: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT agent_id FROM revocations WHERE device_id = ?1 OR device_id = '*'",
        )?;
        let rows = stmt.query_map(params![device_id], |r| r.get::<_, String>(0))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Every revocation, for the dashboard's remote-kill panel.
    pub fn list_revocations(&self) -> Result<Vec<Revocation>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT device_id, agent_id, ts FROM revocations ORDER BY ts DESC")?;
        let rows = stmt.query_map([], |r| {
            Ok(Revocation {
                device_id: r.get(0)?,
                agent_id: r.get(1)?,
                ts: r.get(2)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Aggregate all events on/after `since_ts` into a canonical summary.
    pub fn summary(&self, since_ts: i64) -> Result<Summary> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT agent_id, model, tokens_in, tokens_out, tokens_cached, cache_hit,
                    compression_ratio, policy_decision, estimated_cost, subject
             FROM events WHERE ts >= ?1",
        )?;
        let rows = stmt.query_map(params![since_ts], |r| {
            Ok(Row {
                agent: r.get(0)?,
                model: r.get(1)?,
                tokens_in: r.get::<_, i64>(2)? as u64,
                tokens_out: r.get::<_, i64>(3)? as u64,
                tokens_cached: r.get::<_, i64>(4)? as u64,
                cache_hit: r.get::<_, i64>(5)? != 0,
                compression_ratio: r.get(6)?,
                policy_decision: parse_decision(&r.get::<_, String>(7)?),
                reported_cost: r.get(8)?,
                subject: r.get::<_, Option<String>>(9)?,
            })
        })?;

        let mut summary = Summary::default();
        for row in rows {
            let row = row?;
            let (actual, counter) = canonical(
                &self.prices,
                &EventFacts {
                    model: &row.model,
                    tokens_in: row.tokens_in,
                    tokens_out: row.tokens_out,
                    tokens_cached: row.tokens_cached,
                    compression_ratio: row.compression_ratio,
                    cache_hit: row.cache_hit,
                    policy_decision: row.policy_decision,
                    reported_cost: row.reported_cost,
                },
            );
            // Re-split the same raw token fields into compression vs.
            // provider-prompt-cache savings, independent of `canonical`'s
            // per-decision actual-cost overrides (see `decompose_savings`
            // doc comment for why a cache-hit's own token fields already
            // zero this out correctly without needing a special case here).
            let breakdown = decompose_savings(
                &self.prices,
                &row.model,
                row.tokens_in,
                row.tokens_out,
                row.tokens_cached,
                row.compression_ratio,
            );
            let mut buckets: Vec<&mut Rollup> = vec![
                &mut summary.total,
                summary.by_agent.entry(row.agent.clone()).or_default(),
                summary.by_model.entry(row.model.clone()).or_default(),
            ];
            if let Some(subj) = row.subject.as_ref().filter(|s| !s.is_empty()) {
                buckets.push(summary.by_subject.entry(subj.clone()).or_default());
            }
            for bucket in buckets {
                bucket.requests += 1;
                if row.cache_hit {
                    bucket.cache_hits += 1;
                }
                bucket.tokens_in += row.tokens_in;
                bucket.tokens_out += row.tokens_out;
                bucket.actual_cost += actual;
                bucket.counterfactual_cost += counter;
                bucket.compression_savings += breakdown.compression_savings;
                bucket.provider_cache_savings += breakdown.provider_cache_savings;
            }
        }
        summary.finalize();
        Ok(summary)
    }
}

#[derive(Debug, Serialize)]
pub struct Revocation {
    pub device_id: String,
    pub agent_id: String,
    pub ts: i64,
}

struct Row {
    agent: String,
    model: String,
    tokens_in: u64,
    tokens_out: u64,
    tokens_cached: u64,
    cache_hit: bool,
    compression_ratio: f64,
    policy_decision: PolicyDecision,
    reported_cost: f64,
    subject: Option<String>,
}

/// The `events` table stores `policy_decision` as free text (see
/// `TelemetryEvent`/`PolicyDecision::as_str`); parse it back for the
/// canonical recompute. An unrecognized value (e.g. a newer shim's decision
/// variant talking to an older relay) degrades to `Allow` -- i.e. cost is
/// recomputed from tokens rather than silently trusted or silently zeroed.
fn parse_decision(s: &str) -> PolicyDecision {
    match s {
        "cache_hit" => PolicyDecision::CacheHit,
        "semantic_cache_hit" => PolicyDecision::SemanticCacheHit,
        "budget_blocked" => PolicyDecision::BudgetBlocked,
        "soft_cap_warn" => PolicyDecision::SoftCapWarn,
        "policy_blocked" => PolicyDecision::PolicyBlocked,
        _ => PolicyDecision::Allow,
    }
}

#[derive(Debug, Default, Serialize, Clone)]
pub struct Rollup {
    pub requests: u64,
    pub cache_hits: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub actual_cost: f64,
    pub counterfactual_cost: f64,
    pub savings: f64,
    /// Savings from compression + provider prompt-cache -- applies on every
    /// call, hit or not. See `halo_common::pricing::SavingsBreakdown`.
    pub compression_savings: f64,
    pub provider_cache_savings: f64,
    /// `savings` minus the two fields above: the remainder specifically
    /// attributable to a Halo cache hit never calling the provider.
    pub hit_savings: f64,
}

impl Rollup {
    fn finalize(&mut self) {
        self.savings = (self.counterfactual_cost - self.actual_cost).max(0.0);
        self.hit_savings = (self.savings - self.compression_savings - self.provider_cache_savings).max(0.0);
    }
}

#[derive(Debug, Default, Serialize)]
pub struct Summary {
    pub total: Rollup,
    #[serde(serialize_with = "map_to_vec")]
    pub by_agent: BTreeMap<String, Rollup>,
    #[serde(serialize_with = "map_to_vec")]
    pub by_model: BTreeMap<String, Rollup>,
    /// Per-subject (channel/sub-agent) rollup. Only populated for events that
    /// carried a subject; the relay handler may blank this out entirely when
    /// the relay license doesn't entitle `subject_attribution`.
    #[serde(serialize_with = "map_to_vec")]
    pub by_subject: BTreeMap<String, Rollup>,
}

impl Summary {
    fn finalize(&mut self) {
        self.total.finalize();
        for r in self.by_agent.values_mut() {
            r.finalize();
        }
        for r in self.by_model.values_mut() {
            r.finalize();
        }
        for r in self.by_subject.values_mut() {
            r.finalize();
        }
    }

    /// Drop the per-subject breakdown (used when the relay isn't entitled for
    /// `subject_attribution`, so the gated drill-down never reaches the wire).
    pub fn strip_subjects(&mut self) {
        self.by_subject.clear();
    }
}

/// Serialize a map as a sorted array of `{ name, ...rollup }` for the frontend.
fn map_to_vec<S>(map: &BTreeMap<String, Rollup>, s: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    use serde::ser::SerializeSeq;
    let mut seq = s.serialize_seq(Some(map.len()))?;
    for (name, r) in map {
        seq.serialize_element(&NamedRollup { name, r })?;
    }
    seq.end()
}

#[derive(Serialize)]
struct NamedRollup<'a> {
    name: &'a str,
    #[serde(flatten)]
    r: &'a Rollup,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn store() -> (TempDir, Store) {
        let tmp = TempDir::new().unwrap();
        let s = Store::open(tmp.path().join("t.db").to_str().unwrap()).unwrap();
        (tmp, s)
    }

    #[test]
    fn device_scoped_and_fleet_wide_revocations() {
        let (_t, s) = store();
        // Fleet-wide revocation is visible to every device.
        s.revoke("runaway", None).unwrap();
        assert!(s.revoked_for("dev_a").unwrap().contains(&"runaway".to_string()));
        assert!(s.revoked_for("dev_b").unwrap().contains(&"runaway".to_string()));

        // Device-scoped revocation is visible only to that device.
        s.revoke("scoped", Some("dev_a")).unwrap();
        assert!(s.revoked_for("dev_a").unwrap().contains(&"scoped".to_string()));
        assert!(!s.revoked_for("dev_b").unwrap().contains(&"scoped".to_string()));
    }

    #[test]
    fn unrevoke_lifts_the_kill() {
        let (_t, s) = store();
        s.revoke("agent", None).unwrap();
        assert!(!s.revoked_for("dev").unwrap().is_empty());
        s.unrevoke("agent", None).unwrap();
        assert!(s.revoked_for("dev").unwrap().is_empty());
    }

    #[test]
    fn list_revocations_reports_all() {
        let (_t, s) = store();
        s.revoke("a", None).unwrap();
        s.revoke("b", Some("dev_x")).unwrap();
        let list = s.list_revocations().unwrap();
        assert_eq!(list.len(), 2);
    }
}
