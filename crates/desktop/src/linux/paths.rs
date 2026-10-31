//! Well-known locations (XDG base directories) and machine identity.

use std::path::PathBuf;

const APP_DIR: &str = "audiobridge";

fn home() -> PathBuf {
    std::env::var_os("HOME").filter(|v| !v.is_empty()).map_or_else(|| PathBuf::from("."), PathBuf::from)
}

/// `$XDG_*_HOME` if set to an absolute path (the spec says to ignore relative ones), else `~/<fallback>`.
fn xdg_dir(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(fallback))
}

/// `~/.config/audiobridge`: keys, pairing secret, `settings.json`, `pairing.txt`.
pub fn data_dir() -> PathBuf {
    #[cfg(debug_assertions)]
    if let Some(dir) = dev_dir() {
        return dir;
    }
    xdg_dir("XDG_CONFIG_HOME", ".config").join(APP_DIR)
}

/// `~/.local/state/audiobridge/logs`.
pub fn log_dir() -> PathBuf {
    #[cfg(debug_assertions)]
    if let Some(dir) = dev_dir() {
        return dir.join("logs");
    }
    xdg_dir("XDG_STATE_HOME", ".local/state").join(APP_DIR).join("logs")
}

/// `~/.config/autostart`.
pub fn autostart_dir() -> PathBuf {
    xdg_dir("XDG_CONFIG_HOME", ".config").join("autostart")
}

/// The single-instance socket: `$XDG_RUNTIME_DIR/audiobridge.sock` (per user and session).
pub fn instance_socket() -> PathBuf {
    #[cfg(debug_assertions)]
    if let Some(dir) = dev_dir() {
        return dir.join("instance.sock");
    }
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        // without a runtime dir (no login session) the data dir is still per user
        .unwrap_or_else(data_dir);
    runtime.join("audiobridge.sock")
}

/// Isolated debug identity and logs; release builds never inspect this override.
#[cfg(debug_assertions)]
pub fn dev_dir() -> Option<PathBuf> {
    std::env::var_os("AUDIOBRIDGE_DEV_DIR").filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// The computer's name as the user set it: `hostnamectl --pretty` (e.g. "Ноутбук"), else the
/// host name.
pub fn computer_name() -> String {
    let pretty = std::fs::read_to_string("/etc/machine-info").ok().and_then(|s| pretty_hostname(&s));
    pretty
        .or_else(|| {
            std::fs::read_to_string("/proc/sys/kernel/hostname")
                .ok()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "PC".to_owned())
}

/// `PRETTY_HOSTNAME=` from `/etc/machine-info` (an env-style file; values may be quoted).
fn pretty_hostname(machine_info: &str) -> Option<String> {
    machine_info.lines().find_map(|line| {
        let value = line.trim().strip_prefix("PRETTY_HOSTNAME=")?.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(value);
        let value = value.replace("\\\"", "\"").replace("\\\\", "\\");
        (!value.trim().is_empty()).then_some(value)
    })
}

#[cfg(test)]
mod tests {
    use super::pretty_hostname;

    #[test]
    fn pretty_hostname_is_unquoted() {
        assert_eq!(pretty_hostname("PRETTY_HOSTNAME=\"Ноутбук\"\n").as_deref(), Some("Ноутбук"));
        assert_eq!(pretty_hostname("CHASSIS=laptop\nPRETTY_HOSTNAME=Work PC\n").as_deref(), Some("Work PC"));
        assert_eq!(pretty_hostname("PRETTY_HOSTNAME=\"\"\n"), None);
        assert_eq!(pretty_hostname("CHASSIS=desktop\n"), None);
    }
}
