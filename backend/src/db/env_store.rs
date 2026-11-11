//! Per-project environment variables, keyed hierarchically:
//! `/{keyword}/{project}/{env}/{KEY}` → value (plain string).
//!
//! `env` 是用户在 ENV 面板创建的环境维度（dev/prod/自定义），与 session
//! 复合键的第 4 段（会话实例名）无关。所有段不得含 `/`，由 API 层校验。

use std::sync::Arc;

use anyhow::Result;
use redb::Database;

use super::ENVS;

pub struct EnvStore {
    db: Arc<Database>,
}

fn full_key(keyword: &str, project: &str, env: &str, key: &str) -> String {
    format!("/{}/{}/{}/{}", keyword, project, env, key)
}

/// Exclusive upper bound for a prefix range: bump the last byte ('/' → '0',
/// the next code point in byte order) so every key starting with `prefix`
/// sorts before the bound.
fn upper_bound(prefix: &str) -> String {
    let mut s = prefix.to_string();
    let last = s.pop().expect("prefix is non-empty");
    s.push(((last as u8) + 1) as char);
    s
}

impl EnvStore {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    pub fn set_var(&self, keyword: &str, project: &str, env: &str, key: &str, value: &str) -> Result<()> {
        let write = self.db.begin_write()?;
        {
            let mut table = write.open_table(ENVS)?;
            table.insert(full_key(keyword, project, env, key).as_str(), value)?;
        }
        write.commit()?;
        Ok(())
    }

    /// Returns false when the key did not exist.
    pub fn delete_var(&self, keyword: &str, project: &str, env: &str, key: &str) -> Result<bool> {
        let write = self.db.begin_write()?;
        let removed = {
            let mut table = write.open_table(ENVS)?;
            let existed = table.remove(full_key(keyword, project, env, key).as_str())?.is_some();
            existed
        };
        write.commit()?;
        Ok(removed)
    }

    /// All (key, value) pairs of one environment, sorted by key.
    pub fn list_vars(&self, keyword: &str, project: &str, env: &str) -> Result<Vec<(String, String)>> {
        let prefix = format!("/{}/{}/{}/", keyword, project, env);
        let read = self.db.begin_read()?;
        let table = read.open_table(ENVS)?;
        let mut out = Vec::new();
        for entry in table.range(prefix.as_str()..upper_bound(&prefix).as_str())? {
            let (k, v) = entry?;
            out.push((
                k.value()[prefix.len()..].to_string(),
                v.value().to_string(),
            ));
        }
        Ok(out)
    }

    /// Distinct environment names that hold at least one key, sorted.
    pub fn list_envs(&self, keyword: &str, project: &str) -> Result<Vec<String>> {
        let prefix = format!("/{}/{}/", keyword, project);
        let read = self.db.begin_read()?;
        let table = read.open_table(ENVS)?;
        let mut envs: Vec<String> = Vec::new();
        for entry in table.range(prefix.as_str()..upper_bound(&prefix).as_str())? {
            let (k, _) = entry?;
            let rest = &k.value()[prefix.len()..];
            let env = rest.split('/').next().unwrap_or("");
            if !env.is_empty() && envs.last().map(|e| e != env).unwrap_or(true) {
                envs.push(env.to_string());
            }
        }
        Ok(envs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(tag: &str) -> EnvStore {
        let path = std::env::temp_dir().join(format!(
            "env-store-test-{}-{}.redb",
            tag,
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let db = Database::create(&path).unwrap();
        let write = db.begin_write().unwrap();
        {
            let _ = write.open_table(ENVS).unwrap();
        }
        write.commit().unwrap();
        EnvStore::new(Arc::new(db))
    }

    #[test]
    fn set_list_roundtrip_sorted() {
        let store = temp_store("roundtrip");
        store.set_var("kw", "proj", "dev", "B_KEY", "2").unwrap();
        store.set_var("kw", "proj", "dev", "A_KEY", "1").unwrap();
        store.set_var("kw", "proj", "dev", "C_KEY", "3").unwrap();

        let vars = store.list_vars("kw", "proj", "dev").unwrap();
        assert_eq!(
            vars,
            vec![
                ("A_KEY".to_string(), "1".to_string()),
                ("B_KEY".to_string(), "2".to_string()),
                ("C_KEY".to_string(), "3".to_string()),
            ]
        );
    }

    #[test]
    fn list_envs_distinct_sorted() {
        let store = temp_store("envs");
        store.set_var("kw", "proj", "prod", "X", "1").unwrap();
        store.set_var("kw", "proj", "dev", "A", "1").unwrap();
        store.set_var("kw", "proj", "dev", "B", "2").unwrap();

        assert_eq!(store.list_envs("kw", "proj").unwrap(), vec!["dev", "prod"]);
        assert!(store.list_envs("kw", "other").unwrap().is_empty());
    }

    #[test]
    fn delete_var() {
        let store = temp_store("delete");
        store.set_var("kw", "proj", "dev", "A", "1").unwrap();
        assert!(store.delete_var("kw", "proj", "dev", "A").unwrap());
        assert!(!store.delete_var("kw", "proj", "dev", "A").unwrap());
        assert!(store.list_vars("kw", "proj", "dev").unwrap().is_empty());
    }

    #[test]
    fn prefix_isolation_between_projects_and_envs() {
        let store = temp_store("isolation");
        store.set_var("kw", "proj-a", "dev", "K", "a-dev").unwrap();
        store.set_var("kw", "proj-a", "prod", "K", "a-prod").unwrap();
        store.set_var("kw", "proj-b", "dev", "K", "b-dev").unwrap();

        assert_eq!(
            store.list_vars("kw", "proj-a", "dev").unwrap(),
            vec![("K".to_string(), "a-dev".to_string())]
        );
        assert_eq!(
            store.list_vars("kw", "proj-a", "prod").unwrap(),
            vec![("K".to_string(), "a-prod".to_string())]
        );
        assert_eq!(
            store.list_vars("kw", "proj-b", "dev").unwrap(),
            vec![("K".to_string(), "b-dev".to_string())]
        );
    }

    /// Keys whose bytes sit right at the range upper bound boundary
    /// (e.g. starting with '0', one past '/') must not be skipped or leak
    /// into neighboring prefixes.
    #[test]
    fn range_upper_bound_boundary() {
        let store = temp_store("boundary");
        // '0' sorts immediately after '/' — key "0AUTH" still belongs to env "dev"
        store.set_var("kw", "proj", "dev", "0AUTH", "x").unwrap();
        // A key containing '/' chars after the env segment stays whole
        store.set_var("kw", "proj", "dev", "NESTED/PATH", "y").unwrap();
        // env "dev0" must not leak into env "dev" range
        store.set_var("kw", "proj", "dev0", "LEAK", "z").unwrap();

        let vars = store.list_vars("kw", "proj", "dev").unwrap();
        assert_eq!(vars.len(), 2);
        assert!(vars.contains(&("0AUTH".to_string(), "x".to_string())));
        assert!(vars.contains(&("NESTED/PATH".to_string(), "y".to_string())));
        assert!(!vars.iter().any(|(k, _)| k == "LEAK"));

        assert_eq!(store.list_envs("kw", "proj").unwrap(), vec!["dev", "dev0"]);
    }
}
