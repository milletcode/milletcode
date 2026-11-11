use std::sync::Arc;

use anyhow::Result;
use redb::Database;

use super::SETTINGS;

#[derive(Clone)]
pub struct SettingsStore {
    db: Arc<Database>,
}

impl SettingsStore {
    pub(crate) fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    pub fn get(&self, key: &str) -> Result<Option<String>> {
        let read = self.db.begin_read()?;
        let table = read.open_table(SETTINGS)?;
        match table.get(key)? {
            Some(v) => Ok(Some(v.value().to_string())),
            None => Ok(None),
        }
    }

    pub fn set(&self, key: &str, value: &str) -> Result<()> {
        let write = self.db.begin_write()?;
        {
            let mut table = write.open_table(SETTINGS)?;
            table.insert(key, value)?;
        }
        write.commit()?;
        Ok(())
    }

    pub fn delete(&self, key: &str) -> Result<bool> {
        let write = self.db.begin_write()?;
        let removed;
        {
            let mut table = write.open_table(SETTINGS)?;
            removed = table.remove(key)?.is_some();
        }
        write.commit()?;
        Ok(removed)
    }

    pub fn list(&self, prefix: &str) -> Result<Vec<(String, String)>> {
        let read = self.db.begin_read()?;
        let table = read.open_table(SETTINGS)?;
        let mut results = Vec::new();
        let end = prefix_end(prefix);
        let range = table.range::<&str>(prefix..&end)?;
        for item in range {
            let (k, v) = item?;
            results.push((k.value().to_string(), v.value().to_string()));
        }
        Ok(results)
    }
}

/// Exclusive upper bound for a prefix range: increment the last byte
/// ("agents." → "agents/"). Appending '\0' would sort BEFORE every real
/// continuation and yield an empty range.
fn prefix_end(prefix: &str) -> String {
    let mut end = prefix.to_string();
    if let Some(last) = end.pop() {
        end.push((last as u8).wrapping_add(1) as char);
    }
    end
}
