//! Thin safe wrappers over the vendored native libopus (`audiobridge-opus-sys`). libopus keeps
//! its temporaries on the stack, so `encode`/`decode` never allocate: both are safe to call on
//! real-time audio threads.

use anyhow::{bail, Result};
use audiobridge_opus_sys as op;

use crate::proto::{FRAME_SAMPLES, SAMPLE_RATE};

/// Opus encoder for 10 ms frames in RESTRICTED_LOWDELAY (CELT-only, 2.5 ms lookahead) mode.
pub(crate) struct Encoder {
    st: *mut op::OpusEncoder,
    channels: usize,
}

// SAFETY: the encoder state is exclusively owned and only accessed through `&mut self`.
unsafe impl Send for Encoder {}

impl Encoder {
    pub(crate) fn new(channels: usize, bitrate: i32, complexity: i32) -> Result<Self> {
        let mut err = 0i32;
        // SAFETY: plain constructor call with valid arguments; result checked below.
        let st = unsafe {
            op::opus_encoder_create(
                SAMPLE_RATE as i32,
                channels as i32,
                op::OPUS_APPLICATION_RESTRICTED_LOWDELAY,
                &mut err,
            )
        };
        if st.is_null() || err != op::OPUS_OK {
            bail!("opus_encoder_create failed: {err}");
        }
        let enc = Self { st, channels };
        // SAFETY: `st` is a valid encoder.
        unsafe {
            ctl_ok(op::opus_encoder_ctl(enc.st, op::OPUS_SET_BITRATE_REQUEST, bitrate))?;
            ctl_ok(op::opus_encoder_ctl(enc.st, op::OPUS_SET_COMPLEXITY_REQUEST, complexity))?;
            ctl_ok(op::opus_encoder_ctl(enc.st, op::OPUS_SET_VBR_REQUEST, 1i32))?;
            ctl_ok(op::opus_encoder_ctl(
                enc.st,
                op::OPUS_SET_EXPERT_FRAME_DURATION_REQUEST,
                op::OPUS_FRAMESIZE_10_MS,
            ))?;
        }
        Ok(enc)
    }

    pub(crate) fn channels(&self) -> usize {
        self.channels
    }

    /// Encodes exactly one 10 ms frame (`FRAME_SAMPLES * channels` samples). Returns bytes written.
    pub(crate) fn encode(&mut self, pcm: &[f32], out: &mut [u8]) -> Result<usize> {
        debug_assert_eq!(pcm.len(), FRAME_SAMPLES * self.channels);
        // SAFETY: buffers are valid for the given lengths; the encoder is valid.
        let n = unsafe {
            op::opus_encode_float(
                self.st,
                pcm.as_ptr(),
                FRAME_SAMPLES as i32,
                out.as_mut_ptr(),
                out.len() as i32,
            )
        };
        if n < 0 {
            bail!("opus_encode_float failed: {n}");
        }
        Ok(n as usize)
    }

    /// Resets the codec state (used when a new connection starts).
    pub(crate) fn reset(&mut self) {
        // SAFETY: valid encoder.
        unsafe {
            op::opus_encoder_ctl(self.st, op::OPUS_RESET_STATE);
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: created by opus_encoder_create and destroyed exactly once.
        unsafe { op::opus_encoder_destroy(self.st) }
    }
}

/// Opus decoder with packet-loss concealment.
pub(crate) struct Decoder {
    st: *mut op::OpusDecoder,
    channels: usize,
}

// SAFETY: exclusively owned state, accessed through `&mut self` only.
unsafe impl Send for Decoder {}

impl Decoder {
    pub(crate) fn new(channels: usize) -> Result<Self> {
        let mut err = 0i32;
        // SAFETY: plain constructor call; result checked.
        let st = unsafe { op::opus_decoder_create(SAMPLE_RATE as i32, channels as i32, &mut err) };
        if st.is_null() || err != op::OPUS_OK {
            bail!("opus_decoder_create failed: {err}");
        }
        Ok(Self { st, channels })
    }

    /// Decodes one packet into `out` (`FRAME_SAMPLES * channels`). `None` runs PLC.
    /// On a corrupt packet the frame is concealed instead.
    pub(crate) fn decode(&mut self, packet: Option<&[u8]>, out: &mut [f32]) {
        debug_assert_eq!(out.len(), FRAME_SAMPLES * self.channels);
        let (ptr, len) = match packet {
            Some(p) => (p.as_ptr(), p.len() as i32),
            None => (std::ptr::null(), 0),
        };
        // SAFETY: valid decoder; `out` holds FRAME_SAMPLES frames.
        let n = unsafe {
            op::opus_decode_float(self.st, ptr, len, out.as_mut_ptr(), FRAME_SAMPLES as i32, 0)
        };
        if n < 0 {
            if packet.is_some() {
                self.decode(None, out);
            } else {
                out.fill(0.0);
            }
        } else if (n as usize) < FRAME_SAMPLES {
            out[n as usize * self.channels..].fill(0.0);
        }
    }

    pub(crate) fn reset(&mut self) {
        // SAFETY: valid decoder.
        unsafe {
            op::opus_decoder_ctl(self.st, op::OPUS_RESET_STATE);
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: created by opus_decoder_create and destroyed exactly once.
        unsafe { op::opus_decoder_destroy(self.st) }
    }
}

fn ctl_ok(ret: i32) -> Result<()> {
    if ret != op::OPUS_OK {
        bail!("opus ctl failed: {ret}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_sine() {
        let mut enc = Encoder::new(2, 192_000, 9).unwrap();
        let mut dec = Decoder::new(2).unwrap();
        let mut pcm = vec![0f32; FRAME_SAMPLES * 2];
        let mut out = vec![0f32; FRAME_SAMPLES * 2];
        let mut pkt = [0u8; 1500];
        let mut energy = 0.0f64;
        for f in 0..50 {
            for i in 0..FRAME_SAMPLES {
                let t = (f * FRAME_SAMPLES + i) as f32 / SAMPLE_RATE as f32;
                let v = (t * 1000.0 * std::f32::consts::TAU).sin() * 0.5;
                pcm[2 * i] = v;
                pcm[2 * i + 1] = v;
            }
            let n = enc.encode(&pcm, &mut pkt).unwrap();
            assert!(n > 10 && n < 600, "unexpected packet size {n}");
            dec.decode(Some(&pkt[..n]), &mut out);
            if f >= 10 {
                energy += out.iter().map(|x| (*x as f64).powi(2)).sum::<f64>();
            }
        }
        let rms = (energy / (40.0 * FRAME_SAMPLES as f64 * 2.0)).sqrt();
        assert!((rms - 0.3535).abs() < 0.05, "rms {rms}");
        // PLC produces output without a packet
        dec.decode(None, &mut out);
        enc.reset();
        dec.reset();
    }
}
