//! Network side of a received stream: hands encoded packets (with their arrival time) to the
//! real-time playout through a lock-free SPSC queue, and tracks stream activity. Decoding, loss
//! concealment and buffering decisions happen in [`super::playout`], on the audio thread.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rtrb::Producer;

use crate::proto::{DatagramHeader, PacketKind, DATAGRAM_HEADER_LEN, MAX_AUDIO_PAYLOAD};

/// Packets in flight between the network task and the playout (1.28 s of audio).
pub(crate) const PACKET_QUEUE: usize = 128;
/// A stream counts as inactive after this long without audio packets.
pub(crate) const ACTIVE_HOLD: Duration = Duration::from_millis(1500);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PktKind {
    Audio,
    /// The sender went quiet; `seq` is its next audio seq.
    Silence,
    /// New stream owner (connection): drop everything and start over.
    Reset,
}

/// One queued packet. Fixed size, so the queue never allocates.
pub(crate) struct Packet {
    pub(crate) kind: PktKind,
    pub(crate) seq: u32,
    pub(crate) arrival: Instant,
    pub(crate) len: u16,
    pub(crate) data: [u8; MAX_AUDIO_PAYLOAD],
}

impl Packet {
    pub(crate) fn payload(&self) -> &[u8] {
        &self.data[..self.len as usize]
    }
}

/// Counters shared between the network side, the playout and the stats loop.
pub(crate) struct RxShared {
    /// Smoothed amount of audio buffered ahead of the playout point, ms (f32 bits).
    pub(crate) buffer_ms: AtomicU32,
    /// Current adaptive target delay, ms (f32 bits).
    pub(crate) target_ms: AtomicU32,
    /// Outage events: the buffer ran dry while audio was expected.
    pub(crate) underruns: AtomicU64,
    /// Frames concealed because their packet never arrived in time (loss or lateness).
    pub(crate) lost: AtomicU64,
    /// Packets that arrived after their playout time and were dropped.
    pub(crate) late: AtomicU64,
    /// All concealed (PLC) frames: losses plus expansion during outages.
    pub(crate) concealed: AtomicU64,
    pub(crate) bytes: AtomicU64,
    /// Keep-awake packets received (ignored otherwise).
    pub(crate) pace: AtomicU64,
    /// Fallback reset request when the packet queue is full.
    pub(crate) flush: AtomicBool,
    /// Mixer slot assigned to a peer (hub only; unassigned slots are skipped once silent).
    pub(crate) in_use: AtomicBool,
    /// The phone user muted this peer: the feeder stops queueing its audio and the mixer leaves
    /// it out (with a ramp). Changed only under the feeder lock.
    pub(crate) muted: AtomicBool,
}

impl RxShared {
    pub(crate) fn new() -> Self {
        Self {
            buffer_ms: AtomicU32::new(0f32.to_bits()),
            target_ms: AtomicU32::new(0f32.to_bits()),
            underruns: AtomicU64::new(0),
            lost: AtomicU64::new(0),
            late: AtomicU64::new(0),
            concealed: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            pace: AtomicU64::new(0),
            flush: AtomicBool::new(false),
            in_use: AtomicBool::new(false),
            muted: AtomicBool::new(false),
        }
    }

    /// Zeroes the statistics (slot handed to a new peer).
    pub(crate) fn clear_stats(&self) {
        for c in [
            &self.underruns,
            &self.lost,
            &self.late,
            &self.concealed,
            &self.bytes,
            &self.pace,
        ] {
            c.store(0, Ordering::Relaxed);
        }
        self.buffer_ms.store(0f32.to_bits(), Ordering::Relaxed);
    }
}

/// Network-side feeder, owned by the session (behind a mutex; never touched by the audio thread).
pub(crate) struct RxFeeder {
    shared: Arc<RxShared>,
    producer: Producer<Packet>,
    last_audio: Option<Instant>,
    silenced: bool,
    active: bool,
    /// Muted: the Silence marker that ends the stream for the playout has been queued.
    mute_marked: bool,
}

impl RxFeeder {
    pub(crate) fn new(producer: Producer<Packet>, shared: Arc<RxShared>) -> Self {
        Self {
            shared,
            producer,
            last_audio: None,
            silenced: true,
            active: false,
            mute_marked: false,
        }
    }

    /// Starts a fresh stream (new connection, new owner, or unmuted).
    pub(crate) fn reset(&mut self) {
        self.last_audio = None;
        self.silenced = true;
        self.active = false;
        self.mute_marked = false;
        if !self.push(PktKind::Reset, 0, Instant::now(), &[]) {
            self.shared.flush.store(true, Ordering::Release);
        }
    }

