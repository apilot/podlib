//! Thin UI layer: panels, confirm modals and the worker-thread plumbing.
//!
//! Every blocking step (sudo mount, eject sequence, library open) runs on
//! a background thread and reports through an mpsc channel; the UI only
//! drains it with `try_recv` (egui FAQ: never block the UI thread), and
//! workers wake the event loop via `ctx.request_repaint()`. The library
//! backend itself lives on its own persistent worker thread — see
//! [`crate::library`] for why.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, TryRecvError};

use eframe::egui;
use egui_extras::{Column, TableBuilder};
use podlib::{Playlist, PlaylistId, Track, TrackDraft, TrackId};

use crate::device::{self, DeviceCandidate, MountError};
use crate::library::{
    changes_from_track, drafts_from_paths, guard_playlist_mutation, needs_transcode, panic_message,
    path_extension, plural, EditDialogFields, DraggedTracks, LibraryCommand, LibraryOutcome,
    LibrarySnapshot, LibraryWorker,
};

/// Row height of the track table rows, keeps the column grid aligned.
/// 26px (round 4 UX): a touch taller for a comfortable click/hover target.
const ROW_HEIGHT: f32 = 26.0;
/// Fixed column widths of the track table: artist/album/duration. The
/// title column takes the remainder (see `track_table`).
const ARTIST_COLUMN: f32 = 180.0;
const ALBUM_COLUMN: f32 = 200.0;
const DURATION_COLUMN: f32 = 64.0;

/// Messages from worker threads to the UI.
enum DeviceEvent {
    /// Device mounted read-write; payload is the mount point plus
    /// whether THIS run performed the mount (round 5.3: only then does
    /// app close unmount it).
    Connected(Result<(PathBuf, bool), ConnectError>),
    Ejected(Result<(), String>),
}

#[derive(Debug)]
enum ConnectError {
    Mount { error: MountError, stderr: String },
    /// Worker thread panicked mid-operation; operation state is unknown.
    Internal(String),
}

/// One scanned device candidate with its resolved block device.
struct DeviceEntry {
    candidate: DeviceCandidate,
    block_dev: Option<PathBuf>,
}

/// Device currently connected through the GUI.
struct ConnectedDevice {
    mount: PathBuf,
    block_dev: PathBuf,
    /// Hardware label from sysfs (e.g. «Apple iPod»), frozen at connect
    /// time so the app bar stays stable even if the scan list changes.
    label: String,
}

/// Error prepared for display: message + optional advice + raw tool output.
struct UiError {
    message: String,
    hint: Option<String>,
    stderr: Option<String>,
}

/// Decision of the confirm-on-close guard (F4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloseDecision {
    /// Nothing to do, or safe to let the window close.
    Allow,
    /// Cancel the OS close and open the confirmation modal.
    BlockWithConfirm,
}

/// Close-guard state machine: an OS close while an operation is in
/// flight would kill the detached worker thread mid-persist (possible
/// corruption of the DB on the device), so the first close is cancelled
/// and confirmed. After the user confirms, later close requests pass
/// through — no confirm loop. While idle the window closes as usual.
/// Round 5.3: what a confirmed window close does — a device mounted by
/// this app run is ejected first (the window closes when the eject
/// lands), anything else closes immediately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloseAction {
    EjectFirst,
    CloseNow,
}

fn close_confirmation_action(mounted_by_us: bool) -> CloseAction {
    if mounted_by_us {
        CloseAction::EjectFirst
    } else {
        CloseAction::CloseNow
    }
}

/// A deferred eject (close requested while the worker was busy) fires
/// as soon as the app is idle again.
fn deferred_eject_due(pending: bool, busy: bool, library_idle: bool) -> bool {
    pending && !busy && library_idle
}
fn close_decision(
    close_requested: bool,
    operation_active: bool,
    close_confirmed: bool,
) -> CloseDecision {
    if !close_requested || close_confirmed || !operation_active {
        return CloseDecision::Allow;
    }
    CloseDecision::BlockWithConfirm
}

/// Which operation the UI is waiting on; at most one at a time. Only
/// bookkeeping — the outcome itself carries the fresh snapshot and the
/// ready-made status text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingKind {
    Refresh,
    AddTracks,
    RemoveTrack,
    EditTrack,
    CreatePlaylist,
    DeletePlaylist,
    AddToPlaylist,
    RemoveFromPlaylist,
}

/// Edit-dialog state: which track plus the text as shown on screen.
struct EditDialogState {
    id: TrackId,
    fields: EditDialogFields,
    error: Option<String>,
}

/// Track-table choices collected during one frame, applied afterwards
/// (the render loop borrows the snapshot immutably; mutating actions
/// need the unique borrow back). Last menu click wins, as before.
#[derive(Default)]
struct TrackRowActions {
    edit: Option<TrackId>,
    add_to_playlist: Option<Vec<TrackId>>,
    delete: Option<TrackId>,
    /// Keyboard cursor move (the last touched row).
    select: Option<TrackId>,
    /// Click on a visible row: (index, ctrl, shift) — resolved by the
    /// caller against the visible list (apply_row_click).
    row_click: Option<(usize, bool, bool)>,
    /// «Убрать из плейлиста» in a playlist view: (playlist, member).
    remove_from_playlist: Option<(PlaylistId, TrackId)>,
}

impl TrackRowActions {
    fn merge(&mut self, other: Self) {
        self.edit = other.edit.or(self.edit.take());
        self.add_to_playlist = other.add_to_playlist.or(self.add_to_playlist.take());
        self.delete = other.delete.or(self.delete.take());
        self.select = other.select.or(self.select.take());
        self.row_click = other.row_click.or(self.row_click.take());
        self.remove_from_playlist = other
            .remove_from_playlist
            .or(self.remove_from_playlist.take());
    }
}

/// Playlist-panel choices collected during one frame, applied after the
/// render loop (same borrow discipline as [`TrackRowActions`]).
#[derive(Default)]
struct PlaylistPanelActions {
    /// Tracks dropped onto a user playlist.
    add_drop: Option<(PlaylistId, Vec<TrackId>)>,
    /// Tracks dropped onto the master playlist: refused with a hint.
    master_drop: Option<String>,
    /// Tracks dropped onto a built-in device view: refused with a hint.
    view_drop: Option<String>,
    /// «Удалить…» chosen on the master row: refused honestly, no
    /// round trip to the device.
    master_delete: Option<String>,
    delete_playlist: Option<PlaylistId>,
}

impl PlaylistPanelActions {
    fn merge(&mut self, other: Self) {
        self.add_drop = other.add_drop.or(self.add_drop.take());
        self.master_drop = other.master_drop.or(self.master_drop.take());
        self.view_drop = other.view_drop.or(self.view_drop.take());
        self.master_delete = other.master_delete.or(self.master_delete.take());
        self.delete_playlist = other.delete_playlist.or(self.delete_playlist.take());
    }
}

/// Live library UI state: the worker handle plus a local snapshot that is
/// replaced wholesale after every operation (full re-list, KISS).
struct LibraryUi {
    worker: LibraryWorker,
    snapshot: LibrarySnapshot,
    /// Reply channel of the initial open; yields exactly one message.
    opening: Option<Receiver<LibraryOutcome>>,
    /// The single operation in flight, with its reply channel.
    pending: Option<(PendingKind, Receiver<LibraryOutcome>)>,
    // Dialog/confirm state (modal flags live here so they survive frames).
    /// Rows awaiting the delete confirmation: one or many (Ф3).
    confirm_delete_track: Option<Vec<TrackId>>,
    purge_media: bool,
    confirm_delete_playlist: Option<PlaylistId>,
    create_open: bool,
    new_playlist_name: String,
    edit_dialog: Option<EditDialogState>,
    picker_tracks: Option<Vec<TrackId>>,
    /// Sidebar view context (round 4.1 Ф2): master, a built-in device
    /// view or a user playlist.
    view: LibraryView,
    /// Keyboard cursor (the last touched row); arrows and Enter work
    /// through it without disturbing the multi-selection.
    selected_track: Option<TrackId>,
    /// Multi-selection in the central table, in click order.
    selected_tracks: Vec<TrackId>,
    /// Anchor row (index into the visible list) for shift-range clicks.
    selection_anchor: Option<usize>,
    /// Remaining rows of a multi-track delete (one in-flight worker
    /// command at a time; the chain continues on each Applied).
    delete_queue: Vec<TrackId>,
    /// Expanded books in the «Аудиокниги» view (round 5), keyed by
    /// album. All collapsed by default; stale keys (a book renamed or
    /// gone) simply never render — no fix-up needed.
    expanded_books: HashSet<String>,
    /// Toolbar search box; empty string = no filter.
    search_query: String,
}

pub struct PodlibApp {
    /// Saved once (AppCreator) so worker threads can request repaints.
    ctx: egui::Context,
    devices: Vec<DeviceEntry>,
    selected: Option<usize>,
    password: String,
    confirm_mount: bool,
    confirm_eject: bool,
    /// Confirm-close modal is open: an operation was in flight when the
    /// user asked to close the window.
    confirm_close: bool,
    /// The user confirmed closing despite an in-flight operation; the
    /// guard lets subsequent close requests pass (no confirm loop).
    force_close: bool,
    start_mount: bool,
    start_eject: bool,
    busy: bool,
    status: String,
    error: Option<UiError>,
    rx: Option<Receiver<DeviceEvent>>,
    connected: Option<ConnectedDevice>,
    /// Present while a device is connected and its library worker is open.
    library: Option<LibraryUi>,
    /// Deferred «Убрать из плейлиста» from the toolbar (the request
    /// needs the worker free; collected during the render pass).
    pending_remove_from_playlist: Option<(PlaylistId, Vec<TrackId>)>,
    /// True only when THIS app run performed the mount (round 5.3):
    /// closing the app then unmounts the volume; a skipped idempotent
    /// mount (already mounted externally) never sets it.
    mounted_by_us: bool,
    /// Open tag-review modal (round 5.5): an untagged batch is edited
    /// here before the regular AddTracks dispatch; lives on the app so
    /// it survives even while the library is opening.
    tag_review: Option<TagReview>,
    /// A confirmed close waits for the eject to finish before the
    /// window actually closes (round 5.3: close = safe unmount when
    /// the app created the mount).
    pending_close_after_eject: bool,
}

impl PodlibApp {
    pub fn new(ctx: egui::Context) -> Self {
        // WHY the theme follows the system: owner requirement (round 4
        // UX) — the app must not impose dark/light on the desktop.
        ctx.set_theme(egui::ThemePreference::System);
        let mut app = Self {
            ctx,
            devices: Vec::new(),
            selected: None,
            password: String::new(),
            confirm_mount: false,
            confirm_eject: false,
            confirm_close: false,
            force_close: false,
            start_mount: false,
            start_eject: false,
            busy: false,
            status: String::new(),
            error: None,
            rx: None,
            connected: None,
            library: None,
            pending_remove_from_playlist: None,
            mounted_by_us: false,
            pending_close_after_eject: false,
            tag_review: None,
        };
        app.refresh_devices();
        app
    }

    fn refresh_devices(&mut self) {
        self.devices = device::discover_devices()
            .into_iter()
            .map(|(candidate, block_dev)| DeviceEntry {
                block_dev,
                candidate,
            })
            .collect();
        self.selected = None;
    }

    fn selected_entry(&self) -> Option<&DeviceEntry> {
        self.selected.and_then(|index| self.devices.get(index))
    }

    // ---- event draining ----

