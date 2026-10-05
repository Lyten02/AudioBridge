//! Connection management for both sides: PC [`Server`] and phone [`Hub`].

mod hub;
mod net;
mod server;

use std::net::IpAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use iroh::endpoint::Connection;
use iroh::TransportAddr;
use tokio::sync::watch;

use crate::audio::tx::TxStats;
use crate::audio::RxSlot;
use crate::proto::{DatagramHeader, PacketKind, StreamId};

pub use hub::{Hub, MAX_PEERS};
pub use server::Server;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnState {
    Starting,
    /// Server: listening, no phone connected.
    WaitingForPeer,
    /// Phone: first connection attempt to this PC in progress.
    Connecting,
    Connected,
    Reconnecting,
    Stopped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathKind {
    Lan,
    Tailscale,
    Direct,
    Relay,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct StreamStats {
    pub active: bool,
    pub buffer_ms: f32,
    pub underruns: u64,
    pub lost_packets: u64,
    pub kbps: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Status {
    pub state: ConnState,
    /// Phone name on the PC side; PC name on the phone side (the pairing name until the PC
    /// introduces itself, then the last name it reported).
    pub peer_name: Option<String>,
    pub path: Option<PathKind>,
    pub rtt_ms: Option<f32>,
    /// Effective toggle (PC side is authoritative, mirrored to the phone).
    pub pc_audio_enabled: bool,
    /// Effective toggle (both sides must allow).
    pub mic_enabled: bool,
    /// PC reports a consumer is capturing the virtual mic.
    pub mic_demanded: bool,
    pub pc_audio: StreamStats,
    pub mic: StreamStats,
    pub last_error: Option<String>,
}

impl Status {
    fn new() -> Self {
        Self {
            state: ConnState::Starting,
            peer_name: None,
            path: None,
            rtt_ms: None,
            pc_audio_enabled: true,
            mic_enabled: true,
            mic_demanded: false,
            pc_audio: StreamStats::default(),
            mic: StreamStats::default(),
            last_error: None,
        }
    }
}

/// PC-side configuration.
pub struct ServerConfig {
    pub data_dir: std::path::PathBuf,
    pub pc_name: String,
}

/// Phone-side configuration.
pub struct HubConfig {
    pub data_dir: std::path::PathBuf,
    pub device_name: String,
}

/// Status of one paired PC.
#[derive(Clone, Debug, PartialEq)]
pub struct PeerStatus {
    /// [`crate::pairing::PairingInfo::peer_id`].
    pub id: String,
    pub status: Status,
    /// Muted on the phone ([`Hub::set_muted`]): stays connected, left out of the mix.
    pub muted: bool,
}

/// Phone-side status: one entry per paired PC (in `set_peers` order) plus aggregates.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HubStatus {
    pub peers: Vec<PeerStatus>,
    /// Phone-side mic permission/toggle.
    pub mic_enabled: bool,
    /// Some connected PC has the mic enabled and demanded: the phone should record.
    pub mic_wanted: bool,
    /// Some unmuted PC's audio is arriving: the phone output should be open.
    pub pc_audio_active: bool,
}

impl HubStatus {
    fn recompute(&mut self) {
        self.mic_wanted = self.peers.iter().any(|p| {
            p.status.state == ConnState::Connected && p.status.mic_enabled && p.status.mic_demanded
        });
        self.pc_audio_active = self
            .peers
            .iter()
            .any(|p| p.status.pc_audio.active && !p.muted);
    }
}

/// Networking knobs; [`NetOptions::default`] is what the apps use.
#[derive(Clone, Debug)]
pub struct NetOptions {
    /// Use the n0 relay servers (hole-punching assistance and relay fallback).
    pub relay: bool,
    /// Server: publish its address via n0 DNS/pkarr. Hub: resolve PCs through it.
    pub discovery: bool,
    /// Server: put 127.0.0.1 into the pairing info (tests / same-machine probing).
    pub include_loopback: bool,
    /// Server IPv4 UDP port. `None`: the last used port (initially [`crate::proto::DEFAULT_PORT`]),
    /// falling back to a random port if busy. `Some(0)`: random. `Some(p)`: exactly `p`.
    pub port: Option<u16>,
}

impl Default for NetOptions {
    fn default() -> Self {
        Self {
            relay: true,
            discovery: true,
            include_loopback: false,
            port: None,
        }
    }
}

impl NetOptions {
    /// Loopback/LAN only: no relay, no discovery, loopback in the pairing info, random port.
    pub fn local_only() -> Self {
        Self {
            relay: false,
            discovery: false,
            include_loopback: true,
            port: Some(0),
        }
    }
}

/// Status publisher that only notifies watchers on real changes. Either a standalone status
/// (server) or one peer's entry inside the hub status.
#[derive(Clone)]
enum StatusCell {
    Single(Arc<watch::Sender<Status>>),
    Peer {
        hub: Arc<watch::Sender<HubStatus>>,
        id: Arc<str>,
    },
}

impl StatusCell {
    fn update(&self, f: impl FnOnce(&mut Status)) {
        match self {
            Self::Single(tx) => {
                tx.send_if_modified(|s| {
                    let before = s.clone();
                    f(s);
                    *s != before
                });
            }
            Self::Peer { hub, id } => {
                hub.send_if_modified(|h| {
                    let Some(p) = h.peers.iter_mut().find(|p| *p.id == **id) else {
                        return false;
                    };
                    let before = p.status.clone();
                    f(&mut p.status);
                    if p.status == before {
                        return false;
                    }
                    h.recompute();
                    true
                });
            }
        }
    }
}

fn is_tailscale(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 100 && (o[1] & 0xC0) == 64
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
        }
    }
}

