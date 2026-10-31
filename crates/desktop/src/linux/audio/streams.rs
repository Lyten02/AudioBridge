//! The two PipeWire streams: the loopback capture of the default sink's monitor (PC audio → phone)
//! and the virtual microphone source (phone mic → PC apps).
//!
//! Both process callbacks run on PipeWire's real-time data thread. The core handles live in
//! `Arc<Mutex<Option<_>>>` slots owned by the engine, so they survive stream re-creation; the
//! data thread only ever `try_lock`s them (never waits) and the engine never locks them while a
//! stream is connected. Neither callback allocates.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use audiobridge_core::audio::{CaptureHandle, PlayoutHandle};
use pipewire as pw;
use pw::core::CoreRc;
use pw::properties::properties;
use pw::spa;
use pw::spa::param::audio::{AudioFormat, AudioInfoRaw, MAX_CHANNELS};
use pw::spa::pod::{serialize::PodSerializer, Object, Pod, Value};
use pw::spa::utils::Direction;
use pw::stream::{StreamFlags, StreamListener, StreamRc, StreamState};

use super::{MIC_NODE_DESCRIPTION, MIC_NODE_NAME};

const RATE: u32 = 48_000;
/// 10 ms, the core's frame: asks the graph for a short quantum (the graph may still pick more).
const LATENCY: &str = "480/48000";
/// Largest block handled per callback (samples); bigger buffers are processed in pieces.
const SCRATCH: usize = 8192;

pub type CaptureSlot = Arc<Mutex<Option<CaptureHandle>>>;
pub type MicSlot = Arc<Mutex<Option<PlayoutHandle>>>;

/// `EnumFormat`: F32 interleaved at 48 kHz with the given channel positions; PipeWire's adapter
/// converts from/to whatever the graph runs at.
fn format_param(positions: &[u32]) -> Result<Vec<u8>> {
    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::F32LE);
    info.set_rate(RATE);
    info.set_channels(positions.len() as u32);
    let mut position = [0; MAX_CHANNELS];
    position[..positions.len()].copy_from_slice(positions);
    info.set_position(position);
    let object = Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    let (cursor, _) = PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(object))
        .context("serializing the stream format")?;
    Ok(cursor.into_inner())
}

/// A connected stream; disconnects (synchronously with the data thread) before its callbacks'
/// state is freed.
struct Connected<D: 'static> {
    process: Option<StreamListener<D>>,
    state: Option<StreamListener<()>>,
    stream: StreamRc,
}

impl<D> Drop for Connected<D> {
    fn drop(&mut self) {
        if let Err(e) = self.stream.disconnect() {
            tracing::debug!("stream disconnect: {e}");
        }
        self.process.take();
        self.state.take();
    }
}

struct CaptureData {
    slot: CaptureSlot,
    scratch: Vec<f32>,
}

/// Loopback capture of the default sink's monitor (`stream.capture.sink`). Without a target the
/// session manager links it to the default sink and moves it when the default changes.
pub struct Capture {
    _connected: Connected<CaptureData>,
}

impl Capture {
    pub fn connect(core: &CoreRc, slot: CaptureSlot, on_state: impl Fn(StreamState) + 'static) -> Result<Self> {
        let stream = StreamRc::new(
            core.clone(),
            "audiobridge-capture",
            properties! {
                *pw::keys::MEDIA_TYPE => "Audio",
                *pw::keys::MEDIA_CATEGORY => "Capture",
                *pw::keys::MEDIA_ROLE => "Music",
                *pw::keys::STREAM_CAPTURE_SINK => "true",
                *pw::keys::NODE_NAME => "audiobridge_capture",
                *pw::keys::NODE_DESCRIPTION => "AudioBridge: звук компьютера → телефон",
                *pw::keys::NODE_LATENCY => LATENCY,
                *pw::keys::APP_NAME => "AudioBridge",
            },
        )
        .context("creating the capture stream")?;
        let data = CaptureData { slot, scratch: vec![0.0; SCRATCH] };
        let process = stream
            .add_local_listener_with_user_data(data)
            .process(|stream, data| {
                let Some(mut buffer) = stream.dequeue_buffer() else { return };
                let Some(d) = buffer.datas_mut().first_mut() else { return };
                let (offset, size) = (d.chunk().offset() as usize, d.chunk().size() as usize);
                let Some(bytes) = d.data() else { return };
                let end = offset.saturating_add(size).min(bytes.len());
                let bytes = &bytes[offset.min(end)..end];
                let Ok(mut guard) = data.slot.try_lock() else { return };
                let Some(handle) = guard.as_mut() else { return };
                for block in bytes.chunks(SCRATCH * 4) {
                    let n = block.len() / 4;
                    for (dst, src) in data.scratch[..n].iter_mut().zip(block.as_chunks::<4>().0) {
                        *dst = f32::from_le_bytes(*src);
                    }
                    handle.push(&data.scratch[..n]);
                }
            })
            .register()
            .context("capture stream listener")?;
        let state = stream
            .add_local_listener_with_user_data(())
            .state_changed(move |_, _, _, new| on_state(new))
            .register()
            .context("capture state listener")?;
        let param = format_param(&[spa::sys::SPA_AUDIO_CHANNEL_FL, spa::sys::SPA_AUDIO_CHANNEL_FR])?;
        let mut params = [Pod::from_bytes(&param).context("format pod")?];
        let connected = Connected { process: Some(process), state: Some(state), stream };
        connected
            .stream
            .connect(
                Direction::Input,
                None,
                StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
                &mut params,
            )
            .context("connecting the capture stream")?;
        Ok(Self { _connected: connected })
    }
}

