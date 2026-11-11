//! 官方/第三方技能包：下载、解压、跨节点分发与卸载。
//!
//! 日志红线：本模块的 tracing 只允许出现技能名与计数，禁止出现
//! hostname、"remote"、discovery keyword 等节点身份信息；节点明细
//! 只通过 SkillTask（API 响应）暴露给前端。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::api::uploads::build_ssh_args;
use crate::config::{Config, ConfigManager};
use crate::db::sanitize_segment;

pub const MANIFEST_TOML: &str = include_str!("../../skills-manifest.toml");

/// 分发目标 agent（官方技能装到各节点的该 agent 技能目录）。
pub const DISTRIBUTION_AGENT: &str = "claude";

#[derive(Debug, Clone, Deserialize)]
pub struct SkillEntry {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub icon: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SkillsManifest {
    #[serde(default, rename = "skill")]
    pub skills: Vec<SkillEntry>,
}

pub fn load_manifest() -> Result<SkillsManifest> {
    toml::from_str(MANIFEST_TOML).context("skills-manifest.toml parse failed")
}

/// 技能包运行时工作目录（下载/解压/第三方审阅），不存在则创建。
pub fn skills_workspace() -> Result<PathBuf> {
    let dir = dirs::home_dir()
        .context("HOME not set")?
        .join(".milletcode")
        .join("runtime")
        .join("skills");
    Ok(dir)
}

// ---------------------------------------------------------------------------
// 节点计划与探测
// ---------------------------------------------------------------------------

/// 一个待分发节点。label 仅用于 API 响应展示，禁止写入日志。
#[derive(Debug, Clone, Serialize)]
pub struct NodeTarget {
    pub label: String,
    pub is_local: bool,
    #[serde(skip_serializing)]
    pub hostname: String,
    #[serde(skip_serializing)]
    pub ssh_config_path: String,
    #[serde(skip_serializing)]
    pub explicit_agent_path: String,
}

/// 本地节点 + 所有启用 ssh 的 discovery path 节点。
pub fn plan_nodes(config_manager: &ConfigManager, agent_id: &str) -> Vec<NodeTarget> {
    let config = config_manager.config();
    let local_explicit = config
        .agent
        .find_by_id(agent_id)
        .and_then(|(_, c)| c.path.clone())
        .unwrap_or_default();

    let mut nodes = vec![NodeTarget {
        label: "local".to_string(),
        is_local: true,
        hostname: String::new(),
        ssh_config_path: String::new(),
        explicit_agent_path: local_explicit,
    }];

    let mut remotes: Vec<(String, (PathBuf, String, String, String, bool))> =
        config_manager.resolve_project_discovery_paths()
        .into_iter()
        .filter(|(_, (_, _, remote_mode, _, _))| remote_mode == "ssh")
        .collect();
    remotes.sort_by(|a, b| a.0.cmp(&b.0));

    for (keyword, (_, hostname, _, ssh_config_path, _)) in remotes {
        if hostname.is_empty() {
            continue;
        }
        let explicit = config
            .project
            .get_discovery_path(&keyword)
            .and_then(|dp| dp.paths.get(agent_id))
            .cloned()
            .unwrap_or_default();
        nodes.push(NodeTarget {
            label: keyword,
            is_local: false,
            hostname,
            ssh_config_path,
            explicit_agent_path: explicit,
        });
    }
    nodes
}

fn agent_bin_name(config: &Config, agent_id: &str) -> String {
    config
        .agent
        .find_by_id(agent_id)
        .and_then(|(_, c)| c.path.clone())
        .map(|p| {
            Path::new(&p)
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or(p)
        })
        .unwrap_or_else(|| agent_id.to_string())
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// 节点上是否存在指定 agent。探测失败一律视为不存在。
pub async fn probe_agent(node: &NodeTarget, agent_id: &str, config: &Config) -> bool {
    if node.is_local {
        if !node.explicit_agent_path.is_empty() {
            let p = crate::api::uploads::expand_tilde(&node.explicit_agent_path);
            return Path::new(&p).exists();
        }
        let bin = agent_bin_name(config, agent_id);
        return std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("command -v {}", shell_quote(&bin)))
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
    }
    let cmd = if !node.explicit_agent_path.is_empty() {
        format!(
            "test -x {}",
            shell_quote(&crate::api::uploads::expand_tilde(&node.explicit_agent_path))
        )
    } else {
        format!("command -v {}", shell_quote(&agent_bin_name(config, agent_id)))
    };
    match run_ssh_capture(node, &cmd).await {
        Ok(out) => out.status.success(),
        Err(_) => false,
    }
}

/// 技能包工作目录在远端节点上的路径。
pub const REMOTE_SKILLS_WORKSPACE: &str = "~/.milletcode/runtime/skills";

/// 确保远端任意目录存在，返回解析后的绝对路径。
pub async fn ensure_remote_dir(node: &NodeTarget, dir: &str) -> Result<String> {
    let out = run_ssh_capture(
        node,
        &format!("mkdir -p {dir} && cd {dir} && pwd"),
    )
    .await?;
    if !out.status.success() {
        anyhow::bail!(
            "remote mkdir failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )
    }
    let dir = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if dir.is_empty() {
        anyhow::bail!("remote mkdir returned empty path");
    }
    Ok(dir)
}

// ---------------------------------------------------------------------------
// 安装验收标记（第三方安装 watcher 用）
// ---------------------------------------------------------------------------

/// 安装会话开始前的验收基准：会话结束后检查 agent 全局技能目录里
/// 是否出现了比标记更新的条目——有才算真的装进全局目录。
pub struct InstallMarker {
    /// 本地节点：标记文件路径（取 mtime 为基准）
    pub local_path: Option<PathBuf>,
    /// 远端节点：标记文件在远端的路径（~/ 形式，供 find -newer 用）
    pub remote_path: Option<String>,
}

/// 会话开始前在每个节点上放置验收标记。
pub async fn create_install_marker(
    node: &NodeTarget,
    run_id: &str,
    agent_id: &str,
) -> Result<InstallMarker> {
    if node.is_local {
        let path = std::env::temp_dir().join(format!(
            "millet-skill-marker-{}-{}",
            sanitize_segment(run_id),
            sanitize_segment(agent_id)
        ));
        tokio::fs::write(&path, b"").await?;
        return Ok(InstallMarker {
            local_path: Some(path),
            remote_path: None,
        });
    }
    let marker = format!("{REMOTE_SKILLS_WORKSPACE}/.marker-{}-{}", sanitize_segment(run_id), sanitize_segment(agent_id));
    let out = run_ssh_capture(
        node,
        &format!("mkdir -p {REMOTE_SKILLS_WORKSPACE} && touch {marker} && test -f {marker}"),
    )
    .await?;
    if !out.status.success() {
        anyhow::bail!(
            "remote marker setup failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(InstallMarker {
        local_path: None,
        remote_path: Some(marker),
    })
}

/// 本地验收：全局技能目录里是否有 mtime 晚于标记的条目。
fn verify_local_global_dir(base: &Path, marker: &Path) -> bool {
    let Ok(md) = std::fs::symlink_metadata(marker) else {
        return false;
    };
    let marker_time = md.modified().unwrap_or(std::time::UNIX_EPOCH);
    let Ok(rd) = std::fs::read_dir(base) else {
        return false;
    };
    rd.filter_map(|e| e.ok())
        .filter_map(|e| e.metadata().ok())
        .filter_map(|m| m.modified().ok())
        .any(|t| t > marker_time)
}

/// 会话结束后的验收：agent 全局技能目录相比标记时刻是否有新条目。
/// Err = 检查本身失败；Ok(()) = 验收通过；Ok 内 Err 文案可进留痕（已脱敏）。
pub async fn verify_global_install(
    node: &NodeTarget,
    agent_id: &str,
    marker: &InstallMarker,
) -> Result<(), String> {
    if node.is_local {
        let home = dirs::home_dir().ok_or_else(|| "HOME not set".to_string())?;
        let base = skills_dir_for_home(&home, agent_id);
        let Some(m) = marker.local_path.as_ref() else {
            return Err("verify_failed: missing local marker".to_string());
        };
        if verify_local_global_dir(&base, m) {
            return Ok(());
        }
        return Err("verify_failed: no new entry in global skills dir".to_string());
    }
    let Some(m) = marker.remote_path.as_ref() else {
        return Err("verify_failed: missing remote marker".to_string());
    };
    let dir = remote_skills_dir(agent_id);
    let cmd = format!(
        "if [ -d {d} ] && find {d} -mindepth 1 -maxdepth 1 -newer {m} 2>/dev/null | grep -q .; then echo VERIFY:YES; else echo VERIFY:NO; fi",
        d = dir,
        m = m,
    );
    match run_ssh_capture(node, &cmd).await {
        Ok(out) => {
            let s = String::from_utf8_lossy(&out.stdout);
            if s.contains("VERIFY:YES") {
                Ok(())
            } else {
                Err("verify_failed: no new entry in global skills dir".to_string())
            }
        }
        Err(_) => Err("verify_failed: target check failed".to_string()),
    }
}

// ---------------------------------------------------------------------------
// SSH 执行
// ---------------------------------------------------------------------------

async fn run_ssh_capture(node: &NodeTarget, remote_cmd: &str) -> Result<std::process::Output> {
    let mut cmd = tokio::process::Command::new("ssh");
    cmd.args(build_ssh_args(&node.hostname, &node.ssh_config_path));
    cmd.arg(remote_cmd);
    cmd.output().await.context("ssh exec failed")
}

async fn run_ssh_stdin(
    node: &NodeTarget,
    remote_cmd: &str,
    payload: &[u8],
) -> Result<std::process::Output> {
    use tokio::io::AsyncWriteExt;
    let mut cmd = tokio::process::Command::new("ssh");
    cmd.args(build_ssh_args(&node.hostname, &node.ssh_config_path));
    cmd.arg(remote_cmd);
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().context("ssh spawn failed")?;
    if let Some(mut stdin) = child.stdin.take() {
        // 守卫脚本遇到保护模式会不读 stdin 直接退出（EPIPE）。这不是失败——
        // 结果标记已经在 stdout 里，交由调用方解析；真正失败时 stderr 兜底。
        if stdin.write_all(payload).await.is_err() {
            // ignore; fall through to wait_with_output
        }
        stdin.flush().await.ok();
    } // stdin dropped → EOF
    child.wait_with_output().await.context("ssh wait failed")
}

// ---------------------------------------------------------------------------
// 目标目录
// ---------------------------------------------------------------------------

pub fn skills_dir_for_home(home: &Path, agent_id: &str) -> PathBuf {
    match agent_id {
        "claude" | "claude_code" => home.join(".claude").join("skills"),
        other => home.join(format!(".{other}")).join("skills"),
    }
}

pub fn local_skills_dir(agent_id: &str) -> Result<PathBuf> {
    Ok(skills_dir_for_home(
        &dirs::home_dir().context("HOME not set")?,
        agent_id,
    ))
}

pub fn remote_skills_dir(agent_id: &str) -> String {
    match agent_id {
        "claude" | "claude_code" => "~/.claude/skills".to_string(),
        other => format!("~.{other}/skills"),
    }
}

// ---------------------------------------------------------------------------
// 下载 / 解压 / 技能目录识别
// ---------------------------------------------------------------------------

pub async fn download_to(url: &str, dest: &Path) -> Result<u64> {
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;
    let resp = reqwest::get(url)
        .await
        .context("download request failed")?
        .error_for_status()
        .context("download request returned error status")?;
    let mut file = tokio::fs::File::create(dest)
        .await
        .with_context(|| format!("create temp file failed: {}", dest.display()))?;
    let mut stream = resp.bytes_stream();
    let mut total: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("download stream error")?;
        file.write_all(&chunk).await.context("write temp file failed")?;
        total += chunk.len() as u64;
    }
    file.flush().await.ok();
    file.sync_all().await.ok();
    Ok(total)
}

pub async fn extract_7z(archive: &Path, password: &str, dest: &Path) -> Result<()> {
    tokio::fs::create_dir_all(dest).await?;
    let archive = archive.to_path_buf();
    let dest = dest.to_path_buf();
    let password = password.to_string();
    tokio::task::spawn_blocking(move || {
        if password.is_empty() {
            sevenz_rust::decompress_file(&archive, &dest)
        } else {
            sevenz_rust::decompress_file_with_password(
                &archive,
                &dest,
                sevenz_rust::Password::from(password.as_str()),
            )
        }
    })
    .await
    .map_err(|e| anyhow!("extract task join failed: {e}"))?
    .map_err(|e| anyhow!("7z extract failed: {e}"))
}

/// 解压结果中找出技能目录：根目录直接含 SKILL.md，或一级子目录含 SKILL.md。
pub async fn find_skill_dirs(dir: &Path) -> Result<Vec<PathBuf>> {
    if dir.join("SKILL.md").is_file() {
        return Ok(vec![dir.to_path_buf()]);
    }
    let mut out = Vec::new();
    let mut rd = tokio::fs::read_dir(dir).await?;
    while let Some(e) = rd.next_entry().await? {
        let p = e.path();
        if p.is_dir() && p.join("SKILL.md").is_file() {
            out.push(p);
        }
    }
    out.sort();
    Ok(out)
}

/// 技能目录名 → 目标目录名（防路径穿越；与源目录名可能不同）。
pub fn sanitize_skill_name(dir: &Path) -> String {
    let raw = dir
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| "skill".to_string());
    let cleaned = sanitize_segment(&raw).to_lowercase();
    if cleaned.is_empty() {
        "skill".to_string()
    } else {
        cleaned
    }
}

