//! AudioBridge for Windows: streams PC audio to the paired phone and exposes the phone
//! microphone through VB-CABLE. Lives in the notification area.
//!
//! Command line:
//! * `--background` — start hidden in the tray (used by the autostart entry).
//! * `--print-pairing` — print the pairing URI (the QR content) to stdout and exit. If AudioBridge
//!   is already running, prints the URI the running instance published to
//!   `%APPDATA%\AudioBridge\pairing.txt`; otherwise starts the server just long enough to obtain it.
//! * `--no-install` — run from the current location instead of self-installing (release builds
//!   started from anywhere else copy themselves to `%LOCALAPPDATA%\Programs\AudioBridge`,
//!   register autostart there, launch that copy and exit).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod cable_install;
mod autostart;
mod icon;
mod instance;
mod paths;
mod settings;
mod shared;
mod selfinstall;
mod tray;
mod ui;

use std::path::Path;
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use audiobridge_core::pairing::PairingInfo;
use audiobridge_core::session::{Server, ServerConfig};
use windows::core::{w, HSTRING};
use windows::Win32::System::Console::{AttachConsole, GetStdHandle, ATTACH_PARENT_PROCESS, STD_OUTPUT_HANDLE};
use windows::Win32::System::ProcessStatus::K32EmptyWorkingSet;
use windows::Win32::System::Threading::GetCurrentProcess;
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};

use crate::audio::{AudioEngine, Hooks};
use crate::settings::Settings;
use crate::shared::{shared, MainCmd};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|a| a == flag);
    // SAFETY: process-wide setting before any window exists; failure (already set) is harmless.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let data_dir = paths::data_dir();
    if let Err(e) = std::fs::create_dir_all(&data_dir) {
        fatal(&format!("Не удалось создать папку {}: {e}", data_dir.display()));
        return ExitCode::FAILURE;
    }
    if has("--print-pairing") {
        return match print_pairing(&data_dir) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e:#}");
                ExitCode::FAILURE
            }
        };
    }
    let _log_guard = init_logging();
    install_panic_hook();
    if selfinstall::needed(&args) {
        match selfinstall::install_and_launch(&args, &data_dir) {
            Ok(()) => return ExitCode::SUCCESS,
            Err(e) => tracing::warn!("self-install failed, running in place: {e:#}"),
        }
    }
    let primary = match instance::acquire() {
        Ok(Some(p)) => p,
        Ok(None) => {
            // A plain launch brings the running window forward; autostart/background launches stay quiet.
            if !has("--background") {
                instance::signal_existing();
            }
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            fatal(&format!("Не удалось запустить AudioBridge: {e}"));
            return ExitCode::FAILURE;
        }
    };
    tracing::info!("AudioBridge {} starting (background={})", env!("CARGO_PKG_VERSION"), has("--background"));
    match run(primary, has("--background"), &data_dir) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("fatal: {e:#}");
            fatal(&format!("AudioBridge остановлен из-за ошибки:\n{e:#}"));
            ExitCode::FAILURE
        }
    }
}

