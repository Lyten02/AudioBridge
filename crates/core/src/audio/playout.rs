//! Real-time receive path ("NetEQ-lite"), run inside the platform audio callback:
//!
//! * encoded packets arrive through a lock-free SPSC queue and are kept in a fixed jitter buffer
//!   keyed by sequence number;
//! * frames are decoded on demand, 10 ms at a time: the next packet if present, Opus PLC and
//!   advance if it is missing but newer packets exist (loss), or PLC *without* advancing when the
//!   buffer is empty ("expand"), so nothing is skipped once late packets arrive. After 70 ms of
//!   continuous PLC the output fades to silence; the next real packet fades back in;
//! * the target delay is the 98th percentile of the packets' relative arrival delay (histogram,
//!   30 s half-life) plus one frame — fast to rise, slow to fall;
//! * a PI controller on (playout delay − target) drives a small-ratio Hermite resampler: the
//!   integral absorbs clock skew (≤ 0.2 %), the proportional part (≤ 0.1 %) trims the delay, so
//!   pitch never moves audibly. Large excess is removed by dropping a frame with a
//!   correlation-aligned 5 ms crossfade (WSOLA-lite).
//!
//! No allocation, locking or blocking happens in [`Playout::fill`]; libopus keeps its
//! temporaries on the stack.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use rtrb::Consumer;

use super::codec::Decoder;
use super::rx::{Packet, PktKind, RxShared};
use crate::proto::{FRAME_SAMPLES, MAX_AUDIO_PAYLOAD, SAMPLE_RATE};

const FRAME_MS: f64 = 10.0;
const FRAMES_PER_S: f64 = 1000.0 / FRAME_MS;
const SAMPLES_PER_MS: f64 = SAMPLE_RATE as f64 / 1000.0;
/// Jitter buffer capacity (frames).
const JB_SLOTS: usize = 128;

pub(crate) const TARGET_START_MS: f64 = 30.0;
pub(crate) const TARGET_MIN_MS: f64 = 20.0;
pub(crate) const TARGET_MAX_MS: f64 = 500.0;
/// Delay quantile the target covers.
const QUANTILE: f32 = 0.98;
/// Histogram of relative delays: 1 ms bins, 0..=500 ms.
const HIST_BINS: usize = 501;
/// Per-packet forgetting factor: half-life of 3000 packets (30 s at 100 packets/s).
const HIST_LAMBDA: f32 = 0.999_769;
/// Below this histogram weight the start target is used.
const HIST_MIN_WEIGHT: f32 = 50.0;
/// The minimum one-way delay is taken over 2–4 s (two rotating 2 s buckets).
const BASE_BUCKET_S: f64 = 2.0;

/// Continuous PLC frames before the output fades to silence (the last one fades out).
const EXPAND_FADE_FRAMES: u32 = 7;
/// Fade-in length when audio resumes from silence (2.5 ms).
const FADE_IN: usize = 120;
/// Fade-out length when the stream stops mid-signal (2 ms).
const TAIL_FADE: u32 = 96;
/// A gap between callbacks longer than this means the device was closed: start over.
const STALL_S: f64 = 0.2;
/// After a stall, queued packets older than this are discarded.
const STALE_S: f64 = 0.1;

/// Proportional gain (per frame of error) when the delay is below target (slow down).
const KP_UP: f64 = 0.004;
/// Proportional gain when the delay is above target (speed up gently).
const KP_DOWN: f64 = 0.001;
/// Integral gain (per frame·second). Integrates only within the accelerate threshold, so long
/// outages do not wind it up; it learns the clock skew.
const KI: f64 = 0.0002;
const I_BAND_FRAMES: f64 = ACCEL_MIN_FRAMES;
/// Excess delay below this fraction of the target is kept (no proportional drain): the margin
/// an outage added protects against the next one.
const DRAIN_DEADBAND: f64 = 0.3;
/// Peak level below which a decoded frame counts as digital silence.
const SILENT_LEVEL: f32 = 1e-5;
/// Clamp of the measured minimum-delay drift (frames per second; 0.2 = 0.2 % clock skew).
/// Real sound-card clocks differ by well under 0.05 %.
const MAX_DRIFT: f64 = 0.2;
/// Smoothing of the drift estimate per bucket (≈ 10 buckets = 20 s).
const DRIFT_SMOOTHING: f64 = 0.1;
/// Clock-skew compensation range (integral term). Together with the trim the playback rate
/// never moves more than 0.3 % (5 cents): pitch changes stay inaudible on sustained music.
const MAX_SKEW: f64 = 0.002;
/// Delay-trim range (proportional term). Larger excess is removed by frame drops.
const MAX_TRIM: f64 = 0.001;
/// Smoothing of the delay error.
const ERR_TAU_S: f64 = 0.5;
/// Frame drops start above max(this, target/2) of excess delay.
const ACCEL_MIN_FRAMES: f64 = 6.0;
/// At most one frame drop per this many frames (≤ 10 % faster).
const ACCEL_EVERY: u32 = 10;
/// Crossfade of a frame drop (5 ms), placed after a short lead-in of the first frame.
const XFADE: usize = 240;
const XFADE_LEAD: usize = 120;
/// Lag search range for the crossfade alignment (5 ms).
const ACCEL_MAX_LAG: usize = 240;
const BUFFER_TAU_S: f64 = 0.3;

