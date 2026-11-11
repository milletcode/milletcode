//! SFTP file transfer implementation
//!
//! Provides file transfer capabilities over SSH connections.

use anyhow::Result;
use russh_sftp::client::{Config as SftpConfig, SftpSession};
use serde::Serialize;
use std::path::Path;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::ssh::SharedSshHandle;

/// One directory entry from `list_entries`.
#[derive(Debug, Serialize)]
pub struct SftpEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

/// SFTP client for remote file operations
pub struct SftpClient {
    session: SftpSession,
}

impl SftpClient {
    /// Create a new SFTP client from an SSH handle
    pub async fn new(ssh_handle: &SharedSshHandle) -> Result<Self> {
        let mut handle = ssh_handle.lock().await;
        let channel = handle.channel_open_session().await?;
        drop(handle);

        // OpenSSH (incl. macOS) only activates the SFTP server on a session
        // channel after a `subsystem "sftp"` request. Without it the server
        // never answers the SFTP INIT, so the handshake hangs until the
        // request timeout fires.
        channel.request_subsystem(true, "sftp").await?;

        // m4-02 is reachable via a 4-hop ProxyJump chain; SFTP handshake can exceed
        // the russh-sftp default 10s request timeout. Raise it to 30s.
        let mut sftp_cfg = SftpConfig::default();
        sftp_cfg.request_timeout_secs = 30;
        let sftp_session = SftpSession::new_with_config(channel.into_stream(), sftp_cfg).await?;

        Ok(Self {
            session: sftp_session,
        })
    }

    /// Absolute path of the remote user's home directory. The SFTP session
    /// starts in the login home, so realpath(".") resolves it.
    pub async fn home_dir(&mut self) -> Result<String> {
        Ok(self.session.canonicalize(".").await?)
    }

    /// Upload a file to the remote host
    pub async fn upload(&mut self, local_path: &Path, remote_path: &str) -> Result<()> {
        let mut local_file = tokio::fs::File::open(local_path).await?;
        let mut remote_file = self.session.create(remote_path).await?;

        let mut buffer = vec![0u8; 8192];
        loop {
            let bytes_read = local_file.read(&mut buffer).await?;
            if bytes_read == 0 {
                break;
            }
            remote_file.write_all(&buffer[..bytes_read]).await?;
        }

        remote_file.shutdown().await?;
        tracing::info!("Uploaded {:?} to {}", local_path, remote_path);
        Ok(())
    }

    /// Download a file from the remote host
    pub async fn download(&mut self, remote_path: &str, local_path: &Path) -> Result<()> {
        let mut remote_file = self.session.open(remote_path).await?;
        let mut local_file = tokio::fs::File::create(local_path).await?;

        let mut buffer = vec![0u8; 8192];
        loop {
            let bytes_read = remote_file.read(&mut buffer).await?;
            if bytes_read == 0 {
                break;
            }
            local_file.write_all(&buffer[..bytes_read]).await?;
        }

        local_file.flush().await?;
        tracing::info!("Downloaded {} to {:?}", remote_path, local_path);
        Ok(())
    }

    /// Read file contents as string
    pub async fn read_to_string(&mut self, remote_path: &str) -> Result<String> {
        let mut remote_file = self.session.open(remote_path).await?;
        let mut contents = String::new();
        remote_file.read_to_string(&mut contents).await?;
        Ok(contents)
    }

    /// Read file contents as bytes
    pub async fn read(&mut self, remote_path: &str) -> Result<Vec<u8>> {
        let mut remote_file = self.session.open(remote_path).await?;
        let mut contents = Vec::new();
        remote_file.read_to_end(&mut contents).await?;
        Ok(contents)
    }

    /// Read at most `max` bytes, returning the bytes and whether the file was
    /// truncated (i.e. larger than `max`).
    pub async fn read_capped(&mut self, remote_path: &str, max: usize) -> Result<(Vec<u8>, bool)> {
        let mut remote_file = self.session.open(remote_path).await?;
        let mut contents = Vec::new();
        let mut truncated = false;
        let mut buffer = vec![0u8; 16384];
        loop {
            let n = remote_file.read(&mut buffer).await?;
            if n == 0 {
                break;
            }
            if contents.len() + n > max {
                let remaining = max - contents.len();
                contents.extend_from_slice(&buffer[..remaining]);
                truncated = true;
                break;
            }
            contents.extend_from_slice(&buffer[..n]);
        }
        Ok((contents, truncated))
    }

    /// Write string contents to a file
    pub async fn write(&mut self, remote_path: &str, contents: &str) -> Result<()> {
        let mut remote_file = self.session.create(remote_path).await?;
        remote_file.write_all(contents.as_bytes()).await?;
        remote_file.shutdown().await?;
        Ok(())
    }

    /// Write bytes to a file
    pub async fn write_bytes(&mut self, remote_path: &str, contents: &[u8]) -> Result<()> {
        let mut remote_file = self.session.create(remote_path).await?;
        remote_file.write_all(contents).await?;
        remote_file.shutdown().await?;
        Ok(())
    }

    /// List directory contents
    pub async fn list_dir(&mut self, remote_path: &str) -> Result<Vec<String>> {
        let entries = self.session.read_dir(remote_path).await?;
        let mut names = Vec::new();

        for entry in entries {
            names.push(entry.file_name());
        }

        Ok(names)
    }

    /// List directory contents with type and size metadata.
    pub async fn list_entries(&mut self, remote_path: &str) -> Result<Vec<SftpEntry>> {
        let entries = self.session.read_dir(remote_path).await?;
        let mut out = Vec::new();

        for entry in entries {
            let metadata = entry.metadata();
            out.push(SftpEntry {
                name: entry.file_name(),
                is_dir: entry.file_type().is_dir(),
                size: metadata.size.unwrap_or(0),
            });
        }

        Ok(out)
    }

    /// Check if a file exists
    pub async fn exists(&mut self, remote_path: &str) -> Result<bool> {
        match self.session.metadata(remote_path).await {
            Ok(_) => Ok(true),
            Err(_) => Ok(false),
        }
    }

    /// Remove a file
    pub async fn remove_file(&mut self, remote_path: &str) -> Result<()> {
        self.session.remove_file(remote_path).await?;
        Ok(())
    }

    /// Create a directory
    pub async fn create_dir(&mut self, remote_path: &str) -> Result<()> {
        self.session.create_dir(remote_path).await?;
        Ok(())
    }

    /// Remove a directory
    pub async fn remove_dir(&mut self, remote_path: &str) -> Result<()> {
        self.session.remove_dir(remote_path).await?;
        Ok(())
    }

    /// Rename a file or directory
    pub async fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        self.session.rename(from, to).await?;
        Ok(())
    }

    /// Get file size
    pub async fn file_size(&mut self, remote_path: &str) -> Result<u64> {
        let attrs = self.session.metadata(remote_path).await?;
        Ok(attrs.size.unwrap_or(0))
    }
}
