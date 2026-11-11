use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

use crate::AppState;
use crate::db::CustomCommand;

#[derive(Debug, Serialize)]
pub struct ListCommandsResponse {
    pub commands: Vec<CustomCommand>,
}

#[derive(Debug, Deserialize)]
pub struct CreateCommandRequest {
    pub title: String,
    pub command: String,
    #[serde(default = "default_type")]
    pub r#type: String,
}

fn default_type() -> String {
    "default".to_string()
}

/// GET /api/commands - List all custom commands
pub async fn list_commands(
    State(state): State<Arc<AppState>>,
) -> Result<Json<ListCommandsResponse>, (StatusCode, String)> {
    let commands = state
        .commands_store
        .list()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to list commands: {}", e)))?;

    Ok(Json(ListCommandsResponse { commands }))
}

/// POST /api/commands - Create a new custom command
pub async fn create_command(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateCommandRequest>,
) -> Result<Json<CustomCommand>, (StatusCode, String)> {
    let cmd = CustomCommand {
        id: Uuid::new_v4().to_string(),
        title: req.title,
        command: req.command,
        r#type: req.r#type,
    };

    state
        .commands_store
        .set(&cmd)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to save command: {}", e)))?;

    Ok(Json(cmd))
}

/// DELETE /api/commands/:id - Delete a custom command
pub async fn delete_command(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    state
        .commands_store
        .delete(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to delete command: {}", e)))?;

    Ok(StatusCode::NO_CONTENT)
}

/// POST /api/commands/reset - Reset all commands to defaults
pub async fn reset_commands(
    State(state): State<Arc<AppState>>,
) -> Result<StatusCode, (StatusCode, String)> {
    tracing::info!("POST /api/commands/reset called");
    state
        .commands_store
        .reset_to_defaults()
        .map_err(|e| {
            tracing::error!("Failed to reset commands: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to reset commands: {}", e))
        })?;

    // Verify the reset worked
    match state.commands_store.list() {
        Ok(cmds) => tracing::info!("After reset, commands count: {}", cmds.len()),
        Err(e) => tracing::error!("Failed to list commands after reset: {}", e),
    }

    Ok(StatusCode::NO_CONTENT)
}
