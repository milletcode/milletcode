//! code-server 式端口反向代理。
//!
//! 浏览器访问控制台 origin 下的 `/p/{session_key}/{port}/{rest}`，控制台在
//! HTTP 层把请求转发到 workspace 端口（Local 直连 / Ssh direct-tcpip，复用
//! connect_forward_target）。与端口转发（listener 在控制台）和 millet-tunnel
//! （listener 在用户机器）互补：用户侧零安装，代价是上游应用必须使用相对
//! 路径（注意事项已回写 milletcode-project-init 技能包）。
//!
//! 鉴权：新标签页打开 /p/ 链接时浏览器带不上 Authorization，所以链接由
//! POST /api/forwards/proxy-link 预签发，形如
//! `/p/{session_key}/{port}/?_pwt={exp}.{hmac}`；auth 中间件对 /p/ 前缀
//! 校验该签名（见 auth.rs），签名不过再落回常规认证。

use axum::extract::ws::{Message as AxumMessage, WebSocket, WebSocketUpgrade};
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::Request;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hmac::{Hmac, Mac};
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use sha2::Sha256;

use crate::runtime::port_forward::ForwardStream;

type HmacSha256 = Hmac<Sha256>;

/// `_pwt` 有效期（秒）：24h，覆盖一个工作日，泄露面仅限该 session/port
pub const PROXY_TOKEN_TTL_SECS: i64 = 24 * 3600;

/// 签名密钥在 redb 里的 key（不带 "settings." 前缀，不出现在设置 API 里）
pub const PROXY_SECRET_KEY: &str = "_proxy.secret";

// ---------- 签名链接 ----------

fn generate_secret() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// 读（或首次生成）代理签名密钥。并发首建时后写者胜，写完回读保证
/// 所有调用方拿到同一个值，先签发的 token 在极小竞态窗口内可能作废。
pub fn proxy_secret(store: &crate::db::SettingsStore) -> String {
    if let Ok(Some(s)) = store.get(PROXY_SECRET_KEY) {
        return s;
    }
    let s = generate_secret();
    let _ = store.set(PROXY_SECRET_KEY, &s);
    store.get(PROXY_SECRET_KEY).ok().flatten().unwrap_or(s)
}

fn token_payload(session_key: &str, port: u16, exp: i64) -> String {
    format!("{exp}|{session_key}|{port}")
}

/// `_pwt = "{exp}.{base64url(hmac_sha256(secret, payload))}"`
pub fn sign_proxy_token(secret: &str, session_key: &str, port: u16, exp: i64) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .expect("hmac accepts any key length");
    mac.update(token_payload(session_key, port, exp).as_bytes());
    format!("{exp}.{}", URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes()))
}

/// 校验签名与有效期（绑定 session_key + port，不能挪用到别的端口）。
pub fn verify_proxy_token(
    secret: &str,
    token: &str,
    session_key: &str,
    port: u16,
    now: i64,
) -> bool {
    let Some((exp_s, sig)) = token.split_once('.') else {
        return false;
    };
    let Ok(exp) = exp_s.parse::<i64>() else {
        return false;
    };
    if now >= exp {
        return false;
    }
    let Ok(sig_bytes) = URL_SAFE_NO_PAD.decode(sig) else {
        return false;
    };
    let mut mac = match HmacSha256::new_from_slice(secret.as_bytes()) {
        Ok(m) => m,
        Err(_) => return false,
    };
    mac.update(token_payload(session_key, port, exp).as_bytes());
    mac.verify_slice(&sig_bytes).is_ok()
}

/// `/p/{org}/{project}/{agent}/{name}/{port}[/...]` → (session_key, port)。
/// 形状不符返回 None（调用方落回常规认证）。
pub fn parse_proxy_path(path: &str) -> Option<(String, u16)> {
    let trimmed = path.trim_end_matches('/');
    let segs: Vec<&str> = trimmed.split('/').collect();
    if segs.len() < 7 || segs[1] != "p" {
        return None;
    }
    let port = segs[6].parse().ok()?;
    Some((segs[2..6].join("/"), port))
}

/// 从 query string 里取原始参数值（_pwt 是 base64url，无需 URL 解码）。
pub fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then_some(v)
    })
}

/// 从 Cookie 头里取参数值（"a=1; _pwt=x; b=2"）。
pub fn cookie_param<'a>(cookie_header: &'a str, key: &str) -> Option<&'a str> {
    cookie_header.split(';').find_map(|kv| {
        let (k, v) = kv.trim().split_once('=')?;
        (k == key).then_some(v)
    })
}

