//! Tray icon (StatusNotifierItem over D-Bus: Waybar, Quickshell, KDE, GNOME with AppIndicator).
//! The SNI service runs on its own threads; `refresh` only enqueues, so it is safe from any
//! thread, including tokio tasks. Without a session bus the app runs without a tray: launching
//! AudioBridge again shows the window.

use std::sync::mpsc::{self, Sender};
use std::sync::Mutex;
use std::thread::JoinHandle;

use anyhow::{Context, Result};
use ksni::blocking::TrayMethods;
use ksni::menu::{CheckmarkItem, StandardItem};
use ksni::{Icon, MenuItem, ToolTip};

use crate::icon;
use crate::shared::shared;

enum Cmd {
    Refresh,
    Quit,
}

static TX: Mutex<Option<Sender<Cmd>>> = Mutex::new(None);

pub struct Tray {
    join: JoinHandle<()>,
}

impl Tray {
    /// Removes the icon and ends the tray thread.
    pub fn quit(self) {
        send(Cmd::Quit);
        let _ = self.join.join();
    }
}

fn send(cmd: Cmd) {
    if let Some(tx) = TX.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        let _ = tx.send(cmd);
    }
}

/// Re-reads state, tooltip, icon and menu from `shared()`.
pub fn refresh() {
    send(Cmd::Refresh);
}

pub fn spawn() -> Result<Tray> {
    let (tx, rx) = mpsc::channel();
    *TX.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
    let join = std::thread::Builder::new()
        .name("tray".into())
        .spawn(move || {
            // Waits for the panel's StatusNotifierWatcher if it starts after us (autostart).
            let handle = match AppTray.assume_sni_available(true).spawn() {
                Ok(h) => Some(h),
                Err(e) => {
                    tracing::warn!("tray icon unavailable: {e}");
                    None
                }
            };
            while let Ok(cmd) = rx.recv() {
                let Some(handle) = &handle else {
                    if matches!(cmd, Cmd::Quit) {
                        break;
                    }
                    continue;
                };
                match cmd {
                    Cmd::Refresh => {
                        // coalesce a burst of refreshes into one D-Bus update
                        if rx.try_iter().any(|c| matches!(c, Cmd::Quit)) {
                            handle.shutdown().wait();
                            break;
                        }
                        handle.update(|_| ());
                    }
                    Cmd::Quit => {
                        handle.shutdown().wait();
                        break;
                    }
                }
            }
        })
        .context("tray thread")?;
    Ok(Tray { join })
}

struct AppTray;

fn pixmap(size: u32, connected: bool, disabled: bool) -> Icon {
    let rgba = icon::render(size, connected);
    let mut data = Vec::with_capacity(rgba.len());
    for px in rgba.as_chunks::<4>().0 {
        let [r, g, b, a] = *px;
        if disabled {
            let grey = ((u16::from(r) + u16::from(g) + u16::from(b)) / 3) as u8;
            data.extend_from_slice(&[a, grey, grey, grey]);
        } else {
            data.extend_from_slice(&[a, r, g, b]);
        }
    }
    Icon { width: size as i32, height: size as i32, data }
}

impl ksni::Tray for AppTray {
    fn id(&self) -> String {
        "audiobridge".into()
    }

    fn title(&self) -> String {
        "AudioBridge".into()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        let (connected, _) = shared().tray_state();
        let disabled = !shared().settings().service_enabled;
        [22, 32, 48, 64].into_iter().map(|s| pixmap(s, connected, disabled)).collect()
    }

    fn tool_tip(&self) -> ToolTip {
        let (_, tip) = shared().tray_state();
        ToolTip { title: tip, ..Default::default() }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        shared().show_window();
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let s = shared().settings();
        let enabled_label = if s.service_enabled { "Выключить AudioBridge" } else { "Включить AudioBridge" };
        vec![
            StandardItem {
                label: "Открыть".into(),
                activate: Box::new(|_: &mut Self| shared().show_window()),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: enabled_label.into(),
                activate: Box::new(move |_: &mut Self| shared().set_enabled(!s.service_enabled)),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            CheckmarkItem {
                label: "Звук компьютера".into(),
                enabled: s.service_enabled,
                checked: s.pc_audio_enabled,
                activate: Box::new(move |_: &mut Self| shared().set_pc_audio(!s.pc_audio_enabled)),
                ..Default::default()
            }
            .into(),
            CheckmarkItem {
                label: "Микрофон".into(),
                enabled: s.service_enabled,
                checked: s.mic_enabled,
                activate: Box::new(move |_: &mut Self| shared().set_mic(!s.mic_enabled)),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Выход".into(),
                activate: Box::new(|_: &mut Self| shared().request_exit()),
                ..Default::default()
            }
            .into(),
        ]
    }
}
