//! Plays the phone microphone (48 kHz mono) into the VB-CABLE "CABLE Input" render endpoint.

use std::collections::VecDeque;

use anyhow::{bail, Context, Result};
use audiobridge_core::audio::PlayoutHandle;
use windows::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Media::Audio::{
    IAudioClient, IAudioRenderClient, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX,
};
use windows::Win32::System::Com::{CoTaskMemFree, CLSCTX_ALL};
use windows::Win32::System::Threading::WaitForMultipleObjects;

use super::com::{self, Com, Event, MmcssGuard};
use super::devices;
use super::format::{float_format, Resampler, StreamFormat};

const RATE: u32 = 48_000;
/// 20 ms engine buffer: two 10 ms periods, the minimum that is robust in shared mode.
const BUFFER_HNS: i64 = 200_000;

/// Device-side layout: channel count, sample encoding, and an optional rate converter.
struct Output {
    format: StreamFormat,
    resampler: Option<Resampler>,
    /// Mono samples at device rate waiting to be written (fallback path only).
    pending: VecDeque<f32>,
    pull: Vec<f32>,
    resampled: Vec<f32>,
}

impl Output {
    /// Produces `frames` mono samples at device rate into `mono`.
    fn produce(&mut self, playout: &mut PlayoutHandle, frames: usize, mono: &mut Vec<f32>) {
        mono.clear();
        match &mut self.resampler {
            None => {
                mono.resize(frames, 0.0);
                playout.fill(mono);
            }
            Some(rs) => {
                while self.pending.len() < frames {
                    self.pull.resize(480, 0.0);
                    playout.fill(&mut self.pull);
                    self.resampled.clear();
                    rs.process(&self.pull, &mut self.resampled);
                    self.pending.extend(self.resampled.iter().copied());
                }
                mono.extend(self.pending.drain(..frames));
            }
        }
    }
}

fn activate(device: &windows::Win32::Media::Audio::IMMDevice) -> Result<IAudioClient> {
    // SAFETY: COM call on a valid device.
    Ok(unsafe { device.Activate(CLSCTX_ALL, None)? })
}

/// Runs until `stop` is signalled (Ok) or the device fails (Err).
pub fn run(playout: &mut PlayoutHandle, stop: &Event) -> Result<()> {
    let _com = Com::init();
    let enumerator = com::enumerator()?;
    let endpoint = devices::cable_render(&enumerator).context("VB-CABLE \"CABLE Input\" not found")?;
    let mut client = activate(&endpoint.device)?;
    // SAFETY: the mix format pointer is valid until CoTaskMemFree.
    let mix_channels = unsafe {
        let mix = client.GetMixFormat()?;
        let ch = std::ptr::read_unaligned(mix).nChannels;
        CoTaskMemFree(Some(mix as *const _));
        ch.max(1)
    };
    let base = AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
    let want = float_format(RATE, mix_channels);
    // SAFETY: `want` outlives the call.
    let init = unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            base | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            BUFFER_HNS,
            0,
            (&raw const want).cast::<WAVEFORMATEX>(),
            None,
        )
    };
    let mut out = match init {
        Ok(()) => Output {
            // SAFETY: `want` is a valid format.
            format: unsafe { StreamFormat::parse((&raw const want).cast())? },
            resampler: None,
            pending: VecDeque::new(),
            pull: Vec::new(),
            resampled: Vec::new(),
        },
        Err(e) => {
            tracing::warn!("mic render AUTOCONVERTPCM init failed ({e}); using the mix format");
            client = activate(&endpoint.device)?;
            // SAFETY: as above.
            let format = unsafe {
                let mix = client.GetMixFormat()?;
                let parsed = StreamFormat::parse(mix);
                let r = client.Initialize(AUDCLNT_SHAREMODE_SHARED, base, BUFFER_HNS, 0, mix, None);
                CoTaskMemFree(Some(mix as *const _));
                let f = parsed?;
                r?;
                f
            };
            Output {
                format,
                resampler: (format.rate != RATE).then(|| Resampler::new(RATE, format.rate, 1)),
                pending: VecDeque::with_capacity(4096),
                pull: Vec::with_capacity(480),
                resampled: Vec::with_capacity(1024),
            }
        }
    };
    let event = Event::new()?;
    // SAFETY: COM calls on an initialised client.
    let (writer, buffer_frames): (IAudioRenderClient, u32) = unsafe {
        client.SetEventHandle(event.handle())?;
        (client.GetService()?, client.GetBufferSize()?)
    };
    // SAFETY: starting an initialised client.
    unsafe { client.Start()? };
    let _mmcss = MmcssGuard::pro_audio();
    tracing::info!("mic render started on \"{}\" ({} ch)", endpoint.name, out.format.channels);

    let result = pump(&client, &writer, buffer_frames, playout, stop, &event, &mut out);
    // SAFETY: stopping an initialised client.
    unsafe {
        let _ = client.Stop();
    }
    tracing::info!("mic render stopped");
    result
}

fn pump(
    client: &IAudioClient,
    writer: &IAudioRenderClient,
    buffer_frames: u32,
    playout: &mut PlayoutHandle,
    stop: &Event,
    event: &Event,
    out: &mut Output,
) -> Result<()> {
    let handles = [stop.handle(), event.handle()];
    let ch = out.format.channels;
    let mut mono = Vec::with_capacity(buffer_frames as usize);
    let mut wide = Vec::with_capacity(buffer_frames as usize * ch);
    loop {
        // SAFETY: both handles are valid for the duration of the call.
        let w = unsafe { WaitForMultipleObjects(&handles, false, 500) };
        if w == WAIT_OBJECT_0 {
            return Ok(());
        }
        if w != WAIT_TIMEOUT && w.0 != WAIT_OBJECT_0.0 + 1 {
            bail!("wait failed: {w:?}");
        }
        // SAFETY: GetBuffer/ReleaseBuffer are paired and the buffer is valid for `avail` frames.
        unsafe {
            let padding = client.GetCurrentPadding()?;
            let avail = buffer_frames.saturating_sub(padding) as usize;
            if avail == 0 {
                continue;
            }
            out.produce(playout, avail, &mut mono);
            let data = writer.GetBuffer(avail as u32)?;
            let dst = std::slice::from_raw_parts_mut(data, avail * out.format.block_align);
            if ch == 1 {
                out.format.encode(&mono, dst);
            } else {
                wide.clear();
                for &s in &mono {
                    wide.extend(std::iter::repeat_n(s, ch));
                }
                out.format.encode(&wide, dst);
            }
            writer.ReleaseBuffer(avail as u32, 0)?;
        }
    }
}
