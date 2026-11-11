use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use redb::{Database, ReadableTable};
use serde::{Deserialize, Serialize};

use super::SESSIONS;

/// No-op migration — we drop the database during development instead of migrating.
pub fn migrate_uuid_to_composite_keys(_db: &Database) -> Result<()> {
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    /// Composite redb key: "keyword/project/agent-name/session-name" (same as redb key)
    #[serde(rename = "id")]
    pub session_id: String,
    /// UUID for Claude CLI (used for --session-id/--resume, disk paths, hook URLs)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claude_session_id: Option<String>,
    pub project_id: Option<String>,
    /// Keyword (tag) from project_discovery_paths configuration
    #[serde(default)]
    pub keyword: String,
    pub agent_id: String,
    pub agent_name: String,
    pub status: String,
    pub prompt: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub create_time: u64,
    pub update_time: u64,
    /// PID of the Claude process managing this session (for lifecycle tracking)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Start time of that PID (`ps -o lstart=`) captured at spawn — compared
    /// at reclaim so a reused PID belonging to a foreign process is never killed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid_started_at: Option<String>,
    /// Idle auto-notification (bell) enabled. Persisted so the backend idle
    /// scanner keeps notifying even with no browser tab connected.
    #[serde(default)]
    pub idle_notify_enabled: bool,
    /// smartcode 当前生效模型：config_changed 回执确认后落盘，restore/resume
    /// 后据此补发 set_config（进程内存里的切换不落盘就会丢）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// 最近一次崩溃/错误原因（如 stderr 尾部）。status=error 时落盘——
    /// 会话页刷新后对话从 agent JSONL 重建，平台内部的错误提示不在里面，
    /// 不落库就只能在崩溃瞬间连着 WS 才能看到。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

impl SessionRecord {
    pub fn now_millis() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }
}

#[derive(Clone)]
pub struct SessionStore {
    db: Arc<Database>,
}

impl SessionStore {
    pub(crate) fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    /// Build composite key: "keyword/project/agent-id/session-name"
    /// project_id format: "keyword/name" (e.g., "milletcode/millet-agents")
    pub fn make_key(keyword: &str, project_id: Option<&str>, agent_id: &str, session_name: &str) -> String {
        let (kw, project) = match project_id {
            Some(pid) => {
                // project_id is "keyword/name" format
                let (k, p) = pid.split_once('/').unwrap_or((keyword, pid));
                (k.to_string(), sanitize_segment(p))
            }
            None => (keyword.to_string(), "unknown".to_string()),
        };
        // Use agent_id directly without sanitization
        let name = sanitize_segment(session_name);
        let name = if name.is_empty() { "default".to_string() } else { name };
        let kw = if kw.is_empty() { "default".to_string() } else { kw };
        format!("{}/{}/{}/{}", kw, project, agent_id, name)
    }

    pub fn put(&self, key: &str, record: &SessionRecord) -> Result<()> {
        let value = serde_json::to_string(&record)?;
        let write = self.db.begin_write()?;
        {
            let mut table = write.open_table(SESSIONS)?;
            table.insert(key, value.as_str())?;
        }
        write.commit()?;
        Ok(())
    }

    pub fn delete_all(&self) -> Result<()> {
        let write = self.db.begin_write()?;
        {
            let mut table = write.open_table(SESSIONS)?;
            let keys: Vec<String> = table.iter()?.filter_map(|item| item.ok().map(|(k, _)| k.value().to_string())).collect();
            for key in keys {
                table.remove(key.as_str())?;
            }
        }
        write.commit()?;
        Ok(())
    }

    pub fn get(&self, key: &str) -> Result<Option<SessionRecord>> {
        let read = self.db.begin_read()?;
        let table = read.open_table(SESSIONS)?;
        match table.get(key)? {
            Some(v) => {
                let record = serde_json::from_str(v.value())?;
                Ok(Some(record))
            }
            None => Ok(None),
        }
    }

    pub fn list(&self) -> Result<Vec<(String, SessionRecord)>> {
        let read = self.db.begin_read()?;
        let table = read.open_table(SESSIONS)?;
        let mut results = Vec::new();

        for item in table.iter()? {
            let (k, v) = item?;
            let key = k.value().to_string();
            let record = serde_json::from_str(v.value())?;
            results.push((key, record));
        }

        Ok(results)
    }

    pub fn delete(&self, key: &str) -> Result<bool> {
        let write = self.db.begin_write()?;
        let removed;
        {
            let mut table = write.open_table(SESSIONS)?;
            removed = table.remove(key)?.is_some();
        }
        write.commit()?;
        Ok(removed)
    }
}

pub fn slugify(text: &str) -> String {
    let s = text
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("-");
    s.chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .collect()
}

pub fn sanitize_segment(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect::<String>()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_record() -> SessionRecord {
        SessionRecord {
            session_id: "kw/proj/claude/default".to_string(),
            claude_session_id: None,
            project_id: Some("kw/proj".to_string()),
            keyword: "kw".to_string(),
            agent_id: "claude".to_string(),
            agent_name: "claude".to_string(),
            status: "error".to_string(),
            prompt: String::new(),
            created_at: chrono::Utc::now(),
            create_time: 0,
            update_time: 0,
            pid: None,
            pid_started_at: None,
            idle_notify_enabled: false,
            model: None,
            last_error: None,
        }
    }

    #[test]
    fn last_error_round_trips_through_json() {
        let mut record = base_record();
        record.last_error = Some("进程异常退出（exit code: 1）。stderr 输出：\nboom".to_string());
        let json = serde_json::to_string(&record).unwrap();
        let parsed: SessionRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed.last_error.as_deref(),
            Some("进程异常退出（exit code: 1）。stderr 输出：\nboom")
        );
    }

    /// 老库里的记录没有 last_error 字段——serde default 必须兜住，否则
    /// 升级后所有历史会话反序列化直接失败。
    #[test]
    fn legacy_record_without_last_error_deserializes() {
        let mut record = base_record();
        record.last_error = Some("crash".to_string());
        let json = serde_json::to_string(&record).unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let had_field = value.as_object_mut().unwrap().remove("last_error").is_some();
        assert!(had_field);
        let legacy: SessionRecord = serde_json::from_value(value).unwrap();
        assert_eq!(legacy.last_error, None);
    }
}
