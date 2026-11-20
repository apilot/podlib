# podlib

**podlib** is a free software library and command-line tool for managing music libraries on USB-connected iPods — a modern Rust take on the classic libgpod + gtkpod workflow.

Also available in Russian: [README.ru.md](README.ru.md).

## Features

- Browse tracks and playlists stored on the device
- Add, remove and edit tracks (title, artist, album, genre, track/disc numbers)
- Create and delete playlists, add and remove tracks from playlists
- Firmware-safe writes: changes are staged and installed atomically, and the on-device checksum files (HASHAB/CBK) are re-signed automatically

## Supported devices

| Device | Status |
|---|---|
| iPod Nano 7th generation | ✅ Verified on real hardware (firmware 1.1.2) |
| Classic click-wheel iPods | Planned (phase 2) |

podlib is built on [libopod](https://github.com/koriwi/libopod) (vendored, see below) — the first library with write support for the Nano 7G database format. Our database-layer fixes (NULL-safe rank queries, orphan-artist delete guard, empty-key reindex) are merged upstream as [koriwi/libopod#1](https://github.com/koriwi/libopod/pull/1).

## Installing

Prebuilt binaries (`podlib` + `podlib-gui`, Linux x86_64/aarch64) are published
by the release workflow on every `v*` tag. Install both with
[cargo-binstall](https://github.com/cargo-bins/cargo-binstall):

```sh
cargo binstall podlib-cli podlib-gui
```

The GUI additionally needs ffmpeg and a desktop GL stack at runtime — see
[Distro notes](#distro-notes).

## Building

Requirements:

- Rust 1.95 or newer (via [rustup](https://rustup.rs) recommended)
- A C compiler (gcc/clang) — SQLite is bundled via libopod and compiled at build time
- [ffmpeg](https://ffmpeg.org) — runtime, used by the GUI to transcode FLAC/OGG/WAV files to AAC on add
- The GUI (`podlib-gui`) additionally needs a desktop GL stack (Wayland or X11 client libraries) — present on any regular desktop installation

```sh
git clone https://github.com/apilot/podlib.git
cd podlib
cargo build --release
```

The `podlib` CLI binary lands in `target/release/`; `podlib-gui` is the desktop app.

### Distro notes

The CLI needs only the Rust toolchain — SQLite is bundled, no system libraries required.

| Distro | Command | Note |
|---|---|---|
| Arch | `pacman -S rust` | current enough |
| Fedora | `dnf install rust cargo` | current enough |
| Gentoo | `emerge --ask dev-lang/rust` | current enough |
| Debian / Ubuntu | use [rustup](https://rustup.rs) | distro packages are often older than the required 1.95 |

GUI extras (ffmpeg for transcoding; a C compiler for the bundled SQLite):

| Distro | Command |
|---|---|
| Arch | `pacman -S ffmpeg base-devel` |
| Fedora | `dnf install ffmpeg gcc` (ffmpeg via [RPM Fusion](https://rpmfusion.org)) |
| Gentoo | `emerge --ask media-video/ffmpeg` |
| Debian / Ubuntu | `sudo apt install ffmpeg build-essential` |

Mounting from the GUI uses `sudo`; `udisksctl` (part of udisks2) is recommended for the safe eject flow.

## Usage

```sh
podlib <MOUNT> list tracks
podlib <MOUNT> list playlists
podlib <MOUNT> playlist create "Road trip" 3220638850441492002
podlib <MOUNT> playlist add "Road trip" 4091926143709481856
podlib <MOUNT> track add --title "Song" --artist "Artist" --album "Album" /path/to/song.mp3
podlib <MOUNT> track remove 3220638850441492002 --purge
```

`<MOUNT>` is the mounted iPod filesystem root (for example `/run/media/user/07516d6c-ab53-3096-a656-d954226a0226`). Always eject safely afterwards — see below.

### Linux mount note

Most desktop distributions (udisks 2.9+) mount HFS+ iPod filesystems **read-only** by default. To write to the device, remount it read-write:

```sh
sudo mount -t hfsplus -o force,uid=$(id -u),gid=$(id -g),umask=22,nls=utf8 /dev/sdX2 /media/ipod
```

A dirty HFS+ journal blocks write mounts — run `fsck.hfsplus -f` against the unmounted volume first. Before disconnecting, unmount and power the device off:

```sh
sync
udisksctl unmount -b /dev/sdX2
udisksctl power-off -b /dev/sdX
```

A GUI (`podlib-gui`) ships with one-click connect (this mount flow is automated inside the app).

## How it works

- `crates/podlib` — the library: a small domain model (tracks, playlists), a `LibraryBackend` trait, and an adapter over [libopod](https://github.com/koriwi/libopod)
- `crates/podlib-cli` — the `podlib` command-line tool
- `crates/vendor/libopod` — pinned copy of libopod; its database-layer fixes are merged upstream ([koriwi/libopod#1](https://github.com/koriwi/libopod/pull/1)), podlib-specific edit-session fixes stay local until a crates.io release with playlist support (see [CONTRIBUTING.md](CONTRIBUTING.md))

Writes never touch the live database directly: an edit session is staged in a temporary directory and installed atomically, and the Nano 7G checksum files (CBK) are re-signed with the device's FireWire GUID. The whole flow is verified end-to-end on a Nano 7G: playlists created and deleted, tracks added and removed, firmware starts normally afterwards.

## Roadmap

1. ✅ Core library + CLI (iPod Nano 7G)
2. ✅ Classic iPod database (iTunesDB) backend
3. Smart playlists, artwork, ratings and play counts, import/export, orphan and duplicate detection
4. ✅ Graphical interface (drag & drop, one-click connect)

## License

podlib is free software: you can redistribute it and/or modify it under the terms of the [MIT license](LICENSE).

Third-party components:

- [libopod](https://github.com/koriwi/libopod) (vendored) — MIT license; podlib's fixes to it are merged upstream as [koriwi/libopod#1](https://github.com/koriwi/libopod/pull/1)
- SQLite (bundled via libopod) — public domain

podlib is an independent implementation developed from public research of the iPod database formats. It contains no Apple source code.

## Contributing

Bug reports, feature ideas and pull requests are welcome at the [issue tracker](https://github.com/apilot/podlib/issues). For larger changes, please open an issue first to discuss the approach. See [CONTRIBUTING.md](CONTRIBUTING.md) for the development setup and conventions: unit tests (`cargo test`) and a clean `cargo clippy` run are expected.
