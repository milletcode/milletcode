use std::sync::Arc;
use redb::{Database, ReadableTable};
use serde::{Deserialize, Serialize};

use super::PROJECT_SETTINGS;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectTerminalCommand {
    pub id: String,
    pub name: String,
    pub command: String,
}

pub struct ProjectStore {
    db: Arc<Database>,
}

impl ProjectStore {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    /// Get terminal commands for a project
    pub fn get_terminal_commands(&self, project_path: &str) -> Result<Vec<ProjectTerminalCommand>, redb::Error> {
        let read = self.db.begin_read()?;
        let table = read.open_table(PROJECT_SETTINGS)?;
        let key = format!("project/terminal/{}", project_path);

        if let Some(value) = table.get(key.as_str())? {
            Ok(serde_json::from_str(value.value()).unwrap_or_default())
        } else {
            Ok(Vec::new())
        }
    }

    /// Set terminal commands for a project
    pub fn set_terminal_commands(&self, project_path: &str, commands: &[ProjectTerminalCommand]) -> anyhow::Result<()> {
        let write = self.db.begin_write()?;
        {
            let mut table = write.open_table(PROJECT_SETTINGS)?;
            let key = format!("project/terminal/{}", project_path);
            let json = serde_json::to_string(commands)?;
            table.insert(key.as_str(), json.as_str())?;
        }
        write.commit()?;
        Ok(())
    }
}
