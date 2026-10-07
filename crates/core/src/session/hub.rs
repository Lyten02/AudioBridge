//! Phone side: one iroh endpoint, one connection task per paired PC. PC audio of all PCs is
//! mixed (per-PC jitter buffer and drift compensation); the mic fans out to every PC that
//! demands it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use iroh::endpoint::{Connection, RecvStream, SendStream, VarInt};
use iroh::Endpoint;
use parking_lot::Mutex;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use tokio::task::JoinHandle;

use super::{
    clear_connection_status, net, recv_media, spawn_control_reader, stats_loop, ConnState, Dir, HubConfig,
    HubStatus, Media, NetOptions, PeerStatus, Status, StatusCell,
};
use crate::audio::tx::TxStats;
use crate::audio::{CaptureHandle, IncomingStream, OutgoingStream, PlayoutHandle, RxSlot};
use crate::pairing::PairingInfo;
use crate::proto::{
    ControlMsg, DatagramHeader, PacketKind, PcControls, PcRequest, PhoneControls, PhoneRequest, StreamId, ALPN,
    DATAGRAM_HEADER_LEN, PROTOCOL_VERSION,
};

/// Maximum number of simultaneously paired PCs.
pub const MAX_PEERS: usize = 8;
/// Audio slots: a removed PC keeps its slot until its task has fully stopped, so up to
/// `MAX_PEERS` more can be in flight while new PCs are added.
const SLOTS: usize = 2 * MAX_PEERS;

const MIC_BITRATE: i32 = 64_000;
const MIC_COMPLEXITY: i32 = 5;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
const BACKOFF_MIN: Duration = Duration::from_secs(1);
/// Offline PCs (e.g. a laptop that is switched off) are retried at most this often;
/// `network_changed()` still retries immediately.
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// A session shorter than this does not reset the backoff (avoids tight reconnect loops).
const STABLE_SESSION: Duration = Duration::from_secs(5);
/// Keep-awake interval while a PC's audio is arriving and we are not sending it our mic.
const PACE_INTERVAL: Duration = Duration::from_millis(20);

/// Hub-wide state shared by all peer tasks.
struct Shared {
    endpoint: Endpoint,
    device_name: String,
    status: Arc<watch::Sender<HubStatus>>,
    /// Phone-side controls, reported to every PC.
    phone: watch::Sender<PhoneControls>,
    /// Requests from PCs, for the app to apply.
    requests: mpsc::UnboundedSender<PhoneRequest>,
    /// Incremented by `network_changed()`; every peer task watches it.
    net_changed: watch::Sender<u64>,
    pc_audio: IncomingStream,
    mic: OutgoingStream,
    slot_busy: Mutex<[bool; SLOTS]>,
    cancel: CancellationToken,
    runtime: tokio::runtime::Handle,
}

/// PC-side state as last reported by the PC.
#[derive(Clone, Copy, Debug)]
struct Remote {
    pc: PcControls,
    mic_demand: bool,
}

/// One paired PC.
struct Peer {
    hub: Arc<Shared>,
    slot: usize,
    info: Mutex<PairingInfo>,
    status: StatusCell,
    remote: Mutex<Option<Remote>>,
    /// Requests for this PC while connected (written by the session's control loop).
    to_pc: Mutex<Option<mpsc::UnboundedSender<PcRequest>>>,
    mic_stats: Arc<TxStats>,
    /// Notified when this PC's audio stream becomes active (starts uplink pacing).
    audio_started: tokio::sync::Notify,
    /// Keep-awake packets sent.
    pace_sent: std::sync::atomic::AtomicU64,
    cancel: CancellationToken,
}

struct PeerEntry {
    id: String,
    peer: Arc<Peer>,
    task: JoinHandle<()>,
}

/// Phone side: ONE iroh endpoint shared by all peers; one connection task per paired PC.
/// Each peer dials its PC and reconnects forever with backoff (1 s → 30 s), immediately on
/// [`Hub::network_changed`].
pub struct Hub {
    shared: Arc<Shared>,
    peers: Mutex<Vec<PeerEntry>>,
    requests: Mutex<Option<mpsc::UnboundedReceiver<PhoneRequest>>>,
}

impl Hub {
    pub async fn start(cfg: HubConfig) -> Result<Hub> {
        Self::start_with_options(cfg, NetOptions::default()).await
    }

