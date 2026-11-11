//! 技能包：官方分发 + 第三方审阅安装。
//! 日志红线：只打计数/任务结果，不出现节点身份信息。

use std::path::{Path as StdPath, PathBuf};
use std::sync::Arc;

use axum::{Json, extract::{Path, State}, http::StatusCode, response::IntoResponse};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::AppState;
use crate::db::{self, SkillInstallRecord};
use crate::runtime::skills::{
    self, NodeTarget, SkillTask, TaskStatus,
};

const SETTING_OVERWRITE: &str = "skills.overwrite";
const SETTING_OFFICIAL_PREFIX: &str = "skills.official.";

#[derive(Debug, Serialize)]
pub struct OfficialSkillInfo {
    pub name: String,
    pub description: String,
    pub icon: String,
    pub installed: bool,
    pub installed_at: Option<String>,
    pub nodes: Vec<OfficialNodeInfo>,
}

#[derive(Debug, Serialize)]
pub struct OfficialNodeInfo {
    pub node: String,
    /// "installed" | "skipped" | "protected" | "failed"
    pub status: String,
    pub installed_at: Option<String>,
    pub reason: Option<String>,
}

/// 单节点的官方技能安装状态。
#[derive(Debug, Serialize, Deserialize, Clone)]
struct OfficialNodeState {
    /// "installed" | "skipped" | "protected" | "failed"
    status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    installed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

/// 官方技能的按节点安装状态。旧版记录是 `{"installed_at": "..."}`——
/// nodes/updated_at 缺字段时 serde default 兜底，旧语义（有记录即已安装）保留。
#[derive(Debug, Serialize, Deserialize, Clone)]
struct OfficialInstallState {
    #[serde(default = "default_updated_at")]
    updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    installed_at: Option<String>,
    #[serde(default)]
    nodes: std::collections::BTreeMap<String, OfficialNodeState>,
}

fn default_updated_at() -> String {
    chrono::Utc::now().to_rfc3339()
}

impl OfficialInstallState {
    fn now() -> String {
        chrono::Utc::now().to_rfc3339()
    }

    fn fresh() -> Self {
        Self {
            updated_at: Self::now(),
            installed_at: None,
            nodes: Default::default(),
        }
    }

    /// 是否仍有节点处于已安装/受保护状态——决定状态记录去留。
    /// 旧版记录没有 nodes：有 installed_at 即视为已安装（旧语义）。
    fn has_installed_nodes(&self) -> bool {
        if self.nodes.is_empty() {
            return self.installed_at.is_some();
        }
        self.nodes
            .values()
            .any(|n| n.status == "installed" || n.status == "protected")
    }

    /// 安装任务终态后按节点合并。NodeReport 语义（run_skill_task）：
    /// ok=true reason=None → installed；"exists" → installed（已在全局目录）；
    /// "skipped_no_agent" → skipped；"protected" → protected；ok=false → failed。
    fn merge_install(&mut self, reports: &[skills::NodeReport]) {
        self.updated_at = Self::now();
        for r in reports {
            let entry = self.nodes.entry(r.node.clone()).or_insert_with(|| OfficialNodeState {
                status: "failed".to_string(),
                installed_at: None,
                reason: None,
            });
            match (r.ok, r.reason.as_deref()) {
                (true, None | Some("installed") | Some("exists")) => {
                    entry.status = "installed".to_string();
                    entry.installed_at = Some(entry.installed_at.clone().unwrap_or_else(Self::now));
                    entry.reason = None;
                }
                (true, Some("skipped_no_agent")) => {
                    entry.status = "skipped".to_string();
                    entry.reason = Some("skipped_no_agent".to_string());
                }
                (true, Some(other)) => {
                    entry.status = "protected".to_string();
                    entry.reason = Some(other.to_string());
                }
                (false, detail) => {
                    entry.status = "failed".to_string();
                    entry.reason = Some(detail.unwrap_or_default().to_string());
                }
            }
        }
        self.installed_at = self
            .nodes
            .values()
            .filter_map(|n| n.installed_at.clone())
            .min();
    }

