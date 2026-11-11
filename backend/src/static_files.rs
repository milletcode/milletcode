use axum::{
    body::Body,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "../frontend-svelte/dist/"]
struct Assets;

/// Version string from the embedded dist/version.json — the same source the
/// UI displays, so /api/health and the sidebar can never disagree.
pub fn embedded_version() -> String {
    static CACHE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| {
            Assets::get("version.json")
                .and_then(|f| serde_json::from_slice::<serde_json::Value>(&f.data).ok())
                .and_then(|v| v.get("version").and_then(|s| s.as_str()).map(str::to_string))
                .unwrap_or_default()
        })
        .clone()
}

/// 没设 MILETCODE_STATIC_CACHE_CONTROL 时的默认缓存秒数。镜像里的 15 分钟：
/// 发版后最多十几分钟客户端跟上来，不至于把固定文件名的资源永远钉在旧版上。
const DEFAULT_STATIC_CACHE_SECS: u64 = 900;

/// 名字固定、内容随版本变的资源（version.json、index.html、logos/*）用什么
/// Cache-Control。给它们 immutable 是错的：文件名不变，浏览器一年内不再回源，
/// Sidebar 就永远停在第一次拿到的那个版本号上（实际踩过这个坑）。
/// 取值为秒数，运行时从 MILETCODE_STATIC_CACHE_CONTROL 读（见 parse_cache_control），
/// 环境变量在进程内只解析一次。
///
/// 为什么是运行时而不是 option_env! 编译期常量：编译期会让同一份源码编出行为
/// 不同的两个二进制（镜像里 900、本地 no-cache），本地不复现的问题镜像里也难验；
/// 换个时长还得重新构建、重新发镜像。运行时读，一个二进制到处跑，部署侧（k8s
/// 的 env）还能按环境单独调，改完重启即可。
fn static_cache_fixed() -> &'static str {
    static CACHE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| {
        let value = parse_cache_control(std::env::var("MILETCODE_STATIC_CACHE_CONTROL").ok().as_deref());
        tracing::info!(cache_control = %value, "固定文件名静态资源的 Cache-Control");
        value
    })
}

/// 环境变量的值 → Cache-Control 头。None = 没设：
///   没设 / 空 → 默认 900 秒（public, max-age=900）——空串是 docker 里 build arg
///               没传的常见结果，按「没设」处理而不是当成非法值
///   "0"      → no-cache（本地开发：本机跑的就是刚构建出来的产物，一秒都别缓）
///   "N"      → public, max-age=N
///   认不出的 → 回落默认 900 并告警。宁可日志里喊一声，也不要因为拼错一个值
///              就让静态资源每次请求都 500（header 值非法会让 builder 报错）
fn parse_cache_control(raw: Option<&str>) -> String {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return max_age(DEFAULT_STATIC_CACHE_SECS);
    };
    match raw.parse::<u64>() {
        Ok(0) => "no-cache".to_string(),
        Ok(secs) => max_age(secs),
        Err(_) => {
            tracing::warn!(
                value = %raw,
                default_secs = DEFAULT_STATIC_CACHE_SECS,
                "MILETCODE_STATIC_CACHE_CONTROL 不是秒数，按默认值处理"
            );
            max_age(DEFAULT_STATIC_CACHE_SECS)
        }
    }
}

fn max_age(secs: u64) -> String {
    format!("public, max-age={secs}")
}

/// vite 产物按内容哈希命名（assets/index-CEvEBgx8.js），内容一变文件名就变，
/// 一年 immutable 是安全的。与上面那个开关无关，写死。
const STATIC_CACHE_HASHED: &str = "public, max-age=31536000, immutable";

/// 只有 assets/ 下是内容哈希命名的，其余（version.json、logos/、index.html）
/// 一律按固定文件名处理。
fn cache_control_for(path: &str) -> &'static str {
    if path.starts_with("assets/") {
        STATIC_CACHE_HASHED
    } else {
        static_cache_fixed()
    }
}

/// 独立登录页（plain/local 模式）：自包含的单文件 HTML（logo 内嵌 data URI，
/// 零外部资源），不依赖 SPA bundle。未登录时 `/`、`/assets/*` 全部拦在认证后，
/// 匿名访客拿不到任何前端结构。
pub async fn login_page() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        include_str!("../assets/login.html"),
    )
}

/// 独立更新日志页（匿名访问，见 auth::PUBLIC_PATHS）：登录页底部的「更新日志」
/// 链接指向 `/changelog`。SPA 在匿名下加载不出来（index.html 与 /assets/* 都在
/// 认证后），所以这页是自包含单文件 HTML；所有模式下都直接出这页，不掺登录
/// 体系 —— 内容只有独立密码门（POST /api/changelog），与 SPA 门共用 localStorage
/// 键，记住过密码就不用重输。
pub async fn changelog_page() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        include_str!("../assets/changelog.html"),
    )
}

