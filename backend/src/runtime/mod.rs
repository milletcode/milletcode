//! Runtime abstraction for process execution
//!
//! This module provides a unified interface for spawning and managing processes,
//! abstracting away the differences between local and remote execution.

mod local;
pub mod port_forward;
mod process;
mod sftp;
pub mod skills;
pub mod ssh;
pub mod tunnel;
pub mod web_proxy;

pub use local::LocalRuntime;
pub use process::Process;
pub use sftp::SftpClient;
pub use ssh::{SharedSshHandle, SshHandler, SshRuntime, parse_ssh_config};

use anyhow::Result;
use async_trait::async_trait;

/// Runtime trait for spawning and managing processes
///
/// Implementations can execute processes locally or remotely (e.g., via SSH).
/// The trait provides a unified interface that hides the execution environment details.
#[async_trait]
pub trait Runtime: Send + Sync {
    /// Spawn a new process
    ///
    /// # Arguments
    /// * `command` - The command to execute
    /// * `args` - Command arguments
    /// * `working_dir` - Optional working directory
    /// * `env` - Environment variables to set
    ///
    /// # Returns
    /// A boxed Process instance that can be used to interact with the spawned process
    async fn spawn(
        &self,
        command: &str,
        args: &[&str],
        working_dir: Option<&str>,
        env: Vec<(&str, &str)>,
    ) -> Result<Box<dyn Process>>;
}

/// Build command arguments for Claude CLI
///
/// This is a shared helper used by both LocalRuntime and SshRuntime to construct
/// the claude command line with all necessary flags.
pub fn build_claude_args(
    binary: &str,
    claude_session_id: Option<&str>,
    is_resume: bool,
    _auto_approve: bool,
    extra_args: &[String],
    name: Option<&str>,
) -> (String, Vec<String>) {
    let mut args = vec![
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--input-format".to_string(),
        "stream-json".to_string(),
        "--verbose".to_string(),
        "--replay-user-messages".to_string(),
    ];

    if let Some(n) = name {
        args.push("--name".to_string());
        args.push(n.to_string());
    }

    match (claude_session_id, is_resume) {
        (Some(uuid), true) => {
            args.push("--resume".to_string());
            args.push(uuid.to_string());
        }
        (Some(uuid), false) => {
            args.push("--session-id".to_string());
            args.push(uuid.to_string());
        }
        (None, _) => {
            args.push("--continue".to_string());
        }
    }

    // In-band control protocol: every permission decision (tool approvals and
    // AskUserQuestion alike) arrives as a control_request on stdout and must be
    // answered with a control_response on stdin. Blocking is indefinite by
    // design — there is deliberately no bypass and no timeout.
    args.push("--permission-prompt-tool".to_string());
    args.push("stdio".to_string());
    // File edits (Write/Edit) are auto-accepted; Bash and other tools still
    // route through the control protocol above.
    args.push("--permission-mode".to_string());
    args.push("acceptEdits".to_string());

    for arg in extra_args {
        args.push(arg.clone());
    }

    (binary.to_string(), args)
}
