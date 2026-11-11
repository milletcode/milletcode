use std::sync::Arc;

use anyhow::Result;
use redb::{Database, ReadableTable, ReadableTableMetadata};
use serde::{Deserialize, Serialize};

use super::SKILLS_HISTORY;

/// One install session fanned out to a single (node, agent) pair.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillInstallSession {
    pub node: String,
    pub agent_id: String,
    pub session_key: String,
    /// "running" | "complete" | "error"
    pub status: String,
    /// 验收失败的补充说明（已脱敏），成功/运行中为空
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// One third-party skill install run: a prompt fanned out to install
/// sessions across every node × present agent. Keyed by (created_at, id)
/// so the table is time-ordered — retention pruning is a range delete.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillInstallRecord {
    pub id: String,
    pub prompt: String,
    pub created_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<u64>,
    /// node 数(去重) × 实际存在的 agent 种类
    pub agent_total: usize,
    pub sessions: Vec<SkillInstallSession>,
    /// "running" | "success" | "partial" | "failed"
    pub status: String,
}

#[derive(Clone)]
pub struct SkillsStore {
    db: Arc<Database>,
}

impl SkillsStore {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    pub fn record(&self, rec: &SkillInstallRecord) -> Result<()> {
        let value = serde_json::to_string(rec)?;
        let write = self.db.begin_write()?;
        {
            let mut table = write.open_table(SKILLS_HISTORY)?;
            table.insert((rec.created_at, rec.id.as_str()), value.as_str())?;
        }
        write.commit()?;
        Ok(())
    }

    pub fn get(&self, id: &str) -> Result<Option<SkillInstallRecord>> {
        let read = self.db.begin_read()?;
        let table = read.open_table(SKILLS_HISTORY)?;
        for item in table.range((0u64, "")..)?.rev() {
            let (_, v) = item?;
            if let Ok(rec) = serde_json::from_str::<SkillInstallRecord>(v.value()) {
                if rec.id == id {
                    return Ok(Some(rec));
                }
            }
        }
        Ok(None)
    }

    pub fn update(&self, rec: &SkillInstallRecord) -> Result<()> {
        self.record(rec)
    }

    /// Remove a record by id. Returns whether a row was deleted.
    pub fn delete(&self, id: &str) -> Result<bool> {
        // Key 是 (created_at, id)，按 id 找得扫一遍值；表有保留上限，规模很小。
        let key: Option<(u64, String)> = {
            let read = self.db.begin_read()?;
            let table = read.open_table(SKILLS_HISTORY)?;
            table
                .range((0u64, "")..)?
                .filter_map(|item| item.ok())
                .find(|(_, v)| {
                    serde_json::from_str::<SkillInstallRecord>(v.value())
                        .map(|rec| rec.id == id)
                        .unwrap_or(false)
                })
                .map(|(k, _)| {
                    let (ts, id) = k.value();
                    (ts, id.to_string())
                })
        };
        let Some((ts, id)) = key else {
            return Ok(false);
        };
        let write = self.db.begin_write()?;
        {
            let mut table = write.open_table(SKILLS_HISTORY)?;
            table.remove((ts, id.as_str()))?;
        }
        write.commit()?;
        Ok(true)
    }

    /// Newest-first page of install runs; returns entries plus whether more
    /// pages exist after this one.
    pub fn list(&self, offset: usize, limit: usize) -> Result<(Vec<SkillInstallRecord>, bool)> {
        let read = self.db.begin_read()?;
        let table = read.open_table(SKILLS_HISTORY)?;
        let mut out = Vec::new();
        let mut skipped = 0usize;
        for item in table.range((0u64, "")..)?.rev() {
            let (_, v) = item?;
            let Ok(rec) = serde_json::from_str::<SkillInstallRecord>(v.value()) else {
                continue;
            };
            if skipped < offset {
                skipped += 1;
                continue;
            }
            if out.len() < limit {
                out.push(rec);
            }
        }
        let total = table.len()? as usize;
        let has_more = offset + out.len() < total;
        Ok((out, has_more))
    }