    pub async fn start_with_options(cfg: HubConfig, opts: NetOptions) -> Result<Hub> {
        let key = net::client_identity(&cfg.data_dir)?;
        let endpoint = net::bind_client(key, &opts).await?;
        let phone = PhoneControls { mic: true, mic_ready: true, volume: None };
        let status = HubStatus {
            mic_enabled: phone.mic_allowed(),
            ..HubStatus::default()
        };
        let (requests_tx, requests) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            endpoint,
            device_name: cfg.device_name,
            status: Arc::new(watch::channel(status).0),
            phone: watch::channel(phone).0,
            requests: requests_tx,
            net_changed: watch::channel(0).0,
            pc_audio: IncomingStream::new_mixed(StreamId::PcAudio, SLOTS)?,
            mic: OutgoingStream::new(StreamId::Mic, MIC_BITRATE, MIC_COMPLEXITY)?,
            slot_busy: Mutex::new([false; SLOTS]),
            cancel: CancellationToken::new(),
            runtime: tokio::runtime::Handle::current(),
        });
        debug_assert_eq!(shared.pc_audio.len(), SLOTS);
        Ok(Hub {
            shared,
            peers: Mutex::new(Vec::new()),
            requests: Mutex::new(Some(requests)),
        })
    }

    /// Sets the full list of paired PCs (diffed by `peer_id`): new PCs are added, missing ones
    /// dropped, existing connections are left untouched (updated pairing data is used on the
    /// next reconnect). At most [`MAX_PEERS`]; extra entries are ignored.
    pub fn set_peers(&self, peers: Vec<PairingInfo>) {
        let mut wanted: Vec<PairingInfo> = Vec::with_capacity(peers.len());
        for p in peers {
            if wanted.iter().any(|w| w.peer_id() == p.peer_id()) {
                continue;
            }
            if wanted.len() == MAX_PEERS {
                tracing::warn!("more than {MAX_PEERS} PCs paired; ignoring {}", p.pc_name());
                continue;
            }
            wanted.push(p);
        }
        let mut current = self.peers.lock();

        // drop removed peers (their tasks release their audio slot when they finish)
        current.retain(|e| {
            let keep = wanted.iter().any(|w| w.peer_id() == e.id);
            if !keep {
                tracing::info!("removing PC {}", e.id);
                e.peer.cancel.cancel();
            }
            keep
        });

        let mut next = Vec::with_capacity(wanted.len());
        for info in wanted {
            let id = info.peer_id();
            if let Some(pos) = current.iter().position(|e| e.id == id) {
                let entry = current.swap_remove(pos);
                {
                    let mut cur = entry.peer.info.lock();
                    if *cur != info {
                        *cur = info;
                    }
                }
                next.push(entry);
                continue;
            }
            let Some(slot) = self.claim_slot() else {
                tracing::error!("no free audio slot for PC {id}; skipping");
                continue;
            };
            tracing::info!("adding PC {} ({id})", info.pc_name());
            let span = tracing::info_span!("pc", name = %info.pc_name());
            let peer = Arc::new(Peer {
                hub: self.shared.clone(),
                slot,
                status: StatusCell::Peer {
                    hub: self.shared.status.clone(),
                    id: id.as_str().into(),
                },
                info: Mutex::new(info),
                remote: Mutex::new(None),
                to_pc: Mutex::new(None),
                mic_stats: Arc::new(TxStats::default()),
                audio_started: tokio::sync::Notify::new(),
                pace_sent: std::sync::atomic::AtomicU64::new(0),
                cancel: self.shared.cancel.child_token(),
            });
            next.push(PeerEntry {
                id,
                task: self.shared.runtime.spawn(peer.clone().run().instrument(span)),
                peer,
            });
        }
        debug_assert!(current.is_empty());
        *current = next;

        // status entries follow the new list order; existing entries keep their state
        self.shared.status.send_modify(|h| {
            let mut old = std::mem::take(&mut h.peers);
            h.peers = current
                .iter()
                .map(|e| match old.iter().position(|p| p.id == e.id) {
                    Some(i) => old.swap_remove(i),
                    None => PeerStatus {
                        id: e.id.clone(),
                        status: Status {
                            peer_name: Some(e.peer.info.lock().pc_name().to_owned()),
                            ..Status::new()
                        },
                    },
                })
                .collect();
            h.recompute();
        });
        for e in current.iter() {
            e.peer.refresh_toggles();
        }
    }

    fn claim_slot(&self) -> Option<usize> {
        let mut busy = self.shared.slot_busy.lock();
        let slot = busy.iter().position(|b| !b)?;
        busy[slot] = true;
        let rx = self.shared.pc_audio.slot(slot);
        rx.feeder.lock().reset();
        rx.shared.clear_stats();
        rx.shared.in_use.store(true, std::sync::atomic::Ordering::Release);
        Some(slot)
    }

    pub fn status(&self) -> watch::Receiver<HubStatus> {
        self.shared.status.subscribe()
    }

    /// Phone-side controls (apply to all PCs): the mic is sent only if
    /// [`PhoneControls::mic_allowed`]; every PC is told the new values.
    pub fn set_phone_controls(&self, phone: PhoneControls) {
        self.shared.phone.send_if_modified(|v| std::mem::replace(v, phone) != phone);
        let allowed = phone.mic_allowed();
        self.shared.status.send_if_modified(|h| {
            if h.mic_enabled == allowed {
                return false;
            }
            h.mic_enabled = allowed;
            true
        });
        for e in self.peers.lock().iter() {
            e.peer.refresh_toggles();
        }
    }

    /// Sends a remote-control request to the PC `peer_id`; `false` if it is not connected.
    pub fn request_pc(&self, peer_id: &str, req: PcRequest) -> bool {
        let peers = self.peers.lock();
        let Some(e) = peers.iter().find(|e| e.id == peer_id) else {
            return false;
        };
        let sent = e.peer.to_pc.lock().as_ref().is_some_and(|tx| tx.send(req).is_ok());
        sent
    }

    /// Remote-control requests from PCs. The hub does not apply them; the app does (and
    /// reports the result through [`Hub::set_phone_controls`]). `None` after the first call.
    pub fn take_requests(&self) -> Option<mpsc::UnboundedReceiver<PhoneRequest>> {
        self.requests.lock().take()
    }

    /// Call on connectivity changes: every PC retries immediately and iroh re-probes paths.
    pub fn network_changed(&self) {
        self.shared.net_changed.send_modify(|v| *v = v.wrapping_add(1));
        let ep = self.shared.endpoint.clone();
        self.shared.runtime.spawn(async move { ep.network_change().await });
    }

    /// Stereo mix of all PCs' audio (can be taken again after the previous one is dropped).
    pub fn pc_audio_playout(&self) -> PlayoutHandle {
        self.shared.pc_audio.take_handle("pc_audio_playout")
    }

    /// Mono mic capture, sent to every PC that demands it (can be taken again after the
    /// previous one is dropped).
    pub fn mic_capture(&self) -> CaptureHandle {
        self.shared.mic.take_handle("mic_capture")
    }

    pub async fn shutdown(self) {
        self.shared.cancel.cancel();
        let tasks: Vec<JoinHandle<()>> = {
            let mut peers = self.peers.lock();
            peers.drain(..).map(|e| e.task).collect()
        };
        for t in tasks {
            let _ = t.await;
        }
        self.shared.endpoint.close().await;
        self.shared.status.send_modify(|h| {
            for p in &mut h.peers {
                clear_connection_status(&mut p.status);
                p.status.state = ConnState::Stopped;
            }
            h.recompute();
        });
    }
}

