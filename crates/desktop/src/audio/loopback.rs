//! Event-driven WASAPI loopback capture of the default playback device → 48 kHz f32 stereo.

use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use audiobridge_core::audio::CaptureHandle;
use windows::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Media::Audio::{
    IAudioCaptureClient, IAudioClient, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX,
};
use windows::Win32::System::Com::{CoTaskMemFree, CLSCTX_ALL};
use windows::Win32::System::Threading::{WaitForMultipleObjects, INFINITE};

use super::com::{self, Com, Event, MmcssGuard};
use super::devices;
use super::format::{float_format, Resampler, StreamFormat};

const RATE: u32 = 48_000;
/// Engine-side buffer. Only bounds how late we may wake; latency is set by the 10 ms engine period.
const BUFFER_HNS: i64 = 500_000;
/// After playback stops (no packets at all), keep feeding digital silence for this long so the
/// sender emits its silence marker, then sleep until the engine delivers audio again.
const GAP_FILL: Duration = Duration::from_millis(400);
/// Packets normally arrive every ~10 ms with a few ms of jitter (11.7 ms measured); only a gap
/// this long means the engine stopped delivering because nothing is playing.
const GAP_DETECT: Duration = Duration::from_millis(50);
const GAP_TICK: Duration = Duration::from_millis(10);

/// Decides when to feed silence after the loopback engine stops delivering packets.
/// Zeros are only pushed once playback has really stopped, never in the middle of a stream:
/// a late packet must not turn into an inserted hole plus surplus audio downstream.
struct GapFiller {
    last_data: Instant,
    filled_to: Option<Instant>,
    filled: Duration,
}

impl GapFiller {
    fn new(now: Instant) -> Self {
        Self { last_data: now, filled_to: None, filled: Duration::ZERO }
    }

    fn on_data(&mut self, now: Instant) {
        self.last_data = now;
        self.filled_to = None;
        self.filled = Duration::ZERO;
    }

    /// Silence to push now that a wait returned without packets.
    fn on_idle(&mut self, now: Instant) -> Duration {
        if self.filled >= GAP_FILL || now - self.last_data < GAP_DETECT {
            return Duration::ZERO;
        }
        // the stop is detected late; filling starts now rather than back-filling the gap
        let Some(from) = self.filled_to else {
            self.filled_to = Some(now);
            return Duration::ZERO;
        };
        let d = (now - from).min(GAP_FILL - self.filled);
        if d < GAP_TICK {
            return Duration::ZERO;
        }
        self.filled += d;
        self.filled_to = Some(now);
        d
    }

    /// Wait timeout: tick while silence may still be due, sleep once it has been fed.
    fn timeout_ms(&self) -> u32 {
        if self.filled >= GAP_FILL {
            INFINITE
        } else {
            GAP_TICK.as_millis() as u32
        }
    }
}

/// Conversion used when the engine refuses AUTOCONVERTPCM.
struct Fallback {
    format: StreamFormat,
    resampler: Option<Resampler>,
    decoded: Vec<f32>,
    stereo: Vec<f32>,
}

impl Fallback {
    fn push(&mut self, bytes: &[u8], capture: &mut CaptureHandle) {
        self.decoded.clear();
        self.format.decode(bytes, &mut self.decoded);
        let ch = self.format.channels;
        self.stereo.clear();
        for f in self.decoded.chunks_exact(ch) {
            let (l, r) = if ch == 1 { (f[0], f[0]) } else { (f[0], f[1]) };
            self.stereo.push(l);
            self.stereo.push(r);
        }
        match &mut self.resampler {
            Some(rs) => {
                self.decoded.clear();
                rs.process(&self.stereo, &mut self.decoded);
                capture.push(&self.decoded);
            }
            None => capture.push(&self.stereo),
        }
    }
}

fn activate(device: &windows::Win32::Media::Audio::IMMDevice) -> Result<IAudioClient> {
    // SAFETY: COM call on a valid device.
    Ok(unsafe { device.Activate(CLSCTX_ALL, None)? })
}