fn run(primary: instance::Primary, background: bool, data_dir: &Path) -> Result<()> {
    let (settings, first_run) = Settings::load(data_dir);
    if first_run {
        settings.save(data_dir);
    }
    if settings.autostart {
        // Refresh on every start so the entry follows the exe if it moved.
        if let Err(e) = autostart::set(true) {
            tracing::warn!("autostart: {e:#}");
        }
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("audiobridge-rt")
        .enable_all()
        .build()
        .context("tokio runtime")?;
    let pc_name = paths::computer_name();
    let server = rt
        .block_on(Server::start(ServerConfig { data_dir: data_dir.to_path_buf(), pc_name: pc_name.clone() }))
        .context("сетевой сервер")?;
    server.set_pc_audio_enabled(settings.pc_audio_enabled);
    server.set_mic_enabled(settings.mic_enabled);
    let capture = server.pc_audio_capture();
    let playout = server.mic_playout();

    let (main_tx, main_rx) = mpsc::channel();
    let sh = shared::init(data_dir.to_path_buf(), pc_name, server, settings, main_tx);

    let audio = AudioEngine::spawn(
        capture,
        playout,
        Hooks {
            on_devices: Box::new(|d| shared().set_devices(d)),
            on_demand: Box::new(|v| shared().set_mic_demand(v)),
        },
    )
    .context("audio thread")?;
    sh.set_audio_sender(audio.sender());
    sh.spawn_watchers(&rt, audio.sender());
    let tray = tray::spawn().context("tray icon")?;
    primary.listen_for_show(|| shared().show_window());
    primary.listen_for_quit(|| shared().request_exit());

    if !background {
        ui::run_window();
        trim_working_set();
    }
    while !sh.is_exiting() {
        match main_rx.recv() {
            Ok(MainCmd::Show) => {
                ui::run_window();
                trim_working_set();
            }
            Ok(MainCmd::Exit) | Err(_) => break,
        }
    }

    tracing::info!("shutting down");
    tray.quit();
    audio.shutdown();
    if let Some(server) = sh.take_server() {
        rt.block_on(async {
            if tokio::time::timeout(Duration::from_secs(3), server.shutdown()).await.is_err() {
                tracing::warn!("server shutdown timed out");
            }
        });
    }
    rt.shutdown_timeout(Duration::from_secs(1));
    Ok(())
}

/// `--print-pairing`: see the crate docs.
fn print_pairing(data_dir: &Path) -> Result<()> {
    attach_console();
    let uri = if instance::another_running() {
        let file = data_dir.join("pairing.txt");
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            match std::fs::read_to_string(&file) {
                Ok(s) if !s.trim().is_empty() => break s.trim().to_owned(),
                _ if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(200)),
                _ => anyhow::bail!("the running instance has not published {}", file.display()),
            }
        }
    } else {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
        rt.block_on(async {
            let server = Server::start(ServerConfig { data_dir: data_dir.to_path_buf(), pc_name: paths::computer_name() })
                .await?;
            let mut rx = server.pairing();
            let got = tokio::time::timeout(Duration::from_secs(30), rx.wait_for(Option::is_some)).await;
            let uri = match got {
                Ok(Ok(p)) => p.as_ref().map(PairingInfo::to_uri),
                _ => None,
            };
            server.shutdown().await;
            uri.context("pairing info not available within 30 s")
        })?
    };
    println!("{uri}");
    Ok(())
}

/// Release builds are GUI-subsystem: attach to the parent console only if stdout isn't redirected.
fn attach_console() {
    // SAFETY: plain console API calls.
    unsafe {
        let has_stdout = GetStdHandle(STD_OUTPUT_HANDLE).is_ok_and(|h| !h.is_invalid() && !h.0.is_null());
        if !has_stdout {
            let _ = AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
}

fn init_logging() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::filter::Targets;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::Layer;

    let filter = std::env::var("AUDIOBRIDGE_LOG")
        .ok()
        .and_then(|s| s.parse::<Targets>().ok())
        .unwrap_or_else(|| {
            Targets::new()
                .with_default(tracing::Level::WARN)
                .with_target("AudioBridge", tracing::Level::INFO)
                .with_target("audiobridge_core", tracing::Level::INFO)
        });
    let dir = paths::log_dir();
    let _ = std::fs::create_dir_all(&dir);
    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("audiobridge")
        .filename_suffix("log")
        .max_log_files(7)
        .build(&dir)
        .ok()?;
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let file_layer = tracing_subscriber::fmt::layer().with_writer(writer).with_ansi(false).with_filter(filter.clone());
    let console_layer = cfg!(debug_assertions)
        .then(|| tracing_subscriber::fmt::layer().with_writer(std::io::stderr).with_filter(filter));
    tracing_subscriber::registry().with(file_layer).with(console_layer).try_init().ok()?;
    Some(guard)
}

fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("panic: {info}");
        default(info);
    }));
}

/// Return memory touched by the (now destroyed) window to the OS.
fn trim_working_set() {
    // SAFETY: pseudo-handle of the current process.
    unsafe {
        let _ = K32EmptyWorkingSet(GetCurrentProcess());
    }
}

fn fatal(text: &str) {
    // SAFETY: modal message box with valid strings.
    unsafe {
        MessageBoxW(None, &HSTRING::from(text), w!("AudioBridge"), MB_OK | MB_ICONERROR);
    }
}
