//! Terminal session management: local PTY or remote SSH shell.
//!
//! No tmux. The PTY / SSH channel lives in the backend independent of any
//! WebSocket, so a network blip only kills the WS — reconnecting replays the
//! scrollback snapshot and continues the live stream. Output events are
//! numbered by `seq`; a shared mutex makes "append to scrollback + broadcast"
//! atomic against "snapshot + subscribe", so every event is delivered exactly
//! once across snapshot ∪ broadcast.

use anyhow::{anyhow, Context, Result};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use rand::Rng;
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, watch};
use uuid::Uuid;

use crate::runtime::ssh::{self, SharedSshHandle};

/// Scrollback capacity per session (bytes). Covers a few minutes of typical
/// output; full-screen apps may still paint incompletely after a resync.
const SCROLLBACK_CAP: usize = 200 * 1024;

/// A single output event, numbered by a monotonically increasing seq.
#[derive(Clone, Debug)]
pub struct TerminalOutput {
    pub seq: u64,
    pub data: Vec<u8>,
}

/// Ring buffer of raw output chunks. Seqs count events, not bytes — evicted
/// chunks still consume their seq.
pub struct Scrollback {
    chunks: VecDeque<Vec<u8>>,
    total: usize,
    next_seq: u64,
    cap: usize,
}

impl Scrollback {
    fn new(cap: usize) -> Self {
        Self {
            chunks: VecDeque::new(),
            total: 0,
            next_seq: 1,
            cap,
        }
    }

    /// Append a chunk; returns the seq-tagged event to broadcast.
    fn push(&mut self, data: Vec<u8>) -> TerminalOutput {
        self.total += data.len();
        self.chunks.push_back(data.clone());
        // Evict whole chunks from the front. Always keep the newest chunk
        // even if it alone exceeds the cap.
        while self.total > self.cap && self.chunks.len() > 1 {
            self.total -= self.chunks.pop_front().expect("len > 1").len();
        }
        let event = TerminalOutput {
            seq: self.next_seq,
            data,
        };
        self.next_seq += 1;
        event
    }

    /// Concatenated contents and the seq of the last retained event
    /// (0 when empty).
    fn snapshot(&self) -> (Vec<u8>, u64) {
        let mut out = Vec::with_capacity(self.total);
        for chunk in &self.chunks {
            out.extend_from_slice(chunk);
        }
        (out, self.next_seq.saturating_sub(1))
    }
}

pub type SharedScrollback = Arc<parking_lot::Mutex<Scrollback>>;

/// Producer critical section: push and broadcast under the same lock that
/// `attach` takes, so an event lands in exactly one of snapshot / broadcast.
fn emit_output(
    scrollback: &SharedScrollback,
    output_tx: &broadcast::Sender<TerminalOutput>,
    data: Vec<u8>,
) {
    let mut guard = scrollback.lock();
    let event = guard.push(data);
    let _ = output_tx.send(event);
}

/// What a WS receiver should do with a broadcast event, given the last seq it
/// already sent.
#[derive(Debug, PartialEq, Eq)]
pub enum SeqAction {
    Forward,
    Skip,
    /// A gap (receiver lagged) — take a fresh snapshot and clear the screen.
    Resync,
}

pub fn seq_action(last_seq: u64, event_seq: u64) -> SeqAction {
    if event_seq == last_seq + 1 {
        SeqAction::Forward
    } else if event_seq <= last_seq {
        SeqAction::Skip
    } else {
        SeqAction::Resync
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TerminalMode {
    Local,
    Ssh { hostname: String },
}

impl TerminalMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            TerminalMode::Local => "local",
            TerminalMode::Ssh { .. } => "ssh",
        }
    }

    pub fn hostname(&self) -> Option<&str> {
        match self {
            TerminalMode::Local => None,
            TerminalMode::Ssh { hostname } => Some(hostname),
        }
    }
}

/// Commands for the task that owns the SSH channel (Channel is !Clone and
/// `wait` needs `&mut`, so all writes are routed through this channel).
#[derive(Debug)]
pub enum RemoteCtl {
    Input(Vec<u8>),
    Resize(u16, u16),
    Close,
}

