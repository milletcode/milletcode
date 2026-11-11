use std::sync::Arc;
use redb::{Database, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Custom commands table: key = UUID, value = CustomCommand (JSON)
const COMMANDS: TableDefinition<&str, &str> = TableDefinition::new("commands");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomCommand {
    pub id: String,
    pub title: String,
    pub command: String,
    #[serde(default = "default_type")]
    pub r#type: String, // "default" or "git"
}

fn default_type() -> String {
    "default".to_string()
}

pub struct CommandsStore {
    db: Arc<Database>,
}

impl CommandsStore {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    /// Get all custom commands
    pub fn list(&self) -> Result<Vec<CustomCommand>, redb::Error> {
        let read = self.db.begin_read()?;
        let table = read.open_table(COMMANDS)?;
        let mut commands = Vec::new();

        for result in table.iter()? {
            let (_, value) = result?;
            if let Ok(cmd) = serde_json::from_str::<CustomCommand>(value.value()) {
                commands.push(cmd);
            }
        }

        // Sort by type (default first, then git), then by title
        commands.sort_by(|a, b| {
            let type_order = |t: &str| if t == "git" { 1 } else { 0 };
            type_order(&a.r#type)
                .cmp(&type_order(&b.r#type))
                .then_with(|| a.title.cmp(&b.title))
        });

        Ok(commands)
    }

    /// Get a single command by ID
    pub fn get(&self, id: &str) -> Result<Option<CustomCommand>, redb::Error> {
        let read = self.db.begin_read()?;
        let table = read.open_table(COMMANDS)?;
        if let Some(value) = table.get(id)? {
            Ok(serde_json::from_str(value.value()).ok())
        } else {
            Ok(None)
        }
    }

    /// Create or update a command
    pub fn set(&self, cmd: &CustomCommand) -> anyhow::Result<()> {
        let write = self.db.begin_write()?;
        {
            let mut table = write.open_table(COMMANDS)?;
            let json = serde_json::to_string(cmd)?;
            table.insert(cmd.id.as_str(), json.as_str())?;
        }
        write.commit()?;
        Ok(())
    }

    /// Delete a command
    pub fn delete(&self, id: &str) -> Result<(), redb::Error> {
        let write = self.db.begin_write()?;
        {
            let mut table = write.open_table(COMMANDS)?;
            table.remove(id)?;
        }
        write.commit()?;
        Ok(())
    }

    /// Clear all commands and reset to defaults
    pub fn reset_to_defaults(&self) -> anyhow::Result<()> {
        tracing::info!("Resetting commands to defaults...");
        let write = self.db.begin_write()?;
        {
            let mut table = write.open_table(COMMANDS)?;

            // Clear existing commands - collect keys first to avoid iterator issues
            let keys: Vec<String> = table
                .iter()?
                .filter_map(|r| r.ok())
                .map(|(k, _)| k.value().to_string())
                .collect();

            tracing::info!("Removing {} existing commands", keys.len());
            for key in keys {
                if let Err(e) = table.remove(key.as_str()) {
                    tracing::warn!("Failed to remove key {}: {}", key, e);
                }
            }

            // Insert default commands
            let defaults = Self::default_commands();
            tracing::info!("Inserting {} default commands", defaults.len());
            for (i, cmd) in defaults.iter().enumerate() {
                match serde_json::to_string(&cmd) {
                    Ok(json) => {
                        if let Err(e) = table.insert(cmd.id.as_str(), json.as_str()) {
                            tracing::error!("Failed to insert command {}: {}", cmd.title, e);
                        } else {
                            tracing::debug!("Inserted command {}/{}: {}", i + 1, defaults.len(), cmd.title);
                        }
                    }
                    Err(e) => {
                        tracing::error!("Failed to serialize command {}: {}", cmd.title, e);
                    }
                }
            }
        }
        write.commit()?;
        tracing::info!("Commands reset to defaults completed");
        Ok(())
    }

    /// Initialize with default commands if empty, and backfill defaults that
    /// are missing by title so existing installs receive newly added ones
    /// (deleting a default command resurrects it on restart — edit instead).
    pub fn init_defaults_if_empty(&self) -> anyhow::Result<()> {
        let commands = self.list()?;
        if commands.is_empty() {
            self.reset_to_defaults()?;
            return Ok(());
        }
        let titles: std::collections::HashSet<&str> =
            commands.iter().map(|c| c.title.as_str()).collect();
        for cmd in Self::default_commands() {
            if !titles.contains(cmd.title.as_str()) {
                tracing::info!("Backfilling default command: {}", cmd.title);
                self.set(&cmd)?;
            }
        }
        Ok(())
    }

    /// Get default commands
    fn default_commands() -> Vec<CustomCommand> {
        vec![
            CustomCommand {
                id: Uuid::new_v4().to_string(),
                title: "配置启动脚本".to_string(),
                command: "请创建 Makefile(如果没有), 之后创建 make dev , 同时启动前端和后端.".to_string(),
                r#type: "default".to_string(),
            },
            CustomCommand {
                id: Uuid::new_v4().to_string(),
                title: "自动修正运行端口".to_string(),
                command: "/milletcode-pickup-port".to_string(),
                r#type: "default".to_string(),
            },
            CustomCommand {
                id: Uuid::new_v4().to_string(),
                title: "图标优化".to_string(),
                command: "请把刚才开发好的功能中的 icon 和 logo, 都使用现代简洁风".to_string(),
                r#type: "default".to_string(),
            },
            CustomCommand {
                id: Uuid::new_v4().to_string(),
                title: "备份代码".to_string(),
                command: "/git commit + /git push".to_string(),
                r#type: "git".to_string(),
            },
            CustomCommand {
                id: Uuid::new_v4().to_string(),
                title: "快速发版".to_string(),
                command: "/git commit --tag + /git push".to_string(),
                r#type: "git".to_string(),
            },
            CustomCommand {
                id: Uuid::new_v4().to_string(),
                title: "发布补丁版本".to_string(),
                command: "/git commit --tag patch + /git push".to_string(),
                r#type: "git".to_string(),
            },
            CustomCommand {
                id: Uuid::new_v4().to_string(),
                title: "发布小版本".to_string(),
                command: "/git commit --tag minor + /git push".to_string(),
                r#type: "git".to_string(),
            },
            CustomCommand {
                id: Uuid::new_v4().to_string(),
                title: "发布大版本".to_string(),
                command: "/git commit --tag major + /git push".to_string(),
                r#type: "git".to_string(),
            },
        ]
    }
}
