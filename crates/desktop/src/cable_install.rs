//! One-click VB-CABLE installation: download the official driver pack, extract it, and run
//! the vendor installer elevated. Device notifications pick up the new endpoints afterwards.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use windows::core::{w, HSTRING, IUnknown};
use windows::Win32::Foundation::{CloseHandle, ERROR_CANCELLED};
use windows::Win32::System::Com::IBindStatusCallback;
use windows::Win32::System::Com::Urlmon::URLDownloadToFileW;
use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, CREATE_NO_WINDOW, INFINITE};
use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use crate::audio::com::Com;
use crate::shared::shared;

const PACK_URL: &str = "https://download.vb-audio.com/Download_CABLE/VBCABLE_Driver_Pack45.zip";
const PACK_FILE: &str = "VBCABLE_Driver_Pack45.zip";
const SETUP_EXE: &str = "VBCABLE_Setup_x64.exe";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum InstallState {
    #[default]
    Idle,
    Downloading,
    Extracting,
    Installing,
    Failed(String),
}

impl InstallState {
    pub fn busy(&self) -> bool {
        matches!(self, Self::Downloading | Self::Extracting | Self::Installing)
    }
}

/// Starts the installation on a background thread (no-op if one is already running).
pub fn start() {
    let sh = shared();
    if sh.cable_install_state().busy() {
        return;
    }
    sh.set_cable_install_state(InstallState::Downloading);
    let spawned = std::thread::Builder::new().name("vbcable-install".into()).spawn(|| {
        let sh = shared();
        let result = run(|s| sh.set_cable_install_state(s));
        match result {
            Ok(()) => {
                tracing::info!("VB-CABLE installer finished");
                sh.set_cable_install_state(InstallState::Idle);
            }
            Err(e) => {
                tracing::warn!("VB-CABLE installation failed: {e:#}");
                sh.set_cable_install_state(InstallState::Failed(format!("{e:#}")));
            }
        }
        sh.rescan_devices();
    });
    if let Err(e) = spawned {
        sh.set_cable_install_state(InstallState::Failed(e.to_string()));
    }
}

fn run(set: impl Fn(InstallState)) -> Result<()> {
    let _com = Com::init();
    let dir = std::env::temp_dir().join("AudioBridge-VBCABLE");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).with_context(|| format!("создание {}", dir.display()))?;
    let zip = dir.join(PACK_FILE);

    // SAFETY: valid NUL-terminated strings; no caller/callback objects.
    unsafe {
        URLDownloadToFileW(
            None::<&IUnknown>,
            &HSTRING::from(PACK_URL),
            &HSTRING::from(zip.as_os_str()),
            0,
            None::<&IBindStatusCallback>,
        )
    }
    .context("не удалось скачать VB-CABLE")?;

    set(InstallState::Extracting);
    extract(&zip, &dir)?;
    let setup = dir.join(SETUP_EXE);
    if !setup.exists() {
        bail!("в архиве нет {SETUP_EXE}");
    }

    set(InstallState::Installing);
    run_elevated_and_wait(&setup, &dir)
}

/// Extracts with the `tar.exe` (bsdtar) that ships with Windows 10 1803+ and reads zip files.
fn extract(zip: &Path, dir: &Path) -> Result<()> {
    let windir = std::env::var_os("WINDIR").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
    let status = Command::new(windir.join("System32").join("tar.exe"))
        .arg("-xf")
        .arg(zip)
        .arg("-C")
        .arg(dir)
        .creation_flags(CREATE_NO_WINDOW.0)
        .status()
        .context("не удалось распаковать архив")?;
    if !status.success() {
        bail!("распаковка завершилась с ошибкой ({status})");
    }
    Ok(())
}

fn run_elevated_and_wait(exe: &Path, dir: &Path) -> Result<()> {
    let file = HSTRING::from(exe.as_os_str());
    let directory = HSTRING::from(dir.as_os_str());
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: w!("runas"),
        lpFile: windows::core::PCWSTR(file.as_ptr()),
        lpDirectory: windows::core::PCWSTR(directory.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    // SAFETY: `info` and the strings it points to outlive the call; the process handle is closed below.
    unsafe {
        if let Err(e) = ShellExecuteExW(&mut info) {
            if e.code() == ERROR_CANCELLED.to_hresult() {
                bail!("установка отменена");
            }
            return Err(e).context("не удалось запустить установщик");
        }
        if info.hProcess.is_invalid() {
            return Ok(());
        }
        WaitForSingleObject(info.hProcess, INFINITE);
        let mut code = 0u32;
        let _ = GetExitCodeProcess(info.hProcess, &mut code);
        let _ = CloseHandle(info.hProcess);
        tracing::info!("VB-CABLE setup exited with code {code}");
    }
    Ok(())
}
