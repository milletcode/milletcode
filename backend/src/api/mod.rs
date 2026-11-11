use axum::{
    routing::{any, delete, get, post, put},
    middleware, Router,
};
use std::sync::Arc;

use crate::AppState;

mod agents;
mod auth;
mod changelog;
mod commands;
pub mod config;
pub(crate) mod debug;
mod files;
mod forwards;
mod health;
mod logout;
mod oauth_proxy;
mod pending;
mod projects;
pub mod proxy;
mod sessions;
mod settings;
mod skills;
mod slash_commands;
mod terminal;
mod tunnels;
pub(crate) mod uploads;

pub fn create_router(state: Arc<AppState>) -> Router<()> {
    let mut router: Router<Arc<AppState>> = Router::new()
        // API routes
        .route("/api/health", get(health::health_check))
        .route("/api/logout", get(logout::logout))
        .route("/api/auth/login", post(auth::login))
        .route("/api/captcha", get(auth::captcha))
        // plain 模式独立登录页（自包含，匿名可访问；见 auth::is_plain_public_path）
        .route("/login", get(crate::static_files::login_page))
        // 独立更新日志页：匿名可访问（PUBLIC_PATHS），只有独立密码门、不参与登录体系
        .route("/changelog", get(crate::static_files::changelog_page))
        .route("/api/sessions", get(sessions::list_sessions).post(sessions::create_session))
        .route("/api/sessions/idle", get(sessions::list_idle_sessions))
        // Session routes: /api/sessions/{org}/{project}/{agent-name}/{session-name}
        .route("/api/sessions/{org}/{project}/{agent_name}/{session_name}", get(sessions::get_session).delete(sessions::delete_session))
        .route("/api/sessions/{org}/{project}/{agent_name}/{session_name}/stop", post(sessions::stop_session))
        .route("/api/sessions/{org}/{project}/{agent_name}/{session_name}/restore", post(sessions::restore_session))
        .route("/api/sessions/{org}/{project}/{agent_name}/{session_name}/message", post(sessions::send_message))
        .route("/api/sessions/{org}/{project}/{agent_name}/{session_name}/idle-notify", post(sessions::set_idle_notify))
        .route("/api/sessions/{org}/{project}/{agent_name}/{session_name}/approve", post(sessions::handle_approval))
        .route("/api/sessions/{org}/{project}/{agent_name}/{session_name}/answer", post(sessions::answer_question))
        .route("/api/sessions/{org}/{project}/{agent_name}/{session_name}/config", post(sessions::set_session_config))
        .route("/api/pending", get(pending::list_pending))
        .route("/api/approvals", get(pending::list_history))
        .route("/api/sessions/{org}/{project}/{agent_name}/{session_name}/history", get(sessions::get_session_history))
        .route("/api/sessions/{org}/{project}/{agent_name}/{session_name}/files", get(files::list_files))
        .route("/api/sessions/{org}/{project}/{agent_name}/{session_name}/file", get(files::read_file))
        .route("/api/sessions/{org}/{project}/{agent_name}/{session_name}/file/download", get(files::download_file))
        .route("/api/sessions/{org}/{project}/{agent_name}/{session_name}/full", delete(sessions::full_delete_session))
        .route("/api/agents", get(agents::list_agents))
        .route("/api/agents/{id}", put(agents::update_agent))
        .route("/api/settings", get(settings::get_settings))
        .route("/api/settings/auth", get(settings::get_auth_settings))
        .route("/api/ws-ticket", post(crate::auth::issue_ws_ticket))
        .route("/api/settings/ssh-hosts", get(settings::get_ssh_hosts))
        .route("/api/settings/capacity", get(settings::get_capacity))
        .route("/api/settings/online", get(settings::get_online_stats))
        // 技能包：官方分发 + 第三方审阅安装
        .route("/api/skills/official", get(skills::list_official))
        .route("/api/skills/overwrite", put(skills::set_overwrite))
        .route("/api/skills/official/{name}/install", post(skills::install_official))
        .route("/api/skills/official/{name}/uninstall", post(skills::uninstall_official))
        .route("/api/skills/third", post(skills::create_third).get(skills::list_third))
        .route("/api/skills/third/{id}", delete(skills::delete_third))
        .route("/api/skills/history-count", get(skills::get_history_count).put(skills::set_history_count))
        .route("/api/skills/tasks/{id}", get(skills::get_task))
        .merge(config::create_config_router())
        // 对外更新日志：密码门 + 外部域名 CORS（不参与登录体系）
        .merge(changelog::create_changelog_router())
        // Project routes — /projects/refresh MUST be before /projects/{id}
        .route("/api/projects", get(projects::list_projects).post(projects::create_project))
        .route("/api/projects/discovery-paths", get(projects::list_discovery_paths))
        .route("/api/projects/refresh", post(projects::refresh_projects))
        .route("/api/projects/clone", post(projects::clone_project))
        .route("/api/projects/{keyword}/{name}/archive", post(projects::archive_project))
        .route("/api/projects/{keyword}/{name}/unarchive", post(projects::unarchive_project))
        .route("/api/projects/{keyword}/{name}", get(projects::get_project).delete(projects::delete_project))
        .route("/api/projects/{keyword}/{name}/sessions", get(projects::list_project_sessions))
        .route("/api/projects/{keyword}/{name}/git-status", get(projects::git_status))
        // Project settings: env vars (envs.redb) and terminal commands
        .route("/api/projects/{keyword}/{name}/envs", get(projects::list_project_envs))
        .route("/api/projects/{keyword}/{name}/envs/{env}", get(projects::list_project_env_vars).put(projects::set_project_env_var))
        .route("/api/projects/{keyword}/{name}/envs/{env}/{key}", delete(projects::delete_project_env_var))
        .route("/api/projects/{keyword}/{name}/issue-config", get(projects::get_project_issue_config).put(projects::set_project_issue_config).delete(projects::delete_project_issue_config))
        .route("/api/projects/{keyword}/{name}/link", get(projects::get_project_link).put(projects::set_project_link))
        .route("/api/projects/{keyword}/{name}/terminal/commands", get(projects::get_project_terminal_commands).post(projects::set_project_terminal_commands))
        .route("/api/slash-commands", get(slash_commands::list_slash_commands))
        .route("/api/commands", get(commands::list_commands).post(commands::create_command))
        .route("/api/commands/reset", post(commands::reset_commands))
        .route("/api/commands/{id}", delete(commands::delete_command))
        .merge(uploads::upload_route(state.clone()))
        .route("/api/debug/ps", get(debug::ps))
        // Port forwarding routes — id contains '/' (session key), wildcard match
        .route("/api/forwards", get(forwards::list_forwards).post(forwards::create_forward))
        .route("/api/forwards/scan", post(forwards::scan))
        .route("/api/forwards/test", post(forwards::test_forward))
        .route("/api/forwards/{*id}", delete(forwards::delete_forward))
        // millet-tunnel: user-side tunnel CLI connects here; token in query
        // self-authenticates (exempted from Basic auth in auth middleware)
        .route("/api/tunnels", get(tunnels::list_tunnels).post(tunnels::create_tunnel))
        .route("/api/tunnels/connect", get(tunnels::connect_tunnel))
        .route("/api/tunnels/{*id}", delete(tunnels::delete_tunnel))
        // code-server style /p/ reverse proxy + signed-link minting (auth
        // middleware validates ?_pwt for /p/ before falling back to Basic)
        .route("/api/forwards/proxy-link", post(proxy::proxy_link))
        .route("/p/{org}/{project}/{agent_name}/{session_name}/{port}", any(proxy::proxy_entry))
        .route("/p/{org}/{project}/{agent_name}/{session_name}/{port}/", any(proxy::proxy_entry))
        .route("/p/{org}/{project}/{agent_name}/{session_name}/{port}/{*rest}", any(proxy::proxy_entry))
        .route("/api/debug/logs", get(debug::list_logs))
        .route("/api/debug/session/{*session_uuid}", get(debug::read_session_log))
        .route("/api/debug/log/info", get(debug::get_log_info))
        .route("/api/debug/log/tail", get(debug::get_log_tail))
        // Terminal routes
        .route("/api/terminal/create", post(terminal::create_terminal))
        .route("/api/terminal/{id}/resize", post(terminal::resize_terminal))
        .route("/api/terminal/{id}/input", post(terminal::input_terminal))
        .route("/api/terminal/{id}/command", post(terminal::command_terminal))
        .route("/api/terminal/{id}", delete(terminal::close_terminal))
        .route("/api/terminal/list", get(terminal::list_terminals))
        .route("/api/terminal/{id}/ws", get(terminal::terminal_ws_handler))
        // OAuth2 Proxy reverse proxy - must be before auth middleware
        .route("/oauth2/{*path}", get(oauth_proxy::oauth_proxy_handler).post(oauth_proxy::oauth_proxy_handler))
        // 空路径（/oauth2 与 /oauth2/）不匹配 {*path}，曾落到 SPA fallback
        // 匿名吐出前端入口页骨架；显式 404 堵住该信息泄露出口。
        .route("/oauth2", get(oauth_proxy::oauth_root_not_found))
        .route("/oauth2/", get(oauth_proxy::oauth_root_not_found))
        .route("/ws/{org}/{project}/{agent_id}", get(crate::ws::ws_handler));

    // Auth middleware must wrap the SPA fallback too — attached after
    // with_state/fallback, otherwise static pages are served unauthenticated.
    let requires_auth = {
        let auth = &state.config_manager.config().auth;
        !auth.effective_mode().is_empty()
    };
    let router = router.with_state(state.clone()).fallback(get(static_handler));

    // 错误响应保密改写：>=400 的 JSON body 统一过 sanitize_public_error，
    // 终端/转发/上传等各处手写的错误文案不必逐点记得调用。
    if requires_auth {
        router
            .layer(middleware::from_fn(sanitize_error_response))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                crate::auth::auth_middleware,
            ))
    } else {
        router.layer(middleware::from_fn(sanitize_error_response))
    }
}