// ---------------------------------------------------------------------------
// 安装 / 卸载（单节点）
// ---------------------------------------------------------------------------

/// 本地安装。返回 "installed" | "exists" | "protected"。
pub async fn install_local(base: &Path, src: &Path, name: &str, overwrite: bool) -> Result<&'static str> {
    let dest = base.join(name);
    match tokio::fs::symlink_metadata(&dest).await {
        Ok(md) if md.file_type().is_symlink() => return Ok("protected"),
        Ok(_) => {
            if !overwrite {
                return Ok("exists");
            }
            tokio::fs::remove_dir_all(&dest).await?;
        }
        Err(_) => {}
    }
    copy_dir_recursive(src, &dest).await?;
    Ok("installed")
}

/// 本地卸载。返回 "removed" | "absent" | "protected"。
pub async fn uninstall_local(base: &Path, name: &str) -> Result<&'static str> {
    let dest = base.join(name);
    match tokio::fs::symlink_metadata(&dest).await {
        Ok(md) if md.file_type().is_symlink() => Ok("protected"),
        Ok(_) => {
            tokio::fs::remove_dir_all(&dest).await?;
            Ok("removed")
        }
        Err(_) => Ok("absent"),
    }
}

/// 远程安装：本地打 tar → ssh stdin 管道 → 远端守卫脚本解包。
/// 守卫脚本在远端单次会话内完成软链接检查，避免先探测后删除的竞态。
/// 返回 "installed" | "exists" | "protected"。
pub async fn install_remote(
    src: &Path,
    name: &str,
    agent_id: &str,
    node: &NodeTarget,
    overwrite: bool,
) -> Result<&'static str> {
    let tmp = std::env::temp_dir().join(format!(
        "millet-skill-{}-{}.tar",
        sanitize_segment(name),
        uuid::Uuid::new_v4().simple()
    ));
    let parent = src.parent().context("skill dir has no parent")?;
    let tar_name = src
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .context("skill dir has no name")?;
    let status = tokio::process::Command::new("tar")
        .arg("-C")
        .arg(parent)
        .arg("-cf")
        .arg(&tmp)
        .arg(&tar_name)
        .status()
        .await
        .context("local tar failed")?;
    anyhow::ensure!(status.success(), "local tar failed");
    let payload = tokio::fs::read(&tmp).await?;
    let _ = tokio::fs::remove_file(&tmp).await;

    // 远端脚本用 $HOME 而非 ~（~ 在单引号内不展开）
    let dir = remote_skills_dir(agent_id)
        .strip_prefix("~/")
        .map(|rest| format!("$HOME/{rest}"))
        .unwrap_or_else(|| remote_skills_dir(agent_id));
    let script = format!(
        "d=\"{dir}/{name}\"; if [ -L \"$d\" ]; then echo GUARD:LINK; exit 0; fi; \
         mkdir -p \"{dir}\"; \
         if [ -e \"$d\" ]; then if [ '{ow}' = '1' ]; then rm -rf \"$d\"; else echo GUARD:EXISTS; exit 0; fi; fi; \
         tar -xf - -C \"{dir}\" && echo GUARD:INSTALLED",
        dir = dir,
        name = name,
        ow = if overwrite { "1" } else { "0" },
    );
    let out = run_ssh_stdin(node, &script, &payload).await?;
    parse_guard_outcome_scrubbed(&out, &node.hostname)
}

