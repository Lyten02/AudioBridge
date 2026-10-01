//! Process-wide engine: a small tokio runtime whose single manager task owns the core `Hub` (one iroh
//! endpoint, one connection per paired PC). JNI calls only enqueue commands, so they never block the calling
//! (often main) thread.

use std::future::pending;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use audiobridge_core::pairing::PairingInfo;
use audiobridge_core::session::{Hub, HubConfig, HubStatus};
use jni::objects::GlobalRef;
use jni::JavaVM;
use tokio::runtime::Runtime;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::{watch, Notify};

use super::controller::{self, Event};
use super::listener::Listener;
use crate::policy::{mic_should_capture, Backoff};
use crate::status::StatusView;

pub enum Cmd {
    /// The full set of paired PCs (already deduplicated by peer id); empty = disconnect all.
    SetPeers(Vec<PairingInfo>),
    MicAllowed(bool),
    NetworkChanged,
}

pub struct Engine {
    // Kept alive for the process lifetime; the manager task runs on it.
    _rt: Runtime,
    cmds: UnboundedSender<Cmd>,
    latest: Arc<Mutex<StatusView>>,
    listener: Listener,
}

static ENGINE: OnceLock<Engine> = OnceLock::new();
static INIT_LOCK: Mutex<()> = Mutex::new(());

pub fn get() -> Option<&'static Engine> {
    ENGINE.get()
}

/// Idempotent: later calls (e.g. Activity and Service both calling init) are no-ops.
pub fn init(vm: JavaVM, files_dir: &str, device_name: &str) -> Result<(), String> {
    let _guard = INIT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if ENGINE.get().is_some() {
        return Ok(());
    }
    let data_dir = PathBuf::from(files_dir).join("audiobridge");
    std::fs::create_dir_all(&data_dir).map_err(|e| format!("create {}: {e}", data_dir.display()))?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("ab-rt")
        .enable_all()
        .build()
        .map_err(|e| format!("tokio runtime: {e}"))?;
    let latest = Arc::new(Mutex::new(StatusView::idle()));
    let listener = Listener::spawn(vm, StatusView::idle()).map_err(|e| format!("listener thread: {e}"))?;
    let kick = Arc::new(Notify::new());
    let mic_capturing = Arc::new(AtomicBool::new(false));
    let audio = {
        let kick = kick.clone();
        controller::spawn(mic_capturing.clone(), move || kick.notify_one()).map_err(|e| format!("audio thread: {e}"))?
    };
    let (cmds, rx) = unbounded_channel();
    let manager = Manager {
        data_dir,
        device_name: device_name.to_owned(),
        cmds: rx,
        kick,
        mic_capturing,
        audio,
        latest: latest.clone(),
        listener: listener.clone(),
        published: StatusView::idle(),
        peers: Vec::new(),
        hub: None,
        status_rx: None,
        status_closed: false,
        retry_at: None,
        backoff: Backoff::default(),
        start_error: None,
        mic_allowed: false,
        audio_demand: None,
        glitches: std::collections::HashMap::new(),
    };
    rt.spawn(manager.run());
    log::info!("engine started (device {device_name:?})");
    let _ = ENGINE.set(Engine { _rt: rt, cmds, latest, listener });
    Ok(())
}

impl Engine {
    pub fn send(&self, cmd: Cmd) {
        let _ = self.cmds.send(cmd);
    }

    pub fn status_json(&self) -> String {
        self.latest.lock().unwrap_or_else(|e| e.into_inner()).to_json()
    }

    pub fn set_listener(&self, listener: Option<GlobalRef>) {
        self.listener.set(listener);
    }
}

/// A configured PC: `(peer id, pairing info)`.
struct Peer {
    id: String,
    info: PairingInfo,
}

struct Manager {
    data_dir: PathBuf,
    device_name: String,
    cmds: UnboundedReceiver<Cmd>,
    /// Woken by the audio thread when mic capture starts/stops (part of the status view).
    kick: Arc<Notify>,
    mic_capturing: Arc<AtomicBool>,
    audio: std::sync::mpsc::Sender<Event>,
    latest: Arc<Mutex<StatusView>>,
    listener: Listener,
    published: StatusView,
    peers: Vec<Peer>,
    hub: Option<Hub>,
    status_rx: Option<watch::Receiver<HubStatus>>,
    status_closed: bool,
    /// Next `Hub::start` attempt after a failure.
    retry_at: Option<Instant>,
    backoff: Backoff,
    start_error: Option<String>,
    mic_allowed: bool,
    /// Last `(pc_active, mic_wanted)` sent to the audio thread.
    audio_demand: Option<(bool, bool)>,
    /// Last seen `(underruns, lost)` of each PC's audio stream, to log glitches as they happen.
    glitches: std::collections::HashMap<String, (u64, u64)>,
}

