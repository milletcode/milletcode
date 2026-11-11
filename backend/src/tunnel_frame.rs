//! millet-tunnel 线协议 v1：二进制 WebSocket 帧。
//!
//! 帧格式 `[u8 type][u32 sid 大端][payload]`：
//! - OPEN (0x01)  C→S，请求打开一条到 workspace 端口的流（payload 空，
//!   远端端口由隧道 token 对应的 TunnelEntry 决定）
//! - DATA (0x02)  双向，payload 为原始字节（发送侧按 CHUNK_SIZE 分块）
//! - CLOSE (0x03) 双向，payload 空，正常关/异常关不区分
//!
//! 本文件零依赖（无 serde/axum），`src/bin/millet-tunnel.rs` 通过
//! `#[path]` 直接引用同一份实现，保证两端协议一致。

pub const TYPE_OPEN: u8 = 0x01;
pub const TYPE_DATA: u8 = 0x02;
pub const TYPE_CLOSE: u8 = 0x03;

/// 单帧 payload 上限；超过只可能是对端实现错误，decode 拒收防滥用
pub const MAX_PAYLOAD: usize = 64 * 1024;

/// DATA 发送侧分块大小（接收侧自然重组，无需对齐）
pub const CHUNK_SIZE: usize = 16 * 1024;

const HEADER_LEN: usize = 5;

pub fn encode(ftype: u8, sid: u32, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(HEADER_LEN + payload.len());
    frame.push(ftype);
    frame.extend_from_slice(&sid.to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

/// 解析一帧。None = 长度不足或 payload 超限（非法帧，调用方应断开）。
pub fn decode(frame: &[u8]) -> Option<(u8, u32, &[u8])> {
    if frame.len() < HEADER_LEN || frame.len() > HEADER_LEN + MAX_PAYLOAD {
        return None;
    }
    let ftype = frame[0];
    let sid = u32::from_be_bytes([frame[1], frame[2], frame[3], frame[4]]);
    Some((ftype, sid, &frame[HEADER_LEN..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_pins_big_endian_layout() {
        let frame = encode(TYPE_DATA, 0x0102_0304, b"hi");
        // 钉死线格式：type、sid 大端、payload 原样
        assert_eq!(frame, vec![0x02, 0x01, 0x02, 0x03, 0x04, b'h', b'i']);
    }

    #[test]
    fn roundtrip() {
        for (t, sid, payload) in [
            (TYPE_OPEN, 1, &[][..]),
            (TYPE_DATA, u32::MAX, b"abc".as_slice()),
            (TYPE_CLOSE, 0, b""),
        ] {
            let frame = encode(t, sid, payload);
            let (t2, sid2, p2) = decode(&frame).expect("decode");
            assert_eq!((t2, sid2), (t, sid));
            assert_eq!(p2, payload);
        }
    }

    #[test]
    fn rejects_short_frame() {
        assert_eq!(decode(&[]), None);
        assert_eq!(decode(&[TYPE_DATA]), None);
        assert_eq!(decode(&[TYPE_DATA, 0, 0, 0]), None);
    }

    #[test]
    fn rejects_oversized_payload() {
        let mut frame = encode(TYPE_DATA, 1, &vec![0u8; MAX_PAYLOAD]);
        assert!(decode(&frame).is_some());
        frame.push(0);
        assert_eq!(decode(&frame), None);
    }
}