/// 取 token 里的 exp（"{exp}.{sig}"），用于计算 cookie Max-Age。
pub fn token_exp(token: &str) -> Option<i64> {
    token.split_once('.')?.0.parse().ok()
}

// ---------- 反代核心 ----------

/// 统一入口：WS 升级请求走 tungstenite 握手透传，普通 HTTP 走 hyper 一次性
/// 上游连接。`rest_path` 是剥掉 `/p/{session_key}/{port}` 前缀后的剩余路径
/// （可含 query），空串归一为 "/"。
pub async fn handle_proxy(
    stream: ForwardStream,
    rest_path: String,
    port: u16,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
    req: Request,
) -> Response {
    let rest = strip_pwt(&normalize_rest(&rest_path));
    match ws {
        Ok(upgrade) => upgrade
            .on_upgrade(move |socket| async move {
                proxy_ws(socket, stream, rest, port).await;
            })
            .into_response(),
        Err(_) => proxy_http(stream, req, rest, port).await,
    }
}

fn normalize_rest(rest: &str) -> String {
    if rest.is_empty() {
        "/".to_string()
    } else if rest.starts_with('/') {
        rest.to_string()
    } else {
        format!("/{rest}")
    }
}

/// 剥掉 query 里的 `_pwt`（控制台认证工件，不该泄漏给 workspace 应用，
/// 也避免上游对陌生参数敏感）。其余参数原样保留。
fn strip_pwt(rest: &str) -> String {
    let Some((path, query)) = rest.split_once('?') else {
        return rest.to_string();
    };
    let filtered: Vec<&str> = query
        .split('&')
        .filter(|kv| !kv.starts_with("_pwt=") && *kv != "_pwt")
        .collect();
    if filtered.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{}", filtered.join("&"))
    }
}

/// 转发给上游前剥掉的请求头：凭证（控制台的 Basic/OAuth 与 /p/ 的 _pwt 都
/// 不该泄漏给 workspace）与 hop-by-hop 头。
const STRIP_REQ_HEADERS: &[&str] = &[
    "authorization",
    "cookie",
    "host",
    "connection",
    "upgrade",
    "te",
    "trailer",
    "transfer-encoding",
    "proxy-authorization",
    "proxy-authenticate",
    "x-auth-user",
    "x-auth-request-user",
    "x-auth-request-email",
    "x-auth-request-groups",
];

async fn proxy_http(
    stream: ForwardStream,
    mut req: Request,
    rest: String,
    port: u16,
) -> Response {
    let upstream_uri: axum::http::Uri = match rest.parse() {
        Ok(u) => u,
        Err(e) => return bad_gateway(format!("invalid upstream path: {e}")),
    };
    *req.uri_mut() = upstream_uri;
    let headers = req.headers_mut();
    for h in STRIP_REQ_HEADERS {
        headers.remove(*h);
    }
    if let Ok(host) = format!("127.0.0.1:{port}").parse() {
        headers.insert(header::HOST, host);
    }

    // 每请求一次性连接：无连接池状态，Ssh/Local 都由调用方给 stream
    let (mut sender, conn) = match http1::handshake(TokioIo::new(stream)).await {
        Ok(v) => v,
        Err(e) => return bad_gateway(e),
    };
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            tracing::debug!("proxy upstream connection ended: {e}");
        }
    });
    match sender.send_request(req).await {
        Ok(resp) => resp.map(axum::body::Body::new).into_response(),
        Err(e) => bad_gateway(e),
    }
}

async fn proxy_ws(socket: WebSocket, stream: ForwardStream, rest: String, port: u16) {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let uri = format!("ws://127.0.0.1:{port}{rest}");
    let request = match uri.into_client_request() {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("proxy ws handshake request build failed: {e}");
            return;
        }
    };
    // client_async 在既有 ForwardStream 上完成 WS 握手（Sec-WebSocket-Key
    // 由 tungstenite 生成；下游子协议头不透传，开发服务器场景用不到）
    let (mut upstream, _resp) = match tokio_tungstenite::client_async(request, stream).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("proxy ws upstream handshake failed: {e}");
            return;
        }
    };
    pump_ws(socket, &mut upstream).await;
}