impl Manager {
    async fn run(mut self) {
        loop {
            self.refresh();
            tokio::select! {
                cmd = self.cmds.recv() => match cmd {
                    Some(cmd) => self.handle(cmd).await,
                    None => break,
                },
                open = status_changed(&mut self.status_rx, self.status_closed) => {
                    if !open {
                        self.status_closed = true;
                    }
                }
                () = self.kick.notified() => {}
                () = sleep_until(self.retry_at) => {
                    self.retry_at = None;
                    self.try_start().await;
                }
            }
        }
        self.stop_hub().await;
    }

    async fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::SetPeers(infos) => {
                let names: Vec<&str> = infos.iter().map(PairingInfo::pc_name).collect();
                log::info!("paired PCs: {names:?}");
                self.peers = infos.into_iter().map(|info| Peer { id: info.peer_id(), info }).collect();
                if self.peers.is_empty() {
                    self.stop_hub().await;
                } else if let Some(hub) = &self.hub {
                    hub.set_peers(self.peer_infos());
                } else if self.retry_at.is_none() {
                    self.backoff.reset();
                    self.try_start().await;
                }
            }
            Cmd::MicAllowed(allowed) => {
                self.mic_allowed = allowed;
                if let Some(hub) = &self.hub {
                    hub.set_mic_enabled(allowed);
                }
            }
            Cmd::NetworkChanged => {
                if let Some(hub) = &self.hub {
                    hub.network_changed();
                } else if !self.peers.is_empty() {
                    self.backoff.reset();
                    self.retry_at = Some(Instant::now());
                }
            }
        }
    }

    fn peer_infos(&self) -> Vec<PairingInfo> {
        self.peers.iter().map(|p| p.info.clone()).collect()
    }

    async fn try_start(&mut self) {
        if self.peers.is_empty() || self.hub.is_some() {
            return;
        }
        let cfg = HubConfig { data_dir: self.data_dir.clone(), device_name: self.device_name.clone() };
        match Hub::start(cfg).await {
            Ok(hub) => {
                hub.set_mic_enabled(self.mic_allowed);
                hub.set_peers(self.peer_infos());
                let _ = self.audio.send(Event::Attach { playout: hub.pc_audio_playout(), capture: hub.mic_capture() });
                self.audio_demand = None;
                self.status_rx = Some(hub.status());
                self.status_closed = false;
                self.hub = Some(hub);
                self.start_error = None;
                self.retry_at = None;
                log::info!("hub started");
            }
            Err(e) => {
                let delay = self.backoff.fail().max(Duration::from_secs(1));
                log::warn!("hub start failed: {e:#}; retrying in {delay:?}");
                self.start_error = Some(format!("{e:#}"));
                self.retry_at = Some(Instant::now() + delay);
            }
        }
    }

    async fn stop_hub(&mut self) {
        let _ = self.audio.send(Event::Detach);
        self.audio_demand = None;
        self.retry_at = None;
        self.start_error = None;
        self.status_rx = None;
        if let Some(hub) = self.hub.take() {
            hub.shutdown().await;
            log::info!("hub stopped");
        }
    }

    /// Recomputes the view and the audio demand from the newest hub status; publishes changes.
    fn refresh(&mut self) {
        let capturing = self.mic_capturing.load(Ordering::SeqCst);
        let configured = self.peers.iter().map(|p| (p.id.as_str(), p.info.pc_name()));
        let view = match (&self.hub, &mut self.status_rx) {
            (Some(_), Some(rx)) => {
                let status = rx.borrow_and_update();
                let demand = (status.pc_audio_active, mic_should_capture(&status, self.mic_allowed));
                if self.audio_demand != Some(demand) {
                    self.audio_demand = Some(demand);
                    let _ = self.audio.send(Event::Update { pc_active: demand.0, mic_wanted: demand.1 });
                }
                for peer in &status.peers {
                    let s = &peer.status.pc_audio;
                    let prev = self.glitches.insert(peer.id.clone(), (s.underruns, s.lost_packets));
                    if prev.is_some_and(|p| p != (s.underruns, s.lost_packets)) {
                        log::info!(
                            "pc audio glitch: {} underruns={} lost={} buffer={:.0}ms rtt={:?}ms path={:?}",
                            peer.status.peer_name.as_deref().unwrap_or(&peer.id),
                            s.underruns,
                            s.lost_packets,
                            s.buffer_ms,
                            peer.status.rtt_ms,
                            peer.status.path
                        );
                    }
                }
                StatusView::from_hub(&status, configured, capturing)
            }
            _ if !self.peers.is_empty() => {
                StatusView::starting(configured, self.mic_allowed, self.start_error.as_deref())
            }
            _ => StatusView::idle(),
        };
        if view != self.published {
            *self.latest.lock().unwrap_or_else(|e| e.into_inner()) = view.clone();
            self.listener.update(view.clone());
            self.published = view;
        }
    }
}

/// Resolves when the status changes (`true`) or its sender is gone (`false`); never resolves without one.
async fn status_changed(rx: &mut Option<watch::Receiver<HubStatus>>, closed: bool) -> bool {
    match rx {
        Some(rx) if !closed => rx.changed().await.is_ok(),
        _ => pending().await,
    }
}

async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at.into()).await,
        None => pending().await,
    }
}
