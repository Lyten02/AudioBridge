//! Process-wide state shared by the UI, tray, audio hooks and status watchers.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};

use audiobridge_core::pairing::PairingInfo;
use audiobridge_core::proto::MAX_LEVEL;
use audiobridge_core::session::{ConnState, PcMedia, PcRequest, PhoneRequest, Server, Status, Volume};
use tokio::task::JoinHandle;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    IsIconic, PostMessageW, SetForegroundWindow, ShowWindow, SW_RESTORE, SW_SHOW,
};

use crate::audio::{AudioMsg, Desired, DeviceSummary};
use crate::cable_install::InstallState;
use crate::settings::Settings;
use crate::{autostart, tray};

pub enum MainCmd {
    Show,
    Exit,
    SetEnabled(bool),
    DefaultDevice { generation: u64, id: String, what: &'static str },
    Remote { generation: u64, request: PcRequest },
}

pub struct Shared {
    pub data_dir: PathBuf,
    pub pc_name: String,
    server: Mutex<Option<Server>>,
    status: Mutex<Status>,
    pairing: Mutex<Option<PairingInfo>>,
    settings: Mutex<Settings>,
    devices: Mutex<DeviceSummary>,
    ui_ctx: Mutex<Option<eframe::egui::Context>>,
    window_hwnd: AtomicIsize,
    window_state: Mutex<WindowState>,
    tray_hwnd: AtomicIsize,
    tray_connected: AtomicBool,
    tray_tip: Mutex<String>,
    main_tx: Sender<MainCmd>,
    exiting: AtomicBool,
    service_generation: AtomicU64,
    audio_tx: Mutex<Option<Sender<AudioMsg>>>,
    media_tx: Mutex<Option<crate::media::Sender>>,
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
    background: bool,
    settings: Settings,
    main_tx: Sender<MainCmd>,
) -> &'static Shared {
    let status = stopped_status(&settings);
    let pairing = std::fs::read_to_string(data_dir.join("pairing.txt"))
        .ok()
        .and_then(|uri| PairingInfo::from_uri(&uri).ok());
    let tip = if settings.service_enabled { "AudioBridge — Запуск…" } else { "AudioBridge — Выключено" };
    let state = Shared {
        data_dir,
        pc_name,
        server: Mutex::new(None),
        status: Mutex::new(status),
        pairing: Mutex::new(pairing),
        settings: Mutex::new(settings),
        devices: Mutex::new(DeviceSummary::default()),
        ui_ctx: Mutex::new(None),
        window_hwnd: AtomicIsize::new(0),
        window_state: Mutex::new(if background { WindowState::Closed } else { WindowState::Opening }),
        tray_hwnd: AtomicIsize::new(0),
        tray_connected: AtomicBool::new(false),
        tray_tip: Mutex::new(tip.to_owned()),
        main_tx,
        exiting: AtomicBool::new(false),
        service_generation: AtomicU64::new(0),
        audio_tx: Mutex::new(None),
        media_tx: Mutex::new(None),
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


/// Coalesce requests before native window creation; never queue another window behind an open one.
#[derive(Debug, PartialEq, Eq)]
enum WindowState {
    Closed,
    Opening,
    Open,
}

impl WindowState {
    fn request_open(&mut self) -> bool {
        if *self != Self::Closed {
            return false;
        }
        *self = Self::Opening;
        true
    }
}

fn stopped_status(settings: &Settings) -> Status {
    Status {
        state: ConnState::Stopped,
        peer_name: None,
        path: None,
        rtt_ms: None,
        pc_audio_enabled: settings.pc_audio_enabled,
        mic_enabled: settings.mic_enabled,
        mic_demanded: false,
        pc_audio: Default::default(),
        mic: Default::default(),
        pc: Default::default(),
        phone: None,
        media: Default::default(),
        last_error: None,
    }
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

    pub fn install_server(&self, server: Server) {
        *lock(&self.status) = server.status().borrow().clone();
        *lock(&self.server) = Some(server);
    }

    pub fn status(&self) -> Status {
        lock(&self.status).clone()
    }

    pub fn pairing_uri(&self) -> Option<String> {
        lock(&self.pairing).as_ref().map(PairingInfo::to_uri)
    }

    pub fn set_enabled(&self, enabled: bool) {
        let _ = self.main_tx.send(MainCmd::SetEnabled(enabled));
    }

    pub fn persist_enabled(&self, enabled: bool) {
        self.update_settings(|s| s.service_enabled = enabled);
        self.refresh_tray();
    }

    pub fn remember_mic_default(&self, enabled: bool) {
        self.update_settings(|s| s.resume_mic_default = enabled);
    }

    pub fn service_stopped(&self, error: Option<String>) {
        let mut status = stopped_status(&self.settings());
        status.last_error = error;
        *lock(&self.status) = status;
        *lock(&self.audio_tx) = None;
        *lock(&self.media_tx) = None;
        self.update_tray(false, "AudioBridge — Выключено".into());
        self.repaint();
    }

    pub fn set_generation(&self, generation: u64) {
        self.service_generation.store(generation, Ordering::SeqCst);
    }

    fn refresh_tray(&self) {
        let hwnd = self.tray_hwnd.load(Ordering::SeqCst);
        if hwnd != 0 {
            // SAFETY: posting a refresh to our own tray window.
            unsafe {
                let _ = PostMessageW(Some(HWND(hwnd as *mut _)), tray::WM_TRAY_REFRESH, WPARAM(0), LPARAM(0));
            }
        }
    }

    pub fn settings(&self) -> Settings {
        lock(&self.settings).clone()
    }

    fn update_settings(&self, f: impl FnOnce(&mut Settings)) {
        {
            let mut s = lock(&self.settings);
            f(&mut s);
            // Serialize mutation and tmp+rename with every other settings writer.
            s.save(&self.data_dir);
        }
        self.repaint();
    }

    /// "Звук ПК": streams PC audio; switching it on also undoes a VB-CABLE takeover of the
    /// default playback device (loopback pauses while CABLE is the default).
    pub fn set_pc_audio(&'static self, on: bool) {
        self.with_server(|s| s.set_pc_audio_enabled(on));
        self.update_settings(|s| s.pc_audio_enabled = on);
        let d = self.devices();
        if on && self.settings().service_enabled && d.default_is_cable {
            let remembered = self.settings().last_default_render;
            match d.render_restore_candidate(remembered.as_deref()) {
                Some(target) => self.restore_default_render(target.id.clone()),
                None => tracing::warn!("default playback device is VB-CABLE and no other playback device is active"),
            }
        }
    }

    /// One-button "Микрофон": on = the PC accepts the phone mic, the phone mic is switched on
    /// remotely and the virtual mic becomes the default recording device; off = the PC stops
    /// accepting it and the previous recording device is restored. The phone mic itself stays
    /// on (other PCs may use it; the phone stops capturing when no PC wants it).
    pub fn set_mic(&'static self, on: bool) {
        self.with_server(|s| s.set_mic_enabled(on));
        self.update_settings(|s| s.mic_enabled = on);
        if on {
            let phone = self.status().phone;
            if phone.is_some_and(|p| !p.mic) {
                self.set_phone_mic(true);
            }
        }
        if self.settings().service_enabled {
            self.set_mic_default(on);
        }
    }

    /// Makes the virtual mic ("CABLE Output") the Windows default recording device, or restores
    /// the previous recording device if it is the default.
    pub fn set_mic_default(&'static self, on: bool) {
        let d = self.devices();
        if on {
            match &d.cable_capture_id {
                Some(_) if d.default_capture_is_cable => {}
                Some(id) => self.switch_default_device(id.clone(), "recording"),
                None => tracing::warn!("cannot make the virtual mic the default recording device: no VB-CABLE"),
            }
        } else if d.default_capture_is_cable {
            let remembered = self.settings().last_default_capture;
            match d.capture_restore_candidate(remembered.as_deref()) {
                Some(target) => self.switch_default_device(target.id.clone(), "recording"),
                None => tracing::warn!("the virtual mic is the default recording device and there is nothing to restore"),
            }
        }
    }

    /// Default playback device volume, percent.
    pub fn set_volume(&self, level: u8) {
        self.send_audio(AudioMsg::SetVolume(level.min(MAX_LEVEL)));
    }

    /// Mutes/unmutes the default playback device.
    pub fn set_mute(&self, muted: bool) {
        self.send_audio(AudioMsg::SetMute(muted));
    }

    /// Default playback device volume as read by the audio engine (reported to the phone).
    pub fn set_pc_volume_state(&self, v: Option<Volume>) {
        self.with_server(|s| s.set_volume(v));
    }

    pub fn set_media_state(&self, media: PcMedia) {
        self.with_server(|s| s.set_media(media));
    }

    pub fn set_media_sender(&self, sender: crate::media::Sender) {
        *lock(&self.media_tx) = Some(sender);
    }

    /// Switches the connected phone's mic.
    pub fn set_phone_mic(&self, on: bool) {
        self.request_phone(PhoneRequest::Mic(on));
    }

    /// Sets the connected phone's media volume, percent.
    pub fn set_phone_volume(&self, level: u8) {
        self.request_phone(PhoneRequest::Volume(level.min(MAX_LEVEL)));
    }

    fn request_phone(&self, req: PhoneRequest) {
        let mut sent = false;
        self.with_server(|s| sent = s.request_phone(req));
        if !sent {
            tracing::info!("phone request {req:?} dropped: no phone connected");
        }
    }

    /// Applies a remote-control request from the phone; results flow back through `Status`.
    pub fn apply_request(&'static self, req: PcRequest) {
        match req {
            PcRequest::Media(_) => {}
            PcRequest::Volume(_) => tracing::debug!("phone request: {req:?}"),
            _ => tracing::info!("phone request: {req:?}"),
        }
        match req {
            PcRequest::Audio(on) => self.set_pc_audio(on),
            PcRequest::Mic(on) => self.set_mic(on),
            PcRequest::MicDefault(on) => self.set_mic_default(on),
            PcRequest::Volume(level) => self.set_volume(level),
            PcRequest::Mute(muted) => self.set_mute(muted),
            PcRequest::Media(command) => {
                if let Some(sender) = lock(&self.media_tx).as_ref() {
                    sender.send(command);
                }
            }
        }
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
        self.with_server(|s| s.set_mic_default(d.default_capture_is_cable));
        // Remember the user's own devices so a VB-CABLE default can be undone later.
        let render = d.default_render_id.as_ref().filter(|_| !d.default_is_cable);
        let capture = d
            .default_capture_id
            .as_ref()
            .filter(|id| d.capture_endpoints.iter().any(|c| c.id == **id && !c.is_cable));
        let (render, capture) = {
            let s = lock(&self.settings);
            (
                render.filter(|id| s.last_default_render.as_ref() != Some(*id)).cloned(),
                capture.filter(|id| s.last_default_capture.as_ref() != Some(*id)).cloned(),
            )
        };
        if render.is_some() || capture.is_some() {
            self.update_settings(|s| {
                if let Some(id) = render {
                    s.last_default_render = Some(id);
                }
                if let Some(id) = capture {
                    s.last_default_capture = Some(id);
                }
            });
        }
        self.repaint();
    }

    pub fn set_audio_sender(&self, tx: Sender<AudioMsg>) {
        *lock(&self.audio_tx) = Some(tx);
    }

    fn send_audio(&self, msg: AudioMsg) {
        if let Some(tx) = lock(&self.audio_tx).as_ref() {
            let _ = tx.send(msg);
        }
    }

    /// Re-reads the device list (e.g. after the VB-CABLE installer exits).
    pub fn rescan_devices(&self) {
        self.send_audio(AudioMsg::DevicesChanged);
    }

    /// Makes `id` the default playback device again (undoes a VB-CABLE takeover).
    pub fn restore_default_render(&'static self, id: String) {
        self.switch_default_device(id, "playback");
    }

    /// Makes `id` the default device on the service supervisor, serialized with service shutdown.
    fn switch_default_device(&'static self, id: String, what: &'static str) {
        let generation = self.service_generation.load(Ordering::SeqCst);
        let _ = self.main_tx.send(MainCmd::DefaultDevice { generation, id, what });
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
        *lock(&self.window_state) = WindowState::Open;
    }

    pub fn unregister_window(&self) {
        self.window_hwnd.store(0, Ordering::SeqCst);
        *lock(&self.ui_ctx) = None;
        *lock(&self.window_state) = WindowState::Closed;
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
        } else if !self.is_exiting() && lock(&self.window_state).request_open() {
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
    pub fn spawn_watchers(
        &'static self,
        rt: &tokio::runtime::Runtime,
        audio: Sender<AudioMsg>,
        generation: u64,
    ) -> Vec<JoinHandle<()>> {
        let server = lock(&self.server);
        let server = server.as_ref().expect("service installed before watchers");
        let mut status = server.status();
        let mut pairing = server.pairing();
        let mut requests = server.take_requests().expect("one request reader per service");
        let status_task = rt.spawn(async move {
            let mut last = None;
            let mut mic_glitches = (0u64, 0u64);
            loop {
                let (desired, connected, tip) = {
                    let s = status.borrow_and_update();
                    *lock(&self.status) = s.clone();
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
        let path = self.data_dir.join("pairing.txt");
        let pairing_task = rt.spawn(async move {
            loop {
                // Keep the published QR stable across off/on and application restarts.
                let info = pairing.borrow_and_update().clone();
                {
                    let mut cached = lock(&self.pairing);
                    if let (None, Some(info)) = (&*cached, info) {
                        let uri = info.to_uri();
                        let tmp = path.with_extension("txt.tmp");
                        if let Err(e) = std::fs::write(&tmp, &uri).and_then(|()| std::fs::rename(&tmp, &path)) {
                            tracing::warn!("cannot write {}: {e}", path.display());
                        }
                        *cached = Some(info);
                    }
                }
                self.repaint();
                if pairing.changed().await.is_err() {
                    break;
                }
            }
        });
        let requests_task = rt.spawn(async move {
            while let Some(request) = requests.recv().await {
                if self.main_tx.send(MainCmd::Remote { generation, request }).is_err() {
                    break;
                }
            }
        });
        vec![status_task, pairing_task, requests_task]
    }
}

#[cfg(test)]
mod tests {
    use super::WindowState;

    #[test]
    fn repeated_tray_events_open_only_one_window() {
        let mut state = WindowState::Closed;
        assert!(state.request_open());
        // WM_LBUTTONUP, WM_LBUTTONDBLCLK and the second UP may precede registration.
        assert!(!state.request_open());
        assert!(!state.request_open());
        state = WindowState::Open;
        assert!(!state.request_open());
        state = WindowState::Closed;
        // Only a new explicit request can open it after X.
        assert!(state.request_open());
    }

    #[test]
    fn foreground_start_coalesces_show_during_creation() {
        let mut state = WindowState::Opening;
        assert!(!state.request_open());
    }
}
