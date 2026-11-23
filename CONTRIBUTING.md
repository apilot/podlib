# Contributing to podlib

Thanks for your interest in improving podlib!

Also available in Russian: [CONTRIBUTING.ru.md](CONTRIBUTING.ru.md).

## Development setup

- Rust 1.95 or newer (via [rustup](https://rustup.rs)) — the workspace MSRV
- A C compiler (gcc/clang) — SQLite is bundled and compiled at build time
- For the GUI: [ffmpeg](https://ffmpeg.org) (transcoding) and a desktop GL
  stack (Wayland/X11), present on any regular desktop installation

```sh
git clone https://github.com/apilot/podlib.git
cd podlib
cargo build
cargo test
```

## Before opening a PR

- `cargo test` passes across the workspace (unit + fixture tests)
- `cargo clippy --all-targets -- -D warnings` is clean
- New behavior comes with tests; bug fixes come with a failing-test-first
  reproduction
- Keep functions small; everything that knows the on-device database format
  stays inside the backend adapter (`crates/podlib` → libopod adapter)

## Working with a real device

- Writes to an iPod always go through the staged edit session
  (stage → install). Never mutate the live database directly: the firmware
  catches SQLite ↔ CDB ↔ CBK desynchronization.
- Hardware-dependent tests are guarded (see
  `crates/vendor/libopod/HARDWARE_TESTING.md`) — do not remove or bypass
  the guards.
- If you can test on real hardware (Nano 7G or a classic iPod), say so in
  the PR — it helps a lot.

## The vendored libopod

`crates/vendor/libopod` is a pinned fork of
[koriwi/libopod](https://github.com/koriwi/libopod), published to crates.io
as [`podlib-libopod`](https://crates.io/crates/podlib-libopod) (upstream
owns the `libopod-rs` name there). The policy:

- Do not edit it casually. Fixes that belong to the database layer go
  upstream first — our [koriwi/libopod#1](https://github.com/koriwi/libopod/pull/1)
  (NULL-safe rank queries, orphan-artist delete guard, empty-key reindex)
  is an example of that flow — and then land in the vendored copy.
- podlib-specific behavior (edit-session bookkeeping, artwork-safe removal,
  playlist mutations, classic-device support) lives in the fork until
  upstream picks it up or ships a crates.io release carrying it; the fork
  crate version is bumped for every published change.

## Reporting issues

Open an [issue](https://github.com/apilot/podlib/issues) with: device model
and firmware version, podlib version (git rev), what you did, what you
expected, what happened. Include the CLI/GUI output. Please do not attach
your actual database dumps unless asked.