struct Slot {
    seq: u32,
    valid: bool,
    len: u16,
    data: [u8; MAX_AUDIO_PAYLOAD],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// No stream (never started, or the sender announced silence).
    Idle,
    /// Collecting the first packets of a stream until the target delay is reached.
    Priming,
    Playing,
}

#[inline]
fn seq_diff(a: u32, b: u32) -> i32 {
    a.wrapping_sub(b) as i32
}

pub(crate) struct Playout {
    queue: Consumer<Packet>,
    shared: Arc<RxShared>,
    decoder: Decoder,
    channels: usize,
    origin: Instant,
    jb: Box<[Slot]>,
    phase: Phase,
    next_seq: u32,
    epoch_seq0: u32,
    newest: u32,
    silence_at: Option<u32>,

    // delay statistics
    /// Minimum one-way delay (frames) and when it was observed: current and previous bucket.
    base_cur: (f64, f64),
    base_prev: (f64, f64),
    bucket_start: f64,
    /// Drift of the minimum delay (frames per second), from consecutive bucket minima.
    drift: f64,
    hist: Box<[f32]>,
    hist_total: f32,
    target_ms: f64,
    callback_ms: f64,

    // delay control
    err_f: f64,
    integ: f64,
    since_accel: u32,

    // output
    stage: Box<[f32]>,
    stage_len: usize,
    stage_pos: usize,
    stage_advancing: bool,
    scratch: Box<[f32]>,
    frac: f64,
    hist4: [[f32; 4]; 2],
    plc_run: u32,
    /// Expanded frames not yet paid back by a merge (current episode).
    expand_frames: u32,
    /// Expanded frames in the current episode.
    expand_total: u32,
    /// The current episode ended via a merge (missing packet treated as lost).
    merged: bool,
    /// The last decoded frame was digital silence.
    silent: bool,
    faded: bool,
    last_out: [f32; 2],
    tail_left: u32,
    last_fill: Option<f64>,
    buffer_ms: f64,

    /// Frames played from real packets (accelerated frames included).
    pub(crate) decoded: u64,
    /// Frame drops performed to reduce excess delay.
    pub(crate) accelerated: u64,
}

impl Playout {
    pub(crate) fn new(channels: usize, queue: Consumer<Packet>, shared: Arc<RxShared>) -> Result<Self> {
        assert!(channels == 1 || channels == 2, "1 or 2 channels supported");
        shared
            .target_ms
            .store((TARGET_START_MS as f32).to_bits(), Ordering::Relaxed);
        let jb = (0..JB_SLOTS)
            .map(|_| Slot {
                seq: 0,
                valid: false,
                len: 0,
                data: [0; MAX_AUDIO_PAYLOAD],
            })
            .collect();
        Ok(Self {
            queue,
            shared,
            decoder: Decoder::new(channels)?,
            channels,
            origin: Instant::now(),
            jb,
            phase: Phase::Idle,
            next_seq: 0,
            epoch_seq0: 0,
            newest: 0,
            silence_at: None,
            base_cur: (0.0, 0.0),
            base_prev: (0.0, 0.0),
            bucket_start: 0.0,
            drift: 0.0,
            hist: vec![0.0; HIST_BINS].into_boxed_slice(),
            hist_total: 0.0,
            target_ms: TARGET_START_MS,
            callback_ms: 0.0,
            err_f: 0.0,
            integ: 0.0,
            since_accel: 0,
            stage: vec![0.0; 2 * FRAME_SAMPLES * channels].into_boxed_slice(),
            stage_len: 0,
            stage_pos: 0,
            stage_advancing: false,
            scratch: vec![0.0; 2 * FRAME_SAMPLES * channels].into_boxed_slice(),
            frac: 0.0,
            hist4: [[0.0; 4]; 2],
            plc_run: 0,
            expand_frames: 0,
            expand_total: 0,
            merged: false,
            silent: true,
            faded: true,
            last_out: [0.0; 2],
            tail_left: 0,
            last_fill: None,
            buffer_ms: 0.0,
            decoded: 0,
            accelerated: 0,
        })
    }

    #[cfg(test)]
    pub(crate) fn with_origin(mut self, origin: Instant) -> Self {
        self.origin = origin;
        self
    }

    pub(crate) fn channels(&self) -> usize {
        self.channels
    }

    /// Whether `fill` may currently produce non-silent output.
    pub(crate) fn is_audible(&self) -> bool {
        self.phase == Phase::Playing || self.tail_left > 0
    }

    /// Whether the mixer must run this playout even if it is silent.
    pub(crate) fn needs_service(&self) -> bool {
        self.shared.in_use.load(Ordering::Relaxed)
            || self.shared.flush.load(Ordering::Relaxed)
            || self.is_audible()
            || !self.queue.is_empty()
    }

    pub(crate) fn fill(&mut self, out: &mut [f32]) {
        self.fill_at(out, Instant::now());
    }

