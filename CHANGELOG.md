# Changelog

All notable changes to podlib are documented in this file.

Also available in Russian: [CHANGELOG.ru.md](CHANGELOG.ru.md).

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - Unreleased

First public release: core library, CLI and GUI for managing music libraries
on USB-connected iPods, verified end-to-end on iPod Nano 7G.

### Library (`crates/podlib`)

- Domain model (tracks, playlists) behind the `LibraryBackend` trait; the
  libopod adapter is the only format-aware component
- Firmware-safe writes: every change is staged in an edit session and
  installed atomically; Nano 7G checksum files (HASHAB/CBK) are re-signed
  automatically
- Classic click-wheel iPod support (phase 2): raw iTunesDB parsing and
  serialization, re-tiling, HASH58 signing

### CLI (`crates/podlib-cli`)

- Browse tracks and playlists, add/remove/edit tracks, create/rename/delete
  playlists, manage playlist members
- iPod mount autodetection

### GUI (`crates/podlib-gui`)

- One-click connect (the sudo mount flow, automated) and safe eject
  (sync → unmount drain → power-off, a single sudo password prompt)
- Drag & drop import with ffmpeg transcode (FLAC/OGG/WAV → AAC) and a
  tag-review modal for untagged files
- Track tables with checkbox multi-select and select-all; batch
  add-to-playlist / remove actions
- Compact toolbar icon buttons with tooltips and right-click context menus —
  no button overflow on narrow windows
- Column sorting in track tables: ascending → descending → back to the device
  order, stable across refreshes
- Playlist management from the sidebar: create, rename and delete playlists
  (deleting a playlist keeps its tracks in the library)
- Music views by albums and artists, unified with the audiobooks view;
  m4b chapter groups with ffprobe duration probing

### Fixed

- Tracks added through the transcode flow (FLAC → ALAC, OGG → AAC) now get
  their real duration instead of 0:00

### Packaging

- Release workflow: a `v*` tag builds Linux x86_64 (gnu) and aarch64 (musl)
  tarballs with sha256 checksums; `cargo binstall` metadata on both binaries

### Upstream contributions

- NULL-safe rank queries (real firmware databases contain artist rows with
  NULL names), an orphan-artist delete guard and empty-key reindexing were
  contributed upstream and merged in
  [koriwi/libopod#1](https://github.com/koriwi/libopod/pull/1)

[0.1.0]: https://github.com/apilot/podlib/releases/tag/v0.1.0