    /// 卸载任务终态后按节点合并：removed/absent 的节点移除记录；
    /// protected/failed/skipped 的节点保持原样（该节点仍视为已安装）。
    fn merge_uninstall(&mut self, reports: &[skills::NodeReport]) {
        self.updated_at = Self::now();
        for r in reports {
            if r.ok && matches!(r.reason.as_deref(), None | Some("removed") | Some("absent")) {
                self.nodes.remove(&r.node);
            }
        }
        self.installed_at = self
            .nodes
            .values()
            .filter_map(|n| n.installed_at.clone())
            .min();
    }
}

fn overwrite_enabled(state: &AppState) -> bool {
    match state.settings_store.get(SETTING_OVERWRITE) {
        Ok(Some(v)) => v != "false",
        _ => true,
    }
}

fn official_states(state: &AppState) -> Vec<(String, OfficialInstallState)> {
    state
        .settings_store
        .list(SETTING_OFFICIAL_PREFIX)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(k, v)| {
            let name = k.strip_prefix(SETTING_OFFICIAL_PREFIX)?.to_string();
            serde_json::from_str::<OfficialInstallState>(&v).ok().map(|s| (name, s))
        })
        .collect()
}

/// 任务终态后的持久化动作。
enum Completion {
    /// 按节点合并安装状态（官方安装）。
    OfficialInstall { key: String },
    /// 按节点合并卸载结果（官方卸载）。
    OfficialUninstall { key: String },
    None,
}

fn spawn_task(
    state: &Arc<AppState>,
    kind: &str,
    skill_name: &str,
    nodes: Vec<NodeTarget>,
    src_dirs: Vec<PathBuf>,
    completion: Completion,
) -> String {
    let task_id = format!("skill-{}", Uuid::new_v4().simple());
    let overwrite = overwrite_enabled(state);
    let config = state.config_manager.config();
    let agent_id = skills::DISTRIBUTION_AGENT.to_string();
    let registry = state.skill_tasks.clone();
    let kind = kind.to_string();
    let skill_name = skill_name.to_string();

    state.skill_tasks.push(SkillTask {
        id: task_id.clone(),
        kind: kind.clone(),
        skill: skill_name.clone(),
        status: TaskStatus::Running,
        total_nodes: nodes.len(),
        done_nodes: 0,
        per_node: Vec::new(),
        summary: String::new(),
        created_at: chrono::Utc::now().to_rfc3339(),
    });

    let state_for_hook = state.clone();
    let task_id_inner = task_id.clone();
    tokio::spawn(async move {
        let status = skills::run_skill_task(
            registry,
            task_id_inner.clone(),
            &kind,
            &skill_name,
            nodes,
            config,
            &agent_id,
            overwrite,
            src_dirs,
        )
        .await;
        let success = status == TaskStatus::Success;
        let store = &state_for_hook.settings_store;
        // 终态后的节点级报告：按节点合并进状态记录，Partial 不再整条清除
        let reports = state_for_hook
            .skill_tasks
            .get(&task_id_inner)
            .map(|t| t.per_node)
            .unwrap_or_default();
        let installing = matches!(completion, Completion::OfficialInstall { .. });
        match completion {
            Completion::OfficialInstall { key } | Completion::OfficialUninstall { key } => {
                let mut st = store
                    .get(&key)
                    .ok()
                    .flatten()
                    .and_then(|v| serde_json::from_str::<OfficialInstallState>(&v).ok())
                    .unwrap_or_else(OfficialInstallState::fresh);
                if installing {
                    st.merge_install(&reports);
                } else {
                    st.merge_uninstall(&reports);
                }
                let res = if st.has_installed_nodes() {
                    serde_json::to_string(&st)
                        .map_err(anyhow::Error::msg)
                        .and_then(|v| store.set(&key, &v).map_err(anyhow::Error::msg).map(|_| ()))
                } else {
                    store.delete(&key).map(|_| ()).map_err(anyhow::Error::msg)
                };
                tracing::info!("skill task done: success={} hook={:?}", success, res.is_ok());
            }
            Completion::None => {}
        }
    });
    task_id
}

