//! Endpoint enumeration and VB-CABLE matching.

use windows::core::PWSTR;
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, EDataFlow, IMMDevice, IMMDeviceEnumerator, DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Com::{CoTaskMemFree, STGM_READ};

pub struct Endpoint {
    pub id: String,
    pub name: String,
    pub device: IMMDevice,
}

/// An active playback endpoint, as listed for the "restore default device" fix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderInfo {
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
    pub cable_capture_present: bool,
    pub render_endpoints: Vec<RenderInfo>,
}

impl DeviceSummary {
    /// The playback device to restore when VB-CABLE took over the default: the remembered one
    /// if it is still active, otherwise the first active non-CABLE device.
    pub fn restore_candidate(&self, remembered: Option<&str>) -> Option<&RenderInfo> {
        let usable = |r: &&RenderInfo| !r.is_cable;
        remembered
            .and_then(|id| self.render_endpoints.iter().filter(usable).find(|r| r.id == id))
            .or_else(|| self.render_endpoints.iter().find(usable))
    }
}

fn take_pwstr(p: PWSTR) -> String {
    // SAFETY: `p` was allocated by COM and is NUL-terminated; freed right after copying.
    unsafe {
        let s = p.to_string().unwrap_or_default();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

fn endpoint(device: IMMDevice) -> Option<Endpoint> {
    // SAFETY: COM calls on a valid device; PROPVARIANT is dropped (cleared) by its Drop impl.
    unsafe {
        let id = take_pwstr(device.GetId().ok()?);
        let name = device
            .OpenPropertyStore(STGM_READ)
            .and_then(|store| store.GetValue(&PKEY_Device_FriendlyName))
            .and_then(|pv| PropVariantToStringAlloc(&pv))
            .map(take_pwstr)
            .unwrap_or_default();
        Some(Endpoint { id, name, device })
    }
}

fn list(e: &IMMDeviceEnumerator, flow: EDataFlow) -> Vec<Endpoint> {
    // SAFETY: COM calls on a valid enumerator.
    unsafe {
        let Ok(coll) = e.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE) else {
            return Vec::new();
        };
        let n = coll.GetCount().unwrap_or(0);
        (0..n).filter_map(|i| coll.Item(i).ok().and_then(endpoint)).collect()
    }
}

pub fn default_render(e: &IMMDeviceEnumerator) -> Option<Endpoint> {
    // SAFETY: COM call on a valid enumerator.
    unsafe { e.GetDefaultAudioEndpoint(eRender, eConsole).ok().and_then(endpoint) }
}

fn contains(hay: &str, needle: &str) -> bool {
    hay.to_lowercase().contains(&needle.to_lowercase())
}

/// Any VB-CABLE playback endpoint ("CABLE Input", "CABLE In 16ch", "Speakers (VB-Audio Virtual Cable)").
/// Loopback-capturing one of these would loop PC audio into the virtual mic.
pub fn is_cable_render_name(name: &str) -> bool {
    contains(name, "VB-Audio Virtual Cable") || contains(name, "CABLE In")
}

fn pick_cable(endpoints: Vec<Endpoint>, preferred: &str) -> Option<Endpoint> {
    let mut fallback = None;
    for ep in endpoints {
        if contains(&ep.name, preferred) {
            return Some(ep);
        }
        if fallback.is_none() && contains(&ep.name, "VB-Audio Virtual Cable") && !contains(&ep.name, "16ch") {
            fallback = Some(ep);
        }
    }
    fallback
}

/// The render endpoint the phone mic is played into ("CABLE Input").
pub fn cable_render(e: &IMMDeviceEnumerator) -> Option<Endpoint> {
    pick_cable(list(e, eRender), "CABLE Input")
}

/// The capture endpoint apps record the phone mic from ("CABLE Output").
pub fn cable_capture(e: &IMMDeviceEnumerator) -> Option<Endpoint> {
    pick_cable(list(e, eCapture), "CABLE Output")
}

pub fn summarize(e: &IMMDeviceEnumerator) -> DeviceSummary {
    let def = default_render(e);
    let renders = list(e, eRender);
    DeviceSummary {
        default_is_cable: def.as_ref().is_some_and(|d| is_cable_render_name(&d.name)),
        default_render_id: def.as_ref().map(|d| d.id.clone()),
        default_render_name: def.map(|d| d.name),
        render_endpoints: renders
            .iter()
            .map(|r| RenderInfo { id: r.id.clone(), name: r.name.clone(), is_cable: is_cable_render_name(&r.name) })
            .collect(),
        cable_render_id: pick_cable(renders, "CABLE Input").map(|c| c.id),
        cable_capture_present: cable_capture(e).is_some(),
    }
}
