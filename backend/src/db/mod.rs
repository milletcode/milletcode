use std::path::PathBuf;
use std::sync::Arc;

use redb::{Database, TableDefinition};

mod session_store;
mod settings_store;
mod commands_store;
mod project_store;
mod approval_store;
mod env_store;
mod issue_store;
mod skills_store;

pub use session_store::SessionRecord;
pub use session_store::SessionStore;
pub use session_store::slugify;
pub use session_store::sanitize_segment;
pub use settings_store::SettingsStore;
pub use commands_store::{CommandsStore, CustomCommand};
pub use project_store::{ProjectStore, ProjectTerminalCommand};
pub use approval_store::{ApprovalStore, DecisionRecord};
pub use env_store::EnvStore;
pub use issue_store::IssueStore;
pub use skills_store::{SkillInstallRecord, SkillInstallSession, SkillsStore};

/// Sessions table: key = "org/project/agent/default", value = SessionRecord (includes session_id)
const SESSIONS: TableDefinition<&str, &str> = TableDefinition::new("sessions");

/// Settings table: key = "settings.gpt_api.primary", value = JSON string
const SETTINGS: TableDefinition<&str, &str> = TableDefinition::new("settings");

/// Legacy approvals table (hook era): key = approval UUID, value = JSON.
/// Kept only so `migrate_legacy` can import then drop it.
const APPROVALS: TableDefinition<&str, &str> = TableDefinition::new("approvals");

/// Decision history: key = (decided_at ms, request_id) so the table is
/// time-ordered and retention is a bounded range delete.
const APPROVAL_HISTORY: TableDefinition<(u64, &str), &str> =
    TableDefinition::new("approval_history");

/// Custom commands table: key = UUID, value = CustomCommand (JSON)
const COMMANDS: TableDefinition<&str, &str> = TableDefinition::new("commands");

/// Project settings table: key = "project/terminal/{path}", value = JSON
const PROJECT_SETTINGS: TableDefinition<&str, &str> = TableDefinition::new("project_settings");

/// Env vars table: key = "/{keyword}/{project}/{env}/{KEY}", value = plain string
const ENVS: TableDefinition<&str, &str> = TableDefinition::new("envs");

/// Project issue-intake config table: key = "/{keyword}/{project}", value = JSON {url, token}
const ISSUE_CONFIG: TableDefinition<&str, &str> = TableDefinition::new("issue_config");

/// Third-party skill install runs: key = (created_at ms, id) — time-ordered,
/// retention is a bounded range delete.
const SKILLS_HISTORY: TableDefinition<(u64, &str), &str> =
    TableDefinition::new("skills_history");

/// Opens a single redb file and ensures the given tables exist.
///
/// redb 首次建库分两步落盘：先写不含 magic 的结构头，数据结构就绪后才补写
/// magic（redb page_manager 初始化序列）。进程在两步之间被杀（断电、
/// SIGTERM）会留下「有内容但无 magic」的半成品文件，此后 create/open 一律
/// InvalidData 拒开，库永久打不开。这种文件不可能含已提交数据——每次
/// commit 重写头时都带 magic——所以按「初始化被中断」处理：删除重建。
fn open_db(path: impl Into<PathBuf>) -> Result<Arc<Database>, redb::Error> {
    let path = path.into();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match Database::create(&path) {
        Ok(db) => Ok(Arc::new(db)),
        Err(redb::DatabaseError::Storage(redb::StorageError::Io(e)))
            if e.kind() == std::io::ErrorKind::InvalidData =>
        {
            if !has_zeroed_magic(&path) {
                return Err(e.into());
            }
            tracing::warn!(
                "数据库文件 {} 缺少 redb magic（首次初始化被中断），删除重建",
                path.display()
            );
            std::fs::remove_file(&path)?;
            Ok(Arc::new(Database::create(&path)?))
        }
        Err(e) => Err(e.into()),
    }
}

/// redb magic 是文件头 9 个非零字节（"redb\x1a\x0a\xa9\x0d\x0a"）；
/// 全零 = 初始化第一步的头（to_bytes(false)），即被中断的首次建库。
fn has_zeroed_magic(path: &std::path::Path) -> bool {
    use std::io::Read;
    let mut head = [0u8; 9];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut head))
        .map(|_| head.iter().all(|&b| b == 0))
        .unwrap_or(false)
}