// ---------------------------------------------------------------------------
// 官方技能
// ---------------------------------------------------------------------------

pub async fn list_official(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let manifest = match skills::load_manifest() {
        Ok(m) => m,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() })))
                .into_response()
        }
    };
    let installed: std::collections::HashMap<String, OfficialInstallState> =
        official_states(&state).into_iter().collect();
    let list: Vec<OfficialSkillInfo> = manifest
        .skills
        .iter()
        .map(|s| {
            let inst = installed.get(&s.name);
            OfficialSkillInfo {
                name: s.name.clone(),
                description: s.description.clone(),
                icon: s.icon.clone(),
                installed: inst.map(|st| st.has_installed_nodes()).unwrap_or(false),
                installed_at: inst.and_then(|st| st.installed_at.clone()),
                nodes: inst
                    .map(|st| {
                        st.nodes
                            .iter()
                            .map(|(node, n)| OfficialNodeInfo {
                                node: node.clone(),
                                status: n.status.clone(),
                                installed_at: n.installed_at.clone(),
                                reason: n.reason.clone(),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            }
        })
        .collect();
    Json(serde_json::json!({
        "skills": list,
        "overwrite": overwrite_enabled(&state),
    }))
    .into_response()
}

pub async fn set_overwrite(State(state): State<Arc<AppState>>, Json(req): Json<serde_json::Value>) -> impl IntoResponse {
    let enabled = req.get("enabled").and_then(|v| v.as_bool());
    match enabled {
        Some(b) => {
            let _ = state.settings_store.set(SETTING_OVERWRITE, if b { "true" } else { "false" });
            (StatusCode::OK, Json(serde_json::json!({ "overwrite": b }))).into_response()
        }
        None => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "enabled (bool) required" })),
        )
            .into_response(),
    }
}

/// 官方包归档来源（下载地址 + 解压密码）：只来自 config.toml 的 [skills] 段，
/// 下载地址 = official_base_url/{name}.7z。安装前先校验，未配置时直接报错、
/// 不消耗任何下载流量。
fn official_archive_source(state: &AppState, name: &str) -> Result<(String, String), String> {
    let conf = state.config_manager.config();
    let base = conf
        .skills
        .official_base_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let password = conf
        .skills
        .official_password
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    match (base, password) {
        (Some(base), Some(password)) => {
            Ok((format!("{}/{}.7z", base.trim_end_matches('/'), name), password.to_string()))
        }
        _ => Err(
            "当前版本需要先在 config.toml 的 [skills] 段配置官方技能包下载地址（official_base_url）与解压密码（official_password）；如不知道，请联系官方申请。"
                .to_string(),
        ),
    }
}

async fn fetch_and_extract(url: &str, password: &str, dest: &PathBuf) -> Result<Vec<PathBuf>, String> {
    let work = skills::skills_workspace().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
    let archive = work.join(format!("pkg-{}.7z", Uuid::new_v4().simple()));
    skills::download_to(url, &archive)
        .await
        .map_err(|e| e.to_string())?;
    let result = skills::extract_7z(&archive, password, dest).await;
    let _ = tokio::fs::remove_file(&archive).await;
    result.map_err(|e| e.to_string())?;
    skills::find_skill_dirs(dest).await.map_err(|e| e.to_string())
}

