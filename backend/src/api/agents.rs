use axum::{Json, extract::{State, Path}, http::StatusCode, response::IntoResponse, extract::Json as ExtractJson};
use std::sync::Arc;
use serde::{Deserialize, Serialize};

use crate::AppState;

#[derive(Serialize)]
pub struct AgentInfo {
    pub id: String,
    #[serde(flatten)]
    pub config: crate::config::AgentConfig,
}

pub async fn list_agents(
    State(state): State<Arc<AppState>>,
) -> Json<Vec<AgentInfo>> {
    let instances = &state.config_manager.config().agent.instances;
    let agents: Vec<AgentInfo> = instances.iter()
        .map(|(k, v)| AgentInfo { id: k.clone(), config: v.clone() })
        .collect();
    Json(agents)
}

#[derive(Deserialize)]
pub struct UpdateAgentRequest {
    pub name: Option<String>,
    pub enabled: Option<bool>,
    pub release_channel: Option<String>,
}

#[derive(Serialize)]
pub struct UpdateAgentResponse {
    pub status: String,
    pub id: String,
}

/// PUT /api/agents/{id} — update agent name/enabled/release_channel, persist to config.redb
pub async fn update_agent(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    ExtractJson(req): ExtractJson<UpdateAgentRequest>,
) -> impl IntoResponse {
    // Verify agent exists
    let agent = match state.config_manager.get_agent(&id) {
        Some(a) => a,
        None => {
            return (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "agent not found" }))).into_response();
        }
    };

    if let Some(channel) = &req.release_channel {
        if !matches!(channel.as_str(), "stable" | "beta" | "alpha") {
            return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
                "error": "invalid release_channel, must be stable/beta/alpha"
            }))).into_response();
        }
    }

    // Build updated config
    let mut updated = agent.clone();
    if let Some(name) = req.name {
        updated.name = name;
    }
    if let Some(enabled) = req.enabled {
        updated.enabled = enabled;
    }
    if let Some(channel) = req.release_channel {
        updated.release_channel = channel;
    }

    // Persist to config.redb via settings_store
    let key = format!("agents.{}", id);
    let value = serde_json::to_string(&updated).unwrap_or_default();
    match state.settings_store.set(&key, &value) {
        Ok(()) => {
            // Also update in-memory config so list_agents returns fresh data
            let _ = state.config_manager.update_agent(&id, updated);
            (StatusCode::OK, Json(serde_json::json!({ "status": "ok", "id": id }))).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))).into_response(),
    }
}
