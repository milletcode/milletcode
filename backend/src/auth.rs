use axum::{
    extract::State,
    http::{header, StatusCode},
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
    Json,
};
use parking_lot::Mutex;
use std::{collections::HashMap, sync::Arc, time::{Duration, Instant}};

use crate::AppState;

/// One-time, short-lived tickets that authorize a WebSocket upgrade. Browsers
/// cannot set an Authorization header on the WebSocket API, so the client
/// mints a ticket over an authenticated REST call and appends it to the WS URL.
pub struct WsTickets {
    inner: Mutex<HashMap<String, Instant>>,
}

const WS_TICKET_TTL: Duration = Duration::from_secs(30);

impl WsTickets {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    pub fn issue(&self) -> String {
        use rand::RngCore;
        let mut bytes = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        let ticket = base64::Engine::encode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            bytes,
        );
        let mut guard = self.inner.lock();
        guard.retain(|_, exp| *exp > Instant::now());
        guard.insert(ticket.clone(), Instant::now() + WS_TICKET_TTL);
        ticket
    }

    /// Single use: a valid ticket is consumed on success.
    pub fn verify(&self, ticket: &str) -> bool {
        let mut guard = self.inner.lock();
        match guard.remove(ticket) {
            Some(exp) if exp > Instant::now() => true,
            _ => false,
        }
    }
}

/// Check if current request has debug mode enabled
/// Debug mode is enabled when:
/// 1. state.env == "debug" (traditional debug mode), OR
/// 2. mode == "oauth2_proxy" && user is in oauth2_proxy.allowed_admins
pub fn is_debug_mode(state: &AppState, headers: &axum::http::HeaderMap) -> bool {
    // Traditional debug mode
    if state.env == "debug" {
        return true;
    }

    // OAuth admin debug mode
    let auth = &state.config_manager.config().auth;
    if auth.effective_mode() == "oauth2_proxy" {
        if let Some(user_header) = headers.get("X-Auth-User") {
            if let Ok(username) = user_header.to_str() {
                return auth.is_admin(username);
            }
        }
    }

    false
}

/// plain 模式的内存会话表：login 签发、cookie 携带、滑动过期。不落库——
/// 重启后全部失效，浏览器重新登录即可，凭据本体永远只在配置文件里。
pub struct PlainSessions {
    inner: Mutex<HashMap<String, Instant>>,
}

pub const PLAIN_SESSION_TTL: Duration = Duration::from_secs(7 * 24 * 3600);

impl Default for PlainSessions {
    fn default() -> Self {
        Self::new()
    }
}

impl PlainSessions {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    pub fn issue(&self) -> String {
        use rand::RngCore;
        let mut bytes = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        let sid = base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, bytes);
        let mut guard = self.inner.lock();
        guard.retain(|_, exp| *exp > Instant::now());
        guard.insert(sid.clone(), Instant::now() + PLAIN_SESSION_TTL);
        sid
    }

    /// 滑动过期：命中即续期。
    pub fn verify(&self, sid: &str) -> bool {
        let mut guard = self.inner.lock();
        match guard.get(sid) {
            Some(exp) if *exp > Instant::now() => {
                guard.insert(sid.to_string(), Instant::now() + PLAIN_SESSION_TTL);
                true
            }
            _ => {
                guard.remove(sid);
                false
            }
        }
    }

    pub fn revoke(&self, sid: &str) {
        self.inner.lock().remove(sid);
    }
}

/// plain 模式的会话 cookie 名（HttpOnly、SameSite=Lax、Path=/）
pub const PLAIN_SESSION_COOKIE: &str = "millet_session";

pub fn plain_session_cookie_value(cookie_header: &str) -> Option<&str> {
    cookie_header
        .split(';')
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == PLAIN_SESSION_COOKIE)
        .map(|(_, v)| v.trim())
}

// ---------------------------------------------------------------------------
// 时间槽 token（内部测试用）：`slot:HMAC-SHA256(token_secret, slot)`。
// slot = unix 秒 / 300（5 分钟一槽），±1 槽宽限。无状态——服务端只存种子，
// token 本身不出服务器，泄露窗口最多一个槽。
// ---------------------------------------------------------------------------

pub const TIME_TOKEN_SLOT_SECS: u64 = 300;
/// 允许的槽偏移：客户端时钟与服务端相差不超过 ±5 分钟。
pub const TIME_TOKEN_SLOT_GRACE: u64 = 1;

pub fn current_time_slot() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() / TIME_TOKEN_SLOT_SECS)
        .unwrap_or(0)
}

pub fn time_token_compute(secret: &str, slot: u64) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(slot.to_string().as_bytes());
    let digest = mac.finalize().into_bytes();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn verify_time_token(secret: &str, token: &str) -> bool {
    let Some((slot_str, sig)) = token.split_once(':') else {
        return false;
    };
    let Ok(slot) = slot_str.parse::<u64>() else {
        return false;
    };
    let now = current_time_slot();
    if slot.abs_diff(now) > TIME_TOKEN_SLOT_GRACE {
        return false;
    }
    constant_time_eq(sig, &time_token_compute(secret, slot))
}

// ---------------------------------------------------------------------------
// 登录验证码：自托管 SVG，内存表 TTL 5 分钟、单次使用。只有浏览器登录页走
// /api/auth/login，脚本/测试客户端走 Bearer 层，因此常开、无需配置开关。
// ---------------------------------------------------------------------------

pub struct CaptchaStore {
    /// id → (答案小写, 过期时刻)
    inner: Mutex<HashMap<String, (String, Instant)>>,
}

const CAPTCHA_TTL: Duration = Duration::from_secs(5 * 60);
/// 去掉易混淆字符（0/O、1/I/l）的 5 位数字验证码
const CAPTCHA_ALPHABET: &[u8] = b"23456789";

