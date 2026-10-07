# Repository Guidelines

## Project Overview
AudioBridge turns an Android phone into the "headphones and microphone" of one or more Windows PCs:
- PC system audio (WASAPI loopback) → phone → Bluetooth headphones.
- Phone mic → PC virtual microphone (VB-CABLE "CABLE Output").

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
  - **Real-time rule:** `push`, `fill`, `Playout::fill` and `Mixer::fill` must never allocate, lock or block. Mixer slots are preallocated; peers attach and detach via atomics; the sender is woken with `Thread::unpark`.
- **Roles:** `session::Server` is the PC side; it serves one active phone and a new connection replaces the old one (close codes 1 = replaced, 2 = rejected, 3 = shutdown). `session::Hub` is the phone side: one iroh endpoint, one task per PC, `MAX_PEERS = 8`, `SLOTS = 16`.
- **Dialing:** the phone always dials the PC, which works through NAT and double NAT.
- **Control (protocol v2):** a single bi-stream carrying `ControlMsg` frames (`len u16 | tag | body`): Hello{secret, PhoneControls} → Welcome{PcControls}/Reject, PcState, MicDemand, PhoneState, SetPc, SetPhone. Each side reads the stream through `spawn_control_reader` (a task feeding a channel), so `select!` never cancels a half-read frame. A Hello of another version still decodes far enough to be rejected with a readable reason.
- **Remote control:** each side owns its controls and reports them (`PcControls`: audio, mic, mic_default = CABLE Output is the Windows default recording device, default playback volume; `PhoneControls`: mic switch, mic_ready, media volume). The other side may ask for a change (`PcRequest` / `PhoneRequest`). Core never applies requests: `Server::take_requests` / `Hub::take_requests` hand them to the app, which applies them and reports the result through the setters (`Server::set_mic_default`, `set_volume`, …; `Hub::set_phone_controls`). `Status.pc` / `Status.phone` carry the reported state.
- **One-switch actions (PC):** "Микрофон" on = accept the mic + ask the phone to switch its mic on + make CABLE Output the default recording device; off = restore the remembered recording device (the phone mic is left alone: other PCs may use it). "Звук ПК" on also undoes a CABLE takeover of the default playback device.
- **Datagram header:** 8 bytes, `0xAB | kind | stream | 0 | seq u32 LE`. Kinds: Audio, Silence (after 300 ms of digital silence the sender emits markers and stops), Pace (an empty keep-awake packet that the Hub sends every 20 ms while a PC's audio streams and that PC isn't receiving our mic; with the screen off, Android ≥14 won't let apps disable Wi-Fi power save, and uplink traffic is what keeps the radio awake. Receivers ignore it).
- **Playout (NetEQ-lite, `audio/playout.rs`):**
  - Underrun policy: a missing frame with newer packets present → PLC, advance; buffer empty → "expand" PLC without advancing (no skipped content); after 70 ms of PLC fade to silence; resume with a 2.5 ms fade-in; never re-buffer the full target.
  - Target buffer: p98 of an exponentially forgotten arrival-delay histogram, clamped 20–500 ms. Excess buffer is drained by WSOLA-style frame drops.
  - Clock drift: PI-controlled Hermite resampling, ±1 %.
- **Hub backoff:** an offline PC is redialed 1 s → 30 s; `network_changed` retries immediately.
- **Status:** `tokio::sync::watch` (`Status` / `HubStatus`) via `StatusCell::update` → `send_if_modified`. Stats refresh at 2 Hz with change thresholds, so only real changes wake watchers. Keep this "notify on change" pattern; don't add polling.
- **Desktop audio supervisor:**
  - Loopback runs only when `connected && pc_audio_enabled` and the default device is not a VB-CABLE device.
  - Mic render runs only while `mic.active`.
  - Mic demand comes from `audio/demand.rs`: another process holding an active capture session on "CABLE Output".
- **Android audio policy** (`policy.rs`): output opens on `pcAudioActive` and closes after 30 s idle. Mic capture runs iff `micWanted && phone.mic_allowed()` (user switch && mic_ready).

## Key Directories
- `crates/core/src/`
  - `proto.rs`: wire constants, header, control messages.
  - `pairing.rs`: QR payload.
  - `audio/`: `tx`, `rx` (packet queue), `playout` (jitter buffer + decode-on-demand), `mixer`, `codec`, `sim` (test traces).
  - `session/`: `server.rs`, `hub.rs`, `net.rs` (endpoints, persisted keys, address ranking).
- `crates/desktop/src/`: Windows tray app.
  - `audio/`: WASAPI loopback/render, devices, demand, `policy.rs` (IPolicyConfig default-device restore).
  - `ui.rs` (eframe), `tray.rs`, `selfinstall.rs`, `autostart.rs`, `instance.rs`, `cable_install.rs`.
  - The window uses the brand gradient (indigo `#5B4BFF` → teal `#19C3D0`, the same as the Android launcher icon) and draws its icons with the painter. It repaints on status changes; the only timer (`request_repaint_after`, 40 ms) drives the level bars while PC audio streams and the window is open. Child widgets placed at an absolute rect use `ui.new_child(..)`: `scope_builder` moves the parent cursor and makes later cards overlap.
- `crates/android-native/src/`
  - Pure, host-testable: `status.rs` (statusJson v2 + `NotifyGate`), `policy.rs`, `peers.rs`.
  - `android/` (cfg android only): `jni_api.rs`, `engine.rs` (tokio + Hub manager), `controller.rs` (`ab-audio` thread), `aaudio.rs`, `listener.rs`, `logging.rs`.
- `android/app/src/main/java/app/audiobridge/`
  - `NativeBridge.kt` / `StatusListener.kt`: the JNI surface.
  - `BridgeService.kt`: the foreground service.
  - `BootReceiver.kt`, `AppState.kt` (prefs, StatusHub), `BridgeStatus.kt` (statusJson parser, `PairedPc`), `ui/`.
- `scripts/install-desktop.ps1`: build plus self-install of the desktop app.

## Development Commands
```powershell
cargo test -p audiobridge-core                     # unit tests incl. simulated jitter/power-save/loss/skew traces + real-network integration tests (timing-sensitive)
cargo test -p audiobridge-android                  # host-only pure modules (status/policy/peers)
cargo clippy -p <crate> --all-targets -- -D warnings
cargo build -p audiobridge-desktop --release       # target\release\AudioBridge.exe
powershell -File scripts\install-desktop.ps1 [-SkipBuild] [-NoFirewall]
cargo ndk -t arm64-v8a -o android/app/src/main/jniLibs build --release -p audiobridge-android
cargo ndk -t arm64-v8a clippy -p audiobridge-android --all-targets -- -D warnings   # Android clippy needs cargo-ndk (cc needs NDK clang)
```
- **APK:** run Gradle from inside `android/` (PowerShell: `Set-Location android; .\gradlew.bat :app:assembleDebug`). It runs `buildRustLib` (cargo-ndk) automatically; add `-PskipRust` to reuse the `.so` already in `jniLibs`. Also: `:app:testDebugUnitTest`, `:app:lintDebug`.
- **Manual E2E without a phone or PC:**
  - Phone emulator against a running PC app: `cargo run -p audiobridge-core --example probe -- <pairing-uri> [--seconds N] [--mic-sine HZ] [--local]`.
  - Fake PC: `cargo run -p audiobridge-core --example local_server -- [--seconds N] [--sine HZ] [--local] [--mic-demand]` (prints `PAIRING_URI=`).
- **Get the PC's pairing URI:** `AudioBridge.exe --print-pairing`, or read `%APPDATA%\AudioBridge\pairing.txt`.
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
  - Desktop: daily files in `%LOCALAPPDATA%\AudioBridge\logs`; filter via the `AUDIOBRIDGE_LOG` env var.
  - Android: `tracing` → `log` → logcat tag `AudioBridge`. The filter in `logging.rs` must keep `tracing::span=off`, otherwise span enter/exit lines flood logcat several times per audio frame.
- **Contract coupling:** these pieces change together.
  - The statusJson v2 schema (`android-native/src/status.rs`, incl. the per-PC remote controls `pcMic`, `micDefault`, `pcVolume`, `pcMuted`) ↔ `BridgeStatus.kt` (which throws on unknown states).
  - Rust JNI symbols ↔ `NativeBridge.kt` ↔ `StatusListener.kt` (native calls `onStatus`, `onRemoteMic`, `onRemoteVolume` by name) ↔ `proguard-rules.pro` keep rules.
  - Core `Server` API ↔ `crates/desktop`; core `ControlMsg` ↔ `PROTOCOL_VERSION` (both apps must be updated together).
- **Persisted state:**
  - PC: `%APPDATA%\AudioBridge` holds `server.key`, `pairing.secret`, `server.port`, `settings.json` and `pairing.txt`. Writes go through tmp+rename; corrupt files are regenerated. Changing `server.key` or `pairing.secret` breaks every existing pairing.
  - Phone: `<filesDir>/audiobridge/client.key`, plus SharedPreferences `audiobridge` (`paired_pcs` JSON list, `mic_enabled` (also flipped remotely by a PC), `autostart_done`).
- **Default UDP port:** 47130, falling back to a random port.
- **Screenshots and releases:** the pairing QR contains the PC's secret. Never publish it; promo shots replace it with a QR of the repo URL. GitHub Releases ship `AudioBridge.exe` (the release build) and `AudioBridge.apk` (the debug-signed APK from this machine's `~/.android/debug.keystore`; a different key can't update an installed app).

## Important Files
- `Cargo.toml`: workspace deps, and profiles you must keep:
  - `crates/opus-sys` builds the vendored libopus with `cc` at `-O2` even in dev builds (no cmake; MSVC / NDK clang, which `ring` already needs);
  - the dev profile is disk-saving (`line-tables-only`, no incremental);
  - release uses thin LTO and strip.
- `crates/core/src/{proto,pairing}.rs`: the wire format. Bump `PROTOCOL_VERSION` / the pairing version on incompatible changes.
- `crates/desktop/src/main.rs`: CLI flags `--background`, `--print-pairing`, `--no-install`. Release builds self-install to `%LOCALAPPDATA%\Programs\AudioBridge\AudioBridge.exe`; debug builds run in place.
- `android/app/build.gradle.kts`: `buildRustLib` task, compile/target SDK 35, minSdk 26, arm64-v8a only.
- `android/app/src/main/AndroidManifest.xml`:
  - FGS types `connectedDevice|microphone`;
  - `BootReceiver`: BOOT_COMPLETED, QUICKBOOT_POWERON, MY_PACKAGE_REPLACED;
  - deep link `audiobridge://pair`.

## Runtime/Tooling Preferences
- **Rust:** stable rustc ≥ 1.92 (eframe 0.35). Developed on 1.94.1; there is no `rust-toolchain` file. Don't bump eframe without checking its MSRV.
- **Opus:** vendored native libopus (`crates/opus-sys`, float build, `USE_ALLOCA`) built with `cc`. Keep stack temporaries (`USE_ALLOCA`/`VAR_ARRAYS`): decoding runs inside `PlayoutHandle::fill`, so it must not heap-allocate. Don't switch back to `unsafe-libopus` (its c2rust VLAs allocate on every decode).
- **Android:**
  - NDK r23 (`23.1.7779620`) via cargo-ndk.
  - `[package.metadata.ndk] platform = 26`, because AAudio starts at API 26.
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
- **Disk:** disk space is tight. Share `target/`, don't create extra target dirs, and don't commit generated paths (`target/`, `android/app/src/main/jniLibs/`, `android/local.properties`, Gradle build dirs, `android/.kotlin/`). `Cargo.lock` is committed.

## Testing & QA
- **What's tested:**
  - Core unit tests: proto, pairing, codec, tx gate, rx, the mixer limiter and persisted identity.
  - Playout simulations on a simulated clock (`audio/sim.rs`): jitter, power-save bursts, 2 % loss and ±0.15 % skew. They assert no exact-zero output runs and no skipped content.
  - `tests/rt_alloc.rs`: counts allocations and requires zero inside `push` and `fill`.
  - `crates/core/tests/integration.rs`: a real Server + Hub in-process over `NetOptions::local_only()`, covering both directions plus latency < 60 ms, mixing from two PCs with mic routing, removing a PC, a wrong secret, reconnect, uplink pacing and remote control in both directions. These tests are real-time and timing-sensitive; don't run them under heavy parallel load to judge flakiness.
  - **On-device glitch check:** the phone logs `pc audio glitch: … underruns=… lost=…` and the PC logs `mic glitch: …` whenever counters grow. Compare screen-on vs screen-off with music playing.
  - android-native: host tests for the statusJson v2 schema, `NotifyGate` debounce, policies and peer parsing.
  - Android: JVM tests `BridgeStatusTest` and `PairedPcTest` (`isReturnDefaultValues = false`); there is no instrumented test.
  - Desktop: a single resampler test.
- **What's not tested:** WASAPI, AAudio, JNI, the UI and the service. Verify those on real hardware:
  - PC audio: play a tone on the PC → the phone UI shows "Звук" / buffer.
  - Mic: `ffmpeg -f dshow -i audio="CABLE Output (VB-Audio Virtual Cable)" -t 8 -af volumedetect -f null -` should report non-silent levels while the phone mic is allowed.
  - Autostart: reboot the phone → the PC log shows `phone 'POCO F5' connected`.
- **Clippy:** `-D warnings` is the working bar for all crates. GitHub CI (Windows) runs clippy plus the core lib/rt_alloc, desktop and android-native host tests; the timing-sensitive integration tests and Android/Gradle checks run locally only. Android lint must stay at 0 errors.
