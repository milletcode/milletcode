//! 用户可见错误文案的保密改写。
//!
//! 错误到达用户（WS 事件、API 响应、落库的 last_error）前抹掉底层传输
//! 实现：`ssh`（任意大小写，含 `~/.ssh` 路径、russh crate 名）→
//! `millet-connector`；默认端口 22 → `conn-port`（自定义端口保留，方便
//! 用户排查自己的配置）。服务端日志不经过这里，保留原文供排查。

use regex::Regex;
use std::sync::OnceLock;

pub fn sanitize_public_error(msg: &str) -> String {
    static RE_SSH: OnceLock<Regex> = OnceLock::new();
    static RE_HOST_PORT: OnceLock<Regex> = OnceLock::new();
    static RE_PORT_WORD: OnceLock<Regex> = OnceLock::new();
    let ssh = RE_SSH.get_or_init(|| Regex::new(r"(?i)ssh").unwrap());
    // host:22 形式（:2233 这类非默认端口不动）；regex crate 无 lookahead，
    // 用捕获组吃掉边界字符再原样拼回
    let host_port = RE_HOST_PORT.get_or_init(|| Regex::new(r":22($|[^0-9])").unwrap());
    // "port 22"（OpenSSH 客户端 stderr 风格）/ "端口 22" 文字形式
    let port_word = RE_PORT_WORD
        .get_or_init(|| Regex::new(r"(?i:(port|端口)) ?22($|[^0-9])").unwrap());
    let s = ssh.replace_all(msg, "millet-connector");
    let s = host_port.replace_all(&s, ":conn-port$1");
    let s = port_word.replace_all(&s, "${1} conn-port$2");
    s.into_owned()
}

#[cfg(test)]
mod tests {
    use super::sanitize_public_error;

    #[test]
    fn ssh_word_and_paths_are_rewritten() {
        assert_eq!(
            sanitize_public_error("SSH 连接 192.168.3.34 失败"),
            "millet-connector 连接 192.168.3.34 失败"
        );
        assert_eq!(
            sanitize_public_error("permission denied for ~/.ssh/id_rsa"),
            "permission denied for ~/.millet-connector/id_rsa"
        );
        // crate 名里的 ssh（russh）一并抹掉
        assert!(!sanitize_public_error("russh: ChannelOpenFailure")
            .to_lowercase()
            .contains("ssh"));
    }

    #[test]
    fn default_port_22_is_rewritten_but_custom_ports_kept() {
        assert_eq!(
            sanitize_public_error("connect to 10.0.0.1:22 refused"),
            "connect to 10.0.0.1:conn-port refused"
        );
        // OpenSSH 客户端 stderr 风格
        assert_eq!(
            sanitize_public_error("ssh: connect to host 192.168.3.34 port 22: Connection timed out"),
            "millet-connector: connect to host 192.168.3.34 port conn-port: Connection timed out"
        );
        assert_eq!(
            sanitize_public_error("端口 22 不可达"),
            "端口 conn-port 不可达"
        );
        // 自定义端口保留
        assert_eq!(
            sanitize_public_error("connect to 192.168.3.34:2222 failed"),
            "connect to 192.168.3.34:2222 failed"
        );
        assert_eq!(
            sanitize_public_error("connect to 192.168.3.9:8022 failed"),
            "connect to 192.168.3.9:8022 failed"
        );
    }

    #[test]
    fn ordinary_numbers_are_untouched() {
        // os error 22（EINVAL）不是端口号，不能误伤
        assert_eq!(
            sanitize_public_error("failed: Invalid argument (os error 22)"),
            "failed: Invalid argument (os error 22)"
        );
    }

    /// 真连一个 22 端口捕获连接错误，确认真实错误文本能安全通过改写
    /// （cargo test -- --nocapture 可看到捕获的原文与改写结果）。
    #[tokio::test]
    async fn real_tcp_timeout_output_passes_sanitizer() {
        // TUN 代理环境下黑洞地址可能"连接成功"，逐个候选直到拿到一个
        // 真实的连接错误（超时/不可达/拒绝都算）
        let targets = ["10.255.255.1:22", "192.0.2.1:22", "127.0.0.1:22"];
        let mut raw: Option<String> = None;
        for target in targets {
            match tokio::time::timeout(
                std::time::Duration::from_secs(3),
                tokio::net::TcpStream::connect(target),
            )
            .await
            {
                Err(_elapsed) => {
                    raw = Some(format!("connect to {target} timed out"));
                    break;
                }
                Ok(Err(e)) => {
                    raw = Some(format!("connect to {target}: {e}"));
                    break;
                }
                Ok(Ok(_)) => continue,
            }
        }
        let Some(raw) = raw else {
            eprintln!("skip: no candidate produced a connect error in this network");
            return;
        };
        let sanitized = sanitize_public_error(&raw);
        eprintln!("captured raw error: {raw:?}");
        eprintln!("sanitized:        {sanitized:?}");
        assert!(!sanitized.to_lowercase().contains("ssh"));
        assert!(!sanitized.contains(":22"));
    }
}
