//! Port forwarding: bridge ports listening in a session workspace to TCP
//! listeners on the console host, so browsers can reach them at
//! `http://<console-host>:<local_port>`.
//!
//! ssh 模式走 russh direct-tcpip（`ssh -L` 的底层机制）；本地模式直接代理
//! 127.0.0.1。每条转发独立持有 SSH 连接并在 accept 循环内自愈重连——不做
//! 全局连接缓存，避免 manager 锁竞争与缓存失效竞态；同主机多条转发各占
//! 一条连接，开发场景下可接受。

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use russh::{Channel, ChannelStream};
use russh::client::Msg;
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf, copy_bidirectional};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::agents::RuntimeContext;
use crate::runtime::ssh::{SharedSshHandle, connect_ssh_with_retry};

/// 单次扫描/连通测试的 SSH 往返上限，防止卡死请求
pub const SSH_OP_TIMEOUT: Duration = Duration::from_secs(10);

/// bind 一个 TCP listener，显式关闭 SO_REUSEADDR。
///
/// tokio 的 `TcpListener::bind` 默认开 REUSEADDR，macOS 上它允许
/// 127.0.0.1:X 与已存在的 *:X 共存——端口占用检测会漏报，转发会与原
/// 服务抢连接。占用检查和正式 bind 都必须走这里。
pub async fn bind_strict(bind_addr: &str, port: u16) -> std::io::Result<TcpListener> {
    let addr: std::net::SocketAddr = format!("{}:{}", bind_addr, port)
        .parse()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let socket = socket2::Socket::new(
        socket2::Domain::for_address(addr),
        socket2::Type::STREAM,
        None,
    )?;
    socket.set_reuse_address(false)?;
    socket.bind(&addr.into())?;
    socket.listen(1024)?;
    #[cfg(unix)]
    socket.set_nonblocking(true)?;
    TcpListener::from_std(std::net::TcpListener::from(socket))
}

#[derive(Debug, Clone, Serialize)]
pub struct ForwardEntry {
    /// `{session_key}:{remote_port}`，同会话同远端端口唯一
    pub id: String,
    pub session_key: String,
    /// "local" | "ssh"
    pub mode: String,
    pub hostname: String,
    pub remote_port: u16,
    pub local_port: u16,
    /// true = 0.0.0.0（局域网可访问），false = 127.0.0.1
    pub bind_lan: bool,
    pub created_at: i64,
}

struct ForwardHandle {
    entry: ForwardEntry,
    task: JoinHandle<()>,
}

/// 转发目标：本地 workspace 或远端 SSH 主机
pub enum ForwardTarget {
    Local,
    Ssh(RuntimeContext),
}

#[derive(Debug, thiserror::Error)]
pub enum ForwardError {
    #[error("本地端口绑定失败: {0}")]
    LocalPortBusy(String),
    #[error("远端不可达: {0}")]
    RemoteUnreachable(String),
    #[error("该会话的远端端口已有转发")]
    Duplicate,
}

#[derive(Default)]
pub struct PortForwardManager {
    forwards: HashMap<String, ForwardHandle>,
}

