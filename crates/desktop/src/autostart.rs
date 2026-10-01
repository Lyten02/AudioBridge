//! HKCU `Run` entry: `AudioBridge` = `"<exe>" --background`.

use std::path::Path;

use anyhow::{bail, Context, Result};
use windows::core::{w, HSTRING};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::{RegDeleteKeyValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ};

/// Enables (for the running exe) or disables autostart.
pub fn set(enabled: bool) -> Result<()> {
    let exe = std::env::current_exe().context("current_exe")?;
    set_for(&exe, enabled)
}

/// Enables autostart of `exe`, or removes the entry.
pub fn set_for(exe: &Path, enabled: bool) -> Result<()> {
    let subkey = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
    let name = w!("AudioBridge");
    if enabled {
        let value = HSTRING::from(format!("\"{}\" --background", exe.display()));
        let bytes = (value.len() + 1) * 2; // including the terminating NUL
        // SAFETY: `value` is a NUL-terminated UTF-16 string of `bytes` bytes.
        let rc = unsafe {
            RegSetKeyValueW(HKEY_CURRENT_USER, subkey, name, REG_SZ.0, Some(value.as_ptr().cast()), bytes as u32)
        };
        if rc != ERROR_SUCCESS {
            bail!("RegSetKeyValueW failed: {rc:?}");
        }
    } else {
        // SAFETY: plain registry call with static strings.
        let rc = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, subkey, name) };
        if rc != ERROR_SUCCESS && rc != ERROR_FILE_NOT_FOUND {
            bail!("RegDeleteKeyValueW failed: {rc:?}");
        }
    }
    Ok(())
}