    fn drain_events(&mut self) {
        // Take the receiver so event handlers can freely mutate `self`.
        let rx = match self.rx.take() {
            Some(rx) => rx,
            None => return,
        };
        loop {
            match rx.try_recv() {
                Ok(event) => self.handle_event(event),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    // Worker thread finished; nothing left to report.
                    self.busy = false;
                    return;
                }
            }
        }
        self.rx = Some(rx);
    }

    fn handle_event(&mut self, event: DeviceEvent) {
        self.busy = false;
        match event {
            DeviceEvent::Connected(Ok((mount, mounted_by_us))) => {
                self.mounted_by_us = mounted_by_us;
                let (block_dev, label) = self
                    .selected_entry()
                    .map(|entry| {
                        (
                            entry.block_dev.clone().unwrap_or_default(),
                            entry.candidate.label.clone(),
                        )
                    })
                    .unwrap_or_else(|| (Default::default(), "устройство".to_owned()));
                self.connected = Some(ConnectedDevice {
                    mount: mount.clone(),
                    block_dev,
                    label,
                });
                self.status = "Открываю библиотеку…".to_owned();
                self.start_library(mount);
            }
            DeviceEvent::Connected(Err(error)) => {
                self.error = Some(match error {
                    ConnectError::Mount { error, stderr } => UiError {
                        message: error.to_string(),
                        hint: error.hint().map(str::to_owned),
                        stderr: Some(stderr),
                    },
                    ConnectError::Internal(message) => UiError {
                        message: format!("Внутренняя ошибка операции: {message}"),
                        hint: Some(
                            "Состояние операции после сбоя неизвестно; нажмите «Обновить»."
                                .to_owned(),
                        ),
                        stderr: None,
                    },
                });
                self.status.clear();
            }
            DeviceEvent::Ejected(Ok(())) => {
                self.connected = None;
                self.error = None;
                self.mounted_by_us = false;
                self.status = "Устройство безопасно отключено".to_owned();
                self.refresh_devices();
                // Round 5.3: a close requested while the device was
                // mounted completes after the eject lands.
                if self.pending_close_after_eject {
                    self.pending_close_after_eject = false;
                    self.force_close = true;
                    self.ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
            DeviceEvent::Ejected(Err(message)) => {
                // The library was closed before the sequence ran, so the
                // volume may still be mounted — say so honestly.
                let hint = if self.connected.is_some() {
                    Some(
                        "Библиотека закрыта, но том мог остаться смонтированным. \
                         Подключите устройство заново, чтобы продолжить работу."
                            .to_owned(),
                    )
                } else {
                    None
                };
                self.error = Some(UiError {
                    message,
                    hint,
                    stderr: None,
                });
                self.status.clear();
            }
        }
    }

    // ---- library lifecycle ----

    /// Spawns the library worker for a freshly mounted volume. The
    /// backend is opened inside the worker thread (`Box<dyn
    /// LibraryBackend>` is not `Send` — see library.rs for the rationale).
    fn start_library(&mut self, mount: PathBuf) {
        let (worker, opened) = LibraryWorker::spawn(mount);
        self.library = Some(LibraryUi {
            worker,
            snapshot: LibrarySnapshot::default(),
            opening: Some(opened),
            pending: None,
            confirm_delete_track: None,
            purge_media: false,
            confirm_delete_playlist: None,
            create_open: false,
            new_playlist_name: String::new(),
            edit_dialog: None,
            picker_tracks: None,
            view: LibraryView::Master,
            selected_track: None,
            selected_tracks: Vec::new(),
            selection_anchor: None,
            delete_queue: Vec::new(),
            expanded_books: HashSet::new(),
            search_query: String::new(),
        });
    }

    /// True while no library operation is in flight.
    fn library_idle(&self) -> bool {
        self.library
            .as_ref()
            .is_some_and(|library| library.pending.is_none() && library.opening.is_none())
    }

    fn library_busy(&self) -> bool {
        self.library
            .as_ref()
            .is_some_and(|library| library.pending.is_some() || library.opening.is_some())
    }

    fn drain_library(&mut self) {
        // 1) The initial open result arrives on its own channel. Each
        // match arm re-borrows the field it needs so `self.library = None`
        // arms stay legal.
        let opening = self.library.as_mut().and_then(|library| library.opening.take());
        if let Some(rx) = opening {
            match rx.try_recv() {
                Ok(LibraryOutcome::Loaded(snapshot)) => {
                    if let Some(library) = self.library.as_mut() {
                        library.snapshot = snapshot;
                        // Russian plural forms: «1 трек», «2 трека», «5 треков».
                        let tracks = library.snapshot.tracks.len();
                        let playlists = library.snapshot.playlists.len();
                        self.status = format!(
                            "Подключено: {} {}, {} {}",
                            tracks,
                            plural(tracks, "трек", "трека", "треков"),
                            playlists,
                            plural(playlists, "плейлист", "плейлиста", "плейлистов"),
                        );
                    }
                }
                Ok(LibraryOutcome::Failed { message, hint }) => {
                    self.error = Some(UiError {
                        message,
                        hint,
                        stderr: None,
                    });
                    self.status.clear();
                    // Dead worker; the volume stays mounted.
                    self.library = None;
                }
                Err(TryRecvError::Empty) => {
                    if let Some(library) = self.library.as_mut() {
                        library.opening = Some(rx);
                    }
                }
                Err(TryRecvError::Disconnected) => {
                    self.error = Some(UiError {
                        message: "Воркер библиотеки завершился, не открыв её".to_owned(),
                        hint: Some("Подключите устройство заново.".to_owned()),
                        stderr: None,
                    });
                    self.library = None;
                }
                Ok(unexpected) => {
                    // The open channel yields exactly one message; anything
                    // else is a protocol bug. Keep waiting rather than lie.
                    let _ = unexpected;
                    if let Some(library) = self.library.as_mut() {
                        library.opening = Some(rx);
                    }
                }
            }
            if self.library.is_none() {
                return;
            }
        }

        // 2) The single pending operation, if any.
        let pending = self.library.as_mut().and_then(|library| library.pending.take());
        if let Some((kind, rx)) = pending {
            match rx.try_recv() {
                Ok(LibraryOutcome::Closed) => {
                    self.library = None;
                }
                Ok(LibraryOutcome::Loaded(snapshot)) => {
                    if let Some(library) = self.library.as_mut() {
                        library.snapshot = snapshot;
                    }
                    self.status = "Список обновлён".to_owned();
                }
                Ok(LibraryOutcome::Applied { snapshot, summary }) => {
                    if let Some(library) = self.library.as_mut() {
                        library.snapshot = snapshot;
                    }
                    self.status = summary;
                    // Multi-delete chaining: fire the next queued
                    // RemoveTrack once the previous one landed.
                    if matches!(kind, PendingKind::RemoveTrack) {
                        self.continue_delete_chain();
                    }
                }
                Ok(LibraryOutcome::Failed { message, hint }) => {
                    self.error = Some(UiError {
                        message,
                        hint,
                        stderr: None,
                    });
                    self.status.clear();
                    // A failed step stops the delete chain: the error
                    // must be resolved before removing more rows.
                    if let Some(library) = self.library.as_mut() {
                        library.delete_queue.clear();
                    }
                }
                Err(TryRecvError::Empty) => {
                    if let Some(library) = self.library.as_mut() {
                        library.pending = Some((kind, rx));
                    }
                }
                Err(TryRecvError::Disconnected) => {
                    self.error = Some(UiError {
                        message: "Воркер библиотеки завершился во время операции".to_owned(),
                        hint: Some(
                            "Состояние операции неизвестно; отключите и подключите устройство \
                             заново."
                                .to_owned(),
                        ),
                        stderr: None,
                    });
                    self.library = None;
                }
            }
        }
    }

    /// Sends one library command and remembers to drain its reply. One
    /// operation at a time; failures to send are surfaced as UI errors.
    /// Multi-delete chaining (Ф3): the worker protocol allows one
    /// command in flight, so the remaining ids queue and fire as each
    /// RemoveTrack lands. The purge flag is read fresh per step (the
    /// dialog keeps it in state). A failed step cleared the queue
    /// before this runs — the error must be resolved first.
    fn continue_delete_chain(&mut self) {
        let next = self
            .library
            .as_ref()
            .and_then(|library| library.delete_queue.first().copied());
        let purge = self
            .library
            .as_ref()
            .map(|library| library.purge_media)
            .unwrap_or(false);
        if let Some(id) = next {
            if let Some(library) = self.library.as_mut() {
                library.delete_queue.remove(0);
            }
            self.library_request(
                LibraryCommand::RemoveTrack {
                    id,
                    purge_media: purge,
                },
                PendingKind::RemoveTrack,
                "Удаляю трек…",
            );
        }
    }

    /// Sends one library command and remembers to drain its reply. One
    /// operation at a time; failures to send are surfaced as UI errors.
    /// Returns `true` only when the worker accepted the command, so a
    /// caller can keep pre-send UI state (e.g. a modal with typed
    /// input) on a visible refusal.
    fn library_request(
        &mut self,
        command: LibraryCommand,
        kind: PendingKind,
        status: &str,
    ) -> bool {
        // No library at all is not "busy" — the honest refusal points at
        // connecting a device instead of a wait that would never end.
        let Some(library) = self.library.as_ref() else {
            self.error = Some(UiError {
                message: "Сначала подключите устройство".to_owned(),
                hint: None,
                stderr: None,
            });
            return false;
        };
        if library.pending.is_some() || library.opening.is_some() {
            // Never drop an action silently: buttons inside modals and
            // context-menu choices are not gated on idleness, so a busy
            // worker must refuse visibly instead of ignoring the request.
            self.error = Some(UiError {
                message: "Дождитесь завершения текущей операции".to_owned(),
                hint: None,
                stderr: None,
            });
            return false;
        }
        match library.worker.request(command) {
            Ok(rx) => {
                if let Some(library) = self.library.as_mut() {
                    library.pending = Some((kind, rx));
                }
                self.status = status.to_owned();
                true
            }
            Err(message) => {
                self.error = Some(UiError {
                    message,
                    hint: None,
                    stderr: None,
                });
                false
            }
        }
    }

    // ---- device worker threads ----

    fn spawn_connect(&mut self) {
        let Some(entry) = self.selected_entry() else {
            return;
        };
        let Some(block_dev) = entry.block_dev.clone() else {
            self.error = Some(UiError {
                message: "Не удалось определить блочное устройство по sysfs".to_owned(),
                hint: Some("Нажмите «Обновить» и выберите устройство заново.".to_owned()),
                stderr: None,
            });
            return;
        };
        let mount_path = entry.candidate.mount.clone();
        let Some((uid, gid)) = device::current_uid_gid() else {
            // W4: nothing has run yet, so the UI error must not claim a
            // mounted volume — surfaced locally, worker never spawned.
            self.error = Some(UiError {
                message: "не удалось определить uid/gid процесса".to_owned(),
                hint: None,
                stderr: None,
            });
            return;
        };
        // mkdir -p + rw-mount: one sequence for mounted and unmounted
        // candidates alike (mkdir -p on an existing path is a no-op).
        let steps = device::build_mount_sequence(&device::MountSpec {
            block_dev: block_dev.clone(),
            mount_path: mount_path.clone(),
            uid,
            gid,
        });
        let password = std::mem::take(&mut self.password);
        let ctx = self.ctx.clone();
        let (tx, rx) = channel();
        self.rx = Some(rx);
        self.busy = true;
        self.error = None;
        self.status = format!("Монтирование {} (rw)…", block_dev.display());
        std::thread::spawn(move || {
            // catch_unwind: a worker panic must still produce an event,
            // otherwise the UI would silently stay busy forever.
            let result = run_worker_step(
                || connect_device(password, steps, mount_path, block_dev.clone()),
                ConnectError::Internal,
            );
            let _ = tx.send(DeviceEvent::Connected(result));
            ctx.request_repaint(); // wake the sleeping event loop
        });
    }

    fn spawn_eject(&mut self) {
        let Some(connected) = self.connected.as_ref() else {
            return;
        };
        // Round 5.3: sync → unmount (draining stacked mounts of the
        // same devnode, up to five repeats while /proc/mounts still
        // lists the source) → power off the whole disk.
        let steps = {
            let mut steps = vec![device::CommandSpec::new("sync", &[])];
            steps.extend(
                device::build_unmount_sequence(
                    &connected.mount,
                    &connected.block_dev,
                    Path::new("/sys/dev/block"),
                )
                .into_iter()
                .skip(1), // drop the sequence's own leading sync — the explicit sync above already flushed
            );
            steps.push(device::CommandSpec::new("sync", &[]));
            steps
        };
        let block_dev = connected.block_dev.display().to_string();
        // Take the library out: the backend must be closed (and joined)
        // before the unmount sequence, on the eject thread itself.
        let library = self.library.take();
        let ctx = self.ctx.clone();
        let (tx, rx) = channel();
        self.rx = Some(rx);
        self.busy = true;
        self.error = None;
        self.status =
            "Отключение: закрытие библиотеки → sync → unmount → power-off…".to_owned();
        std::thread::spawn(move || {
            if let Some(library) = library {
                // Blocking close: the backend owns files on the mounted
                // volume, so it must be gone before sync/unmount.
                library.worker.close_and_join();
            }
            // catch_unwind: a worker panic must still produce an event,
            // otherwise the UI would silently stay busy forever.
            let result = run_worker_step(
                || {
                    device::run_sequence(&steps).map_err(|error| error.to_string())?;
                    // Drain stacked mounts: the first unmount may leave
                    // sibling mounts of the same devnode alive.
                    for _ in 0..5 {
                        let mounts = std::fs::read_to_string("/proc/mounts")
                            .map(|content| device::parse_proc_mounts(&content))
                            .unwrap_or_default();
                        if !device::mounts_contain_source(&mounts, Path::new(&block_dev)) {
                            break;
                        }
                        let _ = std::process::Command::new("udisksctl")
                            .args(["unmount", "-b", &block_dev])
                            .status();
                    }
                    Ok(())
                },
                |message| format!("внутренняя ошибка: {message}"),
            );
            let _ = tx.send(DeviceEvent::Ejected(result));
            ctx.request_repaint();
        });
    }

    // ---- panels ----

    fn device_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("Устройство");
        ui.add_space(4.0);
        if ui
            .add_enabled(!self.busy, egui::Button::new("Обновить"))
            .clicked()
        {
            self.refresh_devices();
        }
        ui.separator();
        if self.devices.is_empty() {
            ui.label(
                "Устройства не найдены. Подключите устройство по USB и нажмите \
                 «Обновить» — podlib находит iPod по sysfs даже без automount.",
            );
            return;
        }
        egui::ScrollArea::vertical().show(ui, |ui| {
            // Selection is frozen while an operation runs (the worker
            // holds the chosen device).
            if self.busy {
                ui.disable();
            }
            for (index, entry) in self.devices.iter().enumerate() {
                let dev = entry
                    .block_dev
                    .as_deref()
                    .map(|dev| dev.display().to_string())
                    .unwrap_or_else(|| "блочное устройство не определено".to_owned());
                // Unmounted candidates show the planned mount point so
                // the user knows where «Подключить» will put the volume.
                let place = match entry.candidate.kind {
                    device::CandidateKind::Mounted => entry.candidate.mount.display().to_string(),
                    device::CandidateKind::Unmounted => format!(
                        "не смонтирован → будет в {}",
                        entry.candidate.mount.display()
                    ),
                };
                let text = format!("{}\n{}\n{}", entry.candidate.label, place, dev);
                ui.selectable_value(&mut self.selected, Some(index), text);
            }
        });
        ui.separator();
        let ready = !self.busy && self.connected.is_none() && self.selected.is_some();
        if ui
            .add_enabled(ready, egui::Button::new("Подключить"))
            .clicked()
        {
            self.password.clear();
            self.confirm_mount = true;
        }
    }

    /// Top app bar: app name left; device state and the eject button
    /// right. Replaces the old in-panel «podlib» heading.
    fn app_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("podlib");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                match self.connected.as_ref() {
                    Some(connected) => {
                        let ready = !self.busy && self.library_idle();
                        if ui
                            .add_enabled(ready, egui::Button::new("Безопасно отключить"))
                            .clicked()
                        {
                            self.confirm_eject = true;
                        }
                        // WHY a painted circle: there is no stock dot
                        // widget; a 4px filled circle reads as a status dot.
                        let (rect, _) =
                            ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
                        ui.painter()
                            .circle_filled(rect.center(), 4.0, egui::Color32::LIGHT_GREEN);
                        ui.label(
                            egui::RichText::new(format!(
                                "{} · {}",
                                connected.label,
                                connected.block_dev.display()
                            ))
                            .weak(),
                        );
                    }
                    None => {
                        ui.label(egui::RichText::new("Нет устройства").weak());
                    }
                }
            });
        });
    }

    /// Bottom status bar: operation progress / last result on the left
    /// (spinner while busy), the error in red — visible until replaced,
    /// with the full text + advice + stderr on hover — and the
    /// add-files hint moved here from the old toolbar.
    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let busy = self.busy || self.library_busy();
            if busy {
                ui.add(egui::Spinner::new());
                ui.label(&self.status);
            } else if !self.status.is_empty() {
                ui.label(&self.status);
            } else if self.error.is_none() {
                // Round 5.3 Ф1: the bar never looks empty/missing —
                // a neutral text keeps its height and presence.
                ui.label(egui::RichText::new("Готово").weak());
            }
            if let Some(error) = &self.error {
                ui.colored_label(egui::Color32::LIGHT_RED, &error.message)
                    .on_hover_text(error_tooltip(error));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    egui::RichText::new("Перетащите аудиофайлы в окно, чтобы добавить")
                        .weak(),
                );
            });
        });
    }

    /// Left sidebar once a device is connected: the master row, the
    /// built-in device views (media-kind filters) and user playlists,
    /// each selectable, with «+ Новый плейлист» at the bottom.
    /// Collects row choices (drop/delete/ПКМ) for post-frame applying.
    fn playlists_sidebar(&mut self, ui: &mut egui::Ui) -> PlaylistPanelActions {
        let menus_allowed = context_menu_allowed(self);
        let Some(library) = self.library.as_mut() else {
            return PlaylistPanelActions::default();
        };
        ui.heading("Плейлисты");
        ui.add_space(4.0);
        let ordered = master_first(&library.snapshot.playlists);
        let master = ordered.iter().find(|playlist| playlist.is_master).copied();
        let views: Vec<&Playlist> = ordered
            .iter()
            .copied()
            .filter(|playlist| !playlist.is_master && playlist.distinguished_kind != 0)
            .collect();
        let user: Vec<&Playlist> = ordered
            .iter()
            .copied()
            .filter(|playlist| !playlist.is_master && playlist.distinguished_kind == 0)
            .collect();
        let mut actions = PlaylistPanelActions::default();
        let view_before = library.view;
        // auto_shrink(false): the list reserves the full height so the
        // button below lands at the panel bottom.
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show(ui, |ui| {
                if let Some(playlist) = master {
                    actions.merge(playlist_row(
                        ui,
                        &library.snapshot,
                        playlist,
                        &mut library.view,
                        menus_allowed,
                    ));
                }
                if !views.is_empty() {
                    ui.label(egui::RichText::new("Устройство").weak().small());
                    for playlist in &views {
                        actions.merge(playlist_row(
                            ui,
                            &library.snapshot,
                            playlist,
                            &mut library.view,
                            menus_allowed,
                        ));
                    }
                }
                if !user.is_empty() {
                    ui.label(egui::RichText::new("Плейлисты").weak().small());
                    for playlist in &user {
                        actions.merge(playlist_row(
                            ui,
                            &library.snapshot,
                            playlist,
                            &mut library.view,
                            menus_allowed,
                        ));
                    }
                }
            });
        // A context switch must not drag the old table selection along.
        if library.view != view_before {
            library.selected_tracks.clear();
            library.selection_anchor = None;
            library.selected_track = None;
        }
        ui.separator();
        if ui.button("+ Новый плейлист").clicked() {
            library.create_open = true;
            library.new_playlist_name.clear();
        }
        actions
    }

    /// Central panel: toolbar (search + counter + selection actions)
    /// and the track table for the selected sidebar context. Applies
    /// everything it collects itself (borrow discipline: the render
    /// pass holds the library borrow, requests fire afterwards).
    fn library_view(&mut self, ui: &mut egui::Ui) {
        let menus_allowed = context_menu_allowed(self);
        let mut actions = TrackRowActions::default();
        let toolbar;
        {
            let Some(library) = self.library.as_mut() else {
                return;
            };
            let query = library.search_query.clone();
            let view = library.view;
            let cursor = library.selected_track;

            // Visible rows for the selected context: master → the whole
            // library listing, device view → tracks matching its media
            // kinds, user playlist → its members in order (rows that
            // vanished from the library are skipped).
            let (table_id, total, visible): (egui::Id, usize, Vec<&Track>) = match view {
                LibraryView::Master => (
                    egui::Id::unique(("track_table", 0u64)),
                    library.snapshot.tracks.len(),
                    library.snapshot.tracks.iter().collect(),
                ),
                LibraryView::Media { distinguished } => {
                    let kinds = view_media_kinds(distinguished);
                    let matches: Vec<&Track> = library
                        .snapshot
                        .tracks
                        .iter()
                        .filter(|track| kinds.contains(&track.media_kind))
                        .collect();
                    (
                        // id per view context: switching sections starts
                        // a fresh scroll position.
                        egui::Id::unique(("track_table", u64::try_from(distinguished.max(0)).unwrap_or(0))),
                        matches.len(),
                        matches,
                    )
                }
                LibraryView::Playlist(id) => {
                    let members: Vec<&Track> = library
                        .snapshot
                        .playlist(id)
                        .map(|playlist| {
                            playlist
                                .unique_track_ids()
                                .into_iter()
                                .filter_map(|member| library.snapshot.track(member))
                                .collect()
                        })
                        .unwrap_or_default();
                    (
                        egui::Id::unique(("track_table", id.to_bits())),
                        members.len(),
                        members,
                    )
                }
            };
            let filtered: Vec<&Track> = visible
                .iter()
                .copied()
                .filter(|track| matches_query(track, &query))
                .collect();
            let visible_ids: Vec<TrackId> = filtered.iter().map(|track| track.id).collect();
            let empty_view = matches!(view, LibraryView::Media { .. }) && total == 0;
            let empty_playlist = matches!(view, LibraryView::Playlist(_)) && total == 0;

            let idle = library.pending.is_none() && library.opening.is_none();
            let ctx = ui.ctx().clone();
            toolbar = search_toolbar(
                ui,
                SearchToolbarParams {
                    ctx: &ctx,
                    query: &mut library.search_query,
                    shown: filtered.len(),
                    total,
                    idle,
                    selection_len: library.selected_tracks.len(),
                    playlist_context: matches!(view, LibraryView::Playlist(_)),
                },
            );
            ui.add_space(4.0);

            if filtered.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.label(egui::RichText::new(empty_state_text(
                        library.snapshot.tracks.is_empty(),
                        &query,
                        empty_view,
                        empty_playlist,
                    )).weak());
                });
            } else if matches!(view, LibraryView::Media { distinguished: 5 }) {
                // Round 5: audiobooks group into books — a flat
                // collapsed-by-default list instead of a flat table.
                actions.merge(books_view(
                    ui,
                    BooksViewParams {
                        ctx: &ctx,
                        tracks: &filtered,
                        selected_tracks: &library.selected_tracks,
                        cursor,
                        menus_allowed,
                        visible_ids: &visible_ids,
                        expanded: &mut library.expanded_books,
                    },
                ));
            } else {
                actions.merge(track_table(
                    ui,
                    &filtered,
                    &library.selected_tracks,
                    cursor,
                    view,
                    menus_allowed,
                    table_id,
                ));
            }

            // A click on a visible row resolves through apply_row_click
            // (plain/ctrl/shift) against the current visible list. A
            // stale anchor (list shrank since the anchored click) is
            // dropped here: shift degenerates to a plain click.
            if let Some((index, ctrl, shift)) = actions.row_click.take() {
                let anchor = library.selection_anchor.filter(|&a| a < visible_ids.len());
                let selected_idx: Vec<usize> = library
                    .selected_tracks
                    .iter()
                    .filter_map(|id| visible_ids.iter().position(|candidate| candidate == id))
                    .collect();
                let (new_indices, anchor) =
                    apply_row_click(&selected_idx, anchor, index, ctrl, shift);
                library.selected_tracks = new_indices
                    .iter()
                    .filter_map(|&index| visible_ids.get(index).copied())
                    .collect();
                library.selection_anchor = anchor;
                library.selected_track = library.selected_tracks.last().copied();
            }

            // Toolbar actions over the multi-selection.
            let selected = library.selected_tracks.clone();
            if toolbar.edit && selected.len() == 1 {
                let id = selected[0];
                if let Some(track) = library.snapshot.track(id) {
                    library.edit_dialog = Some(EditDialogState {
                        id,
                        fields: EditDialogFields::from_track(track),
                        error: None,
                    });
                }
            }
            if toolbar.add && !selected.is_empty() {
                library.picker_tracks = Some(selected.clone());
            }
            if toolbar.remove && !selected.is_empty() {
                if let LibraryView::Playlist(playlist_id) = view {
                    self.pending_remove_from_playlist = Some((playlist_id, selected.clone()));
                } else {
                    library.confirm_delete_track = Some(selected.clone());
                    library.purge_media = false;
                }
            }
            drop(selected);

            // Keyboard navigation over the visible (filtered) rows.
            // WHY the focus gate: arrows/Enter in the search box or in
            // a modal TextEdit must keep their native meaning.
            let focused = ui.ctx().memory(|m| m.focused().is_some());
            if !focused {
                let (up, down, enter) = ui.ctx().input(|input| {
                    (
                        input.key_pressed(egui::Key::ArrowUp),
                        input.key_pressed(egui::Key::ArrowDown),
                        input.key_pressed(egui::Key::Enter),
                    )
                });
                if down {
                    if let Some(id) = next_selection(&visible_ids, cursor, 1) {
                        actions.select = Some(id);
                    }
                }
                if up {
                    if let Some(id) = next_selection(&visible_ids, cursor, -1) {
                        actions.select = Some(id);
                    }
                }
                if enter {
                    if let Some(id) = activated(&visible_ids, cursor) {
                        actions.edit = Some(id);
                    }
                }
            }
        }
        // Deferred worker requests (borrow discipline): refresh and the
        // playlist-member removal collected above.
        if toolbar.refresh {
            self.library_request(
                LibraryCommand::Refresh,
                PendingKind::Refresh,
                "Обновляю список…",
            );
        }
        if let Some((playlist, track_ids)) = self.pending_remove_from_playlist.take() {
            self.library_request(
                LibraryCommand::RemoveFromPlaylist {
                    playlist,
                    track_ids,
                },
                PendingKind::RemoveFromPlaylist,
                "Убираю из плейлиста…",
            );
        }
        self.apply_track_actions(actions);
    }

    /// Applies the row choices collected by [`track_table`].
    fn apply_track_actions(&mut self, actions: TrackRowActions) {
        if let Some(id) = actions.select {
            if let Some(library) = self.library.as_mut() {
                library.selected_track = Some(id);
            }
        }
        if let Some(id) = actions.edit {
            if let Some(library) = self.library.as_mut() {
                if let Some(track) = library.snapshot.track(id) {
                    library.edit_dialog = Some(EditDialogState {
                        id,
                        fields: EditDialogFields::from_track(track),
                        error: None,
                    });
                }
            }
        }
        if let Some(track_ids) = actions.add_to_playlist {
            if let Some(library) = self.library.as_mut() {
                library.picker_tracks = Some(track_ids);
            }
        }
        if let Some(id) = actions.delete {
            if let Some(library) = self.library.as_mut() {
                library.confirm_delete_track = Some(vec![id]);
                library.purge_media = false;
            }
        }
        if let Some((playlist, member)) = actions.remove_from_playlist {
            self.library_request(
                LibraryCommand::RemoveFromPlaylist {
                    playlist,
                    track_ids: vec![member],
                },
                PendingKind::RemoveFromPlaylist,
                "Убираю из плейлиста…",
            );
        }
    }

    /// Applies the panel choices collected during the frame.
    fn apply_playlist_actions(&mut self, actions: PlaylistPanelActions) {
        if let Some(name) = actions.master_drop {
            self.error = Some(UiError {
                message: format!("«{name}» — мастер-плейлист: добавлять в него нельзя"),
                hint: Some(
                    "Создайте пользовательский плейлист («+ Плейлист») и бросьте трек на него."
                        .to_owned(),
                ),
                stderr: None,
            });
        }
        if let Some(name) = actions.master_delete {
            // Same honest-refusal pattern as master_drop: the master is
            // the mirror of the library, deletion is a user-playlist
            // operation — explained up front, no round trip to the device.
            self.error = Some(UiError {
                message: format!("«{name}» — мастер-плейлист: его нельзя удалить"),
                hint: Some(
                    "Удалять можно только пользовательские плейлисты.".to_owned(),
                ),
                stderr: None,
            });
        }
        if let Some((playlist, track_ids)) = actions.add_drop {
            self.library_request(
                LibraryCommand::AddToPlaylist {
                    playlist,
                    track_ids,
                },
                PendingKind::AddToPlaylist,
                "Добавляю в плейлист…",
            );
        }
        if let Some(id) = actions.delete_playlist {
            if let Some(library) = self.library.as_mut() {
                library.confirm_delete_playlist = Some(id);
            }
        }
    }

    // ---- file drag-and-drop from the file manager ----

    fn collect_dropped_files(&mut self, ui: &egui::Ui) {
        // Copy the paths immediately: egui owns them for this frame only.
        let dropped: Vec<PathBuf> = ui
            .input(|input| {
                input
                    .raw
                    .dropped_files
                    .iter()
                    // path() is infallible on native (&Path, not Option).
                    .map(|file| PathBuf::from(file.path()))
                    .collect()
            });
        if dropped.is_empty() {
            return;
        }
        // Round 5.1 diagnostics (permanent): does winit deliver
        // Wayland drops at all, and how do they map to drafts? The
        // output lands in podlib-gui.log when run redirected.
        let names: Vec<String> = dropped
            .iter()
            .map(|path| path.display().to_string())
            .collect();
        eprintln!("[dnd] dropped {} file(s): {:?}", dropped.len(), names);
        let (drafts, skipped) = drafts_from_paths(&dropped);
        eprintln!(
            "[dnd] drafts {} ({} for transcode), skipped {}",
            drafts.len(),
            drafts
                .iter()
                .filter(|draft| needs_transcode(&path_extension(&draft.source_path)))
                .count(),
            skipped.len()
        );
        // Round 5.5: while the review modal is open new drops are
        // ignored (the user is mid-edit; dispatching behind their back
        // would lose the reviewed tags).
        if self.tag_review.is_some() {
            return;
        }
        if drafts.is_empty() {
            if !skipped.is_empty() {
                self.status = format!("Пропущены не-аудио файлы: {}", skipped.join(", "));
            }
            return;
        }
        let transcodes = drafts
            .iter()
            .filter(|draft| needs_transcode(&path_extension(&draft.source_path)))
            .count();
        // Round 5.5: any untagged draft in the batch opens the review
        // modal for the WHOLE batch instead of dispatching blind.
        if drafts.iter().any(is_untagged) {
            eprintln!(
                "[dnd] tag review opened ({} untagged of {})",
                drafts.iter().filter(|draft| is_untagged(draft)).count(),
                drafts.len()
            );
            self.tag_review = Some(open_tag_review(drafts));
            return;
        }
        let status = batch_add_status(drafts.len(), transcodes, &skipped);
        self.library_request(
            LibraryCommand::AddTracks(drafts),
            PendingKind::AddTracks,
            &status,
        );
    }

    // ---- modals ----

    /// F4: intercepts the OS close request while an operation is in
    /// flight, cancels it and opens the confirmation modal instead.
    /// Documented eframe 0.37 pattern: check
    /// `ctx.input(|i| i.viewport().close_requested())` during the frame
    /// and answer with `ViewportCommand::CancelClose`.
    fn guard_close_request(&mut self, ui: &egui::Ui) {
        let close_requested = ui.input(|input| input.viewport().close_requested());
        let operation_active = self.busy || self.library_busy();
        if close_decision(close_requested, operation_active, self.force_close)
            == CloseDecision::BlockWithConfirm
        {
            // CancelClose must be sent during this frame, or eframe exits.
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.confirm_close = true;
        }
    }

    fn maybe_mount_modal(&mut self, ctx: &egui::Context) {
        if !self.confirm_mount {
            return;
        }
        let mut confirmed = false;
        let response = egui::Modal::new(egui::Id::unique("confirm_mount")).show(ctx, |ui| {
            ui.set_width(440.0);
            ui.heading("Подключить устройство (запись)");
            ui.add_space(4.0);
            ui.label(
                "Система монтирует том HFS+ только для чтения; для записи нужен \
                 повторный mount с force.",
            );
            ui.label(
                "Пароль sudo нужен, потому что монтирование — системное действие. \
                 Пароль передаётся напрямую команде sudo и нигде не сохраняется.",
            );
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label("Пароль sudo:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.password)
                        .password(true)
                        .desired_width(220.0),
                );
            });
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Отмена").clicked() {
                    ui.close();
                }
                let ready = !self.password.is_empty();
                if ui.add_enabled(ready, egui::Button::new("Подключить")).clicked() {
                    confirmed = true;
                    ui.close();
                }
            });
        });
        if confirmed || response.should_close() {
            self.confirm_mount = false;
        }
        if confirmed {
            self.start_mount = true; // act after the frame (borrow rules)
        }
    }

    fn maybe_eject_modal(&mut self, ctx: &egui::Context) {
        if !self.confirm_eject {
            return;
        }
        let mut confirmed = false;
        let response = egui::Modal::new(egui::Id::unique("confirm_eject")).show(ctx, |ui| {
            ui.set_width(400.0);
            ui.heading("Безопасно отключить устройство?");
            ui.add_space(4.0);
            ui.label("Будут выполнены: закрытие библиотеки, sync, unmount, power-off.");
            ui.label("Убедитесь, что операции записи завершены.");
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Отмена").clicked() {
                    ui.close();
                }
                if ui.button("Отключить").clicked() {
                    confirmed = true;
                    ui.close();
                }
            });
        });
        if confirmed || response.should_close() {
            self.confirm_eject = false;
        }
        if confirmed {
            self.start_eject = true;
        }
    }

    /// F4: asks before closing the window mid-operation. Confirming
    /// sends `ViewportCommand::Close`, which surfaces as a fresh close
    /// request the guard lets through (`force_close`).
    fn maybe_close_confirm_modal(&mut self, ctx: &egui::Context) {
        if !self.confirm_close {
            return;
        }
        let mut confirmed = false;
        let response = egui::Modal::new(egui::Id::unique("confirm_close")).show(ctx, |ui| {
            ui.set_width(440.0);
            ui.heading("Идёт операция с библиотекой");
            ui.add_space(4.0);
            ui.label("Закрыть всё равно? Возможно повреждение базы на устройстве.");
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Отмена").clicked() {
                    ui.close();
                }
                if ui.button("Закрыть").clicked() {
                    confirmed = true;
                    ui.close();
                }
            });
        });
        if confirmed || response.should_close() {
            self.confirm_close = false;
        }
        if confirmed {
            // Round 5.3: a device mounted by this app run is unmounted
            // before the window closes (the owner's requirement); a
            // busy worker defers the eject until the app goes idle.
            match close_confirmation_action(self.mounted_by_us) {
                CloseAction::EjectFirst => {
                    self.pending_close_after_eject = true;
                    if deferred_eject_due(true, self.busy, self.library_idle()) {
                        self.start_eject = true;
                    }
                }
                CloseAction::CloseNow => {
                    self.force_close = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
    }

    /// Delete confirmation for one row or a multi-selection (Ф3).
    /// Confirming queues every id: the worker protocol allows one
    /// command in flight, so the chain continues on each Applied
    /// outcome (see `continue_delete_chain`).
    fn maybe_delete_track_modal(&mut self, ctx: &egui::Context) {
        let Some(track_ids) = self
            .library
            .as_ref()
            .and_then(|library| library.confirm_delete_track.clone())
        else {
            return;
        };
        let title_line = if track_ids.len() == 1 {
            let title = self
                .library
                .as_ref()
                .and_then(|library| library.snapshot.track(track_ids[0]))
                .map(|track| track.title.clone())
                .unwrap_or_default();
            format!("Удалить трек «{title}»?")
        } else {
            let count = track_ids.len();
            format!(
                "Удалить {} {}?",
                count,
                plural(count, "трек", "трека", "треков")
            )
        };
        let mut confirmed = false;
        let response = egui::Modal::new(egui::Id::unique("confirm_delete_track")).show(ctx, |ui| {
            ui.set_width(440.0);
            ui.heading(title_line);
            ui.add_space(4.0);
            ui.label("Треки исчезнут из библиотеки и всех плейлистов.");
            if let Some(library) = self.library.as_mut() {
                // The checkbox lives in the state so it survives frames.
                let label = if track_ids.len() == 1 {
                    "Удалить и медиа-файл с устройства"
                } else {
                    "Удалить и медиа-файлы с устройства"
                };
                ui.checkbox(&mut library.purge_media, label);
            }
            ui.label(
                egui::RichText::new(
                    "Без галочки аудиофайлы останутся на диске как «осиротевшие».",
                )
                .weak(),
            );
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Отмена").clicked() {
                    ui.close();
                }
                if ui.button("Удалить").clicked() {
                    confirmed = true;
                    ui.close();
                }
            });
        });
        if confirmed || response.should_close() {
            if let Some(library) = self.library.as_mut() {
                library.confirm_delete_track = None;
            }
        }
        if confirmed {
            let purge = self
                .library
                .as_ref()
                .map(|library| library.purge_media)
                .unwrap_or(false);
            let (first, rest) = match track_ids.split_first() {
                Some((first, rest)) => (*first, rest.to_vec()),
                None => return,
            };
            if let Some(library) = self.library.as_mut() {
                library.delete_queue = rest;
            }
            self.library_request(
                LibraryCommand::RemoveTrack {
                    id: first,
                    purge_media: purge,
                },
                PendingKind::RemoveTrack,
                "Удаляю трек…",
            );
        }
    }

    fn maybe_delete_playlist_modal(&mut self, ctx: &egui::Context) {
        let Some(playlist_id) = self
            .library
            .as_ref()
            .and_then(|library| library.confirm_delete_playlist)
        else {
            return;
        };
        let name = self
            .library
            .as_ref()
            .and_then(|library| library.snapshot.playlist(playlist_id))
            .map(|playlist| playlist.name.clone())
            .unwrap_or_default();
        let mut confirmed = false;
        let response =
            egui::Modal::new(egui::Id::unique("confirm_delete_playlist")).show(ctx, |ui| {
                ui.set_width(400.0);
                ui.heading(format!("Удалить плейлист «{name}»?"));
                ui.add_space(4.0);
                ui.label("Сами треки останутся в библиотеке.");
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Отмена").clicked() {
                        ui.close();
                    }
                    if ui.button("Удалить").clicked() {
                        confirmed = true;
                        ui.close();
                    }
                });
            });
        if confirmed || response.should_close() {
            if let Some(library) = self.library.as_mut() {
                library.confirm_delete_playlist = None;
            }
        }
        if confirmed {
            self.library_request(
                LibraryCommand::DeletePlaylist { id: playlist_id },
                PendingKind::DeletePlaylist,
                "Удаляю плейлист…",
            );
        }
    }

    fn maybe_create_playlist_modal(&mut self, ctx: &egui::Context) {
        if !self.library.as_ref().is_some_and(|library| library.create_open) {
            return;
        }
        let mut confirmed = false;
        let mut name = String::new();
        let response = egui::Modal::new(egui::Id::unique("create_playlist")).show(ctx, |ui| {
            ui.set_width(380.0);
            ui.heading("Новый плейлист");
            ui.add_space(4.0);
            let Some(library) = self.library.as_mut() else {
                return;
            };
            ui.horizontal(|ui| {
                ui.label("Название:");
                ui.add(
                    egui::TextEdit::singleline(&mut library.new_playlist_name)
                        .desired_width(240.0),
                );
            });
            name = library.new_playlist_name.clone();
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Отмена").clicked() {
                    ui.close();
                }
                let ready = !library.new_playlist_name.trim().is_empty();
                if ui.add_enabled(ready, egui::Button::new("Создать")).clicked() {
                    confirmed = true;
                    ui.close();
                }
            });
        });
        if confirmed || response.should_close() {
            if let Some(library) = self.library.as_mut() {
                library.create_open = false;
            }
        }
        if confirmed && !name.trim().is_empty() {
            self.library_request(
                LibraryCommand::CreatePlaylist { name },
                PendingKind::CreatePlaylist,
                "Создаю плейлист…",
            );
        }
    }

    fn maybe_edit_track_modal(&mut self, ctx: &egui::Context) {
        if !self
            .library
            .as_ref()
            .is_some_and(|library| library.edit_dialog.is_some())
        {
            return;
        }
        let mut save = false;
        let response = egui::Modal::new(egui::Id::unique("edit_track")).show(ctx, |ui| {
            ui.set_width(500.0);
            ui.heading("Изменить трек");
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(
                    "Жанр не редактируется: библиотека не отдаёт его при чтении, \
                     и правка перезаписала бы его пустым значением.",
                )
                .weak(),
            );
            ui.add_space(4.0);
            let Some(library) = self.library.as_mut() else {
                return;
            };
            let Some(dialog) = library.edit_dialog.as_mut() else {
                return;
            };
            save = edit_track_dialog_fields(ui, dialog);
        });
        if !(save || response.should_close()) {
            return; // still open, nothing to apply
        }
        self.apply_edit_track_dialog(save);
    }

    /// Closes/commits the edit dialog: takes it out (the id survives by
    /// contract while the dialog stays open), diffs the fields and then
    /// sends EditTrack — or reopens with the validation error.
    fn apply_edit_track_dialog(&mut self, save: bool) {
        // Take the dialog out to work without nested borrows.
        let Some(library) = self.library.as_mut() else {
            return;
        };
        let Some(dialog) = library.edit_dialog.take() else {
            return;
        };
        if !save {
            return; // plain close: no request, id is untouched
        }
        let attempt = library
            .snapshot
            .track(dialog.id)
            .map(|track| changes_from_track(track, &dialog.fields));
        match attempt {
            // Unchanged dialog → no request, so the id survives by contract.
            Some(Ok(changes)) if changes.is_empty() => {
                self.status = "Изменений нет".to_owned();
            }
            Some(Ok(changes)) => {
                self.library_request(
                    LibraryCommand::EditTrack {
                        id: dialog.id,
                        changes,
                    },
                    PendingKind::EditTrack,
                    "Сохраняю изменения…",
                );
            }
            // Validation failure: reopen with the inline error.
            Some(Err(message)) => {
                if let Some(library) = self.library.as_mut() {
                    library.edit_dialog = Some(EditDialogState {
                        error: Some(message),
                        ..dialog
                    });
                }
            }
            // The track vanished between frames; keep the dialog closed.
            None => {}
        }
    }

    /// Applies the reviewed tags (round 5.5): dispatches the edited
    /// drafts through the regular AddTracks pipeline and closes the
    /// modal. An empty book title keeps the modal open with the
    /// near-field error (validation_attempted); an apply error lands
    /// in the status bar honestly, and a refused request (busy, no
    /// device) keeps the modal open with the typed tags.
    fn commit_tag_review(&mut self) {
        let Some(review) = self.tag_review.as_mut() else {
            return;
        };
        review.validation_attempted = true;
        if review.book_title.trim().is_empty() {
            return; // near-field error is rendered by the modal
        }
        match apply_tag_review(
            &review.drafts,
            &review.book_title,
            &review.author,
            &review.rows,
        ) {
            Ok(drafts) => {
                let transcodes = drafts
                    .iter()
                    .filter(|draft| needs_transcode(&path_extension(&draft.source_path)))
                    .count();
                let status = batch_add_status(drafts.len(), transcodes, &[]);
                // Send first, close on success only: a refusal keeps
                // the modal with the typed tags; the error itself is
                // already shown by library_request.
                if self.library_request(
                    LibraryCommand::AddTracks(drafts),
                    PendingKind::AddTracks,
                    &status,
                ) {
                    self.tag_review = None;
                }
            }
            Err(message) => {
                self.tag_review = None;
                self.error = Some(UiError {
                    message,
                    hint: None,
                    stderr: None,
                });
            }
        }
    }

    /// Round 5.5: the tag-review modal — common book/author fields and
    /// an inline per-file table (№, editable title, file name), the
    /// shared-prefix checkbox and honest near-field validation.
    fn maybe_tag_review_modal(&mut self, ctx: &egui::Context) {
        if self.tag_review.is_none() {
            return;
        }
        let mut cancelled = false;
        let mut committed = false;
        let response = egui::Modal::new(egui::Id::unique("tag_review_modal")).show(ctx, |ui| {
            let Some(review) = self.tag_review.as_mut() else {
                return;
            };
            let title = if review.kind_bits == 8 {
                format!(
                    "Добавление книги: проверьте теги ({} файлов)",
                    review.rows.len()
                )
            } else {
                format!(
                    "Добавление музыки: проверьте теги ({} файлов)",
                    review.rows.len()
                )
            };
            ui.set_width(560.0);
            ui.heading(title);
            ui.add_space(4.0);
            ui.label("Общее для всех файлов");
            let kind_field = if review.kind_bits == 8 {
                "Название книги*"
            } else {
                "Альбом"
            };
            ui.horizontal(|ui| {
                ui.label(kind_field);
                ui.add(
                    egui::TextEdit::singleline(&mut review.book_title)
                        .desired_width(320.0),
                );
            });
            if review.validation_attempted && review.book_title.trim().is_empty() {
                ui.colored_label(egui::Color32::LIGHT_RED, "Укажите название книги");
            }
            ui.horizontal(|ui| {
                ui.label("Автор");
                ui.add(
                    egui::TextEdit::singleline(&mut review.author).desired_width(320.0),
                );
            });
            ui.add_space(6.0);
            if let Some(prefix) = review.prefix.clone() {
                if ui
                    .checkbox(
                        &mut review.strip_prefix,
                        format!("Убрать общий префикс из названий: «{prefix}»"),
                    )
                    .changed()
                {
                    for (row, draft) in review.rows.iter_mut().zip(&review.drafts) {
                        row.title = if review.strip_prefix {
                            strip_prefix_in(&draft.title, &prefix)
                        } else {
                            draft.title.clone()
                        };
                    }
                }
            }
            ui.label("Файлы — проверьте названия (Tab — переход между полями):");
            // Round 5.7 bug 2: NO dnd wrapper here — an empty-payload
            // drag source sat on top of the row and swallowed mouse
            // clicks, so the title TextEdit could only be focused via
            // Tab. Rows render directly; the focused TextEdit IS the
            // row selection («файл за файлом»).
            if review.row_rects.len() != review.rows.len()
                || review.row_edit_ids.len() != review.rows.len()
                || review.row_edit_rects.len() != review.rows.len()
            {
                review.row_rects = vec![egui::Rect::NOTHING; review.rows.len()];
                review.row_edit_ids = vec![egui::Id::NULL; review.rows.len()];
                review.row_edit_rects = vec![egui::Rect::NOTHING; review.rows.len()];
            }
            let focused_row: Cell<Option<usize>> = Cell::new(None);
            egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                for (index, row) in review.rows.iter_mut().enumerate() {
                    let selected = review.selected_row == Some(index);
                    let row_area = ui.allocate_ui(
                        egui::vec2(ui.available_width(), 24.0),
                        |ui| {
                            if selected {
                                ui.painter().rect_filled(
                                    ui.max_rect(),
                                    2.0,
                                    ui.visuals().selection.bg_fill,
                                );
                            }
                            ui.horizontal(|ui| {
                                ui.label(format!("{:>2}.", row.track_number));
                                let title = ui.add(
                                    egui::TextEdit::singleline(&mut row.title)
                                        .desired_width(300.0)
                                        .id_salt(("tag_review_title", index)),
                                );
                                review.row_edit_ids[index] = title.id;
                                review.row_edit_rects[index] = title.rect;
                                if title.has_focus() {
                                    focused_row.set(Some(index));
                                }
                                let file = &review.files[index];
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(file).weak().small(),
                                    )
                                    .truncate(),
                                )
                                .on_hover_text(file);
                            });
                        },
                    );
                    review.row_rects[index] = row_area.response.rect;
                    if row_area.response.clicked() {
                        review.selected_row = Some(index);
                    }
                }
            });
            // Keyboard navigation («файл за файлом») keeps the row
            // highlighted as long as its editor holds focus.
            if let Some(index) = focused_row.get() {
                review.selected_row = Some(index);
            }
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Отмена").clicked() {
                    cancelled = true;
                    ui.close();
                }
                let add_label = if review.kind_bits == 8 {
                    "Добавить книгу"
                } else {
                    "Добавить музыку"
                };
                if ui.button(add_label).clicked() {
                    review.validation_attempted = true;
                    if !review.book_title.trim().is_empty() {
                        committed = true;
                        ui.close();
                    }
                }
            });
        });
        // Round 5.7 bug 1: «Добавить книгу» sets committed AND calls
        // ui.close(), so should_close() is true in the SAME frame —
        // the old cancel-first order cleared the review before the
        // commit ran, and the dispatch never left the app. The commit
        // branch must be processed first.
        self.finish_tag_review_frame(committed, response.should_close(), cancelled);
    }

    /// Single decision point of the tag-review modal frame: a commit
    /// outranks a cancel/close (a click on «Добавить книгу» closes the
    /// modal in the same frame), otherwise any close/cancel drops the
    /// draft edits intentionally.
    fn finish_tag_review_frame(&mut self, committed: bool, should_close: bool, cancelled: bool) {
        if committed {
            self.commit_tag_review();
        } else if cancelled || should_close {
            self.tag_review = None;
        }
    }

    fn maybe_add_to_playlist_modal(&mut self, ctx: &egui::Context) {
        if !self
            .library
            .as_ref()
            .is_some_and(|library| library.picker_tracks.is_some())
        {
            return;
        }
        // Snapshot the choices so the modal closure stays borrow-free.
        let playlists: Vec<Playlist> = self
            .library
            .as_ref()
            .map(|library| library.snapshot.playlists.clone())
            .unwrap_or_default();
        let mut chosen: Option<PlaylistId> = None;
        let response = egui::Modal::new(egui::Id::unique("add_to_playlist")).show(ctx, |ui| {
            ui.set_width(360.0);
            ui.heading("Добавить в плейлист");
            ui.add_space(4.0);
            egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                for playlist in &playlists {
                    let guarded = guard_playlist_mutation(playlist).is_err();
                    let label = if guarded {
                        format!("{} (нельзя)", playlist.name)
                    } else {
                        playlist.name.clone()
                    };
                    let width = ui.available_width() - 8.0;
                    if ui
                        .add_enabled(
                            !guarded,
                            egui::Button::new(label).min_size([width, 26.0].into()),
                        )
                        .clicked()
                    {
                        chosen = Some(playlist.id);
                        ui.close();
                    }
                }
            });
        });
        let (picker_after_close, add_request) = picker_close(
            chosen,
            response.should_close(),
            self.library.as_ref().and_then(|l| l.picker_tracks.clone()),
        );
        if let Some(library) = self.library.as_mut() {
            library.picker_tracks = picker_after_close;
        }
        if let Some((playlist, track_ids)) = add_request {
            self.library_request(
                LibraryCommand::AddToPlaylist {
                    playlist,
                    track_ids,
                },
                PendingKind::AddToPlaylist,
                "Добавляю в плейлист…",
            );
        }
    }
}

