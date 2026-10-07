//! Pure mapping of the remote-control JNI arguments (`NativeBridge.controlPc` / `NativeBridge.setVolume`)
//! to core types.

use audiobridge_core::proto::MAX_LEVEL;
use audiobridge_core::session::PcRequest;

// `controlPc` actions; keep in sync with the `NativeBridge.PC_*` constants.
pub const PC_AUDIO: i32 = 0;
pub const PC_MIC: i32 = 1;
pub const PC_MIC_DEFAULT: i32 = 2;
pub const PC_VOLUME: i32 = 3;
pub const PC_MUTE: i32 = 4;

/// `controlPc(peerId, action, value)`: switches are on for any non-zero `value`, the volume is clamped to
/// `0..=100`. `None` for an unknown action.
pub fn pc_request(action: i32, value: i32) -> Option<PcRequest> {
    let on = value != 0;
    Some(match action {
        PC_AUDIO => PcRequest::Audio(on),
        PC_MIC => PcRequest::Mic(on),
        PC_MIC_DEFAULT => PcRequest::MicDefault(on),
        PC_VOLUME => PcRequest::Volume(clamp_level(value)),
        PC_MUTE => PcRequest::Mute(on),
        _ => return None,
    })
}

/// `setVolume(percent)`: a negative value means unknown (`None`); values above 100 are clamped.
pub fn volume_percent(percent: i32) -> Option<u8> {
    (percent >= 0).then(|| clamp_level(percent))
}

fn clamp_level(v: i32) -> u8 {
    // Lossless: the value is clamped to 0..=100 first.
    v.clamp(0, i32::from(MAX_LEVEL)) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switch_actions_map_to_requests() {
        let cases = [
            (PC_AUDIO, 1, PcRequest::Audio(true)),
            (PC_AUDIO, 0, PcRequest::Audio(false)),
            (PC_MIC, 1, PcRequest::Mic(true)),
            (PC_MIC, 0, PcRequest::Mic(false)),
            (PC_MIC_DEFAULT, 1, PcRequest::MicDefault(true)),
            (PC_MIC_DEFAULT, 0, PcRequest::MicDefault(false)),
            (PC_MUTE, 1, PcRequest::Mute(true)),
            (PC_MUTE, 0, PcRequest::Mute(false)),
            // Any non-zero value switches on.
            (PC_MIC, -1, PcRequest::Mic(true)),
            (PC_MUTE, 7, PcRequest::Mute(true)),
        ];
        for (action, value, req) in cases {
            assert_eq!(pc_request(action, value), Some(req), "action {action} value {value}");
        }
    }

    #[test]
    fn volume_request_is_clamped() {
        for (value, level) in [(0, 0), (42, 42), (100, 100), (101, 100), (i32::MAX, 100), (-1, 0), (i32::MIN, 0)] {
            assert_eq!(pc_request(PC_VOLUME, value), Some(PcRequest::Volume(level)), "value {value}");
        }
    }

    #[test]
    fn unknown_action_is_none() {
        for action in [-1, 5, i32::MIN, i32::MAX] {
            assert_eq!(pc_request(action, 1), None, "action {action}");
        }
    }

    #[test]
    fn phone_volume_percent_maps_unknown_and_clamps() {
        let cases = [
            (-1, None),
            (-50, None),
            (i32::MIN, None),
            (0, Some(0)),
            (55, Some(55)),
            (100, Some(100)),
            (101, Some(100)),
            (i32::MAX, Some(100)),
        ];
        for (percent, volume) in cases {
            assert_eq!(volume_percent(percent), volume, "percent {percent}");
        }
    }
}
