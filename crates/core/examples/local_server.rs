//! Minimal PC-side server for testing phones/probes without the desktop app.
//!
//! ```text
//! cargo run -p audiobridge-core --example local_server -- [--seconds N] [--sine HZ] [--local] [--mic-demand] [--media]
//! ```
//! Prints the pairing URI, streams a sine as "PC audio" while a phone is connected and reports
//! the RMS of the received microphone every second. Every request from the phone is printed
//! (`REQUEST …`). `--media` also reports a fake media player that follows the phone's media
//! commands (headphone buttons), so the phone side can be tested without a real PC player.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use audiobridge_core::session::{MediaCommand, NetOptions, PcMedia, PcRequest, Playback, Server, ServerConfig};
use parking_lot::Mutex;

const BLOCK: usize = 480;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,audiobridge_core=info".into()),
        )
        .init();
    let mut seconds = 30u64;
    let mut sine = 1000.0f64;
    let mut opts = NetOptions::default();
    let mut demand = false;
    let mut media = false;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--seconds" => seconds = it.next().context("--seconds N")?.parse()?,
            "--sine" => sine = it.next().context("--sine HZ")?.parse()?,
            "--local" => opts = NetOptions::local_only(),
            "--mic-demand" => demand = true,
            "--media" => media = true,
            s => bail!("unknown argument {s}"),
        }
    }
    let data_dir = std::env::temp_dir().join("audiobridge-local-server");
    let server = Server::start_with_options(
        ServerConfig {
            data_dir,
            pc_name: "local_server".into(),
        },
        opts,
    )
    .await?;
    server.set_mic_demand(demand);
    let mut requests = server.take_requests().context("requests")?;
    let fake_media = |playing: bool, track: u32| PcMedia {
        playback: if playing { Playback::Playing } else { Playback::Paused },
        app: "Тестовый плеер".into(),
        title: format!("Трек {track}"),
        artist: "local_server".into(),
    };
    let (mut playing, mut track) = (true, 1u32);
    if media {
        server.set_media(fake_media(playing, track));
    }
    let mut pairing = server.pairing();
    tokio::spawn(async move {
        loop {
            if let Some(p) = pairing.borrow_and_update().as_ref() {
                println!("PAIRING_URI={}", p.to_uri());
            }
            if pairing.changed().await.is_err() {
                return;
            }
        }
    });

    let stop = Arc::new(AtomicBool::new(false));
    let mut cap = server.pc_audio_capture();
    let st = stop.clone();
    let cap_thread = std::thread::spawn(move || {
        let mut buf = vec![0f32; BLOCK * 2];
        let step = sine / 48_000.0 * std::f64::consts::TAU;
        let mut phase = 0f64;
        let start = Instant::now();
        let mut n = 0u64;
        while !st.load(Ordering::Relaxed) {
            n += 1;
            for f in buf.as_chunks_mut::<2>().0 {
                f.fill((phase.sin() * 0.5) as f32);
                phase += step;
            }
            if let Some(d) = (start + Duration::from_millis(10 * n)).checked_duration_since(Instant::now()) {
                std::thread::sleep(d);
            }
            cap.push(&buf);
        }
    });

    let mic_level = Arc::new(Mutex::new((0f64, 0u64)));
    let mut mic = server.mic_playout();
    let (st, lvl) = (stop.clone(), mic_level.clone());
    let mic_thread = std::thread::spawn(move || {
        let mut buf = vec![0f32; BLOCK];
        let start = Instant::now();
        let mut n = 0u64;
        while !st.load(Ordering::Relaxed) {
            n += 1;
            if let Some(d) = (start + Duration::from_millis(10 * n)).checked_duration_since(Instant::now()) {
                std::thread::sleep(d);
            }
            mic.fill(&mut buf);
            let mut l = lvl.lock();
            l.0 += buf.iter().map(|s| (*s as f64).powi(2)).sum::<f64>();
            l.1 += buf.len() as u64;
        }
    });

    let status = server.status();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.tick().await;
    let mut sec = 0u64;
    while sec < seconds {
        tokio::select! {
            _ = tick.tick() => {
                sec += 1;
                let s = status.borrow().clone();
                let (sum, n) = std::mem::take(&mut *mic_level.lock());
                let rms = if n == 0 { 0.0 } else { (sum / n as f64).sqrt() };
                println!(
                    "[{sec:>3}s] {:?} peer={:?} path={:?} rtt={:?} | tx {:.0} kbps active={} | mic rms={rms:.3} active={} buf={:.0}ms",
                    s.state, s.peer_name, s.path, s.rtt_ms, s.pc_audio.kbps, s.pc_audio.active, s.mic.active, s.mic.buffer_ms
                );
            }
            Some(req) = requests.recv() => {
                println!("REQUEST {req:?}");
                if let (true, PcRequest::Media(cmd)) = (media, req) {
                    match cmd {
                        MediaCommand::Play => playing = true,
                        MediaCommand::Pause => playing = false,
                        MediaCommand::PlayPause => playing = !playing,
                        MediaCommand::Next => track += 1,
                        MediaCommand::Previous => track = track.saturating_sub(1).max(1),
                    }
                    println!("MEDIA {} track {track}", if playing { "playing" } else { "paused" });
                    server.set_media(fake_media(playing, track));
                }
            }
        }
    }
    stop.store(true, Ordering::Relaxed);
    let _ = cap_thread.join();
    let _ = mic_thread.join();
    server.shutdown().await;
    Ok(())
}
