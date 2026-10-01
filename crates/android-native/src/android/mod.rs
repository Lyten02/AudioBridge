//! Android-only runtime: logcat logging, AAudio streams, the audio controller thread,
//! the hub manager (tokio), the listener thread and the JNI exports.

mod aaudio;
mod controller;
mod engine;
mod jni_api;
mod listener;
mod logging;
