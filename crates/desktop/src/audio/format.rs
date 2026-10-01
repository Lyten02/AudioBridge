//! Wave format helpers, sample decoding and a streaming linear resampler.
//! The resampler/decoder are only used if the engine refuses AUTOCONVERTPCM.

use anyhow::{bail, Result};
use windows::Win32::Media::Audio::{WAVEFORMATEX, WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE_0};
use windows::Win32::Media::KernelStreaming::{KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE};
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};

const WAVE_FORMAT_PCM: u16 = 1;

/// 48 kHz 32-bit float, `channels` channels.
pub fn float_format(rate: u32, channels: u16) -> WAVEFORMATEXTENSIBLE {
    let block_align = channels * 4;
    let mask = match channels {
        1 => 0x4,  // FRONT_CENTER
        2 => 0x3,  // FRONT_LEFT | FRONT_RIGHT
        n => (1u32 << n.min(18)) - 1,
    };
    WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_EXTENSIBLE as u16,
            nChannels: channels,
            nSamplesPerSec: rate,
            nAvgBytesPerSec: rate * block_align as u32,
            nBlockAlign: block_align,
            wBitsPerSample: 32,
            cbSize: (size_of::<WAVEFORMATEXTENSIBLE>() - size_of::<WAVEFORMATEX>()) as u16,
        },
        Samples: WAVEFORMATEXTENSIBLE_0 { wValidBitsPerSample: 32 },
        dwChannelMask: mask,
        SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleKind {
    F32,
    I16,
    I24,
    I32,
}

#[derive(Clone, Copy, Debug)]
pub struct StreamFormat {
    pub rate: u32,
    pub channels: usize,
    pub kind: SampleKind,
    pub block_align: usize,
}

impl StreamFormat {
    /// # Safety
    /// `wfx` must point to a valid WAVEFORMATEX (or WAVEFORMATEXTENSIBLE when tagged so).
    pub unsafe fn parse(wfx: *const WAVEFORMATEX) -> Result<Self> {
        let f = unsafe { std::ptr::read_unaligned(wfx) };
        let tag = f.wFormatTag;
        let bits = f.wBitsPerSample;
        let channels = f.nChannels as usize;
        let block_align = f.nBlockAlign as usize;
        let rate = f.nSamplesPerSec;
        let is_float = if u32::from(tag) == WAVE_FORMAT_EXTENSIBLE {
            let ext = unsafe { std::ptr::read_unaligned(wfx.cast::<WAVEFORMATEXTENSIBLE>()) };
            let sub = ext.SubFormat;
            if sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
                true
            } else if sub == KSDATAFORMAT_SUBTYPE_PCM {
                false
            } else {
                bail!("unsupported sub-format {sub:?}");
            }
        } else if u32::from(tag) == WAVE_FORMAT_IEEE_FLOAT {
            true
        } else if tag == WAVE_FORMAT_PCM {
            false
        } else {
            bail!("unsupported format tag {tag}");
        };
        if channels == 0 || block_align == 0 {
            bail!("invalid format");
        }
        let container = block_align / channels;
        let kind = match (is_float, container, bits) {
            (true, 4, _) => SampleKind::F32,
            (false, 2, _) => SampleKind::I16,
            (false, 3, _) => SampleKind::I24,
            (false, 4, _) => SampleKind::I32,
            _ => bail!("unsupported sample layout: float={is_float} container={container} bits={bits}"),
        };
        Ok(Self { rate, channels, kind, block_align })
    }

    /// Decodes `frames` frames from raw device bytes, appending interleaved f32.
    pub fn decode(&self, bytes: &[u8], out: &mut Vec<f32>) {
        match self.kind {
            SampleKind::F32 => out.extend(bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))),
            SampleKind::I16 => {
                out.extend(bytes.chunks_exact(2).map(|b| f32::from(i16::from_le_bytes([b[0], b[1]])) / 32768.0))
            }
            SampleKind::I24 => out.extend(
                bytes
                    .chunks_exact(3)
                    .map(|b| (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8_388_608.0),
            ),
            SampleKind::I32 => out.extend(
                bytes
                    .chunks_exact(4)
                    .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0),
            ),
        }
    }

    /// Encodes interleaved f32 samples into raw device bytes (`dst.len()` must match).
    pub fn encode(&self, samples: &[f32], dst: &mut [u8]) {
        match self.kind {
            SampleKind::F32 => {
                for (s, d) in samples.iter().zip(dst.chunks_exact_mut(4)) {
                    d.copy_from_slice(&s.to_le_bytes());
                }
            }
            SampleKind::I16 => {
                for (s, d) in samples.iter().zip(dst.chunks_exact_mut(2)) {
                    d.copy_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
                }
            }
            SampleKind::I24 => {
                for (s, d) in samples.iter().zip(dst.chunks_exact_mut(3)) {
                    let v = ((s.clamp(-1.0, 1.0) * 8_388_607.0) as i32).to_le_bytes();
                    d.copy_from_slice(&v[..3]);
                }
            }
            SampleKind::I32 => {
                for (s, d) in samples.iter().zip(dst.chunks_exact_mut(4)) {
                    d.copy_from_slice(&((f64::from(s.clamp(-1.0, 1.0)) * 2_147_483_647.0) as i32).to_le_bytes());
                }
            }
        }
    }
}

/// Streaming linear-interpolation resampler for interleaved audio.
pub struct Resampler {
    channels: usize,
    /// Input frames advanced per output frame (in_rate / out_rate).
    step: f64,
    /// Position of the next output frame; 0.0 == `last`, 1.0 == first frame of the next chunk.
    pos: f64,
    last: Vec<f32>,
}

impl Resampler {
    pub fn new(in_rate: u32, out_rate: u32, channels: usize) -> Self {
        Self { channels, step: f64::from(in_rate) / f64::from(out_rate), pos: 1.0, last: vec![0.0; channels] }
    }

    /// Consumes all of `input` (interleaved) and appends the resampled frames to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let ch = self.channels;
        let n = input.len() / ch;
        if n == 0 {
            return;
        }
        let last = &self.last;
        let frame = |i: usize, c: usize| if i == 0 { last[c] } else { input[(i - 1) * ch + c] };
        let mut pos = self.pos;
        while pos < n as f64 {
            let i = pos as usize;
            let t = (pos - i as f64) as f32;
            for c in 0..ch {
                let a = frame(i, c);
                let b = frame(i + 1, c);
                out.push(a + (b - a) * t);
            }
            pos += self.step;
        }
        self.pos = pos - n as f64;
        self.last.copy_from_slice(&input[(n - 1) * ch..n * ch]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampler_preserves_rate_and_continuity() {
        let mut r = Resampler::new(44_100, 48_000, 1);
        let input: Vec<f32> = (0..44_100).map(|i| i as f32 / 44_100.0).collect();
        let mut out = Vec::new();
        for chunk in input.chunks(441) {
            r.process(chunk, &mut out);
        }
        assert!((out.len() as i64 - 48_000).abs() <= 2, "got {}", out.len());
        // A ramp stays monotonic across chunk boundaries.
        assert!(out.windows(2).all(|w| w[1] >= w[0]));
    }
}
