//! Persistent user toggles (`%APPDATA%\AudioBridge\settings.json`).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub autostart: bool,
    /// Global service switch. Missing in older files means enabled.
    pub service_enabled: bool,
    pub pc_audio_enabled: bool,
    pub mic_enabled: bool,
    /// Restore the pre-off recording route, independently of accepting the phone microphone.
    pub resume_mic_default: bool,
    /// Endpoint id of the last default playback device that was not VB-CABLE, so a CABLE
    /// hijack of the default device (common right after installing VB-CABLE) can be undone.
    pub last_default_render: Option<String>,
    /// Endpoint id of the last default recording device that was not VB-CABLE, restored when the
    /// virtual mic stops being the default microphone.
    pub last_default_capture: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            autostart: true,
            service_enabled: true,
            pc_audio_enabled: true,
            mic_enabled: true,
            resume_mic_default: false,
            last_default_render: None,
            last_default_capture: None,
        }
    }
}

fn file(dir: &Path) -> PathBuf {
    dir.join("settings.json")
}

impl Settings {
    /// Returns the stored settings and whether this is the first run (no file yet).
    pub fn load(dir: &Path) -> (Self, bool) {
        match std::fs::read(file(dir)) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(s) => (s, false),
                Err(e) => {
                    tracing::warn!("settings.json is corrupt ({e}); using defaults");
                    (Self::default(), false)
                }
            },
            Err(_) => (Self::default(), true),
        }
    }

    pub fn save(&self, dir: &Path) {
        let path = file(dir);
        let tmp = path.with_extension("json.tmp");
        let result = serde_json::to_vec_pretty(self)
            .map_err(std::io::Error::other)
            .and_then(|bytes| std::fs::write(&tmp, bytes))
            .and_then(|()| std::fs::rename(&tmp, &path));
        if let Err(e) = result {
            tracing::warn!("failed to save {}: {e}", path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Settings;

    #[test]
    fn old_settings_keep_service_enabled() {
        let settings: Settings = serde_json::from_str(r#"{"mic_enabled":false}"#).unwrap();
        assert!(settings.service_enabled);
        assert!(!settings.mic_enabled);
    }

    #[test]
    fn disabled_service_roundtrips_without_changing_features() {
        let settings = Settings { service_enabled: false, mic_enabled: false, ..Settings::default() };
        let json = serde_json::to_vec(&settings).unwrap();
        assert_eq!(serde_json::from_slice::<Settings>(&json).unwrap(), settings);
    }
}