impl Drop for Hub {
    /// Dropping without [`Hub::shutdown`] still stops all peers and closes the endpoint in the
    /// background.
    fn drop(&mut self) {
        if self.shared.cancel.is_cancelled() {
            return;
        }
        self.shared.cancel.cancel();
        let ep = self.shared.endpoint.clone();
        self.shared.runtime.spawn(async move { ep.close().await });
    }
}

impl Peer {
    fn rx(&self) -> &RxSlot {
        self.hub.pc_audio.slot(self.slot)
    }

    fn refresh_toggles(&self) {
        let allowed = self.hub.phone.borrow().mic_allowed();
        let remote = *self.remote.lock();
        let (pc, mic, demand, connected) = match remote {
            Some(r) => (r.pc, allowed && r.pc.mic, r.mic_demand, true),
            None => (PcControls::default(), allowed, false, false),
        };
        self.hub
            .mic
            .tx
            .shared
            .set_enabled(self.slot, connected && mic && demand);
        self.status.update(|s| {
            s.pc_audio_enabled = pc.audio;
            s.mic_enabled = mic;
            s.mic_demanded = demand;
            s.pc = pc;
        });
    }

    async fn run(self: Arc<Self>) {
        self.reconnect_loop().await;
        // release everything this peer used
        self.hub.mic.tx.shared.set_target(self.slot, None, &self.mic_stats);
        let rx = self.rx();
        rx.shared.in_use.store(false, std::sync::atomic::Ordering::Release);
        rx.feeder.lock().reset();
        self.hub.slot_busy.lock()[self.slot] = false;
    }

