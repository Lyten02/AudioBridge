//! Volume of the default playback device: read and set through `IAudioEndpointVolume`. Windows
//! pushes every change (ours, the volume flyout, media keys) to `IAudioEndpointVolumeCallback`,
//! which only wakes the supervisor; nothing is polled.

use std::sync::mpsc::Sender;

use audiobridge_core::proto::MAX_LEVEL;
use audiobridge_core::session::Volume;
use windows::core::HSTRING;
use windows::Win32::Media::Audio::Endpoints::{
    IAudioEndpointVolume, IAudioEndpointVolumeCallback, IAudioEndpointVolumeCallback_Impl,
};
use windows::Win32::Media::Audio::{IMMDeviceEnumerator, AUDIO_VOLUME_NOTIFICATION_DATA};
use windows::Win32::System::Com::CLSCTX_ALL;

use super::AudioMsg;

#[windows_core::implement(IAudioEndpointVolumeCallback)]
struct VolumeNotifier {
    tx: Sender<AudioMsg>,
}

impl IAudioEndpointVolumeCallback_Impl for VolumeNotifier_Impl {
    fn OnNotify(&self, _data: *mut AUDIO_VOLUME_NOTIFICATION_DATA) -> windows::core::Result<()> {
        let _ = self.tx.send(AudioMsg::VolumeChanged);
        Ok(())
    }
}

/// The volume control of one playback endpoint with a registered change callback.
struct Watch {
    volume: IAudioEndpointVolume,
    callback: IAudioEndpointVolumeCallback,
}

impl Watch {
    fn open(e: &IMMDeviceEnumerator, device_id: &str, tx: Sender<AudioMsg>) -> windows::core::Result<Self> {
        let id = HSTRING::from(device_id);
        // SAFETY: COM calls on a valid enumerator/device; COM is initialised on this thread.
        let volume: IAudioEndpointVolume = unsafe { e.GetDevice(&id)?.Activate(CLSCTX_ALL, None)? };
        let callback: IAudioEndpointVolumeCallback = VolumeNotifier { tx }.into();
        // SAFETY: COM call on a valid interface; unregistered in Drop, and `callback` is kept
        // alive until then.
        unsafe { volume.RegisterControlChangeNotify(&callback)? };
        Ok(Self { volume, callback })
    }

    fn read(&self) -> windows::core::Result<Volume> {
        // SAFETY: COM calls on a valid interface.
        let (scalar, muted) = unsafe { (self.volume.GetMasterVolumeLevelScalar()?, self.volume.GetMute()?) };
        let level = (scalar.clamp(0.0, 1.0) * f32::from(MAX_LEVEL)).round() as u8;
        Ok(Volume { level, muted: muted.as_bool() })
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        // SAFETY: matches the registration in `open`; the callback is still alive.
        unsafe {
            let _ = self.volume.UnregisterControlChangeNotify(&self.callback);
        }
    }
}

/// Follows the default playback device for the supervisor and applies volume requests to it.
pub struct VolumeControl {
    tx: Sender<AudioMsg>,
    /// The device the watch belongs to (or failed to open for).
    device_id: Option<String>,
    watch: Option<Watch>,
    /// The last value handed out by [`VolumeControl::take_change`].
    reported: Option<Option<Volume>>,
}

impl VolumeControl {
    /// `tx` receives `AudioMsg::VolumeChanged` whenever the watched volume changes.
    pub fn new(tx: Sender<AudioMsg>) -> Self {
        Self { tx, device_id: None, watch: None, reported: None }
    }

    /// Moves the change callback to `default_render_id` if the default device changed (or the
    /// last attempt to open it failed). Call on startup and after device changes.
    pub fn follow(&mut self, e: &IMMDeviceEnumerator, default_render_id: Option<&String>) {
        if self.device_id.as_ref() == default_render_id && (self.watch.is_some() || default_render_id.is_none()) {
            return;
        }
        // Dropping the old watch unregisters its callback.
        self.watch = None;
        self.device_id = default_render_id.cloned();
        if let Some(id) = default_render_id {
            match Watch::open(e, id, self.tx.clone()) {
                Ok(w) => self.watch = Some(w),
                Err(err) => tracing::warn!("volume control of the default playback device unavailable: {err}"),
            }
        }
    }

    pub fn set_level(&self, level: u8) {
        let Some(w) = &self.watch else {
            tracing::warn!("volume request ignored: no default playback device");
            return;
        };
        let scalar = f32::from(level.min(MAX_LEVEL)) / f32::from(MAX_LEVEL);
        // SAFETY: COM call on a valid interface; a null event context is allowed.
        if let Err(e) = unsafe { w.volume.SetMasterVolumeLevelScalar(scalar, std::ptr::null()) } {
            tracing::warn!("setting the playback volume failed: {e}");
        }
    }

    pub fn set_mute(&self, muted: bool) {
        let Some(w) = &self.watch else {
            tracing::warn!("mute request ignored: no default playback device");
            return;
        };
        // SAFETY: COM call on a valid interface; a null event context is allowed.
        if let Err(e) = unsafe { w.volume.SetMute(muted, std::ptr::null()) } {
            tracing::warn!("setting the playback mute failed: {e}");
        }
    }

    /// Reads the current volume (`None` without a usable default device) and returns it if it
    /// differs from the last returned value.
    pub fn take_change(&mut self) -> Option<Option<Volume>> {
        let now = match self.watch.as_ref().map(Watch::read) {
            Some(Ok(v)) => Some(v),
            Some(Err(e)) => {
                // Typically an invalidated endpoint; the next device change reopens it.
                tracing::warn!("reading the playback volume failed: {e}");
                self.watch = None;
                None
            }
            None => None,
        };
        if self.reported == Some(now) {
            return None;
        }
        self.reported = Some(now);
        Some(now)
    }
}