    /// [`Playout::fill`] at an explicit time (deterministic tests).
    pub(crate) fn fill_at(&mut self, out: &mut [f32], now: Instant) {
        let ch = self.channels;
        let n = out.len() / ch;
        out[n * ch..].fill(0.0);
        let out = &mut out[..n * ch];
        if n == 0 {
            return;
        }
        let t = now.saturating_duration_since(self.origin).as_secs_f64();
        let mut discard_before = None;
        if self.last_fill.is_some_and(|last| t - last > STALL_S) {
            // the device was not pulling audio: whatever is buffered is stale
            self.stop_stream();
            discard_before = Some(t - STALE_S);
        }
        self.last_fill = Some(t);
        if self.shared.flush.swap(false, Ordering::AcqRel) {
            self.reset_stream();
            while self.queue.pop().is_ok() {}
        }
        if self.drain(t, discard_before) {
            self.update_target();
        }
        let cb = n as f64 / SAMPLES_PER_MS;
        if cb > self.callback_ms {
            self.callback_ms = cb;
        } else {
            self.callback_ms += (cb - self.callback_ms) * 0.01;
        }

        if self.phase == Phase::Priming && self.offset_frames(t) >= self.target_ms / FRAME_MS {
            self.phase = Phase::Playing;
            self.frac = 0.0;
            self.hist4 = [[0.0; 4]; 2];
            self.err_f = 0.0;
            self.stage_len = 0;
            self.stage_pos = 0;
            self.faded = true;
        }
        let mut done = 0;
        if self.phase == Phase::Playing {
            done = self.render(out, t);
        }
        self.output_silence(&mut out[done * ch..]);
        self.publish(n);
    }

    /// Resampled output while playing. Returns frames written (fewer if the stream went idle).
    fn render(&mut self, out: &mut [f32], t: f64) -> usize {
        let ch = self.channels;
        let n = out.len() / ch;
        let ratio = self.ratio(t, n);
        for j in 0..n {
            for c in 0..ch {
                let v = hermite(&self.hist4[c], self.frac as f32);
                out[j * ch + c] = v;
                self.last_out[c] = v;
            }
            self.frac += ratio;
            while self.frac >= 1.0 {
                self.frac -= 1.0;
                if self.stage_pos >= self.stage_len && !self.next_frame() {
                    self.phase = Phase::Idle;
                    self.tail_left = TAIL_FADE;
                    return j + 1;
                }
                let base = self.stage_pos * ch;
                for c in 0..ch {
                    let h = &mut self.hist4[c];
                    h.copy_within(1..4, 0);
                    h[3] = self.stage[base + c];
                }
                self.stage_pos += 1;
            }
        }
        n
    }

    /// Stages the next 10 ms of input. Returns `false` when the stream ended (silence marker).
    fn next_frame(&mut self) -> bool {
        let ch = self.channels;
        let frame = FRAME_SAMPLES * ch;
        self.since_accel = self.since_accel.saturating_add(1);
        self.stage_pos = 0;
        loop {
            if self.has(self.next_seq) {
                if self.should_accelerate() && self.has(self.next_seq.wrapping_add(1)) {
                    self.accelerate();
                } else {
                    let i = self.slot(self.next_seq);
                    let (slot, stage) = (&mut self.jb[i], &mut self.stage[..frame]);
                    self.decoder
                        .decode(Some(&slot.data[..slot.len as usize]), stage);
                    slot.valid = false;
                    self.stage_len = FRAME_SAMPLES;
                    self.next_seq = self.next_seq.wrapping_add(1);
                    self.decoded += 1;
                }
                if self.expand_total > 0 {
                    // the awaited packet came late: that was an outage (unless it was silence)
                    self.end_expand(true);
                }
                let len = self.stage_len;
                self.silent = self.stage[..len * ch].iter().all(|s| s.abs() < SILENT_LEVEL);
                if self.faded {
                    fade(&mut self.stage[..len * ch], ch, FADE_IN, true);
                    self.faded = false;
                }
                self.plc_run = 0;
                self.stage_advancing = true;
                return true;
            }
            // packets before an announced end of stream that are missing now count as lost
            let newer_exists = seq_diff(self.newest, self.next_seq) > 0
                || self
                    .silence_at
                    .is_some_and(|m| seq_diff(m, self.next_seq) > 0);
            if !newer_exists
                && self
                    .silence_at
                    .is_some_and(|m| seq_diff(self.next_seq, m) >= 0)
            {
                self.end_expand(false);
                self.stop_stream();
                return false;
            }
            if newer_exists && self.expand_frames > 0 {
                // merge: a frame already expanded stands in for the missing one
                self.shared.lost.fetch_add(1, Ordering::Relaxed);
                self.next_seq = self.next_seq.wrapping_add(1);
                self.expand_frames -= 1;
                self.merged = true;
                continue;
            }
            if newer_exists {
                // lost (or too late): conceal and move on
                if self.expand_total > 0 {
                    self.end_expand(false);
                }
                self.shared.lost.fetch_add(1, Ordering::Relaxed);
                self.stage_advancing = true;
                self.next_seq = self.next_seq.wrapping_add(1);
            } else {
                // empty buffer: expand without advancing, so nothing is skipped later
                self.expand_frames += 1;
                self.expand_total += 1;
                self.stage_advancing = false;
            }
            self.conceal();
            return true;
        }
    }

    /// An expansion episode ended. It counts as an outage (underrun) if it lasted two frames or
    /// more, or if it ended because the awaited packet arrived late, unless the audio before it
    /// was digital silence (inaudible).
    fn end_expand(&mut self, awaited_arrived: bool) {
        let outage = self.expand_total >= 2 || (awaited_arrived && !self.merged);
        if outage && self.expand_total > 0 && !self.silent {
            self.shared.underruns.fetch_add(1, Ordering::Relaxed);
        }
        self.expand_frames = 0;
        self.expand_total = 0;
        self.merged = false;
    }

