//! Well-known locations and machine identity.

use std::path::PathBuf;

use windows::core::PWSTR;
use windows::Win32::System::SystemInformation::{ComputerNamePhysicalDnsHostname, GetComputerNameExW};

const APP_DIR: &str = "AudioBridge";

fn known_dir(var: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .or_else(|| std::env::current_exe().ok().and_then(|p| p.parent().map(PathBuf::from)))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `%APPDATA%\AudioBridge`: keys, pairing secret, `settings.json`, `pairing.txt`.
pub fn data_dir() -> PathBuf {
    known_dir("APPDATA").join(APP_DIR)
}

/// `%LOCALAPPDATA%\AudioBridge\logs`.
pub fn log_dir() -> PathBuf {
    known_dir("LOCALAPPDATA").join(APP_DIR).join("logs")
}

/// The computer's host name as the user set it (e.g. "LYTEN").
pub fn computer_name() -> String {
    let mut buf = [0u16; 256];
    let mut len = buf.len() as u32;
    // SAFETY: buffer and length describe valid writable memory.
    let ok = unsafe { GetComputerNameExW(ComputerNamePhysicalDnsHostname, Some(PWSTR(buf.as_mut_ptr())), &mut len) };
    match ok {
        Ok(()) if len > 0 => String::from_utf16_lossy(&buf[..len as usize]),
        _ => std::env::var("COMPUTERNAME").unwrap_or_else(|_| "PC".to_owned()),
    }
}
