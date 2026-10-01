//! Send side: a dedicated thread drains the capture ring in 10 ms frames, applies silence
//! suppression, encodes with Opus and sends QUIC datagrams. The capture callback wakes it with
//! `Thread::unpark` (lock-free) once per completed frame, so it never busy-polls.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use anyhow::Result;
use parking_lot::Mutex;
use bytes::Bytes;
use iroh::endpoint::Connection;
use rtrb::Consumer;

use super::codec::Encoder;
use crate::proto::{
    DatagramHeader, PacketKind, StreamId, DATAGRAM_HEADER_LEN, FRAME_SAMPLES, MAX_AUDIO_PAYLOAD,
};

/// Peak amplitude below which a frame counts as digital silence (≈ -96 dBFS).
const SILENCE_LEVEL: f32 = 1.0 / 65_536.0;
/// Frames of continuous silence before the sender goes quiet (300 ms).
const SILENCE_FRAMES: u32 = 30;
/// Silence markers sent (on consecutive frame ticks) for robustness against loss.
const MARKER_REPEATS: u32 = 3;

/// What to do with the current frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GateAction {
    Send,
    Marker,
    Skip,
}

/// Silence suppression state machine.
#[derive(Default)]
pub(crate) struct SilenceGate {
    silent_frames: u32,
}

impl SilenceGate {
    pub(crate) fn on_frame(&mut self, silent: bool) -> GateAction {
        if !silent {
            self.silent_frames = 0;
            return GateAction::Send;
        }
        self.silent_frames = self.silent_frames.saturating_add(1);
        if self.silent_frames <= SILENCE_FRAMES {
            GateAction::Send
        } else if self.silent_frames <= SILENCE_FRAMES + MARKER_REPEATS {
            GateAction::Marker
        } else {
            GateAction::Skip
        }
    }

    pub(crate) fn reset(&mut self) {
        self.silent_frames = 0;
    }
}

pub(crate) fn is_silent(frame: &[f32]) -> bool {
    frame.iter().all(|s| s.abs() < SILENCE_LEVEL)
}

/// Per-destination send counters (one per connected peer).
#[derive(Default)]
pub(crate) struct TxStats {
    /// True while non-silent audio is being sent to this destination.
    pub(crate) active: AtomicBool,
    pub(crate) bytes: AtomicU64,
}

struct Target {
    key: usize,
    conn: Connection,
    enabled: bool,
    /// Newly enabled: announce the current seq with a SILENCE marker before audio, so the
    /// receiver does not mistake the skipped sequence numbers for loss.
    fresh: bool,
    stats: Arc<TxStats>,
}

/// State shared between the session (network) side and the sender thread. One encoded stream
/// fans out to every enabled destination.
pub(crate) struct TxShared {
    targets: Mutex<Vec<Target>>,
    reset: AtomicBool,
    stop: AtomicBool,
}

impl TxShared {
    /// Attaches (`Some`) or detaches (`None`) the connection of destination `key`. The new
    /// destination starts disabled. The codec restarts when the first destination attaches.
    pub(crate) fn set_target(&self, key: usize, conn: Option<Connection>, stats: &Arc<TxStats>) {
        let mut targets = self.targets.lock();
        targets.retain(|t| t.key != key);
        stats.active.store(false, Ordering::Relaxed);
        if let Some(conn) = conn {
            if targets.is_empty() {
                self.reset.store(true, Ordering::Release);
            }
            targets.push(Target {
                key,
                conn,
                enabled: false,
                fresh: true,
                stats: stats.clone(),
            });
        }
    }

    /// Whether captured audio should be sent to destination `key` (toggles, mic demand).
    pub(crate) fn set_enabled(&self, key: usize, on: bool) {
        let mut targets = self.targets.lock();
        if let Some(t) = targets.iter_mut().find(|t| t.key == key) {
            if on && !t.enabled {
                t.fresh = true;
            }
            if !on {
                t.stats.active.store(false, Ordering::Relaxed);
            }
            t.enabled = on;
        }
    }
}

/// Owns the sender thread; stops it on drop.
pub(crate) struct TxStream {
    pub(crate) shared: Arc<TxShared>,
    thread: Option<JoinHandle<()>>,
}

