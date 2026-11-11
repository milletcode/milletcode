//! 对外更新日志：POST /api/changelog {password} → CHANGELOG.md 内容。
//!
//! 密码门独立于登录体系（PUBLIC_PATHS 豁免 Basic/ plain 认证），供
//! milletcode.cn / milletcode.com / 玄米.中国 / 玄起.中国 等外部站点
//! 跨域调用 —— CORS 只对这组域名放行（浏览器 Origin 对中文域名发
//! punycode，故两套形式都在白名单里）。

use axum::{
    extract::{ConnectInfo, State},
    http::{HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde::Deserialize;
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};
use tower_http::cors::CorsLayer;

use crate::AppState;

/// 每 IP 失败限速：窗口 15 分钟，最多 5 次错误密码，超出后在该窗口内
/// 一律 429（正确密码也拒，计数防止爆破者继续试探）。成功命中即清零。
/// 与登录端点（LOGIN_MAX_FAILURES=5/15min）同一策略——本端点是独立的
/// 免登录密码门，缺这层时曾实测 29 req/s 无限速在线爆破。
const CHANGELOG_MAX_FAILURES: usize = 5;
const CHANGELOG_FAILURE_WINDOW: Duration = Duration::from_secs(15 * 60);

#[derive(Default)]
struct FailureLimiter {
    hits: Mutex<HashMap<String, Vec<Instant>>>,
}

impl FailureLimiter {
    fn prune(&self, key: &str, now: Instant) -> Vec<Instant> {
        let mut map = self.hits.lock().unwrap();
        let kept: Vec<Instant> = map
            .get(key)
            .map(|v| {
                v.iter()
                    .copied()
                    .filter(|t| now.duration_since(*t) < CHANGELOG_FAILURE_WINDOW)
                    .collect()
            })
            .unwrap_or_default();
        if kept.is_empty() {
            map.remove(key);
        } else {
            map.insert(key.to_string(), kept.clone());
        }
        kept
    }

    fn is_throttled(&self, key: &str) -> bool {
        self.prune(key, Instant::now()).len() >= CHANGELOG_MAX_FAILURES
    }

    fn record_failure(&self, key: &str) {
        self.hits.lock().unwrap().entry(key.to_string()).or_default().push(Instant::now());
    }

    fn clear(&self, key: &str) {
        self.hits.lock().unwrap().remove(key);
    }
}

fn failure_limiter() -> &'static FailureLimiter {
    static LIMITER: OnceLock<FailureLimiter> = OnceLock::new();
    LIMITER.get_or_init(FailureLimiter::default)
}

/// 对外更新日志 —— 仓库根 CHANGELOG.md 编译期嵌入，随版本一起发布。
/// 内容面向客户：只讲功能价值，不出现技术词汇（撰写总纲领）。
const CHANGELOG_MD: &str = include_str!("../../../CHANGELOG.md");

/// 外部站点白名单：用户指定域名 + 中文域名的 punycode 形式，http/https 都放。
const ALLOWED_ORIGINS: &[&str] = &[
    "https://milletcode.cn",
    "http://milletcode.cn",
    "https://milletcode.com",
    "http://milletcode.com",
    "https://玄米.中国",
    "http://玄米.中国",
    "https://玄起.中国",
    "http://玄起.中国",
    // 浏览器实际发送的 IDN punycode 形式
    "https://xn--jbyo4q.xn--fiqs8s",
    "http://xn--jbyo4q.xn--fiqs8s",
    "https://xn--jbyo76c.xn--fiqs8s",
    "http://xn--jbyo76c.xn--fiqs8s",
];

#[derive(Debug, Deserialize)]
pub struct ChangelogRequest {
    pub password: String,
}

/// POST /api/changelog — 验密码，正确则返回更新日志 markdown。
pub async fn get_changelog(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(req): Json<ChangelogRequest>,
) -> Response {
    let key = addr.ip().to_string();
    if failure_limiter().is_throttled(&key) {
        return Response::builder()
            .status(StatusCode::TOO_MANY_REQUESTS)
            .header(header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(
                serde_json::json!({ "error": "rate_limited" }).to_string(),
            ))
            .unwrap_or_else(|_| StatusCode::TOO_MANY_REQUESTS.into_response());
    }
    let expected = state.config_manager.config().changelog.password.clone();
    let resp = changelog_response(&req.password, &expected);
    if resp.status() == StatusCode::FORBIDDEN {
        failure_limiter().record_failure(&key);
        // 轻微延迟拖慢线上爆破；正确路径零开销
        tokio::time::sleep(Duration::from_millis(500)).await;
    } else {
        failure_limiter().clear(&key);
    }
    resp
}

fn changelog_response(password: &str, expected: &str) -> Response {
    if !passwords_match(password, expected) {
        // 轻微延迟拖慢线上爆破；正确路径零开销
        return Response::builder()
            .status(StatusCode::FORBIDDEN)
            .header(header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(
                serde_json::json!({ "error": "密码不正确" }).to_string(),
            ))
            .unwrap_or_else(|_| StatusCode::FORBIDDEN.into_response());
    }
    (
        [(header::CONTENT_TYPE, "application/json; charset=utf-8".to_string())],
        Json(serde_json::json!({ "content": CHANGELOG_MD })),
    )
        .into_response()
}