    async fn reconnect_loop(&self) {
        let mut net = self.hub.net_changed.subscribe();
        net.borrow_and_update();
        let mut backoff = BACKOFF_MIN;
        let mut first = true;
        loop {
            self.status.update(|s| {
                s.state = if first {
                    ConnState::Connecting
                } else {
                    ConnState::Reconnecting
                }
            });
            first = false;
            let attempt = tokio::select! {
                _ = self.cancel.cancelled() => return,
                _ = net.changed() => None,
                r = self.connect_once() => Some(r),
            };
            let mut retry_now = false;
            match attempt {
                None => {
                    tracing::info!("network changed during connect; retrying");
                    backoff = BACKOFF_MIN;
                    retry_now = true;
                }
                Some(Ok(session)) => {
                    let started = Instant::now();
                    let conn = session.conn.clone();
                    let reason = self.run_session(session).await;
                    if self.cancel.is_cancelled() {
                        conn.close(VarInt::from_u32(0), b"shutdown");
                        return;
                    }
                    tracing::info!("connection ended: {reason}");
                    self.status.update(|s| s.last_error = Some(reason));
                    if started.elapsed() >= STABLE_SESSION {
                        backoff = BACKOFF_MIN;
                        retry_now = true;
                    }
                }
                Some(Err(err)) => {
                    tracing::info!("connect failed: {err:#}");
                    self.status.update(|s| s.last_error = Some(format!("{err:#}")));
                }
            }
            if !retry_now {
                tokio::select! {
                    _ = self.cancel.cancelled() => return,
                    _ = net.changed() => backoff = BACKOFF_MIN,
                    _ = tokio::time::sleep(backoff) => backoff = (backoff * 2).min(BACKOFF_MAX),
                }
            }
        }
    }

    /// Dials the PC and performs the Hello/Welcome handshake.
    async fn connect_once(&self) -> Result<Session> {
        let info = self.info.lock().clone();
        let conn = tokio::time::timeout(
            CONNECT_TIMEOUT,
            self.hub.endpoint.connect(info.endpoint_addr(), ALPN),
        )
        .await
        .context("connect timed out")?
        .context("connect")?;
        let handshake = async {
            let (mut send, mut recv) = conn.open_bi().await?;
            let phone = *self.hub.phone.borrow();
            ControlMsg::Hello {
                version: PROTOCOL_VERSION,
                secret: *info.secret(),
                device_name: self.hub.device_name.clone(),
                phone,
            }
            .write_to(&mut send)
            .await?;
            match ControlMsg::read_from(&mut recv).await? {
                ControlMsg::Welcome {
                    pc_name,
                    pc,
                    mic_demanded,
                } => {
                    *self.remote.lock() = Some(Remote {
                        pc,
                        mic_demand: mic_demanded,
                    });
                    Ok((send, recv, pc_name, phone))
                }
                ControlMsg::Reject { reason } => bail!("rejected by PC: {reason}"),
                other => bail!("unexpected handshake reply {other:?}"),
            }
        };
        match tokio::time::timeout(HELLO_TIMEOUT, handshake).await {
            Ok(Ok((send, recv, pc_name, sent))) => Ok(Session { conn, send, recv, pc_name, sent }),
            Ok(Err(err)) => {
                conn.close(VarInt::from_u32(0), b"handshake failed");
                Err(err)
            }
            Err(_) => {
                conn.close(VarInt::from_u32(0), b"handshake timeout");
                bail!("handshake timed out")
            }
        }
    }

