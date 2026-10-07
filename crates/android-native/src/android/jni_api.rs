//! JNI exports for Kotlin `object app.audiobridge.NativeBridge` (instance methods: `(env, this, ...)`).
//! Every export catches panics and returns a neutral value instead of aborting the app.

use std::panic::{catch_unwind, AssertUnwindSafe};

use audiobridge_core::pairing::PairingInfo;
use jni::objects::{JObject, JString};
use jni::sys::{jboolean, jint, jstring, JNI_FALSE};
use jni::JNIEnv;

use super::engine::{self, Cmd};
use super::logging;
use crate::{controls, peers};
use crate::status::{PairingView, StatusView};

fn guard<R>(name: &str, fallback: R, f: impl FnOnce() -> R) -> R {
    logging::init();
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(_) => {
            log::error!("NativeBridge.{name} panicked; returning fallback");
            fallback
        }
    }
}

fn read_string(env: &mut JNIEnv, s: &JString) -> Option<String> {
    if s.is_null() {
        return None;
    }
    match env.get_string(s) {
        Ok(js) => Some(js.into()),
        Err(e) => {
            log::warn!("cannot read Java string: {e}");
            None
        }
    }
}

fn new_jstring(env: &mut JNIEnv, s: &str) -> jstring {
    match env.new_string(s) {
        Ok(js) => js.into_raw(),
        Err(e) => {
            log::error!("cannot create Java string: {e}");
            std::ptr::null_mut()
        }
    }
}

fn engine_or_log(name: &str) -> Option<&'static engine::Engine> {
    let e = engine::get();
    if e.is_none() {
        log::warn!("NativeBridge.{name} called before init");
    }
    e
}

/// Makes the process-wide Android context available to crates that use `ndk-context` (iroh's DNS
/// resolver reads the system DNS servers through it). Installed once, for the lifetime of the process.
fn install_ndk_context(env: &mut JNIEnv) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let app = env
            .call_static_method("android/app/ActivityThread", "currentApplication", "()Landroid/app/Application;", &[])
            .and_then(|v| v.l());
        let app = match app {
            Ok(app) if !app.is_null() => app,
            Ok(_) => {
                log::warn!("ndk-context: no current Application");
                return;
            }
            Err(e) => {
                let _ = env.exception_clear();
                log::warn!("ndk-context: cannot get Application: {e}");
                return;
            }
        };
        let (global, vm) = match (env.new_global_ref(&app), env.get_java_vm()) {
            (Ok(global), Ok(vm)) => (global, vm),
            (Err(e), _) | (_, Err(e)) => {
                log::warn!("ndk-context: {e}");
                return;
            }
        };
        let context = global.as_obj().as_raw();
        // ndk-context keeps the raw reference for the whole process; never release it.
        std::mem::forget(global);
        // SAFETY: both pointers are valid for the process lifetime and `ONCE` guarantees a single call.
        unsafe { ndk_context::initialize_android_context(vm.get_java_vm_pointer().cast(), context.cast()) };
    });
}

#[no_mangle]
pub extern "system" fn Java_app_audiobridge_NativeBridge_init<'l>(
    mut env: JNIEnv<'l>,
    _this: JObject<'l>,
    files_dir: JString<'l>,
    device_name: JString<'l>,
) {
    guard("init", (), || {
        let (Some(dir), Some(name)) = (read_string(&mut env, &files_dir), read_string(&mut env, &device_name)) else {
            log::error!("init: null arguments");
            return;
        };
        let vm = match env.get_java_vm() {
            Ok(vm) => vm,
            Err(e) => {
                log::error!("init: no JavaVM: {e}");
                return;
            }
        };
        install_ndk_context(&mut env);
        if let Err(e) = engine::init(vm, &dir, &name) {
            log::error!("init failed: {e}");
        }
    })
}

#[no_mangle]
pub extern "system" fn Java_app_audiobridge_NativeBridge_parsePairing<'l>(
    mut env: JNIEnv<'l>,
    _this: JObject<'l>,
    uri: JString<'l>,
) -> jstring {
    guard("parsePairing", std::ptr::null_mut(), || {
        let Some(uri) = read_string(&mut env, &uri) else { return std::ptr::null_mut() };
        match PairingInfo::from_uri(uri.trim()) {
            Ok(info) => new_jstring(&mut env, &PairingView { id: &info.peer_id(), name: info.pc_name() }.to_json()),
            Err(e) => {
                log::info!("invalid pairing uri: {e:#}");
                std::ptr::null_mut()
            }
        }
    })
}