pub async fn install_official(State(state): State<Arc<AppState>>, Path(name): Path<String>) -> impl IntoResponse {
    let entry = match skills::load_manifest().ok().and_then(|m| m.skills.into_iter().find(|s| s.name == name)) {
        Some(e) => e,
        None => return (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "skill not found in manifest" }))).into_response(),
    };
    // 先校验配置再下载：base_url / 密码未配置时直接 400，不消耗流量。
    let (url, password) = match official_archive_source(&state, &name) {
        Ok(v) => v,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": e }))).into_response(),
    };
    // 官方包从 skill 根目录打包（SKILL.md 直接在归档根），解压目录名即技能名，
    // 这样分发阶段的源目录名匹配天然成立。
    let dest = match skills::skills_workspace() {
        Ok(w) => w.join(skills::sanitize_skill_name(StdPath::new(&name))),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))).into_response(),
    };
    let src_dirs = match fetch_and_extract(&url, &password, &dest).await {
        Ok(d) if d.is_empty() => {
            let _ = tokio::fs::remove_dir_all(&dest).await;
            return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "archive contains no skill dirs (SKILL.md)" }))).into_response();
        }
        Ok(d) => d,
        Err(e) => {
            let _ = tokio::fs::remove_dir_all(&dest).await;
            return (StatusCode::BAD_GATEWAY, Json(serde_json::json!({ "error": e }))).into_response();
        }
    };

    let config = state.config_manager.config();
    let nodes = skills::plan_nodes(&state.config_manager, skills::DISTRIBUTION_AGENT);
    drop(config);
    let setting_key = format!("{SETTING_OFFICIAL_PREFIX}{name}");
    let task_id = spawn_task(
        &state,
        "install",
        &name,
        nodes,
        src_dirs,
        Completion::OfficialInstall { key: setting_key },
    );
    (StatusCode::ACCEPTED, Json(serde_json::json!({ "task_id": task_id }))).into_response()
}

pub async fn uninstall_official(State(state): State<Arc<AppState>>, Path(name): Path<String>) -> impl IntoResponse {
    let nodes = skills::plan_nodes(&state.config_manager, skills::DISTRIBUTION_AGENT);
    let setting_key = format!("{SETTING_OFFICIAL_PREFIX}{name}");
    let task_id = spawn_task(
        &state,
        "uninstall",
        &name,
        nodes,
        Vec::new(),
        Completion::OfficialUninstall { key: setting_key },
    );
    (StatusCode::ACCEPTED, Json(serde_json::json!({ "task_id": task_id }))).into_response()
}

// ---------------------------------------------------------------------------
// 第三方技能：按（节点 × 实际存在的 agent）fan-out 安装会话
// ---------------------------------------------------------------------------

/// 安装会话的总时限。超时未结束的会话按失败落痕。
const WATCH_DEADLINE_SECS: u64 = 30 * 60;

#[derive(Debug, Deserialize)]
pub struct ThirdInstallRequest {
    /// 安装提示词（含技能包地址），原样交给每个安装会话。
    pub prompt: String,
}

fn now_ms() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

/// 留痕展示用提示词：只保留前 200 个字符。
fn prompt_display(prompt: &str) -> String {
    prompt.chars().take(200).collect()
}