    /// One PLC frame; after `EXPAND_FADE_FRAMES` of them, silence.
    fn conceal(&mut self) {
        let ch = self.channels;
        let frame = FRAME_SAMPLES * ch;
        self.stage_len = FRAME_SAMPLES;
        if self.faded {
            self.stage[..frame].fill(0.0);
        } else {
            self.decoder.decode(None, &mut self.stage[..frame]);
            self.shared.concealed.fetch_add(1, Ordering::Relaxed);
            if self.plc_run + 1 >= EXPAND_FADE_FRAMES {
                fade(&mut self.stage[..frame], ch, FRAME_SAMPLES, false);
                self.faded = true;
            }
        }
        self.plc_run = self.plc_run.saturating_add(1);
    }

    fn should_accelerate(&self) -> bool {
        self.since_accel >= ACCEL_EVERY
            && self.plc_run == 0
            && !self.faded
            && self.err_f > ACCEL_MIN_FRAMES.max(0.5 * self.target_ms / FRAME_MS)
    }

    /// Plays two frames as ~1: the second is crossfaded in at the best-matching lag.
    fn accelerate(&mut self) {
        let ch = self.channels;
        let frame = FRAME_SAMPLES * ch;
        let (ia, ib) = (self.slot(self.next_seq), self.slot(self.next_seq.wrapping_add(1)));
        let (a, b) = self.scratch.split_at_mut(frame);
        let sa = &mut self.jb[ia];
        self.decoder.decode(Some(&sa.data[..sa.len as usize]), a);
        sa.valid = false;
        let sb = &mut self.jb[ib];
        self.decoder.decode(Some(&sb.data[..sb.len as usize]), b);
        sb.valid = false;

        // lag k maximizing the normalized correlation of A[lead..lead+xfade] with B[k..k+xfade]
        let mono = |x: &[f32], i: usize| -> f32 { x[i * ch..i * ch + ch].iter().sum() };
        let mut best = (f32::MIN, 0usize);
        for k in 0..=ACCEL_MAX_LAG {
            let (mut xy, mut yy) = (0f32, 1e-9f32);
            for i in 0..XFADE {
                let x = mono(a, XFADE_LEAD + i);
                let y = mono(b, k + i);
                xy += x * y;
                yy += y * y;
            }
            let score = xy / yy.sqrt();
            if score > best.0 {
                best = (score, k);
            }
        }
        let k = best.1;
        let stage = &mut self.stage;
        stage[..XFADE_LEAD * ch].copy_from_slice(&a[..XFADE_LEAD * ch]);
        for i in 0..XFADE {
            let w = (i as f32 + 0.5) / XFADE as f32;
            for c in 0..ch {
                stage[(XFADE_LEAD + i) * ch + c] =
                    a[(XFADE_LEAD + i) * ch + c] * (1.0 - w) + b[(k + i) * ch + c] * w;
            }
        }
        let rest = FRAME_SAMPLES - k - XFADE;
        let at = XFADE_LEAD + XFADE;
        stage[at * ch..(at + rest) * ch].copy_from_slice(&b[(k + XFADE) * ch..]);
        self.stage_len = at + rest;
        self.next_seq = self.next_seq.wrapping_add(2);
        self.decoded += 2;
        self.accelerated += 1;
        self.since_accel = 0;
    }

    /// Drift/delay controller: resampling ratio for this callback.
    fn ratio(&mut self, t: f64, n: usize) -> f64 {
        let dt = n as f64 / SAMPLE_RATE as f64;
        let err = self.offset_frames(t) - self.target_ms / FRAME_MS;
        self.err_f += (err - self.err_f) * (dt / ERR_TAU_S).min(1.0);
        // excess inside the deadband is kept on purpose; neither term removes it
        let deadband = DRAIN_DEADBAND * self.target_ms / FRAME_MS;
        let e = if self.err_f > 0.0 {
            (self.err_f - deadband).max(0.0)
        } else {
            self.err_f
        };
        if self.err_f.abs() < I_BAND_FRAMES.max(2.0 * deadband) {
            self.integ = (self.integ + KI * e * dt).clamp(-MAX_SKEW, MAX_SKEW);
        }
        let kp = if e > 0.0 { KP_DOWN } else { KP_UP };
        1.0 + self.integ + (kp * e).clamp(-MAX_TRIM, MAX_TRIM)
    }

    /// Current playout delay relative to the fastest observed arrival path, in frames.
    fn offset_frames(&self, t: f64) -> f64 {
        let mut pos = seq_diff(self.next_seq, self.epoch_seq0) as f64;
        if self.stage_advancing && self.stage_len > 0 {
            pos -= (self.stage_len - self.stage_pos.min(self.stage_len)) as f64 / FRAME_SAMPLES as f64;
        }
        t * FRAMES_PER_S - pos - self.base_at(t)
    }

    /// Minimum one-way delay at time `t`, extrapolated with the measured drift (the minimum
    /// moves with the sender/receiver clock difference).
    fn base_at(&self, t: f64) -> f64 {
        let at = |(d, a): (f64, f64)| d + self.drift * (t - a);
        at(self.base_cur).min(at(self.base_prev))
    }

    fn base_reset(&mut self, d: f64, a: f64) {
        self.base_cur = (d, a);
        self.base_prev = (d, a);
        self.bucket_start = a;
    }