/// Snapshot of picker state after a close attempt: the new picker contents
/// (None = cleared) and, if a playlist was chosen, the (playlist, ids)
/// request payload. Ids are captured BEFORE the clear — see picker_close.
type PickerCloseResult = (
    Option<Vec<TrackId>>,
    Option<(PlaylistId, Vec<TrackId>)>,
);

/// Pure close-handling for the add-to-playlist picker. Track ids are
/// captured from the snapshot BEFORE the picker state clears: the previous
/// inline order cleared first and read after, so the worker received an
/// empty Vec and the UI claimed «Добавлено 0 треков» for a real request.
fn picker_close(
    chosen: Option<PlaylistId>,
    should_close: bool,
    picker_tracks: Option<Vec<TrackId>>,
) -> PickerCloseResult {
    let track_ids = picker_tracks.clone().unwrap_or_default();
    let cleared = chosen.is_some() || should_close;
    let state = if cleared { None } else { picker_tracks };
    let request = chosen.map(|playlist| (playlist, track_ids));
    (state, request)
}

/// Status line shown while a batch add runs (P4). The worker protocol
/// is one command → one reply, so per-file progress («Перекодирую i/M…»)
/// would require extending `LibraryCommand`/`LibraryOutcome` — and the
/// backend trait contract is out of bounds. Instead the wait is
/// announced honestly up front — including how many files need an
/// ffmpeg transcode (round 5.2) — meanwhile every button/menu is
/// disabled by the idle gates and the spinner shows in the status area.
fn batch_add_status(count: usize, transcode_count: usize, skipped: &[String]) -> String {
    let transcode_note = if transcode_count > 0 {
        format!(" ({transcode_count} с транскодом)")
    } else {
        String::new()
    };
    let mut status = format!(
        "Добавляется {} {}{}, это может занять время…",
        count,
        plural(count, "файл", "файла", "файлов"),
        transcode_note
    );
    if !skipped.is_empty() {
        status.push_str(&format!("; пропущены: {}", skipped.join(", ")));
    }
    status
}

