//! Phone emulator for end-to-end testing against a running PC app.
//!
//! ```text
//! cargo run -p audiobridge-core --example probe -- <pairing-uri> [--seconds N] [--mic-sine HZ] [--local]
//! ```
//! Connects as a phone (a [`Hub`] with this single PC), prints status/path/RTT every second,
//! reports RMS/peak of the received PC audio and optionally sends a sine as the microphone
//! (sent only while the PC demands the mic).
//! `--local` disables relay and discovery (direct addresses from the pairing code only).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use audiobridge_core::pairing::PairingInfo;
use audiobridge_core::session::{ConnState, Hub, HubConfig, NetOptions, StreamStats};
use parking_lot::Mutex;

const BLOCK: usize = 480;

#[derive(Default)]
struct Meter {
    sum_sq: f64,
    count: u64,
    peak: f32,
}

impl Meter {
    fn take(&mut self) -> (f32, f32) {
        let rms = if self.count == 0 {
            0.0
        } else {
            (self.sum_sq / self.count as f64).sqrt() as f32
        };
        let peak = self.peak;
        *self = Meter::default();
        (rms, peak)
    }
}

fn dbfs(v: f32) -> String {
    if v <= 1e-6 {
        "-inf".into()
    } else {
        format!("{:.1}", 20.0 * v.log10())
    }
}

fn stats(s: &StreamStats) -> String {
    format!(
        "active={} buf={:.0}ms kbps={:.0} underruns={} lost={}",
        s.active, s.buffer_ms, s.kbps, s.underruns, s.lost_packets
    )
}

struct Args {
    uri: String,
    seconds: u64,
    mic_sine: Option<f64>,
    local: bool,
}

fn parse_args() -> Result<Args> {
    let mut it = std::env::args().skip(1);
    let mut uri = None;
    let mut seconds = 20;
    let mut mic_sine = None;
    let mut local = false;
    while let Some(a) = it.next() {
        match a.as_str() {
            "--seconds" => seconds = it.next().context("--seconds N")?.parse()?,
            "--mic-sine" => mic_sine = Some(it.next().context("--mic-sine HZ")?.parse()?),
            "--local" => local = true,
            s if s.starts_with("--") => bail!("unknown option {s}"),
            s => uri = Some(s.to_owned()),
        }
    }
    let Some(uri) = uri else {
        bail!("usage: probe <pairing-uri> [--seconds N] [--mic-sine HZ] [--local]");
    };
    Ok(Args {
        uri,
        seconds,
        mic_sine,
        local,
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,audiobridge_core=info".into()),
        )
        .init();
    let args = parse_args()?;
    let pairing = PairingInfo::from_uri(&args.uri)?;
    println!(
        "pairing: pc={} id={} addrs={:?} relay={:?}",
        pairing.pc_name(),
        pairing.endpoint_id(),
        pairing.addrs(),
        pairing.relay_url().map(|u| u.to_string())
    );
    let data_dir = std::env::temp_dir().join("audiobridge-probe");
    let opts = if args.local {
        NetOptions::local_only()
    } else {
        NetOptions::default()
    };
    let hub = Hub::start_with_options(
        HubConfig {
            data_dir,
            device_name: "probe".into(),
        },
        opts,
    )
    .await?;
    hub.set_peers(vec![pairing]);
    hub.set_mic_enabled(args.mic_sine.is_some());

    let stop = Arc::new(AtomicBool::new(false));
    let meter = Arc::new(Mutex::new(Meter::default()));

    // Playout "device": pulls 10 ms blocks in real time.
    let mut playout = hub.pc_audio_playout();
    let (m, st) = (meter.clone(), stop.clone());
    let out_thread = std::thread::spawn(move || {
        let ch = playout.channels();
        let mut buf = vec![0f32; BLOCK * ch];
        let start = Instant::now();
        let mut n = 0u64;
        while !st.load(Ordering::Relaxed) {
            n += 1;
            if let Some(d) = (start + Duration::from_millis(10 * n)).checked_duration_since(Instant::now()) {
                std::thread::sleep(d);
            }
            playout.fill(&mut buf);
            let mut m = m.lock();
            for s in &buf {
                m.sum_sq += (*s as f64).powi(2);
                m.peak = m.peak.max(s.abs());
            }
            m.count += buf.len() as u64;
        }
    });

    // Microphone "device": pushes a sine in real time.
    let mic_thread = args.mic_sine.map(|freq| {
        let mut cap = hub.mic_capture();
        let st = stop.clone();
        std::thread::spawn(move || {
            let mut buf = vec![0f32; BLOCK];
            let step = freq / 48_000.0 * std::f64::consts::TAU;
            let mut phase = 0f64;
            let start = Instant::now();
            let mut n = 0u64;
            while !st.load(Ordering::Relaxed) {
                n += 1;
                for s in buf.iter_mut() {
                    *s = (phase.sin() * 0.5) as f32;
                    phase += step;
                }
                if let Some(d) = (start + Duration::from_millis(10 * n)).checked_duration_since(Instant::now()) {
                    std::thread::sleep(d);
                }
                cap.push(&buf);
            }
        })
    });

    let status = hub.status();
    let mut ever_connected = false;
    let mut max_rms = 0f32;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.tick().await;
    for sec in 1..=args.seconds {
        tick.tick().await;
        let hs = status.borrow().clone();
        let Some(s) = hs.peers.first().map(|p| p.status.clone()) else {
            bail!("peer missing from hub status");
        };
        ever_connected |= s.state == ConnState::Connected;
        let (rms, peak) = meter.lock().take();
        if s.state == ConnState::Connected {
            max_rms = max_rms.max(rms);
        }
        println!(
            "[{sec:>3}s] {:?} peer={:?} path={:?} rtt={} | pc audio rms={} dBFS peak={} dBFS {} | mic {} demanded={} enabled={} | err={:?}",
            s.state,
            s.peer_name.as_deref().unwrap_or("-"),
            s.path,
            s.rtt_ms.map(|r| format!("{r:.1}ms")).unwrap_or_else(|| "-".into()),
            dbfs(rms),
            dbfs(peak),
            stats(&s.pc_audio),
            stats(&s.mic),
            s.mic_demanded,
            s.mic_enabled,
            s.last_error,
        );
    }
    stop.store(true, Ordering::Relaxed);
    let _ = out_thread.join();
    if let Some(t) = mic_thread {
        let _ = t.join();
    }
    hub.shutdown().await;
    println!(
        "summary: connected={ever_connected} max_rms={} dBFS",
        dbfs(max_rms)
    );
    if !ever_connected {
        bail!("never connected");
    }
    Ok(())
}
