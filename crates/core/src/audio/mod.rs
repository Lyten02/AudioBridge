//! Real-time audio handles at the platform boundary (48 kHz, f32, interleaved).

pub(crate) mod codec;
pub(crate) mod mixer;
pub(crate) mod playout;
pub(crate) mod rx;
#[cfg(test)]
pub(crate) mod sim;
pub(crate) mod tx;

use std::sync::Arc;

use anyhow::Result;
use parking_lot::Mutex;
use rtrb::{Producer, RingBuffer};

use crate::proto::{StreamId, FRAME_SAMPLES, SAMPLE_RATE};
use mixer::Mixer;
use playout::Playout;
use rx::{RxFeeder, RxShared, PACKET_QUEUE};
use tx::TxStream;

/// Capture ring length (ms). Generous: the sender drains it every 10 ms.
const CAPTURE_RING_MS: usize = 200;

/// Handles live in a "home" slot while nobody holds them; dropping a handle returns it there,
/// so platform code may obtain it again after re-opening its device.
type Home<T> = Arc<Mutex<Option<T>>>;

/// Producer side for a captured stream. Owned by the platform capture thread/callback. Real-time
/// safe: no blocking locks, no allocation, never blocks. Channel count fixed per stream.
pub struct CaptureHandle {
    inner: Option<CaptureInner>,
    home: Home<CaptureInner>,
}

pub(crate) struct CaptureInner {
    producer: Producer<f32>,
    channels: usize,
    frame_len: usize,
    pending: usize,
    sender: std::thread::Thread,
}

impl CaptureHandle {
    pub fn channels(&self) -> usize {
        self.inner().channels
    }

    /// Appends interleaved 48 kHz samples (any length). Samples that do not fit are dropped.
    pub fn push(&mut self, interleaved: &[f32]) {
        let inner = self.inner.as_mut().expect("capture handle present until drop");
        let n = interleaved.len().min(inner.producer.slots());
        if n == 0 {
            return;
        }
        let mut chunk = inner.producer.write_chunk(n).expect("slots checked");
        let (a, b) = chunk.as_mut_slices();
        let split = a.len();
        a.copy_from_slice(&interleaved[..split]);
        b.copy_from_slice(&interleaved[split..n]);
        chunk.commit_all();
        inner.pending += n;
        if inner.pending >= inner.frame_len {
            inner.pending %= inner.frame_len;
            inner.sender.unpark();
        }
    }

    fn inner(&self) -> &CaptureInner {
        self.inner.as_ref().expect("capture handle present until drop")
    }
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            *self.home.lock() = Some(inner);
        }
    }
}

/// Consumer side for a received stream. Owned by the platform render callback. Real-time safe,
/// never blocks; writes silence on underrun.
pub struct PlayoutHandle {
    inner: Option<PlayoutSource>,
    home: Home<PlayoutSource>,
}

/// A single peer's playout, or the mix of several peers' playouts.
pub(crate) enum PlayoutSource {
    Single(Box<Playout>),
    Mixed(Mixer),
}

impl PlayoutHandle {
    pub fn channels(&self) -> usize {
        match self.inner.as_ref().expect("playout handle present until drop") {
            PlayoutSource::Single(p) => p.channels(),
            PlayoutSource::Mixed(m) => m.channels(),
        }
    }

    /// Fills `out` (interleaved, 48 kHz) with received audio, or silence when none is buffered.
    pub fn fill(&mut self, out: &mut [f32]) {
        match self.inner.as_mut().expect("playout handle present until drop") {
            PlayoutSource::Single(p) => p.fill(out),
            PlayoutSource::Mixed(m) => m.fill(out),
        }
    }
}

impl Drop for PlayoutHandle {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            *self.home.lock() = Some(inner);
        }
    }
}

/// Session-side ownership of an outgoing stream: sender thread + capture handle slot.
pub(crate) struct OutgoingStream {
    pub(crate) tx: TxStream,
    home: Home<CaptureInner>,
}