/// 双向消息泵：任一侧关闭/出错即退出，并向两侧补发 Close 尽量优雅收尾。
async fn pump_ws(
    mut socket: WebSocket,
    upstream: &mut tokio_tungstenite::WebSocketStream<ForwardStream>,
) {
    use futures::{SinkExt, StreamExt};

    loop {
        tokio::select! {
            down = socket.next() => match down {
                None | Some(Err(_)) => break,
                Some(Ok(m)) => {
                    if upstream.send(to_tungstenite(m)).await.is_err() {
                        break;
                    }
                }
            },
            up = upstream.next() => match up {
                None | Some(Err(_)) => break,
                Some(Ok(m)) => {
                    if socket.send(to_axum(m)).await.is_err() {
                        break;
                    }
                }
            },
        }
    }
    let _ = socket.send(AxumMessage::Close(None)).await;
    let _ = upstream.send(tokio_tungstenite::tungstenite::Message::Close(None)).await;
}

// axum 0.8 把 tungstenite 的 Utf8Bytes/CloseFrame 包成了自己的 newtype（内
// 部字段不公开），只能经 str/u16 往返转换；Bytes 两边是同一个类型
fn to_tungstenite(m: AxumMessage) -> tokio_tungstenite::tungstenite::Message {
    use tokio_tungstenite::tungstenite::Message as Ts;
    use tokio_tungstenite::tungstenite::protocol::CloseFrame as TsCloseFrame;

    match m {
        AxumMessage::Text(t) => Ts::Text(t.as_str().into()),
        AxumMessage::Binary(b) => Ts::Binary(b),
        AxumMessage::Ping(p) => Ts::Ping(p),
        AxumMessage::Pong(p) => Ts::Pong(p),
        AxumMessage::Close(f) => Ts::Close(f.map(|f| TsCloseFrame {
            code: f.code.into(),
            reason: f.reason.as_str().into(),
        })),
    }
}

fn to_axum(m: tokio_tungstenite::tungstenite::Message) -> AxumMessage {
    use axum::extract::ws::CloseFrame as AxumCloseFrame;
    use tokio_tungstenite::tungstenite::Message as Ts;
    use tokio_tungstenite::tungstenite::protocol::CloseFrame as TsCloseFrame;

    match m {
        Ts::Text(t) => AxumMessage::Text(t.as_str().into()),
        Ts::Binary(b) => AxumMessage::Binary(b),
        Ts::Ping(p) => AxumMessage::Ping(p),
        Ts::Pong(p) => AxumMessage::Pong(p),
        Ts::Close(f) => AxumMessage::Close(f.map(|f| AxumCloseFrame {
            code: f.code.into(),
            reason: f.reason.as_str().into(),
        })),
        Ts::Frame(_) => AxumMessage::Binary(Vec::new().into()),
    }
}

