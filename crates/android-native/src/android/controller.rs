//! Audio controller: one plain thread that owns both AAudio streams and the core handles.
//!
//! All opens/closes happen here, never on an AAudio callback thread (AAudio forbids closing a stream from
//! its own callbacks). Error callbacks (device disconnect, route change to/from Bluetooth) post an event;
//! the thread closes the dead stream and reopens it on the new default route.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use audiobridge_core::audio::{CaptureHandle, PlayoutHandle};

use super::aaudio::{result_text, Dir, Endpoint, ErrorSink, Stream};
use crate::policy::{Backoff, OutputPolicy};

/// A stream that survived this long before failing is considered healthy; its retry starts from scratch.
const HEALTHY_STREAM_AGE: Duration = Duration::from_secs(5);

pub enum Event {
    /// New handles from a freshly started hub (replaces any previous ones).
    Attach { playout: PlayoutHandle, capture: CaptureHandle },
    /// Hub gone: close both streams now and drop the handles.
    Detach,
    /// Latest demand derived from the hub status.
    Update { pc_active: bool, mic_wanted: bool },
    StreamError { dir: Dir, gen: u64, code: i32 },
}

/// Starts the controller thread. `mic_capturing` mirrors whether the input stream is running;
/// `on_capture_change` is called whenever it flips.
pub fn spawn(
    mic_capturing: Arc<AtomicBool>,
    on_capture_change: impl Fn() + Send + 'static,
) -> std::io::Result<Sender<Event>> {
    let (tx, rx) = mpsc::channel();
    let err_tx = tx.clone();
    let sink: ErrorSink = Arc::new(move |dir, gen, code| {
        let _ = err_tx.send(Event::StreamError { dir, gen, code });
    });
    std::thread::Builder::new().name("ab-audio".into()).spawn(move || {
        Controller {
            rx,
            sink,
            mic_capturing,
            on_capture_change: Box::new(on_capture_change),
            output: Slot::default(),
            input: Slot::default(),
            output_policy: OutputPolicy::default(),
            pc_active: false,
            mic_wanted: false,
            gen: 0,
        }
        .run()
    })?;
    Ok(tx)
}

struct Controller {
    rx: Receiver<Event>,
    sink: ErrorSink,
    mic_capturing: Arc<AtomicBool>,
    on_capture_change: Box<dyn Fn() + Send>,
    output: Slot<PlayoutHandle>,
    input: Slot<CaptureHandle>,
    output_policy: OutputPolicy,
    pc_active: bool,
    mic_wanted: bool,
    gen: u64,
}

impl Controller {
    fn run(mut self) {
        loop {
            let now = Instant::now();
            let want_output = self.output.attached() && self.output_policy.update(self.pc_active, now);
            let want_input = self.input.attached() && self.mic_wanted;
            self.output.reconcile(want_output, now, &mut self.gen, &self.sink);
            self.input.reconcile(want_input, now, &mut self.gen, &self.sink);

            let capturing = self.input.stream.is_some();
            if self.mic_capturing.swap(capturing, Ordering::SeqCst) != capturing {
                (self.on_capture_change)();
            }

            let deadline = [
                if want_output { self.output_policy.deadline() } else { None },
                self.output.retry_deadline(want_output),
                self.input.retry_deadline(want_input),
            ]
            .into_iter()
            .flatten()
            .min();
            let event = match deadline {
                Some(at) => match self.rx.recv_timeout(at.saturating_duration_since(now)) {
                    Ok(e) => e,
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => break,
                },
                None => match self.rx.recv() {
                    Ok(e) => e,
                    Err(_) => break,
                },
            };
            self.handle(event);
        }
    }

    fn handle(&mut self, event: Event) {
        match event {
            Event::Attach { playout, capture } => {
                self.output.attach(playout);
                self.input.attach(capture);
                self.output_policy.reset();
            }
            Event::Detach => {
                self.output.detach();
                self.input.detach();
                self.output_policy.reset();
                self.pc_active = false;
                self.mic_wanted = false;
            }
            Event::Update { pc_active, mic_wanted } => {
                self.pc_active = pc_active;
                self.mic_wanted = mic_wanted;
            }
            Event::StreamError { dir, gen, code } => {
                let now = Instant::now();
                match dir {
                    Dir::Output => self.output.on_error(gen, code, now),
                    Dir::Input => self.input.on_error(gen, code, now),
                }
            }
        }
    }
}

/// One direction: either the handle is parked (stream closed) or owned by the open stream.
struct Slot<H: Endpoint> {
    parked: Option<H>,
    stream: Option<Stream<H>>,
    retry_at: Option<Instant>,
    backoff: Backoff,
}

impl<H: Endpoint> Default for Slot<H> {
    fn default() -> Self {
        Slot { parked: None, stream: None, retry_at: None, backoff: Backoff::default() }
    }
}

impl<H: Endpoint> Slot<H> {
    fn attached(&self) -> bool {
        self.parked.is_some() || self.stream.is_some()
    }

    fn reconcile(&mut self, want: bool, now: Instant, gen: &mut u64, sink: &ErrorSink) {
        if !want {
            if let Some(stream) = self.stream.take() {
                log::info!("{:?} stream closing", H::DIR);
                self.parked = Some(stream.close());
            }
            self.retry_at = None;
            self.backoff.reset();
            return;
        }
        if self.stream.is_some() || self.retry_at.is_some_and(|at| now < at) {
            return;
        }
        let Some(handle) = self.parked.take() else { return };
        *gen += 1;
        match Stream::open(handle, *gen, sink.clone()) {
            Ok(stream) => {
                self.stream = Some(stream);
                self.retry_at = None;
            }
            Err((handle, reason)) => {
                self.parked = Some(handle);
                let delay = self.backoff.fail();
                self.retry_at = Some(now + delay);
                log::warn!("{:?} stream open failed ({reason}); retrying in {delay:?}", H::DIR);
            }
        }
    }

    fn retry_deadline(&self, want: bool) -> Option<Instant> {
        if want && self.stream.is_none() && self.parked.is_some() {
            self.retry_at
        } else {
            None
        }
    }

    fn on_error(&mut self, gen: u64, code: i32, now: Instant) {
        let Some(stream) = self.stream.take_if(|s| s.gen() == gen) else {
            return; // stale event from an already closed stream
        };
        if stream.age() >= HEALTHY_STREAM_AGE {
            self.backoff.reset();
        }
        self.parked = Some(stream.close());
        let delay = self.backoff.fail();
        self.retry_at = Some(now + delay);
        log::info!("{:?} stream error: {}; reopening in {delay:?}", H::DIR, result_text(code));
    }

    fn attach(&mut self, handle: H) {
        self.detach();
        self.parked = Some(handle);
    }

    fn detach(&mut self) {
        if let Some(stream) = self.stream.take() {
            drop(stream.close());
        }
        self.parked = None;
        self.retry_at = None;
        self.backoff.reset();
    }
}
