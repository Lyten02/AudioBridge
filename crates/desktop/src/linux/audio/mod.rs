//! Audio engine (Linux): one thread runs a PipeWire main loop. It keeps the virtual microphone
//! node alive, captures the default sink's monitor while the phone wants PC audio, follows the
//! default sink's volume, and reports devices, mic demand and volume through [`Hooks`]. Every
//! input is an event (registry, metadata, params, stream states, commands); nothing is polled.
//! If PipeWire goes away the engine reconnects every [`RETRY`].

mod graph;
mod oneshot;
mod streams;
mod volume;

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use audiobridge_core::audio::{CaptureHandle, PlayoutHandle};
use audiobridge_core::session::Volume;
use pipewire as pw;
use pw::core::CoreRc;
use pw::metadata::{Metadata, MetadataListener};
use pw::node::{Node, NodeListener};
use pw::properties::{properties, PropertiesBox};
use pw::registry::{GlobalObject, RegistryRc};
use pw::spa::param::ParamType;
use pw::stream::StreamState;
use pw::types::ObjectType;

pub use crate::endpoints::DeviceSummary;

use self::graph::Graph;
use self::streams::{Capture, CaptureSlot, MicSlot, VirtualMic};
use self::volume::{PropsUpdate, VolumeWriter};

/// `node.name` of the virtual microphone (the Linux counterpart of VB-CABLE's "CABLE Output").
pub const MIC_NODE_NAME: &str = "audiobridge_mic";
/// What mixers and apps show for it.
pub const MIC_NODE_DESCRIPTION: &str = "AudioBridge: микрофон телефона";

/// What the session wants from the audio side, derived from `Status`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Desired {
    pub connected: bool,
    pub pc_audio: bool,
    pub mic_active: bool,
}

pub enum AudioMsg {
    Desired(Desired),
    /// Report the device landscape again (e.g. after the default device was switched).
    DevicesChanged,
    /// Set the default playback device's volume, percent.
    SetVolume(u8),
    /// Mute/unmute the default playback device.
    SetMute(bool),
    Shutdown,
}

/// Sender half of the engine's command channel; wakes the PipeWire loop.
pub type AudioSender = pw::channel::Sender<AudioMsg>;

pub struct Hooks {
    pub on_devices: Box<dyn Fn(&DeviceSummary) + Send>,
    pub on_demand: Box<dyn Fn(bool) + Send>,
    /// Default playback device volume, called on change only (`None` without a default device).
    pub on_volume: Box<dyn Fn(Option<Volume>) + Send>,
}

const RETRY: Duration = Duration::from_secs(2);

/// The current device landscape, read now through a short-lived PipeWire connection.
pub fn current_devices() -> Option<DeviceSummary> {
    match oneshot::current_devices() {
        Ok(d) => Some(d),
        Err(e) => {
            tracing::warn!("reading audio devices: {e:#}");
            None
        }
    }
}

/// Makes the node `id` (a `node.name`) the default playback or recording device.
pub fn set_default_endpoint(id: &str) -> Result<()> {
    oneshot::set_default(id)
}

pub struct AudioEngine {
    tx: AudioSender,
    stop: Arc<AtomicBool>,
    join: JoinHandle<()>,
}

impl AudioEngine {
    pub fn spawn(capture: CaptureHandle, playout: PlayoutHandle, hooks: Hooks) -> std::io::Result<Self> {
        let (tx, rx) = pw::channel::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let join = std::thread::Builder::new()
            .name("audio-supervisor".into())
            .spawn(move || supervise(rx, &thread_stop, capture, playout, hooks))?;
        Ok(Self { tx, stop, join })
    }

    pub fn sender(&self) -> AudioSender {
        self.tx.clone()
    }

    pub fn shutdown(self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.tx.send(AudioMsg::Shutdown);
        let _ = self.join.join();
    }
}

