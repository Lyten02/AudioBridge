//! In-process Servers + Hub over localhost (relay and discovery disabled).

use std::time::{Duration, Instant};

use audiobridge_core::audio::{CaptureHandle, PlayoutHandle};
use audiobridge_core::pairing::PairingInfo;
use audiobridge_core::session::{
    ConnState, Hub, HubConfig, HubStatus, NetOptions, PathKind, Server, ServerConfig, Status,
};
use tokio::sync::watch;

const SR: f64 = 48_000.0;
const BLOCK: usize = 480;

fn init_logs() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,audiobridge_core=info".into()),
        )
        .with_test_writer()
        .try_init();
}

async fn wait_for<T: Clone + std::fmt::Debug>(
    rx: &mut watch::Receiver<T>,
    timeout: Duration,
    what: &str,
    pred: impl Fn(&T) -> bool,
) -> T {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        {
            let s = rx.borrow_and_update();
            if pred(&s) {
                return s.clone();
            }
        }
        if tokio::time::timeout_at(deadline, rx.changed()).await.is_err() {
            panic!("timed out waiting for {what}; last status: {:?}", *rx.borrow());
        }
    }
}

/// Status of the PC with `id` inside the hub status.
fn peer<'a>(h: &'a HubStatus, id: &str) -> Option<&'a Status> {
    h.peers.iter().find(|p| p.id == id).map(|p| &p.status)
}

fn connected(h: &HubStatus, id: &str) -> bool {
    peer(h, id).is_some_and(|s| s.state == ConnState::Connected)
}

async fn start_server(dir: &std::path::Path, name: &str, opts: NetOptions) -> Server {
    Server::start_with_options(
        ServerConfig {
            data_dir: dir.to_path_buf(),
            pc_name: name.into(),
        },
        opts,
    )
    .await
    .expect("server start")
}

async fn start_hub(dir: &std::path::Path, peers: Vec<PairingInfo>) -> Hub {
    let hub = Hub::start_with_options(
        HubConfig {
            data_dir: dir.to_path_buf(),
            device_name: "Test Phone".into(),
        },
        NetOptions::local_only(),
    )
    .await
    .expect("hub start");
    hub.set_peers(peers);
    hub
}

fn pairing_of(server: &Server) -> PairingInfo {
    server.pairing().borrow().clone().expect("pairing info")
}

/// Pushes 10 ms blocks of a sine in real time. `loud_from` switches amplitude 0.02 → 0.5 at
/// that block (block-aligned) and records when that block was pushed.
fn capture_clock(
    mut cap: CaptureHandle,
    freq: f64,
    amp: f64,
    blocks: usize,
    loud_from: Option<usize>,
) -> std::thread::JoinHandle<Option<Instant>> {
    std::thread::spawn(move || {
        let ch = cap.channels();
        let mut buf = vec![0f32; BLOCK * ch];
        let mut phase = 0f64;
        let step = freq / SR * std::f64::consts::TAU;
        let start = Instant::now();
        let mut onset = None;
        for b in 0..blocks {
            let a = match loud_from {
                Some(l) if b < l => 0.02,
                _ => amp,
            };
            for frame in buf.chunks_exact_mut(ch) {
                let v = (phase.sin() * a) as f32;
                phase += step;
                frame.fill(v);
            }
            let due = start + Duration::from_micros(10_000 * b as u64);
            if let Some(d) = due.checked_duration_since(Instant::now()) {
                std::thread::sleep(d);
            }
            if loud_from == Some(b) {
                onset = Some(Instant::now());
            }
            cap.push(&buf);
        }
        onset
    })
}

struct Received {
    /// First channel only.
    samples: Vec<f32>,
    /// Playout time of the first sample above `threshold`.
    first_loud: Option<Instant>,
}

/// Pulls 10 ms blocks in real time, like an audio device.
fn playout_clock(
    mut pl: PlayoutHandle,
    blocks: usize,
    threshold: f32,
) -> std::thread::JoinHandle<Received> {
    std::thread::spawn(move || {
        let ch = pl.channels();
        let mut buf = vec![0f32; BLOCK * ch];
        let mut samples = Vec::with_capacity(blocks * BLOCK);
        let mut first_loud = None;
        let start = Instant::now();
        for b in 0..blocks {
            let due = start + Duration::from_micros(10_000 * b as u64);
            if let Some(d) = due.checked_duration_since(Instant::now()) {
                std::thread::sleep(d);
            }
            let t = Instant::now();
            pl.fill(&mut buf);
            for (i, frame) in buf.chunks_exact(ch).enumerate() {
                if first_loud.is_none() && frame[0].abs() > threshold {
                    first_loud = Some(t + Duration::from_secs_f64(i as f64 / SR));
                }
                samples.push(frame[0]);
            }
        }
        Received {
            samples,
            first_loud,
        }
    })
}