/// 远程卸载，同样带软链接保护。
pub async fn uninstall_remote(name: &str, agent_id: &str, node: &NodeTarget) -> Result<&'static str> {
    let dir = remote_skills_dir(agent_id)
        .strip_prefix("~/")
        .map(|rest| format!("$HOME/{rest}"))
        .unwrap_or_else(|| remote_skills_dir(agent_id));
    let script = format!(
        "d=\"{dir}/{name}\"; if [ -L \"$d\" ]; then echo GUARD:LINK; \
         elif [ -e \"$d\" ]; then rm -rf \"$d\" && echo GUARD:REMOVED; else echo GUARD:ABSENT; fi",
        dir = dir,
        name = name,
    );
    let out = run_ssh_capture(node, &script).await?;
    parse_guard_outcome_scrubbed(&out, &node.hostname)
}

fn parse_guard_outcome(out: &std::process::Output) -> Result<&'static str> {
    parse_guard_outcome_scrubbed(out, "")
}

/// 解析远端守卫脚本输出。失败原因会进入 API 响应（前端可见），
/// 因此按红线脱敏：不出现 hostname / "remote" 字样。
fn parse_guard_outcome_scrubbed(out: &std::process::Output, hostname: &str) -> Result<&'static str> {
    let stdout = String::from_utf8_lossy(&out.stdout);
    if stdout.contains("GUARD:LINK") {
        return Ok("protected");
    }
    if stdout.contains("GUARD:EXISTS") {
        return Ok("exists");
    }
    if stdout.contains("GUARD:ABSENT") {
        return Ok("absent");
    }
    if stdout.contains("GUARD:REMOVED") {
        return Ok("removed");
    }
    if out.status.success() && stdout.contains("GUARD:INSTALLED") {
        return Ok("installed");
    }
    let mut detail = String::from_utf8_lossy(&out.stderr).into_owned();
    if !hostname.is_empty() {
        detail = detail.replace(hostname, "[host]");
    }
    let detail = detail.replace("remote", "[r]").replace("Remote", "[R]");
    let detail: String = detail.chars().take(160).collect::<String>().replace('\n', " ");
    if detail.is_empty() {
        Err(anyhow!("target operation failed (exit status {:?})", out.status.code()))
    } else {
        Err(anyhow!("target operation failed: {}", detail))
    }
}

async fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    tokio::fs::create_dir_all(dst).await?;
    let mut rd = tokio::fs::read_dir(src).await?;
    let mut pending: Vec<(PathBuf, PathBuf)> = Vec::new();
    while let Some(e) = rd.next_entry().await? {
        let ft = e.file_type().await?;
        let target = dst.join(e.file_name());
        if ft.is_dir() {
            pending.push((e.path(), target));
        } else if ft.is_file() {
            tokio::fs::copy(e.path(), target).await?;
        }
        // 源内软链接一律跳过（第三方包安全边界）
    }
    for (s, d) in pending {
        Box::pin(copy_dir_recursive(&s, &d)).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 任务表
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Running,
    Success,
    Partial,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct NodeReport {
    pub node: String,
    pub ok: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SkillTask {
    pub id: String,
    pub kind: String,
    pub skill: String,
    pub status: TaskStatus,
    pub total_nodes: usize,
    pub done_nodes: usize,
    pub per_node: Vec<NodeReport>,
    pub summary: String,
    pub created_at: String,
}

/// 最近 20 条任务的内存表。
#[derive(Default)]
pub struct SkillTaskRegistry(Mutex<Vec<SkillTask>>);

impl SkillTaskRegistry {
    pub fn push(&self, task: SkillTask) {
        let mut g = self.0.lock();
        g.push(task);
        let excess = g.len().saturating_sub(20);
        if excess > 0 {
            g.drain(0..excess);
        }
    }
    pub fn update(&self, id: &str, f: impl FnOnce(&mut SkillTask)) {
        if let Some(t) = self.0.lock().iter_mut().find(|t| t.id == id) {
            f(t);
        }
    }
    pub fn get(&self, id: &str) -> Option<SkillTask> {
        self.0.lock().iter().find(|t| t.id == id).cloned()
    }
}

/// 逐节点执行一次技能分发的完整流程（探测 + 守卫安装/卸载）。
/// kind: "install" | "uninstall"。日志只打计数，不打节点身份。
/// 返回任务终态。
pub async fn run_skill_task(
    registry: Arc<SkillTaskRegistry>,
    task_id: String,
    kind: &str,
    skill_name: &str,
    nodes: Vec<NodeTarget>,
    config: Config,
    agent_id: &str,
    overwrite: bool,
    src_dirs: Vec<PathBuf>,
) -> TaskStatus {
    let total = nodes.len();
    registry.update(&task_id, |t| {
        t.total_nodes = total;
        t.per_node = nodes.iter().map(|n| NodeReport {
            node: n.label.clone(),
            ok: false,
            reason: Some("pending".to_string()),
        }).collect();
    });

    let mut reports: Vec<NodeReport> = Vec::new();
    for node in &nodes {
        if !probe_agent(node, agent_id, &config).await {
            reports.push(NodeReport {
                node: node.label.clone(),
                ok: true,
                reason: Some("skipped_no_agent".to_string()),
            });
            continue;
        }
        let outcome: Result<&'static str, String> = if kind == "uninstall" {
            if node.is_local {
                local_uninstall_report(skill_name, agent_id).await
            } else {
                uninstall_remote(skill_name, agent_id, node).await.map_err(|e| format!("{:#}", e))
            }
        } else {
            // 归档根目录平铺（SKILL.md 直接在根）时 src_dirs 只有一项，
            // 目录名是解压临时名而非技能名——单目录时直接采用。
            let src = src_dirs
                .iter()
                .find(|d| sanitize_skill_name(d) == skill_name)
                .or_else(|| if src_dirs.len() == 1 { src_dirs.first() } else { None });
            match src {
                Some(src) => {
                    if node.is_local {
                        match local_skills_dir(agent_id) {
                            Ok(base) => install_local(&base, src, skill_name, overwrite)
                                .await
                                .map_err(|e| format!("{:#}", e)),
                            Err(e) => Err(format!("{:#}", e)),
                        }
                    } else {
                        install_remote(src, skill_name, agent_id, node, overwrite)
                            .await
                            .map_err(|e| format!("{:#}", e))
                    }
                }
                None => Err("skill dir not found in archive".to_string()),
            }
        };
        match outcome {
            Ok("installed") | Ok("removed") => reports.push(NodeReport {
                node: node.label.clone(),
                ok: true,
                reason: None,
            }),
            Ok(other) => reports.push(NodeReport {
                node: node.label.clone(),
                ok: true,
                reason: Some(other.to_string()),
            }),
            Err(detail) => reports.push(NodeReport {
                node: node.label.clone(),
                ok: false,
                reason: Some(detail),
            }),
        }
    }

    let ok_count = reports.iter().filter(|r| r.ok).count();
    let fail_count = total - ok_count;
    let status = if fail_count == 0 {
        TaskStatus::Success
    } else if ok_count > 0 {
        TaskStatus::Partial
    } else {
        TaskStatus::Failed
    };
    let summary = format!(
        "{} nodes: {} ok, {} failed/skipped-idle",
        total, ok_count, fail_count
    );
    tracing::info!(
        "skill task {} ({}) for '{}' finished: {}/{} nodes ok",
        task_id, kind, skill_name, ok_count, total
    );
    registry.update(&task_id, |t| {
        t.status = status.clone();
        t.done_nodes = total;
        t.per_node = reports;
        t.summary = summary;
    });
    status
}

async fn local_uninstall_report(name: &str, agent_id: &str) -> Result<&'static str, String> {
    match local_skills_dir(agent_id) {
        Ok(base) => uninstall_local(&base, name).await.map_err(|e| format!("{:#}", e)),
        Err(e) => Err(format!("{:#}", e)),
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_parses_with_entries() {
        let m = load_manifest().expect("manifest should parse");
        assert!(!m.skills.is_empty(), "manifest should list at least one skill");
        for s in &m.skills {
            assert!(!s.name.is_empty());
            assert!(s.icon.contains("<svg"));
        }
        let git = m.skills.iter().find(|s| s.name == "git").expect("git entry");
        assert!(!git.description.is_empty());
    }

    /// 清单随二进制发布、对外可见：下载地址与解压密码只能来自 config.toml，
    /// 这里防回归，避免任何一处改动把两者带回流（注释提及字段名、SVG xmlns
    /// 命名空间声明不算）。
    #[test]
    fn manifest_contains_no_urls_or_passwords() {
        let lower = MANIFEST_TOML
            .to_lowercase()
            .replace("xmlns=\"http://www.w3.org/2000/svg\"", "");
        assert!(!lower.contains("password ="), "manifest must not embed password assignments");
        assert!(!lower.contains("http://") && !lower.contains("https://"), "manifest must not embed download URLs");
    }

    #[tokio::test]
    async fn find_skill_dirs_root_and_children() {
        let tmp = std::env::temp_dir().join(format!("millet-skilltest-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(tmp.join("child-a")).await.unwrap();
        tokio::fs::create_dir_all(tmp.join("child-b")).await.unwrap();
        tokio::fs::write(tmp.join("SKILL.md"), "# root").await.unwrap();
        tokio::fs::write(tmp.join("child-a").join("SKILL.md"), "# a").await.unwrap();
        tokio::fs::write(tmp.join("child-b").join("README.md"), "b").await.unwrap();

        let dirs = find_skill_dirs(&tmp).await.unwrap();
        // root wins immediately
        assert_eq!(dirs.len(), 1);
        assert_eq!(dirs[0], tmp);

        // no root SKILL.md → children with SKILL.md only
        tokio::fs::remove_file(tmp.join("SKILL.md")).await.unwrap();
        let dirs = find_skill_dirs(&tmp).await.unwrap();
        assert_eq!(dirs, vec![tmp.join("child-a")]);

        // no SKILL.md anywhere → empty
        tokio::fs::remove_file(tmp.join("child-a").join("SKILL.md")).await.unwrap();
        let dirs = find_skill_dirs(&tmp).await.unwrap();
        assert!(dirs.is_empty());

        tokio::fs::remove_dir_all(&tmp).await.unwrap();
    }

    #[test]
    fn skill_names_are_always_lowercase() {
        assert_eq!(sanitize_skill_name(Path::new("Git")), "git");
        assert_eq!(sanitize_skill_name(Path::new("Millet-Msg")), "millet-msg");
        assert_eq!(sanitize_skill_name(Path::new("millet-msg")), "millet-msg");
    }

    #[tokio::test]
    async fn install_local_overwrite_and_symlink_protection() {
        let tmp = std::env::temp_dir().join(format!("millet-skilltest-{}", uuid::Uuid::new_v4()));
        let base = tmp.join("skills");
        let src = tmp.join("src").join("demo");
        tokio::fs::create_dir_all(&src).await.unwrap();
        tokio::fs::write(src.join("SKILL.md"), "v2").await.unwrap();
        tokio::fs::create_dir_all(&base).await.unwrap();

        // fresh install
        assert_eq!(install_local(&base, &src, "demo", true).await.unwrap(), "installed");
        let installed = tokio::fs::read_to_string(base.join("demo").join("SKILL.md")).await.unwrap();
        assert_eq!(installed, "v2");

        // overwrite=false on existing → "exists", content untouched
        tokio::fs::write(src.join("SKILL.md"), "v3").await.unwrap();
        assert_eq!(install_local(&base, &src, "demo", false).await.unwrap(), "exists");
        let installed = tokio::fs::read_to_string(base.join("demo").join("SKILL.md")).await.unwrap();
        assert_eq!(installed, "v2");

        // overwrite=true replaces
        assert_eq!(install_local(&base, &src, "demo", true).await.unwrap(), "installed");
        let installed = tokio::fs::read_to_string(base.join("demo").join("SKILL.md")).await.unwrap();
        assert_eq!(installed, "v3");

        // symlink target → "protected", pointed-to dir untouched
        let precious = tmp.join("precious");
        tokio::fs::create_dir_all(&precious).await.unwrap();
        tokio::fs::write(precious.join("keep.txt"), "do not touch").await.unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&precious, base.join("linked")).unwrap();
        assert_eq!(install_local(&base, &src, "linked", true).await.unwrap(), "protected");
        let keep = tokio::fs::read_to_string(precious.join("keep.txt")).await.unwrap();
        assert_eq!(keep, "do not touch");

        // uninstall symlink → "protected", target survives
        assert_eq!(uninstall_local(&base, "linked").await.unwrap(), "protected");
        assert!(precious.join("keep.txt").exists());

        // uninstall real dir → "removed"; uninstall missing → "absent"
        assert_eq!(uninstall_local(&base, "demo").await.unwrap(), "removed");
        assert!(!base.join("demo").exists());
        assert_eq!(uninstall_local(&base, "demo").await.unwrap(), "absent");

        tokio::fs::remove_dir_all(&tmp).await.unwrap();
    }

    #[tokio::test]
    async fn guard_outcome_parsing() {
        use std::os::unix::process::ExitStatusExt;
        let mk = |code: i32, stdout: &str| std::process::Output {
            status: std::process::ExitStatus::from_raw(code),
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        };
        assert_eq!(parse_guard_outcome(&mk(0, "GUARD:LINK\n")).unwrap(), "protected");
        assert_eq!(parse_guard_outcome(&mk(0, "GUARD:EXISTS\n")).unwrap(), "exists");
        assert_eq!(parse_guard_outcome(&mk(0, "GUARD:ABSENT\n")).unwrap(), "absent");
        assert_eq!(parse_guard_outcome(&mk(0, "GUARD:REMOVED\n")).unwrap(), "removed");
        assert_eq!(parse_guard_outcome(&mk(0, "GUARD:INSTALLED\n")).unwrap(), "installed");
        assert!(parse_guard_outcome(&mk(1, "some error")).is_err());
    }
}
