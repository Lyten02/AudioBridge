//! `libaudiobridge.so`: JNI bridge between the Kotlin app (`app.audiobridge.NativeBridge`) and
//! `audiobridge-core`, plus AAudio playback/capture.
//!
//! `peers`, `policy` and `status` are pure and host-testable; everything touching JNI/AAudio is Android-only.

pub mod peers;
pub mod policy;
pub mod status;

#[cfg(target_os = "android")]
mod android;