impl OutgoingStream {
    pub(crate) fn new(stream: StreamId, bitrate: i32, complexity: i32) -> Result<Self> {
        let channels = stream.channels();
        let (producer, consumer) =
            RingBuffer::new(SAMPLE_RATE as usize / 1000 * CAPTURE_RING_MS * channels);
        let (tx, sender) = TxStream::spawn(stream, consumer, bitrate, complexity)?;
        let inner = CaptureInner {
            producer,
            channels,
            frame_len: FRAME_SAMPLES * channels,
            pending: 0,
            sender,
        };
        Ok(Self {
            tx,
            home: Arc::new(Mutex::new(Some(inner))),
        })
    }

    /// Hands out the capture handle. Panics if it is currently held elsewhere.
    pub(crate) fn take_handle(&self, what: &str) -> CaptureHandle {
        let inner = self
            .home
            .lock()
            .take()
            .unwrap_or_else(|| panic!("{what}: handle already taken (drop the previous one first)"));
        CaptureHandle {
            inner: Some(inner),
            home: self.home.clone(),
        }
    }
}

/// Network side of one received stream: jitter/decode feeder plus the state shared with its
/// playout.
#[derive(Clone)]
pub(crate) struct RxSlot {
    pub(crate) feeder: Arc<Mutex<RxFeeder>>,
    pub(crate) shared: Arc<RxShared>,
}

/// Session-side ownership of incoming audio: one or more slots + the playout handle slot.
pub(crate) struct IncomingStream {
    slots: Vec<RxSlot>,
    home: Home<PlayoutSource>,
}

impl IncomingStream {
    /// One peer, played directly.
    pub(crate) fn new(stream: StreamId) -> Result<Self> {
        let (slot, playout) = Self::make_slot(stream.channels())?;
        Ok(Self {
            slots: vec![slot],
            home: Arc::new(Mutex::new(Some(PlayoutSource::Single(Box::new(playout))))),
        })
    }

    /// `n` peer slots mixed into one output. All memory is allocated here, up front.
    pub(crate) fn new_mixed(stream: StreamId, n: usize) -> Result<Self> {
        let channels = stream.channels();
        let mut slots = Vec::with_capacity(n);
        let mut playouts = Vec::with_capacity(n);
        for _ in 0..n {
            let (slot, playout) = Self::make_slot(channels)?;
            slots.push(slot);
            playouts.push(playout);
        }
        Ok(Self {
            slots,
            home: Arc::new(Mutex::new(Some(PlayoutSource::Mixed(Mixer::new(channels, playouts))))),
        })
    }

    fn make_slot(channels: usize) -> Result<(RxSlot, Playout)> {
        let (producer, consumer) = RingBuffer::new(PACKET_QUEUE);
        let shared = Arc::new(RxShared::new());
        let feeder = RxFeeder::new(producer, shared.clone());
        let playout = Playout::new(channels, consumer, shared.clone())?;
        Ok((
            RxSlot {
                feeder: Arc::new(Mutex::new(feeder)),
                shared,
            },
            playout,
        ))
    }

    pub(crate) fn slot(&self, i: usize) -> &RxSlot {
        &self.slots[i]
    }

    pub(crate) fn len(&self) -> usize {
        self.slots.len()
    }

    /// Hands out the playout handle. Panics if it is currently held elsewhere.
    pub(crate) fn take_handle(&self, what: &str) -> PlayoutHandle {
        let inner = self
            .home
            .lock()
            .take()
            .unwrap_or_else(|| panic!("{what}: handle already taken (drop the previous one first)"));
        PlayoutHandle {
            inner: Some(inner),
            home: self.home.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_are_send_and_return_home_on_drop() {
        fn assert_send<T: Send>() {}
        assert_send::<CaptureHandle>();
        assert_send::<PlayoutHandle>();

        let out = OutgoingStream::new(StreamId::Mic, 64_000, 5).unwrap();
        let mut h = out.take_handle("mic");
        assert_eq!(h.channels(), 1);
        h.push(&[0.0; 1000]);
        drop(h);
        let h2 = out.take_handle("mic");
        assert_eq!(h2.channels(), 1);

        let inc = IncomingStream::new(StreamId::PcAudio).unwrap();
        let mut p = inc.take_handle("pc");
        assert_eq!(p.channels(), 2);
        let mut buf = [1f32; 64];
        p.fill(&mut buf);
        assert!(buf.iter().all(|x| *x == 0.0));
        drop(p);
        let _p2 = inc.take_handle("pc");
    }
}