    fn base_add(&mut self, d: f64, a: f64) {
        if a - self.bucket_start >= BASE_BUCKET_S {
            // the slope between consecutive bucket minima measures the clock drift
            let ((d1, a1), (d0, a0)) = (self.base_cur, self.base_prev);
            if a1 - a0 > 0.5 {
                let slope = ((d1 - d0) / (a1 - a0)).clamp(-MAX_DRIFT, MAX_DRIFT);
                self.drift += (slope - self.drift) * DRIFT_SMOOTHING;
            }
            self.base_prev = self.base_cur;
            self.base_cur = (d, a);
            self.bucket_start = a;
        } else if d < self.base_cur.0 + self.drift * (a - self.base_cur.1) {
            self.base_cur = (d, a);
        }
    }

    /// Moves queued packets into the jitter buffer. Returns whether delay statistics changed.
    fn drain(&mut self, t: f64, discard_before: Option<f64>) -> bool {
        let mut stats = false;
        while let Ok(p) = self.queue.pop() {
            match p.kind {
                PktKind::Reset => {
                    self.reset_stream();
                    stats = true;
                }
                PktKind::Silence => {
                    if self.phase != Phase::Idle {
                        self.silence_at = Some(p.seq);
                    }
                }
                PktKind::Audio => {
                    let a = p.arrival.saturating_duration_since(self.origin).as_secs_f64();
                    if discard_before.is_some_and(|d| a < d) {
                        continue;
                    }
                    self.insert(&p, a.min(t));
                    stats = true;
                }
            }
        }
        stats
    }

    fn insert(&mut self, p: &Packet, a: f64) {
        if self.phase == Phase::Idle {
            self.start_epoch(p.seq, a);
        } else if self.silence_at.is_some_and(|m| seq_diff(p.seq, m) >= 0) {
            // a new talk spurt after a silence announcement: arrival timing starts over
            self.silence_at = None;
            let d = a * FRAMES_PER_S - seq_diff(p.seq, self.epoch_seq0) as f64;
            self.base_reset(d, a);
        }

        // relative delay statistics
        let d = a * FRAMES_PER_S - seq_diff(p.seq, self.epoch_seq0) as f64;
        self.base_add(d, a);
        let rel_ms = (d - self.base_at(a)) * FRAME_MS;
        self.hist_add(rel_ms);

        // placement
        let mut k = seq_diff(p.seq, self.next_seq);
        if k < 0 {
            if self.phase == Phase::Priming && k >= -4 {
                self.next_seq = p.seq;
                k = 0;
            } else {
                self.shared.late.fetch_add(1, Ordering::Relaxed);
                return;
            }
        }
        if k as usize >= JB_SLOTS {
            // far behind (stalled consumer): jump ahead, keeping half a buffer
            let new_next = p.seq.wrapping_sub(JB_SLOTS as u32 / 2);
            self.shared
                .lost
                .fetch_add(seq_diff(new_next, self.next_seq) as u64, Ordering::Relaxed);
            for s in self.jb.iter_mut() {
                if s.valid && seq_diff(s.seq, new_next) < 0 {
                    s.valid = false;
                }
            }
            self.next_seq = new_next;
        }
        let i = self.slot(p.seq);
        let slot = &mut self.jb[i];
        if slot.valid && slot.seq == p.seq {
            return;
        }
        slot.seq = p.seq;
        slot.valid = true;
        slot.len = p.len;
        slot.data[..p.len as usize].copy_from_slice(p.payload());
        if seq_diff(p.seq, self.newest) > 0 {
            self.newest = p.seq;
        }
    }

    fn start_epoch(&mut self, seq: u32, a: f64) {
        self.phase = Phase::Priming;
        self.next_seq = seq;
        self.epoch_seq0 = seq;
        self.newest = seq;
        self.silence_at = None;
        self.base_reset(a * FRAMES_PER_S, a);
        self.stage_len = 0;
        self.stage_pos = 0;
        self.stage_advancing = false;
        self.plc_run = 0;
        self.expand_frames = 0;
        self.expand_total = 0;
        self.merged = false;
        self.faded = true;
    }

    /// Ends the current stream: buffered packets are dropped, delay statistics are kept.
    fn stop_stream(&mut self) {
        if self.phase == Phase::Playing {
            self.tail_left = TAIL_FADE;
        }
        self.phase = Phase::Idle;
        self.silence_at = None;
        self.stage_len = 0;
        self.stage_pos = 0;
        for s in self.jb.iter_mut() {
            s.valid = false;
        }
    }

    /// New stream owner: forget everything learned about the previous one.
    fn reset_stream(&mut self) {
        self.stop_stream();
        self.decoder.reset();
        self.hist.fill(0.0);
        self.hist_total = 0.0;
        self.target_ms = TARGET_START_MS;
        self.integ = 0.0;
        self.drift = 0.0;
        self.err_f = 0.0;
    }

    fn hist_add(&mut self, ms: f64) {
        for b in self.hist.iter_mut() {
            *b *= HIST_LAMBDA;
        }
        self.hist_total = self.hist_total * HIST_LAMBDA + 1.0;
        let bin = (ms.max(0.0).round() as usize).min(HIST_BINS - 1);
        self.hist[bin] += 1.0;
    }