fn is_lan(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_link_local() || v4.is_loopback(),
        IpAddr::V6(v6) => {
            let s0 = v6.segments()[0];
            v6.is_loopback() || (s0 & 0xfe00) == 0xfc00 || (s0 & 0xffc0) == 0xfe80
        }
    }
}

fn classify(addr: &TransportAddr) -> PathKind {
    match addr {
        TransportAddr::Relay(_) => PathKind::Relay,
        TransportAddr::Ip(sa) => {
            let ip = sa.ip().to_canonical();
            if is_tailscale(ip) {
                PathKind::Tailscale
            } else if is_lan(ip) {
                PathKind::Lan
            } else {
                PathKind::Direct
            }
        }
        _ => PathKind::Direct,
    }
}

/// Selected path kind and its RTT.
fn path_info(conn: &Connection) -> (Option<PathKind>, Option<f32>) {
    let paths = conn.paths();
    let selected = paths.iter().find(|p| p.is_selected());
    match selected {
        Some(p) => (
            Some(classify(p.remote_addr())),
            Some(p.rtt().as_secs_f32() * 1000.0),
        ),
        None => (None, None),
    }
}

fn changed_enough(old: f32, new: f32, abs: f32, rel: f32) -> bool {
    (new - old).abs() >= abs.max(old.abs() * rel)
}

fn round1(v: f32) -> f32 {
    (v * 10.0).round() / 10.0
}

/// Which stats block a stream maps to.
#[derive(Clone, Copy)]
enum Dir {
    PcAudio,
    Mic,
}

impl Dir {
    fn get(self, s: &mut Status) -> &mut StreamStats {
        match self {
            Dir::PcAudio => &mut s.pc_audio,
            Dir::Mic => &mut s.mic,
        }
    }
}

/// Media of one live connection: what we send and what we receive.
struct Media<'a> {
    tx: &'a TxStats,
    out_dir: Dir,
    rx: &'a RxSlot,
    in_stream: StreamId,
    in_dir: Dir,
    /// Notified when the incoming stream becomes active.
    activity: Option<&'a tokio::sync::Notify>,
}

