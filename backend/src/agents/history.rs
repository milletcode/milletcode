use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// A single message turn parsed from JSONL
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryMessage {
    pub role: String,
    #[serde(rename = "type")]
    pub msg_type: String,
    pub content: String,
    pub timestamp: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    #[serde(rename = "parentUuid")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_uuid: Option<String>,
}

/// Paginated history response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryResponse {
    pub messages: Vec<HistoryMessage>,
    pub has_more: bool,
    /// smartcode 会话当前生效的模型（取自最新一条带 model 的 meta 行）；
    /// claude 会话为 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Read and parse Claude's JSONL conversation history file with pagination.
///
/// Path: ~/.claude/projects/{working_dir.replace('/', '-')}/{session_uuid}.jsonl
///
/// `offset` = number of messages from the end to skip (0 = most recent first)
/// `limit` = max messages to return
/// Returns newest messages first (reversed chronological order).
pub fn read_session_history(
    session_uuid: &str,
    working_dir: &str,
    offset: usize,
    limit: usize,
) -> Result<HistoryResponse> {
    let path = jsonl_path(session_uuid, working_dir)?;
    if !path.exists() {
        return Ok(HistoryResponse {
            messages: Vec::new(),
            has_more: false,
            model: None,
        });
    }

    let content = fs::read_to_string(&path)?;
    parse_jsonl_history(&content, offset, limit)
}

/// Parse Claude's JSONL conversation history (shared by local and remote/SSH reads).
pub fn parse_jsonl_history(content: &str, offset: usize, limit: usize) -> Result<HistoryResponse> {

    // Collect all messages with timestamps
    let mut messages: Vec<(u64, HistoryMessage)> = Vec::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let value: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let Some(event_type) = value.get("type").and_then(|v| v.as_str()) else {
            continue;
        };

        let timestamp = value
            .get("timestamp")
            .and_then(|v| v.as_str())
            .map(|s| {
                chrono::DateTime::parse_from_rfc3339(s)
                    .map(|dt| dt.timestamp_millis() as u64)
                    .unwrap_or(0)
            })
            .unwrap_or(0);

        let user_type = value
            .get("userType")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let uuid = value
            .get("uuid")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let parent_uuid = value
            .get("parentUuid")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let msg = match event_type {
            "user" => parse_user_content(&value, timestamp),
            "assistant" => parse_assistant_content(&value, timestamp),
            _ => None,
        };

        if let Some(mut msg) = msg {
            if !msg.content.is_empty() {
                msg.user_type = user_type;
                msg.uuid = uuid;
                msg.parent_uuid = parent_uuid;
                messages.push((timestamp, msg));
            }
        }
    }

    // Sort by timestamp ascending (chronological order)
    messages.sort_by_key(|(ts, _)| *ts);

    let total = messages.len();

    // Apply pagination from the newest end (offset=0 means skip newest messages)
    // Reverse to get newest-first for pagination calculation
    messages.reverse();

    let start = offset;
    let end = (start + limit).min(total);
    let has_more = end < total;

    let page: Vec<HistoryMessage> = if start < total {
        messages[start..end]
            .iter()
            .map(|(_, msg)| msg.clone())
            .collect()
    } else {
        Vec::new()
    };

    // Reverse back to chronological order (oldest first) for display
    let mut page = page;
    page.reverse();

    Ok(HistoryResponse {
        messages: page,
        has_more,
        model: None,
    })
}

/// Read and parse a smartcode session JSONL (absolute path reported via stderr).
pub fn read_smartcode_session_history(
    session_path: &str,
    offset: usize,
    limit: usize,
) -> Result<HistoryResponse> {
    let path = PathBuf::from(session_path);
    if !path.exists() {
        return Ok(HistoryResponse {
            messages: Vec::new(),
            has_more: false,
            model: None,
        });
    }
    let content = fs::read_to_string(&path)?;
    parse_smartcode_history(&content, offset, limit)
}