// ---- round 4 UX: pure helpers (tested) ----

/// Case-insensitive substring search over title/artist/album. An empty
/// or blank query matches everything (the filter is off).
fn matches_query(track: &Track, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    [track.title.as_str()]
        .into_iter()
        .chain(track.artist.iter().map(String::as_str))
        .chain(track.album.iter().map(String::as_str))
        .any(|field| field.to_lowercase().contains(&query))
}

/// Sidebar ordering: the master playlist first, everything else keeps
/// its snapshot order (stable sort).
fn master_first(playlists: &[Playlist]) -> Vec<&Playlist> {
    let mut ordered: Vec<&Playlist> = playlists.iter().collect();
    ordered.sort_by_key(|playlist| !playlist.is_master);
    ordered
}

/// Next selection moving `delta` rows through the visible (filtered)
/// list, clamped at the edges. A stale or missing selection restarts
/// from the first row; an empty list selects nothing.
fn next_selection(visible: &[TrackId], current: Option<TrackId>, delta: isize) -> Option<TrackId> {
    if visible.is_empty() {
        return None;
    }
    let index = current
        .and_then(|id| visible.iter().position(|&candidate| candidate == id))
        .map_or(0, |index| {
            (index as isize + delta).clamp(0, visible.len() as isize - 1) as usize
        });
    Some(visible[index])
}

/// Enter activation: works only when the selection is actually visible.
fn activated(visible: &[TrackId], current: Option<TrackId>) -> Option<TrackId> {
    match current {
        Some(id) if visible.contains(&id) => Some(id),
        _ => None,
    }
}

/// Toolbar counter: «{N} треков», or «{N} из {total}» while filtering.
fn tracks_counter(shown: usize, total: usize, query: &str) -> String {
    if query.trim().is_empty() {
        format!(
            "{} {}",
            total,
            plural(total, "трек", "трека", "треков")
        )
    } else {
        format!("{} из {}", shown, total)
    }
}

// ---- round 4.1: view contexts and selection (pure helpers, tested) ----

/// What the central table shows (round 4.1 Ф2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum LibraryView {
    /// Whole library listing (master view) — the default.
    #[default]
    Master,
    /// Built-in device view (firmware section) by `distinguished_kind`.
    Media { distinguished: i64 },
    /// A user playlist.
    Playlist(PlaylistId),
}

/// Raw iTunesDB media-kind bits covered by a built-in device view.
/// Kinds follow the firmware: 1 = music, 2 = movie, 4 = podcast,
/// 8 = audiobook, 16 = TV show, 32 = music video. The rentals view
/// (7) is not representable as a media kind — it filters to nothing.
fn view_media_kinds(distinguished_kind: i64) -> &'static [u32] {
    match distinguished_kind {
        1 => &[2, 16, 32], // «Видео»: movies + TV + music videos
        2 => &[2],         // «Фильмы»
        3 => &[16],        // «Телешоу»
        4 => &[1],         // «Музыка»
        5 => &[8],         // «Аудиокниги»
        _ => &[],          // 0 (not a view), 7 («Прокат»), anything unknown
    }
}

/// One click on a visible table row resolved to selection state, in
/// index domain (indices into the current visible list). Plain click
/// resets to a single row; ctrl toggles the row; shift selects the
/// range from the anchor. The caller passes the anchor already
/// validated against the visible list length (a stale anchor — the
/// list shrank since the anchored click — is dropped there, so shift
/// degenerates to a plain click).
fn apply_row_click(
    selected: &[usize],
    anchor: Option<usize>,
    clicked: usize,
    ctrl: bool,
    shift: bool,
) -> (Vec<usize>, Option<usize>) {
    if shift {
        if let Some(start) = anchor {
            let (from, to) = if start <= clicked {
                (start, clicked)
            } else {
                (clicked, start)
            };
            let selected = (from..=to).collect();
            return (selected, Some(start));
        }
    }
    if ctrl {
        let mut selected = selected.to_vec();
        if let Some(position) = selected.iter().position(|&index| index == clicked) {
            selected.remove(position);
        } else {
            selected.push(clicked);
            selected.sort_unstable();
        }
        return (selected, anchor.or(Some(clicked)));
    }
    (vec![clicked], Some(clicked))
}

/// Central-panel empty-state text: empty library, empty filter result,
/// empty device view or empty playlist — in that priority order.
fn empty_state_text(library_empty: bool, query: &str, empty_view: bool, empty_playlist: bool) -> String {
    if library_empty {
        "Перетащите аудиофайлы в окно, чтобы добавить".to_owned()
    } else if !query.trim().is_empty() {
        format!("Ничего не найдено по „{}“", query.trim())
    } else if empty_view {
        "В этом разделе устройства нет записей".to_owned()
    } else if empty_playlist {
        "Пустой плейлист. Перетащите треки из таблицы".to_owned()
    } else {
        // Unreachable with a non-empty library, but keep an honest text.
        "Перетащите аудиофайлы в окно, чтобы добавить".to_owned()
    }
}

/// Full error text for the status-bar tooltip: the one-line bar clips,
/// so message + advice + raw tool output live on hover.
fn error_tooltip(error: &UiError) -> String {
    let mut text = error.message.clone();
    if let Some(hint) = &error.hint {
        text.push('\n');
        text.push_str(hint);
    }
    if let Some(stderr) = &error.stderr {
        if !stderr.trim().is_empty() {
            text.push_str("\n\n");
            text.push_str(stderr);
        }
    }
    text
}

// ---- round 5: audiobook grouping (pure helpers, tested) ----

/// One book inside the «Аудиокниги» view: the album is the book, the
/// files are its members in display order. Duration/count are derived
/// from `track_ids` + the snapshot at render time (single source of
/// truth, KISS) — not stored here.
#[derive(Clone, Debug, PartialEq, Eq)]
struct BookGroup {
    album: String,
    author: String,
    track_ids: Vec<TrackId>,
}

/// The album-as-book key: blank/missing tags collapse into one
/// fallback group (pinned last by the sort — see `group_books`).
const NO_ALBUM_GROUP: &str = "Без альбома";

fn album_group_key(track: &Track) -> String {
    track
        .album
        .as_deref()
        .map(str::trim)
        .filter(|album| !album.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| NO_ALBUM_GROUP.to_owned())
}

/// Sort key of a file inside its book: numbered discs/tracks first
/// (0 = «no number» sorts last among equals), then title
/// case-insensitively.
fn book_file_sort_key(track: &Track) -> (bool, u32, bool, u32, String) {
    (
        track.disc_number == 0,
        track.disc_number,
        track.track_number == 0,
        track.track_number,
        track.title.to_lowercase(),
    )
}

/// Groups the (already filtered) audiobook tracks into books.
/// Groups sort by album case-insensitively; files inside a book by
/// (disc, track, title) with «no number» last. The author is the first
/// non-empty album_artist (Apple semantics: album artist outranks the
/// track artist), then artist, in final file order; without any —
/// «Неизвестный автор».
fn group_books<'a>(tracks: impl IntoIterator<Item = &'a Track>) -> Vec<BookGroup> {
    // Insertion-ordered buckets keep a stable start before sorting.
    let mut buckets: Vec<(String, Vec<&Track>)> = Vec::new();
    for track in tracks {
        let album = album_group_key(track);
        match buckets.iter_mut().find(|(name, _)| *name == album) {
            Some((_, members)) => members.push(track),
            None => buckets.push((album, vec![track])),
        }
    }
    buckets.sort_by(|(album_a, _), (album_b, _)| {
        // The synthetic fallback bucket («Без альбома») pins last:
        // real books first, the catch-all below them (the spec only
        // demands case-insensitive album sorting).
        let a_last = album_a == NO_ALBUM_GROUP;
        let b_last = album_b == NO_ALBUM_GROUP;
        a_last.cmp(&b_last).then_with(|| {
            album_a.to_lowercase().cmp(&album_b.to_lowercase())
        })
    });
    buckets
        .into_iter()
        .map(|(album, mut members)| {
            members.sort_by_key(|track| book_file_sort_key(track));
            let author = members
                .iter()
                .filter_map(|track| {
                    track
                        .album_artist
                        .as_deref()
                        .map(str::trim)
                        .filter(|text| !text.is_empty())
                        .map(str::to_owned)
                        .or_else(|| {
                            track
                                .artist
                                .as_deref()
                                .map(str::trim)
                                .filter(|text| !text.is_empty())
                                .map(str::to_owned)
                        })
                })
                .next()
                .unwrap_or_else(|| "Неизвестный автор".to_owned());
            BookGroup {
                album,
                author,
                track_ids: members.into_iter().map(|track| track.id).collect(),
            }
        })
        .collect()
}

/// One rendered line of the «Аудиокниги» view: a collapsible book
/// header or one of its files. A flat list so the proven index-based
/// selection (`apply_row_click`, anchor, keyboard) keeps working.
/// Total book duration as «H:MM» (sub-minute books show «0:00»).
fn format_total_duration(duration_ms: u64) -> String {
    let minutes = duration_ms / 60_000;
    format!("{}:{:02}", minutes / 60, minutes % 60)
}

// ---- round 5.5: tag review before add (pure helpers, tested) ----

/// A dropped draft has no artist AND no album — it needs the review
/// modal before it can land in the library with honest tags.
fn is_untagged(draft: &TrackDraft) -> bool {
    let empty = |text: &Option<String>| -> bool {
        text.as_deref().map(str::trim).unwrap_or("").is_empty()
    };
    empty(&draft.artist) && empty(&draft.album)
}

/// Longest common stem prefix (≥2 stems), trimmed back to a « - » or
/// space boundary, and only if at least 3 characters survive —
/// otherwise None and the prefix checkbox is not shown.
fn common_prefix(stems: &[String]) -> Option<String> {
    if stems.len() < 2 {
        return None;
    }
    let first = stems.first()?.chars().collect::<Vec<_>>();
    let mut shared = first.len();
    for stem in &stems[1..] {
        shared = shared.min(stem.chars().count());
        for (index, ch) in stem.chars().enumerate().take(shared) {
            if first[index] != ch {
                shared = index;
                break;
            }
        }
    }
    let prefix: String = first[..shared].iter().collect();
    let trimmed = match prefix.rfind(" - ") {
        Some(index) => prefix[..index].to_owned(),
        None => match prefix.rfind(' ') {
            Some(index) => prefix[..index].to_owned(),
            None => prefix.clone(),
        },
    };
    (trimmed.chars().count() >= 3).then_some(trimmed)
}

