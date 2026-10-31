//! XDG autostart entry `~/.config/autostart/audiobridge.desktop`: `"<exe>" --background`.
//! Session managers (systemd's xdg-autostart-generator under uwsm/Hyprland, GNOME, KDE) start it
//! at login.

use std::path::Path;

use anyhow::{Context, Result};

use crate::paths;

const FILE: &str = "audiobridge.desktop";

/// Enables (for the running executable) or disables autostart.
pub fn set(enabled: bool) -> Result<()> {
    let path = paths::autostart_dir().join(FILE);
    if !enabled {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(e).with_context(|| format!("removing {}", path.display()))
            }
            _ => Ok(()),
        };
    }
    let exe = std::env::current_exe().context("current_exe")?;
    let dir = paths::autostart_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = path.with_extension("desktop.tmp");
    std::fs::write(&tmp, entry(&exe))
        .and_then(|()| std::fs::rename(&tmp, &path))
        .with_context(|| format!("writing {}", path.display()))
}

fn entry(exe: &Path) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName=AudioBridge\nComment=Звук компьютера на телефон, микрофон телефона на компьютер\nExec={} --background\nIcon=audiobridge\nTerminal=false\nNoDisplay=true\nX-GNOME-Autostart-enabled=true\n",
        quote_exec(&exe.to_string_lossy())
    )
}

/// Quotes an `Exec=` argument per the Desktop Entry spec (inside double quotes `"`, `` ` ``, `$`
/// and `\` are escaped with a backslash; `\` itself must then be escaped again for the
/// string-level unescaping of the key file).
fn quote_exec(arg: &str) -> String {
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    for c in arg.chars() {
        match c {
            '"' | '`' | '$' => {
                out.push_str("\\\\");
                out.push(c);
            }
            '\\' => out.push_str("\\\\\\\\"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::quote_exec;

    #[test]
    fn exec_paths_are_quoted_for_desktop_files() {
        assert_eq!(quote_exec("/usr/bin/audiobridge"), "\"/usr/bin/audiobridge\"");
        assert_eq!(quote_exec("/home/a b/$x"), "\"/home/a b/\\\\$x\"");
    }
}
