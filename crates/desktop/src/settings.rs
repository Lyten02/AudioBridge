//! Persistent user toggles (`%APPDATA%\AudioBridge\settings.json`).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub autostart: bool,
    pub pc_audio_enabled: bool,
    pub mic_enabled: bool,
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
            pc_audio_enabled: true,
            mic_enabled: true,
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
