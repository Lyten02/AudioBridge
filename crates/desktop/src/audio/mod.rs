//! Audio engine: a supervisor thread that opens/closes WASAPI streams from session status
//! and device notifications. It blocks with no timeout while no phone is connected.

pub(crate) mod com;
mod demand;
pub mod devices;
mod format;
mod loopback;
mod policy;
mod render;

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Result;
use audiobridge_core::audio::{CaptureHandle, PlayoutHandle};
use windows::core::PCWSTR;
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Media::Audio::{
    eConsole, eRender, EDataFlow, ERole, IMMNotificationClient, IMMNotificationClient_Impl, DEVICE_STATE,
};

use self::com::{Com, Event};
use self::demand::DemandProbe;
pub use self::devices::DeviceSummary;
pub use self::policy::set_default_render;

/// What the session wants from the audio side, derived from `Status`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Desired {
    pub connected: bool,
    pub pc_audio: bool,
    pub mic_active: bool,
}

pub enum AudioMsg {
    Desired(Desired),
    DevicesChanged,
    Shutdown,
}

pub struct Hooks {
    pub on_devices: Box<dyn Fn(&DeviceSummary) + Send>,
    pub on_demand: Box<dyn Fn(bool) + Send>,
}

const TICK: Duration = Duration::from_secs(1);
const RETRY: Duration = Duration::from_secs(2);

pub struct AudioEngine {
    tx: Sender<AudioMsg>,
    join: JoinHandle<()>,
}

impl AudioEngine {
    pub fn spawn(capture: CaptureHandle, playout: PlayoutHandle, hooks: Hooks) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let notifier_tx = tx.clone();
        let join = std::thread::Builder::new()
            .name("audio-supervisor".into())
            .spawn(move || supervise(rx, notifier_tx, capture, playout, hooks))?;
        Ok(Self { tx, join })
    }

    pub fn sender(&self) -> Sender<AudioMsg> {
        self.tx.clone()
    }

    pub fn shutdown(self) {
        let _ = self.tx.send(AudioMsg::Shutdown);
        let _ = self.join.join();
    }
}

#[windows_core::implement(IMMNotificationClient)]
struct Notifier {
    tx: Sender<AudioMsg>,
}

impl IMMNotificationClient_Impl for Notifier_Impl {
    fn OnDeviceStateChanged(&self, _id: &PCWSTR, _state: DEVICE_STATE) -> windows::core::Result<()> {
        let _ = self.tx.send(AudioMsg::DevicesChanged);
        Ok(())
    }
    fn OnDeviceAdded(&self, _id: &PCWSTR) -> windows::core::Result<()> {
        let _ = self.tx.send(AudioMsg::DevicesChanged);
        Ok(())
    }
    fn OnDeviceRemoved(&self, _id: &PCWSTR) -> windows::core::Result<()> {
        let _ = self.tx.send(AudioMsg::DevicesChanged);
        Ok(())
    }
    fn OnDefaultDeviceChanged(&self, flow: EDataFlow, role: ERole, _id: &PCWSTR) -> windows::core::Result<()> {
        if flow == eRender && role == eConsole {
            let _ = self.tx.send(AudioMsg::DevicesChanged);
        }
        Ok(())
    }
    fn OnPropertyValueChanged(&self, _id: &PCWSTR, _key: &PROPERTYKEY) -> windows::core::Result<()> {
        Ok(())
    }
}

type RunFn<H> = fn(&mut H, &Event) -> Result<()>;

struct Worker<H> {
    stop: Arc<Event>,
    join: JoinHandle<(H, Result<()>)>,
    device_id: Option<String>,
}

enum Slot<H> {
    Idle(H),
    Running(Worker<H>),
    /// The handle was lost (worker panicked or could not be spawned).
    Gone,
}

/// One device stream (loopback or mic render) and its restart policy.
struct Stream<H> {
    name: &'static str,
    slot: Slot<H>,
    retry_at: Option<Instant>,
    run: RunFn<H>,
}

