//! Copy-one-exe setup: a release build started from anywhere other than
//! `%LOCALAPPDATA%\Programs\AudioBridge\AudioBridge.exe` installs itself there (replacing a
//! different build, stopping it first), points autostart at it, starts it and exits.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use windows::Win32::System::Threading::{CREATE_NO_WINDOW, DETACHED_PROCESS};
use windows::Win32::UI::WindowsAndMessaging::{AllowSetForegroundWindow, ASFW_ANY};

use crate::settings::Settings;
use crate::{autostart, instance};

pub fn installed_exe() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map_or_else(|| PathBuf::from("."), PathBuf::from);
    base.join("Programs").join("AudioBridge").join("AudioBridge.exe")
}

fn same_path(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| {
        std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()).to_string_lossy().to_lowercase()
    };
    norm(a) == norm(b)
}

fn same_contents(a: &Path, b: &Path) -> bool {
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(ma), Ok(mb)) if ma.len() == mb.len() => {
            matches!((std::fs::read(a), std::fs::read(b)), (Ok(x), Ok(y)) if x == y)
        }
        _ => false,
    }
}

/// True if this process should hand off to the installed copy.
/// Debug builds and `--no-install` run in place (development / portable use).
pub fn needed(args: &[String]) -> bool {
    if cfg!(debug_assertions) || args.iter().any(|a| a == "--no-install") {
        return false;
    }
    match std::env::current_exe() {
        Ok(exe) => !same_path(&exe, &installed_exe()),
        Err(_) => false,
    }
}

/// Installs (if the installed copy differs), registers autostart and launches the installed copy
/// with the same arguments. On success the caller exits.
pub fn install_and_launch(args: &[String], data_dir: &Path) -> Result<()> {
    let src = std::env::current_exe().context("current_exe")?;
    let dst = installed_exe();
    if same_contents(&src, &dst) {
        tracing::info!("installed copy is up to date: {}", dst.display());
    } else {
        stop_running_instance()?;
        let dir = dst.parent().context("install dir")?;
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let tmp = dst.with_extension("exe.new");
        std::fs::copy(&src, &tmp).with_context(|| format!("copy to {}", tmp.display()))?;
        replace_with_retry(&tmp, &dst)?;
        tracing::info!("installed {} -> {}", src.display(), dst.display());
    }
    let (settings, _) = Settings::load(data_dir);
    if settings.autostart {
        autostart::set_for(&dst, true)?;
    }
    // SAFETY: plain user32 call; lets the installed copy bring its window to the foreground.
    unsafe {
        let _ = AllowSetForegroundWindow(ASFW_ANY);
    }
    Command::new(&dst)
        .args(args)
        .creation_flags(DETACHED_PROCESS.0)
        .spawn()
        .with_context(|| format!("start {}", dst.display()))?;
    Ok(())
}

/// Asks a running AudioBridge to exit and waits for it; force-stops it if it does not react
/// (builds that predate the quit event).
fn stop_running_instance() -> Result<()> {
    if !instance::another_running() {
        return Ok(());
    }
    instance::signal_quit();
    if wait_gone(Duration::from_secs(8)) {
        return Ok(());
    }
    tracing::warn!("running instance did not exit; terminating it");
    let _ = Command::new("taskkill")
        .args(["/F", "/FI", "IMAGENAME eq AudioBridge.exe", "/FI"])
        .arg(format!("PID ne {}", std::process::id()))
        .creation_flags(CREATE_NO_WINDOW.0)
        .status();
    if wait_gone(Duration::from_secs(5)) {
        Ok(())
    } else {
        bail!("the running AudioBridge could not be stopped")
    }
}

fn wait_gone(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !instance::another_running() {
            // The exe file handle is released when the process object goes away; give it a moment.
            std::thread::sleep(Duration::from_millis(300));
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}


/// The just-exited exe can stay locked for a moment (antivirus scan, loader teardown).
fn replace_with_retry(tmp: &Path, dst: &Path) -> Result<()> {
    let mut attempt = 0;
    loop {
        match std::fs::rename(tmp, dst) {
            Ok(()) => return Ok(()),
            Err(e) if attempt < 20 => {
                attempt += 1;
                tracing::debug!("replace {} failed ({e}); retrying", dst.display());
                std::thread::sleep(Duration::from_millis(250));
            }
            Err(e) => {
                let _ = std::fs::remove_file(tmp);
                return Err(e).with_context(|| format!("replace {}", dst.display()));
            }
        }
    }
}