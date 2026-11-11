//! millet-tunnel 服务端：WebSocket 隧道的控制台中点。
//!
//! 用户机器上的 `millet-tunnel` CLI 监听本地端口，TCP 流量按 tunnel_frame
//! 协议经一条 WS 连接多路复用到这里，再逐流桥到 workspace 端口
//! （Local 直连 / Ssh direct-tcpip，复用端口转发的连接逻辑）。
//!
//! 并发模型：所有 S→C 帧进全局 mpsc(256) 由唯一 writer 发送；每条流的
//! C→S 数据进该流 mpsc(64) 由流任务写上游——背压逐级传导，不丢数据。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::extract::ws::{Message, WebSocket};
use base64::Engine as _;
use futures::{SinkExt, StreamExt};
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

use crate::runtime::port_forward::{ForwardStream, ForwardTarget, connect_forward_target};
use crate::runtime::ssh::SharedSshHandle;
use crate::tunnel_frame::{self, TYPE_CLOSE, TYPE_DATA, TYPE_OPEN};

#[derive(Debug, thiserror::Error)]
pub enum TunnelError {
    #[error("该会话的远端端口已有隧道")]
    Duplicate,
}

pub struct TunnelEntry {
    pub id: String,
    pub session_key: String,
    pub remote_port: u16,
    /// 随机 256-bit token：CLI 连 WS 时自认证。DELETE 前长期有效，
    /// 断线后重跑 CLI 即恢复。
    pub token: String,
    pub connected: AtomicBool,
    pub cancel: CancellationToken,
    pub created_at: i64,
    /// 创建时快照的转发目标（与端口转发 accept_loop 持有 target 同语义）
    pub target: Arc<ForwardTarget>,
}

/// API 返回视图 —— 绝不包含 token
#[derive(Debug, Serialize)]
pub struct TunnelInfo {
    pub id: String,
    pub session_key: String,
    pub remote_port: u16,
    pub connected: bool,
    pub created_at: i64,
}

impl From<&TunnelEntry> for TunnelInfo {
    fn from(e: &TunnelEntry) -> Self {
        TunnelInfo {
            id: e.id.clone(),
            session_key: e.session_key.clone(),
            remote_port: e.remote_port,
            connected: e.connected.load(Ordering::SeqCst),
            created_at: e.created_at,
        }
    }
}

pub struct CreatedTunnel {
    pub info: TunnelInfo,
    pub token: String,
}

fn generate_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[derive(Default)]
pub struct TunnelManager {
    tunnels: HashMap<String, Arc<TunnelEntry>>,
}

impl TunnelManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn create(
        &mut self,
        session_key: &str,
        remote_port: u16,
        target: Arc<ForwardTarget>,
    ) -> Result<CreatedTunnel, TunnelError> {
        let id = format!("{}:{}", session_key, remote_port);
        if self.tunnels.contains_key(&id) {
            return Err(TunnelError::Duplicate);
        }
        let entry = Arc::new(TunnelEntry {
            id: id.clone(),
            session_key: session_key.to_string(),
            remote_port,
            token: generate_token(),
            connected: AtomicBool::new(false),
            cancel: CancellationToken::new(),
            created_at: chrono::Utc::now().timestamp(),
            target,
        });
        let info = TunnelInfo::from(entry.as_ref());
        let token = entry.token.clone();
        self.tunnels.insert(id, entry);
        Ok(CreatedTunnel { info, token })
    }

    pub fn list(&self, session_key: Option<&str>) -> Vec<TunnelInfo> {
        let mut infos: Vec<TunnelInfo> = self
            .tunnels
            .values()
            .filter(|e| session_key.map_or(true, |k| e.session_key == k))
            .map(|e| TunnelInfo::from(e.as_ref()))
            .collect();
        infos.sort_by_key(|i| i.created_at);
        infos
    }

    pub fn remove(&mut self, id: &str) -> bool {
        match self.tunnels.remove(id) {
            Some(e) => {
                e.cancel.cancel();
                true
            }
            None => false,
        }
    }

    pub fn find_by_token(&self, token: &str) -> Option<Arc<TunnelEntry>> {
        self.tunnels.values().find(|e| e.token == token).cloned()
    }

    /// id → (视图, token)。POST /api/tunnels 撞重复时用它幂等返回
    /// （token 重新下发——该接口本身已认证，等价于创建时的暴露面）。
    pub fn get_with_token(&self, id: &str) -> Option<(TunnelInfo, String)> {
        self.tunnels
            .get(id)
            .map(|e| (TunnelInfo::from(e.as_ref()), e.token.clone()))
    }
}