/// Runs until `stop` is signalled (Ok) or the device fails (Err).
pub fn run(capture: &mut CaptureHandle, stop: &Event) -> Result<()> {
    let _com = Com::init();
    let enumerator = com::enumerator()?;
    let endpoint = devices::default_render(&enumerator).context("no default playback device")?;
    if devices::is_cable_render_name(&endpoint.name) {
        bail!("default playback device is VB-CABLE; refusing to capture it");
    }
    let base = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
    let want = float_format(RATE, 2);
    let mut client = activate(&endpoint.device)?;
    let mut fallback = None;
    // SAFETY: `want` is a valid WAVEFORMATEXTENSIBLE that outlives the call.
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
    if let Err(e) = init {
        tracing::warn!("loopback AUTOCONVERTPCM init failed ({e}); using the mix format");
        client = activate(&endpoint.device)?;
        // SAFETY: the mix format pointer is valid until CoTaskMemFree.
        unsafe {
            let mix = client.GetMixFormat()?;
            let parsed = StreamFormat::parse(mix);
            let r = client.Initialize(AUDCLNT_SHAREMODE_SHARED, base, BUFFER_HNS, 0, mix, None);
            CoTaskMemFree(Some(mix as *const _));
            let format = parsed?;
            r?;
            fallback = Some(Fallback {
                format,
                resampler: (format.rate != RATE).then(|| Resampler::new(format.rate, RATE, 2)),
                decoded: Vec::with_capacity(16_384),
                stereo: Vec::with_capacity(16_384),
            });
        }
    }
    let event = Event::new()?;
    // SAFETY: COM calls on an initialised client.
    let reader: IAudioCaptureClient = unsafe {
        client.SetEventHandle(event.handle())?;
        let reader = client.GetService()?;
        client.Start()?;
        reader
    };
    let _mmcss = MmcssGuard::pro_audio();
    tracing::info!("loopback capture started on \"{}\"", endpoint.name);

    let result = pump(&reader, capture, stop, &event, fallback.as_mut());
    // SAFETY: stopping an initialised client.
    unsafe {
        let _ = client.Stop();
    }
    tracing::info!("loopback capture stopped");
    result
}

fn pump(
    reader: &IAudioCaptureClient,
    capture: &mut CaptureHandle,
    stop: &Event,
    event: &Event,
    mut fallback: Option<&mut Fallback>,
) -> Result<()> {
    let zeros = [0.0f32; 2 * 480];
    let handles = [stop.handle(), event.handle()];
    let mut gap = GapFiller::new(Instant::now());
    loop {
        let timeout = gap.timeout_ms();
        // SAFETY: both handles are valid for the duration of the call.
        let w = unsafe { WaitForMultipleObjects(&handles, false, timeout) };
        if w == WAIT_OBJECT_0 {
            return Ok(());
        }
        if w != WAIT_TIMEOUT && w.0 != WAIT_OBJECT_0.0 + 1 {
            bail!("wait failed: {w:?}");
        }
        let mut got = 0usize;
        // SAFETY: GetBuffer/ReleaseBuffer are paired; `data` is valid for `frames` frames until release.
        unsafe {
            loop {
                if reader.GetNextPacketSize()? == 0 {
                    break;
                }
                let mut data = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                reader.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                let n = frames as usize;
                let silent = flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;
                match fallback.as_deref_mut() {
                    _ if silent => push_zeros(capture, &zeros, n),
                    None => capture.push(std::slice::from_raw_parts(data.cast::<f32>(), n * 2)),
                    Some(fb) => {
                        let bytes = std::slice::from_raw_parts(data, n * fb.format.block_align);
                        fb.push(bytes, capture);
                    }
                }
                reader.ReleaseBuffer(frames)?;
                got += n;
            }
        }
        let now = Instant::now();
        if got > 0 {
            gap.on_data(now);
        } else {
            let silence = gap.on_idle(now);
            if !silence.is_zero() {
                push_zeros(capture, &zeros, (silence.as_secs_f64() * f64::from(RATE)) as usize);
            }
        }
    }
}

fn push_zeros(capture: &mut CaptureHandle, zeros: &[f32], frames: usize) {
    let mut left = frames * 2;
    while left > 0 {
        let n = left.min(zeros.len());
        capture.push(&zeros[..n]);
        left -= n;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(t0: Instant, v: u64) -> Instant {
        t0 + Duration::from_millis(v)
    }

    #[test]
    fn late_packets_while_playing_insert_no_silence() {
        let t0 = Instant::now();
        let mut g = GapFiller::new(t0);
        // packet intervals seen on a real loopback device, plus an occasional 30 ms hiccup;
        // the wait times out (and `on_idle` runs) every millisecond in between
        let mut t = 0;
        for i in 0..500 {
            let step = [10, 12, 10, 11, 30][i % 5];
            for k in 1..step {
                assert_eq!(g.on_idle(ms(t0, t + k)), Duration::ZERO, "hole at {t}+{k} ms");
            }
            t += step;
            g.on_data(ms(t0, t));
        }
    }

    #[test]
    fn stopped_playback_is_fed_silence_then_sleeps() {
        let t0 = Instant::now();
        let mut g = GapFiller::new(t0);
        g.on_data(t0);
        let mut total = Duration::ZERO;
        let mut t = 0;
        while t < 2000 {
            t += 10;
            total += g.on_idle(ms(t0, t));
        }
        assert_eq!(total, GAP_FILL, "exactly GAP_FILL of silence, so the sender's marker goes out");
        assert_eq!(g.timeout_ms(), INFINITE);
        // audio resumes: the next stop is fed again
        g.on_data(ms(t0, t));
        assert_ne!(g.timeout_ms(), INFINITE);
        assert_eq!(g.on_idle(ms(t0, t + 40)), Duration::ZERO);
    }
}