/// Receives datagrams and queues them for the playout until the connection ends.
async fn recv_media(conn: &Connection, media: &Media<'_>, status: &StatusCell) {
    while let Ok(dg) = conn.read_datagram().await {
        let (hdr, payload) = match DatagramHeader::parse(&dg) {
            Ok(v) => v,
            Err(err) => {
                tracing::debug!("dropping bad datagram: {err:#}");
                continue;
            }
        };
        if hdr.kind == PacketKind::Pace {
            media.rx.shared.pace.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        if hdr.stream != media.in_stream {
            continue;
        }
        let became_active = media.rx.feeder.lock().on_packet(hdr, payload, Instant::now());
        if became_active {
            status.update(|s| media.in_dir.get(s).active = true);
            if let Some(n) = media.activity {
                n.notify_one();
            }
        }
    }
}

const STATS_PERIOD: Duration = Duration::from_millis(500);

/// Refreshes path/RTT and stream statistics at 2 Hz while connected.
async fn stats_loop(conn: &Connection, media: &Media<'_>, status: &StatusCell) {
    let mut tick = tokio::time::interval(STATS_PERIOD);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last = Instant::now();
    let mut rx_bytes = media.rx.shared.bytes.load(Ordering::Relaxed);
    let mut tx_bytes = media.tx.bytes.load(Ordering::Relaxed);
    let mut last_concealed = media.rx.shared.concealed.load(Ordering::Relaxed);
    loop {
        tick.tick().await;
        let now = Instant::now();
        let dt = now.duration_since(last).as_secs_f32().max(1e-3);
        last = now;
        let (path, rtt) = path_info(conn);

        let rx = &media.rx.shared;
        let rxb = rx.bytes.load(Ordering::Relaxed);
        let rx_kbps = rxb.saturating_sub(rx_bytes) as f32 * 8.0 / 1000.0 / dt;
        rx_bytes = rxb;
        let rx_active = {
            let mut f = media.rx.feeder.lock();
            f.poll_active(now);
            f.is_active()
        };
        let buffer_ms = f32::from_bits(rx.buffer_ms.load(Ordering::Relaxed));
        let underruns = rx.underruns.load(Ordering::Relaxed);
        let lost = rx.lost.load(Ordering::Relaxed);
        let concealed = rx.concealed.load(Ordering::Relaxed);
        if concealed != last_concealed {
            tracing::info!(
                "rx {:?}: underruns={underruns} concealed={concealed} lost={lost} late={} buffer={buffer_ms:.0}ms target={:.0}ms",
                media.in_stream,
                rx.late.load(Ordering::Relaxed),
                f32::from_bits(rx.target_ms.load(Ordering::Relaxed)),
            );
            last_concealed = concealed;
        }

        let txb = media.tx.bytes.load(Ordering::Relaxed);
        let tx_kbps = txb.saturating_sub(tx_bytes) as f32 * 8.0 / 1000.0 / dt;
        tx_bytes = txb;
        let tx_active = media.tx.active.load(Ordering::Relaxed);

        status.update(|s| {
            s.path = path;
            match (s.rtt_ms, rtt) {
                (Some(old), Some(new)) if !changed_enough(old, new, 0.5, 0.1) => {}
                (_, new) => s.rtt_ms = new.map(round1),
            }
            let r = media.in_dir.get(s);
            r.active = rx_active;
            if changed_enough(r.buffer_ms, buffer_ms, 1.0, 0.05) {
                r.buffer_ms = buffer_ms.round();
            }
            r.underruns = underruns;
            r.lost_packets = lost;
            if changed_enough(r.kbps, rx_kbps, 2.0, 0.05) {
                r.kbps = round1(rx_kbps);
            }
            let t = media.out_dir.get(s);
            t.active = tx_active;
            if changed_enough(t.kbps, tx_kbps, 2.0, 0.05) {
                t.kbps = round1(tx_kbps);
            }
        });
    }
}

/// Clears connection-scoped status fields after a disconnect.
fn clear_connection_status(s: &mut Status) {
    s.path = None;
    s.rtt_ms = None;
    for st in [&mut s.pc_audio, &mut s.mic] {
        st.active = false;
        st.kbps = 0.0;
        st.buffer_ms = 0.0;
    }
}

/// Constant-time equality for the pairing secret.
fn secret_eq(a: &[u8; 16], b: &[u8; 16]) -> bool {
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_classification() {
        let ip = |s: &str| TransportAddr::Ip(s.parse().unwrap());
        assert_eq!(classify(&ip("100.86.3.4:1")), PathKind::Tailscale);
        assert_eq!(classify(&ip("100.127.255.1:1")), PathKind::Tailscale);
        assert_eq!(classify(&ip("100.128.0.1:1")), PathKind::Direct);
        assert_eq!(classify(&ip("[fd7a:115c:a1e0::1]:1")), PathKind::Tailscale);
        assert_eq!(classify(&ip("192.168.2.3:1")), PathKind::Lan);
        assert_eq!(classify(&ip("[::ffff:192.168.2.3]:1")), PathKind::Lan);
        assert_eq!(classify(&ip("10.0.0.1:1")), PathKind::Lan);
        assert_eq!(classify(&ip("[fe80::1]:1")), PathKind::Lan);
        assert_eq!(classify(&ip("8.8.8.8:1")), PathKind::Direct);
        let relay = TransportAddr::Relay("https://relay.example.com./".parse().unwrap());
        assert_eq!(classify(&relay), PathKind::Relay);
    }

    #[test]
    fn secret_compare() {
        assert!(secret_eq(&[1; 16], &[1; 16]));
        let mut b = [1; 16];
        b[15] = 2;
        assert!(!secret_eq(&[1; 16], &b));
    }
}
