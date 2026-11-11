//! Per-project 需求接收 (git issue intake) config, keyed `/{keyword}/{project}`
//! → JSON `{url, token}`. Consumed by /git issue via spawn-time env injection;
//! takes priority over the system-level `[git.issue]` in config.toml.

use std::sync::Arc;

use anyhow::Result;
use redb::Database;

use crate::config::GitIssueConfig;

use super::ISSUE_CONFIG;

#[derive(Clone)]
pub struct IssueStore {
    db: Arc<Database>,
}

fn full_key(keyword: &str, project: &str) -> String {
    format!("/{}/{}", keyword, project)
}

impl IssueStore {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    pub fn get(&self, keyword: &str, project: &str) -> Result<Option<GitIssueConfig>> {
        let read = self.db.begin_read()?;
        let table = read.open_table(ISSUE_CONFIG)?;
        match table.get(full_key(keyword, project).as_str())? {
            Some(v) => Ok(Some(serde_json::from_str(v.value()).unwrap_or_default())),
            None => Ok(None),
        }
    }

    pub fn set(&self, keyword: &str, project: &str, cfg: &GitIssueConfig) -> Result<()> {
        let write = self.db.begin_write()?;
        {
            let mut table = write.open_table(ISSUE_CONFIG)?;
            let json = serde_json::to_string(cfg)?;
            table.insert(full_key(keyword, project).as_str(), json.as_str())?;
        }
        write.commit()?;
        Ok(())
    }

    /// Returns false when no config was stored for the project.
    pub fn clear(&self, keyword: &str, project: &str) -> Result<bool> {
        let write = self.db.begin_write()?;
        let removed = {
            let mut table = write.open_table(ISSUE_CONFIG)?;
            let existed = table.remove(full_key(keyword, project).as_str())?.is_some();
            existed
        };
        write.commit()?;
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(tag: &str) -> IssueStore {
        let path = std::env::temp_dir().join(format!(
            "issue-store-test-{}-{}.redb",
            tag,
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let db = Database::create(&path).unwrap();
        let write = db.begin_write().unwrap();
        {
            let _ = write.open_table(ISSUE_CONFIG).unwrap();
        }
        write.commit().unwrap();
        IssueStore::new(Arc::new(db))
    }

    #[test]
    fn set_get_roundtrip() {
        let store = temp_store("roundtrip");
        let cfg = GitIssueConfig {
            url: "https://git.example.com/api/v1".into(),
            token: "secret-token".into(),
        };
        store.set("kw", "proj", &cfg).unwrap();
        assert_eq!(store.get("kw", "proj").unwrap(), Some(cfg));
        assert_eq!(store.get("kw", "other").unwrap(), None);
    }

    #[test]
    fn clear_removes_config() {
        let store = temp_store("clear");
        store
            .set("kw", "proj", &GitIssueConfig {
                url: "u".into(),
                token: "t".into(),
            })
            .unwrap();
        assert!(store.clear("kw", "proj").unwrap());
        assert!(!store.clear("kw", "proj").unwrap());
        assert_eq!(store.get("kw", "proj").unwrap(), None);
    }

    #[test]
    fn project_isolation() {
        let store = temp_store("isolation");
        store
            .set("kw-a", "proj", &GitIssueConfig { url: "a".into(), token: "a".into() })
            .unwrap();
        store
            .set("kw-b", "proj", &GitIssueConfig { url: "b".into(), token: "b".into() })
            .unwrap();
        assert_eq!(store.get("kw-a", "proj").unwrap().unwrap().url, "a");
        assert_eq!(store.get("kw-b", "proj").unwrap().unwrap().url, "b");
    }
}