async fn join<T: Send + 'static>(h: std::thread::JoinHandle<T>) -> T {
    tokio::task::spawn_blocking(move || h.join().unwrap())
        .await
        .unwrap()
}

fn rms(x: &[f32]) -> f64 {
    (x.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / x.len().max(1) as f64).sqrt()
}

/// Frequency estimate from zero crossings.
fn freq(x: &[f32]) -> f64 {
    let crossings = x
        .windows(2)
        .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
        .count();
    crossings as f64 / 2.0 / (x.len() as f64 / SR)
}

/// Amplitude of the component near `f` Hz. The receiver's drift compensation may shift pitch by
/// up to 1 %, so each 100 ms segment takes the strongest single-bin DFT within ±1.5 % of `f`;
/// the segment results are averaged.
fn tone_amplitude(x: &[f32], f: f64) -> f64 {
    let seg = 4800;
    let segments: Vec<&[f32]> = x.chunks_exact(seg).collect();
    let steps = 30;
    let total: f64 = segments
        .iter()
        .map(|s| {
            (0..=steps)
                .map(|i| dft_amplitude(s, f * (0.985 + 0.03 * i as f64 / steps as f64)))
                .fold(0.0, f64::max)
        })
        .sum();
    total / segments.len() as f64
}

/// Single-bin DFT amplitude of `f` Hz over `x`.
fn dft_amplitude(x: &[f32], f: f64) -> f64 {
    let w = f / SR * std::f64::consts::TAU;
    let (mut re, mut im) = (0.0, 0.0);
    for (n, s) in x.iter().enumerate() {
        re += *s as f64 * (w * n as f64).cos();
        im -= *s as f64 * (w * n as f64).sin();
    }
    2.0 * (re * re + im * im).sqrt() / x.len() as f64
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn audio_both_directions_and_latency() {
    init_logs();
    let sdir = tempfile::tempdir().unwrap();
    let cdir = tempfile::tempdir().unwrap();
    let server = start_server(sdir.path(), "TEST-PC", NetOptions::local_only()).await;
    let pairing = pairing_of(&server);
    let id = pairing.peer_id();
    assert!(pairing.addrs().iter().any(|a| a.ip().is_loopback()));
    let hub = start_hub(cdir.path(), vec![pairing]).await;

    let mut ss = server.status();
    let mut hs = hub.status();
    let h = wait_for(&mut hs, Duration::from_secs(10), "hub connected", |h| connected(h, &id)).await;
    assert_eq!(peer(&h, &id).unwrap().peer_name.as_deref(), Some("TEST-PC"));
    let s = wait_for(&mut ss, Duration::from_secs(10), "server connected", |s| {
        s.state == ConnState::Connected
    })
    .await;
    assert_eq!(s.peer_name.as_deref(), Some("Test Phone"));

    // mic flows only while a PC app demands it
    server.set_mic_demand(true);
    wait_for(&mut hs, Duration::from_secs(5), "mic wanted", |h| h.mic_wanted).await;

    let loud_block = 300; // 3 s of quiet tone, then a step to full level
    let pc_cap = capture_clock(server.pc_audio_capture(), 1000.0, 0.5, 600, Some(loud_block));
    let pc_out = playout_clock(hub.pc_audio_playout(), 600, 0.25);
    let mic_cap = capture_clock(hub.mic_capture(), 440.0, 0.5, 600, None);
    let mic_out = playout_clock(server.mic_playout(), 600, 0.25);

    // while streaming: both sides report a path and the incoming stream as active
    let h = wait_for(&mut hs, Duration::from_secs(5), "pc audio active", |h| {
        h.pc_audio_active
            && peer(h, &id).is_some_and(|s| s.path.is_some() && s.rtt_ms.is_some())
    })
    .await;
    // Loopback test: iroh may pick any local interface address of this machine (loopback, LAN,
    // Tailscale or a VPN/public one), but never the relay (disabled).
    let path = peer(&h, &id).unwrap().path;
    assert!(path.is_some_and(|p| p != PathKind::Relay), "path {path:?}");
    wait_for(&mut ss, Duration::from_secs(5), "mic active", |s| s.mic.active).await;

    let onset = join(pc_cap).await.expect("onset pushed");
    let pc = join(pc_out).await;
    join(mic_cap).await;
    let mic = join(mic_out).await;

    // PC audio: loud segment has the right level and pitch
    let tail = &pc.samples[pc.samples.len() - 2 * 48_000..pc.samples.len() - 24_000];
    let (r, f) = (rms(tail), freq(tail));
    println!("pc audio: rms {r:.3} freq {f:.1} Hz");
    assert!((r - 0.3536).abs() < 0.07, "pc audio rms {r}");
    assert!((f - 1000.0).abs() < 30.0, "pc audio freq {f}");

    let latency = pc.first_loud.expect("loud onset received").duration_since(onset);
    println!(
        "added latency (capture push -> playout output): {:.1} ms",
        latency.as_secs_f64() * 1000.0
    );
    assert!(latency < Duration::from_millis(60), "latency {latency:?}");

    let tail = &mic.samples[mic.samples.len() - 3 * 48_000..mic.samples.len() - 24_000];
    let (r, f) = (rms(tail), freq(tail));
    println!("mic: rms {r:.3} freq {f:.1} Hz");
    assert!((r - 0.3536).abs() < 0.07, "mic rms {r}");
    assert!((f - 440.0).abs() < 15.0, "mic freq {f}");

    let h = hub.status().borrow().clone();
    let c = peer(&h, &id).unwrap().clone();
    let s = server.status().borrow().clone();
    println!("hub peer status: {c:?}\nserver status: {s:?}");
    assert!(c.pc_audio.kbps > 50.0, "hub rx kbps {}", c.pc_audio.kbps);
    assert!(s.pc_audio.kbps > 50.0, "server tx kbps {}", s.pc_audio.kbps);
    assert_eq!(c.pc_audio.lost_packets, 0);

    hub.shutdown().await;
    wait_for(&mut ss, Duration::from_secs(15), "server back to waiting", |s| {
        s.state == ConnState::WaitingForPeer && s.peer_name.is_none()
    })
    .await;
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_pcs_are_mixed_and_mic_goes_only_where_demanded() {
    init_logs();
    let (da, db, dh) = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    let a = start_server(da.path(), "PC-A", NetOptions::local_only()).await;
    let b = start_server(db.path(), "PC-B", NetOptions::local_only()).await;
    let (pa, pb) = (pairing_of(&a), pairing_of(&b));
    let (ida, idb) = (pa.peer_id(), pb.peer_id());
    assert_ne!(ida, idb);
    let hub = start_hub(dh.path(), vec![pa, pb]).await;
    let mut hs = hub.status();
    let h = wait_for(&mut hs, Duration::from_secs(10), "both connected", |h| {
        connected(h, &ida) && connected(h, &idb)
    })
    .await;
    assert_eq!(h.peers.len(), 2);
    assert_eq!(h.peers[0].id, ida, "status follows set_peers order");

    a.set_mic_demand(true);
    wait_for(&mut hs, Duration::from_secs(5), "mic demanded by A only", |h| {
        h.mic_wanted
            && peer(h, &ida).is_some_and(|s| s.mic_demanded)
            && peer(h, &idb).is_some_and(|s| !s.mic_demanded)
    })
    .await;

    let cap_a = capture_clock(a.pc_audio_capture(), 1000.0, 0.4, 400, None);
    let cap_b = capture_clock(b.pc_audio_capture(), 1500.0, 0.4, 400, None);
    let out = playout_clock(hub.pc_audio_playout(), 400, 2.0);
    let mic_cap = capture_clock(hub.mic_capture(), 440.0, 0.5, 400, None);
    let mic_a = playout_clock(a.mic_playout(), 400, 2.0);
    let mic_b = playout_clock(b.mic_playout(), 400, 2.0);

    join(cap_a).await;
    join(cap_b).await;
    join(mic_cap).await;
    let mixed = join(out).await;
    let ra = join(mic_a).await;
    let rb = join(mic_b).await;

    let tail = &mixed.samples[mixed.samples.len() - 2 * 48_000..mixed.samples.len() - 24_000];
    let (a1000, b1500, other) = (
        tone_amplitude(tail, 1000.0),
        tone_amplitude(tail, 1500.0),
        tone_amplitude(tail, 1250.0),
    );
    println!("mix: 1000 Hz {a1000:.3}, 1500 Hz {b1500:.3}, 1250 Hz {other:.3}");
    for p in &hub.status().borrow().peers {
        println!("hub peer {}: {:?}", p.id, p.status.pc_audio);
    }
    assert!((a1000 - 0.4).abs() < 0.08, "PC-A tone amplitude {a1000}");
    assert!((b1500 - 0.4).abs() < 0.08, "PC-B tone amplitude {b1500}");
    assert!(other < 0.05, "spurious energy {other}");
    let peak = tail.iter().fold(0f32, |m, s| m.max(s.abs()));
    assert!(peak < 1.0, "soft limiter must keep the mix below full scale ({peak})");

    let tail_a = &ra.samples[ra.samples.len() - 2 * 48_000..ra.samples.len() - 24_000];
    let tail_b = &rb.samples[rb.samples.len() - 2 * 48_000..];
    println!("mic at A: rms {:.3}; at B: rms {:.5}", rms(tail_a), rms(tail_b));
    assert!((rms(tail_a) - 0.3536).abs() < 0.07, "mic at demanding PC");
    assert_eq!(rms(tail_b), 0.0, "mic must not reach the PC that did not demand it");
    assert!(b.status().borrow().mic.kbps == 0.0);

    hub.shutdown().await;
    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn muted_pc_stays_connected_but_is_left_out_of_the_mix() {
    init_logs();
    let (da, db, dh) = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    let a = start_server(da.path(), "PC-A", NetOptions::local_only()).await;
    let b = start_server(db.path(), "PC-B", NetOptions::local_only()).await;
    let (pa, pb) = (pairing_of(&a), pairing_of(&b));
    let (ida, idb) = (pa.peer_id(), pb.peer_id());
    let hub = start_hub(dh.path(), Vec::new()).await;
    // Muting before the PC is added applies once it is.
    hub.set_muted(vec![idb.clone()]);
    hub.set_peers(vec![pa, pb]);
    let mut hs = hub.status();
    wait_for(&mut hs, Duration::from_secs(10), "both connected, B muted", |h| {
        connected(h, &ida)
            && connected(h, &idb)
            && h.peers.iter().any(|p| p.id == idb && p.muted)
            && h.peers.iter().any(|p| p.id == ida && !p.muted)
    })
    .await;

    // Phase 1 (4 s): B muted, A plays. Phase 2 (2 s): everything muted.
    let cap_a = capture_clock(a.pc_audio_capture(), 1000.0, 0.4, 600, None);
    let cap_b = capture_clock(b.pc_audio_capture(), 1500.0, 0.4, 600, None);
    let out = playout_clock(hub.pc_audio_playout(), 600, 2.0);
    tokio::time::sleep(Duration::from_secs(3)).await;
    let h = wait_for(&mut hs, Duration::from_secs(2), "both streams active", |h| {
        h.peers.iter().all(|p| p.status.pc_audio.active)
    })
    .await;
    assert!(h.pc_audio_active, "unmuted A keeps the output open");
    tokio::time::sleep(Duration::from_millis(1000)).await;
    hub.set_muted(vec![ida.clone(), idb.clone()]);
    let h = hub.status().borrow().clone();
    assert!(
        !h.pc_audio_active && h.peers.iter().all(|p| p.status.pc_audio.active),
        "audio still arrives but nothing needs the output: {h:?}"
    );

    join(cap_a).await;
    join(cap_b).await;
    let mixed = join(out).await;

    // 2.0–3.5 s (first channel, 48 000 samples/s): only A is heard.
    let first = &mixed.samples[2 * 48_000..7 * 24_000];
    let (a1000, b1500) = (tone_amplitude(first, 1000.0), tone_amplitude(first, 1500.0));
    println!("B muted: 1000 Hz {a1000:.3}, 1500 Hz {b1500:.3}");
    assert!((a1000 - 0.4).abs() < 0.08, "PC-A tone amplitude {a1000}");
    assert!(b1500 < 0.02, "muted PC-B must not be heard ({b1500})");
    // Last second: silence, while both PCs keep streaming.
    let last = &mixed.samples[mixed.samples.len() - 48_000..];
    println!("all muted: rms {:.5}", rms(last));
    assert_eq!(rms(last), 0.0, "all PCs muted");
    let h = hub.status().borrow().clone();
    assert!(connected(&h, &ida) && connected(&h, &idb), "muting keeps the connections");

    // Unmuting brings the audio back without reconnecting.
    hub.set_muted(Vec::new());
    let cap_b = capture_clock(b.pc_audio_capture(), 1500.0, 0.4, 150, None);
    let out = playout_clock(hub.pc_audio_playout(), 150, 2.0);
    join(cap_b).await;
    let rx = join(out).await;
    let tail = &rx.samples[rx.samples.len() - 48_000..rx.samples.len() - 4_800];
    let b1500 = tone_amplitude(tail, 1500.0);
    println!("unmuted: 1500 Hz {b1500:.3}");
    assert!((b1500 - 0.4).abs() < 0.08, "PC-B after unmute {b1500}");
    assert!(hub.status().borrow().pc_audio_active);

    hub.shutdown().await;
    a.shutdown().await;
    b.shutdown().await;
}

/// Android closes the output while every streaming PC is muted, so nobody pulls the playout
/// (here: never, since the hub started). Unmuting must play fresh audio: no stale backlog, no
/// added latency, no packets counted as lost.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unmuting_after_the_output_was_closed_plays_fresh_audio() {
    init_logs();
    let (da, dh) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = start_server(da.path(), "PC-A", NetOptions::local_only()).await;
    let pa = pairing_of(&a);
    let ida = pa.peer_id();
    let hub = start_hub(dh.path(), Vec::new()).await;
    hub.set_muted(vec![ida.clone()]);
    hub.set_peers(vec![pa]);
    let mut hs = hub.status();
    wait_for(&mut hs, Duration::from_secs(10), "connected", |h| connected(h, &ida)).await;

    let cap = capture_clock(a.pc_audio_capture(), 1000.0, 0.5, 350, Some(280));
    tokio::time::sleep(Duration::from_secs(2)).await;
    hub.set_muted(Vec::new());
    let out = playout_clock(hub.pc_audio_playout(), 150, 0.25);
    let onset = join(cap).await.expect("loud onset pushed");
    let rx = join(out).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    let s = hub.status().borrow().peers[0].status.pc_audio.clone();
    let latency = rx.first_loud.map(|t| t.saturating_duration_since(onset));
    println!("after unmute: latency {latency:?}, {s:?}");
    let latency = latency.expect("loud tone heard after unmute");
    assert!(latency > Duration::ZERO && latency < Duration::from_millis(150), "latency {latency:?}");
    assert!(s.lost_packets < 10, "muting must not count as loss: {s:?}");
    assert!(s.buffer_ms < 150.0, "stale backlog: {s:?}");

    hub.shutdown().await;
    a.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removing_a_pc_keeps_the_other_connection() {
    init_logs();
    let (da, db, dh) = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    let a = start_server(da.path(), "PC-A", NetOptions::local_only()).await;
    let b = start_server(db.path(), "PC-B", NetOptions::local_only()).await;
    let (pa, pb) = (pairing_of(&a), pairing_of(&b));
    let (ida, idb) = (pa.peer_id(), pb.peer_id());
    let hub = start_hub(dh.path(), vec![pa.clone(), pb]).await;
    let mut hs = hub.status();
    wait_for(&mut hs, Duration::from_secs(10), "both connected", |h| {
        connected(h, &ida) && connected(h, &idb)
    })
    .await;

    // record every state PC-A goes through from now on
    let states = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
    let mut sa = a.status();
    sa.borrow_and_update();
    let rec = states.clone();
    let watcher = tokio::spawn(async move {
        while sa.changed().await.is_ok() {
            let st = sa.borrow_and_update().state;
            rec.lock().push(st);
        }
    });

    hub.set_peers(vec![pa]);
    let h = hub.status().borrow().clone();
    assert_eq!(h.peers.len(), 1);
    assert_eq!(h.peers[0].id, ida);
    let mut sb = b.status();
    wait_for(&mut sb, Duration::from_secs(10), "PC-B dropped", |s| {
        s.state == ConnState::WaitingForPeer
    })
    .await;

    // PC-A keeps streaming without a reconnect
    let cap = capture_clock(a.pc_audio_capture(), 1000.0, 0.5, 150, None);
    let out = playout_clock(hub.pc_audio_playout(), 150, 2.0);
    join(cap).await;
    let rx = join(out).await;
    let r = rms(&rx.samples[rx.samples.len() - 48_000..]);
    assert!(r > 0.25, "PC-A audio after removing PC-B: rms {r}");
    assert!(connected(&hub.status().borrow(), &ida));

    let seen = states.lock().clone();
    assert!(
        seen.iter().all(|s| *s == ConnState::Connected),
        "PC-A connection was disturbed: {seen:?}"
    );
    watcher.abort();

    hub.shutdown().await;
    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offline_pc_does_not_disturb_audio() {
    init_logs();
    let (da, dh) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = start_server(da.path(), "PC-A", NetOptions::local_only()).await;
    let pa = pairing_of(&a);
    let ida = pa.peer_id();
    // a paired PC that is switched off: valid identity, nothing listening on its address
    let off_port = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    let off = PairingInfo::new(
        iroh::SecretKey::from_bytes(&[9u8; 32]).public(),
        [1; 16],
        "LAPTOP",
        vec![off_port],
        None,
    );
    let idoff = off.peer_id();
    let hub = start_hub(dh.path(), vec![pa, off]).await;
    let mut hs = hub.status();
    wait_for(&mut hs, Duration::from_secs(10), "PC-A connected", |h| connected(h, &ida)).await;

    let cap = capture_clock(a.pc_audio_capture(), 1000.0, 0.5, 500, None);
    let out = playout_clock(hub.pc_audio_playout(), 500, 2.0);
    join(cap).await;
    let rx = join(out).await;
    let h = hub.status().borrow().clone();
    let sa = peer(&h, &ida).unwrap();
    let soff = peer(&h, &idoff).unwrap();
    println!("online: {:?}\noffline: {:?} {:?}", sa.pc_audio, soff.state, soff.last_error);
    assert!(
        matches!(soff.state, ConnState::Connecting | ConnState::Reconnecting),
        "the offline PC is being dialed: {:?}",
        soff.state
    );
    assert_eq!((sa.pc_audio.underruns, sa.pc_audio.lost_packets), (0, 0));
    let r = rms(&rx.samples[rx.samples.len() - 3 * 48_000..]);
    assert!((r - 0.3536).abs() < 0.05, "rms {r}");

    hub.shutdown().await;
    a.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_secret_is_rejected() {
    init_logs();
    let sdir = tempfile::tempdir().unwrap();
    let cdir = tempfile::tempdir().unwrap();
    let server = start_server(sdir.path(), "TEST-PC", NetOptions::local_only()).await;
    let good = pairing_of(&server);
    let mut secret = *good.secret();
    secret[0] ^= 0xFF;
    let bad = PairingInfo::new(
        good.endpoint_id(),
        secret,
        good.pc_name(),
        good.addrs().to_vec(),
        None,
    );
    let id = bad.peer_id();
    let hub = start_hub(cdir.path(), vec![bad]).await;
    let mut hs = hub.status();
    let h = wait_for(&mut hs, Duration::from_secs(10), "rejection", |h| {
        peer(h, &id)
            .and_then(|s| s.last_error.as_deref())
            .is_some_and(|e| e.contains("invalid pairing secret"))
    })
    .await;
    assert_ne!(peer(&h, &id).unwrap().state, ConnState::Connected);
    assert_ne!(server.status().borrow().state, ConnState::Connected);
    hub.shutdown().await;
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconnects_after_server_restart() {
    init_logs();
    let sdir = tempfile::tempdir().unwrap();
    let cdir = tempfile::tempdir().unwrap();
    let server = start_server(sdir.path(), "TEST-PC", NetOptions::local_only()).await;
    let pairing = pairing_of(&server);
    let id = pairing.peer_id();
    let hub = start_hub(cdir.path(), vec![pairing.clone()]).await;
    let mut hs = hub.status();
    wait_for(&mut hs, Duration::from_secs(10), "connected", |h| connected(h, &id)).await;

    server.shutdown().await;
    wait_for(&mut hs, Duration::from_secs(15), "reconnecting", |h| {
        peer(h, &id).is_some_and(|s| s.state == ConnState::Reconnecting)
    })
    .await;

    // Same data dir: same identity, secret and (persisted) port, so the old QR still works.
    let server = start_server(
        sdir.path(),
        "TEST-PC",
        NetOptions {
            port: None,
            ..NetOptions::local_only()
        },
    )
    .await;
    assert_eq!(pairing_of(&server), pairing);
    let t = Instant::now();
    wait_for(&mut hs, Duration::from_secs(20), "reconnected", |h| connected(h, &id)).await;
    println!("reconnected {:.1} s after restart", t.elapsed().as_secs_f32());

    // handles stay valid across reconnects
    let cap = capture_clock(server.pc_audio_capture(), 1000.0, 0.5, 150, None);
    let out = playout_clock(hub.pc_audio_playout(), 150, 2.0);
    join(cap).await;
    let rx = join(out).await;
    let r = rms(&rx.samples[rx.samples.len() - 48_000..]);
    assert!(r > 0.25, "audio after reconnect rms {r}");

    hub.shutdown().await;
    server.shutdown().await;
}