    fn update_target(&mut self) {
        let target = if self.hist_total < HIST_MIN_WEIGHT {
            TARGET_START_MS
        } else {
            let limit = (1.0 - QUANTILE) * self.hist_total;
            let mut acc = 0.0;
            let mut q = 0;
            for (bin, w) in self.hist.iter().enumerate().rev() {
                acc += w;
                if acc > limit {
                    q = bin;
                    break;
                }
            }
            q as f64 + FRAME_MS.max(self.callback_ms) + 2.0
        };
        self.target_ms = target.clamp(TARGET_MIN_MS, TARGET_MAX_MS);
    }

    fn has(&self, seq: u32) -> bool {
        let s = &self.jb[self.slot(seq)];
        s.valid && s.seq == seq
    }

    fn slot(&self, seq: u32) -> usize {
        seq as usize % JB_SLOTS
    }

    /// Silence, starting with a short fade from the last output sample.
    fn output_silence(&mut self, out: &mut [f32]) {
        let ch = self.channels;
        for f in out.chunks_exact_mut(ch) {
            if self.tail_left > 0 {
                let g = self.tail_left as f32 / TAIL_FADE as f32;
                self.tail_left -= 1;
                for (c, s) in f.iter_mut().enumerate() {
                    *s = self.last_out[c] * g;
                }
            } else {
                f.fill(0.0);
            }
        }
    }

    fn publish(&mut self, n: usize) {
        let ahead_ms = if self.phase == Phase::Idle {
            0.0
        } else {
            let mut ahead = seq_diff(self.newest.wrapping_add(1), self.next_seq) as f64;
            if self.stage_advancing {
                ahead += (self.stage_len - self.stage_pos.min(self.stage_len)) as f64
                    / FRAME_SAMPLES as f64;
            }
            ahead.max(0.0) * FRAME_MS
        };
        let alpha = (n as f64 / SAMPLE_RATE as f64 / BUFFER_TAU_S).min(1.0);
        self.buffer_ms += (ahead_ms - self.buffer_ms) * alpha;
        self.shared
            .buffer_ms
            .store((self.buffer_ms as f32).to_bits(), Ordering::Relaxed);
        self.shared
            .target_ms
            .store((self.target_ms as f32).to_bits(), Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn target(&self) -> f64 {
        self.target_ms
    }
}

/// Linear fade over the first (`fade_in`) or last (`!fade_in`) `len` frames of `x`.
fn fade(x: &mut [f32], ch: usize, len: usize, fade_in: bool) {
    let frames = x.len() / ch;
    let len = len.min(frames);
    for i in 0..len {
        let g = (i as f32 + 0.5) / len as f32;
        let (idx, g) = if fade_in {
            (i, g)
        } else {
            (frames - len + i, 1.0 - g)
        };
        for c in 0..ch {
            x[idx * ch + c] *= g;
        }
    }
}

/// 4-point, 3rd-order Hermite interpolation between `x[1]` and `x[2]`.
#[inline]
fn hermite(x: &[f32; 4], t: f32) -> f32 {
    let c0 = x[1];
    let c1 = 0.5 * (x[2] - x[0]);
    let c2 = x[0] - 2.5 * x[1] + 2.0 * x[2] - 0.5 * x[3];
    let c3 = 0.5 * (x[3] - x[0]) + 1.5 * (x[1] - x[2]);
    ((c3 * t + c2) * t + c1) * t + c0
}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;
    use std::time::Duration;

    use super::*;
    use crate::audio::rx::{RxFeeder, PACKET_QUEUE};
    use crate::audio::sim::{arrivals, encoded_sine, Arrival, Metrics, Recorder, Trace};
    use crate::proto::{DatagramHeader, PacketKind, StreamId};

    const TEST_FRAMES: usize = 6000;

    fn packets() -> &'static [Vec<u8>] {
        static P: OnceLock<Vec<Vec<u8>>> = OnceLock::new();
        P.get_or_init(|| encoded_sine(2, TEST_FRAMES, 440.0))
    }

    /// Feeder + playout driven by a simulated clock.
    struct Sim {
        feeder: RxFeeder,
        pl: Playout,
        shared: Arc<RxShared>,
        t0: Instant,
        out: Vec<f32>,
        cb: usize,
        tick: u64,
        rec: Recorder,
        record: bool,
    }

    impl Sim {
        /// `cb`: callback size in frames (480 = 10 ms WASAPI-like, 192 = 4 ms AAudio-like).
        fn new(cb: usize) -> Self {
            let t0 = Instant::now();
            let (p, c) = rtrb::RingBuffer::new(PACKET_QUEUE);
            let shared = Arc::new(RxShared::new());
            Self {
                feeder: RxFeeder::new(p, shared.clone()),
                pl: Playout::new(2, c, shared.clone()).unwrap().with_origin(t0),
                shared,
                t0,
                out: vec![0.0; cb * 2],
                cb,
                tick: 0,
                rec: Recorder::default(),
                record: false,
            }
        }

        fn at(&self, ms: f64) -> Instant {
            self.t0 + Duration::from_secs_f64(ms / 1000.0)
        }

        fn now_ms(&self) -> f64 {
            self.tick as f64 * self.cb as f64 / 48.0
        }

        fn deliver(&mut self, a: Arrival) {
            let hdr = DatagramHeader {
                kind: PacketKind::Audio,
                stream: StreamId::PcAudio,
                seq: a.seq,
            };
            let pkt = &packets()[a.seq as usize % TEST_FRAMES];
            self.feeder.on_packet(hdr, pkt, self.at(a.at_ms));
        }