/// Parse smartcode's session JSONL (shared by local and remote/SFTP reads).
///
/// Line shapes (smart-core session.rs):
/// `{"v":1,"record":"message","message":{"role":"user|assistant","content":[blocks]}}`
/// `{"v":1,"record":"meta",...}` — skipped.
///
/// smartcode lines carry no timestamps; the line index stands in for one so
/// pagination and any downstream sorting stay stable.
pub fn parse_smartcode_history(content: &str, offset: usize, limit: usize) -> Result<HistoryResponse> {
    let mut messages: Vec<(u64, HistoryMessage)> = Vec::new();
    let mut current_model: Option<String> = None;

    for (idx, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value.get("record").and_then(|r| r.as_str()) != Some("message") {
            // meta 行不参与重放，但记录最新 model —— 前端据此显示当前模型
            if let Some(m) = value.pointer("/meta/model").and_then(|m| m.as_str()) {
                if !m.is_empty() {
                    current_model = Some(m.to_string());
                }
            }
            continue;
        }
        let Some(msg_value) = value.get("message") else {
            continue;
        };
        let Some(role) = msg_value.get("role").and_then(|r| r.as_str()) else {
            continue;
        };
        let Some(blocks) = msg_value.get("content").and_then(|c| c.as_array()) else {
            continue;
        };

        // Line index as synthetic timestamp (ms) — preserves order, sorts stably
        let timestamp = (idx as u64).wrapping_mul(1000);
        // Frontend uses uuid to build unique per-block keys (Svelte each key);
        // duplicates crash rendering with each_key_duplicate.
        let uuid = format!("smartcode-{}", idx);

        let msg = match role {
            "user" => parse_smartcode_user_content(blocks, timestamp, &uuid),
            "assistant" => parse_smartcode_assistant_content(blocks, timestamp, &uuid),
            _ => None,
        };

        if let Some(msg) = msg {
            if !msg.content.is_empty() {
                messages.push((timestamp, msg));
            }
        }
    }

    let total = messages.len();
    messages.reverse();
    let start = offset;
    let end = (start + limit).min(total);
    let has_more = end < total;

    let mut page: Vec<HistoryMessage> = if start < total {
        messages[start..end].iter().map(|(_, m)| m.clone()).collect()
    } else {
        Vec::new()
    };
    page.reverse();

    Ok(HistoryResponse {
        messages: page,
        has_more,
        model: current_model,
    })
}

/// smartcode user 消息：text block → 用户文本；tool_result → 回传标记。
/// 仅含 tool_result（无用户文本）时与 claude 解析一致，视为 agent 输出。
fn parse_smartcode_user_content(blocks: &[serde_json::Value], timestamp: u64, uuid: &str) -> Option<HistoryMessage> {
    let mut texts: Vec<String> = Vec::new();
    let mut has_user_text = false;

    for block in blocks {
        let ct = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match ct {
            "text" => {
                let text = block.get("text").and_then(|t| t.as_str()).unwrap_or("");
                if !text.trim().is_empty() {
                    has_user_text = true;
                    texts.push(text.to_string());
                }
            }
            "tool_result" => {
                let content_str = smartcode_tool_result_text(block);
                if content_str.is_empty() {
                    continue;
                }
                let tool_use_id = block.get("tool_use_id").and_then(|i| i.as_str()).unwrap_or("");
                if tool_use_id.is_empty() {
                    texts.push(format!("[tool_result]{}[/tool_result]", content_str));
                } else {
                    texts.push(format!(
                        "[tool_result tool_use_id=\"{}\"]{}[/tool_result]",
                        tool_use_id, content_str
                    ));
                }
            }
            _ => {}
        }
    }

    if texts.is_empty() {
        None
    } else if has_user_text {
        Some(HistoryMessage {
            role: "user".to_string(),
            msg_type: "user".to_string(),
            content: texts.join("\n"),
            timestamp,
            user_type: None,
            uuid: Some(uuid.to_string()),
            parent_uuid: None,
        })
    } else {
        Some(HistoryMessage {
            role: "assistant".to_string(),
            msg_type: "assistant".to_string(),
            content: texts.join("\n"),
            timestamp,
            user_type: None,
            uuid: Some(uuid.to_string()),
            parent_uuid: None,
        })
    }
}