struct MicData {
    slot: MicSlot,
    scratch: Vec<f32>,
}

/// The virtual microphone: an `Audio/Source` node (marked `node.virtual`) apps record from like
/// from any mic. Not `Audio/Source/Virtual`: WirePlumber treats that class as a null-sink style
/// source and configures *input* ports on it, which an output stream can't take (a mono one
/// never finishes, which stalls all of WirePlumber's linking).
/// It is not linked by us; the graph runs it (and calls `process`) only while something records.
/// The node disappears with the stream, and with the process if it dies.
pub struct VirtualMic {
    _connected: Connected<MicData>,
}

impl VirtualMic {
    pub fn connect(core: &CoreRc, slot: MicSlot, on_state: impl Fn(StreamState) + 'static) -> Result<Self> {
        let stream = StreamRc::new(
            core.clone(),
            "audiobridge-mic",
            properties! {
                "media.class" => "Audio/Source",
                *pw::keys::NODE_VIRTUAL => "true",
                // This is a network-fed source, not an autonomous leaf producer. PipeWire 1.6
                // can run an ungrouped leaf just because its previous driver is still active
                // (e.g. unrelated playback), activating idle passive filters as a side effect.
                // A dedicated link group keeps scheduling demand-driven; do not share it with
                // our PC-audio capture, which has independent consumers.
                "node.link-group" => "audiobridge.mic",
                *pw::keys::NODE_NAME => MIC_NODE_NAME,
                *pw::keys::NODE_DESCRIPTION => MIC_NODE_DESCRIPTION,
                *pw::keys::NODE_LATENCY => LATENCY,
                *pw::keys::AUDIO_CHANNELS => "1",
                "audio.position" => "MONO",
                *pw::keys::APP_NAME => "AudioBridge",
                "device.icon-name" => "audio-input-microphone",
            },
        )
        .context("creating the virtual microphone")?;
        let data = MicData { slot, scratch: vec![0.0; SCRATCH] };
        let process = stream
            .add_local_listener_with_user_data(data)
            .process(|stream, data| {
                let Some(mut buffer) = stream.dequeue_buffer() else { return };
                let requested = buffer.requested() as usize;
                let Some(d) = buffer.datas_mut().first_mut() else { return };
                let Some(bytes) = d.data() else { return };
                let max = (bytes.len() / 4).min(SCRATCH);
                let frames = if requested > 0 { requested.min(max) } else { max.min(480) };
                let out = &mut data.scratch[..frames];
                match data.slot.try_lock() {
                    Ok(mut guard) => match guard.as_mut() {
                        Some(handle) => handle.fill(out),
                        None => out.fill(0.0),
                    },
                    Err(_) => out.fill(0.0),
                }
                for (dst, s) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(out.iter()) {
                    *dst = s.to_le_bytes();
                }
                let chunk = d.chunk_mut();
                *chunk.offset_mut() = 0;
                *chunk.stride_mut() = 4;
                *chunk.size_mut() = (frames * 4) as u32;
            })
            .register()
            .context("virtual microphone listener")?;
        let state = stream
            .add_local_listener_with_user_data(())
            .state_changed(move |_, _, _, new| on_state(new))
            .register()
            .context("virtual microphone state listener")?;
        let param = format_param(&[spa::sys::SPA_AUDIO_CHANNEL_MONO])?;
        let mut params = [Pod::from_bytes(&param).context("format pod")?];
        let connected = Connected { process: Some(process), state: Some(state), stream };
        connected
            .stream
            .connect(Direction::Output, None, StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS, &mut params)
            .context("connecting the virtual microphone")?;
        Ok(Self { _connected: connected })
    }
}