    /// Runs one connected session; returns the reason it ended.
    async fn run_session(&self, session: Session) -> String {
        let Session { conn, send, recv, pc_name, sent } = session;
        let conn = &conn;
        tracing::info!("connected to '{pc_name}'");
        self.rx().feeder.lock().reset();
        self.hub
            .mic
            .tx
            .shared
            .set_target(self.slot, Some(conn.clone()), &self.mic_stats);
        let (to_pc, to_pc_rx) = mpsc::unbounded_channel();
        *self.to_pc.lock() = Some(to_pc);
        self.status.update(|s| {
            clear_connection_status(s);
            s.state = ConnState::Connected;
            s.peer_name = Some(pc_name);
            s.last_error = None;
        });
        self.refresh_toggles();
        let media = Media {
            tx: &self.mic_stats,
            out_dir: Dir::Mic,
            rx: self.rx(),
            in_stream: StreamId::PcAudio,
            in_dir: Dir::PcAudio,
            activity: Some(&self.audio_started),
        };
        let reason = tokio::select! {
            _ = self.cancel.cancelled() => "removed".to_owned(),
            e = conn.closed() => format!("connection closed: {e}"),
            r = self.control_loop(send, recv, sent, to_pc_rx) => match r {
                Ok(()) => "control stream closed".to_owned(),
                Err(err) => format!("control stream: {err:#}"),
            },
            _ = recv_media(conn, &media, &self.status) => "datagram stream ended".to_owned(),
            _ = stats_loop(conn, &media, &self.status) => "stats ended".to_owned(),
            _ = self.pace_loop(conn) => "pacing ended".to_owned(),
        };
        *self.to_pc.lock() = None;
        self.hub.mic.tx.shared.set_target(self.slot, None, &self.mic_stats);
        *self.remote.lock() = None;
        conn.close(VarInt::from_u32(0), b"bye");
        self.status.update(clear_connection_status);
        self.refresh_toggles();
        reason
    }

