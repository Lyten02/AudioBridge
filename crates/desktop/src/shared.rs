//! Process-wide state shared by the UI, tray, audio hooks and status watchers.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};

use audiobridge_core::pairing::PairingInfo;
use audiobridge_core::session::{ConnState, Server, Status};
use tokio::sync::watch;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    IsIconic, PostMessageW, SetForegroundWindow, ShowWindow, SW_RESTORE, SW_SHOW,
};

use crate::audio::{set_default_render, AudioMsg, Desired, DeviceSummary};
use crate::cable_install::InstallState;
use crate::settings::Settings;
use crate::{autostart, tray};

pub enum MainCmd {
    Show,
    Exit,
}

pub struct Shared {
    pub data_dir: PathBuf,
    pub pc_name: String,
    server: Mutex<Option<Server>>,
    pub status: watch::Receiver<Status>,
    pub pairing: watch::Receiver<Option<PairingInfo>>,
    settings: Mutex<Settings>,
    devices: Mutex<DeviceSummary>,
    ui_ctx: Mutex<Option<eframe::egui::Context>>,
    window_hwnd: AtomicIsize,
    tray_hwnd: AtomicIsize,
    tray_connected: AtomicBool,
    tray_tip: Mutex<String>,
    main_tx: Sender<MainCmd>,
    exiting: AtomicBool,
    audio_tx: Mutex<Option<Sender<AudioMsg>>>,
    cable_install: Mutex<InstallState>,
}

static SHARED: OnceLock<Shared> = OnceLock::new();

/// The process-wide state. Panics only if called before `init` (a programming error).
pub fn shared() -> &'static Shared {
    SHARED.get().expect("shared state not initialised")
}