impl Default for CaptchaStore {
    fn default() -> Self {
        Self::new()
    }
}

impl CaptchaStore {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// 签发一张验证码，返回 (id, svg)。
    pub fn issue(&self) -> (String, String) {
        use rand::RngCore;
        let mut id_bytes = [0u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut id_bytes);
        let id = base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, id_bytes);
        let mut code = String::with_capacity(5);
        for _ in 0..5 {
            let mut b = [0u8; 1];
            rand::rngs::OsRng.fill_bytes(&mut b);
            code.push(CAPTCHA_ALPHABET[(b[0] as usize) % CAPTCHA_ALPHABET.len()] as char);
        }
        let mut guard = self.inner.lock();
        let now = Instant::now();
        guard.retain(|_, (_, exp)| *exp > now);
        guard.insert(id.clone(), (code.clone(), now + CAPTCHA_TTL));
        (id, captcha_svg(&code))
    }

    /// 单次使用：答案不区分大小写；无论对错都销毁。
    pub fn verify(&self, id: &str, answer: &str) -> bool {
        let entry = self.inner.lock().remove(id);
        match entry {
            Some((expected, exp)) if exp > Instant::now() => {
                constant_time_eq(&answer.trim().to_lowercase(), &expected)
            }
            _ => false,
        }
    }
}

/// 5x7 网格笔画字体（y 向下），只覆盖 CAPTCHA_ALPHABET 的 8 个数字。
/// 刻意不用 <text> 节点 —— 明文 DOM 里的验证码脚本 grep 一下就破了。
fn captcha_glyph(ch: u8) -> &'static [&'static [(i32, i32)]] {
    match ch {
        b'2' => &[&[(0, 0), (3, 0), (4, 1), (4, 3), (0, 6), (4, 6)]],
        b'3' => &[&[(0, 0), (4, 0), (4, 6), (0, 6)], &[(1, 3), (4, 3)]],
        b'4' => &[&[(0, 0), (0, 3), (4, 3)], &[(3, 0), (3, 6)]],
        b'5' => &[&[(4, 0), (0, 0), (0, 3), (3, 3), (4, 4), (4, 5), (3, 6), (0, 6)]],
        b'6' => &[&[(4, 0), (0, 3), (0, 6), (4, 6), (4, 3), (0, 3)]],
        b'7' => &[&[(0, 0), (4, 0), (2, 6)]],
        b'8' => &[&[(0, 0), (4, 0), (4, 6), (0, 6), (0, 0)], &[(0, 3), (4, 3)]],
        b'9' => &[&[(4, 6), (4, 0), (0, 0), (0, 3), (4, 3)]],
        _ => &[],
    }
}

fn captcha_svg(code: &str) -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let colors = ["#8b95e8", "#667eea", "#764ba2", "#40a9ff", "#b490ca"];
    let mut body = String::new();
    for _ in 0..4 {
        let (x1, y1, x2, y2) = (
            rng.gen_range(0..110),
            rng.gen_range(0..40),
            rng.gen_range(0..110),
            rng.gen_range(0..40),
        );
        let c = colors[rng.gen_range(0..colors.len())];
        body.push_str(&format!(
            r#"<line x1="{x1}" y1="{y1}" x2="{x2}" y2="{y2}" stroke="{c}" stroke-width="1" opacity="0.5"/>"#
        ));
    }
    const SCALE: i32 = 3;
    for (i, ch) in code.bytes().enumerate() {
        let origin_x = 14 + i as i32 * 22;
        let origin_y = 9 + rng.gen_range(-3i32..=3);
        let rot = rng.gen_range(-25i32..=25);
        let cx = origin_x + 7;
        let cy = origin_y + 10;
        let c = colors[rng.gen_range(0..colors.len())];
        let (sin, cos) = (rot as f32).to_radians().sin_cos();
        for stroke in captcha_glyph(ch) {
            let points: Vec<String> = stroke
                .iter()
                .map(|(gx, gy)| {
                    // 每个顶点加 ±1px 抖动，再烘焙旋转（不留 transform 属性，
                    // 否则坐标就是未旋转的明文字形，脚本可直接模板比对）
                    let jx = rng.gen_range(-1i32..=1) as f32;
                    let jy = rng.gen_range(-1i32..=1) as f32;
                    let dx = (*gx * SCALE) as f32 + jx - 7.0;
                    let dy = (*gy * SCALE) as f32 + jy - 10.0;
                    format!("{:.1},{:.1}", cx as f32 + dx * cos - dy * sin, cy as f32 + dx * sin + dy * cos)
                })
                .collect();
            body.push_str(&format!(
                r#"<polyline points="{}" fill="none" stroke="{c}" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/>"#,
                points.join(" ")
            ));
        }
    }
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="110" height="40" viewBox="0 0 110 40" role="img"><rect width="110" height="40" rx="6" fill="#232338"/>{body}</svg>"##
    )
}

// ---------------------------------------------------------------------------
// 登录 IP 限速：滑动窗口，连续失败达阈值后拒绝尝试（成功登录即清零）。
// key 取 X-Forwarded-For 末位——vite/Traefik 逐跳追加，末位是我们基础设施
// 看到的对端，客户端伪造的值只会出现在链首；直连（无 XFF）统一记 "local"。
// ---------------------------------------------------------------------------

pub struct LoginThrottle {
    inner: Mutex<HashMap<String, Vec<Instant>>>,
}

pub const LOGIN_WINDOW: Duration = Duration::from_secs(15 * 60);
pub const LOGIN_MAX_FAILURES: usize = 5;

impl Default for LoginThrottle {
    fn default() -> Self {
        Self::new()
    }
}

impl LoginThrottle {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// 当前 key 是否还允许尝试登录。
    pub fn check(&self, key: &str) -> bool {
        let mut guard = self.inner.lock();
        let now = Instant::now();
        let entry = guard.entry(key.to_string()).or_default();
        entry.retain(|t| now.duration_since(*t) < LOGIN_WINDOW);
        entry.len() < LOGIN_MAX_FAILURES
    }

