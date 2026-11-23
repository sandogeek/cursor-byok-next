//! Quits Cursor so a freshly written proxy setting applies at once.
//!
//! This is an accelerator, never a requirement: a failed probe or kill is
//! reported as `false` and takeover continues, because Cursor reads the
//! setting again the next time it starts.

#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::process::Output;

use super::Result;
use tokio::process::Command;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[cfg(windows)]
fn hide_console(command: &mut Command) {
    command.creation_flags(CREATE_NO_WINDOW);
}

/// Ends a running Cursor. `Ok(false)` means there was none to end.
pub async fn terminate_cursor() -> Result<bool> {
    terminate_platform_cursor().await
}

#[cfg(target_os = "macos")]
async fn terminate_platform_cursor() -> Result<bool> {
    terminate_unix_process("Cursor").await
}

#[cfg(target_os = "linux")]
async fn terminate_platform_cursor() -> Result<bool> {
    terminate_unix_process("cursor").await?;
    terminate_unix_process("Cursor").await
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
async fn terminate_unix_process(name: &str) -> Result<bool> {
    let listed: Output = Command::new("pgrep").args(["-x", name]).output().await?;
    if !listed.status.success() {
        return match listed.status.code() {
            // `pgrep` exits 1 when nothing matches.
            Some(1) => Ok(false),
            _ => Err(super::Error::Process(format!(
                "could not inspect the {name} process"
            ))),
        };
    }
    let killed = Command::new("pkill").args(["-x", name]).status().await?;
    if killed.success() || killed.code() == Some(1) {
        Ok(true)
    } else {
        Err(super::Error::Process(format!(
            "could not terminate the {name} process"
        )))
    }
}

#[cfg(target_os = "windows")]
async fn terminate_platform_cursor() -> Result<bool> {
    let mut list = Command::new("tasklist");
    hide_console(&mut list);
    let processes = list
        .args(["/FI", "IMAGENAME eq Cursor.exe", "/NH", "/FO", "CSV"])
        .output()
        .await?;
    if !processes.status.success() {
        return Err(super::Error::Process(
            "could not inspect the Cursor.exe process".into(),
        ));
    }
    if !String::from_utf8_lossy(&processes.stdout)
        .to_ascii_lowercase()
        .contains("cursor.exe")
    {
        return Ok(false);
    }
    let mut kill = Command::new("taskkill");
    hide_console(&mut kill);
    let killed = kill
        .args(["/F", "/T", "/IM", "Cursor.exe"])
        .status()
        .await?;
    if killed.success() {
        Ok(true)
    } else {
        Err(super::Error::Process(
            "could not terminate the Cursor.exe process".into(),
        ))
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
async fn terminate_platform_cursor() -> Result<bool> {
    Err(super::Error::Process(format!(
        "terminating Cursor is unsupported on {}",
        std::env::consts::OS
    )))
}