/// smartcode assistant 消息：text / thinking / tool_use blocks。
fn parse_smartcode_assistant_content(blocks: &[serde_json::Value], timestamp: u64, uuid: &str) -> Option<HistoryMessage> {
    let mut texts: Vec<String> = Vec::new();

    for block in blocks {
        let ct = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match ct {
            "text" => {
                let text = block.get("text").and_then(|t| t.as_str()).unwrap_or("");
                if !text.is_empty() {
                    texts.push(text.to_string());
                }
            }
            "thinking" => {
                let text = block.get("thinking").and_then(|t| t.as_str()).unwrap_or("");
                if !text.is_empty() {
                    texts.push(format!("[thinking]{}[/thinking]", text));
                }
            }
            "tool_use" => {
                let id = block.get("id").and_then(|i| i.as_str()).unwrap_or("");
                let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("unknown");
                let input = block
                    .get("input")
                    .map(|v| serde_json::to_string(v).unwrap_or_default())
                    .unwrap_or_default();
                if id.is_empty() {
                    texts.push(format!("[tool_use]{} {}[/tool_use]", name, input));
                } else {
                    texts.push(format!("[tool_use id=\"{}\"]{} {}[/tool_use]", id, name, input));
                }
            }
            _ => {}
        }
    }

    if texts.is_empty() {
        None
    } else {
        Some(HistoryMessage {
            role: "assistant".to_string(),
            msg_type: "assistant".to_string(),
            content: texts.join("\n"),
            timestamp,
            user_type: None,
            uuid: Some(uuid.to_string()),
            parent_uuid: None,
        })
    }
}