/// 错误响应保密改写（见 crate::sanitize）。只重写 >=400 的 JSON 响应，
/// 成功路径零开销；非 JSON body 原样透传。
async fn sanitize_error_response(
    req: axum::extract::Request,
    next: middleware::Next,
) -> axum::response::Response {
    let res = next.run(req).await;
    if !(res.status().is_client_error() || res.status().is_server_error()) {
        return res;
    }
    let (mut parts, body) = res.into_parts();
    let bytes = match axum::body::to_bytes(body, 256 * 1024).await {
        Ok(b) => b,
        Err(_) => {
            return axum::http::Response::builder()
                .status(parts.status)
                .body(axum::body::Body::empty())
                .unwrap()
        }
    };
    if let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
        sanitize_json_strings(&mut value);
        if let Ok(json) = serde_json::to_vec(&value) {
            parts.headers.remove(axum::http::header::CONTENT_LENGTH);
            return axum::response::Response::from_parts(parts, axum::body::Body::from(json));
        }
    }
    axum::response::Response::from_parts(parts, axum::body::Body::from(bytes))
}

fn sanitize_json_strings(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(s) => *s = crate::sanitize::sanitize_public_error(s),
        serde_json::Value::Array(items) => items.iter_mut().for_each(sanitize_json_strings),
        serde_json::Value::Object(map) => map.values_mut().for_each(sanitize_json_strings),
        _ => {}
    }
}