        fn marker(&mut self, seq: u32) {
            let hdr = DatagramHeader {
                kind: PacketKind::Silence,
                stream: StreamId::PcAudio,
                seq,
            };
            let now = self.at(self.now_ms());
            self.feeder.on_packet(hdr, &[], now);
        }

        fn step(&mut self) {
            let now = self.at(self.now_ms());
            self.pl.fill_at(&mut self.out, now);
            if self.record {
                self.rec.audio(&self.out, 2);
                self.rec.buffer(f32::from_bits(self.shared.buffer_ms.load(Ordering::Relaxed)) as f64);
            }
            self.tick += 1;
        }

        /// Runs until `until_ms`, delivering `arr` (sorted) on time.
        fn run(&mut self, arr: &[Arrival], next: &mut usize, until_ms: f64) {
            while self.now_ms() < until_ms {
                while *next < arr.len() && arr[*next].at_ms <= self.now_ms() {
                    self.deliver(arr[*next]);
                    *next += 1;
                }
                self.step();
            }
        }

        fn counters(&self) -> (u64, u64, u64, u64) {
            let s = &self.shared;
            (
                s.underruns.load(Ordering::Relaxed),
                s.concealed.load(Ordering::Relaxed),
                s.lost.load(Ordering::Relaxed),
                s.late.load(Ordering::Relaxed),
            )
        }
    }

    /// Plays `trace` for 60 s; metrics cover the time after `warmup_s`.
    fn run_trace(trace: Trace, cb: usize, warmup_s: f64) -> (Metrics, Sim) {
        // a little more than 60 s of packets, so the stream does not end inside the window
        let arr = arrivals(trace, 62.0, 7);
        let mut sim = Sim::new(cb);
        let mut next = 0;
        sim.run(&arr, &mut next, warmup_s * 1000.0);
        let before = sim.counters();
        sim.record = true;
        sim.run(&arr, &mut next, 60_000.0);
        let after = sim.counters();
        let rec = std::mem::take(&mut sim.rec);
        let m = rec.finish(Metrics {
            underruns: after.0 - before.0,
            concealed: after.1 - before.1,
            lost: after.2 - before.2,
            late: after.3 - before.3,
            ..Metrics::default()
        });
        (m, sim)
    }

    #[test]
    fn jitter_is_absorbed_without_gaps() {
        let (m, sim) = run_trace(Trace::Jitter { max_ms: 30.0 }, 480, 20.0);
        println!("a jitter 0-30 ms: {m:?} target {:.1} ms", sim.pl.target());
        assert_eq!(m.underruns, 0);
        assert_eq!(m.zero_runs, 0, "no exact-zero gaps once the target covers the jitter");
        assert!(m.late <= 40, "late {}", m.late);
        assert!(m.mean_buf_ms < 60.0 && m.p95_buf_ms < 75.0, "{m:?}");
        assert!((25.0..=60.0).contains(&sim.pl.target()), "target {}", sim.pl.target());
    }

    #[test]
    fn power_save_bursts_adapt_and_skip_nothing() {
        let (m, sim) = run_trace(Trace::PowerSave, 192, 30.0);
        println!("b power-save bursts: {m:?} target {:.1} ms", sim.pl.target());
        assert!(m.underruns <= 5, "underruns after adaptation: {}", m.underruns);
        assert_eq!(m.lost + m.late, 0, "held packets must all be played");
        assert!(m.mean_buf_ms < 350.0, "{m:?}");
        let (_, _, lost, late) = sim.counters();
        assert_eq!(lost + late, 0, "nothing skipped during adaptation either");
    }

    #[test]
    fn random_loss_is_concealed() {
        let (m, _) = run_trace(Trace::Loss { pct: 2.0 }, 480, 10.0);
        println!("c 2% loss: {m:?}");
        assert_eq!(m.underruns, 0);
        assert_eq!(m.zero_runs, 0);
        // 50 s at 2 % ≈ 100 lost frames, each concealed once
        assert!((60..=150).contains(&m.lost), "lost {}", m.lost);
        assert_eq!(m.concealed, m.lost);
    }

    #[test]
    fn clock_skew_keeps_the_buffer_bounded() {
        // a few times the skew of real sound-card clocks (≤ 0.05 %)
        for skew in [0.0015, -0.0015] {
            let (m, sim) = run_trace(Trace::Skew { skew }, 480, 20.0);
            println!("d skew {skew:+}: {m:?} target {:.1} ms", sim.pl.target());
            assert!(m.underruns <= 1, "skew {skew}: {m:?}");
            assert_eq!(m.zero_runs, 0, "skew {skew}: {m:?}");
            assert!(m.max_buf_ms < 80.0, "skew {skew}: {m:?}");
            assert!(m.mean_buf_ms > 5.0, "skew {skew}: {m:?}");
        }
    }

