//! Synchronous subprocess execution with timeouts.
//!
//! All VCS interaction goes through the native CLI tools (git, hg, svn, bzr),
//! mirroring vcstool's approach. This module centralizes spawning, timeout
//! handling and failure reporting.

use std::process::{Command, Stdio};
use std::time::Duration;
use wait_timeout::ChildExt;

use crate::error::{IrError, Result};

/// Default timeout for network-heavy commands (clone, fetch).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);
/// Timeout for quick local commands (rev-parse, status, ...).
pub const LOCAL_TIMEOUT: Duration = Duration::from_secs(30);

/// Retry policy for network operations.
///
/// Read from the environment: `IR_RETRIES` (total attempts, default 3),
/// `IR_RETRY_BASE_MS` (base backoff in milliseconds, default 1000, doubled
/// after each failed attempt). The `--retries` CLI flag sets `IR_RETRIES`.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Total attempts including the first one (>= 1).
    pub attempts: u32,
    pub base: Duration,
}

impl RetryPolicy {
    pub fn from_env() -> Self {
        let attempts = std::env::var("IR_RETRIES")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&n| n >= 1)
            .unwrap_or(3);
        let base_ms = std::env::var("IR_RETRY_BASE_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1000);
        Self {
            attempts,
            base: Duration::from_millis(base_ms),
        }
    }
}

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

/// Run a fallible operation, retrying failures with exponential backoff.
///
/// Sleeps `base * 2^(n-1)` before attempt `n + 1` and returns the last
/// error when all attempts fail. Retries are silent; each attempt is still
/// bounded by the operation's own timeout.
pub fn retry<F, T>(policy: &RetryPolicy, mut op: F) -> Result<T>
where
    F: FnMut() -> Result<T>,
{
    let mut last_err = None;
    for attempt in 1..=policy.attempts {
        match op() {
            Ok(v) => return Ok(v),
            Err(e) => {
                last_err = Some(e);
                if attempt < policy.attempts {
                    std::thread::sleep(policy.base * 2u32.pow(attempt - 1));
                }
            }
        }
    }
    Err(last_err.expect("policy.attempts >= 1"))
}

/// Run a command, retrying failures with exponential backoff.
///
/// Intended for idempotent network operations (fetch, pull, download).
pub fn run_retry(cmd: &mut Command, timeout: Duration, policy: &RetryPolicy) -> Result<CmdOutput> {
    retry(policy, || run(cmd, timeout))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_policy_from_env_defaults() {
        std::env::remove_var("IR_RETRIES");
        std::env::remove_var("IR_RETRY_BASE_MS");
        let p = RetryPolicy::from_env();
        assert_eq!(p.attempts, 3);
        assert_eq!(p.base, Duration::from_millis(1000));
    }

    #[test]
    fn retry_policy_from_env_custom() {
        std::env::set_var("IR_RETRIES", "5");
        std::env::set_var("IR_RETRY_BASE_MS", "50");
        let p = RetryPolicy::from_env();
        assert_eq!(p.attempts, 5);
        assert_eq!(p.base, Duration::from_millis(50));
        std::env::remove_var("IR_RETRIES");
        std::env::remove_var("IR_RETRY_BASE_MS");
    }

    #[test]
    fn retry_policy_rejects_zero() {
        std::env::set_var("IR_RETRIES", "0");
        let p = RetryPolicy::from_env();
        assert_eq!(p.attempts, 3);
        std::env::remove_var("IR_RETRIES");
    }

    /// A failing command is attempted exactly `attempts` times.
    #[test]
    fn run_retry_attempts_failures() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("attempts.log");
        let mut cmd = Command::new("sh");
        cmd.args([
            "-c",
            &format!("echo x >> {}; exit 1", log.to_string_lossy()),
        ]);
        let policy = RetryPolicy {
            attempts: 3,
            base: Duration::from_millis(1),
        };
        let err = run_retry(&mut cmd, Duration::from_secs(10), &policy).unwrap_err();
        assert!(matches!(err, IrError::CommandFailed { .. }));
        let lines = std::fs::read_to_string(&log).unwrap();
        assert_eq!(lines.lines().count(), 3);
    }

    #[test]
    fn run_retry_succeeds_first_try() {
        let mut cmd = Command::new("true");
        let policy = RetryPolicy {
            attempts: 3,
            base: Duration::from_millis(1),
        };
        run_retry(&mut cmd, Duration::from_secs(10), &policy).unwrap();
    }
}