/// 留痕 JSON。stats_mode = total 时前端不展示节点数量，字段仍随响应给出。
fn record_json(rec: &SkillInstallRecord, stats_mode: &str) -> serde_json::Value {
    let node_total = rec
        .sessions
        .iter()
        .map(|s| s.node.as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    serde_json::json!({
        "id": rec.id,
        "prompt": prompt_display(&rec.prompt),
        "created_at": rec.created_at,
        "finished_at": rec.finished_at,
        "agent_total": rec.agent_total,
        "node_total": node_total,
        "status": rec.status,
        "sessions": rec.sessions,
        "stats_mode": stats_mode,
    })
}

/// 跟踪每个安装会话直至全部终态（或超时），然后写入留痕终态并执行保留策略。
/// 会话是交互式的（一轮结束后进程仍存活），因此不看 session 状态机的
/// complete —— 以事件总线上的 `result`（CLI 的权威轮末标记）为完成信号，
/// 且 `result` 之后还要验收全局技能目录真的出现了新条目才算成功。
async fn watch_install_run(
    state: std::sync::Arc<AppState>,
    record_id: String,
    targets: std::collections::HashMap<String, (NodeTarget, String, skills::InstallMarker)>,
    mut rx: tokio::sync::broadcast::Receiver<crate::agents::types::AgentEvent>,
) {
    use crate::agents::types::AgentEvent;
    use std::collections::HashMap;

    let started = std::time::Instant::now();
    let deadline = std::time::Duration::from_secs(WATCH_DEADLINE_SECS);
    // session_key -> "complete" | "error"；空串 = 仍在运行
    let mut terminal: HashMap<String, String> =
        targets.keys().map(|k| (k.clone(), String::new())).collect();
    // 验收失败说明（已脱敏），仅 error 会话携带
    let mut notes: HashMap<String, String> = HashMap::new();

    let persist = |terminal: &HashMap<String, String>,
                   notes: &HashMap<String, String>,
                   finished: bool| {
        if let Ok(Some(mut rec)) = state.skills_store.get(&record_id) {
            for s in &mut rec.sessions {
                if let Some(st) = terminal.get(&s.session_key) {
                    if !st.is_empty() {
                        s.status = st.clone();
                    }
                }
                if let Some(note) = notes.get(&s.session_key) {
                    s.note = Some(note.clone());
                }
            }
            if finished {
                let ok = rec.sessions.iter().filter(|s| s.status == "complete").count();
                let total = rec.sessions.len();
                rec.status = if ok == total {
                    "success".to_string()
                } else if ok == 0 {
                    "failed".to_string()
                } else {
                    "partial".to_string()
                };
                rec.finished_at = Some(now_ms());
                let _ = state.skills_store.update(&rec);
                let retention = state.config_manager.config().skills.history_count;
                if let Ok(n) = state.skills_store.enforce_retention(retention) {
                    if n > 0 {
                        tracing::info!("skill install records pruned: {n}");
                    }
                }
                // 日志红线：只打计数，不出现节点身份信息
                tracing::info!("skill install run done: ok={ok} total={total}");
                return;
            }
            let _ = state.skills_store.update(&rec);
        }
    };

    loop {
        let remaining = deadline.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Err(_elapsed) => break, // deadline
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Ok(event)) => match event {
                AgentEvent::Output {
                    session_id,
                    raw: Some(v),
                    ..
                } if v.get("type").and_then(|t| t.as_str()) == Some("result")
                    && terminal.contains_key(&session_id.0)
                    && terminal.get(&session_id.0).map(String::as_str) != Some("complete") =>
                {
                    // CLI 自报完成 ≠ 真装好：历史上 npx skills 会按 cwd 做
                    // 项目级安装后照样报成功。验收全局技能目录相比会话开始
                    // 前的标记出现新条目，才算安装成功。
                    let outcome = match targets.get(&session_id.0) {
                        Some((node, agent_id, marker)) => {
                            skills::verify_global_install(node, agent_id, marker).await
                        }
                        None => Err("verify_failed: missing marker".to_string()),
                    };
                    match outcome {
                        Ok(()) => {
                            terminal.insert(session_id.0, "complete".to_string());
                        }
                        Err(note) => {
                            notes.insert(session_id.0.clone(), note);
                            terminal.insert(session_id.0, "error".to_string());
                        }
                    }
                }
                AgentEvent::SessionComplete {
                    session_id,
                    exit_code,
                    ..
                } if terminal.contains_key(&session_id.0)
                    && terminal.get(&session_id.0).map(String::as_str) != Some("complete")
                    && terminal.get(&session_id.0).map(String::as_str) != Some("error") =>
                {
                    // 进程退出：轮末标记还没来（正常结束/被停止/崩溃）
                    let _ = exit_code;
                    terminal.insert(session_id.0, "error".to_string());
                }
                _ => {}
            },
            Ok(Err(_)) => break, // channel closed
        }
        if terminal.values().all(|st| !st.is_empty()) {
            persist(&terminal, &notes, true);
            return;
        }
    }
    persist(&terminal, &notes, true);
}