pub fn open_session_db(path: impl Into<PathBuf>) -> Result<SessionStore, redb::Error> {
    let path = path.into();
    let db = open_db(&path)?;
    let write = db.begin_write()?;
    {
        let _ = write.open_table(SESSIONS)?;
    }
    write.commit()?;

    // Migrate legacy key formats to composite keys (idempotent, one-shot)
    if let Err(e) = session_store::migrate_uuid_to_composite_keys(&db) {
        tracing::warn!("Session key migration failed: {}", e);
    }

    Ok(SessionStore::new(db))
}

pub fn open_config_db(path: impl Into<PathBuf>) -> Result<(SettingsStore, CommandsStore), redb::Error> {
    let path = path.into();
    let db = open_db(&path)?;
    let write = db.begin_write()?;
    {
        let _ = write.open_table(SETTINGS)?;
        let _ = write.open_table(COMMANDS)?;
    }
    write.commit()?;
    let settings_store = SettingsStore::new(db.clone());
    let commands_store = CommandsStore::new(db);
    Ok((settings_store, commands_store))
}

pub fn open_project_db(path: impl Into<PathBuf>) -> Result<ProjectStore, redb::Error> {
    let path = path.into();
    let db = open_db(&path)?;
    let write = db.begin_write()?;
    {
        let _ = write.open_table(PROJECT_SETTINGS)?;
    }
    write.commit()?;
    Ok(ProjectStore::new(db))
}

pub fn open_env_db(path: impl Into<PathBuf>) -> Result<(EnvStore, IssueStore), redb::Error> {
    let path = path.into();
    let db = open_db(&path)?;
    let write = db.begin_write()?;
    {
        let _ = write.open_table(ENVS)?;
        let _ = write.open_table(ISSUE_CONFIG)?;
    }
    write.commit()?;
    let env_store = EnvStore::new(db.clone());
    let issue_store = IssueStore::new(db);
    Ok((env_store, issue_store))
}

pub fn open_skills_db(path: impl Into<PathBuf>) -> Result<SkillsStore, redb::Error> {
    let path = path.into();
    let db = open_db(&path)?;
    let write = db.begin_write()?;
    {
        let _ = write.open_table(SKILLS_HISTORY)?;
    }
    write.commit()?;
    Ok(SkillsStore::new(db))
}

pub fn open_approval_db(path: impl Into<PathBuf>) -> Result<ApprovalStore, redb::Error> {
    let path = path.into();
    let db = open_db(&path)?;
    let write = db.begin_write()?;
    {
        let _ = write.open_table(APPROVAL_HISTORY)?;
    }
    write.commit()?;
    let store = ApprovalStore::new(db);
    // Import pre-refactor hook-era records once, then drop the legacy table.
    match store.migrate_legacy() {
        Ok(n) if n > 0 => tracing::info!("Imported {} legacy approval records", n),
        Ok(_) => {}
        Err(e) => tracing::warn!("Legacy approval migration failed: {}", e),
    }
    Ok(store)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("milletcode-redb-test-{}", name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("test.redb")
    }

    #[test]
    fn open_db_recovers_from_interrupted_first_initialization() {
        let path = temp_db_path("recover");
        // 复刻 redb 初始化第一步落盘的形态：文件有长度、头 9 字节（magic 位）全零。
        // 真实形态由固定结构头 + 后续区域组成，这里只需覆盖判定条件即可。
        let mut bytes = vec![0u8; 4096];
        bytes[9] = 0x04; // two_phase_commit god byte，与实际观察一致
        std::fs::write(&path, &bytes).unwrap();

        open_db(&path).expect("aborted-init file should be deleted and recreated");
    }

    #[test]
    fn open_db_rejects_other_invalid_data() {
        let path = temp_db_path("reject");
        // 非 magic 内容但前 9 字节非全零：不属于「初始化被中断」，必须原样报错。
        let bytes = vec![0xAAu8; 4096];
        std::fs::write(&path, &bytes).unwrap();

        let err = open_db(&path).unwrap_err();
        assert!(
            matches!(
                &err,
                redb::Error::Io(e) if e.kind() == std::io::ErrorKind::InvalidData
            ),
            "expected InvalidData, got {:?}",
            err
        );
    }

    #[test]
    fn open_db_creates_and_reopens_clean_file() {
        let path = temp_db_path("clean");
        open_db(&path).expect("fresh create should succeed");
        drop(open_db(&path).expect("reopen should succeed"));
    }
}