    pub fn record_failure(&self, key: &str) {
        let mut guard = self.inner.lock();
        let now = Instant::now();
        let entry = guard.entry(key.to_string()).or_default();
        entry.retain(|t| now.duration_since(*t) < LOGIN_WINDOW);
        entry.push(now);
    }

    pub fn clear(&self, key: &str) {
        self.inner.lock().remove(key);
    }
}

/// 提取登录限速 key。只有当 TCP 对端命中可信代理网段（trusted_proxies，
/// 默认 loopback + RFC1918）**且**带 x-millet-edge 标记头时，才采信
/// X-Forwarded-For 末位——逐跳追加语义下末位是可信代理看到的真实对端，
/// 客户端伪造的值只会出现在链首。其余一切情况（含公网直连伪造 edge 头）
/// 用 TCP 对端地址——它不可伪造。
pub fn login_throttle_key(
    headers: &axum::http::HeaderMap,
    remote: Option<std::net::SocketAddr>,
    trusted: &[IpNet],
) -> String {
    let peer = remote.map(|a| normalize_ip(a.ip()));
    let from_trusted_proxy = peer.is_some_and(|ip| trusted.iter().any(|net| net.contains(ip)));
    if from_trusted_proxy && headers.contains_key("x-millet-edge") {
        let xff = headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit(',').next())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if let Some(ip) = xff {
            return ip.to_string();
        }
    }
    peer.map(|ip| ip.to_string())
        .unwrap_or_else(|| "local".to_string())
}

/// 默认可信代理网段：v4/v6 loopback + RFC1918 内网 + IPv6 ULA。
/// 自托管部署的边缘代理（nginx/Traefik/vite）与本服务同机或同 docker
/// 网络，对端必然落在这些网段里；显式配置 trusted_proxies 时整体替换。
pub const DEFAULT_TRUSTED_PROXIES: &[&str] = &[
    "127.0.0.0/8",
    "::1/128",
    "10.0.0.0/8",
    "172.16.0.0/12",
    "192.168.0.0/16",
    "fc00::/7",
];

/// 解析配置的可信代理网段；未配置时用默认集合。
pub fn trusted_proxy_nets(configured: &[String]) -> Vec<IpNet> {
    let entries: Vec<&str> = if configured.is_empty() {
        DEFAULT_TRUSTED_PROXIES.to_vec()
    } else {
        configured.iter().map(String::as_str).collect()
    };
    entries.iter().filter_map(|s| IpNet::parse(s)).collect()
}

/// v4-mapped v6（::ffff:1.2.3.4，dual-stack listen 下 accept 出的形态）
/// 归一化为 v4 再做网段匹配，否则 127.0.0.0/8 之类永远匹配不上。
fn normalize_ip(ip: std::net::IpAddr) -> std::net::IpAddr {
    match ip {
        std::net::IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => std::net::IpAddr::V4(v4),
            None => ip,
        },
        _ => ip,
    }
}

/// 最小 IP 网段（CIDR）：v4/v6 各一对 (网络号, 掩码)，匹配 = 掩码按位与相等。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum IpNet {
    V4 { net: u32, mask: u32 },
    V6 { net: u128, mask: u128 },
}

impl IpNet {
    /// "127.0.0.0/8"、"::1/128"、"10.0.0.5"（无前缀 = /32 或 /128）。
    pub fn parse(s: &str) -> Option<Self> {
        let (addr, prefix) = match s.trim().rsplit_once('/') {
            Some((a, p)) => (a, Some(p.parse::<u8>().ok()?)),
            None => (s.trim(), None),
        };
        match addr.parse::<std::net::IpAddr>().ok()? {
            std::net::IpAddr::V4(v) => {
                let bits = prefix.unwrap_or(32);
                if bits > 32 {
                    return None;
                }
                let mask = if bits == 0 { 0 } else { !0u32 << (32 - bits) };
                Some(IpNet::V4 { net: u32::from(v) & mask, mask })
            }
            std::net::IpAddr::V6(v) => {
                let bits = prefix.unwrap_or(128);
                if bits > 128 {
                    return None;
                }
                let mask = if bits == 0 { 0 } else { !0u128 << (128 - bits) };
                Some(IpNet::V6 { net: u128::from_be_bytes(v.octets()) & mask, mask })
            }
        }
    }

    pub fn contains(&self, ip: std::net::IpAddr) -> bool {
        let ip = normalize_ip(ip);
        match (self, ip) {
            (IpNet::V4 { net, mask }, std::net::IpAddr::V4(v)) => (u32::from(v) & mask) == *net,
            (IpNet::V6 { net, mask }, std::net::IpAddr::V6(v)) => {
                (u128::from_be_bytes(v.octets()) & mask) == *net
            }
            _ => false,
        }
    }
}

/// Paths that don't require authentication. 精确匹配——前缀匹配会让
/// `/api/healthX`、`/changelogX` 这类变体免认证直达 SPA fallback（渗透
/// 测试发现的泄露出口），非规范形态一律按需认证。
const PUBLIC_PATHS: &[&str] = &[
    "/api/health",
    "/api/settings/auth",
    // Login page posts here before any session exists.
    "/api/auth/login",
    // Logout must work even when auth state is broken — it only expires
    // cookies and redirects, so there is nothing to protect.
    "/api/logout",
    // 对外更新日志：自带密码门（[changelog].password），允许外部域名跨域调用，
    // 不应再叠加登录体系 —— 否则客户站点根本无法拉取。
    "/api/changelog",
    // 更新日志页同理：只有独立密码，不参与登录体系（登录页底部链接的落点，
    // SPA 在匿名下加载不出来，所以一律回自包含页面，见 static_files::changelog_page）。
    "/changelog",
];

