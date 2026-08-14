//! Local budget ledger and kill switch (free-tier priority #1).
//!
//! Append-only spend ledger in redb (embedded, single-file, pure Rust). Two
//! thresholds per scope (global and per-agent):
//!   * soft cap -- warn, keep serving.
//!   * hard cap -- refuse the request, enforced locally, ALWAYS. The kill
//!     switch must work even if the relay has never been reachable, so it
//!     depends on nothing but this local file.
//!
//! Enforcement is pre-flight: we check `spent_in_window + projected_cost`
//! against the caps *before* forwarding, so a single request can't blow past a
//! hard cap.

use anyhow::Result;
use redb::{Database, DatabaseError, ReadableTable, ReadableTableMetadata, TableDefinition};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;

const LEDGER: TableDefinition<&str, &[u8]> = TableDefinition::new("ledger");
/// Prune stale entries roughly every this-many writes to bound scan cost.
const PRUNE_EVERY: u64 = 128;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Spend {
    agent: String,
    cost: f64,
    ts_millis: i64,
}

/// The verdict of a pre-flight budget check.
#[derive(Debug, Clone, PartialEq)]
pub enum BudgetVerdict {
    Allow,
    SoftWarn { scope: String, spent: f64, cap: f64 },
    HardBlock { scope: String, spent: f64, cap: f64 },
}

/// Caps for one check, resolved by the caller from config.
#[derive(Debug, Clone, Copy, Default)]
pub struct Caps {
    pub global_soft: Option<f64>,
    pub global_hard: Option<f64>,
    pub agent_soft: Option<f64>,
    pub agent_hard: Option<f64>,
}

pub struct Ledger {
    db: Database,
    window_millis: i64,
}

impl Ledger {
    pub fn open(path: &Path, window_hours: u64) -> Result<Arc<Self>> {
        Self::try_open(path, window_hours)?
            .ok_or_else(|| anyhow::anyhow!("Database already open. Cannot acquire lock."))
    }

