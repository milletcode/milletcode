use std::sync::Arc;

use anyhow::Result;
use redb::{Database, ReadableTable, ReadableTableMetadata, TableDefinition};
use serde::{Deserialize, Serialize};

use super::{APPROVALS, APPROVAL_HISTORY};

/// One human decision over an in-band control request (approval or
/// AskUserQuestion), persisted for audit. Keyed by (decided_at, request_id)
/// so the table is time-ordered — retention pruning is a range delete.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionRecord {
    pub request_id: String,
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    /// "approval" | "question"
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// approval: the parked prompt; question: the answers JSON
    pub summary: String,
    /// "approved" | "denied" | "answered"
    pub decision: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    /// when the request arrived (ms)
    pub created_at: u64,
    /// when the human decided (ms)
    pub decided_at: u64,
}

#[derive(Clone)]
pub struct ApprovalStore {
    db: Arc<Database>,
}

impl ApprovalStore {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    pub fn record(&self, rec: &DecisionRecord) -> Result<()> {
        let value = serde_json::to_string(rec)?;
        let write = self.db.begin_write()?;
        {
            let mut table = write.open_table(APPROVAL_HISTORY)?;
            table.insert((rec.decided_at, rec.request_id.as_str()), value.as_str())?;
        }
        write.commit()?;
        Ok(())
    }

