//! The device landscape as the UI, the service and the default-device fixes see it.
//!
//! "Cable" means the virtual microphone route: VB-CABLE ("CABLE Input" / "CABLE Output") on
//! Windows, AudioBridge's own PipeWire virtual source on Linux. Endpoint ids are WASAPI endpoint
//! ids on Windows and PipeWire `node.name`s on Linux.

/// An active endpoint (playback or recording), as listed for the default-device fixes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EndpointInfo {
    pub id: String,
    pub name: String,
    pub is_cable: bool,
}

/// What the UI and the supervisor need to know about the current device landscape.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceSummary {
    pub default_render_id: Option<String>,
    pub default_render_name: Option<String>,
    /// The default playback device is a VB-CABLE input (loopback would feed the mic back).
    pub default_is_cable: bool,
    pub cable_render_id: Option<String>,
    pub render_endpoints: Vec<EndpointInfo>,
    pub default_capture_id: Option<String>,
    pub default_capture_name: Option<String>,
    /// The default recording device is the virtual mic.
    pub default_capture_is_cable: bool,
    /// The capture endpoint apps record the phone mic from.
    pub cable_capture_id: Option<String>,
    pub capture_endpoints: Vec<EndpointInfo>,
}

impl DeviceSummary {
    /// The playback device to restore when VB-CABLE took over the default: the remembered one
    /// if it is still active, otherwise the first active non-CABLE device.
    pub fn render_restore_candidate(&self, remembered: Option<&str>) -> Option<&EndpointInfo> {
        restore_candidate(&self.render_endpoints, remembered)
    }

    /// The recording device to restore when the virtual mic stops being the default: the
    /// remembered one if it is still active, otherwise the first active non-virtual device.
    pub fn capture_restore_candidate(&self, remembered: Option<&str>) -> Option<&EndpointInfo> {
        restore_candidate(&self.capture_endpoints, remembered)
    }
}

fn restore_candidate<'a>(endpoints: &'a [EndpointInfo], remembered: Option<&str>) -> Option<&'a EndpointInfo> {
    let usable = |r: &&EndpointInfo| !r.is_cable;
    remembered
        .and_then(|id| endpoints.iter().filter(usable).find(|r| r.id == id))
        .or_else(|| endpoints.iter().find(usable))
}