/// Serve embedded static file with SPA fallback
pub async fn serve_static(path: String) -> impl IntoResponse {
    let path = path.trim_start_matches('/');

    // Try exact file first
    if let Some(content) = Assets::get(path) {
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        return Response::builder()
            .header(header::CONTENT_TYPE, mime.as_ref())
            .header(header::CACHE_CONTROL, cache_control_for(path))
            .body(Body::from(content.data))
            .unwrap();
    }

    // SPA fallback: serve index.html for any unmatched path
    if let Some(content) = Assets::get("index.html") {
        return Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .header(header::CACHE_CONTROL, static_cache_fixed())
            .body(Body::from(content.data))
            .unwrap();
    }

    StatusCode::NOT_FOUND.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_control_unset_or_blank_falls_back_to_default() {
        assert_eq!(parse_cache_control(None), "public, max-age=900");
        assert_eq!(parse_cache_control(Some("")), "public, max-age=900");
        assert_eq!(parse_cache_control(Some("   ")), "public, max-age=900");
    }

    #[test]
    fn cache_control_zero_means_no_cache() {
        assert_eq!(parse_cache_control(Some("0")), "no-cache");
        assert_eq!(parse_cache_control(Some(" 0 ")), "no-cache");
    }

    #[test]
    fn cache_control_seconds() {
        assert_eq!(parse_cache_control(Some("900")), "public, max-age=900");
        assert_eq!(parse_cache_control(Some("60")), "public, max-age=60");
    }

    /// 拼错的值不该把请求打成 500，也不该悄悄按 no-cache 走（那样只是更慢，
    /// 会掩盖配置错误）——按默认值来并在日志里喊一声。
    #[test]
    fn cache_control_garbage_falls_back_to_default() {
        assert_eq!(parse_cache_control(Some("abc")), "public, max-age=900");
        assert_eq!(parse_cache_control(Some("no-cache")), "public, max-age=900");
        assert_eq!(parse_cache_control(Some("-1")), "public, max-age=900");
    }

    /// assets/ 那支与开关无关：无论 MILETCODE_STATIC_CACHE_CONTROL 取什么值
    /// （这里不去改进程环境，免得和别的测试抢变量），它都是 immutable。
    #[test]
    fn hashed_assets_are_always_immutable() {
        assert_eq!(cache_control_for("assets/index-abc.js"), STATIC_CACHE_HASHED);
        assert_eq!(cache_control_for("assets/index-abc.css"), STATIC_CACHE_HASHED);
        // 其余都走固定文件名那一支：解析结果要么是 no-cache，要么是 max-age=N，
        // 绝不会等于上面那个 immutable。
        assert_ne!(cache_control_for("version.json"), STATIC_CACHE_HASHED);
        assert_ne!(cache_control_for("logos/color-1.png"), STATIC_CACHE_HASHED);
    }

    /// 独立更新日志页必须能自举：走 /api/changelog 拿内容、用 SPA 同一枚
    /// localStorage 键记住密码（两个页面共用，一边记住另一边免输）。
    #[test]
    fn standalone_changelog_page_is_self_contained() {
        let html = include_str!("../assets/changelog.html");
        assert!(html.contains("/api/changelog"));
        assert!(html.contains("milletcode.changelog.pw"));
        assert!(!html.contains("src=\"\""), "内嵌图片不允许空 src");
    }

    /// 页面里每个 data URI 都必须是合法 base64 且解出 PNG 魔数 —— logo 是
    /// 13KB 的 base64 长串，手抄错一个字符图片就裂了，还只在浏览器里看得见。
    #[test]
    fn standalone_pages_data_uris_are_valid_png() {
        let check = |html: &'static str, file: &str| {
            let count = html.matches("data:image/png;base64,").count();
            assert!(count > 0, "{file} 应至少内嵌一张 logo");
            for rest in html.split("data:image/png;base64,").skip(1) {
                let b64: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '+' || *c == '/' || *c == '=')
                    .collect();
                let bytes = base64::Engine::decode(
                    &base64::engine::general_purpose::STANDARD,
                    b64.as_bytes(),
                )
                .unwrap_or_else(|e| panic!("{file} 里的 data URI 解码失败: {e}"));
                assert_eq!(
                    &bytes[..8],
                    &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a],
                    "{file} 的 data URI 不是 PNG"
                );
            }
        };
        check(include_str!("../assets/login.html"), "login.html");
        check(include_str!("../assets/changelog.html"), "changelog.html");
    }
}