enum Backend {
    Local {
        master: Box<dyn MasterPty + Send>,
        writer: std::sync::Mutex<Box<dyn Write + Send>>,
        child: Box<dyn portable_pty::Child + Send>,
        reader_handle: tokio::task::JoinHandle<()>,
    },
    Remote {
        conn: SharedSshHandle,
        control_tx: mpsc::UnboundedSender<RemoteCtl>,
        task_handle: tokio::task::JoinHandle<()>,
    },
}

/// A terminal session: local PTY or remote SSH shell.
pub struct TerminalSession {
    pub id: String,
    pub token: String,
    /// Project working directory (remote paths stay `~/...`, unexpanded).
    pub cwd: String,
    pub mode: TerminalMode,
    backend: Backend,
    /// broadcast never reports Closed while the session holds this sender, so
    /// WS loops watch `exit_rx` instead for shell termination.
    output_tx: broadcast::Sender<TerminalOutput>,
    scrollback: SharedScrollback,
    exit_tx: watch::Sender<bool>,
}

/// Data handed to a WebSocket on attach.
pub struct TerminalAttach {
    pub snapshot: Vec<u8>,
    pub last_seq: u64,
    pub output_rx: broadcast::Receiver<TerminalOutput>,
    pub exit_rx: watch::Receiver<bool>,
}

/// Event loop owning the SSH channel. Stdout and stderr both feed the output
/// stream (a PTY on the remote side merges them anyway).
async fn run_remote_channel(
    mut channel: russh::Channel<russh::client::Msg>,
    early_data: Option<Vec<u8>>,
    mut ctl_rx: mpsc::UnboundedReceiver<RemoteCtl>,
    scrollback: SharedScrollback,
    output_tx: broadcast::Sender<TerminalOutput>,
    exit_tx: watch::Sender<bool>,
) {
    if let Some(data) = early_data {
        emit_output(&scrollback, &output_tx, data);
    }
    loop {
        tokio::select! {
            ctl = ctl_rx.recv() => match ctl {
                Some(RemoteCtl::Input(data)) => {
                    if let Err(e) = channel.data(std::io::Cursor::new(&data)).await {
                        tracing::debug!(error = %e, "ssh terminal write failed");
                        break;
                    }
                }
                Some(RemoteCtl::Resize(cols, rows)) => {
                    let _ = channel.window_change(cols as u32, rows as u32, 0, 0).await;
                }
                Some(RemoteCtl::Close) | None => {
                    let _ = channel.close().await;
                    break;
                }
            },
            msg = channel.wait() => match msg {
                Some(russh::ChannelMsg::Data { data }) => {
                    emit_output(&scrollback, &output_tx, data.to_vec());
                }
                Some(russh::ChannelMsg::ExtendedData { data, .. }) => {
                    emit_output(&scrollback, &output_tx, data.to_vec());
                }
                Some(russh::ChannelMsg::ExitStatus { .. }) => continue,
                Some(_) => continue,
                None => break,
            },
        }
    }
    emit_output(&scrollback, &output_tx, b"\r\n[remote shell exited]\r\n".to_vec());
    exit_tx.send_replace(true);
}

/// Manages all terminal sessions.
pub struct TerminalManager {
    sessions: HashMap<String, TerminalSession>,
}