/// 唯一保留子树语义的公开前缀：oauth2-proxy 的对外端点群
/// （/oauth2/sign_in、/oauth2/callback 等），由 proxy 自身做认证。
const PUBLIC_PATH_PREFIXES: &[&str] = &["/oauth2/"];

/// 按原始 path 判定公开路径：精确集合 + `/oauth2/` 子树。
/// 不做 percent-decode / dot-segment 归一化——归一化后再匹配会把
/// `/api/health/%2e%2e/`、`/api%2Fhealth` 之类变体折叠成公开路径，
/// 而路由器按原始形态分发，两者一旦不一致就重新打开泄露口。
/// 非字面形态（尾斜杠、编码、大小写）全部落到"需认证"这一安全默认侧。
fn is_public_path(path: &str) -> bool {
    PUBLIC_PATHS.contains(&path) || PUBLIC_PATH_PREFIXES.iter().any(|p| path.starts_with(p))
}

/// plain 模式下可匿名的路径：`/login` 页面 + 验证码接口（登录页自举所需）。
/// 由后端内嵌的自包含登录页服务（static_files::login_page），不依赖 SPA
/// bundle —— 其余所有路径（含 `/`、`/assets/*`、`/version.json`）未登录一律
/// 302，前端结构不泄露。
pub fn is_plain_public_path(path: &str) -> bool {
    path == "/login" || path == "/api/captcha"
}

/// POST /api/ws-ticket — mint a one-time ticket for a WebSocket upgrade.
/// Requires normal (Basic/OAuth) authentication like any other API route.
pub async fn issue_ws_ticket(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(serde_json::json!({ "ticket": state.ws_tickets.issue() }))
}

