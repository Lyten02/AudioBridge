//! Delivers status JSON and remote-control requests to the Kotlin `StatusListener` from one dedicated native
//! thread that is attached to the JavaVM for its whole life. Status calls are gated by [`NotifyGate`]: ≥250 ms
//! apart, statistics-only changes ≥2 s apart, nothing when the view is unchanged. Remote-control requests are
//! delivered immediately and in order (`onRemoteMic` / `onRemoteVolume`).

use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::Instant;

use audiobridge_core::session::PhoneRequest;
use jni::objects::{GlobalRef, JValue};
use jni::{JNIEnv, JavaVM};

use crate::status::{Gate, NotifyGate, StatusView};

enum Msg {
    View(StatusView),
    Listener(Option<GlobalRef>),
    Remote(PhoneRequest),
}

#[derive(Clone)]
pub struct Listener {
    tx: Sender<Msg>,
}

impl Listener {
    pub fn spawn(vm: JavaVM, initial: StatusView) -> std::io::Result<Listener> {
        let (tx, rx) = mpsc::channel::<Msg>();
        std::thread::Builder::new().name("ab-listener".into()).spawn(move || {
            let mut env = match vm.attach_current_thread_permanently() {
                Ok(env) => env,
                Err(e) => {
                    log::error!("listener thread cannot attach to the JavaVM: {e}");
                    return;
                }
            };
            let mut current = initial;
            let mut listener: Option<GlobalRef> = None;
            let mut gate = NotifyGate::default();
            let mut deadline: Option<Instant> = None;
            loop {
                let msg = match deadline {
                    Some(at) => match rx.recv_timeout(at.saturating_duration_since(Instant::now())) {
                        Ok(m) => Some(m),
                        Err(RecvTimeoutError::Timeout) => None,
                        Err(RecvTimeoutError::Disconnected) => break,
                    },
                    None => match rx.recv() {
                        Ok(m) => Some(m),
                        Err(_) => break,
                    },
                };
                // Coalesce everything already queued; only the newest view matters. Remote requests are never
                // coalesced: each one goes to the listener that is current at that point of the queue.
                for m in msg.into_iter().chain(rx.try_iter()) {
                    match m {
                        Msg::View(v) => current = v,
                        Msg::Listener(l) => {
                            listener = l;
                            gate.reset();
                        }
                        Msg::Remote(req) => match &listener {
                            Some(target) => deliver_remote(&mut env, target, req),
                            None => log::warn!("remote request {req:?} dropped: no listener"),
                        },
                    }
                }
                deadline = None;
                let Some(target) = &listener else { continue };
                let now = Instant::now();
                match gate.poll(now, &current) {
                    Gate::Send => {
                        deliver(&mut env, target, &current.to_json());
                        gate.mark_sent(now, current.clone());
                    }
                    Gate::WaitUntil(at) => deadline = Some(at),
                    Gate::Idle => {}
                }
            }
        })?;
        Ok(Listener { tx })
    }

    pub fn update(&self, view: StatusView) {
        let _ = self.tx.send(Msg::View(view));
    }

    pub fn set(&self, listener: Option<GlobalRef>) {
        let _ = self.tx.send(Msg::Listener(listener));
    }

    /// A PC asks to change a phone-side control; Kotlin applies it and reports the result.
    pub fn remote(&self, req: PhoneRequest) {
        let _ = self.tx.send(Msg::Remote(req));
    }
}

fn deliver(env: &mut JNIEnv, listener: &GlobalRef, json: &str) {
    let result = env.with_local_frame(4, |env| -> jni::errors::Result<()> {
        let s = env.new_string(json)?;
        env.call_method(listener, "onStatus", "(Ljava/lang/String;)V", &[JValue::Object(&s)])?;
        Ok(())
    });
    if let Err(e) = result {
        call_failed(env, "onStatus", &e);
    }
}

/// `onRemoteMic(Z)V` / `onRemoteVolume(I)V`: void calls with primitive arguments create no local references.
fn deliver_remote(env: &mut JNIEnv, listener: &GlobalRef, req: PhoneRequest) {
    let (method, sig, arg) = match req {
        PhoneRequest::Mic(on) => ("onRemoteMic", "(Z)V", JValue::from(on)),
        PhoneRequest::Volume(percent) => ("onRemoteVolume", "(I)V", JValue::Int(i32::from(percent))),
    };
    if let Err(e) = env.call_method(listener, method, sig, &[arg]) {
        call_failed(env, method, &e);
    }
}

/// Clears a pending Java exception (this thread keeps calling into Java) and logs the failure.
fn call_failed(env: &mut JNIEnv, method: &str, e: &jni::errors::Error) {
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_describe();
        let _ = env.exception_clear();
    }
    log::warn!("StatusListener.{method} failed: {e}");
}