/// Trailing digits of a stem = the track number («… - 3» → 3,
/// «12» → 12, «12a»/«intro» → None).
fn trailing_number(stem: &str) -> Option<u32> {
    let digits: String = stem
        .chars()
        .rev()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

/// Removes the shared prefix (with its « - »/space tail) from a title.
fn strip_prefix_in(title: &str, prefix: &str) -> String {
    title
        .strip_prefix(prefix)
        .map(|rest| {
            rest.trim_start_matches([' ', '-'])
                .trim_start()
                .to_owned()
        })
        .unwrap_or_else(|| title.to_owned())
}

/// Applies the reviewed tags to the drafts: album/book title for all,
/// author as album_artist (when non-empty), per-row title (an empty
/// edit falls back to the file stem) and track number.
fn apply_tag_review(
    drafts: &[TrackDraft],
    book_title: &str,
    author: &str,
    rows: &[TagRow],
) -> Result<Vec<TrackDraft>, String> {
    let book = book_title.trim();
    if book.is_empty() {
        return Err("Укажите название книги".to_owned());
    }
    let author = author.trim();
    Ok(drafts
        .iter()
        .zip(rows)
        .map(|(draft, row)| {
            let mut prepared = draft.clone();
            prepared.album = Some(book.to_owned());
            if !author.is_empty() {
                prepared.album_artist = Some(author.to_owned());
            }
            let title = row.title.trim();
            prepared.title = if title.is_empty() {
                draft.title.clone()
            } else {
                title.to_owned()
            };
            prepared.track_number = row.track_number;
            prepared
        })
        .collect())
}

/// Prefill of the book title: the common parent directory name of the
/// batch (a dropped directory = the book); single stray files and
/// mixed parents prefill nothing.
fn prefill_book_title(drafts: &[TrackDraft]) -> String {
    if drafts.len() < 2 {
        return String::new();
    }
    let parent = drafts[0].source_path.parent();
    let Some(parent) = parent else {
        return String::new();
    };
    let same_parent = drafts
        .iter()
        .all(|draft| draft.source_path.parent() == Some(parent));
    if same_parent {
        parent
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_owned()
    } else {
        String::new()
    }
}

/// One editable file line of the review modal.
#[derive(Clone, Debug, PartialEq, Eq)]
struct TagRow {
    title: String,
    track_number: u32,
}

/// State of the tag-review modal (round 5.5): the drafts are edited
/// here first and dispatched through the regular AddTracks pipeline on
/// «Добавить».
struct TagReview {
    drafts: Vec<TrackDraft>,
    kind_bits: u32,
    book_title: String,
    author: String,
    strip_prefix: bool,
    prefix: Option<String>,
    /// Display names of the source files (stable for the file column).
    files: Vec<String>,
    /// Screen rects of the rendered file rows (stable modal id), used
    /// by tests to simulate clicks on a row.
    row_rects: Vec<egui::Rect>,
    /// Widget ids of the per-row title edits (focus targeting in tests
    /// and keyboard routing).
    row_edit_ids: Vec<egui::Id>,
    /// Screen rects of the per-row title edits (click targeting).
    row_edit_rects: Vec<egui::Rect>,
    rows: Vec<TagRow>,
    selected_row: Option<usize>,
    validation_attempted: bool,
}

/// Opens the review modal for a batch: rows prefill from the file
/// stems (trailing digits become track numbers, otherwise the 1-based
/// position), the book title prefills from the common parent
/// directory, and the shared-prefix checkbox is pre-enabled when a
/// prefix was found.
fn open_tag_review(drafts: Vec<TrackDraft>) -> TagReview {
    let kind_bits = drafts.first().map(|d| d.media_kind_bits).unwrap_or(1);
    let rows: Vec<TagRow> = drafts
        .iter()
        .enumerate()
        .map(|(index, draft)| TagRow {
            title: draft.title.clone(),
            track_number: trailing_number(&draft.title)
                .unwrap_or(index as u32 + 1),
        })
        .collect();
    let stems: Vec<String> = drafts.iter().map(|d| d.title.clone()).collect();
    let prefix = common_prefix(&stems);
    TagReview {
        kind_bits,
        book_title: prefill_book_title(&drafts),
        author: String::new(),
        strip_prefix: prefix.is_some(),
        prefix,
        files: drafts
            .iter()
            .map(|d| d.source_path.display().to_string())
            .collect(),
        row_rects: Vec::new(),
        row_edit_ids: Vec::new(),
        row_edit_rects: Vec::new(),
        rows,
        selected_row: None,
        validation_attempted: false,
        drafts,
    }
}

/// Actions requested from the table toolbar in one frame.
#[derive(Default)]
struct ToolbarActions {
    refresh: bool,
    edit: bool,
    add: bool,
    remove: bool,
}

/// Aggregated arguments for [`search_toolbar`] (UI chrome above the
/// track table); `ui` stays a plain argument per egui convention.
struct SearchToolbarParams<'a> {
    ctx: &'a egui::Context,
    query: &'a mut String,
    shown: usize,
    total: usize,
    idle: bool,
    selection_len: usize,
    playlist_context: bool,
}

/// Toolbar above the track table: search box (Ctrl+F focuses it when
/// nothing else holds focus), row counter and the selection actions —
/// always visible, enabled only for a non-empty selection (round 4.1
/// Ф3). In a playlist context the delete button becomes
/// «Убрать из плейлиста» (existing RemoveFromPlaylist command).
fn search_toolbar(ui: &mut egui::Ui, params: SearchToolbarParams<'_>) -> ToolbarActions {
    let SearchToolbarParams {
        ctx,
        query,
        shown,
        total,
        idle,
        selection_len,
        playlist_context,
    } = params;
    let mut toolbar = ToolbarActions::default();
    ui.horizontal(|ui| {
        // A stable root id (not id_salt) so Ctrl+F can request focus on
        // the exact box regardless of where the toolbar lands.
        let search_box_id = egui::Id::unique("podlib_search");
        if ctx.input(|input| input.key_pressed(egui::Key::F) && input.modifiers.ctrl)
            && ctx.memory(|memory| memory.focused().is_none())
        {
            ctx.memory_mut(|memory| memory.request_focus(search_box_id));
        }
        let query_before = query.clone();
        ui.add(
            egui::TextEdit::singleline(query)
                .hint_text("Поиск…")
                .id(search_box_id)
                .desired_width(240.0),
        );
        ui.label(tracks_counter(shown, total, &query_before));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let enabled = idle && selection_len > 0;
            if playlist_context {
                if ui
                    .add_enabled(enabled, egui::Button::new("Убрать из плейлиста"))
                    .clicked()
                {
                    toolbar.remove = true;
                }
            } else if ui.add_enabled(enabled, egui::Button::new("Удалить…")).clicked() {
                toolbar.remove = true;
            }
            if ui
                .add_enabled(enabled, egui::Button::new("Добавить в плейлист…"))
                .clicked()
            {
                toolbar.add = true;
            }
            if ui
                .add_enabled(idle && selection_len == 1, egui::Button::new("Изменить…"))
                .clicked()
            {
                toolbar.edit = true;
            }
            if ui
                .add_enabled(idle, egui::Button::new("Обновить"))
                .clicked()
            {
                toolbar.refresh = true;
            }
        });
    });
    toolbar
}

/// Column setup of the track table, shared with the layout test
/// (round 4.1 Ф4): title takes the remainder with a 120px floor, the
/// other three are exact widths. WHY the floor: with `remainder` +
/// fixed columns in a narrow panel the remainder can collapse to 0
/// (`Sizing::to_lengths` clamps negative remainders to 0) and the row
/// loses its title entirely; the floor keeps it readable.
fn track_table_builder<'a>(ui: &'a mut egui::Ui, table_id: egui::Id) -> TableBuilder<'a> {
    TableBuilder::new(ui)
        .id_salt(table_id)
        .striped(true)
        .vscroll(true)
        .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
        .column(Column::remainder().clip(true).resizable(false).at_least(120.0))
        .column(Column::exact(ARTIST_COLUMN).clip(true).resizable(false))
        .column(Column::exact(ALBUM_COLUMN).clip(true).resizable(false))
        .column(Column::exact(DURATION_COLUMN).resizable(false))
}

/// Aggregated arguments for [`books_view`] («Аудиокниги» table);
/// `ui` stays a plain argument per egui convention.
struct BooksViewParams<'a> {
    ctx: &'a egui::Context,
    tracks: &'a [&'a Track],
    selected_tracks: &'a [TrackId],
    cursor: Option<TrackId>,
    menus_allowed: bool,
    visible_ids: &'a [TrackId],
    expanded: &'a mut HashSet<String>,
}

/// The «Аудиокниги» view (round 5): books (albums) collapsed by
/// default; a header click toggles the book; an expanded book shows an
/// info line (author · files · total duration) and its file list.
/// File rows reuse the proven selection machinery: clicks report their
/// index in `visible_ids` (headers are not selectable — they toggle),
/// so apply_row_click / anchor / toolbar / keyboard all work unchanged.
fn books_view(ui: &mut egui::Ui, params: BooksViewParams<'_>) -> TrackRowActions {
    let BooksViewParams {
        ctx,
        tracks,
        selected_tracks,
        cursor,
        menus_allowed,
        visible_ids,
        expanded,
    } = params;
    let mut actions = TrackRowActions::default();
    if !expanded.is_empty() && ui.button("Свернуть все").clicked() {
        expanded.clear();
    }
    let groups = group_books(tracks.iter().copied());
    // Durations and titles for the infoblock/file rows; the groups
    // carry ids only (single source of truth, see BookGroup).
    let by_id: HashMap<TrackId, &Track> =
        tracks.iter().map(|track| (track.id, *track)).collect();
    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .show(ui, |ui| {
            for group in &groups {
                let is_expanded = expanded.contains(&group.album);
                let arrow = if is_expanded { "▾" } else { "▸" };
                let total_ms: u64 = group
                    .track_ids
                    .iter()
                    .filter_map(|id| by_id.get(id).map(|t| u64::from(t.duration_ms)))
                    .sum();
                // First-class clickable row: ONE widget senses the
                // whole line and the content is painted directly —
                // child labels intercept clicks (and select as text),
                // retroactive .interact() does not win over them.
                let (rect, response) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), ROW_HEIGHT),
                    egui::Sense::click(),
                );
                let painter = ui.painter();
                let visuals = ui.visuals();
                if is_expanded {
                    painter.rect_filled(rect, 4.0, visuals.widgets.inactive.bg_fill);
                }
                if response.hovered() {
                    painter.rect_filled(rect, 4.0, visuals.widgets.hovered.bg_fill);
                }
                let inner = rect.shrink2(egui::vec2(8.0, 0.0));
                let middle = rect.center().y;
                painter.text(
                    egui::pos2(inner.left(), middle),
                    egui::Align2::LEFT_CENTER,
                    format!("{arrow} {}", group.album),
                    egui::FontId::proportional(14.0),
                    visuals.strong_text_color(),
                );
                painter.text(
                    egui::pos2(inner.right(), middle),
                    egui::Align2::RIGHT_CENTER,
                    format!(
                        "{} · {}",
                        group.track_ids.len(),
                        format_total_duration(total_ms)
                    ),
                    egui::FontId::proportional(12.0),
                    visuals.weak_text_color(),
                );
                if response.clicked() {
                    // Insert-else-remove = toggle.
                    if !expanded.insert(group.album.clone()) {
                        expanded.remove(&group.album);
                    }
                }
                if is_expanded {
                    // One framed block: info line + file list (owner:
                    // «автор и сводка по книге — в одном выделенном
                    // блоке»); faint fill reads as a single card.
                    egui::Frame::default()
                        .fill(ui.visuals().extreme_bg_color)
                        .corner_radius(4.0)
                        .inner_margin(egui::Margin::same(6))
                        .show(ui, |ui| {
                            let files_text = format!(
                                "{} {}",
                                group.track_ids.len(),
                                plural(group.track_ids.len(), "файл", "файла", "файлов")
                            );
                            ui.label(
                                egui::RichText::new(format!(
                                    "Автор: {} · {} · {}",
                                    group.author,
                                    files_text,
                                    format_total_duration(total_ms)
                                ))
                                .weak()
                                .size(13.0),
                            );
                            ui.add_space(2.0);
                            for (file_idx, track_id) in group.track_ids.iter().enumerate() {
                                let Some(track) = by_id.get(track_id) else {
                                    continue;
                                };
                                book_file_row(
                                    ui,
                                    ctx,
                                    selected_tracks,
                                    cursor,
                                    menus_allowed,
                                    visible_ids,
                                    file_idx,
                                    track,
                                    &mut actions,
                                );
                            }
                        });
                }
            }
        });
    actions
}

/// One audio file inside an expanded book: a painted row that senses
/// click/dblclick (selection machinery via visible_ids indices), ПКМ
/// menu, and drag — the payload is set manually on drag start.
#[allow(clippy::too_many_arguments)]
fn book_file_row(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    selected_tracks: &[TrackId],
    cursor: Option<TrackId>,
    menus_allowed: bool,
    visible_ids: &[TrackId],
    file_idx: usize,
    track: &Track,
    actions: &mut TrackRowActions,
) {
    let track_id = track.id;
    let selected = selected_tracks.contains(&track_id) || cursor == Some(track_id);
    let payload = DraggedTracks(vec![track_id]);
    let mut clicked = false;
    let mut double_clicked = false;
    // Manual drag source: sense click+drag on the painted row and set
    // the payload on drag start. `dnd_drag_source` re-renders the row
    // into a screen-space tooltip layer and translates by
    // `pointer - rect.center()` — panel coords vs screen coords mix,
    // throwing the row to the left screen edge (owner artifact).
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), ROW_HEIGHT),
        egui::Sense::click() | egui::Sense::drag(),
    );
    let painter = ui.painter();
    let visuals = ui.visuals();
    if selected {
        painter.rect_filled(rect, 2.0, visuals.selection.bg_fill);
    } else if response.hovered() {
        painter.rect_filled(rect, 2.0, visuals.widgets.hovered.bg_fill);
    }
    let inner = rect.shrink2(egui::vec2(4.0, 0.0));
    let middle = rect.center().y;
    painter.text(
        egui::pos2(inner.left(), middle),
        egui::Align2::LEFT_CENTER,
        format!("{}. {}", file_idx + 1, track.title),
        egui::FontId::proportional(14.0),
        visuals.text_color(),
    );
    painter.text(
        egui::pos2(inner.right(), middle),
        egui::Align2::RIGHT_CENTER,
        format_duration(track.duration_ms),
        egui::FontId::proportional(12.0),
        visuals.weak_text_color(),
    );
    let response = response.on_hover_cursor(egui::CursorIcon::Grab);
    if response.drag_started() {
        egui::DragAndDrop::set_payload(ctx, payload);
    }
    if response.clicked() {
        clicked = true;
    }
    if response.double_clicked() {
        double_clicked = true;
    }
    // Double-click wins over plain click (round 4).
    if double_clicked {
        actions.edit = Some(track_id);
        actions.select = Some(track_id);
    } else if clicked {
        let (ctrl, shift) = ctx.input(|input| (input.modifiers.ctrl, input.modifiers.shift));
        if let Some(index) = visible_ids.iter().position(|&candidate| candidate == track_id) {
            actions.row_click = Some((index, ctrl, shift));
        }
    }
    if menus_allowed {
        let mut choice = TrackRowActions::default();
        response.context_menu(|ui| {
            if ui.button("Изменить…").clicked() {
                choice.edit = Some(track_id);
                ui.close();
            }
            if ui.button("Добавить в плейлист…").clicked() {
                choice.add_to_playlist = Some(vec![track_id]);
                ui.close();
            }
            ui.separator();
            if ui.button("Удалить…").clicked() {
                choice.delete = Some(track_id);
                ui.close();
            }
        });
        actions.merge(choice);
    }
}

/// The track table (round 4): `egui_extras::TableBuilder` with striped
/// rows, per-context scroll state, clip + hover-tooltips, a right
/// aligned duration column, multi-row selection (Ф3: plain/ctrl/shift
/// clicks, toolbar actions) and full-row drag. Collects row choices
/// without holding an app-state borrow.
fn track_table(
    ui: &mut egui::Ui,
    tracks: &[&Track],
    selected_tracks: &[TrackId],
    cursor: Option<TrackId>,
    view: LibraryView,
    menus_allowed: bool,
    table_id: egui::Id,
) -> TrackRowActions {
    let mut actions = TrackRowActions::default();
    // Modifiers at click time are the current input state: egui
    // processes clicks within their frame.
    let ctx = ui.ctx().clone();
    track_table_builder(ui, table_id)
        .header(ROW_HEIGHT, |mut header| {
            header.col(|ui| {
                ui.strong("Название");
            });
            header.col(|ui| {
                ui.strong("Исполнитель");
            });
            header.col(|ui| {
                ui.strong("Альбом");
            });
            header.col(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.strong("Длит.");
                });
            });
        })
        .body(|mut body| {
            for (index, track) in tracks.iter().enumerate() {
                let track = *track;
                body.row(ROW_HEIGHT, |mut row| {
                    // Selection highlight straight from TableBuilder:
                    // every row of the multi-selection plus the cursor.
                    row.set_selected(selected_tracks.contains(&track.id) || cursor == Some(track.id));
                    // WHY the same id for every cell: egui keys the drag
                    // payload by id; identical ids + identical payloads
                    // make all four cells drag the one row.
                    let row_id = egui::Id::unique(("track_row", track.id.to_bits()));
                    let payload = DraggedTracks(vec![track.id]);
                    let clicked = Cell::new(false);
                    let double_clicked = Cell::new(false);
                    row.col(|ui| {
                        draggable_cell(ui, row_id, &payload, &clicked, &double_clicked, |ui| {
                            ui.label(egui::RichText::new(&track.title).strong());
                        });
                    });
                    row.col(|ui| {
                        draggable_cell(ui, row_id, &payload, &clicked, &double_clicked, |ui| {
                            tooltip_label(ui, &track.artist);
                        });
                    });
                    row.col(|ui| {
                        draggable_cell(ui, row_id, &payload, &clicked, &double_clicked, |ui| {
                            tooltip_label(ui, &track.album);
                        });
                    });
                    row.col(|ui| {
                        draggable_cell(ui, row_id, &payload, &clicked, &double_clicked, |ui| {
                            // Table numbers read better right-aligned to
                            // the column edge.
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| ui.label(format_duration(track.duration_ms)),
                            );
                        });
                    });
                    // Double-click wins over plain click: the second
                    // click of a double-click also registers as clicked.
                    if double_clicked.get() {
                        actions.edit = Some(track.id);
                        actions.select = Some(track.id);
                    } else if clicked.get() {
                        let (ctrl, shift) =
                            ctx.input(|input| (input.modifiers.ctrl, input.modifiers.shift));
                        actions.row_click = Some((index, ctrl, shift));
                    }
                    if menus_allowed {
                        let mut choice = TrackRowActions::default();
                        // Union of the cell responses: the menu opens on
                        // a right-click anywhere in the row. Menu actions
                        // stay single-row (round 4.1: ПКМ unchanged).
                        row.response().context_menu(|ui| {
                            if ui.button("Изменить…").clicked() {
                                choice.edit = Some(track.id);
                                ui.close();
                            }
                            if ui.button("Добавить в плейлист…").clicked() {
                                choice.add_to_playlist = Some(vec![track.id]);
                                ui.close();
                            }
                            if let LibraryView::Playlist(playlist_id) = view {
                                ui.separator();
                                if ui.button("Убрать из плейлиста").clicked() {
                                    choice.remove_from_playlist =
                                        Some((playlist_id, track.id));
                                    ui.close();
                                }
                            }
                            ui.separator();
                            if ui.button("Удалить…").clicked() {
                                choice.delete = Some(track.id);
                                ui.close();
                            }
                        });
                        actions.merge(choice);
                    }
                });
            }
        });
    actions
}

/// One draggable table cell: drag payload + click/double-click flags.
/// WHY per-cell drag sources: TableBuilder splits the row into cells,
/// so wrapping each cell keeps the whole row draggable.
fn draggable_cell(
    ui: &mut egui::Ui,
    row_id: egui::Id,
    payload: &DraggedTracks,
    clicked: &Cell<bool>,
    double_clicked: &Cell<bool>,
    add: impl FnOnce(&mut egui::Ui),
) {
    let response = ui
        .dnd_drag_source(row_id, payload.clone(), |ui| {
            ui.set_min_height(ROW_HEIGHT);
            add(ui);
        })
        .response;
    if response.clicked() {
        clicked.set(true);
    }
    if response.double_clicked() {
        double_clicked.set(true);
    }
}

/// Artist/album cell: clipped by the column, hovering shows the full text.
fn tooltip_label(ui: &mut egui::Ui, value: &Option<String>) {
    let response = ui.label(optional_text(value));
    if let Some(full) = value.as_deref().map(str::trim).filter(|text| !text.is_empty()) {
        response.on_hover_text(full);
    }
}

