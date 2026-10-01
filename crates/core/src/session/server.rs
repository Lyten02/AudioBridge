//! PC side: accepts one active phone, streams PC audio, receives the phone mic.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Result};
use parking_lot::Mutex;
use iroh::endpoint::{Connection, RecvStream, SendStream, VarInt};
use iroh::{Endpoint, Watcher};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::{
    clear_connection_status, net, recv_media, secret_eq, stats_loop, ConnState, Dir, Media,
    NetOptions, ServerConfig, Status, StatusCell,
};
use crate::audio::tx::TxStats;
use crate::audio::{CaptureHandle, IncomingStream, OutgoingStream, PlayoutHandle};
use crate::pairing::PairingInfo;
use crate::proto::{ControlMsg, StreamId, PROTOCOL_VERSION};

const PC_AUDIO_BITRATE: i32 = 192_000;
const PC_AUDIO_COMPLEXITY: i32 = 9;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const ADDR_REFRESH: Duration = Duration::from_secs(30);
const CLOSE_REPLACED: u32 = 1;
const CLOSE_REJECTED: u32 = 2;
const CLOSE_SHUTDOWN: u32 = 3;
/// The server sends PC audio to exactly one phone.
const PHONE: usize = 0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Toggles {
    pc_audio: bool,
    mic: bool,
    mic_demand: bool,
}

struct ActiveConn {
    id: u64,
    conn: Connection,
    cancel: CancellationToken,
}

pub(super) struct Inner {
    endpoint: Endpoint,
    secret: [u8; 16],
    pc_name: String,
    status_tx: Arc<watch::Sender<Status>>,
    status: StatusCell,
    toggles: watch::Sender<Toggles>,
    /// Phone-side mic permission of the connected phone.
    phone_mic_allowed: AtomicBool,
    pc_audio: OutgoingStream,
    pc_audio_stats: Arc<TxStats>,
    pub(super) mic: IncomingStream,
    active: Mutex<Option<ActiveConn>>,
    next_id: AtomicU64,
    cancel: CancellationToken,
}

/// PC side. Persists an iroh secret key + pairing secret in `data_dir` (so the QR stays stable).
pub struct Server {
    pub(super) inner: Arc<Inner>,
    pairing: watch::Receiver<Option<PairingInfo>>,
    tasks: Vec<JoinHandle<()>>,
}

impl Server {
    pub async fn start(cfg: ServerConfig) -> Result<Server> {
        Self::start_with_options(cfg, NetOptions::default()).await
    }

    pub async fn start_with_options(cfg: ServerConfig, opts: NetOptions) -> Result<Server> {
        let (key, secret) = net::server_identity(&cfg.data_dir)?;
        let endpoint = net::bind_server(&cfg.data_dir, key, &opts).await?;
        let status_tx = Arc::new(watch::channel(Status::new()).0);
        let status = StatusCell::Single(status_tx.clone());
        let toggles = Toggles {
            pc_audio: true,
            mic: true,
            mic_demand: false,
        };
        let inner = Arc::new(Inner {
            endpoint,
            secret,
            pc_name: cfg.pc_name,
            status_tx,
            status,
            toggles: watch::channel(toggles).0,
            phone_mic_allowed: AtomicBool::new(true),
            pc_audio: OutgoingStream::new(StreamId::PcAudio, PC_AUDIO_BITRATE, PC_AUDIO_COMPLEXITY)?,
            pc_audio_stats: Arc::new(TxStats::default()),
            mic: IncomingStream::new(StreamId::Mic)?,
            active: Mutex::new(None),
            next_id: AtomicU64::new(1),
            cancel: CancellationToken::new(),
        });
        inner.refresh_status_toggles();
        inner.status.update(|s| s.state = ConnState::WaitingForPeer);

        let (pairing_tx, pairing) = watch::channel(Some(inner.make_pairing(&opts)));
        tracing::info!(
            "server {} listening on {:?}",
            inner.endpoint.id(),
            inner.endpoint.bound_sockets()
        );
        let tasks = vec![
            tokio::spawn(inner.clone().accept_loop()),
            tokio::spawn(inner.clone().pairing_loop(pairing_tx, opts)),
        ];
        Ok(Server {
            inner,
            pairing,
            tasks,
        })
    }

    /// Pairing info for the QR code; updates when addresses change.
    pub fn pairing(&self) -> watch::Receiver<Option<PairingInfo>> {
        self.pairing.clone()
    }

    pub fn status(&self) -> watch::Receiver<Status> {
        self.inner.status_tx.subscribe()
    }

    pub fn set_pc_audio_enabled(&self, on: bool) {
        self.inner.toggles.send_if_modified(|t| std::mem::replace(&mut t.pc_audio, on) != on);
        self.inner.refresh_status_toggles();
    }

    pub fn set_mic_enabled(&self, on: bool) {
        self.inner.toggles.send_if_modified(|t| std::mem::replace(&mut t.mic, on) != on);
        self.inner.refresh_status_toggles();
    }

