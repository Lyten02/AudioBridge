//! Deterministic arrival traces and output metrics for receive-path tests (simulated clock).

use crate::audio::codec::Encoder;
use crate::proto::{FRAME_SAMPLES, SAMPLE_RATE};

/// Small deterministic PRNG (64-bit LCG).
pub(crate) struct Lcg(u64);

impl Lcg {
    pub(crate) fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407))
    }

    /// Uniform in [0, 1).
    pub(crate) fn next_f64(&mut self) -> f64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }

    pub(crate) fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.next_f64()
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Trace {
    /// Uniform random extra delay in [0, max_ms].
    Jitter { max_ms: f64 },
    /// Wi-Fi power save: every ~1 s all packets are held for 100–250 ms, then released at once.
    PowerSave,
    /// Random loss (`pct` percent) on top of 0–10 ms jitter.
    Loss { pct: f64 },
    /// Sender clock runs `skew` faster (positive) or slower, 0–5 ms jitter.
    Skew { skew: f64 },
}

/// One packet arrival: sequence number and receive time (ms since start).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Arrival {
    pub(crate) seq: u32,
    pub(crate) at_ms: f64,
}

/// Arrivals for `seconds` of 10 ms frames under `trace`, sorted by arrival time.
pub(crate) fn arrivals(trace: Trace, seconds: f64, seed: u64) -> Vec<Arrival> {
    let mut rng = Lcg::new(seed);
    let n = (seconds * 100.0) as u32;
    let mut out = Vec::with_capacity(n as usize);
    // power-save holds: (start, end) windows
    let mut holds = Vec::new();
    if let Trace::PowerSave = trace {
        let mut t = 500.0;
        while t < seconds * 1000.0 + 1000.0 {
            let h = rng.range(100.0, 250.0);
            holds.push((t, t + h));
            t += rng.range(800.0, 1200.0);
        }
    }
    for seq in 0..n {
        let send = match trace {
            Trace::Skew { skew } => seq as f64 * 10.0 / (1.0 + skew),
            _ => seq as f64 * 10.0,
        };
        let natural = match trace {
            Trace::Jitter { max_ms } => send + 5.0 + rng.range(0.0, max_ms),
            Trace::PowerSave => send + 5.0 + rng.range(0.0, 3.0),
            Trace::Loss { pct } => {
                let jitter = rng.range(0.0, 10.0);
                if rng.next_f64() * 100.0 < pct {
                    continue;
                }
                send + 5.0 + jitter
            }
            Trace::Skew { .. } => send + 5.0 + rng.range(0.0, 5.0),
        };
        let at_ms = holds
            .iter()
            .find(|(s, e)| natural >= *s && natural < *e)
            .map(|(_, e)| *e + seq as f64 * 1e-6)
            .unwrap_or(natural);
        out.push(Arrival { seq, at_ms });
    }
    out.sort_by(|a, b| a.at_ms.total_cmp(&b.at_ms));
    out
}

/// Encoded 10 ms frames of a stereo (or mono) sine, index = seq.
pub(crate) fn encoded_sine(channels: usize, frames: usize, freq: f64) -> Vec<Vec<u8>> {
    let mut enc = Encoder::new(channels, 128_000, 5).unwrap();
    let mut pcm = vec![0f32; FRAME_SAMPLES * channels];
    let mut buf = [0u8; 1500];
    let step = freq / SAMPLE_RATE as f64 * std::f64::consts::TAU;
    let mut phase = 0f64;
    (0..frames)
        .map(|_| {
            for f in pcm.chunks_exact_mut(channels) {
                f.fill((phase.sin() * 0.5) as f32);
                phase += step;
            }
            let n = enc.encode(&pcm, &mut buf).unwrap();
            buf[..n].to_vec()
        })
        .collect()
}

/// Output-side measurements of one simulated run.
#[derive(Debug, Default)]
pub(crate) struct Metrics {
    pub(crate) underruns: u64,
    pub(crate) concealed: u64,
    pub(crate) lost: u64,
    pub(crate) late: u64,
    /// Runs of ≥ 0.5 ms exact zeros after the first audible sample.
    pub(crate) zero_runs: u64,
    pub(crate) zero_ms: f64,
    pub(crate) mean_buf_ms: f64,
    pub(crate) p95_buf_ms: f64,
    pub(crate) max_buf_ms: f64,
}

/// Collects zero runs (first channel) and buffer samples.
#[derive(Default)]
pub(crate) struct Recorder {
    started: bool,
    zero_len: usize,
    zero_runs: u64,
    zero_samples: u64,
    bufs: Vec<f64>,
}

impl Recorder {
    pub(crate) fn audio(&mut self, out: &[f32], channels: usize) {
        for f in out.chunks_exact(channels) {
            if f[0] != 0.0 {
                self.started = true;
                self.end_run();
            } else if self.started {
                self.zero_len += 1;
            }
        }
    }

    fn end_run(&mut self) {
        if self.zero_len >= 24 {
            self.zero_runs += 1;
            self.zero_samples += self.zero_len as u64;
        }
        self.zero_len = 0;
    }

    pub(crate) fn buffer(&mut self, ms: f64) {
        self.bufs.push(ms);
    }

    pub(crate) fn finish(mut self, mut m: Metrics) -> Metrics {
        self.end_run();
        m.zero_runs = self.zero_runs;
        m.zero_ms = self.zero_samples as f64 / 48.0;
        if !self.bufs.is_empty() {
            m.mean_buf_ms = self.bufs.iter().sum::<f64>() / self.bufs.len() as f64;
            self.bufs.sort_by(f64::total_cmp);
            m.p95_buf_ms = self.bufs[self.bufs.len() * 95 / 100];
            m.max_buf_ms = *self.bufs.last().unwrap();
        }
        m
    }
}
