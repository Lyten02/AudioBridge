//! Serializes service lifetime and remote requests independently of the blocking window loop.

use std::sync::mpsc::{Receiver, Sender};

use anyhow::{Context, Result};
use audiobridge_core::session::{Server, ServerConfig};
use tokio::runtime::Runtime;
use tokio::task::JoinHandle;

use crate::audio::{self, AudioEngine, Hooks};
use crate::media::MediaTransport;
use crate::shared::{shared, MainCmd};

struct Running {
    audio: AudioEngine,
    media: MediaTransport,
    watchers: Vec<JoinHandle<()>>,
}

impl Running {
    fn start(rt: &Runtime, generation: u64) -> Result<Self> {
        let sh = shared();
        sh.set_generation(generation);
        let server = rt.block_on(Server::start(ServerConfig {
            data_dir: sh.data_dir.clone(),
            pc_name: sh.pc_name.clone(),
        })).context("starting network server")?;
        let settings = sh.settings();
        server.set_pc_audio_enabled(settings.pc_audio_enabled);
        server.set_mic_enabled(settings.mic_enabled);
        let capture = server.pc_audio_capture();
        let playout = server.mic_playout();
        sh.install_server(server);
        let media = match MediaTransport::spawn(|state| shared().set_media_state(state)) {
            Ok(media) => media,
            Err(error) => {
                shutdown_server(rt);
                return Err(error).context("starting media transport");
            }
        };
        sh.set_media_sender(media.sender());
        let audio = match AudioEngine::spawn(capture, playout, Hooks {
            on_devices: Box::new(|d| shared().set_devices(d)),
            on_demand: Box::new(|v| shared().set_mic_demand(v)),
            on_volume: Box::new(|v| shared().set_pc_volume_state(v)),
        }) {
            Ok(audio) => audio,
            Err(error) => {
                media.shutdown();
                shutdown_server(rt);
                return Err(error).context("starting audio engine");
            }
        };
        sh.set_audio_sender(audio.sender());
        let watchers = sh.spawn_watchers(rt, audio.sender(), generation);
        tracing::info!("service started (generation={generation})");
        Ok(Self { audio, media, watchers })
    }

    fn stop(self, rt: &Runtime) {
        // Join cancelled relays before clearing state or starting the next generation.
        for watcher in &self.watchers {
            watcher.abort();
        }
        rt.block_on(async {
            for watcher in self.watchers {
                let _ = watcher.await;
            }
        });
        self.audio.shutdown();
        self.media.shutdown();
        shutdown_server(rt);
        restore_devices(false);
        shared().service_stopped(None);
        tracing::info!("service stopped");
    }
}

fn shutdown_server(rt: &Runtime) {
    if let Some(server) = shared().take_server() {
        // Complete endpoint closure before binding the same persisted port again.
        rt.block_on(server.shutdown());
    }
}

fn set_default(id: &str, what: &str) {
    match audio::set_default_endpoint(id) {
        Ok(()) => tracing::info!("default {what} device set to {id}"),
        Err(error) => tracing::warn!("setting default {what} device failed: {error:#}"),
    }
    shared().rescan_devices();
}

fn restore_devices(enabled: bool) {
    let _com = audio::com::Com::init();
    let Ok(enumerator) = audio::com::enumerator() else { return };
    let devices = audio::devices::summarize(&enumerator);
    let settings = shared().settings();
    // Inspect current devices, not a stale asynchronous notification snapshot.
    shared().set_devices(&devices);
    if enabled {
        if settings.mic_enabled && settings.resume_mic_default {
            if let Some(id) = &devices.cable_capture_id {
                set_default(id, "recording");
            }
        }
    } else {
        shared().remember_mic_default(devices.default_capture_is_cable);
    }
    if !enabled && devices.default_capture_is_cable {
        if let Some(target) = devices.capture_restore_candidate(settings.last_default_capture.as_deref()) {
            set_default(&target.id, "recording");
        } else {
            tracing::warn!("cannot restore recording device: no non-CABLE endpoint");
        }
    }
    if devices.default_is_cable && (!enabled || settings.pc_audio_enabled) {
        if let Some(target) = devices.render_restore_candidate(settings.last_default_render.as_deref()) {
            set_default(&target.id, "playback");
        } else {
            tracing::warn!("cannot restore playback device: no non-CABLE endpoint");
        }
    }
}

fn accepts_remote(running: bool, current: u64, incoming: u64) -> bool {
    running && current == incoming
}

pub fn supervise(rt: Runtime, commands: Receiver<MainCmd>, windows: Sender<()>) {
    let sh = shared();
    let mut running = None;
    let mut generation = 0;
    let start = |generation| match Running::start(&rt, generation) {
        Ok(service) => {
            restore_devices(true);
            Some(service)
        }
        Err(error) => {
            tracing::error!("service start failed: {error:#}");
            sh.persist_enabled(false);
            sh.service_stopped(Some(format!("Не удалось включить AudioBridge: {error:#}")));
            None
        }
    };
    if sh.settings().service_enabled {
        running = start(generation);
    }
    while let Ok(command) = commands.recv() {
        match command {
            MainCmd::Show => { let _ = windows.send(()); }
            MainCmd::Exit => break,
            MainCmd::SetEnabled(enabled) => {
                if enabled == running.is_some() { continue; }
                sh.persist_enabled(enabled);
                if enabled {
                    generation += 1;
                    running = start(generation);
                } else if let Some(service) = running.take() {
                    service.stop(&rt);
                }
            }
            MainCmd::Remote { generation: incoming, request } => {
                if accepts_remote(running.is_some(), generation, incoming) {
                    sh.apply_request(request);
                }
            }
            MainCmd::DefaultDevice { generation: incoming, id, what } => {
                if accepts_remote(running.is_some(), generation, incoming) {
                    set_default(&id, what);
                }
            }
        }
    }
    if let Some(service) = running {
        service.stop(&rt);
    }
    rt.shutdown_timeout(std::time::Duration::from_secs(1));
}

#[cfg(test)]
mod tests {
    use super::accepts_remote;

    #[test]
    fn stopped_and_previous_generation_requests_are_ignored() {
        assert!(accepts_remote(true, 2, 2));
        assert!(!accepts_remote(false, 2, 2));
        assert!(!accepts_remote(true, 3, 2));
    }
}
