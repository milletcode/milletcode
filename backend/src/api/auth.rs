use axum::{
    extract::{ConnectInfo, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use std::sync::Arc;

use crate::auth::{login_throttle_key, trusted_proxy_nets, PLAIN_SESSION_COOKIE};
use crate::AppState;

/// GET /api/captcha — 登录页自举用的验证码（plain 公开路径）。
/// 返回 {id, svg}；svg 直接 innerHTML 注入，点击图片即换一张。
pub async fn captcha(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let (id, svg) = state.captcha_store.issue();
    Json(serde_json::json!({ "id": id, "svg": svg }))
}

/// POST /api/auth/login — plain 模式登录：验证码 → 限速 → 凭据比对，
/// 全部通过后签发内存会话 cookie。失败统一 401（不区分哪个字段错了）。
#[derive(Deserialize)]
pub struct LoginRequest {
    pub user: String,
    pub password: String,
    #[serde(default)]
    pub captcha_id: String,
    #[serde(default)]
    pub captcha_code: String,
}

pub async fn login(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    headers: axum::http::HeaderMap,
    Json(req): Json<LoginRequest>,
) -> Response {
    // 验证码前置：单次使用，无论对错都销毁，客户端需换新图重试。
    if !state.captcha_store.verify(&req.captcha_id, &req.captcha_code) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "captcha" })),
        )
            .into_response();
    }

    // 双维度限速：IP（滑动窗口内失败达阈值即拒）+ 用户名（换 IP / 伪造
    // XFF 重置不了这条计数，是对爆破的硬上限；代价是知道用户名的攻击者
    // 可恶意锁号 15 分钟，窗口短、成功登录即清零，可接受）。成功登录清零。
    let nets = trusted_proxy_nets(&state.config_manager.config().auth.trusted_proxies);
    let ip_key = login_throttle_key(&headers, Some(addr), &nets);
    let user_key = format!("user:{}", req.user.trim().to_lowercase());
    if !state.login_throttle.check(&ip_key) || !state.login_throttle.check(&user_key) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({ "error": "rate_limited" })),
        )
            .into_response();
    }

    let auth = &state.config_manager.config().auth;
    let ok = crate::auth::constant_time_eq(req.user.trim(), &auth.plain.user)
        && crate::auth::constant_time_eq(&req.password, &auth.plain.password);
    if !ok {
        state.login_throttle.record_failure(&ip_key);
        state.login_throttle.record_failure(&user_key);
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "invalid credentials" })),
        )
            .into_response();
    }
    state.login_throttle.clear(&ip_key);
    state.login_throttle.clear(&user_key);

    let sid = state.plain_sessions.issue();
    let cookie = format!(
        "{PLAIN_SESSION_COOKIE}={sid}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}",
        crate::auth::PLAIN_SESSION_TTL.as_secs()
    );
    let mut resp = (StatusCode::OK, Json(serde_json::json!({ "ok": true }))).into_response();
    if let Ok(v) = cookie.parse() {
        resp.headers_mut().append(header::SET_COOKIE, v);
    }
    resp
}