impl TxStream {
    /// Spawns the sender thread. Returns the stream and the thread handle used for waking it.
    pub(crate) fn spawn(
        stream: StreamId,
        consumer: Consumer<f32>,
        bitrate: i32,
        complexity: i32,
    ) -> Result<(Self, std::thread::Thread)> {
        let shared = Arc::new(TxShared {
            targets: Mutex::new(Vec::new()),
            reset: AtomicBool::new(true),
            stop: AtomicBool::new(false),
        });
        let encoder = Encoder::new(stream.channels(), bitrate, complexity)?;
        let sh = shared.clone();
        let thread = std::thread::Builder::new()
            .name(format!("audiobridge-tx-{stream:?}"))
            .spawn(move || sender_loop(stream, consumer, encoder, sh))?;
        let handle = thread.thread().clone();
        Ok((
            Self {
                shared,
                thread: Some(thread),
            },
            handle,
        ))
    }
}

impl Drop for TxStream {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            let _ = t.join();
        }
    }
}

fn send(target: &Target, data: &[u8]) {
    if let Err(err) = target.conn.send_datagram(Bytes::copy_from_slice(data)) {
        tracing::trace!("send_datagram: {err}");
    }
    target.stats.bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
}

fn sender_loop(stream: StreamId, mut consumer: Consumer<f32>, mut encoder: Encoder, shared: Arc<TxShared>) {
    let frame_len = FRAME_SAMPLES * encoder.channels();
    let mut frame = vec![0f32; frame_len];
    let mut packet = vec![0u8; DATAGRAM_HEADER_LEN + MAX_AUDIO_PAYLOAD];
    let mut marker = [0u8; DATAGRAM_HEADER_LEN];
    let mut gate = SilenceGate::default();
    let mut seq: u32 = 0;
    loop {
        if shared.stop.load(Ordering::Acquire) {
            return;
        }
        while consumer.slots() >= frame_len {
            let chunk = consumer.read_chunk(frame_len).expect("slots checked");
            let (a, b) = chunk.as_slices();
            frame[..a.len()].copy_from_slice(a);
            frame[a.len()..].copy_from_slice(b);
            chunk.commit_all();

            if shared.reset.swap(false, Ordering::AcqRel) {
                encoder.reset();
                gate.reset();
            }
            let mut targets = shared.targets.lock();
            if !targets.iter().any(|t| t.enabled) {
                gate.reset();
                continue;
            }
            DatagramHeader {
                kind: PacketKind::Silence,
                stream,
                seq,
            }
            .write(&mut marker);
            match gate.on_frame(is_silent(&frame)) {
                GateAction::Send => {
                    let n = match encoder.encode(&frame, &mut packet[DATAGRAM_HEADER_LEN..]) {
                        Ok(n) => n,
                        Err(err) => {
                            tracing::warn!("opus encode failed: {err:#}");
                            continue;
                        }
                    };
                    DatagramHeader {
                        kind: PacketKind::Audio,
                        stream,
                        seq,
                    }
                    .write(&mut packet);
                    let len = DATAGRAM_HEADER_LEN + n;
                    for t in targets.iter_mut().filter(|t| t.enabled) {
                        if std::mem::take(&mut t.fresh) {
                            send(t, &marker);
                        }
                        send(t, &packet[..len]);
                        t.stats.active.store(true, Ordering::Relaxed);
                    }
                    seq = seq.wrapping_add(1);
                }
                GateAction::Marker => {
                    for t in targets.iter_mut().filter(|t| t.enabled) {
                        t.fresh = false;
                        send(t, &marker);
                        t.stats.active.store(false, Ordering::Relaxed);
                    }
                }
                GateAction::Skip => {}
            }
        }
        std::thread::park();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_gate_sequence() {
        let mut g = SilenceGate::default();
        assert_eq!(g.on_frame(false), GateAction::Send);
        for _ in 0..SILENCE_FRAMES {
            assert_eq!(g.on_frame(true), GateAction::Send);
        }
        for _ in 0..MARKER_REPEATS {
            assert_eq!(g.on_frame(true), GateAction::Marker);
        }
        for _ in 0..1000 {
            assert_eq!(g.on_frame(true), GateAction::Skip);
        }
        // resumes instantly
        assert_eq!(g.on_frame(false), GateAction::Send);
        assert_eq!(g.on_frame(true), GateAction::Send);
    }

    #[test]
    fn silence_detection() {
        assert!(is_silent(&[0.0; 960]));
        assert!(is_silent(&[1e-6; 960]));
        let mut f = [0.0f32; 960];
        f[500] = 0.01;
        assert!(!is_silent(&f));
    }
}