impl PortForwardManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn list(&self, session_key: Option<&str>) -> Vec<ForwardEntry> {
        let mut entries: Vec<ForwardEntry> = self
            .forwards
            .values()
            .filter(|h| session_key.map_or(true, |k| h.entry.session_key == k))
            .map(|h| h.entry.clone())
            .collect();
        entries.sort_by_key(|e| e.created_at);
        entries
    }

    /// 创建转发。bind 与 SSH connect 都在 manager 锁外进行（terminal.rs 同款
    /// 约定：慢操作不阻塞 list/delete），回锁插入时再兜底查重。
    pub async fn add(
        manager: &Arc<Mutex<Self>>,
        session_key: &str,
        hostname: &str,
        remote_port: u16,
        local_port: u16,
        bind_lan: bool,
        target: ForwardTarget,
    ) -> Result<ForwardEntry, ForwardError> {
        let id = format!("{}:{}", session_key, remote_port);
        if manager.lock().await.forwards.contains_key(&id) {
            return Err(ForwardError::Duplicate);
        }

        let bind_addr = if bind_lan { "0.0.0.0" } else { "127.0.0.1" };
        let listener = bind_strict(bind_addr, local_port)
            .await
            .map_err(|e| ForwardError::LocalPortBusy(e.to_string()))?;

        // ssh 模式先建立初始连接：连不上直接失败，不留一个只会拒绝连接的空壳
        let initial_ssh = match &target {
            ForwardTarget::Ssh(ctx) => Some(
                connect_ssh_with_retry(ctx, 3)
                    .await
                    .map_err(|e| {
                        ForwardError::RemoteUnreachable(format!(
                            "SSH 连接 {} 失败: {}",
                            hostname, e
                        ))
                    })?,
            ),
            ForwardTarget::Local => None,
        };

        let entry = ForwardEntry {
            id: id.clone(),
            session_key: session_key.to_string(),
            mode: match &target {
                ForwardTarget::Local => "local".to_string(),
                ForwardTarget::Ssh(_) => "ssh".to_string(),
            },
            hostname: hostname.to_string(),
            remote_port,
            local_port,
            bind_lan,
            created_at: chrono::Utc::now().timestamp(),
        };

        let task = tokio::spawn(accept_loop(listener, target, remote_port, initial_ssh));

        let mut mgr = manager.lock().await;
        if mgr.forwards.contains_key(&id) {
            // 并发创建撞 id：保留先来者，回收刚起的这条（listener 随 task abort 关闭）
            task.abort();
            return Err(ForwardError::Duplicate);
        }
        mgr.forwards.insert(id, ForwardHandle { entry: entry.clone(), task });
        Ok(entry)
    }

    pub async fn remove(manager: &Arc<Mutex<Self>>, id: &str) -> bool {
        let mut mgr = manager.lock().await;
        match mgr.forwards.remove(id) {
            Some(h) => {
                h.task.abort();
                true
            }
            None => false,
        }
    }
}

async fn accept_loop(
    listener: TcpListener,
    target: ForwardTarget,
    remote_port: u16,
    mut ssh: Option<SharedSshHandle>,
) {
    loop {
        let (mut client, _peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!("port forward accept error: {e}");
                continue;
            }
        };
        match connect_forward_target(&target, remote_port, &mut ssh).await {
            Ok(mut upstream) => {
                tokio::spawn(async move {
                    let _ = copy_bidirectional(&mut client, &mut upstream).await;
                });
            }
            Err(e) => {
                tracing::warn!("forward connect port {remote_port} failed: {e:#}");
            }
        }
    }
}

/// 一条到 workspace 端口的上游连接：Local 直连或 Ssh direct-tcpip，
/// 统一暴露 AsyncRead/AsyncWrite（两个变体都 Unpin，可直接 copy_bidirectional）。
pub enum ForwardStream {
    Local(TcpStream),
    Ssh(ChannelStream<Msg>),
}

