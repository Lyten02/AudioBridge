//! AudioBridge core: transport (iroh/QUIC), pairing, Opus codec and real-time audio buffers
//! shared by the Windows desktop app and the Android app.
//!
//! Requires a tokio runtime (any flavor). Logs through `tracing`.

pub mod audio;
pub mod pairing;
pub mod proto;
pub mod session;
