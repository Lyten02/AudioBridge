//! Real-time mixer over a fixed set of per-peer playouts (each with its own jitter buffer and
//! drift compensation). Slots are allocated once; peers are attached/detached through atomics,
//! so `fill` never allocates, locks or frees.

use super::playout::Playout;

/// Samples mixed per inner pass (a multiple of 1 and 2 channels).
const SCRATCH_SAMPLES: usize = 2048;
/// Soft limiter knee: below it the signal passes untouched.
const KNEE: f32 = 0.8;

pub(crate) struct Mixer {
    playouts: Vec<Playout>,
    scratch: Vec<f32>,
    channels: usize,
}

impl Mixer {
    pub(crate) fn new(channels: usize, playouts: Vec<Playout>) -> Self {
        debug_assert!(playouts.iter().all(|p| p.channels() == channels));
        Self {
            playouts,
            scratch: vec![0.0; SCRATCH_SAMPLES],
            channels,
        }
    }

    pub(crate) fn channels(&self) -> usize {
        self.channels
    }

    pub(crate) fn fill(&mut self, out: &mut [f32]) {
        let ch = self.channels;
        let usable = out.len() / ch * ch;
        out[usable..].fill(0.0);
        let now = std::time::Instant::now();
        for chunk in out[..usable].chunks_mut(SCRATCH_SAMPLES) {
            chunk.fill(0.0);
            let scratch = &mut self.scratch[..chunk.len()];
            let mut sources = 0;
            for p in self.playouts.iter_mut().filter(|p| p.needs_service()) {
                p.fill_at(scratch, now);
                if p.is_audible() || scratch.iter().any(|s| *s != 0.0) {
                    sources += 1;
                    for (o, s) in chunk.iter_mut().zip(scratch.iter()) {
                        *o += *s;
                    }
                }
            }
            if sources > 1 {
                for s in chunk.iter_mut() {
                    *s = soft_limit(*s);
                }
            }
        }
    }
}

/// Transparent below the knee, smoothly saturating to ±1 above it (no hard clipping).
#[inline]
pub(crate) fn soft_limit(x: f32) -> f32 {
    let a = x.abs();
    if a <= KNEE {
        x
    } else {
        let range = 1.0 - KNEE;
        (KNEE + range * ((a - KNEE) / range).tanh()).copysign(x)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limiter_is_continuous_and_bounded() {
        assert_eq!(soft_limit(0.5), 0.5);
        assert_eq!(soft_limit(-0.8), -0.8);
        assert!((soft_limit(0.8001) - 0.8001).abs() < 1e-3);
        for x in [1.0f32, 1.5, 3.0, 100.0] {
            let y = soft_limit(x);
            assert!(y <= 1.0 && y > KNEE, "{x} -> {y}");
            assert_eq!(soft_limit(-x), -y);
        }
        let mut prev = soft_limit(0.0);
        for i in 1..4000 {
            let y = soft_limit(i as f32 * 0.001);
            assert!(y >= prev, "monotonic");
            prev = y;
        }
    }
}