/// One sidebar row: name + weak count («Все треки» only on the master),
/// selectable (click = the central table context). Built-in device
/// views accept no drops and offer no ПКМ: the firmware owns them.
fn playlist_row(
    ui: &mut egui::Ui,
    snapshot: &LibrarySnapshot,
    playlist: &Playlist,
    view: &mut LibraryView,
    menus_allowed: bool,
) -> PlaylistPanelActions {
    let mut actions = PlaylistPanelActions::default();
    let is_view = playlist.distinguished_kind != 0;
    let count_text = if playlist.is_master {
        "Все треки".to_owned()
    } else if is_view {
        let kinds = view_media_kinds(playlist.distinguished_kind);
        let count = snapshot
            .tracks
            .iter()
            .filter(|track| kinds.contains(&track.media_kind))
            .count();
        format!("{} {}", count, plural(count, "трек", "трека", "треков"))
    } else {
        let count = playlist.unique_track_ids().len();
        format!("{} {}", count, plural(count, "трек", "трека", "треков"))
    };
    let row_view = if playlist.is_master {
        LibraryView::Master
    } else if is_view {
        LibraryView::Media {
            distinguished: playlist.distinguished_kind,
        }
    } else {
        LibraryView::Playlist(playlist.id)
    };
    let (_, dropped) = ui.dnd_drop_zone::<DraggedTracks, ()>(
        egui::Frame::default().inner_margin(egui::Margin::same(2)),
        |ui| {
            let name = ui.selectable_value(
                view,
                row_view,
                (
                    egui::RichText::new(playlist.name.as_str()),
                    egui::RichText::new(format!("  {count_text}")).weak(),
                ),
            );
            if menus_allowed && !is_view {
                let mut refused_master = false;
                name.context_menu(|ui| {
                    if ui.button("Удалить…").clicked() {
                        if playlist.is_master {
                            refused_master = true;
                        } else {
                            actions.delete_playlist = Some(playlist.id);
                        }
                        ui.close();
                    }
                });
                if refused_master {
                    actions.master_delete = Some(playlist.name.clone());
                }
            }
        },
    );
    if let Some(dragged) = dropped {
        if playlist.is_master {
            actions.master_drop = Some(playlist.name.clone());
        } else if is_view {
            // The firmware owns built-in views: drops are refused
            // up front instead of a failed round trip.
            actions.view_drop = Some(playlist.name.clone());
        } else {
            actions.add_drop = Some((playlist.id, dragged.0.clone()));
        }
    }
    actions
}

/// Field editors of the edit-track dialog; returns true on «Сохранить».
/// Cancel closes via `ui.close()` (surfaces as `should_close`).
fn edit_track_dialog_fields(ui: &mut egui::Ui, dialog: &mut EditDialogState) -> bool {
    let mut save = false;
    // Local copy: egui closures each need their own `&mut` field handle.
    let mut fields = dialog.fields.clone();
    ui.horizontal(|ui| {
        ui.label("Название:      ");
        ui.add(
            egui::TextEdit::singleline(&mut fields.title).desired_width(320.0),
        );
    });
    ui.horizontal(|ui| {
        ui.label("Исполнитель:  ");
        ui.add(
            egui::TextEdit::singleline(&mut fields.artist).desired_width(320.0),
        );
    });
    ui.horizontal(|ui| {
        ui.label("Альбом:       ");
        ui.add(
            egui::TextEdit::singleline(&mut fields.album).desired_width(320.0),
        );
    });
    ui.horizontal(|ui| {
        ui.label("Альбом-артист:");
        ui.add(
            egui::TextEdit::singleline(&mut fields.album_artist)
                .desired_width(320.0),
        );
    });
    ui.horizontal(|ui| {
        ui.label("Трек №:       ");
        ui.add(
            egui::TextEdit::singleline(&mut fields.track_number)
                .desired_width(60.0),
        );
        ui.label("Диск №:");
        ui.add(
            egui::TextEdit::singleline(&mut fields.disc_number)
                .desired_width(60.0),
        );
    });
    dialog.fields = fields;
    if let Some(error) = &dialog.error {
        ui.colored_label(egui::Color32::LIGHT_RED, error);
    }
    ui.separator();
    ui.horizontal(|ui| {
        if ui.button("Отмена").clicked() {
            ui.close();
        }
        if ui.button("Сохранить").clicked() {
            save = true;
            ui.close();
        }
    });
    save
}

/// Headless table render for the Ф4 layout test: same builder config
/// as production, header cells measured via allocated rects.
#[cfg(test)]
fn render_measured_table(ui: &mut egui::Ui) -> Vec<egui::Rect> {
    let rects: [std::rc::Rc<std::cell::RefCell<Option<egui::Rect>>>; 4] =
        Default::default();
    let sinks = rects.clone();
    track_table_builder(ui, egui::Id::unique("test_table"))
        .header(ROW_HEIGHT, move |mut header| {
            for sink in sinks {
                header.col(move |ui| {
                    let (rect, _) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), 16.0),
                        egui::Sense::hover(),
                    );
                    *sink.borrow_mut() = Some(rect);
                });
            }
        })
        .body(|mut body| {
            body.row(ROW_HEIGHT, |mut row| {
                row.col(|ui| {
                    ui.label("Alpha");
                });
                row.col(|ui| {
                    ui.label("Artist");
                });
                row.col(|ui| {
                    ui.label("Album");
                });
                row.col(|ui| {
                    ui.label("2:03");
                });
            });
        });
    rects
        .iter()
        .map(|cell| cell.borrow().expect("header cell must render"))
        .collect()
}

/// Missing tags render as an em dash instead of nothing.
fn optional_text(value: &Option<String>) -> String {
    value
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| "—".to_owned())
}

/// `123456` ms → `2:03`; zero means unknown → em dash.
fn format_duration(duration_ms: u32) -> String {
    if duration_ms == 0 {
        return "—".to_owned();
    }
    let total_seconds = duration_ms / 1000;
    format!("{}:{:02}", total_seconds / 60, total_seconds % 60)
}

/// Gate for opening any context menu: every choice inside fires a
/// library request, and a busy worker must not even be offered more
/// work. Centralised so every menu site follows one rule.
fn context_menu_allowed(app: &PodlibApp) -> bool {
    app.library_idle()
}

/// Full-window overlay while files hover over the viewport.
fn draw_drop_overlay(ui: &egui::Ui, names: &[String]) {
    let ctx = ui.ctx();
    let painter =
        ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::unique("drop_overlay")));
    let rect = ctx.content_rect();
    painter.rect_filled(rect, 12.0, egui::Color32::from_black_alpha(160));
    let font = egui::TextStyle::Heading.resolve(&ctx.global_style());
    let text = if names.len() == 1 {
        format!("Отпустите, чтобы добавить: {}", names[0])
    } else {
        format!("Отпустите, чтобы добавить файлы: {} шт.", names.len())
    };
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        text,
        font,
        egui::Color32::WHITE,
    );
}

/// Worker-thread body: run the sudo mount sequence (`mkdir -p`, then
/// rw-mount) and report the mount point plus whether THIS run actually
/// performed the mount (round 5.3: an already-mounted target is skipped
/// — idempotent connect — and then the mount is not ours, so closing
/// the app must not unmount it). The library is NOT opened here:
/// `Box<dyn LibraryBackend>` is not `Send`, so it is opened on the library
/// worker thread instead (see library.rs).
fn connect_device(
    password: String, // owned: dropped right after the last command (W3)
    steps: Vec<device::CommandSpec>,
    mount_path: PathBuf,
    block_dev: PathBuf,
) -> Result<(PathBuf, bool), ConnectError> {
    // WHY the password lives across the whole loop: every sudo step
    // consumes its own copy from stdin; it is dropped only after the
    // last step, never stored anywhere.
    // Idempotent mount (round 5.3): /proc/mounts decides whether the
    // target is already ours (skip the mount step — and remember the
    // mount is NOT ours, so closing will not unmount it), mounted from
    // another device (honest refusal), or free (mount as usual).
    let mounts = std::fs::read_to_string("/proc/mounts")
        .map(|content| device::parse_proc_mounts(&content))
        .unwrap_or_default();
    match device::decide_mount(&mounts, &block_dev, &mount_path) {
        device::MountDecision::AlreadyMounted => {
            drop(password);
            return Ok((mount_path, false));
        }
        device::MountDecision::ConflictsWith(other) => {
            drop(password);
            return Err(ConnectError::Mount {
                stderr: format!(
                    "{mount_path_display} уже смонтирован с {other} — подключение отменено.",
                    mount_path_display = mount_path.display()
                ),
                error: MountError::AlreadyMounted,
            });
        }
        device::MountDecision::Free => {}
    }
    for spec in &steps {
        let output =
            device::run_sudo_mount(&password, spec).map_err(|error| ConnectError::Mount {
                stderr: String::new(),
                error: MountError::Io(error),
            })?;
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        device::classify_mount_output(&output)
            .map_err(|error| ConnectError::Mount { error, stderr })?;
    }
    drop(password);
    Ok((mount_path, true))
}

/// Runs a worker body, converting a panic into an honest error value.
/// Without this a panicking worker drops the event sender, the UI sees
/// only `Disconnected` and unlocks silently — with no clue what broke.
fn run_worker_step<T, E>(
    body: impl FnOnce() -> Result<T, E> + std::panic::UnwindSafe,
    on_panic: impl FnOnce(String) -> E,
) -> Result<T, E> {
    std::panic::catch_unwind(body)
        .unwrap_or_else(|panic| Err(on_panic(panic_message(panic))))
}