/// 真实 WebSocket 握手判定：GET + `Upgrade: websocket` + `Connection: Upgrade`
/// + `Sec-WebSocket-Key`。后三样是 fetch/XHR 无法伪造的 forbidden headers，
/// 仅有 `Upgrade` 头的 REST 请求（渗透测试发现的认证绕过载荷）不构成握手。
fn is_websocket_handshake(req: &axum::extract::Request) -> bool {
    use axum::http::Method;
    if req.method() != Method::GET {
        return false;
    }
    let headers = req.headers();
    let upgrade_ws = headers
        .get(axum::http::header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    let connection_upgrade = headers
        .get(axum::http::header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_ascii_lowercase().contains("upgrade"));
    upgrade_ws
        && connection_upgrade
        && headers.contains_key(axum::http::header::SEC_WEBSOCKET_KEY)
}

/// `/api/terminal/{id}/ws` 的精确匹配：id 为单段非空、结尾必须是 `/ws`。
/// 前缀级放行会让 list/create/resize 等 REST 路由带上 Upgrade 头匿名直达。
fn is_terminal_ws_path(path: &str) -> bool {
    match path.strip_prefix("/api/terminal/") {
        Some(rest) => match rest.rsplit_once('/') {
            Some((id, "ws")) => !id.is_empty() && !id.contains('/'),
            _ => false,
        },
        None => false,
    }
}

/// Authentication middleware
/// When OAuth is enabled, checks if request has been authenticated by OAuth2 Proxy
/// OAuth2 Proxy injects X-Auth-Request-User header after successful authentication
pub async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    req: axum::extract::Request,
    next: Next,
) -> Response {
    let path = req.uri().path().to_string();
    let auth = &state.config_manager.config().auth;

    // Check if path is public
    if is_public_path(&path) {
        return next.run(req).await;
    }

    // /p/ reverse proxy: browser tab navigations cannot attach an Authorization
    // header, so proxied links carry an HMAC signature in the query (minted by
    // POST /api/forwards/proxy-link, bound to the session+port in the path).
    // Relative asset requests drop the query during URL resolution, so the
    // first validated request plants an HttpOnly cookie scoped to /p that
    // authenticates the rest of the page's subresources. Invalid or missing
    // credentials fall through to the normal auth checks below.
    if path.starts_with("/p/") {
        let query_token = req
            .uri()
            .query()
            .and_then(|q| crate::runtime::web_proxy::query_param(q, "_pwt"))
            .map(str::to_string);
        let cookie_token = req
            .headers()
            .get(header::COOKIE)
            .and_then(|v| v.to_str().ok())
            .and_then(|c| crate::runtime::web_proxy::cookie_param(c, "_pwt"))
            .map(str::to_string);
        if let Some((session_key, port)) = crate::runtime::web_proxy::parse_proxy_path(&path) {
            if let Some(token) = query_token.clone().or_else(|| cookie_token.clone()) {
                let secret = crate::runtime::web_proxy::proxy_secret(&state.settings_store);
                let now = chrono::Utc::now().timestamp();
                if crate::runtime::web_proxy::verify_proxy_token(
                    &secret,
                    &token,
                    &session_key,
                    port,
                    now,
                ) {
                    let mut resp = next.run(req).await;
                    if query_token.is_some() && cookie_token.as_deref() != Some(token.as_str()) {
                        let ttl = crate::runtime::web_proxy::token_exp(&token)
                            .map(|exp| (exp - now).max(1))
                            .unwrap_or(0);
                        let cookie =
                            format!("_pwt={token}; Path=/p; HttpOnly; SameSite=Lax; Max-Age={ttl}");
                        if let Ok(v) = cookie.parse() {
                            resp.headers_mut().append(header::SET_COOKIE, v);
                        }
                    }
                    return resp;
                }
            }
        }
    }

    // NOTE: there is deliberately no MILLET_ENV=debug bypass here. This
    // machine is reachable from the internet through a gateway, and a dev
    // server running with debug env must not disable authentication.

    // WebSocket upgrades: browsers cannot set custom headers on the WebSocket
    // API, so normal Basic auth can't apply. Exemption is restricted to
    // genuine handshake requests (see is_websocket_handshake) targeting the
    // two real WS endpoints — each validates its own credential after the
    // upgrade (terminal: per-session token subprotocol; tunnel: query token).
    // Any other path — including REST routes merely carrying an
    // `Upgrade: websocket` header — falls through to normal authentication.
    if is_websocket_handshake(&req) {
        if path == "/api/tunnels/connect" || is_terminal_ws_path(&path) {
            return next.run(req).await;
        }
        let ticket = req
            .uri()
            .query()
            .and_then(|q| {
                q.split('&').find_map(|kv| {
                    let (k, v) = kv.split_once('=')?;
                    (k == "ticket").then(|| v.to_string())
                })
            })
            .unwrap_or_default();
        if !ticket.is_empty() && state.ws_tickets.verify(&ticket) {
            return next.run(req).await;
        }
        return (StatusCode::UNAUTHORIZED, "WebSocket authentication required")
            .into_response();
    }

    // plain 模式：Bearer token（脚本/测试客户端）→ 内存会话 cookie（浏览器）。
    // 凭据只来自 [auth.plain] 配置；与 basic 不混用，startup 校验已把两种模式隔离开。
    if auth.effective_mode() == "plain" {
        if is_plain_public_path(&path) {
            return next.run(req).await;
        }
        return plain_authenticate(State(state), req, next).await;
    }

    // oauth2_proxy 模式：上游 oauth2-proxy 注入 X-Auth-Request-User 后放行；
    // 直连后端（绕过 proxy）则用 [auth.oauth2_proxy] 的 basic_auth 兜底保护。
    if auth.effective_mode() == "oauth2_proxy" {
        // OAuth2 Proxy injects these headers after successful authentication
        if let Some(user_header) = req.headers().get("X-Auth-Request-User") {
            let username = user_header.to_str().unwrap_or("unknown").to_string();

            // Check if user is authorized (in admins or users list)
            if !auth.is_allowed(&username) {
                return (StatusCode::FORBIDDEN, Json(serde_json::json!({
                    "error": "User not authorized",
                    "username": username
                })))
                    .into_response();
            }

            let basic_auth_user = auth.oauth2_proxy.basic_auth_user.as_deref()
                .expect("auth.oauth2_proxy.basic_auth_user is required in oauth2_proxy mode");
            let basic_auth_password = auth.basic_auth_password_resolved()
                .expect("auth.oauth2_proxy.basic_auth_password is required in oauth2_proxy mode");
            let basic_auth_header = format!(
                "Basic {}",
                base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    format!("{}:{}", basic_auth_user, basic_auth_password)
                )
            );

            let mut req = req;
            req.headers_mut().insert(
                header::AUTHORIZATION,
                basic_auth_header.parse().unwrap(),
            );
            req.headers_mut().insert(
                "X-Auth-User",
                username.parse().unwrap(),
            );

            return next.run(req).await;
        }

        // Fall through to check Basic Auth directly (for API access)
        if let Some(auth_header) = req.headers().get(header::AUTHORIZATION) {
            if let Ok(auth_str) = auth_header.to_str() {
                if auth_str.starts_with("Basic ") {
                    let expected_user = auth.oauth2_proxy.basic_auth_user.as_deref()
                        .expect("auth.oauth2_proxy.basic_auth_user is required in oauth2_proxy mode");
                    let expected_password = auth.basic_auth_password_resolved()
                        .expect("auth.oauth2_proxy.basic_auth_password is required in oauth2_proxy mode");
                    let expected_creds = base64::Engine::encode(
                        &base64::engine::general_purpose::STANDARD,
                        format!("{}:{}", expected_user, expected_password),
                    );
                    let expected_header = format!("Basic {}", expected_creds);

                    if auth_str == expected_header {
                        return next.run(req).await;
                    }
                }
            }
        }

        // Not authenticated - 401 + WWW-Authenticate：直连后端的客户端
        // （浏览器/curl）会弹出 Basic 认证，而不是拿到一个死胡同 JSON。
        let mut resp = (StatusCode::UNAUTHORIZED, Json(serde_json::json!({
            "error": "Authentication required"
        })))
            .into_response();
        if let Ok(v) = header::HeaderValue::from_str("Basic realm=\"MilletCode\"") {
            resp.headers_mut().insert(header::WWW_AUTHENTICATE, v);
        }
        return resp;
    }

    // startup 校验保证只有 plain / oauth2_proxy 两种配置能跑到这里；
    // oauth 信任头缺失时统一 401（前端按 mode 决定跳转目标）。
    (StatusCode::UNAUTHORIZED, Json(serde_json::json!({
        "error": "Authentication required"
    })))
        .into_response()
}

/// plain 模式认证：Bearer token → 会话 cookie → 未认证响应。
/// 绝不发 WWW-Authenticate: Basic —— 浏览器原生弹窗正是要替换掉的体验。
async fn plain_authenticate(
    State(state): State<Arc<AppState>>,
    req: axum::extract::Request,
    next: Next,
) -> Response {
    let auth = &state.config_manager.config().auth;

    // 1) Bearer token，按前缀分发：
    //    sk- 开头 = 客户侧静态 token（constant_time_eq 比对 [auth.plain].token）；
    //    其余格式 = 内部测试时间槽 token（slot:HMAC(secret,slot)，secret 不出服务器）。
    if let Some(header_value) = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        if let Some(token) = header_value.strip_prefix("Bearer ") {
            let token = token.trim();
            let ok = if token.starts_with("sk-") {
                !auth.plain.token.is_empty() && constant_time_eq(token, &auth.plain.token)
            } else {
                !auth.plain.token_secret.is_empty()
                    && verify_time_token(&auth.plain.token_secret, token)
            };
            if ok {
                return next.run(req).await;
            }
        }
    }

    // 2) 内存会话 cookie（浏览器登录签发，滑动过期）
    if let Some(cookie) = req
        .headers()
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
    {
        if let Some(sid) = plain_session_cookie_value(cookie) {
            if state.plain_sessions.verify(sid) {
                return next.run(req).await;
            }
        }
    }

    // 3) 未认证：浏览器页面导航（Accept: text/html）302 到 /login 带回跳地址；
    // 其余一律 401 JSON。fetch/XHR 默认发 Accept: */*，curl 也是 */*——
    // 按「是不是 json」判断会把真实的 API 调用误判成页面请求。
    let wants_html = req
        .headers()
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("text/html"))
        .unwrap_or(false);

    if wants_html {
        let target = req
            .uri()
            .path_and_query()
            .map(|pq| pq.as_str().to_string())
            .unwrap_or_else(|| req.uri().path().to_string());
        Redirect::to(&format!("/login?next={}", urlencoding::encode(&target))).into_response()
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "Authentication required" })),
        )
            .into_response()
    }
}