    /// Whether a PC application is capturing the virtual microphone.
    pub fn set_mic_demand(&self, demanded: bool) {
        self.inner
            .toggles
            .send_if_modified(|t| std::mem::replace(&mut t.mic_demand, demanded) != demanded);
        self.inner.refresh_status_toggles();
    }

    /// Stereo PC-audio capture handle (can be taken again after the previous one is dropped).
    pub fn pc_audio_capture(&self) -> CaptureHandle {
        self.inner.pc_audio.take_handle("pc_audio_capture")
    }

    /// Mono mic playout handle (can be taken again after the previous one is dropped).
    pub fn mic_playout(&self) -> PlayoutHandle {
        self.inner.mic.take_handle("mic_playout")
    }

    pub async fn shutdown(mut self) {
        let inner = self.inner.clone();
        inner.stop();
        for t in std::mem::take(&mut self.tasks) {
            let _ = t.await;
        }
        inner.endpoint.close().await;
        inner.status.update(|s| {
            clear_connection_status(s);
            s.peer_name = None;
            s.state = ConnState::Stopped;
        });
    }
}

impl Drop for Server {
    /// Dropping without [`Server::shutdown`] still stops all tasks and closes the endpoint
    /// in the background.
    fn drop(&mut self) {
        if self.inner.cancel.is_cancelled() {
            return;
        }
        self.inner.stop();
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            let ep = self.inner.endpoint.clone();
            rt.spawn(async move { ep.close().await });
        }
    }
}

impl Inner {
    /// Cancels all tasks and disconnects the phone.
    fn stop(&self) {
        self.cancel.cancel();
        if let Some(active) = self.active.lock().take() {
            active.cancel.cancel();
            active.conn.close(VarInt::from_u32(CLOSE_SHUTDOWN), b"shutdown");
        }
        self.pc_audio.tx.shared.set_target(PHONE, None, &self.pc_audio_stats);
    }

    fn make_pairing(&self, opts: &NetOptions) -> PairingInfo {
        let addrs = net::gather_addrs(&self.endpoint, opts.include_loopback);
        let relay = self.endpoint.addr().relay_urls().next().cloned();
        PairingInfo::new(self.endpoint.id(), self.secret, &self.pc_name, addrs, relay)
    }