impl eframe::App for PodlibApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_events();
        self.drain_library();
        self.guard_close_request(ui);
        self.collect_dropped_files(ui);
        // A close requested mid-operation ejects the device as soon as
        // the app becomes idle (round 5.3).
        if deferred_eject_due(self.pending_close_after_eject, self.busy, self.library_idle()) {
            self.start_eject = true;
        }
        // A selected view deleted from the device falls back to the
        // master view (checked once per frame, before panels).
        if let Some(library) = self.library.as_mut() {
            let stale = match library.view {
                LibraryView::Master => false,
                LibraryView::Media { distinguished } => !library
                    .snapshot
                    .playlists
                    .iter()
                    .any(|playlist| playlist.distinguished_kind == distinguished),
                LibraryView::Playlist(id) => library.snapshot.playlist(id).is_none(),
            };
            if stale {
                library.view = LibraryView::Master;
            }
        }
        // 1) App bar: name + device state + eject.
        egui::Panel::top("app_bar")
            .default_size(48.0)
            .show(ui, |ui| self.app_bar(ui));
        // 2) Left sidebar: device list while disconnected, playlists
        //    once connected (the old right playlists panel is gone —
        //    it duplicated the central table). 240px (Ф4): leaves the
        //    table enough width for all four columns at ≥900px windows.
        egui::Panel::left("device_panel")
            .default_size(240.0)
            .resizable(true)
            .show(ui, |ui| {
                if self.connected.is_some() {
                    let actions = self.playlists_sidebar(ui);
                    self.apply_playlist_actions(actions);
                } else {
                    self.device_panel(ui);
                }
            });
        // 3) Status bar BEFORE the central panel (round 5.3 Ф1): a
        //    bottom panel declared after the central one shrinks to
        //    nothing — the central panel already claimed the space and
        //    the owner lost all feedback. egui lays panels out in
        //    declaration order; the central panel takes what remains.
        egui::Panel::bottom("status_bar")
            .default_size(34.0)
            .min_size(26.0) // round 5.3 Ф1: never collapses below one row
            .resizable(false)
            .show(ui, |ui| self.status_bar(ui));
        // 4) Central panel: the selected playlist context (master = the
        //    whole library), or a plain hint while disconnected.
        egui::CentralPanel::default().show(ui, |ui| {
            if self.connected.is_none() {
                ui.centered_and_justified(|ui| {
                    ui.label(
                        egui::RichText::new(
                            "Подключите устройство, чтобы работать с библиотекой",
                        )
                        .weak(),
                    );
                });
            } else if self.library.is_none() {
                ui.centered_and_justified(|ui| {
                    ui.colored_label(
                        egui::Color32::LIGHT_RED,
                        "Библиотека недоступна: воркер закрылся. Отключите и подключите \
                         устройство заново.",
                    );
                });
            } else {
                self.library_view(ui);
            }
        });
        // Hover preview above everything while files are dragged over us.
        let hovered: Vec<String> = ui
            .input(|input| {
                input
                    .raw
                    .hovered_files
                    .iter()
                    .filter_map(|file| {
                        file.path
                            .as_ref()
                            .map(|path| path.display().to_string())
                    })
                    .collect()
            });
        if !hovered.is_empty() {
            draw_drop_overlay(ui, &hovered);
        }
        let ctx = ui.ctx().clone();
        self.maybe_mount_modal(&ctx);
        self.maybe_eject_modal(&ctx);
        self.maybe_close_confirm_modal(&ctx);
        self.maybe_tag_review_modal(&ctx);
        self.maybe_delete_track_modal(&ctx);
        self.maybe_delete_playlist_modal(&ctx);
        self.maybe_create_playlist_modal(&ctx);
        self.maybe_edit_track_modal(&ctx);
        self.maybe_add_to_playlist_modal(&ctx);
        if self.start_mount {
            self.start_mount = false;
            self.spawn_connect();
        }
        if self.start_eject {
            self.start_eject = false;
            self.spawn_eject();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use podlib::{build_nano2g_fixture, FixtureTrack};
    use tempfile::TempDir;

    use super::*;

    const POLL: Duration = Duration::from_millis(2);
    const DEADLINE: usize = 2500; // ~5 s

    /// Headless app instance: `handle_event`/`drain_*` only touch plain
    /// state (strings, options, channels), no egui rendering.
    fn test_app() -> PodlibApp {
        PodlibApp::new(egui::Context::default())
    }

    fn sample_track(id: u64, title: &str) -> Track {
        Track {
            id: TrackId::from_bits(id),
            title: title.to_owned(),
            artist: Some("Artist".to_owned()),
            album: None,
            album_artist: None,
            duration_ms: 90_000,
            track_number: 1,
            disc_number: 1,
            size_bytes: 1024,
            has_artwork: false,
            media_kind: 1,
        }
    }

    /// A mount with a one-track classic-device fixture.
    fn fixture_mount() -> TempDir {
        let dir = TempDir::new().unwrap();
        build_nano2g_fixture(
            dir.path(),
            &[FixtureTrack {
                title: "Alpha".to_owned(),
                artist: None,
                album: None,
                genre: None,
                file_name: "a.mp3".to_owned(),
                size_bytes: 2048,
                duration_ms: 120_000,
                track_number: 1,
                disc_number: 1,
            }],
            &[],
        )
        .unwrap();
        dir
    }

    /// Polls `drain_library` until `done` says stop or the deadline passes.
    fn pump(app: &mut PodlibApp, mut done: impl FnMut(&PodlibApp) -> bool) -> bool {
        for _ in 0..DEADLINE {
            app.drain_library();
            if done(app) {
                return true;
            }
            std::thread::sleep(POLL);
        }
        false
    }

    // ---- device round-1 behaviour (kept) ----

    #[test]
    fn panic_message_handles_common_payloads() {
        assert_eq!(panic_message(Box::new("str panic")), "str panic");
        assert_eq!(
            panic_message(Box::new(String::from("boxed panic"))),
            "boxed panic"
        );
        assert_eq!(panic_message(Box::new(7u32)), "причина не сообщена");
    }

    #[test]
    fn worker_panic_maps_to_internal_error() {
        let result: Result<PathBuf, ConnectError> =
            run_worker_step(|| panic!("worker exploded"), ConnectError::Internal);
        match result {
            Err(ConnectError::Internal(message)) => {
                assert!(message.contains("worker exploded"))
            }
            other => panic!("expected Err(Internal), got {other:?}"),
        }
    }

    #[test]
    fn panicked_worker_event_unlocks_ui_and_shows_error() {
        let mut app = test_app();
        app.busy = true;
        let (tx, rx) = channel();
        app.rx = Some(rx);
        let error: Result<(PathBuf, bool), ConnectError> =
            run_worker_step(|| panic!("worker exploded"), ConnectError::Internal);
        tx.send(DeviceEvent::Connected(error)).unwrap();
        drop(tx); // the panicking worker is gone: the channel closes
        app.drain_events();
        // The UI must be unlocked honestly, with the panic surfaced.
        assert!(!app.busy);
        let error = app.error.expect("panic must be shown as an error");
        assert!(error.message.contains("worker exploded"));
    }

    #[test]
    fn worker_without_panic_passes_result_through() {
        let result: Result<(PathBuf, bool), ConnectError> = run_worker_step(
            || Ok((PathBuf::from("/run/media/ipod"), true)),
            |_| unreachable!(),
        );
        assert_eq!(
            result.unwrap(),
            (PathBuf::from("/run/media/ipod"), true)
        );
    }

    // ---- library lifecycle through the real worker ----

    #[test]
    fn connected_event_opens_library_and_loads_snapshot() {
        let dir = fixture_mount();
        let mut app = test_app();
        app.handle_event(DeviceEvent::Connected(Ok((dir.path().to_owned(), true))));
        assert!(app.connected.is_some());
        assert!(app.library.is_some());
        let loaded = pump(&mut app, |app| {
            app.library
                .as_ref()
                .is_some_and(|library| library.opening.is_none())
        });
        assert!(loaded, "worker must deliver the initial listing");
        let library = app.library.as_ref().unwrap();
        assert_eq!(library.snapshot.tracks.len(), 1);
        assert_eq!(library.snapshot.tracks[0].title, "Alpha");
        assert!(app.status.contains("Подключено"));
        // P1: Russian plural forms — «1 трек», not «1 треков».
        assert!(
            app.status.contains("1 трек, 1 плейлист"),
            "got: {}",
            app.status
        );
        let library = app.library.take().unwrap();
        library.worker.close_and_join();
    }

    #[test]
    fn open_failure_reports_mounted_volume_and_drops_library() {
        let dir = TempDir::new().unwrap(); // no library inside
        let mut app = test_app();
        app.handle_event(DeviceEvent::Connected(Ok((dir.path().to_owned(), true))));
        let dropped = pump(&mut app, |app| app.library.is_none());
        assert!(dropped, "failed open must drop the library UI");
        let error = app.error.expect("open failure must be shown");
        // The volume IS mounted at this point — the message says so.
        assert!(error.message.contains("смонтирован"));
        assert!(app.connected.is_some(), "device stays connected");
    }

    // ---- pending-op state machine (manual channels, no waiting) ----

    fn app_with_open_library() -> (PodlibApp, TempDir) {
        let dir = fixture_mount();
        let mut app = test_app();
        app.handle_event(DeviceEvent::Connected(Ok((dir.path().to_owned(), true))));
        let settled = pump(&mut app, |app| {
            app.library
                .as_ref()
                .is_some_and(|library| library.opening.is_none())
        });
        assert!(settled, "fixture library must open in tests");
        (app, dir)
    }

    #[test]
    fn applied_outcome_updates_snapshot_and_status() {
        let (mut app, _dir) = app_with_open_library();
        let (tx, rx) = channel();
        app.library
            .as_mut()
            .unwrap()
            .pending = Some((PendingKind::AddTracks, rx));
        tx.send(LibraryOutcome::Applied {
            snapshot: LibrarySnapshot {
                tracks: vec![sample_track(1, "A"), sample_track(2, "B")],
                playlists: Vec::new(),
            },
            summary: "Добавлено 2 трека".to_owned(),
        })
        .unwrap();
        app.drain_library();
        let library = app.library.as_ref().unwrap();
        assert_eq!(library.snapshot.tracks.len(), 2, "snapshot replaced");
        assert!(library.pending.is_none(), "op must be settled");
        assert_eq!(app.status, "Добавлено 2 трека");
        assert!(app.error.is_none());
    }

    #[test]
    fn edit_outcome_reports_id_change_via_summary() {
        let (mut app, _dir) = app_with_open_library();
        let (tx, rx) = channel();
        app.library
            .as_mut()
            .unwrap()
            .pending = Some((PendingKind::EditTrack, rx));
        let mut track = sample_track(9, "Beta");
        track.duration_ms = 0; // renders as an em dash in the table
        tx.send(LibraryOutcome::Applied {
            snapshot: LibrarySnapshot {
                tracks: vec![track],
                playlists: Vec::new(),
            },
            summary: "Обновлено: «Beta» (новый id: 9)".to_owned(),
        })
        .unwrap();
        app.drain_library();
        let library = app.library.as_ref().unwrap();
        assert_eq!(library.snapshot.tracks[0].title, "Beta");
        assert_eq!(app.status, "Обновлено: «Beta» (новый id: 9)");
    }

    #[test]
    fn failed_outcome_shows_error_but_keeps_library() {
        let (mut app, _dir) = app_with_open_library();
        let (tx, rx) = channel();
        app.library
            .as_mut()
            .unwrap()
            .pending = Some((PendingKind::DeletePlaylist, rx));
        tx.send(LibraryOutcome::Failed {
            message: "Плейлист 16 — встроенный, его нельзя изменять".to_owned(),
            hint: Some("Мастер-плейлист доступен только для чтения".to_owned()),
        })
        .unwrap();
        app.drain_library();
        assert!(app.library.is_some(), "backend failure must not kill the UI");
        assert!(app.library.as_ref().unwrap().pending.is_none());
        let error = app.error.expect("backend failure must be shown");
        assert!(error.message.contains("встроенный"));
        assert!(error.hint.is_some());
    }

    #[test]
    fn disconnected_worker_drops_library_and_explains() {
        let (mut app, _dir) = app_with_open_library();
        let (tx, rx) = channel();
        app.library
            .as_mut()
            .unwrap()
            .pending = Some((PendingKind::Refresh, rx));
        drop(tx); // worker died mid-operation
        app.drain_library();
        assert!(app.library.is_none());
        let error = app.error.expect("dead worker must be reported");
        assert!(error.message.contains("завершился"));
    }

    #[test]
    fn closed_outcome_drops_library() {
        let (mut app, _dir) = app_with_open_library();
        let (tx, rx) = channel();
        app.library
            .as_mut()
            .unwrap()
            .pending = Some((PendingKind::Refresh, rx));
        tx.send(LibraryOutcome::Closed).unwrap();
        app.drain_library();
        assert!(app.library.is_none());
    }

    #[test]
    fn ejected_error_keeps_connected_and_hints_reconnect() {
        let (mut app, _dir) = app_with_open_library();
        // spawn_eject takes the library before starting the sequence;
        // simulate that here.
        app.library = None;
        let (tx, rx) = channel();
        tx.send(DeviceEvent::Ejected(Err("unmount failed".to_owned()))).unwrap();
        app.rx = Some(rx);
        app.drain_events();
        assert!(app.connected.is_some(), "failed eject keeps the device");
        let error = app.error.expect("eject failure must be shown");
        assert!(error.hint.is_some());
        assert!(error.hint.unwrap().contains("заново"));
    }

    // ---- busy-worker refusal and menu gate (F2 / P2) ----

    #[test]
    fn request_without_library_asks_to_connect_first() {
        // P2: with no library at all there is no operation to wait for —
        // the refusal must point at connecting a device instead.
        let mut app = test_app();
        assert!(app.library.is_none());
        app.library_request(
            LibraryCommand::Refresh,
            PendingKind::Refresh,
            "Обновляю список…",
        );
        let error = app
            .error
            .expect("request without a library must be refused");
        assert!(
            error.message.contains("подключите"),
            "got: {}",
            error.message
        );
        assert!(
            !error.message.contains("Дождитесь"),
            "there is no operation to wait for"
        );
    }

    #[test]
    fn picker_close_captures_ids_before_clearing() {
        // Regression (release review, round 3): clearing the picker before
        // reading ids sent an empty AddToPlaylist with a lying
        // «Добавлено 0 треков» status for a real user action.
        let ids = vec![TrackId::from_bits(7), TrackId::from_bits(9)];
        let (state, request) =
            picker_close(Some(PlaylistId::from_bits(42)), false, Some(ids.clone()));
        assert_eq!(state, None, "chosen playlist must close the picker");
        assert_eq!(request, Some((PlaylistId::from_bits(42), ids)));
    }

    #[test]
    fn picker_close_without_choice_clears_and_sends_nothing() {
        let (state, request) = picker_close(None, true, Some(vec![TrackId::from_bits(1)]));
        assert_eq!(state, None);
        assert_eq!(request, None);

        // Neither chosen nor closed: picker state survives for the next frame.
        let (state, request) = picker_close(None, false, Some(vec![TrackId::from_bits(1)]));
        assert_eq!(state, Some(vec![TrackId::from_bits(1)]));
        assert_eq!(request, None);
    }

    #[test]
    fn busy_worker_rejects_new_request_visibly() {
        let (mut app, _dir) = app_with_open_library();
        let (_tx, rx) = channel();
        app.library.as_mut().unwrap().pending = Some((PendingKind::Refresh, rx));
        // A request while the worker is busy must be refused visibly,
        // not dropped silently: the user's action deserves an answer.
        app.library_request(
            LibraryCommand::CreatePlaylist {
                name: "Mix".to_owned(),
            },
            PendingKind::CreatePlaylist,
            "Создаю плейлист…",
        );
        let error = app
            .error
            .expect("busy refusal must be visible to the user");
        assert!(
            error.message.contains("Дождитесь завершения"),
            "got: {}",
            error.message
        );
        // The in-flight operation is untouched.
        let library = app.library.as_ref().unwrap();
        assert!(matches!(library.pending, Some((PendingKind::Refresh, _))));
        assert!(
            !app.status.contains("Создаю плейлист"),
            "status must not announce the rejected request"
        );
    }

    #[test]
    fn context_menu_gate_tracks_library_state() {
        let (mut app, _dir) = app_with_open_library();
        assert!(context_menu_allowed(&app), "idle app opens context menus");
        let (_tx, rx) = channel();
        app.library.as_mut().unwrap().pending = Some((PendingKind::Refresh, rx));
        assert!(
            !context_menu_allowed(&app),
            "pending operation must gate context menus"
        );
        // The initial open counts as busy too.
        let (_tx2, rx2) = channel();
        let library = app.library.as_mut().unwrap();
        library.pending = None;
        library.opening = Some(rx2);
        assert!(
            !context_menu_allowed(&app),
            "library opening must gate context menus too"
        );
    }

    // ---- close guard state machine (F4, headless) ----

    #[test]
    fn close_while_idle_closes_without_confirm() {
        assert_eq!(
            close_decision(true, false, false),
            CloseDecision::Allow,
            "idle window closes as usual"
        );
    }

    #[test]
    fn close_while_operation_active_blocks_and_asks() {
        assert_eq!(
            close_decision(true, true, false),
            CloseDecision::BlockWithConfirm,
            "mid-operation close must not kill the worker thread"
        );
    }

    #[test]
    fn confirmed_close_passes_through_even_while_busy() {
        assert_eq!(
            close_decision(true, true, true),
            CloseDecision::Allow,
            "the user already chose to close: no confirm loop"
        );
    }

    #[test]
    fn no_close_request_is_a_noop() {
        assert_eq!(close_decision(false, true, false), CloseDecision::Allow);
        assert_eq!(close_decision(false, false, true), CloseDecision::Allow);
    }

    // ---- close = safe unmount for app-created mounts (round 5.3 Ф3) ----

    #[test]
    fn confirmed_close_ejects_first_only_for_app_created_mounts() {
        assert_eq!(
            close_confirmation_action(true),
            CloseAction::EjectFirst,
            "the app created the mount: unmount before closing"
        );
        assert_eq!(
            close_confirmation_action(false),
            CloseAction::CloseNow,
            "an externally mounted volume is not our responsibility"
        );
    }

    #[test]
    fn deferred_eject_fires_only_when_idle() {
        assert!(deferred_eject_due(true, false, true), "idle → eject now");
        assert!(!deferred_eject_due(true, true, false), "busy → wait");
        assert!(!deferred_eject_due(false, false, true), "no pending close");
    }

    // ---- status bar visibility (round 5.3 Ф1) ----

    /// Регрессия живого прогона 5.2: bottom-панель, объявленная ПОСЛЕ
    /// CentralPanel, схлопывалась до нуля (центральная забирала всё
    /// оставшееся место) и владелец терял все статусы и ошибки. Новый
    /// порядок (bottom до central) обязан держать высоту ≥ 26px.
    #[test]
    fn status_bar_panel_stays_visible_when_declared_before_central() {
        // The regression of live run 5.2: a bottom panel declared AFTER
        // the central one collapsed to nothing (the central panel had
        // already claimed the remaining space) and the owner lost every
        // status and error. The new order (bottom before central) must
        // keep the full panel height — animations disabled so a single
        // frame measures the settled layout.
        egui::__run_test_ui(|ui| {
            ui.style_mut().animation_time = 0.0;
            egui::Panel::top("r53_top").default_size(40.0).show(ui, |ui| {
                ui.label("podlib");
            });
            egui::Panel::left("r53_left").default_size(120.0).show(ui, |ui| {
                ui.label("sidebar");
            });
            let bottom = egui::Panel::bottom("r53_bottom")
                .default_size(34.0)
                .min_size(26.0)
                .resizable(false)
                .show(ui, |ui| {
                    ui.label("Готово");
                });
            assert!(
                bottom.response.rect.height() >= 26.0,
                "status bar collapsed: {} px",
                bottom.response.rect.height()
            );
            egui::CentralPanel::default().show(ui, |ui| {
                ui.label("central");
            });
        });
    }

    // ---- pure UI helpers ----

    #[test]
    fn duration_formats_minutes_seconds_and_unknown() {
        assert_eq!(format_duration(0), "—");
        assert_eq!(format_duration(1_000), "0:01");
        assert_eq!(format_duration(123_456), "2:03");
        assert_eq!(format_duration(3_600_000), "60:00");
    }

    #[test]
    fn optional_text_renders_em_dash_for_missing_tags() {
        assert_eq!(optional_text(&Some("X".to_owned())), "X");
        assert_eq!(optional_text(&Some("   ".to_owned())), "—");
        assert_eq!(optional_text(&None), "—");
    }

    // ---- batch-add status (P4) ----

    #[test]
    fn batch_add_status_announces_the_wait_and_skips() {
        assert_eq!(
            batch_add_status(1, 0, &[]),
            "Добавляется 1 файл, это может занять время…"
        );
        assert_eq!(
            batch_add_status(2, 0, &[]),
            "Добавляется 2 файла, это может занять время…"
        );
        assert_eq!(
            batch_add_status(30, 0, &[]),
            "Добавляется 30 файлов, это может занять время…"
        );
        assert_eq!(
            batch_add_status(3, 0, &["notes.txt".to_owned(), "noext".to_owned()]),
            "Добавляется 3 файла, это может занять время…; \
             пропущены: notes.txt, noext"
        );
        assert_eq!(
            batch_add_status(5, 2, &[]),
            "Добавляется 5 файлов (2 с транскодом), это может занять время…"
        );
    }

    // ---- search filter (round 4 UX) ----

    fn searchable_track(title: &str, artist: Option<&str>, album: Option<&str>) -> Track {
        Track {
            id: TrackId::from_bits(1),
            title: title.to_owned(),
            artist: artist.map(str::to_owned),
            album: album.map(str::to_owned),
            album_artist: None,
            duration_ms: 90_000,
            track_number: 1,
            disc_number: 1,
            size_bytes: 1024,
            has_artwork: false,
            media_kind: 1,
        }
    }

    #[test]
    fn empty_or_blank_query_matches_everything() {
        let track = searchable_track("Anything", Some("Whoever"), Some("Whatever"));
        assert!(matches_query(&track, ""));
        assert!(matches_query(&track, "   "));
    }

    #[test]
    fn query_matches_title_case_insensitively() {
        let track = searchable_track("Moonlight Sonata", None, None);
        assert!(matches_query(&track, "moon"));
        assert!(matches_query(&track, "SONATA"));
        assert!(matches_query(&track, "light son"));
        // Latin vs Cyrillic differs — no accidental matches.
        assert!(!matches_query(&track, "лунная"));
    }

    #[test]
    fn query_matches_cyrillic_case_insensitively() {
        let track = searchable_track("Лунная соната", None, None);
        assert!(matches_query(&track, "лунная"));
        assert!(matches_query(&track, "Соната"));
        assert!(!matches_query(&track, "moon"));
    }

    #[test]
    fn query_matches_artist_and_album_too() {
        let track = searchable_track("Title", Some("Beethoven"), Some("Classics"));
        assert!(matches_query(&track, "beeth"));
        assert!(matches_query(&track, "class"));
        assert!(!matches_query(&track, "mozart"));
    }

    #[test]
    fn missing_tags_are_not_searched_and_do_not_panic() {
        let track = searchable_track("Title", None, None);
        // «—» is a render artifact of empty tags, never part of the data:
        // it must not match a track whose tags are missing.
        assert!(!matches_query(&track, "—"));
        assert!(!matches_query(&track, "beeth"));
    }

    // ---- sidebar ordering: master first (round 4 UX) ----

    fn playlist_of(id: u64, name: &str, is_master: bool) -> Playlist {
        Playlist {
            id: PlaylistId::from_bits(id),
            name: name.to_owned(),
            track_ids: Vec::new(),
            is_master,
            is_smart: false,
            distinguished_kind: 0,
        }
    }

    #[test]
    fn master_first_keeps_master_first_and_rest_in_order() {
        let playlists = vec![
            playlist_of(1, "Медиатека", true),
            playlist_of(2, "Джаз", false),
            playlist_of(3, "Рок", false),
        ];
        let ordered: Vec<String> = master_first(&playlists)
            .into_iter()
            .map(|playlist| playlist.name.clone())
            .collect();
        assert_eq!(ordered, ["Медиатека", "Джаз", "Рок"]);
    }

    #[test]
    fn master_first_moves_master_from_the_middle_to_front() {
        let playlists = vec![
            playlist_of(2, "Джаз", false),
            playlist_of(1, "Медиатека", true),
            playlist_of(3, "Рок", false),
        ];
        let ordered: Vec<String> = master_first(&playlists)
            .into_iter()
            .map(|playlist| playlist.name.clone())
            .collect();
        assert_eq!(ordered, ["Медиатека", "Джаз", "Рок"]);
    }

    #[test]
    fn master_first_handles_empty_and_masterless_lists() {
        let empty: Vec<Playlist> = Vec::new();
        assert!(master_first(&empty).is_empty());
        let playlists = vec![playlist_of(2, "Джаз", false), playlist_of(3, "Рок", false)];
        let ordered: Vec<String> = master_first(&playlists)
            .into_iter()
            .map(|playlist| playlist.name.clone())
            .collect();
        assert_eq!(ordered, ["Джаз", "Рок"]);
    }

    // ---- keyboard navigation over the visible rows (round 4 UX) ----

    fn ids(bits: &[u64]) -> Vec<TrackId> {
        bits.iter().map(|&b| TrackId::from_bits(b)).collect()
    }

    #[test]
    fn navigation_on_empty_list_selects_nothing() {
        let visible: Vec<TrackId> = Vec::new();
        assert_eq!(next_selection(&visible, None, 1), None);
        assert_eq!(next_selection(&visible, None, -1), None);
    }

    #[test]
    fn navigation_without_selection_starts_at_first_row() {
        let visible = ids(&[10, 20, 30]);
        assert_eq!(next_selection(&visible, None, 1), Some(TrackId::from_bits(10)));
        assert_eq!(next_selection(&visible, None, -1), Some(TrackId::from_bits(10)));
    }

    #[test]
    fn navigation_moves_to_the_neighbour_and_clamps_at_edges() {
        let visible = ids(&[10, 20, 30]);
        let first = TrackId::from_bits(10);
        let middle = TrackId::from_bits(20);
        let last = TrackId::from_bits(30);
        assert_eq!(next_selection(&visible, Some(first), 1), Some(middle));
        assert_eq!(next_selection(&visible, Some(middle), -1), Some(first));
        assert_eq!(next_selection(&visible, Some(first), -1), Some(first), "up from top clamps");
        assert_eq!(next_selection(&visible, Some(last), 1), Some(last), "down from bottom clamps");
    }

    #[test]
    fn navigation_of_stale_selection_starts_over() {
        // e.g. the selected row was filtered out or removed: arrows
        // restart from the top instead of jumping anywhere.
        let visible = ids(&[10, 20, 30]);
        let stale = TrackId::from_bits(99);
        assert_eq!(next_selection(&visible, Some(stale), 1), Some(TrackId::from_bits(10)));
        assert_eq!(next_selection(&visible, Some(stale), -1), Some(TrackId::from_bits(10)));
    }

    #[test]
    fn activation_requires_a_visible_selection() {
        let visible = ids(&[10, 20]);
        assert_eq!(activated(&visible, Some(TrackId::from_bits(20))), Some(TrackId::from_bits(20)));
        assert_eq!(activated(&visible, None), None);
        assert_eq!(activated(&visible, Some(TrackId::from_bits(99))), None);
        let empty: Vec<TrackId> = Vec::new();
        assert_eq!(activated(&empty, Some(TrackId::from_bits(10))), None);
    }

    // ---- round 5.5: tag review modal ----

    fn tag_draft(
        stem: &str,
        artist: Option<&str>,
        album: Option<&str>,
        bits: u32,
    ) -> TrackDraft {
        let mut draft =
            TrackDraft::new(PathBuf::from(format!("/drop/{stem}.mp3")), stem).unwrap();
        draft.artist = artist.map(str::to_owned);
        draft.album = album.map(str::to_owned);
        draft.media_kind_bits = bits;
        draft
    }

    #[test]
    fn is_untagged_flags_only_artist_and_album_absence() {
        assert!(is_untagged(&tag_draft("a", None, None, 1)));
        assert!(is_untagged(&tag_draft("a", Some("  "), Some(""), 1)));
        assert!(!is_untagged(&tag_draft("a", Some("Artist"), None, 1)));
        assert!(!is_untagged(&tag_draft("a", None, Some("Album"), 1)));
    }

    #[test]
    fn common_prefix_trims_to_a_boundary_and_needs_3_chars() {
        // The live case: directory «Skazka o ribake i ribki» with files
        // «… - 1..5» → the whole repeating prefix is stripped.
        let stems: Vec<String> = (1..=5)
            .map(|n| format!("Skazka o ribake i ribki - {n}"))
            .collect();
        assert_eq!(
            common_prefix(&stems).as_deref(),
            Some("Skazka o ribake i ribki")
        );
        // No shared prefix at all.
        assert_eq!(common_prefix(&["alpha".to_owned(), "beta".to_owned()]), None);
        // Shared but too short.
        assert_eq!(common_prefix(&["ab1".to_owned(), "ab2".to_owned()]), None);
        // A single stem never shows the checkbox.
        assert_eq!(common_prefix(&["Skazka o ribake i ribki - 1".to_owned()]), None);
    }

    #[test]
    fn trailing_number_reads_the_stem_tail() {
        assert_eq!(
            trailing_number("Skazka o ribake i ribki - 1"),
            Some(1)
        );
        assert_eq!(trailing_number("Chapter 12"), Some(12));
        assert_eq!(trailing_number("12"), Some(12));
        assert_eq!(trailing_number("track-07"), Some(7));
        assert_eq!(trailing_number("12a"), None);
        assert_eq!(trailing_number("intro"), None);
    }

    #[test]
    fn apply_tag_review_fills_fields_validates_and_falls_back_to_stem() {
        let drafts = vec![
            tag_draft("Skazka - 1", None, None, 8),
            tag_draft("Skazka - 2", None, None, 8),
        ];
        let rows = vec![
            TagRow { title: "Глава 1".to_owned(), track_number: 1 },
            TagRow { title: String::new(), track_number: 2 }, // empty → stem
        ];
        // Empty book title is rejected with the near-field text.
        assert_eq!(
            apply_tag_review(&drafts, "   ", "Пушкин", &rows).unwrap_err(),
            "Укажите название книги"
        );
        let applied = apply_tag_review(&drafts, "Сказка о рыбаке", "Пушкин", &rows).unwrap();
        assert_eq!(applied.len(), 2);
        for draft in &applied {
            assert_eq!(draft.album.as_deref(), Some("Сказка о рыбаке"));
            assert_eq!(draft.album_artist.as_deref(), Some("Пушкин"));
        }
        assert_eq!(applied[0].title, "Глава 1");
        assert_eq!(applied[0].track_number, 1);
        assert_eq!(applied[1].title, "Skazka - 2", "empty edit falls back to stem");
        assert_eq!(applied[1].track_number, 2);
        // An empty author must NOT overwrite the draft's artist field.
        let no_author = apply_tag_review(&drafts, "Книга", "", &rows).unwrap();
        assert_eq!(no_author[0].album_artist, None);
    }

    #[test]
    fn open_tag_review_prefills_rows_prefix_and_book_title() {
        let drafts = vec![
            tag_draft("Skazka o ribake i ribki - 1", None, None, 8),
            tag_draft("Skazka o ribake i ribki - 2", None, None, 8),
            tag_draft("Skazka o ribake i ribki - 3", None, None, 8),
        ];
        let review = open_tag_review(drafts);
        assert_eq!(review.kind_bits, 8);
        assert_eq!(review.rows[0].title, "Skazka o ribake i ribki - 1");
        assert_eq!(review.rows[0].track_number, 1, "trailing digits win");
        assert_eq!(review.rows[2].track_number, 3);
        assert_eq!(
            review.prefix.as_deref(),
            Some("Skazka o ribake i ribki")
        );
        assert!(review.strip_prefix, "found prefix → checkbox pre-enabled");
        assert_eq!(review.book_title, "drop", "common parent dir name prefills");
    }

    #[derive(Debug)]
    struct TestDroppedFile(PathBuf);

    impl egui::DroppedFile for TestDroppedFile {
        fn path(&self) -> &std::path::Path {
            &self.0
        }

        fn bytes(&self) -> Result<Vec<u8>, String> {
            std::fs::read(&self.0).map_err(|error| error.to_string())
        }
    }

    /// Round 5.5 end-to-end (headless): an untagged directory drop
    /// opens the review modal INSTEAD of dispatching; after editing the
    /// common fields and one row, «Добавить» dispatches the edited
    /// drafts through the regular AddTracks pipeline.
    #[test]
    fn untagged_drop_opens_review_modal_then_dispatches_edited_drafts() {
        let (mut app, _dir) = app_with_open_library();
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("book")).unwrap();
        let first = dir.path().join("book/01.mp3");
        let second = dir.path().join("book/02.mp3");
        std::fs::write(&first, b"x").unwrap();
        std::fs::write(&second, b"x").unwrap();

        // Frame 1: the directory drop.
        egui::__run_test_ui(|ui| {
            ui.input_mut(|input| {
                input.raw.dropped_files = vec![
                    std::sync::Arc::new(TestDroppedFile(first.clone()))
                        as std::sync::Arc<dyn egui::DroppedFile + Send + Sync>,
                    std::sync::Arc::new(TestDroppedFile(second.clone()))
                        as std::sync::Arc<dyn egui::DroppedFile + Send + Sync>,
                ];
            });
            app.collect_dropped_files(ui);
        });
        let review = app
            .tag_review
            .as_ref()
            .expect("an untagged drop must open the review modal");
        assert_eq!(review.rows.len(), 2);
        assert_eq!(review.book_title, "book", "the directory name prefills");
        assert!(
            app.library
                .as_ref()
                .unwrap()
                .pending
                .is_none(),
            "nothing is dispatched until the review is applied"
        );

        // Frame 2: the user edits fields (modal body)…
        let review = app.tag_review.as_mut().unwrap();
        review.book_title = "Сказка о рыбаке".to_owned();
        review.author = "Пушкин".to_owned();
        review.rows[1].title = "Глава 2".to_owned();
        review.rows[1].track_number = 2;
        // …and presses «Добавить книгу»: the button marks the frame
        // committed AND closes the modal, so the frame ends with
        // committed=true AND should_close=true (round 5.7 bug 1 — the
        // old cancel-first order swallowed exactly this combination).
        egui::__run_test_ui(|ui| {
            app.maybe_tag_review_modal(&ui.ctx().clone());
        });
        app.finish_tag_review_frame(true, true, false);

        let pending = app
            .library
            .as_ref()
            .unwrap()
            .pending
            .as_ref()
            .expect("AddTracks must be dispatched after the review");
        assert!(matches!(pending.0, PendingKind::AddTracks));
        let settled = pump(&mut app, |app| {
            app.library
                .as_ref()
                .is_some_and(|library| library.pending.is_none())
        });
        assert!(settled, "the batch must complete");
        let library = app.library.as_ref().unwrap();
        assert_eq!(library.snapshot.tracks.len(), 3, "fixture Alpha + 2 files");
        let chapter_two = library
            .snapshot
            .tracks
            .iter()
            .find(|track| track.title == "Глава 2")
            .expect("the edited title must land");
        assert_eq!(chapter_two.album.as_deref(), Some("Сказка о рыбаке"));
        // NOTE: the classic fixture parser never returns album_artist
        // (project.rs keeps it None), so it is asserted in
        // apply_tag_review_fills_fields… on the drafts themselves; on
        // the 7G device libopod stores and returns it.
        assert_eq!(chapter_two.track_number, 2);
        assert!(app.tag_review.is_none(), "the modal closes after applying");
        let library = app.library.take().unwrap();
        library.worker.close_and_join();
    }

    #[test]
    fn finish_frame_commit_outranks_same_frame_close() {
        // Round 5.7 bug 1: a real «Добавить книгу» click closes the
        // modal in the same frame (should_close=true) — the commit must
        // still win. Without a library the dispatched request answers
        // with the honest «подключите устройство» error, which proves
        // the request actually REACHED the dispatch path (the old
        // cancel-first order swallowed it silently). The refusal keeps
        // the modal with the typed tags: user input is not lost to a
        // busy/no-device worker (minor-5).
        let mut app = test_app();
        let draft = tag_draft("Skazka - 1", None, None, 8);
        let mut review = open_tag_review(vec![draft]);
        review.book_title = "Книга".to_owned(); // valid title → commit dispatches
        app.tag_review = Some(review);
        app.finish_tag_review_frame(true, true, false);
        let error = app
            .error
            .as_ref()
            .expect("committed request must reach the dispatch path");
        assert!(error.message.contains("подключите"), "got: {}", error.message);
        assert!(app.tag_review.is_some(), "refusal keeps the typed modal open");
    }

    #[test]
    fn finish_frame_cancel_drops_the_draft() {
        let mut app = test_app();
        let draft = tag_draft("Skazka - 1", None, None, 8);
        app.tag_review = Some(open_tag_review(vec![draft]));
        app.finish_tag_review_frame(false, true, false);
        assert!(app.tag_review.is_none(), "cancel drops the draft");
        assert!(app.error.is_none(), "cancel dispatches nothing");
    }

    #[test]
    fn focused_title_edit_selects_its_row() {
        // Round 5.7 bug 2 regression: the row title is a plain TextEdit
        // now (no drag wrapper swallowing clicks); whatever way the
        // user focuses it — mouse click (standard egui behavior, no
        // interceptor anymore) or Tab — the FOCUSED edit selects its
        // row. Focus is simulated programmatically here (headless).
        let draft = tag_draft("Skazka - 1", None, None, 8);
        let mut app = test_app();
        app.tag_review = Some(open_tag_review(vec![
            draft,
            tag_draft("Skazka - 2", None, None, 8),
        ]));
        let ctx = egui::Context::default();
        // Frame 1: render the modal — row edit ids become known.
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 800.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| {
            ui.style_mut().animation_time = 0.0;
            app.maybe_tag_review_modal(&ui.ctx().clone());
        });
        output.textures_delta.clear();
        let title_id = app.tag_review.as_ref().unwrap().row_edit_ids[1];
        assert_ne!(title_id, egui::Id::NULL, "row edit id must be recorded");
        assert_ne!(
            ctx.memory(|memory| memory.focused()),
            Some(title_id),
            "no focus before focusing"
        );

        // Frame 2: focus row 1's editor (as a mouse click / Tab would).
        ctx.memory_mut(|memory| memory.request_focus(title_id));
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 800.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| {
            ui.style_mut().animation_time = 0.0;
            app.maybe_tag_review_modal(&ui.ctx().clone());
        });
        output.textures_delta.clear();
        assert_eq!(
            ctx.memory(|memory| memory.focused()),
            Some(title_id),
            "the title TextEdit keeps focus"
        );
        assert_eq!(
            app.tag_review.as_ref().unwrap().selected_row,
            Some(1),
            "the focused row is the selected row"
        );
    }

    #[test]
    fn commit_without_book_title_keeps_modal_and_sets_validation() {
        let (mut app, _dir) = app_with_open_library();
        let draft = tag_draft("Skazka - 1", None, None, 8);
        app.tag_review = Some(open_tag_review(vec![draft]));
        app.commit_tag_review();
        let review = app.tag_review.as_ref().expect("modal stays open");
        assert!(review.validation_attempted, "near-field error is shown");
        assert!(
            app.library.as_ref().unwrap().pending.is_none(),
            "nothing dispatched on an empty book title"
        );
        app.library.take().unwrap().worker.close_and_join();
    }

    // ---- audiobook grouping (round 5) ----

    /// Book-file constructor: album/author/disk/track/duration per field.
    #[allow(clippy::too_many_arguments)]
    fn book_track(
        id: u64,
        title: &str,
        album: Option<&str>,
        album_artist: Option<&str>,
        artist: Option<&str>,
        disc: u32,
        number: u32,
        duration_ms: u32,
    ) -> Track {
        Track {
            id: TrackId::from_bits(id),
            title: title.to_owned(),
            artist: artist.map(str::to_owned),
            album: album.map(str::to_owned),
            album_artist: album_artist.map(str::to_owned),
            duration_ms,
            track_number: number,
            disc_number: disc,
            size_bytes: 1024,
            has_artwork: false,
            media_kind: 8,
        }
    }

    #[test]
    fn books_group_by_album_with_author_fallbacks() {
        let tracks = vec![
            book_track(1, "Глава 1", Some("Сказки"), Some("Народ"), Some("Иной"), 1, 1, 60_000),
            book_track(2, "Глава 2", Some("Сказки"), Some("  "), Some("Иной"), 1, 2, 60_000),
            book_track(3, "Песня", Some("   "), None, Some("Певец"), 1, 1, 30_000),
        ];
        let groups = group_books(&tracks);
        assert_eq!(groups.len(), 2);
        // Album artist wins over the track artist; blank cells are skipped.
        assert_eq!(groups[0].album, "Сказки");
        assert_eq!(groups[0].author, "Народ");
        assert_eq!(
            groups[0].track_ids,
            [TrackId::from_bits(1), TrackId::from_bits(2)]
        );
        // A blank album is its own group.
        assert_eq!(groups[1].album, "Без альбома");
        assert_eq!(groups[1].author, "Певец");
    }

    #[test]
    fn books_and_files_sort_deterministically() {
        let tracks = vec![
            book_track(1, "глава", Some("Б"), None, None, 1, 3, 60_000),
            book_track(2, "пролог", Some("а"), None, None, 0, 5, 60_000),
            book_track(3, "эпилог", Some("Б"), None, None, 2, 1, 60_000),
            // No disc → after all discs; no number → after numbered ones.
            book_track(4, "без номера", Some("а"), None, None, 0, 0, 60_000),
            // Equal disc+number: title decides, case-insensitively.
            book_track(5, "Бета", Some("Б"), None, None, 1, 1, 60_000),
            book_track(6, "альфа", Some("Б"), None, None, 1, 1, 60_000),
        ];
        let groups = group_books(&tracks);
        let albums: Vec<&str> = groups.iter().map(|g| g.album.as_str()).collect();
        assert_eq!(albums, ["а", "Б"], "groups sort case-insensitively");
        // Group «а»: no-disc file after… actually both files are
        // disc-less: numbered (#5) before unnumbered.
        assert_eq!(groups[0].track_ids, [TrackId::from_bits(2), TrackId::from_bits(4)]);
        // Group «Б»: disc 1 before disc 2; equal disc+number → title
        // case-insensitively («альфа» < «Бета» < «глава»).
        assert_eq!(
            groups[1].track_ids,
            [
                TrackId::from_bits(6),
                TrackId::from_bits(5),
                TrackId::from_bits(1),
                TrackId::from_bits(3),
            ]
        );
        // Author: first non-empty album_artist/artist in file order —
        // here nobody has any → the honest placeholder.
        assert_eq!(groups[0].author, "Неизвестный автор");
    }

    #[test]
    fn no_album_group_pins_last_after_real_books() {
        let tracks = vec![
            book_track(1, "Файл", Some("Ясно"), None, None, 1, 1, 60_000),
            book_track(2, "Файл", None, None, None, 1, 1, 60_000),
        ];
        let groups = group_books(&tracks);
        let albums: Vec<&str> = groups.iter().map(|g| g.album.as_str()).collect();
        assert_eq!(albums, ["Ясно", "Без альбома"], "the catch-all group goes last");
    }

    #[test]
    fn book_without_any_artist_is_unknown() {
        let tracks = vec![book_track(1, "Файл", None, None, None, 1, 1, 60_000)];
        let groups = group_books(&tracks);
        assert_eq!(groups[0].album, "Без альбома");
        assert_eq!(groups[0].author, "Неизвестный автор");
    }

    #[test]
    fn total_duration_formats_hours_minutes() {
        assert_eq!(format_total_duration(0), "0:00");
        assert_eq!(format_total_duration(47_000), "0:00", "sub-minute stays 0:00");
        assert_eq!(format_total_duration(60_000), "0:01");
        assert_eq!(format_total_duration(3_600_000), "1:00");
        assert_eq!(format_total_duration(3_780_000), "1:03");
    }

    // ---- toolbar counter and empty-states (round 4 UX) ----

    #[test]
    fn counter_shows_total_without_query_and_ratio_with_it() {
        assert_eq!(tracks_counter(3, 3, ""), "3 трека");
        assert_eq!(tracks_counter(3, 5, ""), "5 треков");
        assert_eq!(tracks_counter(3, 1, ""), "1 трек");
        assert_eq!(tracks_counter(3, 5, "abc"), "3 из 5");
        // Whitespace counts as «no query»: the total is shown.
        assert_eq!(tracks_counter(0, 5, "   "), "5 треков");
    }

    #[test]
    fn empty_state_texts_follow_the_spec() {
        assert_eq!(
            empty_state_text(true, "", false, false),
            "Перетащите аудиофайлы в окно, чтобы добавить"
        );
        assert_eq!(
            empty_state_text(false, "  джаз  ", false, false),
            "Ничего не найдено по „джаз“"
        );
        assert_eq!(
            empty_state_text(false, "", false, true),
            "Пустой плейлист. Перетащите треки из таблицы"
        );
    }

    // ---- status-bar error tooltip (round 4 UX) ----

    #[test]
    fn error_tooltip_gathers_message_hint_and_nonempty_stderr() {
        let plain = UiError {
            message: "Не удалось определить блочное устройство".to_owned(),
            hint: None,
            stderr: None,
        };
        assert_eq!(error_tooltip(&plain), plain.message);

        let with_stderr = UiError {
            message: "mount failed".to_owned(),
            hint: Some("Проверьте пароль".to_owned()),
            stderr: Some("\n".to_owned()), // whitespace-only → skipped
        };
        assert_eq!(
            error_tooltip(&with_stderr),
            "mount failed\nПроверьте пароль"
        );

        let full = UiError {
            message: "mount failed".to_owned(),
            hint: None,
            stderr: Some("mount: bad option".to_owned()),
        };
        assert_eq!(error_tooltip(&full), "mount failed\n\nmount: bad option");
    }

    // ---- device-view media kinds (round 4.1 Ф2) ----

    #[test]
    fn view_media_kinds_per_firmware_views() {
        assert_eq!(view_media_kinds(4), &[1]);         // «Музыка»
        assert_eq!(view_media_kinds(5), &[8]);         // «Аудиокниги»
        assert_eq!(view_media_kinds(2), &[2]);         // «Фильмы»
        assert_eq!(view_media_kinds(3), &[16]);        // «Телешоу»
        assert_eq!(view_media_kinds(1), &[2, 16, 32]); // «Видео» — всё видео
        assert_eq!(view_media_kinds(7), &([]) as &[u32]);          // «Прокат» — не представим
        assert_eq!(view_media_kinds(0), &([]) as &[u32]);          // обычный/мастер-плейлист
        assert_eq!(view_media_kinds(11), &([]) as &[u32]);         // classic podcast — неизвестен
    }

    // ---- row click → selection (round 4.1 Ф3) ----

    #[test]
    fn plain_click_resets_selection_to_one() {
        let (selected, anchor) = apply_row_click(&[0, 3], Some(3), 5, false, false);
        assert_eq!(selected, [5]);
        assert_eq!(anchor, Some(5));
    }

    #[test]
    fn ctrl_click_toggles_preserving_order() {
        // Toggle off an already selected row.
        let (selected, anchor) = apply_row_click(&[2, 4], Some(2), 4, true, false);
        assert_eq!(selected, [2]);
        assert_eq!(anchor, Some(2), "ctrl keeps the anchor");
        // Toggle on a new row.
        let (selected, anchor) = apply_row_click(&[2], Some(2), 7, true, false);
        assert_eq!(selected, [2, 7]);
        assert_eq!(anchor, Some(2));
    }

    #[test]
    fn shift_click_selects_the_range_from_anchor() {
        let (selected, anchor) = apply_row_click(&[2], Some(2), 6, false, true);
        assert_eq!(selected, [2, 3, 4, 5, 6]);
        assert_eq!(anchor, Some(2), "shift keeps the anchor for further shifts");
        // Backwards shift works too.
        let (selected, anchor) = apply_row_click(&[5], Some(5), 1, false, true);
        assert_eq!(selected, [1, 2, 3, 4, 5]);
        assert_eq!(anchor, Some(5));
    }

    #[test]
    fn shift_without_anchor_acts_like_plain_click() {
        let (selected, anchor) = apply_row_click(&[1, 2], None, 6, false, true);
        assert_eq!(selected, [6]);
        assert_eq!(anchor, Some(6));
    }

    // ---- table columns fit the panel (round 4.1 Ф4) ----

    /// Worst-case central content width at a 900px window: 900 − 240
    /// sidebar − panel margins − vscroll bar. All four columns must be
    /// visible with a ≥120px title.
    #[test]
    fn table_columns_fit_narrow_panel_with_120px_title() {
        let rects: std::rc::Rc<std::cell::RefCell<Vec<egui::Rect>>> = Default::default();
        let sink = rects.clone();
        egui::__run_test_ui(|ui| {
            ui.allocate_ui(egui::vec2(616.0, 400.0), |ui| {
                let columns = render_measured_table(ui);
                *sink.borrow_mut() = columns;
            });
        });
        let rects = rects.borrow();
        assert_eq!(rects.len(), 4, "four header cells must render");
        let panel_right = 616.0;
        for (index, rect) in rects.iter().enumerate() {
            assert!(
                rect.right() <= panel_right + 1.0,
                "column {index} overflows the panel: {rect:?}"
            );
            assert!(rect.width() > 40.0, "column {index} collapsed: {rect:?}");
        }
        assert!(rects[0].width() >= 120.0, "title ≥ 120px, got {}", rects[0].width());
        assert!(rects[3].left() >= rects[2].right(), "columns must not overlap");
    }
}