impl AsyncRead for ForwardStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            ForwardStream::Local(s) => Pin::new(s).poll_read(cx, buf),
            ForwardStream::Ssh(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for ForwardStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        match self.get_mut() {
            ForwardStream::Local(s) => Pin::new(s).poll_write(cx, buf),
            ForwardStream::Ssh(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        match self.get_mut() {
            ForwardStream::Local(s) => Pin::new(s).poll_flush(cx),
            ForwardStream::Ssh(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        match self.get_mut() {
            ForwardStream::Local(s) => Pin::new(s).poll_shutdown(cx),
            ForwardStream::Ssh(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

/// 连接 workspace 端口（端口转发 accept 循环与隧道 OPEN 共用）。
/// `ssh` 是调用方缓存的连接：开 channel 失败视为连接已死，重建一次并写回。
pub async fn connect_forward_target(
    target: &ForwardTarget,
    remote_port: u16,
    ssh: &mut Option<SharedSshHandle>,
) -> anyhow::Result<ForwardStream> {
    match target {
        ForwardTarget::Local => Ok(ForwardStream::Local(
            TcpStream::connect(("127.0.0.1", remote_port)).await?,
        )),
        ForwardTarget::Ssh(ctx) => {
            if ssh.is_none() {
                *ssh = Some(connect_ssh_with_retry(ctx, 3).await?);
            }
            let handle = ssh.clone().expect("ssh handle just ensured");
            let channel = match open_direct(&handle, remote_port).await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("direct-tcpip open failed, reconnecting: {e}");
                    *ssh = None;
                    let fresh = connect_ssh_with_retry(ctx, 3).await?;
                    *ssh = Some(fresh.clone());
                    open_direct(&fresh, remote_port).await?
                }
            };
            Ok(ForwardStream::Ssh(channel.into_stream()))
        }
    }
}

/// 在锁内开 direct-tcpip channel（~1 RTT，terminal.rs 同款约定），返回的
/// channel 独立于 handle，后续读写无需持锁。连通测试也用它探测远端端口。
pub(crate) async fn open_direct(
    handle: &SharedSshHandle,
    remote_port: u16,
) -> anyhow::Result<Channel<Msg>> {
    Ok(handle
        .lock()
        .await
        .channel_open_direct_tcpip("127.0.0.1", remote_port as u32, "127.0.0.1", 0)
        .await?)
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ListeningPort {
    pub port: u16,
    /// 展示用绑定地址："0.0.0.0" / "[::]" / "127.0.0.1" / "*" 等
    pub address: String,
    /// 进程名（需要权限，取不到为 None）
    pub process: Option<String>,
}

/// 解析 netstat / ss / lsof 任一格式的监听端口列表。
///
/// 支持的行格式：
/// - `ss -tln`：`LISTEN 0 511 0.0.0.0:5173 0.0.0.0:* users:(("node",pid=1,fd=20))`
/// - Linux `netstat -tln(p)`：`tcp 0 0 0.0.0.0:22 0.0.0.0:* LISTEN users:((...))`
/// - macOS `netstat -an -p tcp`：`tcp4 0 0 *.8000 *.* LISTEN`（端口点分）
/// - `lsof -nP -iTCP -sTCP:LISTEN`：`node 1234 user IPv4 0x.. TCP *:5173 (LISTEN)`
///
/// IPv4/IPv6 通配同端口合并为一条。
pub fn parse_listening_ports(output: &str) -> Vec<ListeningPort> {
    let mut map: HashMap<u16, ListeningPort> = HashMap::new();
    for line in output.lines() {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.is_empty() {
            continue;
        }
        let (local, process) = if tokens[0] == "LISTEN" {
            // ss: State Recv-Q Send-Q Local Peer [Process]
            (tokens.get(3).copied(), find_process(&tokens))
        } else if tokens[0].starts_with("tcp") {
            // netstat：仅保留 LISTEN 行
            if !tokens.iter().any(|t| *t == "LISTEN") {
                continue;
            }
            (tokens.get(3).copied(), find_process(&tokens))
        } else if tokens.last() == Some(&"(LISTEN)") {
            // lsof：COMMAND ... NAME (LISTEN)；NAME 在倒数第二列
            (
                tokens.get(tokens.len().saturating_sub(2)).copied(),
                Some(tokens[0].to_string()),
            )
        } else {
            continue;
        };
        let Some(local) = local else { continue };
        let Some((address, port)) = split_address_port(local) else {
            continue;
        };
        let new = ListeningPort { port, address, process };
        match map.get_mut(&port) {
            Some(existing) => {
                // 通配地址（0.0.0.0 / :: / *）比具体地址更能代表"端口在监听"
                if is_wildcard(&new.address) && !is_wildcard(&existing.address) {
                    existing.address = new.address;
                }
                if existing.process.is_none() {
                    existing.process = new.process;
                }
            }
            None => {
                map.insert(port, new);
            }
        }
    }
    let mut ports: Vec<ListeningPort> = map.into_values().collect();
    ports.sort_by_key(|p| p.port);
    ports
}

fn find_process(tokens: &[&str]) -> Option<String> {
    let tok = tokens.iter().find(|t| t.contains("users:(("))?;
    let start = tok.find('"')? + 1;
    let end = tok[start..].find('"')? + start;
    Some(tok[start..end].to_string())
}

fn split_address_port(local: &str) -> Option<(String, u16)> {
    if let Some((addr, port)) = local.rsplit_once(':') {
        if let Ok(port) = port.parse::<u16>() {
            let addr = addr.trim_start_matches('[').trim_end_matches(']');
            return Some((addr.to_string(), port));
        }
        // 冒号后不是端口（macOS netstat 的 IPv6 点分格式如 fe80::1%en0.7000），
        // 落到点分分支
    }
    // macOS netstat 点分格式：*.8000 / 127.0.0.1.8000
    let (addr, port) = local.rsplit_once('.')?;
    let port = port.parse::<u16>().ok()?;
    Some((addr.to_string(), port))
}

fn is_wildcard(address: &str) -> bool {
    matches!(address, "*" | "0.0.0.0" | "::")
}

/// 远端扫描命令：lsof 优先（macOS/Linux 远端通用且带进程名；注意远端可能
/// 是 macOS，netstat 的 -p/-t 语义与 Linux 不同，不能放前面），netstat/ss
/// 兜底；全部失败时保持退出码 0，由空结果表达"无端口/环境不支持"。
const REMOTE_SCAN_CMD: &str = "lsof -nP -iTCP -sTCP:LISTEN 2>/dev/null || netstat -tlnp 2>/dev/null || ss -tlnp 2>/dev/null || netstat -tln 2>/dev/null || ss -tln";

/// 本地扫描：依次尝试 lsof（macOS/Linux，有进程名）→ ss → netstat（Linux）→
/// netstat macOS 格式，第一组能解析出端口的输出胜出。控制台可能跑在
/// macOS（开发）或 Linux（容器），不写死平台。
pub fn scan_local_blocking() -> std::io::Result<Vec<ListeningPort>> {
    let attempts: Vec<Vec<&str>> = vec![
        vec!["lsof", "-nP", "-iTCP", "-sTCP:LISTEN"],
        vec!["ss", "-tln"],
        vec!["netstat", "-tln"],
        vec!["netstat", "-an", "-p", "tcp"],
    ];
    let mut any_executed = false;
    let mut last_err: Option<String> = None;
    for cmd in attempts {
        let (program, args) = (cmd[0], &cmd[1..]);
        match std::process::Command::new(program).args(args).output() {
            Ok(output) => {
                // lsof 在无监听端口时也退出非零，只要进程跑起来就算可用
                any_executed = true;
                let stdout = String::from_utf8_lossy(&output.stdout);
                let ports = parse_listening_ports(&stdout);
                if !ports.is_empty() {
                    return Ok(ports);
                }
            }
            Err(e) => {
                last_err = Some(format!("{program}: {e}"));
            }
        }
    }
    // 命令都跑过但没解析到端口：按"无监听端口"处理；只有连命令都
    // 执行不了（典型如精简容器镜像）才报错
    if any_executed {
        Ok(vec![])
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            last_err.unwrap_or_else(|| "no port scanner available".to_string()),
        ))
    }
}

/// 远端（ssh 模式）扫描：shell-out ssh（discovery.rs / debug.rs 同款模式）。
pub fn scan_remote_blocking(hostname: &str, ssh_config_path: &str) -> std::io::Result<Vec<ListeningPort>> {
    let mut args = crate::projects::discovery::build_ssh_args(hostname, ssh_config_path);
    args.push(REMOTE_SCAN_CMD.to_string());
    let output = std::process::Command::new("ssh").args(&args).output()?;
    if !output.status.success() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!(
                "ssh {}: {}",
                hostname,
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_listening_ports(&stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ss_output() {
        let out = "State  Recv-Q Send-Q Local Address:Port  Peer Address:Port Process\n\
                   LISTEN 0      511    0.0.0.0:5173        0.0.0.0:*           users:((\"node\",pid=1,fd=20))\n\
                   LISTEN 0      128    [::]:22            [::]:*              \n";
        assert_eq!(
            parse_listening_ports(out),
            vec![
                ListeningPort { port: 22, address: "::".to_string(), process: None },
                ListeningPort {
                    port: 5173,
                    address: "0.0.0.0".to_string(),
                    process: Some("node".to_string()),
                },
            ]
        );
    }

    #[test]
    fn parses_linux_netstat_with_process() {
        let out = "Active Internet connections (only servers)\n\
                   Proto Recv-Q Send-Q Local Address   Foreign Address  State   Program name\n\
                   tcp        0      0 0.0.0.0:22      0.0.0.0:*        LISTEN  \n\
                   tcp        0      0 127.0.0.1:3000 0.0.0.0:*        LISTEN  users:((\"python3\",pid=42,fd=3))\n\
                   tcp        0      0 10.0.0.5:5432  0.0.0.0:*        ESTABLISHED\n";
        assert_eq!(
            parse_listening_ports(out),
            vec![
                ListeningPort { port: 22, address: "0.0.0.0".to_string(), process: None },
                ListeningPort {
                    port: 3000,
                    address: "127.0.0.1".to_string(),
                    process: Some("python3".to_string()),
                },
            ]
        );
    }

    #[test]
    fn parses_macos_netstat_dotted_ports() {
        let out = "Proto Recv-Q Send-Q  Local Address          Foreign Address        (state)\n\
                   tcp4       0      0  *.8000                 *.*                    LISTEN\n\
                   tcp6       0      0  fe80::1%en0.7000       *.*                    LISTEN\n";
        let ports = parse_listening_ports(out);
        assert_eq!(
            ports,
            vec![
                ListeningPort { port: 7000, address: "fe80::1%en0".to_string(), process: None },
                ListeningPort { port: 8000, address: "*".to_string(), process: None },
            ]
        );
    }

    #[test]
    fn parses_lsof_output() {
        let out = "COMMAND   PID  USER   TYPE   NODE NAME\n\
                   node     1234  u1    IPv4  0x1a TCP *:5173 (LISTEN)\n\
                   Python   1235  u1    IPv6  0x1b TCP localhost:8000 (LISTEN)\n";
        assert_eq!(
            parse_listening_ports(out),
            vec![
                ListeningPort { port: 5173, address: "*".to_string(), process: Some("node".to_string()) },
                ListeningPort {
                    port: 8000,
                    address: "localhost".to_string(),
                    process: Some("Python".to_string()),
                },
            ]
        );
    }

    #[test]
    fn merges_ipv4_ipv6_wildcard_same_port() {
        let out = "LISTEN 0 511 0.0.0.0:3000 0.0.0.0:* users:((\"node\",pid=1,fd=20))\n\
                   LISTEN 0 511 [::]:3000 [::]:*\n";
        assert_eq!(
            parse_listening_ports(out),
            vec![ListeningPort {
                port: 3000,
                address: "0.0.0.0".to_string(),
                process: Some("node".to_string()),
            }]
        );
    }

    #[test]
    fn split_address_port_edge_cases() {
        assert_eq!(split_address_port("0.0.0.0:5173"), Some(("0.0.0.0".into(), 5173)));
        assert_eq!(split_address_port("[::]:22"), Some(("::".into(), 22)));
        assert_eq!(split_address_port(":::8080"), Some(("::".into(), 8080)));
        assert_eq!(split_address_port("*.8000"), Some(("*".into(), 8000)));
        assert_eq!(split_address_port("127.0.0.1.9000"), Some(("127.0.0.1".into(), 9000)));
        assert_eq!(split_address_port("not-a-port"), None);
        assert_eq!(split_address_port("0.0.0.0:notaport"), None);
    }

    #[tokio::test]
    async fn manager_lifecycle_local_mode() {
        let manager = Arc::new(Mutex::new(PortForwardManager::new()));
        // local_port = 0：OS 分配临时端口，测试不挑环境
        let entry = PortForwardManager::add(
            &manager,
            "org/proj/claude/s1",
            "",
            9999,
            0,
            false,
            ForwardTarget::Local,
        )
        .await
        .expect("add should succeed");
        assert_eq!(entry.id, "org/proj/claude/s1:9999");
        assert_eq!(entry.mode, "local");

        // 重复创建 → Duplicate
        let dup = PortForwardManager::add(
            &manager,
            "org/proj/claude/s1",
            "",
            9999,
            0,
            false,
            ForwardTarget::Local,
        )
        .await;
        assert!(matches!(dup, Err(ForwardError::Duplicate)));

        // 列表按 session 过滤
        assert_eq!(manager.lock().await.list(Some("org/proj/claude/s1")).len(), 1);
        assert_eq!(manager.lock().await.list(Some("org/other/claude/s1")).len(), 0);
        assert_eq!(manager.lock().await.list(None).len(), 1);

        // 删除后再创建可成功
        assert!(PortForwardManager::remove(&manager, &entry.id).await);
        assert!(!PortForwardManager::remove(&manager, &entry.id).await);
        PortForwardManager::add(
            &manager,
            "org/proj/claude/s1",
            "",
            9999,
            0,
            false,
            ForwardTarget::Local,
        )
        .await
        .expect("re-add after remove should succeed");
        // 清理，避免 listener 泄漏到其他测试
        let ids: Vec<String> = manager.lock().await.list(None).into_iter().map(|e| e.id).collect();
        for id in ids {
            PortForwardManager::remove(&manager, &id).await;
        }
    }
}