/// 安装会话的系统级追加指令。npx skills 之类的工具默认按 cwd 做**项目级**
/// 安装（./.claude/skills + .agents），仅靠口头嘱咐会被无视——必须给出
/// 硬参数（-g）并明令禁止项目级落点；装完平台还会对全局目录做验收。
fn compose_install_prompt(user_prompt: &str) -> String {
    format!(
        "{user_prompt}\n\n安装要求（必须严格遵守）：\n\
         1. 安装命令必须带全局安装参数，例如 npx skills add <repo> -g；禁止不带 -g 的项目级安装；\n\
         2. 技能最终必须真实存在于全局技能目录（claude 为 ~/.claude/skills/<技能名>/SKILL.md），每个目标 agent 对应它自己的全局目录；\n\
         3. 出现 ./.claude/skills、./.agents、./skills 之类的项目级安装目录即为失败，必须删除后用全局参数重装；\n\
         4. 安装完成后报告最终安装路径。"
    )
}

/// 第三方安装：风险确认后，前端只提交一段安装提示词。后端对每个
/// （节点 × 实际存在的启用 agent）各开一个安装会话（远端节点经 ssh 运行），
/// 全部结束后写入 skills.redb 留痕。
pub async fn create_third(State(state): State<Arc<AppState>>, Json(req): Json<ThirdInstallRequest>) -> impl IntoResponse {
    let prompt = req.prompt.trim().to_string();
    if prompt.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "prompt required" }))).into_response();
    }
    let session_prompt = compose_install_prompt(&prompt);
    let config = state.config_manager.config();
    let enabled_agents: Vec<String> = state
        .config_manager
        .get_enabled_agents()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    if enabled_agents.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "no enabled agents" }))).into_response();
    }

    // 探测：每个启用 agent × 每个节点，agent 实际存在才纳入
    let mut candidates: Vec<(skills::NodeTarget, String)> = Vec::new();
    for agent_id in &enabled_agents {
        for node in skills::plan_nodes(&state.config_manager, agent_id) {
            if skills::probe_agent(&node, agent_id, &config).await {
                candidates.push((node, agent_id.clone()));
            }
        }
    }
    drop(config);
    if candidates.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "no agent found on any node" }))).into_response();
    }

    let id = Uuid::new_v4().simple().to_string();
    let id8 = id[..8].to_string();
    let session_name = format!("skill-{id8}");

    // 订阅事件总线必须早于会话创建，否则早熟的 result 事件会漏掉
    let rx = state.event_bus.subscribe();

    // 只有会话真正创建成功的配对才计入总数 —— 目标机器上不存在的 agent
    // 是合理情况（无 adapter/二进制缺失），不算失败也不占数量。
    let mut sessions: Vec<db::SkillInstallSession> = Vec::new();
    // session_key -> (节点, agent, 验收标记)，watcher 用它在 result 后验收
    let mut targets: std::collections::HashMap<String, (NodeTarget, String, skills::InstallMarker)> =
        std::collections::HashMap::new();
    for (node, agent_id) in &candidates {
        // 每次 run 用独立 scratch 目录做会话 cwd：目录里没有任何项目语义
        // （无 SKILL.md/.claude），agent CLI 不会把安装工具引向某个项目，
        // 配合 prompt 里的硬参数要求，npx skills 只能按 -g 落全局目录。
        let project_path = if node.is_local {
            let Ok(ws) = skills::skills_workspace() else {
                continue;
            };
            let scratch = ws.join(format!("cwd-{id8}"));
            if let Err(e) = std::fs::create_dir_all(&scratch) {
                tracing::warn!("skill install: scratch dir create failed: {e}");
                continue;
            }
            state.project_manager.register_internal_skills_project_at(&scratch);
            state.project_manager.get_by_id("skills/installer").map(|p| p.path)
        } else {
            let scratch = format!("{}/cwd-{}", skills::REMOTE_SKILLS_WORKSPACE, id8);
            match skills::ensure_remote_dir(node, &scratch).await {
                Ok(dir) => {
                    state
                        .project_manager
                        .register_remote_skills_project(&node.label, &node.hostname, &dir);
                    state
                        .project_manager
                        .get_by_id(&format!("{}/skills", node.label))
                        .map(|p| p.path)
                }
                Err(e) => {
                    tracing::warn!("skill install: workspace setup failed on a node: {e}");
                    continue;
                }
            }
        };
        let Some(project_path) = project_path else {
            continue;
        };
        // 会话开始前放置验收基准（事后对比全局技能目录的新条目）
        let marker = match skills::create_install_marker(node, &id, agent_id).await {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("skill install: marker setup failed on a node: {e}");
                continue;
            }
        };
        match state
            .session_manager
            .create_session(Some(project_path), agent_id.clone(), &session_prompt, &session_name)
            .await
        {
            Ok((_sid, key)) => {
                targets.insert(key.clone(), (node.clone(), agent_id.clone(), marker));
                sessions.push(db::SkillInstallSession {
                    node: node.label.clone(),
                    agent_id: agent_id.clone(),
                    session_key: key,
                    status: "running".to_string(),
                    note: None,
                })
            }
            Err(e) => {
                tracing::warn!("skill install: session create failed on a node: {e}");
            }
        }
    }

    if sessions.is_empty() {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": "failed to create any install session" }))).into_response();
    }

    let detail_mode = state.config_manager.config().skills.is_detail_mode();
    let stats_mode = if detail_mode { "detail" } else { "total" };
    let record = SkillInstallRecord {
        id: id.clone(),
        prompt,
        created_at: now_ms(),
        finished_at: None,
        agent_total: sessions.len(),
        sessions,
        status: "running".to_string(),
    };
    if let Err(e) = state.skills_store.record(&record) {
        tracing::warn!("skill install record write failed: {e}");
    }
    let state_for_watch = state.clone();
    let record_id = id;
    tokio::spawn(async move {
        watch_install_run(state_for_watch, record_id, targets, rx).await;
    });
    (StatusCode::CREATED, Json(record_json(&record, stats_mode))).into_response()
}

