//! Logcat output under tag `AudioBridge`. Core's `tracing` events reach `log` through tracing's `log`
//! feature (no tracing subscriber is installed), so one logger covers both.

use std::sync::Once;

use android_logger::{Config, FilterBuilder};
use log::LevelFilter;

const TAG: &str = "AudioBridge";
/// Networking dependencies are chatty at info; keep them to warnings. `tracing::span*` are the span
/// enter/exit records emitted by tracing's `log` feature (several per audio frame) — pure noise and battery.
const FILTER: &str = "info,tracing::span=off,iroh=warn,quinn=warn,rustls=warn,hickory_proto=warn,hickory_resolver=warn,netwatch=warn,portmapper=warn,swarm_discovery=warn";

pub fn init() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        android_logger::init_once(
            Config::default()
                .with_max_level(LevelFilter::Info)
                .with_tag(TAG)
                .with_filter(FilterBuilder::new().parse(FILTER).build())
                .format(|f, record| write!(f, "{}: {}", record.target(), record.args())),
        );
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            log::error!("panic: {info}");
            previous(info);
        }));
    });
}