impl TerminalManager {
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
        }
    }

    /// Create a local PTY session. Fails if `cwd` is not an existing
    /// directory — never silently falls back to the home directory.
    pub fn create_local(&mut self, cwd: &str, cols: u16, rows: u16) -> Result<(String, String)> {
        let meta = std::fs::metadata(cwd)
            .with_context(|| format!("terminal working directory {:?} is not accessible", cwd))?;
        if !meta.is_dir() {
            return Err(anyhow!(
                "terminal working directory {:?} is not a directory",
                cwd
            ));
        }

        let session_id = Uuid::new_v4().to_string();
        let token = Self::generate_secure_token();

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("failed to open PTY")?;

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
        let mut cmd = CommandBuilder::new(&shell);
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");

        let child = pair
            .slave
            .spawn_command(cmd)
            .context("failed to spawn shell in PTY")?;
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .context("failed to clone PTY reader")?;
        let writer = pair
            .master
            .take_writer()
            .context("failed to take PTY writer")?;

        let (output_tx, _rx) = broadcast::channel::<TerminalOutput>(1024);
        let scrollback: SharedScrollback = Arc::new(parking_lot::Mutex::new(Scrollback::new(SCROLLBACK_CAP)));
        let (exit_tx, _exit_rx) = watch::channel(false);

        let sb = scrollback.clone();
        let tx = output_tx.clone();
        let ex = exit_tx.clone();
        let reader_handle = tokio::task::spawn_blocking(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => emit_output(&sb, &tx, buf[..n].to_vec()),
                    Err(e) => {
                        tracing::debug!(error = %e, "PTY reader error");
                        break;
                    }
                }
            }
            emit_output(&sb, &tx, b"\r\n[process exited]\r\n".to_vec());
            ex.send_replace(true);
        });

        let session = TerminalSession {
            id: session_id.clone(),
            token: token.clone(),
            cwd: cwd.to_string(),
            mode: TerminalMode::Local,
            backend: Backend::Local {
                master: pair.master,
                writer: std::sync::Mutex::new(writer),
                child,
                reader_handle,
            },
            output_tx,
            scrollback,
            exit_tx,
        };
        self.sessions.insert(session_id.clone(), session);

        tracing::info!(session_id = %session_id, cwd = %cwd, "local terminal session created");
        Ok((session_id, token))
    }

    /// Create a remote SSH shell session. `conn` must already be established
    /// (SSH connect with retry happens outside the manager lock). Never falls
    /// back to a local shell on failure — any error fails the request.
    pub async fn create_remote(
        &mut self,
        conn: SharedSshHandle,
        hostname: &str,
        remote_path: &str,
        cols: u16,
        rows: u16,
    ) -> Result<(String, String)> {
        let session_id = Uuid::new_v4().to_string();
        let token = Self::generate_secure_token();

        // Channel open is ~1 RTT; safe under the manager lock. (The connect
        // with retries that can take seconds was done by the caller.)
        let mut channel = {
            let mut handle = conn.lock().await;
            handle
                .channel_open_session()
                .await
                .context("failed to open SSH channel")?
        };

        channel
            .request_pty(true, "xterm-256color", cols as u32, rows as u32, 0, 0, &[])
            .await
            .context("SSH pty request failed")?;
        channel
            .request_shell(true)
            .await
            .context("SSH shell request failed")?;

        // RFC 4254: replies arrive in request order — pty first, then shell.
        // Window adjustments may interleave with the replies (benign — skip
        // them). If the server starts the shell without an explicit Success,
        // capture the early data and replay it as the first output chunk.
        let mut early_data: Option<Vec<u8>> = None;
        for what in ["pty", "shell"] {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            let reply = loop {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    return Err(anyhow!("timeout waiting for SSH {} reply", what));
                }
                match tokio::time::timeout(remaining, channel.wait()).await {
                    Ok(Some(russh::ChannelMsg::WindowAdjusted { .. })) => continue,
                    Ok(other) => break other,
                    Err(_) => return Err(anyhow!("timeout waiting for SSH {} reply", what)),
                }
            };
            match reply {
                Some(russh::ChannelMsg::Success) => {}
                Some(russh::ChannelMsg::Data { data }) if what == "shell" => {
                    early_data = Some(data.to_vec());
                }
                Some(russh::ChannelMsg::Failure) => {
                    return Err(anyhow!("remote refused SSH {} request", what));
                }
                other => {
                    tracing::warn!(waiting_for = what, ?other, "unexpected SSH channel message during setup");
                    return Err(anyhow!(
                        "channel closed while waiting for SSH {} reply",
                        what
                    ));
                }
            }
        }

        let (output_tx, _rx) = broadcast::channel::<TerminalOutput>(1024);
        let scrollback: SharedScrollback = Arc::new(parking_lot::Mutex::new(Scrollback::new(SCROLLBACK_CAP)));
        let (exit_tx, _exit_rx) = watch::channel(false);
        let (control_tx, control_rx) = mpsc::unbounded_channel();

        let task_handle = tokio::spawn(run_remote_channel(
            channel,
            early_data,
            control_rx,
            scrollback.clone(),
            output_tx.clone(),
            exit_tx.clone(),
        ));

        let session = TerminalSession {
            id: session_id.clone(),
            token: token.clone(),
            cwd: remote_path.to_string(),
            mode: TerminalMode::Ssh {
                hostname: hostname.to_string(),
            },
            backend: Backend::Remote {
                conn,
                control_tx,
                task_handle,
            },
            output_tx,
            scrollback,
            exit_tx,
        };
        self.sessions.insert(session_id.clone(), session);

        // Land in the project directory on the remote host.
        if !remote_path.is_empty() {
            let cd = format!("cd {}\r", ssh::escape_remote_dir(remote_path));
            let _ = self.write_input(&session_id, cd.as_bytes());
        }

        tracing::info!(session_id = %session_id, hostname = %hostname, remote_path = %remote_path, "ssh terminal session created");
        Ok((session_id, token))
    }

    fn generate_secure_token() -> String {
        use base64::{Engine as _, engine::general_purpose};
        let mut rng = rand::thread_rng();
        let bytes: [u8; 32] = rng.gen();
        general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    }

    pub fn verify_token(&self, session_id: &str, token: &str) -> bool {
        self.sessions
            .get(session_id)
            .map(|s| s.token == token)
            .unwrap_or(false)
    }

    pub fn session_info(&self, session_id: &str) -> Option<TerminalSessionInfo> {
        self.sessions.get(session_id).map(|s| TerminalSessionInfo {
            id: s.id.clone(),
            cwd: s.cwd.clone(),
            mode: s.mode.clone(),
        })
    }

    pub fn resize_session(&self, session_id: &str, cols: u16, rows: u16) -> Result<()> {
        let session = self
            .sessions
            .get(session_id)
            .context("terminal session not found")?;
        match &session.backend {
            Backend::Local { master, .. } => {
                master.resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })?;
                Ok(())
            }
            Backend::Remote { control_tx, .. } => control_tx
                .send(RemoteCtl::Resize(cols, rows))
                .context("ssh terminal task gone"),
        }
    }

    pub fn write_input(&self, session_id: &str, data: &[u8]) -> Result<()> {
        let session = self
            .sessions
            .get(session_id)
            .context("terminal session not found")?;
        match &session.backend {
            Backend::Local { writer, .. } => {
                let mut w = writer
                    .lock()
                    .map_err(|_| anyhow!("PTY writer poisoned"))?;
                w.write_all(data)?;
                w.flush()?;
                Ok(())
            }
            Backend::Remote { control_tx, .. } => control_tx
                .send(RemoteCtl::Input(data.to_vec()))
                .context("ssh terminal task gone"),
        }
    }

    /// Convenience for injecting generated commands: optionally append `\r`.
    pub fn send_text(&self, session_id: &str, text: &str, enter: bool) -> Result<()> {
        let mut data = text.as_bytes().to_vec();
        if enter {
            data.extend_from_slice(b"\r");
        }
        self.write_input(session_id, &data)
    }

    /// Snapshot + live streams, atomic against producers (see `emit_output`).
    pub fn attach(&self, session_id: &str) -> Option<TerminalAttach> {
        let session = self.sessions.get(session_id)?;
        // Snapshot and subscribe under the lock that producers hold for
        // push+send: an event emitted after this point arrives via broadcast,
        // one already in the snapshot never will.
        let guard = session.scrollback.lock();
        let (snapshot, last_seq) = guard.snapshot();
        let output_rx = session.output_tx.subscribe();
        drop(guard);
        Some(TerminalAttach {
            snapshot,
            last_seq,
            output_rx,
            exit_rx: session.exit_tx.subscribe(),
        })
    }

    pub fn close_session(&mut self, session_id: &str) -> Result<()> {
        if let Some(mut session) = self.sessions.remove(session_id) {
            match &mut session.backend {
                Backend::Local {
                    reader_handle,
                    child,
                    ..
                } => {
                    reader_handle.abort();
                    let _ = child.kill();
                }
                Backend::Remote {
                    conn,
                    control_tx,
                    task_handle,
                } => {
                    let _ = control_tx.send(RemoteCtl::Close);
                    task_handle.abort();
                    let conn = conn.clone();
                    tokio::spawn(async move {
                        let handle = conn.lock().await;
                        let _ = handle
                            .disconnect(russh::Disconnect::ByApplication, "", "")
                            .await;
                    });
                }
            }
        }
        tracing::info!(session_id = %session_id, "terminal session closed");
        Ok(())
    }

    pub fn list_sessions(&self) -> Vec<TerminalSessionInfo> {
        self.sessions
            .values()
            .map(|s| TerminalSessionInfo {
                id: s.id.clone(),
                cwd: s.cwd.clone(),
                mode: s.mode.clone(),
            })
            .collect()
    }
}