    /// Keep only the newest `n` records. Returns the number removed.
    pub fn prune_to_last_n(&self, n: usize) -> Result<usize> {
        let excess = {
            let read = self.db.begin_read()?;
            let table = read.open_table(SKILLS_HISTORY)?;
            table.len()?.saturating_sub(n as u64) as usize
        };
        if excess == 0 {
            return Ok(0);
        }
        let write = self.db.begin_write()?;
        let mut removed = 0;
        {
            let mut table = write.open_table(SKILLS_HISTORY)?;
            let stale: Vec<(u64, String)> = table
                .range((0u64, "")..)?
                .take(excess)
                .filter_map(|item| {
                    item.ok().map(|(k, _)| {
                        let (ts, id) = k.value();
                        (ts, id.to_string())
                    })
                })
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

    /// Enforce configured retention; 0 = forever.
    pub fn enforce_retention(&self, count: u32) -> Result<usize> {
        if count == 0 {
            return Ok(0);
        }
        self.prune_to_last_n(count as usize)
    }

    /// Startup sweep: install runs left "running" by a process restart have no
    /// watcher anymore — mark their unfinished sessions and the run as failed.
    /// Returns the number of records finalized.
    pub fn finalize_interrupted_runs(&self) -> Result<usize> {
        let stale: Vec<SkillInstallRecord> = self
            .list(0, usize::MAX)?
            .0
            .into_iter()
            .filter(|r| r.status == "running")
            .collect();
        let mut finalized = 0;
        for mut rec in stale {
            for s in &mut rec.sessions {
                if s.status == "running" {
                    s.status = "error".to_string();
                }
            }
            rec.status = "failed".to_string();
            rec.finished_at = Some(chrono::Utc::now().timestamp_millis().max(0) as u64);
            self.update(&rec)?;
            finalized += 1;
        }
        Ok(finalized)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(tag: &str) -> SkillsStore {
        let path = std::env::temp_dir().join(format!(
            "skills-store-test-{}-{}.redb",
            tag,
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let db = Database::create(&path).unwrap();
        let write = db.begin_write().unwrap();
        {
            let _ = write.open_table(SKILLS_HISTORY).unwrap();
        }
        write.commit().unwrap();
        SkillsStore::new(Arc::new(db))
    }

    fn rec(id: &str, created_at: u64, status: &str) -> SkillInstallRecord {
        SkillInstallRecord {
            id: id.to_string(),
            prompt: "install this skill".to_string(),
            created_at,
            finished_at: None,
            agent_total: 2,
            sessions: vec![SkillInstallSession {
                node: "local".to_string(),
                agent_id: "claude".to_string(),
                session_key: format!("skills/installer/claude/{id}"),
                status: status.to_string(),
                note: None,
            }],
            status: "running".to_string(),
        }
    }

    #[test]
    fn record_get_update_roundtrip() {
        let store = temp_store("roundtrip");
        store.record(&rec("a", 1000, "running")).unwrap();
        assert!(store.get("a").unwrap().is_some());
        assert!(store.get("zz").unwrap().is_none());

        let mut r = store.get("a").unwrap().unwrap();
        r.status = "success".to_string();
        r.finished_at = Some(2000);
        store.update(&r).unwrap();

        let got = store.get("a").unwrap().unwrap();
        assert_eq!(got.status, "success");
        assert_eq!(got.finished_at, Some(2000));
    }

    #[test]
    fn list_is_newest_first_paged() {
        let store = temp_store("list");
        store.record(&rec("a", 1000, "running")).unwrap();
        store.record(&rec("b", 3000, "running")).unwrap();
        store.record(&rec("c", 2000, "running")).unwrap();

        let (page1, more1) = store.list(0, 2).unwrap();
        assert_eq!(
            page1.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["b", "c"]
        );
        assert!(more1);

        let (page2, more2) = store.list(2, 2).unwrap();
        assert_eq!(page2.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), vec!["a"]);
        assert!(!more2);

        assert_eq!(store.list(10, 2).unwrap().0.len(), 0);
    }

    #[test]
    fn prune_keeps_newest_n() {
        let store = temp_store("prune");
        store.record(&rec("a", 1000, "running")).unwrap();
        store.record(&rec("b", 2000, "running")).unwrap();
        store.record(&rec("c", 3000, "running")).unwrap();

        assert_eq!(store.prune_to_last_n(2).unwrap(), 1);
        let all = store.list(0, 100).unwrap().0;
        assert_eq!(all.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), vec!["c", "b"]);
        assert_eq!(store.prune_to_last_n(2).unwrap(), 0);
        assert_eq!(store.prune_to_last_n(50).unwrap(), 0);
    }

    #[test]
    fn retention_zero_keeps_everything() {
        let store = temp_store("keep");
        store.record(&rec("a", 1000, "running")).unwrap();
        assert_eq!(store.enforce_retention(0).unwrap(), 0);
        assert_eq!(store.list(0, 100).unwrap().0.len(), 1);
    }

    #[test]
    fn delete_removes_only_target_id() {
        let store = temp_store("delete");
        store.record(&rec("a", 1000, "running")).unwrap();
        store.record(&rec("b", 2000, "running")).unwrap();

        assert!(!store.delete("zz").unwrap());
        assert!(store.delete("a").unwrap());
        assert!(store.get("a").unwrap().is_none());
        assert!(store.get("b").unwrap().is_some());
        assert_eq!(store.list(0, 100).unwrap().0.len(), 1);
        assert!(!store.delete("a").unwrap());
    }

    #[test]
    fn interrupted_running_runs_are_swept_at_startup() {
        let store = temp_store("sweep");
        store.record(&rec("running1", 1000, "running")).unwrap();
        let mut done = rec("done", 2000, "running");
        done.status = "success".to_string();
        store.record(&done).unwrap();

        assert_eq!(store.finalize_interrupted_runs().unwrap(), 1);
        let swept = store.get("running1").unwrap().unwrap();
        assert_eq!(swept.status, "failed");
        assert!(swept.finished_at.is_some());
        assert!(swept.sessions.iter().all(|s| s.status == "error"));
        // second run: nothing left running
        assert_eq!(store.finalize_interrupted_runs().unwrap(), 0);
        // completed record untouched
        assert_eq!(store.get("done").unwrap().unwrap().status, "success");
    }
}
