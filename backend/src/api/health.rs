use axum::Json;

pub async fn health_check() -> Json<serde_json::Value> {
    // Same source the UI footer reads (embedded dist/version.json) — not
    // CARGO_PKG_VERSION, which drifts from the released version.
    let version = crate::static_files::embedded_version();
    Json(serde_json::json!({
        "status": "ok",
        "version": if version.is_empty() { "unknown" } else { &version },
    }))
}