pub fn init(
    data_dir: PathBuf,
    pc_name: String,
    server: Server,
    settings: Settings,
    main_tx: Sender<MainCmd>,
) -> &'static Shared {
    let status = server.status();
    let pairing = server.pairing();
    let state = Shared {
        data_dir,
        pc_name,
        server: Mutex::new(Some(server)),
        status,
        pairing,
        settings: Mutex::new(settings),
        devices: Mutex::new(DeviceSummary::default()),
        ui_ctx: Mutex::new(None),
        window_hwnd: AtomicIsize::new(0),
        tray_hwnd: AtomicIsize::new(0),
        tray_connected: AtomicBool::new(false),
        tray_tip: Mutex::new("AudioBridge — Ожидание телефона".to_owned()),
        main_tx,
        exiting: AtomicBool::new(false),
        audio_tx: Mutex::new(None),
        cable_install: Mutex::new(InstallState::Idle),
    };
    if SHARED.set(state).is_err() {
        panic!("shared state initialised twice");
    }
    shared()
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Shared {
    fn with_server(&self, f: impl FnOnce(&Server)) {
        if let Some(s) = lock(&self.server).as_ref() {
            f(s);
        }
    }

    pub fn take_server(&self) -> Option<Server> {
        lock(&self.server).take()
    }

    pub fn settings(&self) -> Settings {
        lock(&self.settings).clone()
    }

    fn update_settings(&self, f: impl FnOnce(&mut Settings)) {
        let snapshot = {
            let mut s = lock(&self.settings);
            f(&mut s);
            s.clone()
        };
        snapshot.save(&self.data_dir);
        self.repaint();
    }

    pub fn set_pc_audio(&self, on: bool) {
        self.with_server(|s| s.set_pc_audio_enabled(on));
        self.update_settings(|s| s.pc_audio_enabled = on);
    }

    pub fn set_mic(&self, on: bool) {
        self.with_server(|s| s.set_mic_enabled(on));
        self.update_settings(|s| s.mic_enabled = on);
    }

    pub fn set_autostart(&self, on: bool) {
        if let Err(e) = autostart::set(on) {
            tracing::warn!("autostart update failed: {e:#}");
            return;
        }
        self.update_settings(|s| s.autostart = on);
    }

    pub fn set_mic_demand(&self, demanded: bool) {
        self.with_server(|s| s.set_mic_demand(demanded));
    }

    pub fn devices(&self) -> DeviceSummary {
        lock(&self.devices).clone()
    }

    pub fn set_devices(&self, d: &DeviceSummary) {
        *lock(&self.devices) = d.clone();
        if !d.default_is_cable {
            if let Some(id) = &d.default_render_id {
                if lock(&self.settings).last_default_render.as_ref() != Some(id) {
                    let id = id.clone();
                    self.update_settings(|s| s.last_default_render = Some(id));
                }
            }
        }
        self.repaint();
    }

    pub fn set_audio_sender(&self, tx: Sender<AudioMsg>) {
        *lock(&self.audio_tx) = Some(tx);
    }

    /// Re-reads the device list (e.g. after the VB-CABLE installer exits).
    pub fn rescan_devices(&self) {
        if let Some(tx) = lock(&self.audio_tx).as_ref() {
            let _ = tx.send(AudioMsg::DevicesChanged);
        }
    }

    /// Makes `id` the default playback device again (undoes a VB-CABLE takeover).
    pub fn restore_default_render(&'static self, id: String) {
        let spawned = std::thread::Builder::new().name("set-default-device".into()).spawn(move || {
            match set_default_render(&id) {
                Ok(()) => tracing::info!("default playback device restored to {id}"),
                Err(e) => tracing::warn!("restoring the default playback device failed: {e:#}"),
            }
            self.rescan_devices();
        });
        if let Err(e) = spawned {
            tracing::warn!("set-default-device thread: {e}");
        }
    }

    pub fn cable_install_state(&self) -> InstallState {
        lock(&self.cable_install).clone()
    }

    pub fn set_cable_install_state(&self, s: InstallState) {
        *lock(&self.cable_install) = s;
        self.repaint();
    }

    // ---- window -----------------------------------------------------------------------------

    pub fn register_window(&self, hwnd: isize, ctx: eframe::egui::Context) {
        *lock(&self.ui_ctx) = Some(ctx);
        self.window_hwnd.store(hwnd, Ordering::SeqCst);
    }

    pub fn unregister_window(&self) {
        self.window_hwnd.store(0, Ordering::SeqCst);
        *lock(&self.ui_ctx) = None;
    }

    /// Requests a repaint only while the window exists; costs nothing otherwise.
    pub fn repaint(&self) {
        if let Some(ctx) = lock(&self.ui_ctx).as_ref() {
            ctx.request_repaint();
        }
    }

    /// Brings the window to front, creating it if it is closed.
    pub fn show_window(&self) {
        let hwnd = self.window_hwnd.load(Ordering::SeqCst);
        if hwnd != 0 {
            let hwnd = HWND(hwnd as *mut _);
            // SAFETY: plain user32 calls on a window handle we created (stale handles fail harmlessly).
            unsafe {
                let _ = ShowWindow(hwnd, if IsIconic(hwnd).as_bool() { SW_RESTORE } else { SW_SHOW });
                let _ = SetForegroundWindow(hwnd);
            }
            self.repaint();
        } else if !self.is_exiting() {
            let _ = self.main_tx.send(MainCmd::Show);
        }
    }

    pub fn request_exit(&self) {
        self.exiting.store(true, Ordering::SeqCst);
        let _ = self.main_tx.send(MainCmd::Exit);
        if let Some(ctx) = lock(&self.ui_ctx).as_ref() {
            ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Close);
            ctx.request_repaint();
        }
    }

    pub fn is_exiting(&self) -> bool {
        self.exiting.load(Ordering::SeqCst)
    }

    // ---- tray -------------------------------------------------------------------------------

    pub fn set_tray_hwnd(&self, hwnd: isize) {
        self.tray_hwnd.store(hwnd, Ordering::SeqCst);
    }

    pub fn tray_state(&self) -> (bool, String) {
        (self.tray_connected.load(Ordering::SeqCst), lock(&self.tray_tip).clone())
    }

    fn update_tray(&self, connected: bool, tip: String) {
        let changed = {
            let mut cur = lock(&self.tray_tip);
            let changed = *cur != tip || self.tray_connected.load(Ordering::SeqCst) != connected;
            *cur = tip;
            changed
        };
        self.tray_connected.store(connected, Ordering::SeqCst);
        if changed {
            let hwnd = self.tray_hwnd.load(Ordering::SeqCst);
            if hwnd != 0 {
                // SAFETY: posting to our own tray window.
                unsafe {
                    let _ = PostMessageW(Some(HWND(hwnd as *mut _)), tray::WM_TRAY_REFRESH, WPARAM(0), LPARAM(0));
                }
            }
        }
    }

    // ---- watchers ---------------------------------------------------------------------------

    /// Mirrors status changes into the audio engine, tray and UI. Runs on the tokio runtime.
    pub fn spawn_watchers(&'static self, rt: &tokio::runtime::Runtime, audio: Sender<AudioMsg>) {
        let mut status = self.status.clone();
        rt.spawn(async move {
            let mut last = None;
            let mut mic_glitches = (0u64, 0u64);
            loop {
                let (desired, connected, tip) = {
                    let s = status.borrow_and_update();
                    let connected = s.state == ConnState::Connected;
                    let desired = Desired {
                        connected,
                        pc_audio: s.pc_audio_enabled,
                        mic_active: s.mic.active,
                    };
                    let tip = match (&s.peer_name, connected) {
                        (Some(p), true) => format!("AudioBridge — Подключено: {p}"),
                        _ => "AudioBridge — Ожидание телефона".to_owned(),
                    };
                    let glitches = (s.mic.underruns, s.mic.lost_packets);
                    if glitches != mic_glitches {
                        if glitches > mic_glitches {
                            tracing::info!(
                                "mic glitch: underruns={} lost={} buffer={:.0}ms rtt={:?}ms path={:?}",
                                glitches.0,
                                glitches.1,
                                s.mic.buffer_ms,
                                s.rtt_ms,
                                s.path
                            );
                        }
                        mic_glitches = glitches;
                    }
                    (desired, connected, tip)
                };
                if last != Some(desired) {
                    tracing::info!("session: {desired:?}");
                    let _ = audio.send(AudioMsg::Desired(desired));
                    last = Some(desired);
                }
                self.update_tray(connected, tip);
                self.repaint();
                if status.changed().await.is_err() {
                    break;
                }
            }
        });
        let mut pairing = self.pairing.clone();
        let path = self.data_dir.join("pairing.txt");
        rt.spawn(async move {
            loop {
                let uri = pairing.borrow_and_update().as_ref().map(PairingInfo::to_uri);
                if let Some(uri) = uri {
                    if let Err(e) = std::fs::write(&path, &uri) {
                        tracing::warn!("cannot write {}: {e}", path.display());
                    }
                }
                self.repaint();
                if pairing.changed().await.is_err() {
                    break;
                }
            }
        });
    }
}
