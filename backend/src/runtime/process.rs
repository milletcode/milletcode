//! Process abstraction trait
//!
//! Defines the interface for interacting with spawned processes,
//! whether they are local or remote.

use anyhow::Result;
use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncWrite};

/// Process trait for interacting with spawned processes
///
/// Provides a unified interface for stdin/stdout streams and process control.
/// Implementations can wrap local child processes or remote SSH channels.
///
/// Streams are extracted via `take_stdin`/`take_stdout` (ownership transfer),
/// allowing the caller to store them independently from the process handle.
#[async_trait]
pub trait Process: Send {
    /// Take the stdin writer out of the process
    ///
    /// Returns a boxed AsyncWrite that can be used to send data to the process.
    /// After calling this, stdin is no longer available on the process.
    fn take_stdin(&mut self) -> Option<Box<dyn AsyncWrite + Send + Unpin>>;

    /// Take the stdout reader out of the process
    ///
    /// Returns a boxed AsyncRead that can be used to read from the process stdout.
    /// After calling this, stdout is no longer available on the process.
    fn take_stdout(&mut self) -> Option<Box<dyn AsyncRead + Send + Unpin>>;

    /// Take the stderr reader out of the process
    ///
    /// Returns None for transports that don't capture stderr (e.g. local spawns
    /// inheriting the platform's stderr); SSH channels expose the remote's
    /// ExtendedData stream here.
    fn take_stderr(&mut self) -> Option<Box<dyn AsyncRead + Send + Unpin>> {
        None
    }

    /// Wait for the process to exit
    ///
    /// Returns the exit code when the process terminates.
    async fn wait(&mut self) -> Result<i32>;

    /// Kill the process
    ///
    /// Sends a termination signal to the process.
    async fn kill(&mut self) -> Result<()>;

    /// Get the process ID
    ///
    /// Returns the PID for local processes or a unique identifier for remote processes.
    fn id(&self) -> u32;
}
