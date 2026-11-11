use axum::{extract::{Query, State}, Json};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::AppState;

#[derive(Serialize)]
pub struct SlashCommand {
    pub name: String,
    pub description: String,
}

#[derive(Deserialize)]
pub struct SlashCommandsQuery {
    /// Agent id to load skills for (e.g. "claude")
    pub agent_id: Option<String>,
    /// Alias for agent_id (legacy)
    pub agent_type: Option<String>,
}

/// Get the skills directory for a given agent type
fn get_skills_dir(agent_id: &str) -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    match agent_id {
        "claude" | "claude_code" => Some(format!("{}/.claude/skills", home)),
        // Other agents can be added here
        _ => None,
    }
}

/// Scan a skills directory and return slash commands
fn scan_skills_dir(dir: &str) -> Vec<SlashCommand> {
    let mut commands = Vec::new();

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return commands,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        // Skill name is the directory name
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            // Try to read description from SKILL.md or README.md
            let description = read_skill_description(&path)
                .unwrap_or_else(|| format!("Skill: {}", name));

            commands.push(SlashCommand {
                name: format!("/{}", name),
                description,
            });
        }
    }

    commands.sort_by(|a, b| a.name.cmp(&b.name));
    commands
}

/// Read the first non-empty line from SKILL.md or README.md as description
fn read_skill_description(skill_dir: &std::path::Path) -> Option<String> {
    for filename in &["SKILL.md", "README.md"] {
        let file_path = skill_dir.join(filename);
        if let Ok(content) = std::fs::read_to_string(&file_path) {
            // Find first non-empty, non-heading line
            for line in content.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    continue;
                }
                // Take first 100 chars as description (char-based to handle UTF-8)
                let desc: String = trimmed.chars().take(100).collect();
                let desc = if trimmed.chars().count() > 100 {
                    format!("{}...", desc)
                } else {
                    desc
                };
                return Some(desc);
            }
        }
    }
    None
}

pub async fn list_slash_commands(
    State(state): State<Arc<AppState>>,
    Query(query): Query<SlashCommandsQuery>,
) -> Json<Vec<SlashCommand>> {
    let is_debug = state.env == "debug";

    let mut commands = vec![
        SlashCommand {
            name: "/help".to_string(),
            description: "显示帮助信息".to_string(),
        },
        SlashCommand {
            name: "/clear".to_string(),
            description: "清除对话历史".to_string(),
        },
        SlashCommand {
            name: "/compact".to_string(),
            description: "压缩对话上下文".to_string(),
        },
        SlashCommand {
            name: "/model".to_string(),
            description: "切换模型".to_string(),
        },
        SlashCommand {
            name: "/doctor".to_string(),
            description: "检查环境配置".to_string(),
        },
        SlashCommand {
            name: "/vim".to_string(),
            description: "切换 vim 模式".to_string(),
        },
    ];

    // Debug-only commands
    if is_debug {
        commands.push(SlashCommand {
            name: "/config".to_string(),
            description: "查看/修改配置 (Debug)".to_string(),
        });
    }

    // Add skills from agent's skills directory
    let agent_id = query.agent_id.as_ref().or(query.agent_type.as_ref());
    if let Some(agent_id) = agent_id {
        if let Some(skills_dir) = get_skills_dir(agent_id) {
            let skills = scan_skills_dir(&skills_dir);
            commands.extend(skills);
        }
    }

    Json(commands)
}