/// Handler for serving static files from embedded assets
async fn static_handler(uri: axum::http::Uri) -> impl axum::response::IntoResponse {
    let path = uri.path().trim_start_matches('/').to_string();
    crate::static_files::serve_static(path).await
}

#[cfg(test)]
mod sanitize_middleware_tests {
    use axum::http::{Request, StatusCode};
    use axum::middleware;
    use axum::routing::get;
    use axum::{Json, Router};
    use tower::ServiceExt;

    use super::sanitize_error_response;

    fn test_app() -> Router {
        Router::new()
            .route(
                "/boom",
                get(|| async {
                    (
                        StatusCode::BAD_GATEWAY,
                        Json(serde_json::json!({
                            "error": "SSH 连接 192.168.3.34 失败: ssh: connect to host 192.168.3.34 port 22: Connection timed out",
                            "detail": "fallback ~/.ssh/config",
                            "nested": {"msg": "russh: Connect to 10.0.0.1:22 refused"},
                            "ok_field": "connect to 192.168.3.9:2222 keep"
                        })),
                    )
                }),
            )
            .route(
                "/ok",
                get(|| async { (StatusCode::OK, Json(serde_json::json!({"text": "ssh port 22"}))) }),
            )
            .layer(middleware::from_fn(sanitize_error_response))
    }

    #[tokio::test]
    async fn error_response_ssh_text_is_sanitized() {
        let res = test_app()
            .oneshot(Request::builder().uri("/boom").body(axum::body::Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_GATEWAY);
        let bytes = axum::body::to_bytes(res.into_body(), 64 * 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            v["error"],
            "millet-connector 连接 192.168.3.34 失败: millet-connector: connect to host 192.168.3.34 port conn-port: Connection timed out"
        );
        assert_eq!(v["detail"], "fallback ~/.millet-connector/config");
        assert_eq!(v["nested"]["msg"], "rumillet-connector: Connect to 10.0.0.1:conn-port refused");
        // 非默认端口不动
        assert_eq!(v["ok_field"], "connect to 192.168.3.9:2222 keep");
    }

    #[tokio::test]
    async fn success_response_is_not_touched() {
        let res = test_app()
            .oneshot(Request::builder().uri("/ok").body(axum::body::Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), 64 * 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        // 成功响应不重写——agent 输出里的 ssh 用语不受影响
        assert_eq!(v["text"], "ssh port 22");
    }
}