    /// Keeps this phone's Wi-Fi out of power save while the PC's audio arrives: a tiny datagram
    /// every 20 ms, but only while audio is streaming and the PC is not already receiving our
    /// mic (which is uplink traffic itself). Sleeps (no timer) while there is no audio.
    async fn pace_loop(&self, conn: &Connection) {
        let mut pkt = [0u8; DATAGRAM_HEADER_LEN];
        DatagramHeader {
            kind: PacketKind::Pace,
            stream: StreamId::PcAudio,
            seq: 0,
        }
        .write(&mut pkt);
        let pkt = bytes::Bytes::copy_from_slice(&pkt);
        loop {
            if !self.rx().feeder.lock().is_streaming(Instant::now()) {
                self.audio_started.notified().await;
                continue;
            }
            let mut tick = tokio::time::interval(PACE_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                if !self.rx().feeder.lock().is_streaming(Instant::now()) {
                    break;
                }
                if self.mic_stats.active.load(std::sync::atomic::Ordering::Relaxed) {
                    continue;
                }
                if conn.send_datagram(pkt.clone()).is_ok() {
                    self.pace_sent
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
    }

    async fn control_loop(
        &self,
        mut send: SendStream,
        recv: RecvStream,
        mut sent: PhoneControls,
        mut to_pc: mpsc::UnboundedReceiver<PcRequest>,
    ) -> Result<()> {
        let mut incoming = spawn_control_reader(recv);
        let mut phone_rx = self.hub.phone.subscribe();
        // the controls may have changed between Hello and now
        phone_rx.mark_changed();
        loop {
            tokio::select! {
                msg = incoming.recv() => {
                    let Some(msg) = msg else { return Ok(()) };
                    match msg {
                        ControlMsg::SetPhone(req) => {
                            tracing::info!("PC request: {req:?}");
                            let _ = self.hub.requests.send(req);
                            continue;
                        }
                        msg => {
                            let mut remote = self.remote.lock();
                            let r = remote.as_mut().context("not connected")?;
                            match msg {
                                ControlMsg::PcState(pc) => r.pc = pc,
                                ControlMsg::MicDemand(on) => r.mic_demand = on,
                                other => tracing::debug!("unexpected control message {other:?}"),
                            }
                        }
                    }
                    self.refresh_toggles();
                }
                Some(req) = to_pc.recv() => {
                    ControlMsg::SetPc(req).write_to(&mut send).await?;
                }
                r = phone_rx.changed() => {
                    r?;
                    let now = *phone_rx.borrow_and_update();
                    if now != sent {
                        ControlMsg::PhoneState(now).write_to(&mut send).await?;
                        sent = now;
                    }
                }
            }
        }
    }
}

/// A handshaken connection to a PC.
struct Session {
    conn: Connection,
    send: SendStream,
    recv: RecvStream,
    pc_name: String,
    /// Phone controls carried by the `Hello`.
    sent: PhoneControls,
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::session::{Server, ServerConfig};

    /// Pushes 10 ms blocks in real time until `stop`; amplitude 0 makes digital silence.
    fn feed(mut cap: CaptureHandle, amp: f32, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let ch = cap.channels();
            let mut buf = vec![0f32; 480 * ch];
            let mut phase = 0f32;
            let start = Instant::now();
            let mut n = 0u64;
            while !stop.load(Ordering::Relaxed) {
                for f in buf.chunks_exact_mut(ch) {
                    f.fill(phase.sin() * amp);
                    phase += 0.05;
                }
                n += 1;
                if let Some(d) = (start + Duration::from_millis(10 * n)).checked_duration_since(Instant::now()) {
                    std::thread::sleep(d);
                }
                cap.push(&buf);
            }
        })
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn uplink_pacing_only_while_pc_audio_streams() {
        let (sdir, hdir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let server = Server::start_with_options(
            ServerConfig {
                data_dir: sdir.path().to_path_buf(),
                pc_name: "PC".into(),
            },
            NetOptions::local_only(),
        )
        .await
        .unwrap();
        let hub = Hub::start_with_options(
            HubConfig {
                data_dir: hdir.path().to_path_buf(),
                device_name: "Phone".into(),
            },
            NetOptions::local_only(),
        )
        .await
        .unwrap();
        hub.set_peers(vec![server.pairing().borrow().clone().unwrap()]);
        let mut hs = hub.status();
        tokio::time::timeout(
            Duration::from_secs(10),
            hs.wait_for(|h| h.peers.iter().any(|p| p.status.state == ConnState::Connected)),
        )
        .await
        .unwrap()
        .unwrap();
        let peer = hub.peers.lock()[0].peer.clone();
        let sent = || peer.pace_sent.load(Ordering::Relaxed);
        let received = || server.inner.mic.slot(0).shared.pace.load(Ordering::Relaxed);

        // no PC audio: no pacing
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(sent(), 0);

        // PC audio streaming, mic not demanded: ~50 packets per second
        let stop = Arc::new(AtomicBool::new(false));
        let pc = feed(server.pc_audio_capture(), 0.5, stop.clone());
        tokio::time::sleep(Duration::from_millis(1000)).await;
        let (s0, t0) = (sent(), Instant::now());
        tokio::time::sleep(Duration::from_millis(2000)).await;
        let rate = (sent() - s0) as f64 / t0.elapsed().as_secs_f64();
        println!("pacing rate {rate:.1}/s");
        assert!((40.0..=55.0).contains(&rate), "pacing rate {rate}");

        // the PC receives and ignores them: no mic activity or traffic
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(received() >= sent() - 5, "server saw {} of {}", received(), sent());
        let s = server.status().borrow().clone();
        assert!(!s.mic.active && s.mic.kbps == 0.0, "{:?}", s.mic);
        assert_eq!(server.inner.mic.slot(0).shared.bytes.load(Ordering::Relaxed), 0);

        // while our mic is sent to that PC, the mic itself is the uplink traffic: no pacing
        server.set_mic_demand(true);
        let mic_stop = Arc::new(AtomicBool::new(false));
        let mic = feed(hub.mic_capture(), 0.3, mic_stop.clone());
        tokio::time::sleep(Duration::from_millis(500)).await;
        let s1 = sent();
        tokio::time::sleep(Duration::from_millis(1000)).await;
        assert!(sent() - s1 <= 2, "paced while sending the mic: {}", sent() - s1);
        mic_stop.store(true, Ordering::Relaxed);
        mic.join().unwrap();

        // PC audio stops (silence marker): pacing stops right away
        stop.store(true, Ordering::Relaxed);
        pc.join().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let silent = feed(server.pc_audio_capture(), 0.0, stop.clone());
        tokio::time::sleep(Duration::from_millis(800)).await;
        let s2 = sent();
        tokio::time::sleep(Duration::from_millis(1000)).await;
        assert_eq!(sent(), s2, "pacing continued after the audio went silent");
        stop.store(true, Ordering::Relaxed);
        silent.join().unwrap();

        hub.shutdown().await;
        server.shutdown().await;
    }
}