    #[test]
    fn outage_expands_then_resumes_without_skipping() {
        // steady 0–3 ms jitter for 25 s (mature statistics); packets due during 25.0–25.3 s are
        // held and released at once
        let total = 3200u32;
        let mut arr: Vec<Arrival> = (0..total)
            .map(|seq| {
                let natural = seq as f64 * 10.0 + 2.0 + (seq % 3) as f64;
                let at_ms = if (25_000.0..25_300.0).contains(&natural) { 25_300.0 } else { natural };
                Arrival { seq, at_ms }
            })
            .collect();
        arr.sort_by(|a, b| a.at_ms.total_cmp(&b.at_ms));
        let mut sim = Sim::new(480);
        let mut next = 0;
        sim.run(&arr, &mut next, 24_900.0);
        assert_eq!(sim.counters(), (0, 0, 0, 0));
        sim.record = true;
        sim.run(&arr, &mut next, 31_000.0);
        let (underruns, concealed, lost, late) = sim.counters();
        let m = std::mem::take(&mut sim.rec).finish(Metrics::default());
        println!(
            "outage 300 ms: underruns {underruns} concealed {concealed} lost {lost} late {late} \
             accelerated {} target {:.1} {m:?}",
            sim.pl.accelerated,
            sim.pl.target()
        );
        assert_eq!(underruns, 1);
        assert_eq!(concealed, EXPAND_FADE_FRAMES as u64, "PLC, then silence");
        assert_eq!(lost + late, 0, "late packets are played, not skipped");
        assert_eq!(m.zero_runs, 1);
        // every delivered packet was either decoded exactly once or is still buffered
        let played = sim.pl.decoded;
        let buffered = seq_diff(sim.pl.newest.wrapping_add(1), sim.pl.next_seq) as u64;
        assert_eq!(played + buffered, next as u64);
        // a single outage in a mature history does not move the target; its delay is drained
        assert!(sim.pl.target() < 40.0, "target {}", sim.pl.target());
        assert!(sim.pl.accelerated > 0);
        let buf = f32::from_bits(sim.shared.buffer_ms.load(Ordering::Relaxed));
        assert!(buf < 60.0, "buffer after recovery {buf}");
    }

    #[test]
    fn silence_marker_idles_and_resumes() {
        let mut sim = Sim::new(480);
        let arr: Vec<Arrival> = (0..100u32)
            .map(|seq| Arrival { seq, at_ms: seq as f64 * 10.0 + 3.0 })
            .collect();
        let mut next = 0;
        sim.run(&arr, &mut next, 1001.0);
        // the sender announces silence right after its last (silent) frame
        sim.marker(100);
        sim.run(&[], &mut 0, 3000.0);
        assert!(
            !sim.pl.is_audible(),
            "phase {:?} next {} newest {} silence_at {:?}",
            sim.pl.phase,
            sim.pl.next_seq,
            sim.pl.newest,
            sim.pl.silence_at
        );
        assert_eq!(sim.counters().0, 0, "announced silence is not an underrun");
        // resume 2 s later with the next sequence number
        let arr: Vec<Arrival> = (100..200u32)
            .map(|seq| Arrival { seq, at_ms: 3000.0 + (seq - 100) as f64 * 10.0 + 3.0 })
            .collect();
        sim.record = true;
        let mut next = 0;
        sim.run(&arr, &mut next, 4000.0);
        assert!(sim.pl.is_audible());
        assert_eq!(sim.counters(), (0, 0, 0, 0));
        assert!(sim.pl.target() < 40.0, "silence gap must not inflate the target");
    }

    #[test]
    fn late_packet_is_dropped_and_counted() {
        let mut sim = Sim::new(480);
        let mut arr: Vec<Arrival> = (0..200u32)
            .filter(|s| *s != 100)
            .map(|seq| Arrival { seq, at_ms: seq as f64 * 10.0 + 3.0 })
            .collect();
        arr.push(Arrival { seq: 100, at_ms: 1500.0 });
        arr.sort_by(|a, b| a.at_ms.total_cmp(&b.at_ms));
        let mut next = 0;
        sim.run(&arr, &mut next, 1950.0);
        let (underruns, concealed, lost, late) = sim.counters();
        assert_eq!((underruns, concealed, lost, late), (0, 1, 1, 1));
    }

    #[test]
    fn reset_and_stall_start_over() {
        let mut sim = Sim::new(480);
        let arr: Vec<Arrival> = (0..100u32)
            .map(|seq| Arrival { seq, at_ms: seq as f64 * 10.0 + 3.0 })
            .collect();
        let mut next = 0;
        sim.run(&arr, &mut next, 500.0);
        assert!(sim.pl.is_audible());
        // new owner: sequence numbers start elsewhere
        sim.feeder.reset();
        let arr: Vec<Arrival> = (0..100u32)
            .map(|i| Arrival { seq: 5000 + i, at_ms: 500.0 + i as f64 * 10.0 + 3.0 })
            .collect();
        let mut next = 0;
        sim.run(&arr, &mut next, 1000.0);
        assert!(sim.pl.is_audible());
        assert_eq!(sim.counters(), (0, 0, 0, 0));
        // device closed for 1 s while packets kept arriving: stale audio is discarded
        let t = sim.now_ms();
        while next < arr.len() {
            sim.deliver(arr[next]);
            next += 1;
        }
        sim.tick += (1000.0 * 48.0 / sim.cb as f64) as u64;
        let more: Vec<Arrival> = (100..150u32)
            .map(|i| Arrival { seq: 5000 + i, at_ms: t + 1000.0 + (i - 100) as f64 * 10.0 })
            .collect();
        let mut next = 0;
        let end = sim.now_ms() + 400.0;
        sim.run(&more, &mut next, end);
        assert!(sim.pl.is_audible());
        let buf = f32::from_bits(sim.shared.buffer_ms.load(Ordering::Relaxed));
        assert!(buf < 60.0, "stale packets must not add delay: {buf}");
    }
}