    async fn pairing_loop(self: Arc<Self>, tx: watch::Sender<Option<PairingInfo>>, opts: NetOptions) {
        let mut watcher = self.endpoint.watch_addr();
        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => return,
                r = watcher.updated() => if r.is_err() { return },
                _ = tokio::time::sleep(ADDR_REFRESH) => {}
            }
            let info = self.make_pairing(&opts);
            tx.send_if_modified(|cur| {
                if cur.as_ref() == Some(&info) {
                    false
                } else {
                    tracing::info!("pairing addresses: {:?} relay {:?}", info.addrs(), info.relay_url());
                    *cur = Some(info);
                    true
                }
            });
        }
    }

    fn refresh_status_toggles(&self) {
        let t = *self.toggles.borrow();
        let connected = self.active.lock().is_some();
        let phone_ok = !connected || self.phone_mic_allowed.load(Ordering::Acquire);
        self.pc_audio.tx.shared.set_enabled(PHONE, connected && t.pc_audio);
        self.status.update(|s| {
            s.pc_audio_enabled = t.pc_audio;
            s.mic_enabled = t.mic && phone_ok;
            s.mic_demanded = t.mic_demand;
        });
    }

    async fn accept_loop(self: Arc<Self>) {
        loop {
            let incoming = tokio::select! {
                _ = self.cancel.cancelled() => return,
                inc = self.endpoint.accept() => match inc {
                    Some(inc) => inc,
                    None => return,
                },
            };
            let this = self.clone();
            tokio::spawn(async move {
                let conn = match incoming.accept() {
                    Ok(accepting) => match accepting.await {
                        Ok(conn) => conn,
                        Err(err) => {
                            tracing::debug!("handshake failed: {err:#}");
                            return;
                        }
                    },
                    Err(err) => {
                        tracing::debug!("incoming failed: {err:#}");
                        return;
                    }
                };
                match tokio::time::timeout(HANDSHAKE_TIMEOUT, this.authenticate(&conn)).await {
                    Ok(Ok(Some((name, mic_allowed, send, recv)))) => {
                        this.install(conn, name, mic_allowed, send, recv)
                    }
                    Ok(Ok(None)) => {}
                    Ok(Err(err)) => tracing::info!("phone handshake failed: {err:#}"),
                    Err(_) => tracing::info!("phone handshake timed out"),
                }
            });
        }
    }

    /// Reads `Hello`; answers `Welcome` or `Reject`. `None` means rejected.
    async fn authenticate(
        &self,
        conn: &Connection,
    ) -> Result<Option<(String, bool, SendStream, RecvStream)>> {
        let (mut send, mut recv) = conn.accept_bi().await?;
        let ControlMsg::Hello {
            version,
            secret,
            device_name,
            mic_allowed,
        } = ControlMsg::read_from(&mut recv).await?
        else {
            bail!("expected Hello");
        };
        let reason = if version != PROTOCOL_VERSION {
            Some(format!("unsupported protocol version {version}"))
        } else if !secret_eq(&secret, &self.secret) {
            Some("invalid pairing secret".to_owned())
        } else {
            None
        };
        if let Some(reason) = reason {
            tracing::warn!("rejecting {device_name} ({}): {reason}", conn.remote_id());
            ControlMsg::Reject { reason }.write_to(&mut send).await?;
            let _ = send.finish();
            // let the phone read the reason and close; then close from our side
            let _ = tokio::time::timeout(Duration::from_secs(2), conn.closed()).await;
            conn.close(VarInt::from_u32(CLOSE_REJECTED), b"rejected");
            return Ok(None);
        }
        let t = *self.toggles.borrow();
        ControlMsg::Welcome {
            pc_name: self.pc_name.clone(),
            pc_audio_enabled: t.pc_audio,
            mic_enabled: t.mic,
            mic_demanded: t.mic_demand,
        }
        .write_to(&mut send)
        .await?;
        Ok(Some((device_name, mic_allowed, send, recv)))
    }

    /// Makes `conn` the active phone connection, replacing any previous one.
    fn install(self: &Arc<Self>, conn: Connection, name: String, mic_allowed: bool, send: SendStream, recv: RecvStream) {
        if self.cancel.is_cancelled() {
            conn.close(VarInt::from_u32(CLOSE_SHUTDOWN), b"shutdown");
            return;
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let cancel = self.cancel.child_token();
        {
            let mut active = self.active.lock();
            if let Some(old) = active.take() {
                tracing::info!("replacing previous phone connection");
                old.cancel.cancel();
                old.conn.close(VarInt::from_u32(CLOSE_REPLACED), b"replaced");
            }
            *active = Some(ActiveConn {
                id,
                conn: conn.clone(),
                cancel: cancel.clone(),
            });
        }
        tracing::info!("phone '{name}' connected ({})", conn.remote_id());
        self.phone_mic_allowed.store(mic_allowed, Ordering::Release);
        self.mic.slot(0).feeder.lock().reset();
        self.pc_audio
            .tx
            .shared
            .set_target(PHONE, Some(conn.clone()), &self.pc_audio_stats);
        self.status.update(|s| {
            clear_connection_status(s);
            s.state = ConnState::Connected;
            s.peer_name = Some(name);
            s.last_error = None;
        });
        self.refresh_status_toggles();
        let this = self.clone();
        tokio::spawn(async move {
            this.run_connection(&conn, send, recv, &cancel).await;
            let still_active = {
                let mut active = this.active.lock();
                if active.as_ref().is_some_and(|a| a.id == id) {
                    *active = None;
                    true
                } else {
                    false
                }
            };
            if still_active {
                tracing::info!("phone disconnected: {:?}", conn.close_reason());
                this.pc_audio.tx.shared.set_target(PHONE, None, &this.pc_audio_stats);
                this.status.update(|s| {
                    clear_connection_status(s);
                    s.peer_name = None;
                    if s.state == ConnState::Connected {
                        s.state = ConnState::WaitingForPeer;
                    }
                });
                this.refresh_status_toggles();
            }
            conn.close(VarInt::from_u32(0), b"bye");
        });
    }

    async fn run_connection(&self, conn: &Connection, send: SendStream, recv: RecvStream, cancel: &CancellationToken) {
        let media = Media {
            tx: &self.pc_audio_stats,
            out_dir: Dir::PcAudio,
            rx: self.mic.slot(0),
            in_stream: StreamId::Mic,
            in_dir: Dir::Mic,
            activity: None,
        };
        tokio::select! {
            _ = cancel.cancelled() => {}
            _ = conn.closed() => {}
            r = self.control_loop(send, recv) => {
                if let Err(err) = r {
                    tracing::debug!("control stream ended: {err:#}");
                }
            }
            _ = recv_media(conn, &media, &self.status) => {}
            _ = stats_loop(conn, &media, &self.status) => {}
        }
    }

    async fn control_loop(&self, mut send: SendStream, mut recv: RecvStream) -> Result<()> {
        let mut toggles_rx = self.toggles.subscribe();
        let mut sent = *toggles_rx.borrow_and_update();
        loop {
            tokio::select! {
                msg = ControlMsg::read_from(&mut recv) => match msg? {
                    ControlMsg::MicAllowed(on) => {
                        self.phone_mic_allowed.store(on, Ordering::Release);
                        self.refresh_status_toggles();
                    }
                    other => tracing::debug!("unexpected control message {other:?}"),
                },
                r = toggles_rx.changed() => {
                    r?;
                    let t = *toggles_rx.borrow_and_update();
                    if (t.pc_audio, t.mic) != (sent.pc_audio, sent.mic) {
                        ControlMsg::Toggles { pc_audio_enabled: t.pc_audio, mic_enabled: t.mic }
                            .write_to(&mut send)
                            .await?;
                    }
                    if t.mic_demand != sent.mic_demand {
                        ControlMsg::MicDemand(t.mic_demand).write_to(&mut send).await?;
                    }
                    sent = t;
                }
            }
        }
    }
}