/// WS upgrade 后的入口：按 token 找隧道并进泵；找不到直接关闭。
pub async fn handle_tunnel_ws(manager: &Arc<Mutex<TunnelManager>>, token: &str, mut ws: WebSocket) {
    let entry = manager.lock().await.find_by_token(token);
    match entry {
        Some(e) => serve_tunnel(ws, e).await,
        None => {
            let _ = ws.close().await;
        }
    }
}

async fn serve_tunnel(mut ws: WebSocket, entry: Arc<TunnelEntry>) {
    // 同一隧道同时只允许一条 CLI 连接：后来者立即被关闭
    if entry.connected.swap(true, Ordering::SeqCst) {
        let _ = ws.close().await;
        return;
    }

    let (mut sink, mut stream) = ws.split();
    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(256);
    let writer = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            if sink.send(Message::Binary(frame.into())).await.is_err() {
                return;
            }
        }
        // 所有生产者退出（连接收尾）——尽力完成 close 握手
        let _ = sink.send(Message::Close(None)).await;
    });

    let conn_cancel = CancellationToken::new();
    let mut ssh: Option<SharedSshHandle> = None;
    let mut streams: HashMap<u32, mpsc::Sender<Vec<u8>>> = HashMap::new();

    let reason = loop {
        tokio::select! {
            _ = entry.cancel.cancelled() => break "deleted",
            _ = conn_cancel.cancelled() => break "done",
            msg = stream.next() => match msg {
                None | Some(Err(_)) => break "closed",
                Some(Ok(Message::Binary(data))) => {
                    let Some((ftype, sid, payload)) = tunnel_frame::decode(&data) else {
                        tracing::warn!("tunnel {}: invalid frame, dropping connection", entry.id);
                        break "protocol_error";
                    };
                    match ftype {
                        TYPE_OPEN => {
                            if streams.contains_key(&sid) {
                                continue;
                            }
                            match connect_forward_target(&entry.target, entry.remote_port, &mut ssh).await {
                                Ok(upstream) => {
                                    let (ctl_tx, ctl_rx) = mpsc::channel::<Vec<u8>>(64);
                                    streams.insert(sid, ctl_tx);
                                    tokio::spawn(pump_stream(
                                        upstream,
                                        ctl_rx,
                                        out_tx.clone(),
                                        sid,
                                        conn_cancel.clone(),
                                    ));
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "tunnel {}: connect port {} failed: {e:#}",
                                        entry.id,
                                        entry.remote_port
                                    );
                                    let _ = out_tx
                                        .send(tunnel_frame::encode(TYPE_CLOSE, sid, &[]))
                                        .await;
                                }
                            }
                        }
                        TYPE_DATA => {
                            if let Some(tx) = streams.get(&sid) {
                                if tx.send(payload.to_vec()).await.is_err() {
                                    // 流任务已退出
                                    let _ = out_tx
                                        .send(tunnel_frame::encode(TYPE_CLOSE, sid, &[]))
                                        .await;
                                }
                            } else {
                                let _ = out_tx
                                    .send(tunnel_frame::encode(TYPE_CLOSE, sid, &[]))
                                    .await;
                            }
                        }
                        TYPE_CLOSE => {
                            streams.remove(&sid);
                        }
                        _ => {}
                    }
                }
                Some(Ok(Message::Close(_))) => break "closed",
                Some(Ok(_)) => {} // Ping/Pong 由 axum 自动处理，Text 不应出现
            },
        }
    };

    // 掐断所有流任务 → drop 帧发送端 → writer 自然收尾
    streams.clear();
    conn_cancel.cancel();
    drop(out_tx);
    let _ = writer.await;
    entry.connected.store(false, Ordering::SeqCst);
    tracing::info!("tunnel {}: connection ended ({reason})", entry.id);
}