#[derive(Debug, Deserialize)]
pub struct ListParams {
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}

pub async fn list_third(State(state): State<Arc<AppState>>, axum::extract::Query(params): axum::extract::Query<ListParams>) -> impl IntoResponse {
    let offset = params.offset.unwrap_or(0);
    let limit = params.limit.unwrap_or(20).min(100);
    let detail_mode = state.config_manager.config().skills.is_detail_mode();
    let stats_mode = if detail_mode { "detail" } else { "total" };
    match state.skills_store.list(offset, limit) {
        Ok((records, has_more)) => {
            let records: Vec<serde_json::Value> = records.iter().map(|r| record_json(r, stats_mode)).collect();
            Json(serde_json::json!({ "records": records, "has_more": has_more, "stats_mode": stats_mode })).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))).into_response(),
    }
}

/// 删除一条第三方安装留痕。运行中的记录不允许删：watcher 结束时会把它重新写回。
pub async fn delete_third(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> impl IntoResponse {
    match state.skills_store.get(&id) {
        Ok(Some(rec)) if rec.status == "running" => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": "record is still running" })),
        )
            .into_response(),
        Ok(Some(_)) => match state.skills_store.delete(&id) {
            Ok(true) => StatusCode::NO_CONTENT.into_response(),
            Ok(false) => (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "record not found" })),
            )
                .into_response(),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response(),
        },
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "record not found" })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