impl<H: Send + 'static> Stream<H> {
    fn new(name: &'static str, handle: H, run: RunFn<H>) -> Self {
        Self { name, slot: Slot::Idle(handle), retry_at: None, run }
    }

    fn active(&self) -> bool {
        matches!(self.slot, Slot::Running(_))
    }

    fn finish(&mut self, worker: Worker<H>) {
        match worker.join.join() {
            Ok((h, r)) => {
                if let Err(e) = r {
                    tracing::warn!("{} failed: {e:#}; retrying in {RETRY:?}", self.name);
                    self.retry_at = Some(Instant::now() + RETRY);
                }
                self.slot = Slot::Idle(h);
            }
            Err(_) => {
                tracing::error!("{} thread panicked; stream disabled", self.name);
                self.slot = Slot::Gone;
            }
        }
    }

    fn stop(&mut self) {
        if let Slot::Running(_) = self.slot {
            let Slot::Running(w) = std::mem::replace(&mut self.slot, Slot::Gone) else { unreachable!() };
            w.stop.set();
            self.finish(w);
        }
    }

    fn ensure(&mut self, want: bool, device_id: Option<&String>) {
        if let Slot::Running(w) = &self.slot {
            if w.join.is_finished() {
                let Slot::Running(w) = std::mem::replace(&mut self.slot, Slot::Gone) else { unreachable!() };
                self.finish(w);
            } else if !want || w.device_id.as_ref() != device_id {
                self.stop();
                self.retry_at = None;
            }
        }
        if !want {
            self.retry_at = None;
            return;
        }
        if self.retry_at.is_some_and(|t| Instant::now() < t) || !matches!(self.slot, Slot::Idle(_)) {
            return;
        }
        let Slot::Idle(mut handle) = std::mem::replace(&mut self.slot, Slot::Gone) else { unreachable!() };
        let stop = match Event::new() {
            Ok(e) => Arc::new(e),
            Err(e) => {
                tracing::warn!("{}: CreateEvent failed: {e}", self.name);
                self.slot = Slot::Idle(handle);
                self.retry_at = Some(Instant::now() + RETRY);
                return;
            }
        };
        let run = self.run;
        let thread_stop = stop.clone();
        let spawned = std::thread::Builder::new().name(self.name.into()).spawn(move || {
            let r = run(&mut handle, &thread_stop);
            (handle, r)
        });
        match spawned {
            Ok(join) => {
                self.retry_at = None;
                self.slot = Slot::Running(Worker { stop, join, device_id: device_id.cloned() });
            }
            Err(e) => tracing::error!("{}: thread spawn failed: {e}; stream disabled", self.name),
        }
    }
}

fn supervise(
    rx: Receiver<AudioMsg>,
    notifier_tx: Sender<AudioMsg>,
    capture: CaptureHandle,
    playout: PlayoutHandle,
    hooks: Hooks,
) {
    let _com = Com::init();
    let enumerator = match com::enumerator() {
        Ok(e) => e,
        Err(e) => {
            tracing::error!("MMDeviceEnumerator unavailable: {e}; audio disabled");
            while !matches!(rx.recv(), Ok(AudioMsg::Shutdown) | Err(_)) {}
            return;
        }
    };
    let notifier: IMMNotificationClient = Notifier { tx: notifier_tx }.into();
    // SAFETY: COM call on a valid enumerator; unregistered below before drop.
    if let Err(e) = unsafe { enumerator.RegisterEndpointNotificationCallback(&notifier) } {
        tracing::warn!("device notifications unavailable: {e}");
    }

    let mut devices = devices::summarize(&enumerator);
    (hooks.on_devices)(&devices);
    tracing::info!("audio devices: {devices:?}");

    let mut desired = Desired::default();
    let mut probe = DemandProbe::default();
    let mut last_demand: Option<bool> = None;
    let mut pc = Stream::new("loopback", capture, loopback::run);
    let mut mic = Stream::new("mic-render", playout, render::run);

    'outer: loop {
        let busy = desired.connected || pc.active() || mic.active();
        let first = if busy {
            rx.recv_timeout(TICK)
        } else {
            rx.recv().map_err(|_| RecvTimeoutError::Disconnected)
        };
        let mut devices_dirty = false;
        let mut msgs = match first {
            Ok(m) => vec![m],
            Err(RecvTimeoutError::Timeout) => Vec::new(),
            Err(RecvTimeoutError::Disconnected) => break,
        };
        msgs.extend(rx.try_iter());
        for m in msgs {
            match m {
                AudioMsg::Desired(d) => desired = d,
                AudioMsg::DevicesChanged => devices_dirty = true,
                AudioMsg::Shutdown => break 'outer,
            }
        }
        if devices_dirty {
            let fresh = devices::summarize(&enumerator);
            if fresh != devices {
                tracing::info!("audio devices changed: {fresh:?}");
                devices = fresh;
                (hooks.on_devices)(&devices);
                probe.reset();
            }
        }
        let want_pc = desired.connected
            && desired.pc_audio
            && !devices.default_is_cable
            && devices.default_render_id.is_some();
        pc.ensure(want_pc, devices.default_render_id.as_ref());
        let want_mic = desired.mic_active && devices.cable_render_id.is_some();
        mic.ensure(want_mic, devices.cable_render_id.as_ref());

        if desired.connected {
            if let Some(v) = probe.poll(&enumerator) {
                if last_demand != Some(v) {
                    tracing::info!("mic demand: {v}");
                    (hooks.on_demand)(v);
                    last_demand = Some(v);
                }
            }
        } else {
            last_demand = None;
        }
    }
    pc.stop();
    mic.stop();
    // SAFETY: matches the registration above.
    unsafe {
        let _ = enumerator.UnregisterEndpointNotificationCallback(&notifier);
    }
}