/// Information about a terminal session (for listing).
#[derive(Debug, Clone, serde::Serialize)]
pub struct TerminalSessionInfo {
    pub id: String,
    pub cwd: String,
    pub mode: TerminalMode,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_pipe() -> (SharedScrollback, broadcast::Sender<TerminalOutput>) {
        let sb = Arc::new(parking_lot::Mutex::new(Scrollback::new(SCROLLBACK_CAP)));
        let (tx, _rx) = broadcast::channel(1024);
        (sb, tx)
    }

    #[test]
    fn scrollback_seq_increments_and_snapshot_concatenates() {
        let sb = Arc::new(parking_lot::Mutex::new(Scrollback::new(SCROLLBACK_CAP)));
        sb.lock().push(b"one".to_vec());
        sb.lock().push(b"two".to_vec());
        let (data, last_seq) = sb.lock().snapshot();
        assert_eq!(data, b"onetwo");
        assert_eq!(last_seq, 2);
    }

    #[test]
    fn scrollback_evicts_whole_chunks() {
        let sb = Arc::new(parking_lot::Mutex::new(Scrollback::new(10)));
        for i in 0..5u8 {
            sb.lock().push(vec![i; 4]); // 5 chunks × 4B = 20B > cap 10
        }
        let (data, last_seq) = sb.lock().snapshot();
        // Evicted down to ≤10B: last two chunks ([3],[4]) = 8 bytes; seqs still count all 5.
        assert_eq!(data, vec![3, 3, 3, 3, 4, 4, 4, 4]);
        assert_eq!(last_seq, 5);
    }