    /// Like [`open`], but `Ok(None)` when another process (usually `halo serve`)
    /// already holds the file. CLI reads (`halo status`) use this instead of
    /// dying on the exclusive lock.
    pub fn try_open(path: &Path, window_hours: u64) -> Result<Option<Arc<Self>>> {
        let db = match Database::create(path) {
            Ok(db) => db,
            Err(DatabaseError::DatabaseAlreadyOpen) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        {
            let w = db.begin_write()?;
            {
                let _ = w.open_table(LEDGER)?;
            }
            w.commit()?;
        }
        Ok(Some(Arc::new(Self {
            db,
            window_millis: (window_hours as i64) * 3_600_000,
        })))
    }

    /// Record actual spend after a call completes.
    pub fn record(&self, agent: &str, cost: f64) -> Result<()> {
        let now = chrono::Utc::now().timestamp_millis();
        let key = format!("{now:013}:{}", uuid::Uuid::new_v4().simple());
        let entry = Spend {
            agent: agent.to_string(),
            cost,
            ts_millis: now,
        };
        let bytes = serde_json::to_vec(&entry)?;
        let wtxn = self.db.begin_write()?;
        let should_prune;
        {
            let mut t = wtxn.open_table(LEDGER)?;
            t.insert(key.as_str(), bytes.as_slice())?;
            should_prune = t.len()? % PRUNE_EVERY == 0;
        }
        wtxn.commit()?;
        if should_prune {
            let _ = self.prune();
        }
        Ok(())
    }

    /// Pre-flight check: would `projected_cost` breach any cap?
    pub fn check(&self, agent: &str, projected_cost: f64, caps: Caps) -> Result<BudgetVerdict> {
        let (global, per_agent) = self.spend(agent)?;

        let checks = [
            ("global", global, caps.global_soft, caps.global_hard),
            ("agent", per_agent, caps.agent_soft, caps.agent_hard),
        ];

        let mut soft: Option<BudgetVerdict> = None;
        for (scope, spent, s, h) in checks {
            let projected = spent + projected_cost;
            if let Some(hard) = h {
                if projected > hard {
                    return Ok(BudgetVerdict::HardBlock {
                        scope: scope.to_string(),
                        spent,
                        cap: hard,
                    });
                }
            }
            if let Some(sc) = s {
                if projected > sc && soft.is_none() {
                    soft = Some(BudgetVerdict::SoftWarn {
                        scope: scope.to_string(),
                        spent,
                        cap: sc,
                    });
                }
            }
        }
        Ok(soft.unwrap_or(BudgetVerdict::Allow))
    }

    /// (global, agent) spend within the rolling window.
    pub fn spend(&self, agent: &str) -> Result<(f64, f64)> {
        let cutoff = chrono::Utc::now().timestamp_millis() - self.window_millis;
        let rtxn = self.db.begin_read()?;
        let t = rtxn.open_table(LEDGER)?;
        let mut global = 0.0;
        let mut per_agent = 0.0;
        for row in t.iter()? {
            let (_, v) = row?;
            if let Ok(s) = serde_json::from_slice::<Spend>(v.value()) {
                if s.ts_millis >= cutoff {
                    global += s.cost;
                    if s.agent == agent {
                        per_agent += s.cost;
                    }
                }
            }
        }
        Ok((global, per_agent))
    }

    /// Total spend within the window grouped by agent (for `halo status`).
    pub fn spend_by_agent(&self) -> Result<Vec<(String, f64)>> {
        let cutoff = chrono::Utc::now().timestamp_millis() - self.window_millis;
        let rtxn = self.db.begin_read()?;
        let t = rtxn.open_table(LEDGER)?;
        let mut map: std::collections::BTreeMap<String, f64> = std::collections::BTreeMap::new();
        for row in t.iter()? {
            let (_, v) = row?;
            if let Ok(s) = serde_json::from_slice::<Spend>(v.value()) {
                if s.ts_millis >= cutoff {
                    *map.entry(s.agent).or_insert(0.0) += s.cost;
                }
            }
        }
        Ok(map.into_iter().collect())
    }

    fn prune(&self) -> Result<()> {
        let cutoff = chrono::Utc::now().timestamp_millis() - self.window_millis;
        let wtxn = self.db.begin_write()?;
        {
            let mut t = wtxn.open_table(LEDGER)?;
            let mut stale: Vec<String> = Vec::new();
            for row in t.iter()? {
                let (k, v) = row?;
                if let Ok(s) = serde_json::from_slice::<Spend>(v.value()) {
                    if s.ts_millis < cutoff {
                        stale.push(k.value().to_string());
                    }
                }
            }
            for k in stale {
                t.remove(k.as_str())?;
            }
        }
        wtxn.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn hard_cap_blocks_preflight() {
        let tmp = TempDir::new().unwrap();
        let l = Ledger::open(&tmp.path().join("l.redb"), 24).unwrap();
        l.record("a", 9.0).unwrap();
        let caps = Caps {
            global_hard: Some(10.0),
            ..Default::default()
        };
        // projected 2.0 pushes 9+2=11 > 10 -> block.
        assert!(matches!(
            l.check("a", 2.0, caps).unwrap(),
            BudgetVerdict::HardBlock { .. }
        ));
        // projected 0.5 -> 9.5 <= 10 -> allow.
        assert_eq!(l.check("a", 0.5, caps).unwrap(), BudgetVerdict::Allow);
    }

    #[test]
    fn soft_cap_warns_but_allows() {
        let tmp = TempDir::new().unwrap();
        let l = Ledger::open(&tmp.path().join("l.redb"), 24).unwrap();
        l.record("a", 5.0).unwrap();
        let caps = Caps {
            agent_soft: Some(4.0),
            ..Default::default()
        };
        assert!(matches!(
            l.check("a", 0.0, caps).unwrap(),
            BudgetVerdict::SoftWarn { .. }
        ));
    }

    #[test]
    fn per_agent_isolated() {
        let tmp = TempDir::new().unwrap();
        let l = Ledger::open(&tmp.path().join("l.redb"), 24).unwrap();
        l.record("a", 5.0).unwrap();
        l.record("b", 1.0).unwrap();
        let (global, a) = l.spend("a").unwrap();
        assert_eq!(global, 6.0);
        assert_eq!(a, 5.0);
    }

    #[test]
    fn try_open_none_while_held() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("l.redb");
        let _held = Ledger::open(&path, 24).unwrap();
        assert!(Ledger::try_open(&path, 24).unwrap().is_none());
    }
}
