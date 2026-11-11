use axum::{extract::{Query, State}, response::IntoResponse, Json};
use std::collections::HashMap;
use std::sync::Arc;

use crate::AppState;
use crate::agents::PendingKind;

/// GET /api/pending — live pending control requests (approvals + questions)
/// across all sessions. Field-for-field mirrors the WS `approval_request` /
/// `user_question` payloads so the frontend merges with zero mapping.
pub async fn list_pending(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let mut approvals = Vec::new();
    let mut questions = Vec::new();

    for (sid, info) in state.session_manager.list_pending_controls() {
        match info.kind {
            PendingKind::Approval { prompt, options, tool_name, request_id } => {
                approvals.push(serde_json::json!({
                    "session_id": sid.0,
                    "session_key": sid.0,
                    "prompt": prompt,
                    "options": options,
                    "request_id": request_id,
                    "tool_name": tool_name,
                    "project_name": info.project_name,
                    "agent_name": info.agent_name,
                }));
            }
            PendingKind::Question { tool_use_id, request_id, questions: qs } => {
                questions.push(serde_json::json!({
                    "session_id": sid.0,
                    "tool_use_id": tool_use_id,
                    "request_id": request_id,
                    "questions": qs,
                    "project_name": info.project_name,
                    "agent_name": info.agent_name,
                }));
            }
        }
    }

    Json(serde_json::json!({ "approvals": approvals, "questions": questions }))
}

/// GET /api/approvals?limit=100&decision=approved&kind=approval — decided
/// history from approvals.redb, newest first, for 回溯.
pub async fn list_history(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> axum::response::Response {
    let limit = params
        .get("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(100)
        .clamp(1, 1000);
    let decision = params.get("decision").map(|s| s.to_lowercase());
    let kind = params.get("kind").map(|s| s.to_lowercase());

    match state.approval_store.list(limit * 5) {
        Ok(all) => {
            let records: Vec<_> = all
                .into_iter()
                .filter(|r| {
                    decision.as_ref().map_or(true, |d| r.decision == *d)
                        && kind.as_ref().map_or(true, |k| r.kind == *k)
                })
                .take(limit)
                .collect();
            Json(serde_json::json!({ "records": records, "count": records.len() })).into_response()
        }
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}
