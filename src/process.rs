//! Synchronous subprocess execution with timeouts.
//!
//! All VCS interaction goes through the native CLI tools (git, hg, svn, bzr),
//! mirroring vcstool's approach. This module centralizes spawning, timeout
//! handling and failure reporting.

use std::process::{Command, Stdio};
use std::time::Duration;
use wait_timeout::ChildExt;

use crate::error::{Result, IrError};

/// Default timeout for network-heavy commands (clone, fetch).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);
/// Timeout for quick local commands (rev-parse, status, ...).
pub const LOCAL_TIMEOUT: Duration = Duration::from_secs(30);

/// Captured output of a successful command.
#[derive(Debug)]
pub struct CmdOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl CmdOutput {
    /// stdout as UTF-8, trimmed of trailing whitespace/newlines.
    pub fn stdout_trimmed(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim().to_string()
    }
}

/// Render a command as `prog arg1 arg2 ...` for error messages.
fn cmdline(cmd: &Command) -> String {
    let mut parts = vec![cmd.get_program().to_string_lossy().into_owned()];
    parts.extend(cmd.get_args().map(|a| a.to_string_lossy().into_owned()));
    parts.join(" ")
}

/// Run a command to completion, capturing stdout/stderr.
///
/// Returns [`IrError::CommandFailed`] on non-zero exit and
/// [`IrError::CommandTimeout`] when the timeout elapses (the child is
/// killed in that case).
pub fn run(cmd: &mut Command, timeout: Duration) -> Result<CmdOutput> {
    let cmd_str = cmdline(cmd);
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| IrError::SpawnFailed {
            cmd: cmd_str.clone(),
            source,
        })?;
    match child
        .wait_timeout(timeout)
        .map_err(|source| IrError::SpawnFailed {
            cmd: cmd_str.clone(),
            source,
        })? {
        Some(status) => {
            let output = child
                .wait_with_output()
                .map_err(|source| IrError::SpawnFailed {
                    cmd: cmd_str.clone(),
                    source,
                })?;
            if status.success() {
                Ok(CmdOutput {
                    stdout: output.stdout,
                    stderr: output.stderr,
                })
            } else {
                Err(IrError::CommandFailed {
                    cmd: cmd_str,
                    status: status.to_string(),
                    stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
                })
            }
        }
        None => {
            let _ = child.kill();
            let _ = child.wait();
            Err(IrError::CommandTimeout(cmd_str, timeout))
        }
    }
}
