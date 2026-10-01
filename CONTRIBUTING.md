# Contributing

Thanks for helping! Issues and pull requests are welcome, in English or Russian.

## Workflow

1. Fork the repository and create a branch from `main`.
2. Make your change. Keep each PR focused on one thing.
3. Run the checks below.
4. Open a pull request against `main` and describe what changed and how you tested it.

`main` is protected: every change goes through a pull request, CI must pass, and the maintainer reviews and merges it.

## Checks

```powershell
cargo clippy -p audiobridge-core -p audiobridge-desktop -p audiobridge-android --all-targets -- -D warnings
cargo test -p audiobridge-core --lib --test rt_alloc
cargo test -p audiobridge-desktop
cargo test -p audiobridge-android
```

`cargo test -p audiobridge-core` also runs real-network integration tests. They are timing-sensitive, so run them on an idle machine.

For Android changes: `cd android; .\gradlew.bat :app:testDebugUnitTest :app:lintDebug`.

## Ground rules

- **Real-time audio code** (`CaptureHandle::push`, `PlayoutHandle::fill`, the mixer and playout) must never allocate, lock or block. `tests/rt_alloc.rs` enforces this.
- The **wire format** (`crates/core/src/proto.rs`, `pairing.rs`) is shared by both apps. An incompatible change must bump the protocol or pairing version.
- **UI text** is Russian, code, comments and logs are English.
- Things that can't be unit-tested (WASAPI, AAudio, the UI, the Android service) need a short note in the PR on how you checked them on real hardware.

See [AGENTS.md](AGENTS.md) for the architecture and conventions.