fn supervise(
    mut rx: pw::channel::Receiver<AudioMsg>,
    stop: &AtomicBool,
    capture: CaptureHandle,
    playout: PlayoutHandle,
    hooks: Hooks,
) {
    pw::init();
    let mut engine = Engine {
        hooks,
        capture_slot: Arc::new(Mutex::new(Some(capture))),
        mic_slot: Arc::new(Mutex::new(Some(playout))),
        writer: VolumeWriter::spawn(),
        desired: Desired::default(),
        reported_devices: None,
        reported_volume: None,
        demand: false,
        reported_demand: None,
        shutdown: false,
    };
    let mut failures = 0u32;
    while !stop.load(Ordering::SeqCst) {
        let (back, back_rx, outcome) = run_session(engine, rx);
        engine = back;
        rx = back_rx;
        match outcome {
            Ok(()) => break,
            Err(e) => {
                // first failure loud, the retries quiet (PipeWire may simply not run yet)
                if failures == 0 {
                    tracing::warn!("PipeWire: {e:#}; retrying every {RETRY:?}");
                } else {
                    tracing::debug!("PipeWire: {e:#}");
                }
                failures = failures.saturating_add(1);
                engine.disconnected();
            }
        }
        let deadline = Instant::now() + RETRY;
        while Instant::now() < deadline && !stop.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// State that outlives PipeWire connections.
struct Engine {
    hooks: Hooks,
    capture_slot: CaptureSlot,
    mic_slot: MicSlot,
    writer: VolumeWriter,
    desired: Desired,
    reported_devices: Option<DeviceSummary>,
    reported_volume: Option<Option<Volume>>,
    /// Something records from the virtual mic.
    demand: bool,
    reported_demand: Option<bool>,
    shutdown: bool,
}

impl Engine {
    /// PipeWire went away: the virtual mic is gone with it, and so is the volume.
    fn disconnected(&mut self) {
        self.demand = false;
        self.report(None);
    }

    fn report(&mut self, volume: Option<Volume>) {
        if self.reported_volume != Some(volume) {
            tracing::debug!("playback volume: {volume:?}");
            (self.hooks.on_volume)(volume);
            self.reported_volume = Some(volume);
        }
        if self.desired.connected {
            if self.reported_demand != Some(self.demand) {
                tracing::info!("mic demand: {}", self.demand);
                (self.hooks.on_demand)(self.demand);
                self.reported_demand = Some(self.demand);
            }
        } else {
            self.reported_demand = None;
        }
    }
}

/// The default sink, bound to follow its `Props` (volume, mute).
struct SinkWatch {
    id: u32,
    volume: Option<Volume>,
    _listener: NodeListener,
    _node: Node,
}

/// Objects of one PipeWire connection.
struct Session {
    mainloop: pw::main_loop::MainLoopWeak,
    core: CoreRc,
    registry: RegistryRc,
    graph: Graph,
    /// Audio device node globals, to bind the default sink when it becomes the default.
    globals: HashMap<u32, GlobalObject<PropertiesBox>>,
    metadata: Option<(u32, Metadata, MetadataListener)>,
    sink: Option<SinkWatch>,
    capture: Option<Capture>,
    capture_failed: bool,
    mic: Option<VirtualMic>,
    mic_failed: bool,
    lost: Option<String>,
}

enum Ev {
    Global(GlobalObject<PropertiesBox>),
    Removed(u32),
    Meta(String, Option<String>),
    Props(u32, PropsUpdate),
    MicState(StreamState),
    CaptureState(StreamState),
    Msg(AudioMsg),
    Lost(String),
    /// Periodic: retry streams that failed to connect.
    Tick,
}

struct Inner {
    engine: Engine,
    session: Session,
}

/// Every callback posts an event; the first one drains the queue and applies the result. Events
/// posted while draining (stream callbacks fire synchronously from `connect`) are queued, so
/// nothing is ever borrowed twice.
struct Ctx {
    queue: RefCell<VecDeque<Ev>>,
    pumping: Cell<bool>,
    inner: RefCell<Inner>,
}

impl Ctx {
    fn post(self: &Rc<Self>, ev: Ev) {
        self.queue.borrow_mut().push_back(ev);
        if self.pumping.replace(true) {
            return;
        }
        loop {
            loop {
                let next = self.queue.borrow_mut().pop_front();
                let Some(ev) = next else { break };
                self.inner.borrow_mut().handle(ev, self);
            }
            self.inner.borrow_mut().refresh(self);
            if self.queue.borrow().is_empty() {
                break;
            }
        }
        self.pumping.set(false);
    }

    fn poster(self: &Rc<Self>) -> impl Fn(Ev) + Clone + 'static {
        let weak = Rc::downgrade(self);
        move |ev| {
            if let Some(ctx) = weak.upgrade() {
                ctx.post(ev);
            }
        }
    }
}

type Back = (Engine, pw::channel::Receiver<AudioMsg>, Result<()>);

/// Runs one PipeWire connection until shutdown (`Ok`) or until it fails or is lost (`Err`).
fn run_session(engine: Engine, rx: pw::channel::Receiver<AudioMsg>) -> Back {
    let setup = || -> Result<_> {
        let mainloop = pw::main_loop::MainLoopRc::new(None).context("main loop")?;
        let context = pw::context::ContextRc::new(
            &mainloop,
            Some(properties! {
                *pw::keys::APP_NAME => "AudioBridge",
                "application.icon-name" => "audiobridge",
            }),
        )
        .context("context")?;
        let core = context.connect_rc(None).context("connecting (is PipeWire running?)")?;
        let registry = core.get_registry_rc().context("registry")?;
        Ok((mainloop, context, core, registry))
    };
    let (mainloop, context, core, registry) = match setup() {
        Ok(v) => v,
        Err(e) => return (engine, rx, Err(e)),
    };
    let session = Session {
        mainloop: mainloop.downgrade(),
        core: core.clone(),
        registry: registry.clone(),
        graph: Graph::default(),
        globals: HashMap::new(),
        metadata: None,
        sink: None,
        capture: None,
        capture_failed: false,
        mic: None,
        mic_failed: false,
        lost: None,
    };
    let ctx = Rc::new(Ctx {
        queue: RefCell::new(VecDeque::new()),
        pumping: Cell::new(false),
        inner: RefCell::new(Inner { engine, session }),
    });
    tracing::info!("connected to PipeWire");

    let post = ctx.poster();
    let registry_listener = registry
        .add_listener_local()
        .global({
            let post = post.clone();
            move |obj| post(Ev::Global(obj.to_owned()))
        })
        .global_remove({
            let post = post.clone();
            move |id| post(Ev::Removed(id))
        })
        .register();
    let core_listener = core
        .add_listener_local()
        .error({
            let post = post.clone();
            move |id, _seq, res, message| {
                if id == pw::core::PW_ID_CORE {
                    post(Ev::Lost(format!("connection lost ({res}): {message}")));
                }
            }
        })
        .register();
    let attached = rx.attach(mainloop.loop_(), {
        let post = post.clone();
        move |msg| post(Ev::Msg(msg))
    });
    let tick = mainloop.loop_().add_timer({
        let post = post.clone();
        move |_| post(Ev::Tick)
    });
    let _ = tick.update_timer(Some(RETRY), Some(RETRY));
    // The virtual mic exists for the whole connection.
    post(Ev::Tick);

    mainloop.run();

    let rx = attached.deattach();
    drop(tick);
    drop(core_listener);
    drop(registry_listener);
    drop(post);
    // Disconnecting streams fires their state callbacks synchronously: from here on posts only
    // queue (and are dropped with the queue) instead of re-entering `inner`.
    ctx.pumping.set(true);
    {
        // Streams disconnect first (their data-thread callbacks stop), then the proxies.
        let mut inner = ctx.inner.borrow_mut();
        let s = &mut inner.session;
        s.capture = None;
        s.mic = None;
        s.sink = None;
        s.metadata = None;
    }
    let ctx = match Rc::try_unwrap(ctx) {
        Ok(ctx) => ctx,
        Err(_) => unreachable!("listeners only hold weak references"),
    };
    let Inner { engine, session } = ctx.inner.into_inner();
    let outcome = match (engine.shutdown, session.lost) {
        (true, _) => Ok(()),
        (false, Some(reason)) => Err(anyhow::anyhow!(reason)),
        (false, None) => Err(anyhow::anyhow!("main loop ended")),
    };
    drop(registry);
    drop(core);
    drop(context);
    drop(mainloop);
    (engine, rx, outcome)
}

impl Inner {
    fn handle(&mut self, ev: Ev, ctx: &Rc<Ctx>) {
        let (engine, s) = (&mut self.engine, &mut self.session);
        match ev {
            Ev::Global(obj) => s.global(obj, ctx),
            Ev::Removed(id) => {
                s.graph.remove(id);
                s.globals.remove(&id);
                if s.sink.as_ref().is_some_and(|w| w.id == id) {
                    s.sink = None;
                }
                if s.metadata.as_ref().is_some_and(|(mid, _, _)| *mid == id) {
                    s.metadata = None;
                }
            }
            Ev::Meta(key, value) => {
                s.graph.defaults.apply(&key, value.as_deref());
            }
            Ev::Props(node, update) => {
                if let Some(w) = s.sink.as_mut().filter(|w| w.id == node) {
                    w.volume = update.apply(w.volume);
                }
            }
            Ev::MicState(state) => {
                tracing::debug!("virtual mic: {state:?}");
                if let StreamState::Error(e) = &state {
                    tracing::warn!("virtual mic failed: {e}; recreating in {RETRY:?}");
                    s.mic = None;
                    s.mic_failed = true;
                }
                engine.demand = state == StreamState::Streaming;
            }
            Ev::CaptureState(state) => {
                tracing::debug!("loopback: {state:?}");
                match state {
                    StreamState::Streaming => tracing::info!("loopback started"),
                    StreamState::Error(e) => {
                        tracing::warn!("loopback failed: {e}; retrying in {RETRY:?}");
                        s.capture = None;
                        s.capture_failed = true;
                    }
                    _ => {}
                }
            }
            Ev::Msg(AudioMsg::Desired(d)) => engine.desired = d,
            Ev::Msg(AudioMsg::DevicesChanged) => engine.reported_devices = None,
            Ev::Msg(AudioMsg::SetVolume(level)) => match &s.sink {
                Some(w) => engine.writer.set_level(w.id, level),
                None => tracing::info!("volume request dropped: no default playback device"),
            },
            Ev::Msg(AudioMsg::SetMute(muted)) => match &s.sink {
                Some(w) => engine.writer.set_mute(w.id, muted),
                None => tracing::info!("mute request dropped: no default playback device"),
            },
            Ev::Msg(AudioMsg::Shutdown) => {
                engine.shutdown = true;
                s.quit();
            }
            Ev::Lost(reason) => {
                s.lost = Some(reason);
                s.quit();
            }
            Ev::Tick => {
                s.capture_failed = false;
                s.mic_failed = false;
            }
        }
    }

    /// Brings streams, the volume watch and the reports in line with the current state.
    fn refresh(&mut self, ctx: &Rc<Ctx>) {
        let (engine, s) = (&mut self.engine, &mut self.session);
        if engine.shutdown || s.lost.is_some() {
            return;
        }
        let summary = s.graph.summary();
        if engine.reported_devices.as_ref() != Some(&summary) {
            tracing::info!("audio devices: {summary:?}");
            (engine.hooks.on_devices)(&summary);
            engine.reported_devices = Some(summary);
        }

        let default_sink = s.graph.default_sink().map(|(id, _)| id);
        if s.sink.as_ref().map(|w| w.id) != default_sink {
            s.sink = default_sink.and_then(|id| s.watch_sink(id, ctx));
        }

        if s.mic.is_none() && !s.mic_failed {
            let post = ctx.poster();
            match VirtualMic::connect(&s.core, engine.mic_slot.clone(), move |st| post(Ev::MicState(st))) {
                Ok(mic) => {
                    tracing::info!("virtual mic \"{MIC_NODE_NAME}\" created");
                    s.mic = Some(mic);
                }
                Err(e) => {
                    tracing::warn!("virtual mic: {e:#}; retrying in {RETRY:?}");
                    s.mic_failed = true;
                }
            }
        }

        let want_pc = engine.desired.connected && engine.desired.pc_audio && default_sink.is_some();
        if want_pc && s.capture.is_none() && !s.capture_failed {
            let post = ctx.poster();
            match Capture::connect(&s.core, engine.capture_slot.clone(), move |st| post(Ev::CaptureState(st))) {
                Ok(c) => s.capture = Some(c),
                Err(e) => {
                    tracing::warn!("loopback: {e:#}; retrying in {RETRY:?}");
                    s.capture_failed = true;
                }
            }
        } else if !want_pc && s.capture.take().is_some() {
            tracing::info!("loopback stopped");
        }

        let volume = s.sink.as_ref().and_then(|w| w.volume);
        engine.report(volume);
    }
}

impl Session {
    fn quit(&self) {
        if let Some(l) = self.mainloop.upgrade() {
            l.quit();
        }
    }

    fn global(&mut self, obj: GlobalObject<PropertiesBox>, ctx: &Rc<Ctx>) {
        let get = |k: &str| obj.props.as_ref().and_then(|p| p.get(k));
        match obj.type_ {
            ObjectType::Node => {
                let before = self.graph.nodes.len();
                self.graph.add_node(obj.id, get("node.name"), get("node.description"), get("media.class"));
                if self.graph.nodes.len() != before {
                    self.globals.insert(obj.id, obj);
                }
            }
            ObjectType::Metadata if get("metadata.name") == Some("default") => {
                let metadata = match self.registry.bind::<Metadata, _>(&obj) {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!("binding the default metadata: {e}");
                        return;
                    }
                };
                let post = ctx.poster();
                let listener = metadata
                    .add_listener_local()
                    .property(move |subject, key, _type, value| {
                        if let (0, Some(key)) = (subject, key) {
                            post(Ev::Meta(key.to_owned(), value.map(str::to_owned)));
                        }
                        0
                    })
                    .register();
                self.metadata = Some((obj.id, metadata, listener));
            }
            _ => {}
        }
    }

    fn watch_sink(&self, id: u32, ctx: &Rc<Ctx>) -> Option<SinkWatch> {
        let global = self.globals.get(&id)?;
        let node: Node = match self.registry.bind(global) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!("binding the default sink: {e}");
                return None;
            }
        };
        let post = ctx.poster();
        let listener = node
            .add_listener_local()
            .param(move |_seq, param_id, _index, _next, pod| {
                if param_id == ParamType::Props {
                    if let Some(update) = pod.and_then(volume::parse_props) {
                        post(Ev::Props(id, update));
                    }
                }
            })
            .register();
        node.subscribe_params(&[ParamType::Props]);
        Some(SinkWatch { id, volume: None, _listener: listener, _node: node })
    }
}