    /// Handles one parsed audio/silence datagram. Returns `true` if `active` flipped to `true`.
    ///
    /// While muted, audio still counts for activity but is not queued: the output may be closed,
    /// and an unread queue would replay stale audio after unmuting. One Silence marker ends the
    /// stream cleanly for the playout; unmuting resets the feeder.
    pub(crate) fn on_packet(&mut self, hdr: DatagramHeader, payload: &[u8], now: Instant) -> bool {
        match hdr.kind {
            PacketKind::Audio => {
                self.shared.bytes.fetch_add(
                    (payload.len() + DATAGRAM_HEADER_LEN) as u64,
                    Ordering::Relaxed,
                );
                if !self.shared.muted.load(Ordering::Relaxed) {
                    self.push(PktKind::Audio, hdr.seq, now, payload);
                } else if !self.mute_marked {
                    self.mute_marked = self.push(PktKind::Silence, hdr.seq, now, &[]);
                }
                self.last_audio = Some(now);
                self.silenced = false;
                let became_active = !self.active;
                self.active = true;
                became_active
            }
            PacketKind::Silence => {
                if !self.shared.muted.load(Ordering::Relaxed) {
                    self.push(PktKind::Silence, hdr.seq, now, &[]);
                }
                self.silenced = true;
                false
            }
            PacketKind::Pace => false,
        }
    }

    /// Queues a packet for the playout; drops it if the queue is full (audio thread stalled).
    fn push(&mut self, kind: PktKind, seq: u32, arrival: Instant, payload: &[u8]) -> bool {
        if payload.len() > MAX_AUDIO_PAYLOAD || self.producer.is_full() {
            return false;
        }
        let mut pkt = Packet {
            kind,
            seq,
            arrival,
            len: payload.len() as u16,
            data: [0; MAX_AUDIO_PAYLOAD],
        };
        pkt.data[..payload.len()].copy_from_slice(payload);
        self.producer.push(pkt).is_ok()
    }

    /// Re-evaluates activity; returns `Some(new)` when it changed.
    pub(crate) fn poll_active(&mut self, now: Instant) -> Option<bool> {
        let active = self
            .last_audio
            .is_some_and(|t| now.duration_since(t) < ACTIVE_HOLD);
        (active != self.active).then(|| {
            self.active = active;
            active
        })
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active
    }

    /// Audio is flowing right now: recent audio packets and no silence marker since.
    pub(crate) fn is_streaming(&self, now: Instant) -> bool {
        !self.silenced
            && self
                .last_audio
                .is_some_and(|t| now.duration_since(t) < ACTIVE_HOLD)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::StreamId;

    fn hdr(kind: PacketKind, seq: u32) -> DatagramHeader {
        DatagramHeader {
            kind,
            stream: StreamId::Mic,
            seq,
        }
    }

    #[test]
    fn queues_packets_and_tracks_activity() {
        let (producer, mut consumer) = rtrb::RingBuffer::new(4);
        let shared = Arc::new(RxShared::new());
        let mut f = RxFeeder::new(producer, shared.clone());
        let t0 = Instant::now();
        assert!(f.on_packet(hdr(PacketKind::Audio, 7), &[1, 2, 3], t0));
        assert!(!f.on_packet(hdr(PacketKind::Audio, 8), &[4], t0));
        assert!(f.is_streaming(t0));
        assert!(!f.on_packet(hdr(PacketKind::Pace, 0), &[], t0));
        f.on_packet(hdr(PacketKind::Silence, 9), &[], t0);
        assert!(!f.is_streaming(t0), "silence marker stops streaming");
        assert!(f.is_active(), "activity is held for ACTIVE_HOLD");
        assert_eq!(f.poll_active(t0 + ACTIVE_HOLD), Some(false));
        // queue full: further packets are dropped, reset falls back to the flush flag
        f.on_packet(hdr(PacketKind::Audio, 10), &[5], t0);
        f.reset();
        assert!(shared.flush.load(Ordering::Acquire));

        let kinds: Vec<(PktKind, u32, Vec<u8>)> = std::iter::from_fn(|| consumer.pop().ok())
            .map(|p| (p.kind, p.seq, p.payload().to_vec()))
            .collect();
        assert_eq!(
            kinds,
            vec![
                (PktKind::Audio, 7, vec![1, 2, 3]),
                (PktKind::Audio, 8, vec![4]),
                (PktKind::Silence, 9, vec![]),
                (PktKind::Audio, 10, vec![5]),
            ]
        );
        assert_eq!(shared.bytes.load(Ordering::Relaxed), 3 * 8 + 5);
    }
}
