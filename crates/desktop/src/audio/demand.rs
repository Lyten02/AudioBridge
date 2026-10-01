//! Detects whether any application is recording from VB-CABLE "CABLE Output".

use windows::core::Interface;
use windows::Win32::Media::Audio::{
    AudioSessionStateActive, IAudioSessionControl2, IAudioSessionManager2, IMMDeviceEnumerator,
};
use windows::Win32::System::Com::CLSCTX_ALL;

use super::devices;

#[derive(Default)]
pub struct DemandProbe {
    manager: Option<IAudioSessionManager2>,
}

impl DemandProbe {
    /// Forget the cached endpoint (devices changed).
    pub fn reset(&mut self) {
        self.manager = None;
    }

    /// `Some(true)` if another process has an active capture session on CABLE Output,
    /// `Some(false)` if none (or no VB-CABLE), `None` if the query failed.
    pub fn poll(&mut self, enumerator: &IMMDeviceEnumerator) -> Option<bool> {
        if self.manager.is_none() {
            let Some(ep) = devices::cable_capture(enumerator) else {
                return Some(false);
            };
            // SAFETY: COM call on a valid device.
            match unsafe { ep.device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) } {
                Ok(m) => self.manager = Some(m),
                Err(e) => {
                    tracing::debug!("session manager activation failed: {e}");
                    return None;
                }
            }
        }
        let manager = self.manager.as_ref()?;
        match any_active(manager) {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::debug!("session enumeration failed: {e}");
                self.manager = None;
                None
            }
        }
    }
}

fn any_active(manager: &IAudioSessionManager2) -> windows::core::Result<bool> {
    let own_pid = std::process::id();
    // SAFETY: COM calls on valid interfaces.
    unsafe {
        let sessions = manager.GetSessionEnumerator()?;
        for i in 0..sessions.GetCount()? {
            let control = sessions.GetSession(i)?;
            if control.GetState()? != AudioSessionStateActive {
                continue;
            }
            let pid = control.cast::<IAudioSessionControl2>().and_then(|c| c.GetProcessId()).unwrap_or(0);
            if pid != own_pid {
                return Ok(true);
            }
        }
    }
    Ok(false)
}
