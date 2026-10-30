# Repository Guidelines

## Project Overview
AudioBridge turns an Android phone into the "headphones and microphone" of one or more Windows or Linux PCs:
- PC system audio (WASAPI loopback; on Linux the PipeWire default sink's monitor) → phone → Bluetooth headphones.
- Phone mic → PC virtual microphone (VB-CABLE "CABLE Output"; on Linux AudioBridge's own PipeWire source `audiobridge_mic`).

Transport is iroh QUIC: LAN, hole-punched, relay fallback, and Tailscale IPs included in the pairing. Pairing is a single QR scan (`audiobridge://pair?d=<base64url>`). After that, both sides run in the background and autostart on boot. The phone can be paired with up to 8 PCs at once: audio from all of them is mixed, and the mic goes only to PCs whose apps are actually recording. All UI text is Russian; code, comments and logs are English. `README.md` / `README.ru.md` are the user-facing docs; this file is the developer doc. Public repo: `github.com/Lyten02/AudioBridge` (MIT); `main` is protected, changes go through PRs with the CI in `.github/workflows/ci.yml`.

## Architecture & Data Flow
```
PC (Server)  WASAPI loopback ─► CaptureHandle ─► tx thread (Opus 192k, 10 ms) ─► QUIC datagram ─┐
                                                                                                 ▼
Phone (Hub)  AAudio out ◄─ PlayoutHandle (Mixer over per-PC jitter buffers, decode in fill) ◄─ RxFeeder (packet queue) ◄┘
Phone (Hub)  AAudio in ─► CaptureHandle ─► tx thread (Opus 64k, fan-out to demanding PCs) ─► datagram ─┐
PC (Server)  WASAPI render "CABLE Input" ◄─ PlayoutHandle (jitter buffer, decode in fill) ◄─ RxFeeder ◄──────────────┘
```
- **Core (`audiobridge_core`)** is platform-neutral. Platforms touch audio only through `CaptureHandle::push` and `PlayoutHandle::fill`: 48 kHz f32 interleaved. Capture goes through an rtrb SPSC ring to the sender thread; received packets go through an rtrb SPSC packet queue into a jitter buffer that `fill` decodes on demand (NetEQ-lite: PLC/expand, p98 delay target, drift PI + Hermite resampler, WSOLA-style frame drops). Neither call allocates or locks (`tests/rt_alloc.rs` enforces this).
  - **Real-time rule:** `push`, `fill`, `Playout::fill` and `Mixer::fill` must never allocate, lock or block. Mixer slots are preallocated; peers attach, detach and are muted via atomics; the sender is woken with `Thread::unpark`.
- **Roles:** `session::Server` is the PC side; it serves one active phone and a new connection replaces the old one (close codes 1 = replaced, 2 = rejected, 3 = shutdown). `session::Hub` is the phone side: one iroh endpoint, one task per PC, `MAX_PEERS = 8`, `SLOTS = 16`.
- **Dialing:** the phone always dials the PC, which works through NAT and double NAT.
- **Control (protocol v3):** a single bi-stream carrying `ControlMsg` frames (`len u16 | tag | body`): Hello{secret, PhoneControls} → Welcome{PcControls}/Reject, PcState, MicDemand, PhoneState, SetPc, SetPhone, PcMedia (v3). Each side reads the stream through `spawn_control_reader` (a task feeding a channel), so `select!` never cancels a half-read frame. A Hello of another version still decodes far enough to be rejected with a readable reason.
- **Remote control:** each side owns its controls and reports them (`PcControls`: audio, mic, mic_default = the virtual mic is the default recording device, default playback volume; `PhoneControls`: mic switch, mic_ready, media volume). The other side may ask for a change (`PcRequest` / `PhoneRequest`). Core never applies requests: `Server::take_requests` / `Hub::take_requests` hand them to the app, which applies them and reports the result through the setters (`Server::set_mic_default`, `set_volume`, …; `Hub::set_phone_controls`). `Status.pc` / `Status.phone` carry the reported state.
- **One-switch actions (PC):** "Микрофон" on = accept the mic + ask the phone to switch its mic on + make CABLE Output the default recording device; off = restore the remembered recording device (the phone mic is left alone: other PCs may use it). "Звук ПК" on also undoes a CABLE takeover of the default playback device.
- **Headphone buttons → PC media (v3):** the earbuds turn taps into AVRCP commands; Android's Bluetooth stack dispatches play/pause/next/previous as media key events to the media session of the app that played audio last. `HeadsetControl.kt` (owned by `BridgeService`) keeps a `MediaSession` while the feature is on and exactly one PC is the target (`MediaTarget` in `MediaKeys.kt`: the chosen PC while connected, else the unmuted connected PC that plays / played or was controlled last / the only one; ambiguous → none, never fan-out). Without a target the session is released, so the keys go to other players. Its `PlaybackState` mirrors the target's `PcMedia`, which is also what the earbuds use to choose "play" or "pause". A press (first ACTION_DOWN) becomes `PcRequest::Media(MediaCommand)` via `controlPc(id, PC_MEDIA, code)`; a 3 s timed wake lock covers the screen-off send. The PC applies it to the Windows media session (GSMTC, `crates/desktop/src/media.rs`; SendInput media keys only when no session exists or the call fails) and reports `PcMedia` (playback, app, title, artist) through `Server::set_media`. Volume taps are NOT media keys: AVRCP volume up/down is dropped by `MediaSessionService` (not `isMediaSessionKey`) and absolute volume sets the phone's STREAM_MUSIC directly, so apps can't see or remap them; don't add a system-volume observer to fake it. "Один наушник" remaps only what arrives: the play/pause group = 2 taps, next/previous = 3 taps.
- **Datagram header:** 8 bytes, `0xAB | kind | stream | 0 | seq u32 LE`. Kinds: Audio, Silence (after 300 ms of digital silence the sender emits markers and stops), Pace (an empty keep-awake packet that the Hub sends every 20 ms while a PC's audio streams and that PC isn't receiving our mic; with the screen off, Android ≥14 won't let apps disable Wi-Fi power save, and uplink traffic is what keeps the radio awake. Receivers ignore it).
- **Playout (NetEQ-lite, `audio/playout.rs`):**
  - Underrun policy: a missing frame with newer packets present → PLC, advance; buffer empty → "expand" PLC without advancing (no skipped content); after 70 ms of PLC fade to silence; resume with a 2.5 ms fade-in; never re-buffer the full target.
  - Target buffer: p98 of an exponentially forgotten arrival-delay histogram, clamped 20–500 ms. Excess buffer is drained by WSOLA-style frame drops.
  - Clock drift: PI-controlled Hermite resampling, ±1 %.
- **Hub backoff:** an offline PC is redialed 1 s → 30 s; `network_changed` retries immediately.
- **Phone-side mute:** `Hub::set_muted(ids)` (Kotlin `NativeBridge.setMuted`, persisted as `muted` per entry in `paired_pcs`). A muted PC stays connected, keeps the mic and keeps streaming. Its feeder still tracks activity but stops queueing audio, so a closed (unread) output never builds a stale backlog; one Silence marker ends the stream, and the mixer ramps it out over 10 ms. Unmuting resets the feeder: fresh stream, no replayed audio. A muted PC doesn't count toward `pc_audio_active`, so the output closes and the wake locks are released, and it gets no Pace packets. It's local to the phone: no wire/protocol change, so it works with any PC version.
- **Status:** `tokio::sync::watch` (`Status` / `HubStatus`) via `StatusCell::update` → `send_if_modified`. Stats refresh at 2 Hz with change thresholds, so only real changes wake watchers. Keep this "notify on change" pattern; don't add polling.
- **Desktop audio supervisor:**
  - `service.rs` owns the service lifetime on `service-supervisor`, independently of the blocking UI loop. Global `settings.json.service_enabled` defaults to true for old files; off joins audio/media workers, cancels and joins relays, shuts down the server endpoint and restores non-CABLE default devices. Restart uses the same identity directory and cached pairing QR. Generation-tagged remote/device commands cannot leak across restart.
  - Loopback runs only when `connected && pc_audio_enabled` and the default device is not a VB-CABLE device.
  - Mic render runs only while `mic.active`.
  - Mic demand comes from `audio/demand.rs`: another process holding an active capture session on "CABLE Output".
- **Linux desktop (`crates/desktop/src/linux/`, PipeWire):** the same `service.rs`/`shared.rs`/`ui.rs`; `main.rs` swaps the platform modules with `#[cfg_attr(target_os = "linux", path = "linux/…")]` behind the same API (`audio`, `autostart`, `instance`, `media`, `paths`, `tray`).
  - `audio/` runs one PipeWire main loop on `audio-supervisor` (reconnects every 2 s if PipeWire goes away). Every input is an event: registry globals, the `default` metadata, the default sink's `Props`, stream states, commands (`pw::channel`); callbacks only queue `Ev`s and `Ctx::post` drains them, because stream callbacks fire synchronously from `connect`/`disconnect`.
  - Virtual mic `audiobridge_mic` (mono, `media.class = Audio/Source` + `node.virtual`; not `Audio/Source/Virtual`, which WirePlumber configures with input ports like a null sink, and a mono one never finishes configuring, stalling all its linking) exists for the whole connection; we never link it. PipeWire runs it only while something records, so mic demand = its stream state `Streaming`. It disappears with the process, also on a crash.
  - Loopback `audiobridge_capture` (`stream.capture.sink`, no target: WirePlumber links it to the default sink's monitor and moves it with the default) runs only when `connected && pc_audio_enabled` and a default sink exists. Being linked keeps the sink running, so silence arrives as zeros and the sender's silence gate works without a gap filler.
  - Core handles live in `Arc<Mutex<Option<_>>>` slots that survive stream re-creation; the real-time process callbacks only `try_lock` them and never allocate.
  - Devices: endpoint ids are `node.name`s; `DeviceSummary` (`endpoints.rs`) comes from the registry + `default` metadata (`graph.rs`). "Микрофон по умолчанию" writes `default.configured.audio.source`; the user's configured choice counts, so turning AudioBridge off restores the remembered mic even though WirePlumber already fell back. `service.rs` asks through short-lived connections (`oneshot.rs`).
  - Volume: read from the default sink's `Props` (`channelVolumes` linear → cubic percent, `mute`), written with `wpctl` (WirePlumber's mixer API also updates hardware routes).
  - Media keys: MPRIS on the session bus (`zbus`, `PropertiesChanged`/`NameOwnerChanged`), exactly one target: the playing player (most recently active first), else the most recent one. Tray: StatusNotifierItem (`ksni`), optional (no session bus → no tray; a second launch shows the window). Autostart: `~/.config/autostart/audiobridge.desktop`. Single instance: Unix socket `$XDG_RUNTIME_DIR/audiobridge.sock` (`show`/`quit`). SIGTERM/SIGINT/SIGHUP run the normal shutdown.
- **Android audio policy** (`policy.rs`): output opens on `pcAudioActive` and closes after 30 s idle. Mic capture runs iff `micWanted && phone.mic_allowed()` (user switch && mic_ready).

## Key Directories
- `crates/core/src/`
  - `proto.rs`: wire constants, header, control messages.
  - `pairing.rs`: QR payload.
  - `audio/`: `tx`, `rx` (packet queue), `playout` (jitter buffer + decode-on-demand), `mixer`, `codec`, `sim` (test traces).
  - `session/`: `server.rs`, `hub.rs`, `net.rs` (endpoints, persisted keys, address ranking).
- `crates/desktop/src/`: PC tray app. Top-level platform modules are Windows; `linux/` holds their Linux counterparts; `endpoints.rs`, `service.rs`, `shared.rs`, `settings.rs`, `ui.rs` and `icon.rs` are shared.
  - `audio/`: WASAPI loopback/render, devices, demand, `policy.rs` (IPolicyConfig default-device restore).
  - `ui.rs` (eframe), `tray.rs`, `selfinstall.rs`, `autostart.rs`, `instance.rs`, `cable_install.rs`.
  - `linux/audio/`: `mod.rs` (engine), `streams.rs` (loopback + virtual mic), `graph.rs`, `volume.rs`, `oneshot.rs`; `linux/{media,tray,instance,autostart,paths}.rs`.
  - The window uses the brand gradient (indigo `#5B4BFF` → teal `#19C3D0`, the same as the Android launcher icon) and draws its icons with the painter. It repaints on status changes; the only timer (`request_repaint_after`, 40 ms) drives the level bars while PC audio streams and the window is open. Child widgets placed at an absolute rect use `ui.new_child(..)`: `scope_builder` moves the parent cursor and makes later cards overlap.
  - Close destroys the window, not the service. `WindowState` coalesces show requests while creation is pending; tray UP/DBLCLK/UP must not enqueue multiple windows that open after successive closes. Status repaint never opens a window.
- `crates/android-native/src/`
  - Pure, host-testable: `status.rs` (statusJson v2 + `NotifyGate`), `controls.rs` (`controlPc` actions), `policy.rs`, `peers.rs`.
  - `android/` (cfg android only): `jni_api.rs`, `engine.rs` (tokio + Hub manager), `controller.rs` (`ab-audio` thread), `aaudio.rs`, `listener.rs`, `logging.rs`.
- `android/app/src/main/java/app/audiobridge/`
  - `NativeBridge.kt` / `StatusListener.kt`: the JNI surface.
  - `BridgeService.kt`: the foreground service. `HeadsetControl.kt`: the media session for headphone buttons; `MediaKeys.kt`: its pure logic (key mapping, target choice), JVM-tested.
  - `BootReceiver.kt`, `AppState.kt` (prefs incl. headset settings, StatusHub), `BridgeStatus.kt` (statusJson parser, `PairedPc`), `ui/` (`HeadsetCard.kt`: headphone-button settings).
- `scripts/install-desktop.ps1`: build plus self-install of the desktop app.
- `scripts/package-linux.sh` + `packaging/linux/`: Linux release packaging (per-user `.tar.gz` with `install.sh`, pacman `.pkg.tar.zst`, launcher entry, icon).

## Development Commands
```powershell
cargo test -p audiobridge-core                     # unit tests incl. simulated jitter/power-save/loss/skew traces + real-network integration tests (timing-sensitive)
cargo test -p audiobridge-android                  # host-only pure modules (status/policy/peers)
cargo clippy -p <crate> --all-targets -- -D warnings
cargo build -p audiobridge-desktop --release       # target\release\AudioBridge.exe
powershell -File scripts\install-desktop.ps1 [-SkipBuild] [-NoFirewall]
cargo ndk -t arm64-v8a --platform 26 build --release -p audiobridge-android   # target/aarch64-linux-android/release/libaudiobridge.so (no -o: cargo-ndk 4 would also copy iroh's cdylibs)
cargo ndk -t arm64-v8a --platform 26 clippy -p audiobridge-android --all-targets -- -D warnings   # Android clippy needs cargo-ndk (cc needs NDK clang)
```
- **Linux desktop:** needs the PipeWire dev files (`pkg-config libpipewire-0.3`) and libclang for bindgen. On the netcup runner they are unpacked without root under `~/opt/pipewire-sdk`; `. ~/opt/pipewire-sdk/env.sh` first (it also provides a headless `pipewire`/`wireplumber`/`pw-*`/`wpctl` for smoke tests).
  ```sh
  cargo clippy -p audiobridge-desktop --all-targets -- -D warnings && cargo test -p audiobridge-desktop
  cargo build -p audiobridge-desktop --release && scripts/package-linux.sh   # target/dist
  ```
- Debug-only isolation: `AUDIOBRIDGE_DEV_DIR=<absolute temp directory>` gives a separate identity, logs and instance mutex/events; seed `settings.json` with `"autostart": false` before testing beside the installed app. Release builds ignore this variable.
- **APK:** run Gradle from inside `android/` (PowerShell: `Set-Location android; .\gradlew.bat :app:assembleDebug`). It runs `buildRustLib` (cargo-ndk) automatically; add `-PskipRust` to reuse the `.so` already in `jniLibs`. Also: `:app:testDebugUnitTest`, `:app:lintDebug`.
- **Manual E2E without a phone or PC:**
  - Phone emulator against a running PC app: `cargo run -p audiobridge-core --example probe -- <pairing-uri> [--seconds N] [--mic-sine HZ] [--local]`.
  - Fake PC: `cargo run -p audiobridge-core --example local_server -- [--seconds N] [--sine HZ] [--local] [--mic-demand]` (prints `PAIRING_URI=`).
- **Get the PC's pairing URI:** `AudioBridge.exe --print-pairing` (Linux: `audiobridge --print-pairing`), or read `pairing.txt` in the data directory.
- **Device:** `adb install -r android/app/build/outputs/apk/debug/app-debug.apk`; logs via `adb logcat -s AudioBridge:*`. You can add a PC with `adb shell am start -a android.intent.action.VIEW -d '<pairing-uri>' app.audiobridge`.

## Code Conventions & Common Patterns
- **Errors:** Rust uses `anyhow::Result` + `.context(...)`. Audio paths never panic:
  - encode errors: warn and skip the frame;
  - decode errors: PLC;
  - device errors: reopen with backoff (desktop retries after 2 s; Android 200 ms → 10 s).
- **JNI:** every export is wrapped in `guard()` (`catch_unwind`) and returns a neutral value. Calls before `init` log a warning and do nothing. JNI calls only enqueue `Cmd`s and never block.
- **Threads** have kebab-case names: `audiobridge-tx-*`, `audio-supervisor`, `ab-audio`, `ab-listener`, `ab-rt`. Every `unsafe` block needs a `// SAFETY:` comment.
- **Logging:**
  - Core: `tracing`.
  - Desktop: daily files in `%LOCALAPPDATA%\AudioBridge\logs` (Linux: `~/.local/state/audiobridge/logs`); filter via the `AUDIOBRIDGE_LOG` env var.
  - Android: `tracing` → `log` → logcat tag `AudioBridge`. The filter in `logging.rs` must keep `tracing::span=off`, otherwise span enter/exit lines flood logcat several times per audio frame.
- **Contract coupling:** these pieces change together.
  - The statusJson v2 schema (`android-native/src/status.rs`, incl. the per-PC remote controls `pcMic`, `micDefault`, `pcVolume`, `pcMuted` and `media`) ↔ `BridgeStatus.kt` (which throws on unknown states).
  - Rust JNI symbols ↔ `NativeBridge.kt` ↔ `StatusListener.kt` (native calls `onStatus`, `onRemoteMic`, `onRemoteVolume` by name) ↔ `proguard-rules.pro` keep rules.
  - `controlPc` actions `PC_*` and media codes `MEDIA_*` (`NativeBridge.kt`) ↔ `controls.rs` ↔ `MediaCommand::code` (never renumber).
  - Core `Server` API ↔ `crates/desktop`; core `ControlMsg` ↔ `PROTOCOL_VERSION` (both apps must be updated together).
  - Persisted headset settings keys and `TapAction.pref` values (`AppState.kt`, `MediaKeys.kt`): never rename.
- **Persisted state:**
  - PC: `%APPDATA%\AudioBridge` holds `server.key`, `pairing.secret`, `server.port`, `settings.json` and `pairing.txt`. Writes go through tmp+rename; corrupt files are regenerated. Changing `server.key` or `pairing.secret` breaks every existing pairing.
  - Linux PC: the same files in `~/.config/audiobridge` (`$XDG_CONFIG_HOME`); endpoint ids in `settings.json` are PipeWire node names.
  - Phone: `<filesDir>/audiobridge/client.key`, plus SharedPreferences `audiobridge` (`paired_pcs` JSON list of `{id, name, uri, muted}`, `mic_enabled` (also flipped remotely by a PC), `autostart_done`, `service_enabled` (the global switch, missing = on), headphone buttons: `headset_enabled`, `headset_single_earbud`, `headset_double_tap`, `headset_triple_tap`, `headset_target`).
- **Global switch (phone):** `Prefs.serviceEnabled` (written with `commit`). Off stops `BridgeService` (native `setPeers("[]")` → hub stopped, no notification, no media session); every start path (`MainActivity.startBridge`, `BootReceiver`, a sticky restart in `onStartCommand`) checks `Prefs.shouldRun()`. Pairings and settings stay. It never tells PCs to switch off: they just see the phone offline.
- **Default UDP port:** 47130, falling back to a random port.
- **Screenshots and releases:** the pairing QR contains the PC's secret. Never publish it; promo shots replace it with a QR of the repo URL. GitHub Releases ship `AudioBridge.exe` (the release build) and `AudioBridge.apk` (the debug-signed APK from this machine's `~/.android/debug.keystore`; a different key can't update an installed app).

## Important Files
- `Cargo.toml`: workspace deps, and profiles you must keep:
  - `crates/opus-sys` builds the vendored libopus with `cc` at `-O2` even in dev builds (no cmake; MSVC / NDK clang, which `ring` already needs);
  - the dev profile is disk-saving (`line-tables-only`, no incremental);
  - release uses thin LTO and strip.
- `crates/core/src/{proto,pairing}.rs`: the wire format. Bump `PROTOCOL_VERSION` / the pairing version on incompatible changes.
- `crates/desktop/src/main.rs`: CLI flags `--background`, `--print-pairing`, `--no-install` (Windows), `--quit` (Linux: asks the running instance to exit, used by `install.sh`). Windows release builds self-install to `%LOCALAPPDATA%\Programs\AudioBridge\AudioBridge.exe`; debug builds run in place. Linux is installed by the package (`/usr/bin/audiobridge`) or `install.sh` (`~/.local/bin/audiobridge`).
- `android/app/build.gradle.kts`: `buildRustLib` task, compile/target SDK 35, minSdk 26, arm64-v8a only.
- `android/app/src/main/AndroidManifest.xml`:
  - FGS types `connectedDevice|microphone`;
  - `BootReceiver`: BOOT_COMPLETED, QUICKBOOT_POWERON, MY_PACKAGE_REPLACED;
  - deep link `audiobridge://pair`.

## Runtime/Tooling Preferences
- **Rust:** stable rustc ≥ 1.92 (eframe 0.35). Developed on 1.94.1; there is no `rust-toolchain` file. Don't bump eframe without checking its MSRV.
- **Opus:** vendored native libopus (`crates/opus-sys`, float build, `USE_ALLOCA`) built with `cc`. Keep stack temporaries (`USE_ALLOCA`/`VAR_ARRAYS`): decoding runs inside `PlayoutHandle::fill`, so it must not heap-allocate. Don't switch back to `unsafe-libopus` (its c2rust VLAs allocate on every decode).
- **Android:**
  - NDK r30 (`30.0.16248370`) via cargo-ndk 4.x.
  - Always pass `--platform 26` to cargo-ndk, because AAudio starts at API 26 (cargo-ndk 4 ignores `[package.metadata.ndk]` and defaults to API 21, which fails with `-laaudio`).
  - Use raw `ndk-sys` AAudio; API-28 setters are looked up with `dlsym`. Never enable the `ndk` crate's `api-level-28` feature: the library would fail to load on API 26/27.
  - Input preset `VoiceRecognition`, never `VoiceCommunication`, which forces Bluetooth SCO/HFP and ruins A2DP.
- **Android build:** AGP 8.10.1, Kotlin 2.2.0, Gradle 8.11.1 wrapper, JDK 17 target. Compose versions are pinned explicitly (no BOM). `FAIL_ON_PROJECT_REPOS` means repositories are declared only in `settings.gradle.kts`.
- **Android 14/15 FGS rules:**
  - Boot and background starts may use only `connectedDevice`.
  - The `microphone` type is added only from a visible activity or the notification action (a while-in-use exemption). It is then kept whenever RECORD_AUDIO is granted, independent of the user's mic switch, so a PC can switch the mic on remotely in the background. `setMicState(enabled, ready)` reports the switch and whether the type is held; capture needs both. After a reboot the phone reports `mic_ready = false` until the app or the notification action is opened once, and the PC UI says so.
  - HyperOS "Clear all" force-stops unlocked apps, so users must enable Autostart and lock the app in recents.
- **Windows:**
  - Desktop `windows` crate features are listed explicitly; new Win32 APIs need the matching feature added.
  - Mic routing requires VB-CABLE. Installing it can hijack the default playback device; the app detects this and offers a restore.
- **Linux:**
  - Runtime: libpipewire-0.3 (≥ 0.3.49) and WirePlumber (`wpctl`, the `default` metadata, linking); the GUI loads Wayland/X11, xkbcommon and EGL with `dlopen`. The release binary is built on Debian 13 and links only glibc, libgcc and libpipewire.
  - Platform crates are target-specific dependencies (`pipewire`, `ksni`, `zbus`; eframe `wayland`+`x11`). Keep Windows-only code behind `#[cfg(windows)]` so `cargo xwin clippy` and Linux clippy both stay clean.
  - The window has the app id/class `audiobridge` (Hyprland: `windowrule = float, class:^(audiobridge)$`).
  - Keep EGL vsync off on Linux (`glow_options.vsync = false` in `ui.rs`). With vsync, a repaint while the window sits on a hidden workspace blocks the UI thread in the EGL swap (`wl_display_dispatch_queue`) until the window is shown again: compositor pings go unanswered (Hyprland's "not responding" dialog kills with SIGKILL, so no cleanup runs) and SIGTERM/"Выход" cannot finish.
  - Headless smoke without real audio hardware: a private `pipewire` + `wireplumber` with a `support.null-audio-sink` as the default sink; play a tone into it, run `probe --mic-sine 440` against the app, record `pw-record --target audiobridge_mic`. A fake mic made from a null sink must be `Audio/Source/Virtual` (a null sink is an input node; as `Audio/Source` it stalls WirePlumber).
- **Disk:** disk space is tight. Share `target/`, don't create extra target dirs, and don't commit generated paths (`target/`, `android/app/src/main/jniLibs/`, `android/local.properties`, Gradle build dirs, `android/.kotlin/`). `Cargo.lock` is committed.

## Testing & QA
- **What's tested:**
  - Core unit tests: proto, pairing, codec, tx gate, rx, the mixer limiter and persisted identity.
  - Playout simulations on a simulated clock (`audio/sim.rs`): jitter, power-save bursts, 2 % loss and ±0.15 % skew. They assert no exact-zero output runs and no skipped content.
  - `tests/rt_alloc.rs`: counts allocations and requires zero inside `push` and `fill`.
  - `crates/core/tests/integration.rs`: a real Server + Hub in-process over `NetOptions::local_only()`, covering both directions plus latency < 60 ms, mixing from two PCs with mic routing, muting PCs on the phone, removing a PC, a wrong secret, reconnect, uplink pacing, remote control in both directions and media commands (only the addressed PC, exactly once, in order; `PcMedia` reaches the phone and is cleared on disconnect). These tests are real-time and timing-sensitive; don't run them under heavy parallel load to judge flakiness.
  - **On-device glitch check:** the phone logs `pc audio glitch: … underruns=… lost=…` and the PC logs `mic glitch: …` whenever counters grow. Compare screen-on vs screen-off with music playing.
  - android-native: host tests for the statusJson v2 schema, `NotifyGate` debounce, `controlPc` mapping, policies and peer parsing.
  - Android: JVM tests `BridgeStatusTest`, `PairedPcTest` and `MediaKeysTest` (key mapping incl. single-earbud, one press = one command, target choice, settings persistence) (`isReturnDefaultValues = false`); there is no instrumented test.
  - Desktop: resampler, persisted service default/roundtrip, show-request coalescing, and service-generation request gating tests.
  - Linux desktop: device summary from the PipeWire graph (configured vs effective default mic), linear → cubic volume, MPRIS target choice and command resolution, PRETTY_HOSTNAME parsing, autostart `Exec=` quoting.
- **What's not tested:** WASAPI, PipeWire, AAudio, JNI, the UI and the service. Verify those on real hardware:
  - PC audio: play a tone on the PC → the phone UI shows "Звук" / buffer.
  - Mic: `ffmpeg -f dshow -i audio="CABLE Output (VB-Audio Virtual Cable)" -t 8 -af volumedetect -f null -` should report non-silent levels while the phone mic is allowed.
  - Linux mic: `pw-record --target audiobridge_mic -` (or any app recording «AudioBridge: микрофон телефона») → the PC log shows `mic demand: true` and the recording isn't silent; after "Выход" or `kill -9`, `pw-cli ls Node` has no `audiobridge_*` nodes. `wpctl status` lists `audiobridge_mic` under "Default Configured Devices" after a `kill -9` with the phone mic as default: that is the remembered default (metadata), not a node.
  - Hidden window (Linux): put the window on another workspace, then add/remove an audio device or connect the phone. The main thread must stay in `epoll_pwait` (`/proc/<pid>/syscall` starts with 281, not 7 = `poll`), and `kill <pid>` must exit at once with `shutting down` in the log.
  - Autostart: reboot the phone → the PC log shows `phone 'POCO F5' connected`.
  - Headphone buttons: play music on the PC through AudioBridge, screen off, double/triple tap → the PC log shows `media command …` and the player reacts; the app's "Последняя команда" line shows what the phone received (`adb logcat -s AudioBridge:*` → `media command N -> PC`).
- **Clippy:** `-D warnings` is the working bar for all crates. GitHub CI (Windows) runs clippy plus the core lib/rt_alloc, desktop and android-native host tests; the timing-sensitive integration tests and Android/Gradle checks run locally only. Android lint must stay at 0 errors.
