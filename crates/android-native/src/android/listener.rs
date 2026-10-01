//! Delivers status JSON to the Kotlin `StatusListener` from one dedicated native thread that is attached
//! to the JavaVM for its whole life. Calls are gated by [`NotifyGate`]: ≥250 ms apart, statistics-only
//! changes ≥2 s apart, nothing when the view is unchanged.

use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::Instant;

use jni::objects::{GlobalRef, JValue};
use jni::{JNIEnv, JavaVM};

use crate::status::{Gate, NotifyGate, StatusView};

enum Msg {
    View(StatusView),
    Listener(Option<GlobalRef>),
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
                // Coalesce everything already queued; only the newest view matters.
                for m in msg.into_iter().chain(rx.try_iter()) {
                    match m {
                        Msg::View(v) => current = v,
                        Msg::Listener(l) => {
                            listener = l;
                            gate.reset();
                        }
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
}

fn deliver(env: &mut JNIEnv, listener: &GlobalRef, json: &str) {
    let result = env.with_local_frame(4, |env| -> jni::errors::Result<()> {
        let s = env.new_string(json)?;
        env.call_method(listener, "onStatus", "(Ljava/lang/String;)V", &[JValue::Object(&s)])?;
        Ok(())
    });
    if let Err(e) = result {
        if env.exception_check().unwrap_or(false) {
            let _ = env.exception_describe();
            let _ = env.exception_clear();
        }
        log::warn!("StatusListener.onStatus failed: {e}");
    }
}