    /// Newest-first decision history, at most `limit` entries.
    pub fn list(&self, limit: usize) -> Result<Vec<DecisionRecord>> {
        let read = self.db.begin_read()?;
        let table = read.open_table(APPROVAL_HISTORY)?;
        let mut out = Vec::new();
        for item in table.range((0u64, "")..)?.rev() {
            let (_, v) = item?;
            if let Ok(rec) = serde_json::from_str::<DecisionRecord>(v.value()) {
                out.push(rec);
                if out.len() >= limit {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// Keep only the newest `n` decisions. redb's table counter makes the
    /// length check O(1); the delete touches only the overflow (typically one
    /// record per new decision). Returns the number of records removed.
    pub fn prune_to_last_n(&self, n: usize) -> Result<usize> {
        let excess = {
            let read = self.db.begin_read()?;
            let table = read.open_table(APPROVAL_HISTORY)?;
            table.len()?.saturating_sub(n as u64) as usize
        };
        if excess == 0 {
            return Ok(0);
        }
        let write = self.db.begin_write()?;
        let mut removed = 0;
        {
            let mut table = write.open_table(APPROVAL_HISTORY)?;
            // Oldest-first: take exactly the overflow prefix.
            let stale: Vec<(u64, String)> = table
                .range((0u64, "")..)?
                .take(excess)
                .filter_map(|item| item.ok().map(|(k, _)| {
                    let (ts, id) = k.value();
                    (ts, id.to_string())
                }))
                .collect();
            for (ts, id) in stale {
                if table.remove((ts, id.as_str()))?.is_some() {
                    removed += 1;
                }
            }
        }
        write.commit()?;
        Ok(removed)
    }

    /// One-shot import of the pre-refactor `approvals` table (hook-era
    /// records keyed by UUID) into the time-ordered history table, then drop
    /// the legacy table. Idempotent: a missing table is a no-op.
    pub fn migrate_legacy(&self) -> Result<usize> {
        let read = self.db.begin_read()?;
        let legacy = match read.open_table(APPROVALS) {
            Ok(t) => t,
            Err(_) => return Ok(0),
        };
        let mut imported = 0;
        let write = self.db.begin_write()?;
        {
            let mut table = write.open_table(APPROVAL_HISTORY)?;
            for item in legacy.iter()? {
                let (k, v) = item?;
                let raw = serde_json::from_str::<serde_json::Value>(v.value());
                let Ok(raw) = raw else { continue };
                // Dead hook-era records that never got a decision are not
                // decisions — leave them out of the audit trail.
                let decision = raw.get("decision").and_then(|d| d.as_str()).unwrap_or("");
                if decision == "pending" {
                    continue;
                }
                let id = raw.get("id").and_then(|i| i.as_str()).unwrap_or_default().to_string();
                let created_secs = raw.get("created_at").and_then(|t| t.as_u64()).unwrap_or(0);
                let decided_secs = raw.get("decided_at").and_then(|t| t.as_u64()).unwrap_or(created_secs);
                let rec = DecisionRecord {
                    request_id: id.clone(),
                    session_id: raw.get("session_id").and_then(|s| s.as_str()).unwrap_or_default().to_string(),
                    project_name: raw.get("project_name").and_then(|s| s.as_str()).map(str::to_string),
                    agent_name: None,
                    kind: "approval".to_string(),
                    tool_name: raw.get("tool_name").and_then(|s| s.as_str()).map(str::to_string),
                    summary: raw.get("prompt").and_then(|s| s.as_str()).unwrap_or_default().to_string(),
                    decision: decision.to_string(),
                    comment: None,
                    created_at: created_secs.saturating_mul(1000),
                    decided_at: decided_secs.saturating_mul(1000),
                };
                let value = serde_json::to_string(&rec)?;
                table.insert((rec.decided_at, id.as_str()), value.as_str())?;
                imported += 1;
            }
            write.delete_table(APPROVALS)?;
        }
        write.commit()?;
        Ok(imported)
    }

    /// Enforce the configured retention: keep the last `count` decisions,
    /// 0 = forever. Called opportunistically after each record.
    /// Returns the number of records removed.
    pub fn enforce_retention(&self, count: u32) -> Result<usize> {
        if count == 0 {
            return Ok(0);
        }
        self.prune_to_last_n(count as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(tag: &str) -> ApprovalStore {
        let path = std::env::temp_dir().join(format!(
            "approval-store-test-{}-{}.redb",
            tag,
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let db = Database::create(&path).unwrap();
        // create the history table the way open_approval_db does
        let write = db.begin_write().unwrap();
        {
            let _ = write.open_table(APPROVAL_HISTORY).unwrap();
        }
        write.commit().unwrap();
        ApprovalStore::new(Arc::new(db))
    }

    fn rec(request_id: &str, decided_at: u64, decision: &str) -> DecisionRecord {
        DecisionRecord {
            request_id: request_id.to_string(),
            session_id: "org/proj/claude/s1".to_string(),
            project_name: Some("proj".to_string()),
            agent_name: Some("claude".to_string()),
            kind: "approval".to_string(),
            tool_name: Some("Bash".to_string()),
            summary: "rm -rf /tmp/x".to_string(),
            decision: decision.to_string(),
            comment: None,
            created_at: decided_at - 1000,
            decided_at,
        }
    }

    #[test]
    fn list_is_newest_first_with_limit() {
        let store = temp_store("list");
        store.record(&rec("a", 1000, "approved")).unwrap();
        store.record(&rec("b", 3000, "denied")).unwrap();
        store.record(&rec("c", 2000, "approved")).unwrap();

        let all = store.list(100).unwrap();
        assert_eq!(
            all.iter().map(|r| r.request_id.as_str()).collect::<Vec<_>>(),
            vec!["b", "c", "a"]
        );
        assert_eq!(store.list(2).unwrap().len(), 2);
    }

    #[test]
    fn same_timestamp_distinct_ids_both_kept() {
        let store = temp_store("tie");
        store.record(&rec("a", 5000, "approved")).unwrap();
        store.record(&rec("b", 5000, "denied")).unwrap();
        assert_eq!(store.list(100).unwrap().len(), 2);
    }

    #[test]
    fn prune_keeps_newest_n() {
        let store = temp_store("prune");
        store.record(&rec("a", 1000, "approved")).unwrap();
        store.record(&rec("b", 2000, "approved")).unwrap();
        store.record(&rec("c", 3000, "approved")).unwrap();

        let removed = store.prune_to_last_n(2).unwrap();
        assert_eq!(removed, 1);
        let rest = store.list(100).unwrap();
        assert_eq!(
            rest.iter().map(|r| r.request_id.as_str()).collect::<Vec<_>>(),
            vec!["c", "b"]
        );
        // already at the cap — no-op
        assert_eq!(store.prune_to_last_n(2).unwrap(), 0);
        // cap above current size — no-op
        assert_eq!(store.prune_to_last_n(50).unwrap(), 0);
    }

    #[test]
    fn retention_zero_keeps_everything() {
        let store = temp_store("keep");
        store.record(&rec("a", 1000, "approved")).unwrap();
        assert_eq!(store.enforce_retention(0).unwrap(), 0);
        assert_eq!(store.list(100).unwrap().len(), 1);
    }

    #[test]
    fn legacy_records_are_imported_and_dropped() {
        let store = temp_store("migrate");
        let db = store.db.clone();
        let legacy_json = |id: &str, decision: &str, decided: u64| {
            format!(
                r#"{{"id":"{id}","session_id":"s","tool_name":"Bash","prompt":"rm x","decision":"{decision}","created_at":10,"decided_at":{decided}}}"#
            )
        };
        let write = db.begin_write().unwrap();
        {
            let mut legacy = write.open_table(APPROVALS).unwrap();
            legacy.insert("p1", legacy_json("p1", "pending", 11).as_str()).unwrap();
            legacy.insert("ok1", legacy_json("ok1", "approved", 12).as_str()).unwrap();
            legacy.insert("no1", legacy_json("no1", "denied", 13).as_str()).unwrap();
        }
        write.commit().unwrap();

        let imported = store.migrate_legacy().unwrap();
        assert_eq!(imported, 2); // pending skipped
        let all = store.list(100).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].request_id, "no1"); // decided_at 13s > 12s
        assert_eq!(all[0].decided_at, 13_000);
        assert_eq!(all[1].decision, "approved");

        // second run: legacy table gone, no-op
        assert_eq!(store.migrate_legacy().unwrap(), 0);
    }
}
