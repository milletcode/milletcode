//! Local process runtime implementation
//!
//! Wraps tokio::process::Child to implement the Process trait.

use anyhow::Result;
use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::process::{Child, Command};

use super::{Process, Runtime};

/// Local process wrapper
pub struct LocalProcess {
    child: Child,
    pid: u32,
}

impl LocalProcess {
    pub fn new(mut child: Child) -> Self {
        let pid = child.id().unwrap_or(0);
        Self { child, pid }
    }
}

#[async_trait]
impl Process for LocalProcess {
    fn take_stdin(&mut self) -> Option<Box<dyn AsyncWrite + Send + Unpin>> {
        self.child.stdin.take().map(|s| Box::new(s) as Box<dyn AsyncWrite + Send + Unpin>)
    }

    fn take_stdout(&mut self) -> Option<Box<dyn AsyncRead + Send + Unpin>> {
        self.child.stdout.take().map(|s| Box::new(s) as Box<dyn AsyncRead + Send + Unpin>)
    }

    fn take_stderr(&mut self) -> Option<Box<dyn AsyncRead + Send + Unpin>> {
        self.child.stderr.take().map(|s| Box::new(s) as Box<dyn AsyncRead + Send + Unpin>)
    }

    async fn wait(&mut self) -> Result<i32> {
        let status = self.child.wait().await?;
        Ok(status.code().unwrap_or(-1))
    }

    async fn kill(&mut self) -> Result<()> {
        self.child.kill().await?;
        Ok(())
    }

    fn id(&self) -> u32 {
        self.pid
    }
}

/// Local runtime that spawns processes on the local machine
pub struct LocalRuntime;

impl LocalRuntime {
    pub fn new() -> Self {
        Self
    }
}

impl Default for LocalRuntime {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Runtime for LocalRuntime {
    async fn spawn(
        &self,
        command: &str,
        args: &[&str],
        working_dir: Option<&str>,
        env: Vec<(&str, &str)>,
    ) -> Result<Box<dyn Process>> {
        let mut cmd = Command::new(command);
        cmd.args(args);

        for (key, value) in env {
            cmd.env(key, value);
        }

        if let Some(dir) = working_dir {
            cmd.current_dir(dir);
        }

        // Always pipe stdin/stdout/stderr
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let child = cmd.spawn()?;
        Ok(Box::new(LocalProcess::new(child)))
    }
}