    // The two interleavings that must never duplicate or lose an event.
    #[test]
    fn attach_after_emits_sees_snapshot_then_live() {
        let (sb, tx) = make_pipe();
        emit_output(&sb, &tx, b"a".to_vec());
        emit_output(&sb, &tx, b"b".to_vec());
        let (snapshot, last_seq, mut rx) = {
            let guard = sb.lock();
            let (snap, seq) = guard.snapshot();
            let rx = tx.subscribe();
            drop(guard);
            (snap, seq, rx)
        };
        assert_eq!(snapshot, b"ab");
        assert_eq!(last_seq, 2);
        // Events emitted post-attach arrive via broadcast, seq continues.
        emit_output(&sb, &tx, b"c".to_vec());
        let ev = rx.try_recv().expect("live event");
        assert_eq!((ev.seq, ev.data.as_slice()), (3, &b"c"[..]));
    }

    #[test]
    fn attach_before_emits_sees_all_via_broadcast() {
        let (sb, tx) = make_pipe();
        let (snapshot, last_seq, mut rx) = {
            let guard = sb.lock();
            let (snap, seq) = guard.snapshot();
            let rx = tx.subscribe();
            drop(guard);
            (snap, seq, rx)
        };
        assert!(snapshot.is_empty());
        assert_eq!(last_seq, 0);
        for (i, b) in b"xyz".iter().enumerate() {
            emit_output(&sb, &tx, vec![*b]);
            let ev = rx.try_recv().expect("live event");
            assert_eq!(ev.seq, (i + 1) as u64);
        }
    }

    #[test]
    fn seq_discipline() {
        assert_eq!(seq_action(2, 3), SeqAction::Forward);
        assert_eq!(seq_action(0, 1), SeqAction::Forward);
        assert_eq!(seq_action(2, 2), SeqAction::Skip); // already in snapshot
        assert_eq!(seq_action(2, 1), SeqAction::Skip);
        assert_eq!(seq_action(2, 5), SeqAction::Resync); // lagged
    }

    #[test]
    fn create_local_rejects_missing_cwd_without_home_fallback() {
        let mut mgr = TerminalManager::new();
        let bad = "/definitely/not/a/real/directory/xyz";
        let err = mgr.create_local(bad, 80, 24).unwrap_err().to_string();
        assert!(err.contains(bad), "error should name the bad path: {err}");
        assert!(mgr.list_sessions().is_empty());
    }

    #[tokio::test]
    async fn verify_token_rejects_wrong_or_unknown() {
        let mut mgr = TerminalManager::new();
        let cwd = std::env::temp_dir().to_string_lossy().to_string();
        let (id, token) = mgr
            .create_local(&cwd, 80, 24)
            .expect("temp dir is a valid cwd");
        assert!(mgr.verify_token(&id, &token));
        assert!(!mgr.verify_token(&id, "wrong"));
        assert!(!mgr.verify_token("no-such-id", &token));
        mgr.close_session(&id).unwrap();
    }
}
