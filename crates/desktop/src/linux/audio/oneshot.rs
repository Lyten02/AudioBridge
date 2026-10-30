//! Short-lived PipeWire connections for synchronous questions from the service thread: the
//! current device landscape, and switching the default device (also while the engine is down,
//! e.g. restoring the user's microphone after AudioBridge was switched off).

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use pipewire as pw;
use pw::metadata::{Metadata, MetadataListener};
use pw::properties::properties;
use pw::types::ObjectType;

use super::graph::{node_name_json, Graph};
use super::MIC_NODE_NAME;

const TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Default)]
struct State {
    graph: Graph,
    metadata: Option<(Metadata, MetadataListener)>,
    /// Roundtrips completed: 1 = all globals seen, 2 = bound metadata reported its properties.
    phase: u8,
    pending: Option<i32>,
    timed_out: bool,
}

/// Connects, collects nodes and the `default` metadata, runs `act` (which may write metadata),
/// flushes with one more roundtrip and disconnects.
fn with_graph<R>(act: impl FnOnce(&Graph, Option<&Metadata>) -> Result<R>) -> Result<R> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).context("PipeWire main loop")?;
    let context = pw::context::ContextRc::new(&mainloop, Some(properties! { *pw::keys::APP_NAME => "AudioBridge" }))
        .context("PipeWire context")?;
    let core = context.connect_rc(None).context("connecting to PipeWire (is it running?)")?;
    let registry = core.get_registry_rc().context("PipeWire registry")?;
    let state = Rc::new(RefCell::new(State::default()));

    let registry_listener = registry
        .add_listener_local()
        .global({
            let state = state.clone();
            let registry = registry.downgrade();
            move |obj| {
                let props = obj.props.as_ref();
                match obj.type_ {
                    ObjectType::Node => {
                        let get = |k| props.and_then(|p| p.get(k));
                        state.borrow_mut().graph.add_node(obj.id, get("node.name"), get("node.description"), get("media.class"));
                    }
                    ObjectType::Metadata if props.and_then(|p| p.get("metadata.name")) == Some("default") => {
                        let Some(registry) = registry.upgrade() else { return };
                        let Ok(metadata) = registry.bind::<Metadata, _>(obj) else { return };
                        let listener = metadata
                            .add_listener_local()
                            .property({
                                let state = state.clone();
                                move |subject, key, _type, value| {
                                    if let (0, Some(key)) = (subject, key) {
                                        state.borrow_mut().graph.defaults.apply(key, value);
                                    }
                                    0
                                }
                            })
                            .register();
                        state.borrow_mut().metadata = Some((metadata, listener));
                    }
                    _ => {}
                }
            }
        })
        .register();

    let core_listener = core
        .add_listener_local()
        .done({
            let state = state.clone();
            let core = core.downgrade();
            let mainloop = mainloop.downgrade();
            move |id, seq| {
                if id != pw::core::PW_ID_CORE || state.borrow().pending != Some(seq.seq()) {
                    return;
                }
                let mut s = state.borrow_mut();
                s.phase += 1;
                s.pending = None;
                if s.phase < 2 {
                    if let Some(next) = core.upgrade().and_then(|c| c.sync(0).ok()) {
                        s.pending = Some(next.seq());
                        return;
                    }
                }
                if let Some(l) = mainloop.upgrade() {
                    l.quit();
                }
            }
        })
        .error({
            let mainloop = mainloop.downgrade();
            move |id, _seq, res, message| {
                if id == pw::core::PW_ID_CORE {
                    tracing::warn!("PipeWire error {res}: {message}");
                    if let Some(l) = mainloop.upgrade() {
                        l.quit();
                    }
                }
            }
        })
        .register();

    let timer = mainloop.loop_().add_timer({
        let state = state.clone();
        let mainloop = mainloop.downgrade();
        move |_| {
            state.borrow_mut().timed_out = true;
            if let Some(l) = mainloop.upgrade() {
                l.quit();
            }
        }
    });
    let _ = timer.update_timer(Some(TIMEOUT), None);

    state.borrow_mut().pending = Some(core.sync(0).context("PipeWire roundtrip")?.seq());
    mainloop.run();
    if state.borrow().phase < 2 {
        bail!(if state.borrow().timed_out { "PipeWire did not answer" } else { "PipeWire connection failed" });
    }

    let result = {
        let s = state.borrow();
        act(&s.graph, s.metadata.as_ref().map(|(m, _)| m))
    };
    // Flush writes before disconnecting.
    if result.is_ok() {
        state.borrow_mut().phase = 1;
        state.borrow_mut().pending = Some(core.sync(0).context("PipeWire roundtrip")?.seq());
        let _ = timer.update_timer(Some(TIMEOUT), None);
        mainloop.run();
    }
    drop(timer);
    drop(core_listener);
    drop(registry_listener);
    state.borrow_mut().metadata = None;
    result
}

/// The current device landscape.
pub fn current_devices() -> Result<crate::endpoints::DeviceSummary> {
    with_graph(|graph, _| Ok(graph.summary()))
}

/// Makes the node named `id` the configured default sink or source (by its media class). Our
/// virtual mic may be absent (service off or still starting): the session manager applies the
/// choice as soon as it appears.
pub fn set_default(id: &str) -> Result<()> {
    with_graph(|graph, metadata| {
        let metadata = metadata.ok_or_else(|| anyhow!("no \"default\" metadata (is WirePlumber running?)"))?;
        let key = match graph.find(id) {
            Some((_, n)) if n.class == "Audio/Sink" => "default.configured.audio.sink",
            Some((_, n)) if n.class.starts_with("Audio/Source") || n.class == "Audio/Duplex" => {
                "default.configured.audio.source"
            }
            None if id == MIC_NODE_NAME => "default.configured.audio.source",
            _ => bail!("no audio device named {id}"),
        };
        metadata.set_property(0, key, Some("Spa:String:JSON"), Some(&node_name_json(id)));
        Ok(())
    })
}
