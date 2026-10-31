//! Volume of the default sink. Reading: the node's `Props` param (PipeWire pushes every change,
//! from us, the desktop's mixer or media keys). Writing: `wpctl`, which goes through
//! WirePlumber's mixer API and so updates hardware routes the same way every desktop mixer does.

use std::process::Command;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;

use audiobridge_core::proto::MAX_LEVEL;
use audiobridge_core::session::Volume;
use pipewire::spa::pod::deserialize::PodDeserializer;
use pipewire::spa::pod::{Pod, Value, ValueArray};
use pipewire::spa::sys::{SPA_PROP_channelVolumes, SPA_PROP_mute};

/// Volume and mute from a `Props` param. `None` if it carries neither (partial updates).
pub fn parse_props(pod: &Pod) -> Option<PropsUpdate> {
    let (_, value) = PodDeserializer::deserialize_any_from(pod.as_bytes()).ok()?;
    let Value::Object(object) = value else { return None };
    let mut update = PropsUpdate::default();
    for prop in object.properties {
        match (prop.key, prop.value) {
            (key, Value::ValueArray(ValueArray::Float(v))) if key == SPA_PROP_channelVolumes && !v.is_empty() => {
                update.level = Some(level_from_linear(v.iter().copied().fold(0.0, f32::max)));
            }
            (key, Value::Bool(m)) if key == SPA_PROP_mute => update.muted = Some(m),
            _ => {}
        }
    }
    (update.level.is_some() || update.muted.is_some()).then_some(update)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PropsUpdate {
    pub level: Option<u8>,
    pub muted: Option<bool>,
}

impl PropsUpdate {
    /// Merges into the last known state; a complete volume needs a level.
    pub fn apply(self, current: Option<Volume>) -> Option<Volume> {
        let level = self.level.or(current.map(|v| v.level))?;
        let muted = self.muted.or(current.map(|v| v.muted)).unwrap_or(false);
        Some(Volume { level, muted })
    }
}

/// Channel volumes are linear amplitude; mixers (wpctl, pavucontrol, the phone's slider) show the
/// cubic scale: percent = cbrt(linear) × 100.
pub fn level_from_linear(linear: f32) -> u8 {
    (linear.max(0.0).cbrt() * 100.0).round().min(f32::from(MAX_LEVEL)) as u8
}

enum Op {
    Level(u32, u8),
    Mute(u32, bool),
}

/// Runs `wpctl` off the PipeWire thread; a dragged slider's burst collapses to the latest value.
pub struct VolumeWriter {
    tx: Option<Sender<Op>>,
    join: Option<JoinHandle<()>>,
}

impl VolumeWriter {
    pub fn spawn() -> Self {
        let (tx, rx) = mpsc::channel();
        match std::thread::Builder::new().name("volume-writer".into()).spawn(move || write_loop(&rx)) {
            Ok(join) => Self { tx: Some(tx), join: Some(join) },
            Err(e) => {
                tracing::warn!("volume writer thread: {e}; PC volume control disabled");
                Self { tx: None, join: None }
            }
        }
    }

    pub fn set_level(&self, node: u32, level: u8) {
        self.send(Op::Level(node, level.min(MAX_LEVEL)));
    }

    pub fn set_mute(&self, node: u32, muted: bool) {
        self.send(Op::Mute(node, muted));
    }

    fn send(&self, op: Op) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(op);
        }
    }
}

impl Drop for VolumeWriter {
    fn drop(&mut self) {
        drop(self.tx.take());
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn write_loop(rx: &Receiver<Op>) {
    while let Ok(first) = rx.recv() {
        let mut level = None;
        let mut mute = None;
        for op in std::iter::once(first).chain(rx.try_iter()) {
            match op {
                Op::Level(node, l) => level = Some((node, l)),
                Op::Mute(node, m) => mute = Some((node, m)),
            }
        }
        if let Some((node, l)) = level {
            wpctl(&["set-volume", &node.to_string(), &format!("{:.2}", f32::from(l) / 100.0)]);
        }
        if let Some((node, m)) = mute {
            wpctl(&["set-mute", &node.to_string(), if m { "1" } else { "0" }]);
        }
    }
}

fn wpctl(args: &[&str]) {
    match Command::new("wpctl").args(args).output() {
        Ok(out) if out.status.success() => tracing::debug!("wpctl {}", args.join(" ")),
        Ok(out) => tracing::warn!(
            "wpctl {} failed ({}): {}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        Err(e) => tracing::warn!("cannot run wpctl (WirePlumber): {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linear_volume_maps_to_mixer_percent() {
        assert_eq!(level_from_linear(1.0), 100);
        assert_eq!(level_from_linear(0.125), 50);
        assert_eq!(level_from_linear(0.0), 0);
        // software boost above 100 % is reported as the maximum the protocol carries
        assert_eq!(level_from_linear(3.0), MAX_LEVEL);
    }

    #[test]
    fn partial_props_updates_keep_the_other_half() {
        let before = Some(Volume { level: 40, muted: false });
        assert_eq!(PropsUpdate { level: None, muted: Some(true) }.apply(before), Some(Volume { level: 40, muted: true }));
        assert_eq!(PropsUpdate { level: Some(70), muted: None }.apply(before), Some(Volume { level: 70, muted: false }));
        assert_eq!(PropsUpdate { level: None, muted: Some(true) }.apply(None), None);
    }
}