pub async fn get_history_count(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(serde_json::json!({
        "count": state.config_manager.config().skills.history_count,
    }))
}

pub async fn set_history_count(State(state): State<Arc<AppState>>, Json(body): Json<serde_json::Value>) -> impl IntoResponse {
    match body.get("count").and_then(|c| c.as_u64()) {
        Some(n) => {
            let n = n.min(u32::MAX as u64) as u32;
            if let Err(e) = state.config_manager.set_skills_history_count(n) {
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))).into_response();
            }
            Json(serde_json::json!({ "count": n })).into_response()
        }
        None => (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "count (number) required" }))).into_response(),
    }
}

pub async fn get_task(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> impl IntoResponse {
    match state.skill_tasks.get(&id) {
        Some(t) => Json(t).into_response(),
        None => (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "task not found" }))).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_prompt_appends_global_dir_requirement() {
        let p = compose_install_prompt("npx skills add hugohe3/ppt-master");
        assert!(p.starts_with("npx skills add hugohe3/ppt-master"));
        assert!(p.contains(" -g"));
        assert!(p.contains("全局技能目录"));
        assert!(p.contains("./.claude/skills"));
        // 留痕只记用户原文，追加指令只进会话
        let empty = compose_install_prompt("");
        assert!(empty.starts_with("\n\n安装要求"));
    }

    fn report(node: &str, ok: bool, reason: Option<&str>) -> skills::NodeReport {
        skills::NodeReport {
            node: node.to_string(),
            ok,
            reason: reason.map(str::to_string),
        }
    }

    #[test]
    fn official_state_legacy_record_parses_and_counts_installed() {
        let st: OfficialInstallState =
            serde_json::from_str(r#"{"installed_at":"2026-01-01T00:00:00Z"}"#).unwrap();
        assert_eq!(st.installed_at.as_deref(), Some("2026-01-01T00:00:00Z"));
        assert!(st.nodes.is_empty());
        // 旧语义：有记录即已安装
        assert!(st.has_installed_nodes());
    }

    #[test]
    fn official_state_merge_install_records_per_node_outcomes() {
        let mut st = OfficialInstallState::fresh();
        st.merge_install(&[
            report("local", true, None),
            report("n1", true, Some("exists")),
            report("n2", true, Some("skipped_no_agent")),
            report("n3", true, Some("protected")),
            report("n4", false, Some("boom")),
        ]);
        assert_eq!(st.nodes["local"].status, "installed");
        assert!(st.nodes["local"].installed_at.is_some());
        assert_eq!(st.nodes["n1"].status, "installed");
        assert_eq!(st.nodes["n2"].status, "skipped");
        assert_eq!(st.nodes["n3"].status, "protected");
        assert_eq!(st.nodes["n4"].status, "failed");
        assert_eq!(st.nodes["n4"].reason.as_deref(), Some("boom"));
        // Partial：有节点装上 → 记录保留，不再整条清除
        assert!(st.has_installed_nodes());
    }

    #[test]
    fn official_state_all_failed_or_skipped_drops_record() {
        let mut st = OfficialInstallState::fresh();
        st.merge_install(&[
            report("a", false, Some("x")),
            report("b", true, Some("skipped_no_agent")),
        ]);
        assert!(!st.has_installed_nodes());
    }

    #[test]
    fn official_state_merge_uninstall_keeps_protected_nodes() {
        let mut st = OfficialInstallState::fresh();
        st.merge_install(&[report("a", true, None), report("b", true, Some("protected"))]);
        st.merge_uninstall(&[
            report("a", true, Some("removed")),
            report("b", true, Some("protected")),
        ]);
        assert!(!st.nodes.contains_key("a"));
        assert!(st.nodes.contains_key("b"));
        assert!(st.has_installed_nodes());

        st.merge_uninstall(&[report("b", true, Some("absent"))]);
        assert!(st.nodes.is_empty());
        assert!(!st.has_installed_nodes());
    }
}