pub(crate) fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handshake_request(method: &str, headers: &[(&str, &str)]) -> axum::extract::Request {
        let mut builder = axum::http::Request::builder().method(method).uri("/api/terminal/x/ws");
        for (k, v) in headers {
            builder = builder.header(*k, *v);
        }
        builder.body(axum::body::Body::empty()).unwrap()
    }

    #[test]
    fn websocket_handshake_requires_full_forbidden_header_set() {
        // 真实浏览器握手
        let full = handshake_request(
            "GET",
            &[
                ("upgrade", "websocket"),
                ("connection", "Upgrade"),
                ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
            ],
        );
        assert!(is_websocket_handshake(&full));
        // upgrade 头大小写不敏感
        let ci = handshake_request(
            "GET",
            &[
                ("upgrade", "WebSocket"),
                ("connection", "keep-alive, Upgrade"),
                ("sec-websocket-key", "k"),
            ],
        );
        assert!(is_websocket_handshake(&ci));

        // 渗透测试载荷：REST 请求仅伪造 Upgrade 头（缺 Connection/Key，可被
        // fetch 自由设置的头部）不构成握手，必须落入正常认证
        let forged = handshake_request("POST", &[("upgrade", "websocket")]);
        assert!(!is_websocket_handshake(&forged));
        let get_only_header = handshake_request("GET", &[("upgrade", "websocket")]);
        assert!(!is_websocket_handshake(&get_only_header));
        let no_key = handshake_request(
            "GET",
            &[("upgrade", "websocket"), ("connection", "Upgrade")],
        );
        assert!(!is_websocket_handshake(&no_key));
        // DELETE 等写方法即使头部齐全也不是握手
        let delete_ws = handshake_request(
            "DELETE",
            &[
                ("upgrade", "websocket"),
                ("connection", "Upgrade"),
                ("sec-websocket-key", "k"),
            ],
        );
        assert!(!is_websocket_handshake(&delete_ws));
    }

    #[test]
    fn terminal_ws_path_match_is_exact() {
        assert!(is_terminal_ws_path("/api/terminal/abc123/ws"));
        assert!(is_terminal_ws_path(
            "/api/terminal/0192a7e5-9c1b-7ccf-a6b0-3f1e6f0b8f2e/ws"
        ));
        // REST 路由不得命中豁免
        assert!(!is_terminal_ws_path("/api/terminal/list"));
        assert!(!is_terminal_ws_path("/api/terminal/create"));
        assert!(!is_terminal_ws_path("/api/terminal/abc/resize"));
        assert!(!is_terminal_ws_path("/api/terminal/abc/input"));
        assert!(!is_terminal_ws_path("/api/terminal/abc"));
        // 尾斜杠/多段/空 id 变体
        assert!(!is_terminal_ws_path("/api/terminal/abc/ws/"));
        assert!(!is_terminal_ws_path("/api/terminal/a/b/ws"));
        assert!(!is_terminal_ws_path("/api/terminal//ws"));
        assert!(!is_terminal_ws_path("/api/terminal/ws"));
        assert!(!is_terminal_ws_path("/api/terminal/list/"));
        assert!(!is_terminal_ws_path("/api/terminalistics/x/ws"));
    }

    #[test]
    fn ticket_is_single_use() {
        let t = WsTickets::new();
        let ticket = t.issue();
        assert!(t.verify(&ticket));
        assert!(!t.verify(&ticket));
    }

    #[test]
    fn unknown_ticket_rejected() {
        let t = WsTickets::new();
        assert!(!t.verify("not-a-ticket"));
        assert!(!t.verify(""));
    }

    #[test]
    fn expired_ticket_rejected() {
        let t = WsTickets::new();
        let ticket = t.issue();
        t.inner
            .lock()
            .insert(ticket.clone(), Instant::now() - Duration::from_secs(1));
        assert!(!t.verify(&ticket));
    }

    #[test]
    fn issue_prunes_expired_entries() {
        let t = WsTickets::new();
        let stale = t.issue();
        t.inner
            .lock()
            .insert(stale, Instant::now() - Duration::from_secs(1));
        t.issue();
        assert_eq!(t.inner.lock().len(), 1);
    }

    #[test]
    fn plain_session_issue_verify_revoke() {
        let s = PlainSessions::new();
        let sid = s.issue();
        assert!(s.verify(&sid));
        s.revoke(&sid);
        assert!(!s.verify(&sid));
        assert!(!s.verify("never-issued"));
    }

    #[test]
    fn plain_session_expires_and_is_removed() {
        let s = PlainSessions::new();
        let sid = s.issue();
        s.inner
            .lock()
            .insert(sid.clone(), Instant::now() - Duration::from_secs(1));
        assert!(!s.verify(&sid));
        assert!(s.inner.lock().get(&sid).is_none(), "expired entry must be dropped");
    }

    #[test]
    fn plain_session_verify_slides_expiry() {
        let s = PlainSessions::new();
        let sid = s.issue();
        let before = *s.inner.lock().get(&sid).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        assert!(s.verify(&sid));
        let after = *s.inner.lock().get(&sid).unwrap();
        assert!(after > before, "verify must extend the session (sliding TTL)");
    }

    #[test]
    fn plain_session_cookie_parses_from_header() {
        assert_eq!(
            plain_session_cookie_value("other=x; millet_session=abc; more=y"),
            Some("abc")
        );
        assert_eq!(plain_session_cookie_value("millet_session=x"), Some("x"));
        assert_eq!(plain_session_cookie_value("a=1"), None);
        assert_eq!(plain_session_cookie_value(""), None);
    }

    #[test]
    fn constant_time_eq_basic() {
        assert!(constant_time_eq("tok", "tok"));
        assert!(!constant_time_eq("tok", "toK"));
        assert!(!constant_time_eq("tok", "to"));
        assert!(!constant_time_eq("", "x"));
        assert!(constant_time_eq("", ""));
    }

    #[test]
    fn plain_public_paths_are_login_and_captcha_only() {
        assert!(is_plain_public_path("/login"));
        assert!(is_plain_public_path("/api/captcha"));
        // SPA 壳、静态资源、版本信息全部在认证后：未登录不给任何前端结构
        assert!(!is_plain_public_path("/"));
        assert!(!is_plain_public_path("/assets/index-abc.js"));
        assert!(!is_plain_public_path("/logos/color-1.png"));
        assert!(!is_plain_public_path("/version.json"));
        assert!(!is_plain_public_path("/favicon.ico"));
        assert!(!is_plain_public_path("/sessions"));
        assert!(!is_plain_public_path("/projects/x/y"));
        assert!(!is_plain_public_path("/p/org/proj/agent/sess/8080"));
        assert!(!is_plain_public_path("/api/sessions"));
    }

    /// 渗透测试（ARTEX）PoC 变体全集：前缀撞车、尾斜杠、dot-segment、
    /// percent-encode、大小写——全部不得命中公开路径（按需认证）。
    #[test]
    fn public_path_prefix_variants_are_not_public() {
        for p in [
            "/api/healthX",
            "/api/health/",
            "/api/health/foo",
            "/api/health/%2e%2e/",
            "/api/health/../",
            "/api/health%2e",
            "/api//health",
            "/API/HEALTH",
            "/api/logoutX",
            "/api/auth/loginX",
            "/api/settings/authX",
            "/api/changelogX",
            "/changelogX",
            "/changelog/foo",
            "/oauth2X",
        ] {
            assert!(!is_public_path(p), "must not be public: {p}");
        }
    }

    #[test]
    fn public_path_exact_and_oauth2_subtree() {
        for p in [
            "/api/health",
            "/api/auth/login",
            "/api/logout",
            "/api/settings/auth",
            "/api/changelog",
            "/changelog",
            "/oauth2/",
            "/oauth2/sign_in",
            "/oauth2/callback",
        ] {
            assert!(is_public_path(p), "must be public: {p}");
        }
        // 认证后才可见的路径不在公开集合
        for p in [
            "/",
            "/api/sessions",
            "/api/settings",
            "/assets/index-abc.js",
            "/version.json",
            "/api/captcha",
            "/login",
            "/oauth2",
        ] {
            assert!(!is_public_path(p), "must not be public: {p}");
        }
    }

    #[test]
    fn time_token_current_slot_accepted() {
        let secret = "test-secret";
        let token = format!("{}:{}", current_time_slot(), time_token_compute(secret, current_time_slot()));
        assert!(verify_time_token(secret, &token));
    }

    #[test]
    fn time_token_previous_slot_accepted_next_rejected() {
        let secret = "test-secret";
        let prev = current_time_slot() - 1;
        assert!(verify_time_token(
            secret,
            &format!("{}:{}", prev, time_token_compute(secret, prev))
        ));
        let future = current_time_slot() + 2;
        assert!(!verify_time_token(
            secret,
            &format!("{}:{}", future, time_token_compute(secret, future))
        ));
    }

    #[test]
    fn time_token_wrong_secret_or_garbage_rejected() {
        let token = format!(
            "{}:{}",
            current_time_slot(),
            time_token_compute("secret-a", current_time_slot())
        );
        assert!(!verify_time_token("secret-b", &token));
        for garbage in ["", "noslot", "abc:def", "123:zz", "123:", "::"] {
            assert!(!verify_time_token("secret-a", garbage), "must reject {garbage:?}");
        }
    }

    #[test]
    fn captcha_issue_verify_single_use() {
        let store = CaptchaStore::new();
        let (id, svg) = store.issue();
        assert!(!store.verify(&id, "wrong"));
        assert!(svg.contains("<svg"), "issue must return renderable svg");
        // 错误答案也销毁票据（单次使用）
        assert!(!store.verify(&id, "wrong"));
        // 正确答案已无法再通过
        let (id2, _) = store.issue();
        let code = store.inner.lock().get(&id2).unwrap().0.clone();
        assert!(store.verify(&id2, &code));
        assert!(!store.verify(&id2, &code), "must be single-use");
    }

    #[test]
    fn captcha_svg_leaks_no_plaintext() {
        let svg = captcha_svg("2345");
        // 无 <text> 节点：答案不能以明文出现在 DOM 里
        assert!(!svg.contains("<text"));
        assert!(!svg.contains(">2<"));
        assert!(svg.contains("<polyline"), "glyphs must render as strokes");
    }

    #[test]
    fn captcha_case_insensitive_and_expired_rejected() {
        let store = CaptchaStore::new();
        let (id, _) = store.issue();
        let code = store.inner.lock().get(&id).unwrap().0.clone();
        assert!(store.verify(&id, &code.to_uppercase()));
        let (id2, _) = store.issue();
        store.inner.lock().get_mut(&id2).unwrap().1 = Instant::now() - Duration::from_secs(1);
        let code2 = store.inner.lock().get(&id2).unwrap().0.clone();
        assert!(!store.verify(&id2, &code2));
        assert!(!store.verify("never-issued", "abc"));
    }

    #[test]
    fn throttle_blocks_after_max_failures_and_clear_resets() {
        let t = LoginThrottle::new();
        for _ in 0..LOGIN_MAX_FAILURES {
            assert!(t.check("1.2.3.4"));
            t.record_failure("1.2.3.4");
        }
        assert!(!t.check("1.2.3.4"), "6th attempt must be blocked");
        // 其它 key 不受影响
        assert!(t.check("5.6.7.8"));
        t.clear("1.2.3.4");
        assert!(t.check("1.2.3.4"));
    }

    #[test]
    fn throttle_window_expiry_restores_access() {
        let t = LoginThrottle::new();
        for _ in 0..LOGIN_MAX_FAILURES {
            t.record_failure("1.2.3.4");
        }
        assert!(!t.check("1.2.3.4"));
        // 把全部记录推到窗口之外
        let mut guard = t.inner.lock();
        *guard.get_mut("1.2.3.4").unwrap() = vec![Instant::now() - LOGIN_WINDOW - Duration::from_secs(1)];
        drop(guard);
        assert!(t.check("1.2.3.4"));
    }

    #[test]
    fn throttle_key_requires_trusted_proxy_peer() {
        let mut headers = axum::http::HeaderMap::new();
        let trusted = trusted_proxy_nets(&[]);
        // 可信代理（RFC1918）直连、无边缘标记：用 TCP 对端，忽略 XFF
        let lan: std::net::SocketAddr = "192.168.1.7:5555".parse().unwrap();
        headers.insert("x-forwarded-for", "8.8.8.8".parse().unwrap());
        assert_eq!(login_throttle_key(&headers, Some(lan), &trusted), "192.168.1.7");
        assert_eq!(login_throttle_key(&headers, None, &trusted), "local");
        // 可信代理转发 + 边缘标记：取 XFF 末位（逐跳追加，末位是真实对端）
        headers.insert("x-millet-edge", "vite".parse().unwrap());
        headers.insert("x-forwarded-for", "8.8.8.8, 1.2.3.4".parse().unwrap());
        assert_eq!(login_throttle_key(&headers, Some(lan), &trusted), "1.2.3.4");
        // 边缘标记在但 XFF 空：回退 TCP 对端
        headers.insert("x-forwarded-for", "  ".parse().unwrap());
        assert_eq!(login_throttle_key(&headers, Some(lan), &trusted), "192.168.1.7");
    }

    /// 渗透实测的绕过手法：公网直连 + 伪造 x-millet-edge + 伪造 XFF。
    /// TCP 对端不在可信网段，伪造头必须全部失效，key = 真实对端地址。
    #[test]
    fn throttle_key_ignores_forged_edge_from_untrusted_peer() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("x-millet-edge", "1".parse().unwrap());
        headers.insert("x-forwarded-for", "203.0.113.163".parse().unwrap());
        let attacker: std::net::SocketAddr = "203.0.113.9:5555".parse().unwrap();
        let trusted = trusted_proxy_nets(&[]);
        assert_eq!(
            login_throttle_key(&headers, Some(attacker), &trusted),
            "203.0.113.9"
        );
    }

    /// 显式配置 trusted_proxies 时整体替换默认集合（loopback 不再特殊）。
    #[test]
    fn throttle_key_custom_proxies_replace_defaults() {
        let custom = vec!["203.0.113.0/24".to_string()];
        let trusted = trusted_proxy_nets(&custom);
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("x-millet-edge", "1".parse().unwrap());
        headers.insert("x-forwarded-for", "9.9.9.9".parse().unwrap());
        let loopback: std::net::SocketAddr = "127.0.0.1:5555".parse().unwrap();
        assert_eq!(login_throttle_key(&headers, Some(loopback), &trusted), "127.0.0.1");
        let inside: std::net::SocketAddr = "203.0.113.5:5555".parse().unwrap();
        assert_eq!(login_throttle_key(&headers, Some(inside), &trusted), "9.9.9.9");
    }

    #[test]
    fn ip_net_parse_and_contains() {
        let loopback4 = IpNet::parse("127.0.0.0/8").unwrap();
        assert!(loopback4.contains("127.0.0.1".parse().unwrap()));
        assert!(!loopback4.contains("128.0.0.1".parse().unwrap()));
        // 裸 IP = /32
        let host = IpNet::parse("10.0.0.5").unwrap();
        assert!(host.contains("10.0.0.5".parse().unwrap()));
        assert!(!host.contains("10.0.0.6".parse().unwrap()));
        // /0 全量匹配
        assert!(IpNet::parse("0.0.0.0/0").unwrap().contains("8.8.8.8".parse().unwrap()));
        // v6 loopback 与 ULA
        let v6lo = IpNet::parse("::1/128").unwrap();
        assert!(v6lo.contains("::1".parse().unwrap()));
        assert!(!v6lo.contains("::2".parse().unwrap()));
        assert!(IpNet::parse("fd00::/8").unwrap().contains("fd12:3456::1".parse().unwrap()));
        // v4-mapped v6 归一化后按 v4 匹配
        let mapped: std::net::IpAddr = "::ffff:127.0.0.1".parse().unwrap();
        assert!(loopback4.contains(mapped));
        // v4 与 v6 网段互不匹配
        assert!(!loopback4.contains("::1".parse().unwrap()));
        // 非法输入
        for bad in ["10.0.0.0/33", "::1/129", "not-an-ip", "10.0.0.1/abc", ""] {
            assert!(IpNet::parse(bad).is_none(), "must reject {bad:?}");
        }
    }
}