/// 恒时比较（长度差异会泄露，对这种密码门无关紧要）。
fn passwords_match(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn changelog_cors() -> CorsLayer {
    let origins: Vec<HeaderValue> = ALLOWED_ORIGINS
        .iter()
        .map(|o| HeaderValue::from_str(o).expect("static origin is valid"))
        .collect();
    CorsLayer::new()
        .allow_origin(origins)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION])
        .allow_credentials(false)
        .max_age(Duration::from_secs(86400))
}

/// 独立 CORS 层只挂在这条路由上，不放宽全站。
pub fn create_changelog_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/changelog", post(get_changelog))
        .layer(changelog_cors())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn wrong_password_is_rejected() {
        let res = changelog_response("guess", "MilletcodeIsAwesome!");
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn correct_password_returns_changelog() {
        let res = changelog_response("MilletcodeIsAwesome!", "MilletcodeIsAwesome!");
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), 256 * 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let content = v["content"].as_str().unwrap();
        assert!(content.contains("# 更新日志"));
        // 不钉具体版本号：CHANGELOG 合并/新增版本段不该让测试跟着改，
        // 只要还是「# 标题 + ## 版本段」结构就说明嵌入了当前文件。
        assert!(content.contains("\n## v"));
        assert!(content.contains("[[beta]]"));
    }

    #[test]
    fn failure_limiter_throttles_after_max_failures_per_ip() {
        let limiter = FailureLimiter::default();
        for _ in 0..CHANGELOG_MAX_FAILURES {
            assert!(!limiter.is_throttled("1.2.3.4"), "窗口内前 N-1 次不应被限");
            limiter.record_failure("1.2.3.4");
        }
        assert!(limiter.is_throttled("1.2.3.4"), "第 N 次失败后必须限速");
        // 其他 IP 不受牵连
        assert!(!limiter.is_throttled("5.6.7.8"));
        limiter.record_failure("5.6.7.8");
        assert!(!limiter.is_throttled("5.6.7.8"));
        // 成功命中清零，可立即继续
        limiter.clear("1.2.3.4");
        assert!(!limiter.is_throttled("1.2.3.4"));
    }

    #[test]
    fn failure_limiter_expires_old_hits() {
        let limiter = FailureLimiter::default();
        let mut map = limiter.hits.lock().unwrap();
        map.insert(
            "1.2.3.4".to_string(),
            vec![Instant::now() - CHANGELOG_FAILURE_WINDOW - Duration::from_secs(1); CHANGELOG_MAX_FAILURES],
        );
        drop(map);
        assert!(!limiter.is_throttled("1.2.3.4"), "过期失败记录不计入");
    }

    #[test]
    fn password_compare() {
        assert!(passwords_match("abc", "abc"));
        assert!(!passwords_match("abc", "abd"));
        assert!(!passwords_match("abc", "abcd"));
        assert!(!passwords_match("", "x"));
    }

    #[test]
    fn idn_origins_are_punycode_whitelisted() {
        for o in ["https://xn--jbyo4q.xn--fiqs8s", "http://xn--jbyo76c.xn--fiqs8s"] {
            assert!(
                ALLOWED_ORIGINS.contains(&o),
                "punycode 形式 {o} 必须在白名单（浏览器 Origin 头恒为 ASCII）"
            );
        }
        assert!(ALLOWED_ORIGINS.contains(&"https://milletcode.cn"));
        assert!(ALLOWED_ORIGINS.contains(&"https://milletcode.com"));
    }

    fn cors_test_router() -> Router {
        Router::new()
            .route("/api/changelog", post(|| async { "x" }))
            .layer(changelog_cors())
    }

    #[tokio::test]
    async fn cors_preflight_allows_listed_origin() {
        use axum::body::Body;
        use tower::ServiceExt;
        let res = cors_test_router()
            .oneshot(
                axum::http::Request::builder()
                    .method("OPTIONS")
                    .uri("/api/changelog")
                    .header("origin", "https://xn--jbyo4q.xn--fiqs8s")
                    .header("access-control-request-method", "POST")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let allow = res
            .headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert_eq!(allow, "https://xn--jbyo4q.xn--fiqs8s");
    }

    #[tokio::test]
    async fn cors_preflight_rejects_unknown_origin() {
        use axum::body::Body;
        use tower::ServiceExt;
        let res = cors_test_router()
            .oneshot(
                axum::http::Request::builder()
                    .method("OPTIONS")
                    .uri("/api/changelog")
                    .header("origin", "https://evil.example.com")
                    .header("access-control-request-method", "POST")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            res.headers().get("access-control-allow-origin").is_none(),
            "未列域名不应拿到 allow-origin 头"
        );
    }
}