/// tool_result content：纯文本或 block 数组（取其中 text block 拼接）。
fn smartcode_tool_result_text(block: &serde_json::Value) -> String {
    match block.get("content") {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .filter_map(|elem| elem.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn jsonl_path(session_uuid: &str, working_dir: &str) -> Result<PathBuf> {
    // Claude Code creates JSONL files in ~/.claude/projects/{working_dir_slug}/
    // where working_dir_slug is the absolute path with '/' replaced by '-'
    let working_dir_slug = working_dir.replace('/', "-");

    let path = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
        .join("projects")
        .join(&working_dir_slug)
        .join(format!("{}.jsonl", session_uuid));

    tracing::debug!(
        session_uuid = %session_uuid,
        working_dir = %working_dir,
        working_dir_slug = %working_dir_slug,
        path = %path.display(),
        "jsonl_path constructed"
    );

    Ok(path)
}

fn parse_user_content(value: &serde_json::Value, timestamp: u64) -> Option<HistoryMessage> {
    let content = value.get("message").and_then(|m| m.get("content"))?;

    // In Claude's JSONL, user messages have content as a string, not an array
    if let Some(text) = content.as_str() {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        return Some(HistoryMessage {
            role: "user".to_string(),
            msg_type: "user".to_string(),
            content: text.to_string(),
            timestamp,
            user_type: None,
            uuid: None,
            parent_uuid: None,
        });
    }

    let parts = content.as_array()?;

    let texts: Vec<String> = parts
        .iter()
        .filter_map(|p| {
            let ct = p.get("type").and_then(|t| t.as_str())?;
            match ct {
                "text" => p.get("text").and_then(|t| t.as_str()).map(|s| s.to_string()),
                "tool_result" => {
                    let is_error = p.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false);
                    let content_val = p.get("content").and_then(|c| c.as_str());
                    // Try nested array content: [{"type":"text","text":"..."}]
                    let content_str = match content_val {
                        Some(s) => {
                            // Skip the SDK's auto-error response to AskUserQuestion
                            if is_error && s == "Answer questions?" {
                                return None;
                            }
                            s.to_string()
                        }
                        None => {
                            let nested = p.get("content").and_then(|c| c.as_array());
                            match nested {
                                Some(arr) => {
                                    let inner: Vec<String> = arr.iter()
                                        .filter_map(|elem| {
                                            elem.get("text").and_then(|t| t.as_str()).map(|s| s.to_string())
                                        })
                                        .collect();
                                    if inner.is_empty() {
                                        return None;
                                    }
                                    inner.join("\n")
                                }
                                None => return None,
                            }
                        }
                    };
                    let tool_use_id = p.get("tool_use_id").and_then(|i| i.as_str()).unwrap_or("");
                    if tool_use_id.is_empty() {
                        Some(format!("[tool_result]{}[/tool_result]", content_str))
                    } else {
                        Some(format!("[tool_result tool_use_id=\"{}\"]{}[/tool_result]", tool_use_id, content_str))
                    }
                }
                _ => None,
            }
        })
        .collect();

    if texts.is_empty() {
        None
    } else {
        // If the message contains ONLY tool_result (no user text), this is
        // Claude feeding tool results back to itself — treat as agent output.
        let has_only_tool_results = parts.iter().all(|p| {
            p.get("type").and_then(|t| t.as_str()) == Some("tool_result")
        });
        let has_user_text = parts.iter().any(|p| {
            p.get("type").and_then(|t| t.as_str()) == Some("text")
        });

        if has_only_tool_results && !has_user_text {
            Some(HistoryMessage {
                role: "assistant".to_string(),
                msg_type: "assistant".to_string(),
                content: texts.join("\n"),
                timestamp,
                user_type: None,
                uuid: None,
                parent_uuid: None,
            })
        } else {
            Some(HistoryMessage {
                role: "user".to_string(),
                msg_type: "user".to_string(),
                content: texts.join("\n"),
                timestamp,
                user_type: None,
                uuid: None,
                parent_uuid: None,
            })
        }
    }
}

fn parse_assistant_content(value: &serde_json::Value, timestamp: u64) -> Option<HistoryMessage> {
    let content = value
        .get("message")
        .and_then(|m| m.get("content"))?;
    let parts = content.as_array()?;

    let texts: Vec<String> = parts
        .iter()
        .filter_map(|p| {
            let ct = p.get("type").and_then(|t| t.as_str())?;
            match ct {
                "text" => p.get("text").and_then(|t| t.as_str()).map(|s| s.to_string()),
                "thinking" => p.get("thinking").and_then(|t| t.as_str()).map(|s| format!("[thinking]{}[/thinking]", s)),
                "tool_use" => {
                    let id = p.get("id").and_then(|i| i.as_str()).unwrap_or("");
                    let name = p.get("name").and_then(|n| n.as_str()).unwrap_or("unknown");
                    if name == "AskUserQuestion" {
                        // id 必须嵌入：前端 historyParser 靠它把卡片与 userQuestions store
                        // 里的 pending tool_use_id 配对，丢了 id 刷新后就配不上、提交按钮消失
                        let input_json = p.get("input").map(|v| serde_json::to_string(v).unwrap_or_default()).unwrap_or_default();
                        return Some(if id.is_empty() {
                            format!("[ask_user_question]{}[/ask_user_question]", input_json)
                        } else {
                            format!("[ask_user_question id=\"{}\"]{}[/ask_user_question]", id, input_json)
                        });
                    }
                    let input = p.get("input").map(|v| serde_json::to_string(v).unwrap_or_default()).unwrap_or_default();
                    if id.is_empty() {
                        Some(format!("[tool_use]{} {}[/tool_use]", name, input))
                    } else {
                        Some(format!("[tool_use id=\"{}\"]{} {}[/tool_use]", id, name, input))
                    }
                }
                _ => None,
            }
        })
        .collect();

    if texts.is_empty() {
        None
    } else {
        Some(HistoryMessage {
            role: "assistant".to_string(),
            msg_type: "assistant".to_string(),
            content: texts.join("\n"),
            timestamp,
            user_type: None,
            uuid: None,
            parent_uuid: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::parse_assistant_content;

    // AskUserQuestion 打平必须嵌入 tool_use id：前端把卡片 toolUseId 与
    // userQuestions store 的 pending tool_use_id 配对来决定「提交答案」按钮，
    // id 丢失时刷新/重进会话后按钮消失、卡片误显「已作答」。
    #[test]
    fn ask_user_question_marker_embeds_tool_use_id() {
        let value = json!({
            "message": {
                "content": [
                    {"type": "text", "text": "need a decision"},
                    {"type": "tool_use", "id": "toolu_01ABC", "name": "AskUserQuestion", "input": {"questions": []}}
                ]
            }
        });
        let msg = parse_assistant_content(&value, 0).expect("message parsed");
        assert!(
            msg.content.contains("[ask_user_question id=\"toolu_01ABC\"]"),
            "marker must embed id, got: {}",
            msg.content
        );
        assert!(msg.content.contains("[/ask_user_question]"));
        // 普通文本块照常保留
        assert!(msg.content.contains("need a decision"));
    }

    #[test]
    fn ask_user_question_marker_without_id_stays_bare() {
        let value = json!({
            "message": {
                "content": [
                    {"type": "tool_use", "name": "AskUserQuestion", "input": {"questions": []}}
                ]
            }
        });
        let msg = parse_assistant_content(&value, 0).expect("message parsed");
        assert!(
            msg.content.contains("[ask_user_question]{"),
            "missing id must not fabricate an attribute, got: {}",
            msg.content
        );
    }

    #[test]
    fn plain_tool_use_marker_format_unchanged() {
        let value = json!({
            "message": {
                "content": [
                    {"type": "tool_use", "id": "toolu_02", "name": "Bash", "input": {"command": "ls"}}
                ]
            }
        });
        let msg = parse_assistant_content(&value, 0).expect("message parsed");
        assert!(msg.content.contains("[tool_use id=\"toolu_02\"]Bash"));
    }

    // ---- smartcode 格式 ----

    use super::{parse_smartcode_history, smartcode_tool_result_text};

    fn smartcode_line(role: &str, content: serde_json::Value) -> String {
        serde_json::json!({"v":1, "record":"message", "message":{"role":role, "content":content}})
            .to_string()
    }

    #[test]
    fn smartcode_history_parses_user_and_assistant_messages() {
        let content = [
            smartcode_line(
                "user",
                serde_json::json!([{"type":"text","text":"帮我看看这个文件"}]),
            ),
            smartcode_line(
                "assistant",
                serde_json::json!([
                    {"type":"thinking","thinking":"先读文件","signature":"sig"},
                    {"type":"text","text":"好的"},
                    {"type":"tool_use","id":"t1","name":"Read","input":{"path":"/a"}}
                ]),
            ),
            smartcode_line(
                "user",
                serde_json::json!([
                    {"type":"tool_result","tool_use_id":"t1","content":"file content","is_error":false}
                ]),
            ),
            r#"{"v":1,"record":"meta","meta":{"event":"turn_finished","model":"m","turn":1,"usage":{}}}"#.to_string(),
            "not json".to_string(),
        ]
        .join("\n");

        let resp = parse_smartcode_history(&content, 0, 50).expect("parsed");
        assert_eq!(resp.messages.len(), 3, "meta 行与非 JSON 行应被跳过");
        assert_eq!(resp.has_more, false);
        assert_eq!(resp.model.as_deref(), Some("m"), "meta 行的 model 应被提取");

        // user 文本
        assert_eq!(resp.messages[0].role, "user");
        assert_eq!(resp.messages[0].content, "帮我看看这个文件");

        // assistant：thinking + text + tool_use 顺序保留
        assert_eq!(resp.messages[1].role, "assistant");
        assert!(resp.messages[1].content.contains("[thinking]先读文件[/thinking]"));
        assert!(resp.messages[1].content.contains("好的"));
        assert!(resp.messages[1].content.contains("[tool_use id=\"t1\"]Read"));
        // 合成 timestamp 随行序递增，前端排序稳定
        assert!(resp.messages[1].timestamp > resp.messages[0].timestamp);

        // 纯 tool_result 的 user 行 → 归为 assistant 输出
        assert_eq!(resp.messages[2].role, "assistant");
        assert!(resp.messages[2].content.contains("[tool_result tool_use_id=\"t1\"]file content[/tool_result]"));
    }

    #[test]
    fn smartcode_history_pagination_from_newest_end() {
        let mut lines = Vec::new();
        for i in 0..10 {
            lines.push(smartcode_line(
                "user",
                serde_json::json!([{"type":"text","text":format!("msg{}", i)}]),
            ));
        }
        let content = lines.join("\n");

        let resp = parse_smartcode_history(&content, 0, 3).expect("parsed");
        assert_eq!(resp.messages.len(), 3);
        assert_eq!(resp.has_more, true);
        // 最新 3 条（msg7/8/9），且按时间正序展示
        assert_eq!(resp.messages[0].content, "msg7");
        assert_eq!(resp.messages[2].content, "msg9");

        let resp = parse_smartcode_history(&content, 9, 3).expect("parsed");
        assert_eq!(resp.messages.len(), 1);
        assert_eq!(resp.has_more, false);
        assert_eq!(resp.messages[0].content, "msg0");
    }

    #[test]
    fn smartcode_tool_result_content_blocks_flatten_to_text() {
        let block = serde_json::json!({
            "type":"tool_result",
            "tool_use_id":"t2",
            "content":[{"type":"text","text":"行1"},{"type":"text","text":"行2"}],
            "is_error":true
        });
        assert_eq!(smartcode_tool_result_text(&block), "行1\n行2");

        let text_block = serde_json::json!({"type":"tool_result","content":"纯文本"});
        assert_eq!(smartcode_tool_result_text(&text_block), "纯文本");
    }

    #[test]
    fn smartcode_empty_and_malformed_input_yield_empty_history() {
        let resp = parse_smartcode_history("", 0, 10).expect("parsed");
        assert!(resp.messages.is_empty());
        assert!(!resp.has_more);

        let resp = parse_smartcode_history("garbage\n\n{}", 0, 10).expect("parsed");
        assert!(resp.messages.is_empty());
    }
}