/// `urisJson`: JSON array with the full set of pairing URIs; `[]` disconnects all PCs. Invalid URIs are
/// skipped; a malformed array leaves the current set unchanged.
#[no_mangle]
pub extern "system" fn Java_app_audiobridge_NativeBridge_setPeers<'l>(
    mut env: JNIEnv<'l>,
    _this: JObject<'l>,
    uris_json: JString<'l>,
) {
    guard("setPeers", (), || {
        let Some(engine) = engine_or_log("setPeers") else { return };
        let Some(json) = read_string(&mut env, &uris_json) else { return };
        let uris = match peers::parse_string_list(&json) {
            Ok(uris) => uris,
            Err(e) => {
                log::error!("setPeers: {e}");
                return;
            }
        };
        let infos = uris.iter().filter_map(|uri| match PairingInfo::from_uri(uri.trim()) {
            Ok(info) => Some(info),
            Err(e) => {
                log::warn!("setPeers: skipping invalid pairing uri: {e:#}");
                None
            }
        });
        engine.send(Cmd::SetPeers(peers::dedupe(infos, PairingInfo::peer_id)));
    })
}

/// `idsJson`: JSON array with the full set of muted peer ids; `[]` unmutes all PCs. A malformed array leaves the
/// current set unchanged.
#[no_mangle]
pub extern "system" fn Java_app_audiobridge_NativeBridge_setMuted<'l>(
    mut env: JNIEnv<'l>,
    _this: JObject<'l>,
    ids_json: JString<'l>,
) {
    guard("setMuted", (), || {
        let Some(engine) = engine_or_log("setMuted") else { return };
        let Some(json) = read_string(&mut env, &ids_json) else { return };
        match peers::parse_string_list(&json) {
            Ok(ids) => engine.send(Cmd::SetMuted(ids)),
            Err(e) => log::error!("setMuted: {e}"),
        }
    })
}

/// `enabled`: the user's mic switch; `ready`: RECORD_AUDIO is granted and the service holds the microphone FGS
/// type. Capture needs both; every PC sees both.
#[no_mangle]
pub extern "system" fn Java_app_audiobridge_NativeBridge_setMicState<'l>(
    _env: JNIEnv<'l>,
    _this: JObject<'l>,
    enabled: jboolean,
    ready: jboolean,
) {
    guard("setMicState", (), || {
        if let Some(engine) = engine_or_log("setMicState") {
            engine.send(Cmd::MicState { enabled: enabled != JNI_FALSE, ready: ready != JNI_FALSE });
        }
    })
}

/// Phone media volume in percent reported to the PCs; negative = unknown, above 100 is clamped.
#[no_mangle]
pub extern "system" fn Java_app_audiobridge_NativeBridge_setVolume<'l>(
    _env: JNIEnv<'l>,
    _this: JObject<'l>,
    percent: jint,
) {
    guard("setVolume", (), || {
        if let Some(engine) = engine_or_log("setVolume") {
            engine.send(Cmd::Volume(controls::volume_percent(percent)));
        }
    })
}

/// Remote control of the PC `peerId`: `action` is a `NativeBridge.PC_*` constant, `value` 0/1 for switches and
/// `0..=100` for the volume (see [`controls::pc_request`]). Dropped (logged) if that PC is not connected.
#[no_mangle]
pub extern "system" fn Java_app_audiobridge_NativeBridge_controlPc<'l>(
    mut env: JNIEnv<'l>,
    _this: JObject<'l>,
    peer_id: JString<'l>,
    action: jint,
    value: jint,
) {
    guard("controlPc", (), || {
        let Some(engine) = engine_or_log("controlPc") else { return };
        let Some(peer_id) = read_string(&mut env, &peer_id) else { return };
        match controls::pc_request(action, value) {
            Some(req) => engine.send(Cmd::ControlPc { peer_id, req }),
            None => log::warn!("controlPc: unknown action {action}"),
        }
    })
}

#[no_mangle]
pub extern "system" fn Java_app_audiobridge_NativeBridge_networkChanged<'l>(_env: JNIEnv<'l>, _this: JObject<'l>) {
    guard("networkChanged", (), || {
        if let Some(engine) = engine_or_log("networkChanged") {
            engine.send(Cmd::NetworkChanged);
        }
    })
}

#[no_mangle]
pub extern "system" fn Java_app_audiobridge_NativeBridge_statusJson<'l>(
    mut env: JNIEnv<'l>,
    _this: JObject<'l>,
) -> jstring {
    guard("statusJson", std::ptr::null_mut(), || {
        let json = match engine::get() {
            Some(engine) => engine.status_json(),
            None => StatusView::idle().to_json(),
        };
        new_jstring(&mut env, &json)
    })
}

#[no_mangle]
pub extern "system" fn Java_app_audiobridge_NativeBridge_setListener<'l>(
    env: JNIEnv<'l>,
    _this: JObject<'l>,
    listener: JObject<'l>,
) {
    guard("setListener", (), || {
        let Some(engine) = engine_or_log("setListener") else { return };
        let listener = if listener.is_null() {
            None
        } else {
            match env.new_global_ref(&listener) {
                Ok(g) => Some(g),
                Err(e) => {
                    log::error!("setListener: cannot create global ref: {e}");
                    return;
                }
            }
        };
        engine.set_listener(listener);
    })
}
