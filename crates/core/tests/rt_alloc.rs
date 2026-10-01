//! The real-time entry points (`CaptureHandle::push`, `PlayoutHandle::fill`) must never touch the
//! heap, including Opus decoding, packet loss concealment and mixing. A counting global allocator
//! records allocations made by threads while they are inside those calls.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use audiobridge_core::session::{
    ConnState, Hub, HubConfig, HubStatus, NetOptions, Server, ServerConfig,
};

struct Counting;

static RT_ALLOCS: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static IN_RT: Cell<bool> = const { Cell::new(false) };
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if IN_RT.with(|f| f.get()) {
            RT_ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if IN_RT.with(|f| f.get()) {
            RT_ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        System.dealloc(ptr, layout)
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if IN_RT.with(|f| f.get()) {
            RT_ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn rt<R>(f: impl FnOnce() -> R) -> R {
    IN_RT.with(|c| c.set(true));
    let r = f();
    IN_RT.with(|c| c.set(false));
    r
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn push_and_fill_never_allocate() {
    let (sdir, hdir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let server = Server::start_with_options(
        ServerConfig {
            data_dir: sdir.path().to_path_buf(),
            pc_name: "PC".into(),
        },
        NetOptions::local_only(),
    )
    .await
    .unwrap();
    let hub = Hub::start_with_options(
        HubConfig {
            data_dir: hdir.path().to_path_buf(),
            device_name: "Phone".into(),
        },
        NetOptions::local_only(),
    )
    .await
    .unwrap();
    hub.set_peers(vec![server.pairing().borrow().clone().unwrap()]);
    let mut hs = hub.status();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let connected = |h: &HubStatus| h.peers.iter().any(|p| p.status.state == ConnState::Connected);
    while !connected(&hs.borrow_and_update()) {
        tokio::time::timeout_at(deadline, hs.changed()).await.unwrap().unwrap();
    }
    server.set_mic_demand(true);
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut pc_cap = server.pc_audio_capture();
    let mut pc_out = hub.pc_audio_playout();
    let mut mic_cap = hub.mic_capture();
    let mut mic_out = server.mic_playout();
    let worker = std::thread::spawn(move || {
        let start = Instant::now();
        let mut stereo = vec![0f32; 480 * 2];
        let mut mono = vec![0f32; 480];
        let mut phase = 0f32;
        let mut out2 = vec![0f32; 192 * 2];
        let mut out1 = vec![0f32; 441];
        for block in 0..300u64 {
            for f in stereo.chunks_exact_mut(2) {
                f.fill(phase.sin() * 0.5);
                phase += 0.06;
            }
            for (m, s) in mono.iter_mut().zip(stereo.iter().step_by(2)) {
                *m = *s;
            }
            let due = start + Duration::from_millis(10 * block);
            if let Some(d) = due.checked_duration_since(Instant::now()) {
                std::thread::sleep(d);
            }
            // capture callbacks, then several playout callbacks of odd sizes
            rt(|| {
                pc_cap.push(&stereo);
                mic_cap.push(&mono);
                for _ in 0..2 {
                    pc_out.fill(&mut out2);
                }
                mic_out.fill(&mut out1);
            });
        }
        out2.iter().map(|s| s.abs()).fold(0f32, f32::max)
    });
    let peak = tokio::task::spawn_blocking(move || worker.join().unwrap())
        .await
        .unwrap();
    assert!(peak > 0.1, "audio flowed (peak {peak})");
    assert_eq!(
        RT_ALLOCS.load(Ordering::Relaxed),
        0,
        "push/fill touched the heap"
    );
    hub.shutdown().await;
    server.shutdown().await;
}