/// 单条流的泵：客户端 DATA → 上游写；上游读 → DATA 帧；任一侧结束发 CLOSE。
async fn pump_stream(
    mut upstream: ForwardStream,
    mut ctl: mpsc::Receiver<Vec<u8>>,
    out: mpsc::Sender<Vec<u8>>,
    sid: u32,
    conn_cancel: CancellationToken,
) {
    let mut buf = vec![0u8; tunnel_frame::CHUNK_SIZE];
    loop {
        tokio::select! {
            _ = conn_cancel.cancelled() => break,
            data = ctl.recv() => match data {
                Some(bytes) => {
                    if upstream.write_all(&bytes).await.is_err() {
                        break;
                    }
                }
                None => {
                    // 客户端半关闭这条流：停止写上游，读完剩余响应后结束
                    let _ = upstream.shutdown().await;
                    loop {
                        if conn_cancel.is_cancelled() {
                            break;
                        }
                        match upstream.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if out
                                    .send(tunnel_frame::encode(TYPE_DATA, sid, &buf[..n]))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        }
                    }
                    break;
                }
            },
            r = upstream.read(&mut buf) => match r {
                Ok(0) => break,
                Ok(n) => {
                    if out
                        .send(tunnel_frame::encode(TYPE_DATA, sid, &buf[..n]))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(_) => break,
            },
        }
    }
    let _ = upstream.shutdown().await;
    let _ = out.send(tunnel_frame::encode(TYPE_CLOSE, sid, &[])).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::{RawQuery, State};
    use axum::extract::ws::WebSocketUpgrade;
    use axum::routing::get;
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    fn local_manager() -> Arc<Mutex<TunnelManager>> {
        Arc::new(Mutex::new(TunnelManager::new()))
    }

    #[test]
    fn manager_crud_and_isolation() {
        let mut mgr = TunnelManager::new();
        let target = Arc::new(ForwardTarget::Local);
        let created = mgr
            .create("org/p/claude/s1", 8123, target.clone())
            .expect("create");
        assert_eq!(created.info.id, "org/p/claude/s1:8123");
        assert!(!created.token.is_empty());

        assert!(matches!(
            mgr.create("org/p/claude/s1", 8123, target.clone()),
            Err(TunnelError::Duplicate)
        ));
        // 同会话不同端口、不同会话同端口互不冲突
        assert!(mgr.create("org/p/claude/s1", 8124, target.clone()).is_ok());
        assert!(mgr.create("org/p/claude/s2", 8123, target.clone()).is_ok());

        assert_eq!(mgr.list(Some("org/p/claude/s1")).len(), 2);
        assert_eq!(mgr.list(None).len(), 3);

        let found = mgr.find_by_token(&created.token).expect("token lookup");
        assert_eq!(found.id, created.info.id);
        assert!(mgr.find_by_token("bogus").is_none());

        assert!(mgr.remove("org/p/claude/s1:8123"));
        assert!(!mgr.remove("org/p/claude/s1:8123"));
        assert!(mgr.create("org/p/claude/s1", 8123, target).is_ok());
    }

    #[test]
    fn tunnel_info_json_never_contains_token() {
        let mut mgr = TunnelManager::new();
        let created = mgr
            .create("org/p/claude/s1", 8123, Arc::new(ForwardTarget::Local))
            .unwrap();
        let json = serde_json::to_string(&created.info).unwrap();
        assert!(!json.contains("token"));
        assert!(!json.contains(&created.token));
    }

    async fn spawn_echo_server() -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 4096];
                    loop {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if sock.write_all(&buf[..n]).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
            }
        });
        port
    }

    async fn spawn_tunnel_server(manager: Arc<Mutex<TunnelManager>>) -> u16 {
        let app = axum::Router::new()
            .route(
                "/connect",
                get(|ws: WebSocketUpgrade, State(m): State<Arc<Mutex<TunnelManager>>>, RawQuery(q): RawQuery| async move {
                    let token = q
                        .and_then(|q| q.split("token=").nth(1).map(|t| t.split('&').next().unwrap_or(t).to_string()))
                        .unwrap_or_default();
                    ws.on_upgrade(move |socket| async move {
                        handle_tunnel_ws(&m, &token, socket).await;
                    })
                }),
            )
            .with_state(manager);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        port
    }

    type TestWs = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    async fn ws_connect(port: u16, token: &str) -> TestWs {
        let (ws, _) = tokio_tungstenite::connect_async(format!(
            "ws://127.0.0.1:{port}/connect?token={token}"
        ))
        .await
        .expect("ws connect");
        ws
    }

    /// 读到下一帧并解码，超时 5s 防挂死
    async fn next_frame(ws: &mut TestWs) -> (u8, u32, Vec<u8>) {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
            .await
            .expect("frame timeout")
            .expect("stream ended")
            .expect("ws error");
        match msg {
            WsMessage::Binary(data) => tunnel_frame::decode(&data).map(|(t, s, p)| (t, s, p.to_vec())).expect("valid frame"),
            other => panic!("expected binary frame, got {other:?}"),
        }
    }

    async fn send_frame(ws: &mut TestWs, ftype: u8, sid: u32, payload: &[u8]) {
        ws.send(WsMessage::Binary(tunnel_frame::encode(ftype, sid, payload).into()))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn tunnel_echo_roundtrip() {
        let echo_port = spawn_echo_server().await;
        let manager = local_manager();
        let created = manager
            .lock()
            .await
            .create("org/p/claude/s1", echo_port, Arc::new(ForwardTarget::Local))
            .unwrap();
        let port = spawn_tunnel_server(manager.clone()).await;

        let mut ws = ws_connect(port, &created.token).await;

        send_frame(&mut ws, TYPE_OPEN, 7, &[]).await;
        send_frame(&mut ws, TYPE_DATA, 7, b"ping").await;

        let (ftype, sid, payload) = next_frame(&mut ws).await;
        assert_eq!((ftype, sid, payload.as_slice()), (TYPE_DATA, 7, b"ping".as_slice()));

        send_frame(&mut ws, TYPE_CLOSE, 7, &[]).await;
        let (ftype, sid, _) = next_frame(&mut ws).await;
        assert_eq!((ftype, sid), (TYPE_CLOSE, 7));

        // 连接状态在 WS 存活期间为 true
        assert!(manager.lock().await.list(None)[0].connected);
    }

    #[tokio::test]
    async fn tunnel_concurrent_streams_do_not_interleave() {
        let echo_port = spawn_echo_server().await;
        let manager = local_manager();
        let created = manager
            .lock()
            .await
            .create("org/p/claude/s1", echo_port, Arc::new(ForwardTarget::Local))
            .unwrap();
        let port = spawn_tunnel_server(manager.clone()).await;

        let mut ws = ws_connect(port, &created.token).await;
        send_frame(&mut ws, TYPE_OPEN, 1, &[]).await;
        send_frame(&mut ws, TYPE_OPEN, 2, &[]).await;
        send_frame(&mut ws, TYPE_DATA, 1, b"one").await;
        send_frame(&mut ws, TYPE_DATA, 2, b"two").await;

        let mut got = HashMap::new();
        for _ in 0..2 {
            let (ftype, sid, payload) = next_frame(&mut ws).await;
            assert_eq!(ftype, TYPE_DATA);
            got.insert(sid, payload);
        }
        assert_eq!(got.remove(&1).as_deref(), Some(b"one".as_slice()));
        assert_eq!(got.remove(&2).as_deref(), Some(b"two".as_slice()));
    }

    #[tokio::test]
    async fn tunnel_open_unreachable_port_gets_close() {
        // 占一个端口再释放，拿到一个（几乎必然）未被监听的端口
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_port = l.local_addr().unwrap().port();
        drop(l);

        let manager = local_manager();
        let created = manager
            .lock()
            .await
            .create("org/p/claude/s1", dead_port, Arc::new(ForwardTarget::Local))
            .unwrap();
        let port = spawn_tunnel_server(manager.clone()).await;

        let mut ws = ws_connect(port, &created.token).await;
        send_frame(&mut ws, TYPE_OPEN, 3, &[]).await;

        let (ftype, sid, _) = next_frame(&mut ws).await;
        assert_eq!((ftype, sid), (TYPE_CLOSE, 3));
    }

    #[tokio::test]
    async fn second_connection_with_same_token_is_rejected() {
        let echo_port = spawn_echo_server().await;
        let manager = local_manager();
        let created = manager
            .lock()
            .await
            .create("org/p/claude/s1", echo_port, Arc::new(ForwardTarget::Local))
            .unwrap();
        let port = spawn_tunnel_server(manager.clone()).await;

        let mut first = ws_connect(port, &created.token).await;
        let mut second = ws_connect(port, &created.token).await;

        // 后来者应立即收到 Close / 连接结束
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), second.next())
            .await
            .expect("second conn should end")
            .expect("stream ended");
        assert!(matches!(msg, Ok(WsMessage::Close(_)) | Err(_)));

        // 先来者不受影响，仍可正常开流
        send_frame(&mut first, TYPE_OPEN, 9, &[]).await;
        send_frame(&mut first, TYPE_DATA, 9, b"still-alive").await;
        let (ftype, sid, payload) = next_frame(&mut first).await;
        assert_eq!((ftype, sid, payload.as_slice()), (TYPE_DATA, 9, b"still-alive".as_slice()));
    }

    #[tokio::test]
    async fn unknown_token_connection_is_closed() {
        let manager = local_manager();
        let port = spawn_tunnel_server(manager).await;
        let mut ws = ws_connect(port, "not-a-token").await;
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
            .await
            .expect("should end")
            .expect("stream ended");
        assert!(matches!(msg, Ok(WsMessage::Close(_)) | Err(_)));
    }

    #[tokio::test]
    async fn delete_tunnel_cancels_connection() {
        let echo_port = spawn_echo_server().await;
        let manager = local_manager();
        let created = manager
            .lock()
            .await
            .create("org/p/claude/s1", echo_port, Arc::new(ForwardTarget::Local))
            .unwrap();
        let port = spawn_tunnel_server(manager.clone()).await;

        let mut ws = ws_connect(port, &created.token).await;
        send_frame(&mut ws, TYPE_OPEN, 1, &[]).await;

        manager.lock().await.remove(&created.info.id);

        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
            .await
            .expect("connection should end after delete");
        assert!(msg.is_none()
            || matches!(msg, Some(Ok(WsMessage::Close(_))))
            || matches!(msg, Some(Err(_))));
    }
}