pub(crate) fn bad_gateway(e: impl std::fmt::Display) -> Response {
    (
        StatusCode::BAD_GATEWAY,
        Json(serde_json::json!({ "error": format!("upstream unreachable: {e}") })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use axum::routing::{any, get};
    use axum::Router;
    use serde_json::Value;

    // ---------- 签名 ----------

    #[test]
    fn token_sign_verify_roundtrip() {
        let secret = "test-secret";
        let exp = 2_000_000_000_000i64;
        let token = sign_proxy_token(secret, "org/proj/claude/s1", 8123, exp);
        assert!(verify_proxy_token(secret, &token, "org/proj/claude/s1", 8123, exp - 1));
    }

    #[test]
    fn token_expired_rejected() {
        let token = sign_proxy_token("s", "sk", 80, 100);
        assert!(!verify_proxy_token("s", &token, "sk", 80, 100));
        assert!(!verify_proxy_token("s", &token, "sk", 80, 101));
    }

    #[test]
    fn token_bound_to_session_and_port() {
        let token = sign_proxy_token("s", "a/b/c/d", 3000, 999_999);
        assert!(!verify_proxy_token("s", &token, "a/b/c/e", 3000, 1));
        assert!(!verify_proxy_token("s", &token, "a/b/c/d", 3001, 1));
    }

    #[test]
    fn token_wrong_secret_or_tampered_rejected() {
        let token = sign_proxy_token("secret-one", "sk", 1, 999_999);
        assert!(!verify_proxy_token("secret-two", &token, "sk", 1, 1));
        let tampered = format!("999998.{}", &token[8..]);
        assert_ne!(tampered, token);
        assert!(!verify_proxy_token("secret-one", &tampered, "sk", 1, 1));
        assert!(!verify_proxy_token("secret-one", "garbage", "sk", 1, 1));
        assert!(!verify_proxy_token("secret-one", "", "sk", 1, 1));
        assert!(!verify_proxy_token("secret-one", "123", "sk", 1, 1));
    }

    // ---------- 路径解析 ----------

    #[test]
    fn parses_proxy_path_shapes() {
        assert_eq!(
            parse_proxy_path("/p/org/proj/claude/default/8123/"),
            Some(("org/proj/claude/default".to_string(), 8123))
        );
        assert_eq!(
            parse_proxy_path("/p/org/proj/claude/default/8123"),
            Some(("org/proj/claude/default".to_string(), 8123))
        );
        assert_eq!(
            parse_proxy_path("/p/o/p/a/n/123/deep/path?x=1"),
            Some(("o/p/a/n".to_string(), 123))
        );
        assert_eq!(parse_proxy_path("/p/org/proj/claude"), None);
        assert_eq!(parse_proxy_path("/p/org/proj/claude/default"), None);
        assert_eq!(parse_proxy_path("/p/org/proj/claude/default/notaport"), None);
        assert_eq!(parse_proxy_path("/api/sessions"), None);
    }

    #[test]
    fn extracts_query_param() {
        assert_eq!(query_param("a=1&_pwt=abc&b=2", "_pwt"), Some("abc"));
        assert_eq!(query_param("_pwt=abc", "_pwt"), Some("abc"));
        assert_eq!(query_param("a=1", "_pwt"), None);
        assert_eq!(query_param("", "_pwt"), None);
    }

    #[test]
    fn strips_pwt_but_keeps_other_params() {
        assert_eq!(strip_pwt("/x?_pwt=tok&a=1&b=2"), "/x?a=1&b=2");
        assert_eq!(strip_pwt("/x?a=1&_pwt=tok"), "/x?a=1");
        assert_eq!(strip_pwt("/x?_pwt=tok"), "/x");
        assert_eq!(strip_pwt("/x?_pwt"), "/x");
        assert_eq!(strip_pwt("/x?a=1"), "/x?a=1");
        assert_eq!(strip_pwt("/x"), "/x");
    }

    #[test]
    fn parses_cookie_param_and_token_exp() {
        assert_eq!(cookie_param("a=1; _pwt=tok; b=2", "_pwt"), Some("tok"));
        assert_eq!(cookie_param("_pwt=tok", "_pwt"), Some("tok"));
        assert_eq!(cookie_param("a=1", "_pwt"), None);
        assert_eq!(cookie_param("", "_pwt"), None);
        assert_eq!(token_exp("12345.ABCDEF"), Some(12345));
        assert_eq!(token_exp("bad"), None);
        assert_eq!(token_exp("x.y"), None);
    }

    // ---------- HTTP / WS 反代（真实回环链路） ----------

    /// 上游 echo：回显 method/path/query/关键头/请求体，供断言代理改写行为
    async fn reflect_handler(req: Request) -> Json<Value> {
        let method = req.method().to_string();
        let uri = req.uri().clone();
        let has_auth = req.headers().get("authorization").is_some();
        let has_cookie = req.headers().get("cookie").is_some();
        let host = req
            .headers()
            .get("host")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = axum::body::to_bytes(req.into_body(), 1024 * 1024)
            .await
            .unwrap_or_default();
        Json(serde_json::json!({
            "method": method,
            "path": uri.path(),
            "query": uri.query(),
            "has_auth": has_auth,
            "has_cookie": has_cookie,
            "host": host,
            "body": String::from_utf8_lossy(&body),
        }))
    }

    async fn ws_echo(ws: WebSocketUpgrade) -> Response {
        ws.on_upgrade(|mut socket: WebSocket| async move {
            use futures::{SinkExt, StreamExt};
            while let Some(Ok(msg)) = socket.next().await {
                if socket.send(msg).await.is_err() {
                    break;
                }
            }
        })
    }

    async fn spawn_upstream() -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // 全路径回显：/、/reflect、/assets/app.js 都返回请求元信息
        let app = Router::new()
            .route("/ws", get(ws_echo))
            .route("/", any(reflect_handler))
            .route("/{*any}", any(reflect_handler));
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        port
    }

    /// 被测代理路由：剥掉 /px 前缀，转发到 State 指定的上游端口——与
    /// api/proxy.rs 的 handler 同构，但不依赖 AppState。
    async fn px_handler(
        State(port): State<u16>,
        ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
        req: Request,
    ) -> Response {
        let rest = {
            let r = req.uri().path().strip_prefix("/px").unwrap_or("");
            let query = req
                .uri()
                .query()
                .map(|q| format!("?{q}"))
                .unwrap_or_default();
            format!("{r}{query}")
        };
        let stream = match tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
            Ok(s) => ForwardStream::Local(s),
            Err(e) => return bad_gateway(e),
        };
        handle_proxy(stream, rest, port, ws, req).await
    }

    fn px_router(port: u16) -> Router {
        Router::new()
            .route("/px", any(px_handler))
            .route("/px/", any(px_handler))
            .route("/px/{*rest}", any(px_handler))
            .with_state(port)
    }

    async fn spawn_router(router: Router) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        port
    }

    fn client() -> reqwest::Client {
        reqwest::Client::new()
    }

    #[tokio::test]
    async fn proxies_get_strips_prefix_query_and_credentials() {
        let upstream_port = spawn_upstream().await;
        let proxy_port = spawn_router(px_router(upstream_port)).await;

        let resp = client()
            .get(format!("http://127.0.0.1:{proxy_port}/px/reflect?a=1&b=two"))
            .basic_auth("alice", Some("hunter2"))
            .header("cookie", "session=xyz")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let v: Value = resp.json().await.unwrap();
        assert_eq!(v["method"], "GET");
        assert_eq!(v["path"], "/reflect");
        assert_eq!(v["query"], "a=1&b=two");
        assert_eq!(v["has_auth"], false);
        assert_eq!(v["has_cookie"], false);
        assert_eq!(v["host"], format!("127.0.0.1:{upstream_port}"));

        // _pwt 是控制台认证工件，不得透传给上游，其余参数保留
        let v: Value = client()
            .get(format!(
                "http://127.0.0.1:{proxy_port}/px/reflect?_pwt=tok123&real=1"
            ))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(v["query"], "real=1");
    }

    #[tokio::test]
    async fn proxies_post_body_roundtrip() {
        let upstream_port = spawn_upstream().await;
        let proxy_port = spawn_router(px_router(upstream_port)).await;

        let resp = client()
            .post(format!("http://127.0.0.1:{proxy_port}/px/reflect?x=y"))
            .body("hello-proxy-body")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let v: Value = resp.json().await.unwrap();
        assert_eq!(v["method"], "POST");
        assert_eq!(v["path"], "/reflect");
        assert_eq!(v["query"], "x=y");
        assert_eq!(v["body"], "hello-proxy-body");
    }

    #[tokio::test]
    async fn trailing_slash_maps_to_root_and_deep_paths_pass_through() {
        let upstream_port = spawn_upstream().await;
        let proxy_port = spawn_router(px_router(upstream_port)).await;

        // 签发的链接形如 /p/{sk}/{port}/ —— 尾斜杠必须命中代理且 rest="/"
        let root: Value = client()
            .get(format!("http://127.0.0.1:{proxy_port}/px/"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(root["path"], "/");

        // 无尾斜杠的精确路径同样命中（rest 归一为 "/"）
        let exact: Value = client()
            .get(format!("http://127.0.0.1:{proxy_port}/px"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(exact["path"], "/");

        // 深层路径原样透传
        let deep: Value = client()
            .get(format!("http://127.0.0.1:{proxy_port}/px/assets/app.js?v=3"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(deep["path"], "/assets/app.js");
        assert_eq!(deep["query"], "v=3");
    }

    #[tokio::test]
    async fn unreachable_upstream_returns_502() {
        // 找一个确定没人监听的端口
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_port = probe.local_addr().unwrap().port();
        drop(probe);

        let proxy_port = spawn_router(px_router(dead_port)).await;
        let resp = client()
            .get(format!("http://127.0.0.1:{proxy_port}/px/anything"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 502);
    }

    #[tokio::test]
    async fn proxies_websocket_echo() {
        use futures::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message as Ts;

        let upstream_port = spawn_upstream().await;
        let proxy_port = spawn_router(px_router(upstream_port)).await;

        let (mut ws, _resp) =
            tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{proxy_port}/px/ws"))
                .await
                .expect("ws connect through proxy should succeed");
        ws.send(Ts::Text("ping-through-proxy".into())).await.unwrap();
        let echoed = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
            .await
            .expect("echo timeout")
            .expect("stream ended")
            .expect("ws error");
        match echoed {
            Ts::Text(t) => assert_eq!(t.as_str(), "ping-through-proxy"),
            other => panic!("expected text echo, got {other:?}"),
        }
        let _ = ws.close(None).await;
    }
}
