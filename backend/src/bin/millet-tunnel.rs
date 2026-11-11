//! millet-tunnel：用户本机的隧道客户端（milletcode 版 `ssh -L`）。
//!
//! 在用户自己的机器上监听 `--listen`，TCP 流量按 tunnel_frame 协议经一条
//! WebSocket 多路复用到控制台，由控制台逐流桥到 workspace 端口。浏览器
//! 访问 `http://127.0.0.1:<port>` 即达远端服务。
//!
//! MVP：不自动重连——WS 断开即退出，用户重跑本命令（token 长期有效）。

// 与服务端共享同一份线协议实现
#[path = "../tunnel_frame.rs"]
mod tunnel_frame;

use std::collections::HashMap;
use std::time::Duration;

use clap::Parser;
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tunnel_frame::{CHUNK_SIZE, TYPE_CLOSE, TYPE_DATA, TYPE_OPEN};

#[derive(Parser)]
#[command(
    name = "millet-tunnel",
    about = "Bridge a local listener to a milletcode workspace port over WebSocket"
)]
struct Args {
    /// 控制台地址（http/https/ws/wss 均可），如 http://192.0.2.10:3030
    #[arg(long)]
    server: String,

    /// 端口转发面板生成的隧道 token
    #[arg(long)]
    token: String,

    /// 本机监听地址，如 127.0.0.1:30303
    #[arg(long)]
    listen: String,
}

fn ws_url(server: &str, token: &str) -> Result<String, String> {
    let base = server.trim_end_matches('/');
    let (scheme, host) = match base.split_once("://") {
        Some(("http", h)) => ("ws", h),
        Some(("https", h)) => ("wss", h),
        Some(("ws", _)) | Some(("wss", _)) => {
            return Ok(format!("{base}/api/tunnels/connect?token={token}"))
        }
        Some((other, _)) => return Err(format!("不支持的 server scheme: {other}")),
        None => ("ws", base),
    };
    Ok(format!("{scheme}://{host}/api/tunnels/connect?token={token}"))
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let url = match ws_url(&args.server, &args.token) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("ERROR: {e}");
            std::process::exit(2);
        }
    };

    // 先建 WS（token 错误/服务不可达立即失败），再占本地端口
    let (ws, _resp) = match tokio_tungstenite::connect_async(&url).await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("ERROR: 连接 {} 失败: {e}", args.server);
            std::process::exit(1);
        }
    };
    eprintln!("已连接 {}", args.server);
    let (mut ws_sink, mut ws_stream) = ws.split();

    let listener = match TcpListener::bind(&args.listen).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("ERROR: 监听 {} 失败: {e}", args.listen);
            std::process::exit(1);
        }
    };
    eprintln!("隧道就绪: http://{} -> workspace", args.listen);

    // 所有 C→S 帧进全局队列由唯一 writer 发送（与服务端同构）；20s ping
    // 保活穿过中间代理的 idle 超时
    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(256);
    let writer = tokio::spawn(async move {
        let mut ping = tokio::time::interval(Duration::from_secs(20));
        ping.tick().await; // interval 首次立即触发，跳过
        loop {
            tokio::select! {
                frame = out_rx.recv() => match frame {
                    Some(f) => {
                        if ws_sink.send(WsMessage::Binary(f.into())).await.is_err() {
                            break;
                        }
                    }
                    None => {
                        let _ = ws_sink.send(WsMessage::Close(None)).await;
                        break;
                    }
                },
                _ = ping.tick() => {
                    if ws_sink.send(WsMessage::Ping(Vec::new().into())).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    let mut streams: HashMap<u32, mpsc::Sender<Vec<u8>>> = HashMap::new();
    let mut next_sid: u32 = 0;

    let err = loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((tcp, _peer)) = accepted else { continue };
                next_sid = next_sid.wrapping_add(1);
                let sid = next_sid;
                if out_tx.send(tunnel_frame::encode(TYPE_OPEN, sid, &[])).await.is_err() {
                    break "连接已断开";
                }
                let (ctl_tx, ctl_rx) = mpsc::channel::<Vec<u8>>(64);
                streams.insert(sid, ctl_tx);
                tokio::spawn(pump_tcp(tcp, ctl_rx, out_tx.clone(), sid));
            }
            msg = ws_stream.next() => match msg {
                None | Some(Err(_)) => break "连接已断开",
                Some(Ok(WsMessage::Binary(data))) => {
                    let Some((ftype, sid, payload)) = tunnel_frame::decode(&data) else {
                        break "服务端发来非法帧";
                    };
                    match ftype {
                        TYPE_DATA => {
                            match streams.get(&sid) {
                                Some(tx) => {
                                    if tx.send(payload.to_vec()).await.is_err() {
                                        // 本地连接已关，通知服务端收尾
                                        let _ = out_tx
                                            .send(tunnel_frame::encode(TYPE_CLOSE, sid, &[]))
                                            .await;
                                    }
                                }
                                None => {
                                    let _ = out_tx
                                        .send(tunnel_frame::encode(TYPE_CLOSE, sid, &[]))
                                        .await;
                                }
                            }
                        }
                        TYPE_CLOSE => {
                            // 服务端已关流：drop 发送端 → pump 断开本地连接
                            streams.remove(&sid);
                        }
                        _ => {}
                    }
                }
                Some(Ok(WsMessage::Close(_))) => break "服务端关闭了连接",
                Some(Ok(_)) => {} // Pong/Text 忽略
            },
        }
    };

    writer.abort();
    eprintln!("隧道退出: {err}（重跑本命令即可恢复）");
    std::process::exit(1);
}

/// 单条本地 TCP 连接的泵：本地读 → DATA 帧；帧数据 → 本地写。
async fn pump_tcp(
    tcp: TcpStream,
    mut ctl: mpsc::Receiver<Vec<u8>>,
    out: mpsc::Sender<Vec<u8>>,
    sid: u32,
) {
    let (mut rd, mut wr) = tcp.into_split();
    let mut buf = vec![0u8; CHUNK_SIZE];
    loop {
        tokio::select! {
            data = ctl.recv() => match data {
                Some(bytes) => {
                    if wr.write_all(&bytes).await.is_err() {
                        break;
                    }
                }
                // 服务端已关流（或全局退出）→ 直接断开本地连接
                None => break,
            },
            r = rd.read(&mut buf) => match r {
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
            },
        }
    }
    let _ = wr.shutdown().await;
    let _ = out.send(tunnel_frame::encode(TYPE_CLOSE, sid, &[])).await;
}
