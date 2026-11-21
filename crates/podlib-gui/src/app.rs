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
    path_extension, plural, AUDIOBOOK_BIT, EditDialogFields, DraggedTracks, LibraryCommand,
    LibraryOutcome, LibrarySnapshot, LibraryWorker,
};
use crate::text_menu;

/// Row height of the track table rows, keeps the column grid aligned.
/// 26px (round 4 UX): a touch taller for a comfortable click/hover target.
const ROW_HEIGHT: f32 = 26.0;
/// Fixed column widths of the track table: artist/album/duration. The
/// title column takes the remainder (see `track_table`).
const ARTIST_COLUMN: f32 = 180.0;
const ALBUM_COLUMN: f32 = 200.0;
const DURATION_COLUMN: f32 = 64.0;
/// Width of the checkbox column (round 6 П2): one narrow first column
/// carrying the row checkbox; the header holds the tri-state «выбрать
/// все». Fits the 616px worst-case panel together with the other four
/// fixed columns (28+120+180+200+64 = 592).
const CHECK_COLUMN: f32 = 28.0;
/// Minimum width the central panel keeps for the track table: 28
/// (checkbox) + 100 (title, `at_least`) + 180 + 200 + 64 fixed columns
/// plus item spacing (round 6.5). The sidebar's width ceiling is
/// derived from it, so dragging the divider can never collapse the
/// table — egui itself clamps panel drags only at the screen edge.
const MIN_CENTRAL_PANEL_WIDTH: f32 = 616.0;
/// Height of one sidebar row (round 6 П1) — the table row height, so
/// both lists share one rhythm.
const SIDEBAR_ROW_HEIGHT: f32 = 26.0;
/// Left/right text inset inside a sidebar row. Identical for every
/// section (master / «Устройство» / «Плейлисты»): one geometry.
const SIDEBAR_ROW_INSET: f32 = 8.0;
/// Gap kept between a (clipped) row name and the right-aligned count.
const SIDEBAR_NAME_COUNT_GAP: f32 = 8.0;

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

/// Honest confirm texts (batch F): busy — the worker must not die
/// mid-write; idle with our mount — the close ejects the device, and
/// the eject needs the sudo password for it.
fn close_confirm_texts(busy: bool) -> (&'static str, &'static str) {
    if busy {
        (
            "Идёт операция с библиотекой",
            "Закрыть всё равно? Возможно повреждение базы на устройстве.",
        )
    } else {
        (
            "Закрыть podlib?",
            "Смонтированное устройство будет безопасно отключено перед \
             закрытием (понадобится пароль sudo).",
        )
    }
}

/// Retry hint for a failed eject (batch F): the library is already
/// closed and the button is available again, so a retry is safe.
/// Sudo-auth failures (classified by `classify_mount_failure`) get the
/// layout/Caps Lock wording; everything else keeps the honest «volume
/// may still be mounted».
fn ejected_failure_hint(failure: device::MountFailure) -> &'static str {
    match failure {
        device::MountFailure::WrongPassword => {
            "Пароль sudo отклонён — проверьте раскладку и Caps Lock. \
             Нажмите „Безопасно отключить“ ещё раз: библиотека уже \
             закрыта, том не тронут, повторный ввод пароля безопасен."
        }
        _ => {
            "Нажмите „Безопасно отключить“ ещё раз — библиотека уже \
             закрыта и повторная попытка безопасна; том мог остаться \
             смонтированным."
        }
    }
}
fn close_decision(
    close_requested: bool,
    operation_active: bool,
    close_confirmed: bool,
    mounted_by_us: bool,
) -> CloseDecision {
    if !close_requested || close_confirmed {
        return CloseDecision::Allow;
    }
    // Batch F: an idle close with a device mounted by this app must
    // also ask — the round 5.3 eject runs sudo steps and cannot be
    // done silently behind the user's back.
    if operation_active || mounted_by_us {
        return CloseDecision::BlockWithConfirm;
    }
    CloseDecision::Allow
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
    SetArtist,
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

/// «Задать исполнителя» modal state: the selection snapshot plus the
/// typed value; lives in [`LibraryUi`] so both survive frames.
#[derive(Clone)]
struct SetArtistDialog {
    track_ids: Vec<TrackId>,
    value: String,
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
    /// Round 6 П2: header checkbox toggled → target state (true =
    /// select all visible rows).
    select_all: Option<bool>,
    /// Round 6 П2: a row checkbox was clicked → explicit toggle of
    /// that id in the shared selection.
    checkbox_toggle: Option<TrackId>,
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
        self.select_all = other.select_all.or(self.select_all.take());
        self.checkbox_toggle = other.checkbox_toggle.or(self.checkbox_toggle.take());
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
    /// Open «Задать исполнителя» modal over the multi-selection: the
    /// snapshot of the selection plus the typed value (dialog fields
    /// live here so they survive frames).
    set_artist_dialog: Option<SetArtistDialog>,
    picker_tracks: Option<Vec<TrackId>>,
    /// Sidebar view context (round 4.1 Ф2): master, a built-in device
    /// view or a user playlist.
    view: LibraryView,
    /// Keyboard cursor (the last touched row); arrows and Enter work
    /// through it without disturbing the multi-selection.
    selected_track: Option<TrackId>,
    /// Multi-selection in the central table, in click order.
    selected_tracks: Vec<TrackId>,
    /// Anchor row (round 6 tail nit-10): (index into the visible list,
    /// TrackId of the anchored row) for shift-range clicks. The id
    /// makes the anchor identity-checked: after a re-list that replaced
    /// the rows, a stale anchor is dropped instead of shift-selecting
    /// a foreign range.
    selection_anchor: Option<(usize, TrackId)>,
    /// Remaining rows of a multi-track delete (one in-flight worker
    /// command at a time; the chain continues on each Applied).
    delete_queue: Vec<TrackId>,
    /// Expanded books in the «Аудиокниги» view (round 5), keyed by
    /// album. All collapsed by default; stale keys (a book renamed or
    /// gone) simply never render — no fix-up needed.
    expanded_books: HashSet<String>,
    /// Round 6 П3: expanded groups of the music section's album/artist
    /// views (books keep their own set — the names are different
    /// domains). Same stale-key semantics.
    expanded_groups: HashSet<String>,
    /// Round 6 П3: display modes of the grouping-capable sections.
    /// «Музыка» defaults to the flat list; «Аудиокниги» keeps the
    /// round-5 books view as its default (no behavior change).
    music_mode: SectionMode,
    books_mode: SectionMode,
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
    /// True when this app run is responsible for the safe unmount
    /// (round 5.3): closing the app then ejects the volume. Batch F:
    /// set for mounts we performed AND for adopted already-mounted
    /// volumes («Подключить» to a mount orphaned by a restart, or a
    /// foreign mount the owner deliberately connected to).
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
                // Batch F: a failed eject also cancels the pending close —
                // otherwise the deferred path would restart the eject every
                // idle frame (previously with an empty password again). The
                // window stays; the user can close it again, which now goes
                // through the password modal.
                self.pending_close_after_eject = false;
                // Batch F retry support: the library is already closed
                // (and `library_idle` counts None as idle), so the eject
                // button is available again — the hint says a retry is
                // safe. Sudo-auth failures get the layout/Caps Lock
                // wording, the rest keeps the honest «volume may still
                // be mounted».
                let hint = self.connected.is_some().then(|| {
                    ejected_failure_hint(device::classify_mount_failure(&message))
                        .to_owned()
                });
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
            set_artist_dialog: None,
            picker_tracks: None,
            view: LibraryView::Master,
            selected_track: None,
            selected_tracks: Vec::new(),
            selection_anchor: None,
            delete_queue: Vec::new(),
            expanded_books: HashSet::new(),
            expanded_groups: HashSet::new(),
            music_mode: SectionMode::List,
            books_mode: SectionMode::Books,
            search_query: String::new(),
        });
    }

    /// True while no library operation is in flight. Batch F: no library
    /// at all is also idle — after a failed eject the worker is closed
    /// (`None`), and the eject button plus the deferred-close paths must
    /// stay available for a retry.
    fn library_idle(&self) -> bool {
        self.library
            .as_ref()
            .is_none_or(|library| library.pending.is_none() && library.opening.is_none())
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
                        // Review 🟢-5: the applied operation may have
                        // removed tracks — dead ids must not stay in the
                        // selection, the toolbar would count rows that
                        // no longer exist and leak them into later
                        // actions. The anchor carries the same TrackId
                        // identity; dropping a stale one here mirrors
                        // `valid_anchor` (which would degenerate it to
                        // a plain click later anyway).
                        // M4 (batch edits): после «Задать исполнителя»
                        // ВСЕ выбранные сменили pid (контракт edit_track
                        // = remove+re-add) — осознанный сброс выделения
                        // вместо ремапа старых id на новые.
                        library
                            .selected_tracks
                            .retain(|id| snapshot.track(*id).is_some());
                        library.selection_anchor = library
                            .selection_anchor
                            .filter(|&(_, id)| snapshot.track(id).is_some());
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
        // Round 6.1: close library → sync → drain unmounts (one per
        // stacked mount of the devnode, per the /proc/mounts snapshot)
        // → power off the whole disk, strictly LAST.
        let block_dev = connected.block_dev.clone();
        // Round 6.2: EVERY shutdown step runs under sudo -S (password
        // over stdin, never argv) with this ONE password — the volume
        // was mounted by us as root, so a user-side unmount would pop a
        // polkit dialog (the second password prompt this round kills).
        // Taken here so it is consumed and cleared like in the connect
        // flow.
        let password = std::mem::take(&mut self.password);
        // Live-run diagnostic (round 6.2 F): sudo rejected the delivered
        // line once with «1 incorrect password attempt»; log the LENGTH
        // only — never the content — so an empty/short delivery is
        // distinguishable from a typed-but-rejected password.
        eprintln!(
            "[eject] пароль sudo: {} симв., ascii={}",
            password.chars().count(),
            password.bytes().all(|byte| byte.is_ascii()),
        );
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
                    // The mount snapshot decides the drain length: one
                    // unmount per stacked layer. The old code unmounted
                    // once, powered off, and only then tried to drain —
                    // behind a `?` — so power-off died on «The drive is
                    // in use» while sibling mounts were still alive.
                    let mounts = std::fs::read_to_string("/proc/mounts")
                        .map(|content| device::parse_proc_mounts(&content))
                        .unwrap_or_default();
                    let (steps, power_off) = device::build_shutdown_steps(
                        &block_dev,
                        Path::new("/sys/dev/block"),
                        &mounts,
                    );
                    // Same loop shape as the connect flow: each step
                    // under sudo, fail fast on the exit status, honest
                    // stderr in the message. WHY password lives across
                    // the whole loop: every sudo step consumes its own
                    // copy from stdin; it is dropped only after the
                    // last step, never stored anywhere.
                    for spec in steps.iter().chain(std::iter::once(&power_off)) {
                        let output = device::run_sudo_command(&password, spec)
                            .map_err(|error| device::shutdown_step_error(spec, &error.to_string()))?;
                        if !output.status.success() {
                            let stderr = String::from_utf8_lossy(&output.stderr);
                            return Err(device::shutdown_step_error(spec, &stderr));
                        }
                    }
                    drop(password);
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
                            self.password.clear();
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
            let (total, visible): (usize, Vec<&Track>) = match view {
                LibraryView::Master => (
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
                    (matches.len(), matches)
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
                    (members.len(), members)
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

            // Round 6 П3: the unified view switcher over the
            // grouping-capable sections («Музыка»: список/альбомы/
            // исполнители; «Аудиокниги»: список/книги). The mode is
            // re-read after the switcher — a click applies THIS frame.
            // Search keeps working over every mode: the filter runs
            // before grouping (the grouped views receive `filtered`).
            let switchable = matches!(view, LibraryView::Media { distinguished: 4 | 5 });
            if switchable {
                let distinguished = match view {
                    LibraryView::Media { distinguished } => distinguished,
                    _ => 0,
                };
                let current = if distinguished == 4 {
                    &mut library.music_mode
                } else {
                    &mut library.books_mode
                };
                view_switcher(ui, section_modes(distinguished), current);
                ui.add_space(2.0);
            }
            let mode = match view {
                LibraryView::Media { distinguished: 4 } => library.music_mode,
                LibraryView::Media { distinguished: 5 } => library.books_mode,
                _ => SectionMode::List,
            };
            // Per-context (and per-mode) scroll id: switching sections
            // or modes starts a fresh scroll position.
            let table_id = match view {
                LibraryView::Master => egui::Id::unique(("track_table", 0u64)),
                LibraryView::Media { distinguished } => egui::Id::unique((
                    "track_table",
                    u64::try_from(distinguished.max(0)).unwrap_or(0),
                    mode as u64,
                )),
                LibraryView::Playlist(id) => {
                    egui::Id::unique(("track_table", id.to_bits()))
                }
            };

            if filtered.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.label(egui::RichText::new(empty_state_text(
                        library.snapshot.tracks.is_empty(),
                        &query,
                        empty_view,
                        empty_playlist,
                    )).weak());
                });
            } else if switchable && mode != SectionMode::List {
                // Grouped views (round 5 books + round 6 П3) share ONE
                // component; each mode keeps its own expanded set.
                let groups = match mode {
                    SectionMode::Albums => group_by_album(filtered.iter().copied()),
                    SectionMode::Artists => group_by_artist(filtered.iter().copied()),
                    _ => group_books(filtered.iter().copied()),
                };
                let expanded = if mode == SectionMode::Books {
                    &mut library.expanded_books
                } else {
                    &mut library.expanded_groups
                };
                actions.merge(groups_view(
                    ui,
                    GroupsViewParams {
                        ctx: &ctx,
                        tracks: &filtered,
                        groups: &groups,
                        selected_tracks: &library.selected_tracks,
                        cursor,
                        menus_allowed,
                        visible_ids: &visible_ids,
                        expanded,
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

            // Row-click / «выбрать все» / checkbox resolution lives in
            // the pure [`resolve_selection_actions`] (review 🟡-3);
            // `cursor` is the value read before rendering and nothing
            // below has touched the field yet, so passing it back is a
            // no-op when no branch moves the cursor.
            let update = resolve_selection_actions(
                &library.selected_tracks,
                library.selection_anchor,
                cursor,
                &visible_ids,
                actions.row_click.take(),
                actions.select_all.take(),
                actions.checkbox_toggle.take(),
            );
            library.selected_tracks = update.selected_tracks;
            library.selection_anchor = update.selection_anchor;
            library.selected_track = update.selected_track;

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
            if toolbar.set_artist && !selected.is_empty() {
                library.set_artist_dialog = Some(SetArtistDialog {
                    track_ids: selected.clone(),
                    value: String::new(),
                });
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
        if close_decision(
            close_requested,
            operation_active,
            self.force_close,
            self.mounted_by_us,
        ) == CloseDecision::BlockWithConfirm
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
        let decision = password_modal(
            ctx,
            "confirm_mount",
            &mut self.password,
            "Подключить устройство (запись)",
            "Система монтирует том HFS+ только для чтения; для записи нужен \
             повторный mount с force. Пароль sudo нужен, потому что \
             монтирование — системное действие; он передаётся напрямую \
             команде sudo и нигде не сохраняется.",
            "Подключить",
        );
        if decision == PasswordModal::Confirmed {
            self.start_mount = true; // act after the frame (borrow rules)
        }
        if decision != PasswordModal::Open {
            self.confirm_mount = false;
        }
    }

    fn maybe_eject_modal(&mut self, ctx: &egui::Context) {
        if !self.confirm_eject {
            return;
        }
        // Round 6.1: power-off runs under sudo, so disconnect asks for
        // the same password as connect — one shared modal.
        let decision = password_modal(
            ctx,
            "confirm_eject",
            &mut self.password,
            "Безопасно отключить устройство?",
            "Будут выполнены: закрытие библиотеки, sync, unmount (с др\
             ейном слоёв) и power-off под sudo. Убедитесь, что операции \
             записи завершены. Пароль передаётся напрямую команде sudo и \
             нигде не сохраняется.",
            "Отключить",
        );
        if decision == PasswordModal::Confirmed {
            self.start_eject = true;
        }
        // Batch F: cancelling the eject also cancels the close that asked
        // for it — the window stays, and the deferred path cannot re-open
        // this modal every idle frame.
        if decision == PasswordModal::Cancelled && self.pending_close_after_eject {
            self.pending_close_after_eject = false;
        }
        if decision != PasswordModal::Open {
            self.confirm_eject = false;
        }
    }

    /// A close requested mid-operation (round 5.3) fires its eject as soon
    /// as the app goes idle again. Batch F (round 6.1 F3): the eject opens
    /// the shared password modal instead of starting directly — the direct
    /// start ran sudo on an empty password. Opening is harmless while the
    /// modal is already up; cancellation clears `pending_close_after_eject`
    /// (see `maybe_eject_modal`), so the prompt cannot loop.
    fn maybe_deferred_close_eject(&mut self) {
        if deferred_eject_due(self.pending_close_after_eject, self.busy, self.library_idle()) {
            self.confirm_eject = true;
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
        let (title, body) = close_confirm_texts(self.busy || self.library_busy());
        let response = egui::Modal::new(egui::Id::unique("confirm_close")).show(ctx, |ui| {
            ui.set_width(440.0);
            ui.heading(title);
            ui.add_space(4.0);
            ui.label(body);
            ui.separator();
            // Enter confirms the primary action («Закрыть») at the
            // input level, regardless of focus — the same pattern as
            // the password modal (round 6.1 F3).
            if ui.input(|input| input.key_pressed(egui::Key::Enter)) {
                confirmed = true;
                ui.close();
            }
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
            self.apply_close_confirmation(ctx);
        }
    }

    /// What a confirmed window close does (round 5.3): a device mounted
    /// by this app run is ejected first — through the password modal
    /// (batch F), never a direct start with an empty password; a busy
    /// worker defers the eject until the app goes idle. Anything else
    /// closes immediately.
    fn apply_close_confirmation(&mut self, ctx: &egui::Context) {
        match close_confirmation_action(self.mounted_by_us) {
            CloseAction::EjectFirst => {
                self.pending_close_after_eject = true;
                if deferred_eject_due(true, self.busy, self.library_idle()) {
                    self.confirm_eject = true;
                }
            }
            CloseAction::CloseNow => {
                self.force_close = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
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
            // Enter confirms the primary action («Удалить») at the
            // input level, regardless of focus — the same pattern as
            // the password modal (round 6.1 F3).
            if ui.input(|input| input.key_pressed(egui::Key::Enter)) {
                confirmed = true;
                ui.close();
            }
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
                // Enter confirms the primary action («Удалить») at the
                // input level, regardless of focus — the same pattern as
                // the password modal (round 6.1 F3).
                if ui.input(|input| input.key_pressed(egui::Key::Enter)) {
                    confirmed = true;
                    ui.close();
                }
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
                let name = ui.add(
                    egui::TextEdit::singleline(&mut library.new_playlist_name)
                        .desired_width(240.0),
                );
                text_menu::attach_text_menu(&name);
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

    /// Batch «Задать исполнителя» over the multi-selection (round 6.3).
    /// One SetArtist command carries the whole batch — the worker loops
    /// ids and reports the honest per-id tail. An empty (whitespace
    /// only) value disables «Присвоить»: the domain treats a blank
    /// artist as «no value», so confirming would be a silent no-op.
    fn maybe_set_artist_modal(&mut self, ctx: &egui::Context) {
        let Some(dialog) = self
            .library
            .as_ref()
            .and_then(|library| library.set_artist_dialog.clone())
        else {
            return;
        };
        let count = dialog.track_ids.len();
        let mut confirmed = false;
        let mut artist = String::new();
        let response = egui::Modal::new(egui::Id::unique("set_artist")).show(ctx, |ui| {
            ui.set_width(440.0);
            let Some(library) = self.library.as_mut() else {
                return;
            };
            let Some(dialog) = library.set_artist_dialog.as_mut() else {
                return;
            };
            // The typed value appears in the heading as soon as it is
            // non-empty, mirroring the quoted-entity style of the delete
            // confirmation («Удалить трек «X»?»).
            let value_trimmed = dialog.value.trim().to_owned();
            if value_trimmed.is_empty() {
                ui.heading(format!(
                    "Присвоить исполнителя выбранным трекам ({count})?"
                ));
            } else {
                ui.heading(format!(
                    "Присвоить исполнителя «{value_trimmed}» выбранным трекам ({count})?"
                ));
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label("Исполнитель:");
                let field = ui.add(
                    egui::TextEdit::singleline(&mut dialog.value)
                        .hint_text("Артист")
                        .desired_width(300.0),
                );
                text_menu::attach_text_menu(&field);
                field.request_focus();
            });
            // N1: the command carries the TRIMMED value — the header and
            // the readiness gate both use the trimmed form, so an
            // untrimmed send would store an artist invisible to the user.
            artist = dialog.value.trim().to_owned();
            ui.separator();
            // Enter confirms at the input level regardless of focus —
            // the password-modal pattern (round 6.1 F3).
            let ready = !artist.trim().is_empty();
            if ready && ui.input(|input| input.key_pressed(egui::Key::Enter)) {
                confirmed = true;
                ui.close();
            }
            ui.horizontal(|ui| {
                if ui.button("Отмена").clicked() {
                    ui.close();
                }
                if ui.add_enabled(ready, egui::Button::new("Присвоить")).clicked() {
                    confirmed = true;
                    ui.close();
                }
            });
        });
        if confirmed || response.should_close() {
            if let Some(library) = self.library.as_mut() {
                library.set_artist_dialog = None;
            }
        }
        if confirmed && !artist.trim().is_empty() {
            self.library_request(
                LibraryCommand::SetArtist {
                    track_ids: dialog.track_ids,
                    artist,
                },
                PendingKind::SetArtist,
                "Задаю исполнителя…",
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
        // Kind-aware validation text (round 6 tail nit-8).
        let texts = tag_review_texts(review.kind_bits);
        match apply_tag_review(
            &review.drafts,
            &review.book_title,
            &review.author,
            &review.rows,
            texts.empty_title,
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
            // Kind-aware texts (round 6 tail nit-8): audiobooks speak
            // «книга/автор», music «альбом/исполнитель».
            let texts = tag_review_texts(review.kind_bits);
            let title = format!(
                "Добавление {}: проверьте теги ({} файлов)",
                texts.subject,
                review.rows.len()
            );
            ui.set_width(560.0);
            ui.heading(title);
            ui.add_space(4.0);
            ui.label("Общее для всех файлов");
            ui.horizontal(|ui| {
                ui.label(texts.title_field);
                let book_title = ui.add(
                    egui::TextEdit::singleline(&mut review.book_title)
                        .desired_width(320.0),
                );
                text_menu::attach_text_menu(&book_title);
            });
            if review.validation_attempted && review.book_title.trim().is_empty() {
                ui.colored_label(egui::Color32::LIGHT_RED, texts.empty_title);
            }
            ui.horizontal(|ui| {
                ui.label(texts.author_field);
                let author = ui.add(
                    egui::TextEdit::singleline(&mut review.author).desired_width(320.0),
                );
                text_menu::attach_text_menu(&author);
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
                    // nit-9: only rows still showing the previous
                    // automatic value are re-derived — manual edits in
                    // the title column survive the toggle.
                    retoggle_prefix_titles(&mut review.rows, &prefix, review.strip_prefix);
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
                                text_menu::attach_text_menu(&title);
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
                if ui.button(texts.add_button).clicked() {
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

// ---- round 6 П3: section view modes ----

/// Display mode of a section's central panel. The switcher itself is
/// ONE component reused by every grouping-capable section; playlists
/// and the master stay flat lists (and never render it).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SectionMode {
    /// Flat track table (with the checkbox column).
    #[default]
    List,
    /// Grouped by album («Музыка»).
    Albums,
    /// Grouped by artist («Музыка»).
    Artists,
    /// Grouped by book = album («Аудиокниги», round 5 default).
    Books,
}

impl SectionMode {
    /// Switcher label.
    fn label(self) -> &'static str {
        match self {
            Self::List => "Список",
            Self::Albums => "Альбомы",
            Self::Artists => "Исполнители",
            Self::Books => "Книги",
        }
    }
}

/// Switcher modes of a built-in device view, in display order:
/// «Музыка» offers список/альбомы/исполнители, «Аудиокниги»
/// список/книги, the rest just the flat list (the switcher is not
/// rendered for them).
fn section_modes(distinguished_kind: i64) -> &'static [SectionMode] {
    match distinguished_kind {
        4 => &[SectionMode::List, SectionMode::Albums, SectionMode::Artists],
        5 => &[SectionMode::List, SectionMode::Books],
        _ => &[SectionMode::List],
    }
}

/// The unified view switcher (round 6 П3): a segmented row of labels
/// directly over the table/content. Returns true when the active mode
/// changed. Theme-driven `selectable_label` — no custom colors, the
/// system theme decides (owner requirement, round 4).
fn view_switcher(ui: &mut egui::Ui, modes: &[SectionMode], current: &mut SectionMode) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        for mode in modes {
            let active = *mode == *current;
            if ui.selectable_label(active, mode.label()).clicked() && !active {
                *current = *mode;
                changed = true;
            }
        }
    });
    changed
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

// ---- round 6 П2: checkbox multi-select (pure helpers, tested) ----

/// Tri-state of the «выбрать все» header checkbox against the visible
/// rows: every visible id selected → checked, none → unchecked,
/// anything between (including a partial-visibility edge) → dash.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SelectAllState {
    All,
    Some,
    None,
}

fn select_all_state(selected: &[TrackId], visible: &[TrackId]) -> SelectAllState {
    let hits = visible.iter().filter(|id| selected.contains(id)).count();
    match (hits == 0, hits == visible.len()) {
        (true, _) => SelectAllState::None,
        (_, true) => SelectAllState::All,
        (false, false) => SelectAllState::Some,
    }
}

/// Header-checkbox toggle: checking adds every visible id (the click
/// order of already-selected rows is preserved, new ones append in
/// visible order); unchecking removes exactly the visible ids, so a
/// selection made elsewhere (another view, out-of-filter rows)
/// survives. The result feeds the same `selected_tracks` — the single
/// selection mechanism (no parallel state).
fn apply_select_all(selected: &[TrackId], visible: &[TrackId], check: bool) -> Vec<TrackId> {
    if check {
        let mut result = selected.to_vec();
        for id in visible {
            if !result.contains(id) {
                result.push(*id);
            }
        }
        result
    } else {
        selected
            .iter()
            .copied()
            .filter(|id| !visible.contains(id))
            .collect()
    }
}

/// One checkbox click = an explicit toggle of that row in the shared
/// selection. Unlike a row click it never resets the rest, never moves
/// the shift anchor — and keeps the click order stable.
fn toggle_selected(selected: &[TrackId], id: TrackId) -> Vec<TrackId> {
    let mut result = selected.to_vec();
    match result.iter().position(|candidate| candidate == &id) {
        Some(position) => {
            result.remove(position);
        }
        None => result.push(id),
    }
    result
}

/// The stored shift-anchor is valid only if the row identity still
/// matches the visible list (round 6 tail nit-10): a boundary check
/// alone let a re-listed view shift-select a foreign range. Returns
/// the anchor index for `apply_row_click`, or None (shift then
/// degenerates to a plain click).
fn valid_anchor(anchor: Option<(usize, TrackId)>, visible: &[TrackId]) -> Option<usize> {
    anchor
        .filter(|&(index, id)| visible.get(index) == Some(&id))
        .map(|(index, _)| index)
}

/// The selection state after resolving one frame's row interactions.
struct SelectionUpdate {
    selected_tracks: Vec<TrackId>,
    selection_anchor: Option<(usize, TrackId)>,
    selected_track: Option<TrackId>,
}

/// Pure selection resolution (review 🟡-3: functions < 50 lines —
/// extracted from `library_view`): row clicks (plain/ctrl/shift),
/// «выбрать все» and a row-checkbox toggle, applied in that order to
/// the SAME `selected_tracks` — one selection mechanism. Checkbox
/// changes land here too, so the toolbar and the row clicks read the
/// same state. The cursor moves only where the mechanics demand it: a
/// row click targets the clicked row, a checkbox check targets the
/// toggled row; «выбрать все» rewrites the row set and deliberately
/// leaves the cursor put (review 🟡-1), so Enter/«Изменить…» keep the
/// row they had before the bulk edit.
fn resolve_selection_actions(
    selected_tracks: &[TrackId],
    selection_anchor: Option<(usize, TrackId)>,
    cursor: Option<TrackId>,
    visible_ids: &[TrackId],
    row_click: Option<(usize, bool, bool)>,
    select_all: Option<bool>,
    checkbox_toggle: Option<TrackId>,
) -> SelectionUpdate {
    let mut selection = SelectionUpdate {
        selected_tracks: selected_tracks.to_vec(),
        selection_anchor,
        selected_track: cursor,
    };

    // A click on a visible row resolves through apply_row_click
    // (plain/ctrl/shift) against the current visible list. The
    // anchor is identity-checked first (nit-10): a stale anchor
    // (the list shrank OR the row was replaced by a re-list) is
    // dropped, so shift degenerates to a plain click.
    if let Some((index, ctrl, shift)) = row_click {
        let anchor = valid_anchor(selection.selection_anchor, visible_ids);
        let selected_idx: Vec<usize> = selection
            .selected_tracks
            .iter()
            .filter_map(|id| visible_ids.iter().position(|candidate| candidate == id))
            .collect();
        let (new_indices, anchor_index) =
            apply_row_click(&selected_idx, anchor, index, ctrl, shift);
        selection.selected_tracks = new_indices
            .iter()
            .filter_map(|&index| visible_ids.get(index).copied())
            .collect();
        // Whatever anchor apply_row_click kept, the row identity
        // is recorded with it.
        selection.selection_anchor = anchor_index
            .and_then(|index| visible_ids.get(index).map(|&id| (index, id)));
        selection.selected_track = selection.selected_tracks.last().copied();
    }

    // «выбрать все» edits the row set only: the cursor stays put
    // (review 🟡-1) — a bulk selection is not a navigation act.
    if let Some(check) = select_all {
        selection.selected_tracks = apply_select_all(&selection.selected_tracks, visible_ids, check);
    }

    // A checkbox check moves the cursor to the toggled row (Enter
    // targets the last touched row); an uncheck leaves it where it is.
    if let Some(id) = checkbox_toggle {
        selection.selected_tracks = toggle_selected(&selection.selected_tracks, id);
        if selection.selected_tracks.contains(&id) {
            selection.selected_track = Some(id);
        }
    }

    selection
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

// ---- round 5 + 6 П3: track grouping (pure helpers, tested) ----

/// One named group (book / album / artist) with its tracks in display
/// order. Duration/count are derived from `track_ids` + the snapshot
/// at render time (single source of truth, KISS) — not stored here.
#[derive(Clone, Debug, PartialEq, Eq)]
struct TrackGroup {
    name: String,
    /// Secondary info line: the author of a book or album. `None` →
    /// the info line shows only the counters (the «Исполнители» view,
    /// where the name IS the artist).
    author: Option<String>,
    track_ids: Vec<TrackId>,
}

/// The album-as-book key: blank/missing tags collapse into one
/// fallback group (pinned last by the sort — see `group_books`).
const NO_ALBUM_GROUP: &str = "Без альбома";
/// The artist catch-all, pinned last in the «Исполнители» view.
const NO_ARTIST_GROUP: &str = "Неизвестный исполнитель";
/// Honest placeholder of the books view (round 5 behavior).
const UNKNOWN_BOOK_AUTHOR: &str = "Неизвестный автор";

fn album_group_key(track: &Track) -> String {
    track
        .album
        .as_deref()
        .map(str::trim)
        .filter(|album| !album.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| NO_ALBUM_GROUP.to_owned())
}

/// Artist of a track for the «Исполнители» view: album_artist →
/// artist (the same fallback as the author line — Apple semantics);
/// blank/missing tags collapse into the pinned catch-all group.
fn artist_group_key(track: &Track) -> String {
    [track.album_artist.as_deref(), track.artist.as_deref()]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|text| !text.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| NO_ARTIST_GROUP.to_owned())
}

/// Sort key of a file inside its group: numbered discs/tracks first
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

/// Shared grouping engine: buckets tracks by `key` (insertion order),
/// sorts buckets case-insensitively with the `catch_all` bucket pinned
/// last, then orders the members with `member_sort`.
fn group_tracks_by<'a>(
    tracks: impl IntoIterator<Item = &'a Track>,
    key: impl Fn(&Track) -> String,
    catch_all: &str,
    member_sort: impl Fn(&mut [&'a Track]),
) -> Vec<(String, Vec<&'a Track>)> {
    let mut buckets: Vec<(String, Vec<&Track>)> = Vec::new();
    for track in tracks {
        let group = key(track);
        match buckets.iter_mut().find(|(name, _)| *name == group) {
            Some((_, members)) => members.push(track),
            None => buckets.push((group, vec![track])),
        }
    }
    buckets.sort_by(|(a, _), (b, _)| {
        // The synthetic catch-all bucket pins last: real groups first,
        // the fallback below them.
        (a == catch_all)
            .cmp(&(b == catch_all))
            .then_with(|| a.to_lowercase().cmp(&b.to_lowercase()))
    });
    for (_, members) in &mut buckets {
        member_sort(members);
    }
    buckets
}

/// The author of a group: the first non-empty album_artist (Apple
/// semantics: the album artist outranks the track artist), then
/// artist, in the group's final file order.
fn resolved_author(members: &[&Track]) -> Option<String> {
    members
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
}

/// Groups the (already filtered) audiobook tracks into books (round 5
/// behavior): albums case-insensitive, «Без альбома» last, files by
/// (disc, track, title); the author line always shows — honestly
/// «Неизвестный автор» when nobody is tagged.
fn group_books<'a>(tracks: impl IntoIterator<Item = &'a Track>) -> Vec<TrackGroup> {
    group_tracks_by(tracks, album_group_key, NO_ALBUM_GROUP, |members| {
        members.sort_by_key(|track| book_file_sort_key(track));
    })
    .into_iter()
    .map(|(name, members)| TrackGroup {
        author: Some(
            resolved_author(&members).unwrap_or_else(|| UNKNOWN_BOOK_AUTHOR.to_owned()),
        ),
        name,
        track_ids: members.iter().map(|track| track.id).collect(),
    })
    .collect()
}

/// Round 6 П3: the music view grouped by album — the same engine as
/// books («Без альбома» pinned last, files by disc/track/title), but
/// the author line stays honest: an untagged album shows no author.
fn group_by_album<'a>(tracks: impl IntoIterator<Item = &'a Track>) -> Vec<TrackGroup> {
    group_tracks_by(tracks, album_group_key, NO_ALBUM_GROUP, |members| {
        members.sort_by_key(|track| book_file_sort_key(track));
    })
    .into_iter()
    .map(|(name, members)| TrackGroup {
        author: resolved_author(&members),
        name,
        track_ids: members.iter().map(|track| track.id).collect(),
    })
    .collect()
}

/// Round 6 П3: the music view grouped by artist («Неизвестный
/// исполнитель» pinned last); the files of one artist are ordered by
/// album first, then by disc/track/title — a tiny discography.
fn group_by_artist<'a>(tracks: impl IntoIterator<Item = &'a Track>) -> Vec<TrackGroup> {
    group_tracks_by(tracks, artist_group_key, NO_ARTIST_GROUP, |members| {
        members.sort_by(|a, b| {
            album_group_key(a)
                .to_lowercase()
                .cmp(&album_group_key(b).to_lowercase())
                .then_with(|| book_file_sort_key(a).cmp(&book_file_sort_key(b)))
        });
    })
    .into_iter()
    .map(|(name, members)| TrackGroup {
        author: None,
        name,
        track_ids: members.iter().map(|track| track.id).collect(),
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

// ---- round 6 tail nit-8: kind-aware tag-review texts ----

/// Field labels and messages of the tag-review modal, chosen by the
/// media kind: audiobooks speak «книга/автор», music speaks
/// «альбом/исполнитель». The check is the semantic audiobook BIT —
/// combined bits stay book-flavored.
struct TagReviewTexts {
    /// Subject of the heading («Добавление {subject}: …»).
    subject: &'static str,
    /// Label of the shared title field.
    title_field: &'static str,
    /// Label of the shared author field.
    author_field: &'static str,
    /// Near-field validation text for an empty title.
    empty_title: &'static str,
    /// The commit button.
    add_button: &'static str,
}

fn tag_review_texts(kind_bits: u32) -> TagReviewTexts {
    if kind_bits & AUDIOBOOK_BIT != 0 {
        TagReviewTexts {
            subject: "книги",
            title_field: "Название книги*",
            author_field: "Автор",
            empty_title: "Укажите название книги",
            add_button: "Добавить книгу",
        }
    } else {
        TagReviewTexts {
            subject: "музыки",
            title_field: "Альбом",
            author_field: "Исполнитель",
            empty_title: "Укажите название альбома",
            add_button: "Добавить музыку",
        }
    }
}

// ---- round 6 tail nit-9: prefix toggle keeps manual edits ----

/// Re-derives the automatic titles after the prefix-checkbox toggle,
/// but only for rows still showing the PREVIOUS automatic value — a
/// manually edited title survives (round 6 tail nit-9: the checkbox
/// used to overwrite every row from the raw file stem).
fn retoggle_prefix_titles(rows: &mut [TagRow], prefix: &str, strip_now: bool) {
    for row in rows {
        let auto_now = if strip_now {
            strip_prefix_in(&row.original_title, prefix)
        } else {
            row.original_title.clone()
        };
        let auto_before = if strip_now {
            row.original_title.clone()
        } else {
            strip_prefix_in(&row.original_title, prefix)
        };
        if row.title == auto_before {
            row.title = auto_now;
        }
    }
}

/// Applies the reviewed tags to the drafts: album/book title for all,
/// author as album_artist (when non-empty) AND as artist for drafts
/// without an own artist — the vendor's display_artist() persists
/// item.artist only, so unpropagated tracks stay nameless in «Песни» —
/// per-row title (an empty
/// edit falls back to the file stem) and track number. The empty-title
/// validation text is kind-aware (round 6 tail nit-8).
fn apply_tag_review(
    drafts: &[TrackDraft],
    book_title: &str,
    author: &str,
    rows: &[TagRow],
    empty_title_error: &str,
) -> Result<Vec<TrackDraft>, String> {
    let book = book_title.trim();
    if book.is_empty() {
        return Err(empty_title_error.to_owned());
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
                // Propagate into `artist` when the draft has none: the
                // vendor's display_artist() persists item.artist only,
                // so without this the track stays nameless in «Песни».
                // A pre-existing artist is never overwritten.
                if prepared.artist.as_deref().is_none_or(str::is_empty) {
                    prepared.artist = Some(author.to_owned());
                }
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

/// One editable file line of the review modal. `original_title` is
/// the untouched file stem — the reference for the prefix toggle to
/// detect (and spare) manual edits (round 6 tail nit-9).
#[derive(Clone, Debug, PartialEq, Eq)]
struct TagRow {
    title: String,
    original_title: String,
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
    let stems: Vec<String> = drafts.iter().map(|d| d.title.clone()).collect();
    let prefix = common_prefix(&stems);
    let rows: Vec<TagRow> = drafts
        .iter()
        .enumerate()
        .map(|(index, draft)| TagRow {
            title: draft.title.clone(),
            original_title: draft.title.clone(),
            track_number: trailing_number(&draft.title)
                .unwrap_or(index as u32 + 1),
        })
        .collect();
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
    set_artist: bool,
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
        let search = ui.add(
            egui::TextEdit::singleline(query)
                .hint_text("Поиск…")
                .id(search_box_id)
                .desired_width(240.0),
        );
        text_menu::attach_text_menu(&search);
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
                .add_enabled(enabled, egui::Button::new("Задать исполнителя…"))
                .clicked()
            {
                toolbar.set_artist = true;
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
/// (round 4.1 Ф4): a narrow checkbox column (round 6 П2), the title
/// takes the remainder with a 100px floor, the other three are exact
/// widths. WHY the floor: with `remainder` + fixed columns in a narrow
/// panel the remainder can collapse to 0 (`Sizing::to_lengths` clamps
/// negative remainders to 0) and the row loses its title entirely; the
/// floor keeps it readable. 100px (was 120 in round 4.1): the checkbox
/// column joined the same 616px worst case, and the title now floors
/// just under its 104px worst-case share instead of overflowing.
fn track_table_builder<'a>(ui: &'a mut egui::Ui, table_id: egui::Id) -> TableBuilder<'a> {
    TableBuilder::new(ui)
        .id_salt(table_id)
        .striped(true)
        .vscroll(true)
        .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
        .column(Column::exact(CHECK_COLUMN).resizable(false))
        .column(Column::remainder().clip(true).resizable(false).at_least(100.0))
        .column(Column::exact(ARTIST_COLUMN).clip(true).resizable(false))
        .column(Column::exact(ALBUM_COLUMN).clip(true).resizable(false))
        .column(Column::exact(DURATION_COLUMN).resizable(false))
}

/// Left sidebar of the main layout: 160px floor (решение владельца,
/// 20.11.2026) and a ceiling that keeps MIN_CENTRAL_PANEL_WIDTH for the
/// track table — egui clamps panel drags only at the screen edge and
/// reserves nothing for the central panel, so without the ceiling the
/// divider can be pulled until the table disappears (round 6.5).
/// Shared by the production layout and the panel tests so the width
/// contract lives in exactly one place.
fn device_panel_builder(ui: &egui::Ui) -> egui::Panel {
    let screen_w = ui.input(|input| input.viewport_rect().width());
    egui::Panel::left("device_panel")
        .default_size(240.0)
        .resizable(true)
        .size_range(egui::Rangef::new(
            160.0,
            (screen_w - MIN_CENTRAL_PANEL_WIDTH).max(160.0),
        ))
}

/// Aggregated arguments for [`groups_view`] (grouped track views);
/// `ui` stays a plain argument per egui convention. `groups` arrive
/// precomputed (group_books / group_by_album / group_by_artist) — the
/// component is render-only (round 6 П3: ONE grouped-list component
/// for books, albums and artists).
struct GroupsViewParams<'a> {
    ctx: &'a egui::Context,
    tracks: &'a [&'a Track],
    groups: &'a [TrackGroup],
    selected_tracks: &'a [TrackId],
    cursor: Option<TrackId>,
    menus_allowed: bool,
    visible_ids: &'a [TrackId],
    expanded: &'a mut HashSet<String>,
}

/// The grouped view component (round 5 books / round 6 П3): groups
/// collapsed by default; a header click toggles the group; an expanded
/// group shows an info line (optional author · files · total duration)
/// and its file list. File rows reuse the proven selection machinery:
/// clicks report their index in `visible_ids` (headers are not
/// selectable — they toggle), so apply_row_click / anchor / toolbar /
/// keyboard all work unchanged.
fn groups_view(ui: &mut egui::Ui, params: GroupsViewParams<'_>) -> TrackRowActions {
    let GroupsViewParams {
        ctx,
        tracks,
        groups,
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
    // Durations and titles for the infoblock/file rows; the groups
    // carry ids only (single source of truth, see TrackGroup).
    let by_id: HashMap<TrackId, &Track> =
        tracks.iter().map(|track| (track.id, *track)).collect();
    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .show(ui, |ui| {
            for group in groups {
                let is_expanded = expanded.contains(&group.name);
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
                    format!("{arrow} {}", group.name),
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
                    if !expanded.insert(group.name.clone()) {
                        expanded.remove(&group.name);
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
                            let total_text = format_total_duration(total_ms);
                            let info = match &group.author {
                                Some(author) => {
                                    format!("Автор: {author} · {files_text} · {total_text}")
                                }
                                None => format!("{files_text} · {total_text}"),
                            };
                            ui.label(egui::RichText::new(info).weak().size(13.0));
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

/// The tri-state «выбрать все» header checkbox (round 6 П2): a click
/// selects every visible row; from the checked state a click clears
/// them again. From the partial dash a click selects too — egui's
/// indeterminate flag is purely visual and does not invert the
/// toggle. Returns the requested target state when the widget was
/// clicked.
fn header_select_all(ui: &mut egui::Ui, state: SelectAllState) -> Option<bool> {
    let mut checked = state == SelectAllState::All;
    let response = ui.add(
        egui::Checkbox::without_text(&mut checked)
            .indeterminate(state == SelectAllState::Some),
    );
    response.changed().then_some(checked)
}

/// One checkbox cell of a table row (round 6 П2): an explicit toggle
/// of the row in the shared selection. A plain widget on purpose —
/// unlike the draggable title/artist/album/duration cells it must not
/// intercept or emit row clicks.
fn row_checkbox(ui: &mut egui::Ui, selected_tracks: &[TrackId], id: TrackId) -> Option<TrackId> {
    let mut checked = selected_tracks.contains(&id);
    let response = ui.add(egui::Checkbox::without_text(&mut checked));
    response.changed().then_some(id)
}

/// The track table (round 4): `egui_extras::TableBuilder` with striped
/// rows, per-context scroll state, clip + hover-tooltips, a right
/// aligned duration column, multi-row selection (Ф3: plain/ctrl/shift
/// clicks, toolbar actions; round 6 П2: checkbox column + «выбрать
/// все») and full-row drag. Collects row choices without holding an
/// app-state borrow.
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
    let visible_ids: Vec<TrackId> = tracks.iter().map(|track| track.id).collect();
    track_table_builder(ui, table_id)
        .header(ROW_HEIGHT, |mut header| {
            header.col(|ui| {
                if let Some(check) =
                    header_select_all(ui, select_all_state(selected_tracks, &visible_ids))
                {
                    actions.select_all = Some(check);
                }
            });
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
                    // Round 6 П2: the checkbox column — an explicit
                    // toggle, kept out of the drag/click machinery.
                    row.col(|ui| {
                        if let Some(id) = row_checkbox(ui, selected_tracks, track.id) {
                            actions.checkbox_toggle = Some(id);
                        }
                    });
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

/// One sidebar row (round 6 П1): the selection spans the FULL panel
/// width — the standard list pattern — instead of ending at the text
/// (the old `selectable_value` hugged its label, so rows had ragged
/// widths). Name at a fixed left inset, count pinned to the right
/// edge; selected + hover are painted backgrounds. A too-long name is
/// clipped before the count and shows the full text on hover.
fn sidebar_row(ui: &mut egui::Ui, selected: bool, name: &str, count: &str) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), SIDEBAR_ROW_HEIGHT),
        egui::Sense::click(),
    );
    let painter = ui.painter();
    let visuals = ui.visuals();
    if selected {
        painter.rect_filled(rect, 4.0, visuals.selection.bg_fill);
    } else if response.hovered() {
        painter.rect_filled(rect, 4.0, visuals.widgets.hovered.bg_fill);
    }
    let inner = rect.shrink2(egui::vec2(SIDEBAR_ROW_INSET, 0.0));
    let middle = rect.center().y;
    let count_galley = painter.layout_no_wrap(
        count.to_owned(),
        egui::FontId::proportional(12.0),
        visuals.weak_text_color(),
    );
    painter.text(
        egui::pos2(inner.right(), middle),
        egui::Align2::RIGHT_CENTER,
        count,
        egui::FontId::proportional(12.0),
        visuals.weak_text_color(),
    );
    // The name is laid out first and clipped to the room left of the
    // count: a long playlist title must never run under the numbers.
    let name_galley = painter.layout_no_wrap(
        name.to_owned(),
        egui::FontId::proportional(14.0),
        visuals.text_color(),
    );
    let name_width = (inner.width() - count_galley.size().x - SIDEBAR_NAME_COUNT_GAP).max(0.0);
    let name_clipped = name_galley.size().x > name_width;
    let name_clip = egui::Rect::from_min_max(
        egui::pos2(inner.left(), rect.top()),
        egui::pos2(inner.left() + name_width, rect.bottom()),
    );
    painter.with_clip_rect(name_clip).galley(
        egui::pos2(inner.left(), middle - 0.5 * name_galley.size().y),
        name_galley,
        visuals.text_color(),
    );
    if name_clipped {
        return response.on_hover_text(name);
    }
    response
}

/// One sidebar row wiring: painted full-width row (see [`sidebar_row`]),
/// click switches the central context, ПКМ offers delete (user
/// playlists only; the master is refused honestly), drops add tracks.
/// Built-in device views accept no drops and no ПКМ: the firmware owns
/// them. «Все треки» only on the master; the others show a count.
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
            let selected = *view == row_view;
            let row = sidebar_row(ui, selected, &playlist.name, &count_text);
            if row.clicked() {
                *view = row_view;
            }
            if menus_allowed && !is_view {
                let mut refused_master = false;
                row.context_menu(|ui| {
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
        let title = ui.add(
            egui::TextEdit::singleline(&mut fields.title).desired_width(320.0),
        );
        text_menu::attach_text_menu(&title);
    });
    ui.horizontal(|ui| {
        ui.label("Исполнитель:  ");
        let artist = ui.add(
            egui::TextEdit::singleline(&mut fields.artist).desired_width(320.0),
        );
        text_menu::attach_text_menu(&artist);
    });
    ui.horizontal(|ui| {
        ui.label("Альбом:       ");
        let album = ui.add(
            egui::TextEdit::singleline(&mut fields.album).desired_width(320.0),
        );
        text_menu::attach_text_menu(&album);
    });
    ui.horizontal(|ui| {
        ui.label("Альбом-артист:");
        let album_artist = ui.add(
            egui::TextEdit::singleline(&mut fields.album_artist)
                .desired_width(320.0),
        );
        text_menu::attach_text_menu(&album_artist);
    });
    ui.horizontal(|ui| {
        ui.label("Трек №:       ");
        let track_number = ui.add(
            egui::TextEdit::singleline(&mut fields.track_number)
                .desired_width(60.0),
        );
        text_menu::attach_text_menu(&track_number);
        ui.label("Диск №:");
        let disc_number = ui.add(
            egui::TextEdit::singleline(&mut fields.disc_number)
                .desired_width(60.0),
        );
        text_menu::attach_text_menu(&disc_number);
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
/// as production, header cells measured via allocated rects. Five
/// columns since round 6 П2 (checkbox + four data columns).
#[cfg(test)]
fn render_measured_table(ui: &mut egui::Ui) -> Vec<egui::Rect> {
    let rects: [std::rc::Rc<std::cell::RefCell<Option<egui::Rect>>>; 5] =
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

/// Outcome of one frame of the shared sudo-password modal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PasswordModal {
    /// Still open — keep showing it next frame.
    Open,
    /// The user confirmed (submit button, or Enter on a non-empty field).
    Confirmed,
    /// The user cancelled (Cancel button, Esc, backdrop click).
    Cancelled,
}

/// Stable id of the shared modal's password field (the autofocus
/// target), derived from the modal salt so each flow gets its own.
fn password_field_id(id_salt: &str) -> egui::Id {
    egui::Id::unique(id_salt).with("password_field")
}

/// Unified sudo-password modal (round 6.1) behind connect AND
/// disconnect: the field takes focus whenever nothing else holds it
/// (so it is focused the moment the modal opens — the search-box
/// pattern), Enter submits a non-empty password, Esc and the backdrop
/// click cancel, and the submit button stays disabled while the field
/// is empty. The password never leaves the app: the caller consumes
/// and clears it when the worker starts.
fn password_modal(
    ctx: &egui::Context,
    id_salt: &str,
    password: &mut String,
    heading: &str,
    body: &str,
    submit_label: &str,
) -> PasswordModal {
    let field_id = password_field_id(id_salt);
    let mut outcome = PasswordModal::Open;
    let response = egui::Modal::new(egui::Id::unique(id_salt)).show(ctx, |ui| {
        ui.set_width(440.0);
        ui.heading(heading);
        ui.add_space(4.0);
        ui.label(body);
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label("Пароль sudo:");
            let password_field = ui.add(
                egui::TextEdit::singleline(password)
                    .password(true)
                    .desired_width(220.0)
                    .id(field_id),
            );
            text_menu::attach_secret_text_menu(&password_field);
            if ctx.memory(|memory| memory.focused().is_none()) {
                ctx.memory_mut(|memory| memory.request_focus(field_id));
            }
            // Enter submits a non-empty password; an empty one keeps
            // the modal open — the same rule as the disabled button.
            // Checked at the input level (not via the field's
            // lost_focus) so the shortcut works regardless of which
            // widget egui considers focused inside the modal.
            let submit_now =
                ui.input(|input| input.key_pressed(egui::Key::Enter)) && !password.is_empty();
            if submit_now {
                outcome = PasswordModal::Confirmed;
                ui.close();
            }
        });
        ui.separator();
        ui.horizontal(|ui| {
            if ui.button("Отмена").clicked() {
                outcome = PasswordModal::Cancelled;
                ui.close();
            }
            let ready = !password.is_empty();
            if ui
                .add_enabled(ready, egui::Button::new(submit_label))
                .clicked()
            {
                outcome = PasswordModal::Confirmed;
                ui.close();
            }
        });
    });
    // Esc / backdrop close the modal without an in-closure decision —
    // that is a cancellation too.
    if response.should_close() && outcome == PasswordModal::Open {
        outcome = PasswordModal::Cancelled;
    }
    outcome
}

/// Worker-thread body: run the sudo mount sequence (`mkdir -p`, then
/// rw-mount) and report the mount point plus whether this app run is
/// now responsible for the safe unmount. Round 5.3: an already-mounted
/// target is skipped (idempotent connect). Batch F: such a mount is
/// ADOPTED (`mounted_by_us = true`) — the old «not ours, do not
/// unmount» semantics left mounts orphaned by a podlib restart or
/// crash mounted rw forever (live incident). Foreign mounts the owner
/// connected to deliberately are adopted too — the price of keeping
/// this simple; ejecting them on close is accepted by the owner.
/// Round 6.3: the one foreign mount that must NOT be adopted is a
/// read-only automount (udisks2 on HFS+): reads work, every write
/// fails, so it is unmounted first and the usual mkdir+mount runs.
/// The library is NOT opened here: `Box<dyn LibraryBackend>` is not
/// `Send`, so it is opened on the library worker thread instead (see
/// library.rs).
/// What must happen before (or instead of) the mkdir+rw-mount sequence,
/// decided from the /proc/mounts snapshot (rounds 5.3 / batch F / 6.3).
enum ConnectPlan {
    /// Target already carries the same source rw — skip the mount and
    /// adopt it (mounted_by_us=true so closing ejects it).
    AdoptMounted,
    /// Honest refusal: another device already owns the target.
    Conflict(String),
    /// Target free (or the foreign ro automount was just removed): run
    /// the mkdir+rw-mount steps as usual.
    Mount,
}

/// Mount-time branch of the connect flow: /proc/mounts decides whether
/// the target is already mounted (adopt — idempotent connect, round
/// 5.3), mounted read-only by the system (one extra sudo unmount first
/// — round 6.3), mounted from another device (honest refusal), or free.
fn resolve_connect_plan(
    password: &str,
    mount_path: &Path,
    block_dev: &Path,
) -> Result<ConnectPlan, ConnectError> {
    let mounts = std::fs::read_to_string("/proc/mounts")
        .map(|content| device::parse_proc_mounts_full(&content))
        .unwrap_or_default();
    match device::decide_mount_full(&mounts, block_dev, mount_path) {
        device::MountDecision::AlreadyMounted => Ok(ConnectPlan::AdoptMounted),
        device::MountDecision::ForeignReadOnly => {
            // One extra sudo step; on success the target is free and the
            // caller proceeds with the usual mkdir+mount.
            unmount_foreign_ro_volume(password, block_dev)?;
            Ok(ConnectPlan::Mount)
        }
        device::MountDecision::ConflictsWith(other) => Ok(ConnectPlan::Conflict(other)),
        device::MountDecision::Free => Ok(ConnectPlan::Mount),
    }
}

fn connect_device(
    password: String, // owned: dropped right after the last command (W3)
    steps: Vec<device::CommandSpec>,
    mount_path: PathBuf,
    block_dev: PathBuf,
) -> Result<(PathBuf, bool), ConnectError> {
    // WHY the password lives across the whole flow: every sudo step
    // (ro-unmount, mount sequence) consumes its own copy from stdin;
    // it is dropped only after the last step, never stored anywhere.
    match resolve_connect_plan(&password, &mount_path, &block_dev)? {
        ConnectPlan::AdoptMounted => {
            drop(password);
            // Batch F (adoption): connecting to an already-mounted
            // volume makes its safe unmount this app's responsibility.
            return Ok((mount_path, true));
        }
        ConnectPlan::Conflict(other) => {
            drop(password);
            return Err(ConnectError::Mount {
                stderr: format!(
                    "{mount_path_display} уже смонтирован с {other} — подключение отменено.",
                    mount_path_display = mount_path.display()
                ),
                error: MountError::AlreadyMounted,
            });
        }
        ConnectPlan::Mount => {}
    }
    for spec in &steps {
        let output =
            device::run_sudo_command(&password, spec).map_err(|error| ConnectError::Mount {
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

/// Round 6.3: removes the foreign read-only mount blocking the connect
/// target (`udisksctl unmount -b <dev>` under sudo, password already in
/// hand). Review M1: sudo auth failures classify as WrongPassword —
/// the same hint and retry flow as the mount loop below; other failures
/// keep the honest read-only message with the tool's stderr.
fn unmount_foreign_ro_volume(password: &str, block_dev: &Path) -> Result<(), ConnectError> {
    let spec = device::build_unmount_command(block_dev);
    let output =
        device::run_sudo_command(password, &spec).map_err(|error| ConnectError::Mount {
            stderr: String::new(),
            error: MountError::Io(error),
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        return Err(ConnectError::Mount {
            error: device::unmount_failure_class(&stderr),
            stderr,
        });
    }
    Ok(())
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
        // the app becomes idle (round 5.3) — through the shared password
        // modal (batch F), never by starting the sequence directly.
        self.maybe_deferred_close_eject();
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
        //    The width ceiling comes from device_panel_builder: a drag
        //    (or a persisted oversized state) can never eat the central
        //    panel below MIN_CENTRAL_PANEL_WIDTH (round 6.5).
        device_panel_builder(ui).show(ui, |ui| {
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
        self.maybe_set_artist_modal(&ctx);
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
    fn applied_prunes_dead_ids_from_selection_and_anchor() {
        // Review 🟢-5: after an applied operation the toolbar must not
        // count rows that no longer exist in the new snapshot.
        let (mut app, _dir) = app_with_open_library();
        let (tx, rx) = channel();
        {
            let library = app.library.as_mut().unwrap();
            library.pending = Some((PendingKind::RemoveTrack, rx));
            library.selected_tracks = vec![
                TrackId::from_bits(1),
                TrackId::from_bits(2),
                TrackId::from_bits(77), // removed by the operation
            ];
            // The anchor row vanished with the re-list: dropped instead
            // of surviving as a stale identity.
            library.selection_anchor = Some((2, TrackId::from_bits(77)));
        }
        tx.send(LibraryOutcome::Applied {
            snapshot: LibrarySnapshot {
                tracks: vec![sample_track(1, "A"), sample_track(2, "B")],
                playlists: Vec::new(),
            },
            summary: "Удалено".to_owned(),
        })
        .unwrap();
        app.drain_library();
        let library = app.library.as_ref().unwrap();
        assert_eq!(
            library.selected_tracks,
            vec![TrackId::from_bits(1), TrackId::from_bits(2)],
            "dead ids must leave the selection"
        );
        assert_eq!(
            library.selection_anchor, None,
            "a stale anchor must be dropped"
        );
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
        // Batch F: the generic hint now offers the safe retry instead of
        // pushing the user to reconnect.
        let hint = error.hint.unwrap();
        assert!(
            hint.contains("ещё раз") && hint.contains("безопасна"),
            "got: {hint}"
        );
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
            close_decision(true, false, false, false),
            CloseDecision::Allow,
            "idle window without our mount closes as usual"
        );
    }

    #[test]
    fn close_while_operation_active_blocks_and_asks() {
        assert_eq!(
            close_decision(true, true, false, false),
            CloseDecision::BlockWithConfirm,
            "mid-operation close must not kill the worker thread"
        );
    }

    /// Batch F: an idle close with a device mounted by this app must ask
    /// before closing — the round 5.3 eject needs the sudo password and
    /// must never run silently.
    #[test]
    fn close_while_idle_with_our_mount_blocks_and_asks() {
        assert_eq!(
            close_decision(true, false, false, true),
            CloseDecision::BlockWithConfirm,
            "our mount must be ejected before the window closes"
        );
        // Without the mount the same idle close stays a plain Allow.
        assert_eq!(
            close_decision(true, false, false, false),
            CloseDecision::Allow,
            "not-our-device close behaviour is unchanged"
        );
    }

    #[test]
    fn confirmed_close_passes_through_even_while_busy() {
        assert_eq!(
            close_decision(true, true, true, false),
            CloseDecision::Allow,
            "the user already chose to close: no confirm loop"
        );
        assert_eq!(
            close_decision(true, true, true, true),
            CloseDecision::Allow,
            "a confirmed close is not re-asked even with our mount"
        );
    }

    #[test]
    fn no_close_request_is_a_noop() {
        assert_eq!(close_decision(false, true, false, true), CloseDecision::Allow);
        assert_eq!(close_decision(false, false, true, true), CloseDecision::Allow);
    }

    /// Batch F: the idle+mounted case now reaches the confirm modal too,
    /// so its text must not claim an operation is in flight.
    #[test]
    fn close_confirm_texts_are_honest_for_idle_mount() {
        let (busy_title, _) = close_confirm_texts(true);
        let (idle_title, idle_body) = close_confirm_texts(false);
        assert_eq!(busy_title, "Идёт операция с библиотекой");
        assert_ne!(idle_title, busy_title, "idle close is not an operation");
        assert!(
            idle_body.contains("безопасно отключено"),
            "idle text must say the device is ejected, got: {idle_body}"
        );
        assert!(
            idle_body.contains("пароль"),
            "idle text must warn about the sudo password, got: {idle_body}"
        );
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

    // ---- sidebar vs central panel (round 6.5) ----

    /// Реплика живого прогона 6.2–6.4: разделитель сайдбара оттянут до
    /// правого края окна — egui клампит драг только по экрану и не
    /// резервирует место под центральную панель, поэтому таблица
    /// схлопывается в ноль. Размер панели персистится между кадрами,
    /// поэтому «оттянули в прошлой сессии» = засеянный PanelState с
    /// шириной всего окна. Контракт раунда 6.5: центральная панель
    /// держит минимум MIN_CENTRAL_PANEL_WIDTH.
    #[test]
    fn fully_dragged_sidebar_cannot_collapse_the_central_panel() {
        egui::__run_test_ui(|ui| {
            ui.style_mut().animation_time = 0.0;
            let screen_w = ui.input(|input| input.viewport_rect().width());
            let panel_id = ui.scope_id().with_salt(egui::IdSalt::new("device_panel"));
            ui.ctx().data_mut(|data| {
                data.insert_persisted(
                    panel_id,
                    egui::PanelState {
                        outer_rect: egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(screen_w, 600.0),
                        ),
                    },
                );
            });
            egui::Panel::top("app_bar").default_size(48.0).show(ui, |ui| {
                ui.label("podlib");
            });
            let sidebar = device_panel_builder(ui).show(ui, |ui| {
                // Realistic content: sidebar rows span the full panel
                // width (see sidebar_row_spans_the_full_panel_width), so
                // the panel paints at its full reserved size. Empty
                // content would let egui shrink the frame to the row
                // minimum and hide the whole regression.
                ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), 10.0),
                    egui::Sense::hover(),
                );
            });
            egui::Panel::bottom("status_bar").default_size(34.0).show(ui, |_ui| {});
            let central = egui::CentralPanel::default().show(ui, |ui| {
                ui.label("central");
            });
            let sidebar_rect = sidebar.response.rect;
            let central_rect = central.response.rect;
            assert!(
                sidebar_rect.right() <= central_rect.left() + 0.5,
                "sidebar ({sidebar_rect:?}) overlaps the central panel ({central_rect:?})"
            );
            assert!(
                central_rect.width() >= MIN_CENTRAL_PANEL_WIDTH,
                "a fully dragged sidebar left the central panel only {} px \
                 (minimum is {MIN_CENTRAL_PANEL_WIDTH})",
                central_rect.width()
            );
        });
    }

    /// Sweep ширины окна 900/800/700/500: панели не пересекаются, а
    /// чекбокс и название таблицы (первые две колонки) остаются внутри
    /// центральной панели. Правые колонки на узких окнах деградируют
    /// клипом (clip(true)) — это осознанное решение раунда 6.5.
    #[test]
    fn sidebar_and_central_panel_stay_disjoint_across_window_widths() {
        for width in [900.0, 800.0, 700.0, 500.0] {
            let ctx = egui::Context::default();
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(width, 600.0),
                    )),
                    ..Default::default()
                },
                |ui| {
                    ui.style_mut().animation_time = 0.0;
                    egui::Panel::top("app_bar").default_size(48.0).show(ui, |_ui| {});
                    let sidebar = device_panel_builder(ui).show(ui, |ui| {
                        // Same full-width content contract as the
                        // fully-dragged test above: the panel must paint
                        // at its reserved size for disjointness to mean
                        // anything.
                        ui.allocate_exact_size(
                            egui::vec2(ui.available_width(), 10.0),
                            egui::Sense::hover(),
                        );
                    });
                    egui::Panel::bottom("status_bar").default_size(34.0).show(ui, |_ui| {});
                    let mut header = Vec::new();
                    let central = egui::CentralPanel::default().show(ui, |ui| {
                        header = render_measured_table(ui);
                    });
                    let sidebar_rect = sidebar.response.rect;
                    let central_rect = central.response.rect;
                    assert!(
                        sidebar_rect.right() <= central_rect.left() + 0.5,
                        "at {width}px: sidebar ({sidebar_rect:?}) overlaps the \
                         central panel ({central_rect:?})"
                    );
                    for (name, rect) in [("чекбокс", header[0]), ("название", header[1])] {
                        assert!(
                            rect.right() <= central_rect.right() + 0.5
                                && rect.left() >= central_rect.left() - 0.5,
                            "at {width}px: колонка {name} ({rect:?}) вышла за \
                             пределы центральной панели ({central_rect:?})"
                        );
                    }
                },
            );
            output.textures_delta.clear();
        }
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
            TagRow { title: "Глава 1".to_owned(), original_title: "Skazka - 1".to_owned(), track_number: 1 },
            TagRow { title: String::new(), original_title: "Skazka - 2".to_owned(), track_number: 2 }, // empty → stem
        ];
        // Empty book title is rejected with the KIND-aware near-field
        // text (nit-8): books say «книги», music says «альбома».
        assert_eq!(
            apply_tag_review(&drafts, "   ", "Пушкин", &rows, "Укажите название книги")
                .unwrap_err(),
            "Укажите название книги"
        );
        assert_eq!(
            apply_tag_review(&drafts, "  ", "Пушкин", &rows, "Укажите название альбома")
                .unwrap_err(),
            "Укажите название альбома"
        );
        let applied =
            apply_tag_review(&drafts, "Сказка о рыбаке", "Пушкин", &rows, "ignored").unwrap();
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
        let no_author = apply_tag_review(&drafts, "Книга", "", &rows, "x").unwrap();
        assert_eq!(no_author[0].album_artist, None);
    }

    /// The author lands in `album_artist` AND in `artist` when the draft
    /// has no own artist: the vendor's display_artist() writes
    /// item.artist only, so without the propagation the track never
    /// shows up in the «Песни» list (round 6.4).
    #[test]
    fn tag_review_propagates_author_to_artist_when_artist_is_empty() {
        let drafts = vec![
            tag_draft("Ch1", None, None, 8),
            // A pre-existing artist wins: propagation fills only blanks.
            tag_draft("Ch2", Some("Свой артист"), None, 8),
        ];
        let rows = vec![
            TagRow { title: "Глава 1".to_owned(), original_title: "Ch1".to_owned(), track_number: 1 },
            TagRow { title: "Глава 2".to_owned(), original_title: "Ch2".to_owned(), track_number: 2 },
        ];
        let applied = apply_tag_review(&drafts, "Книга", "Котэ", &rows, "x").unwrap();
        assert_eq!(applied[0].artist.as_deref(), Some("Котэ"));
        assert_eq!(applied[0].album_artist.as_deref(), Some("Котэ"));
        assert_eq!(applied[1].artist.as_deref(), Some("Свой артист"));
        assert_eq!(applied[1].album_artist.as_deref(), Some("Котэ"));
        // No author at all — nothing to propagate.
        let no_author = apply_tag_review(&drafts, "Книга", "", &rows, "x").unwrap();
        assert_eq!(no_author[0].artist, None);
    }

    // ---- tail nit-8: kind-aware texts ----

    #[test]
    fn tag_review_texts_speak_books_for_books_and_music_for_music() {
        // Books: the audiobook BIT decides (combined bits stay book).
        let books = tag_review_texts(8);
        assert_eq!(books.title_field, "Название книги*");
        assert_eq!(books.author_field, "Автор");
        assert_eq!(books.empty_title, "Укажите название книги");
        assert_eq!(books.add_button, "Добавить книгу");
        assert_eq!(tag_review_texts(9).title_field, "Название книги*");

        let music = tag_review_texts(1);
        assert_eq!(music.title_field, "Альбом");
        assert_eq!(music.author_field, "Исполнитель");
        assert_eq!(music.empty_title, "Укажите название альбома");
        assert_eq!(music.add_button, "Добавить музыку");
        assert!(music.subject.contains("музыки"));
    }

    // ---- tail nit-9: the prefix toggle spares manual edits ----

    #[test]
    fn prefix_toggle_rederives_only_untouched_rows() {
        let prefix = "Skazka";
        let mut rows = vec![
            TagRow {
                title: "Skazka - 1".to_owned(),
                original_title: "Skazka - 1".to_owned(),
                track_number: 1,
            },
            // The user edited this one before touching the checkbox.
            TagRow {
                title: "Моё название".to_owned(),
                original_title: "Skazka - 2".to_owned(),
                track_number: 2,
            },
        ];
        // OFF: nothing was stripped yet — every row already shows its
        // automatic (original) value, so nothing changes.
        retoggle_prefix_titles(&mut rows, prefix, false);
        assert_eq!(rows[0].title, "Skazka - 1");
        assert_eq!(rows[1].title, "Моё название");
        // ON: untouched rows strip; the manual edit survives.
        retoggle_prefix_titles(&mut rows, prefix, true);
        assert_eq!(rows[0].title, "1", "stem minus the «Skazka - » prefix");
        assert_eq!(rows[1].title, "Моё название", "manual edit kept");
        // OFF again: the untouched row returns to the stem; the manual
        // edit is still there.
        retoggle_prefix_titles(&mut rows, prefix, false);
        assert_eq!(rows[0].title, "Skazka - 1");
        assert_eq!(rows[1].title, "Моё название");
    }

    #[test]
    fn prefix_toggle_restores_a_previously_stripped_row() {
        let prefix = "Book";
        let mut rows = vec![TagRow {
            title: "5".to_owned(),
            original_title: "Book - 5".to_owned(),
            track_number: 5,
        }];
        // OFF: the stripped automatic value is the "previous auto", so
        // it re-derives to the full stem.
        retoggle_prefix_titles(&mut rows, prefix, false);
        assert_eq!(rows[0].title, "Book - 5");
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
        // Round 6.1 B1: dispatched drafts must survive the duration
        // probe, so the drop sources are real audio (ffmpeg AAC), not
        // garbage bytes.
        let mut sources = Vec::new();
        for name in ["01", "02"] {
            let path = dir.path().join(format!("book/{name}.m4a"));
            let generated = std::process::Command::new("ffmpeg")
                .args([
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440:duration=0.2",
                    "-c:a",
                    "aac",
                    &path.display().to_string(),
                ])
                .output()
                .expect("run ffmpeg");
            assert!(
                generated.status.success(),
                "fixture ffmpeg failed: {}",
                String::from_utf8_lossy(&generated.stderr)
            );
            sources.push(path);
        }
        let first = sources[0].clone();
        let second = sources[1].clone();

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

    // ---- round 6.1 F3: shared sudo-password modal ----

    /// Runs one headless frame through both password modals (the same
    /// order as the production `ui()`), feeding extra input events.
    fn password_modal_frame(app: &mut PodlibApp, ctx: &egui::Context, events: Vec<egui::Event>) {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 800.0),
            )),
            events,
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| {
            ui.style_mut().animation_time = 0.0;
            app.maybe_mount_modal(&ui.ctx().clone());
            app.maybe_eject_modal(&ui.ctx().clone());
        });
        output.textures_delta.clear();
    }

    fn enter_pressed() -> egui::Event {
        egui::Event::Key {
            key: egui::Key::Enter,
            pressed: true,
            physical_key: None,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        }
    }

    fn escape_pressed() -> egui::Event {
        egui::Event::Key {
            key: egui::Key::Escape,
            pressed: true,
            physical_key: None,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        }
    }

    #[test]
    fn password_modal_autofocuses_the_field() {
        let mut app = test_app();
        app.confirm_mount = true;
        let ctx = egui::Context::default();
        password_modal_frame(&mut app, &ctx, Vec::new());
        assert_eq!(
            ctx.memory(|memory| memory.focused()),
            Some(password_field_id("confirm_mount")),
            "the password field must take focus the moment the modal opens"
        );
    }

    #[test]
    fn eject_modal_autofocuses_its_own_field() {
        let mut app = test_app();
        app.confirm_eject = true;
        let ctx = egui::Context::default();
        password_modal_frame(&mut app, &ctx, Vec::new());
        assert_eq!(
            ctx.memory(|memory| memory.focused()),
            Some(password_field_id("confirm_eject")),
            "each flow focuses its own field, not the other flow's"
        );
        assert!(!app.start_eject && app.confirm_eject, "still open");
    }

    #[test]
    fn password_modal_enter_submits_non_empty_password() {
        let mut app = test_app();
        app.confirm_mount = true;
        let ctx = egui::Context::default();
        password_modal_frame(&mut app, &ctx, Vec::new()); // open + autofocus
        app.password = "sudo-pass".to_owned();
        password_modal_frame(&mut app, &ctx, vec![enter_pressed()]);
        assert!(!app.confirm_mount, "modal must close on Enter");
        assert!(app.start_mount, "Enter on a non-empty password confirms");
    }

    #[test]
    fn password_modal_enter_on_empty_password_keeps_it_open() {
        let mut app = test_app();
        app.confirm_mount = true;
        let ctx = egui::Context::default();
        password_modal_frame(&mut app, &ctx, Vec::new());
        password_modal_frame(&mut app, &ctx, vec![enter_pressed()]); // empty field
        assert!(app.confirm_mount, "an empty password must not confirm");
        assert!(!app.start_mount);
    }

    #[test]
    fn password_modal_esc_cancels() {
        let mut app = test_app();
        app.confirm_eject = true;
        app.password = "x".to_owned();
        let ctx = egui::Context::default();
        password_modal_frame(&mut app, &ctx, Vec::new());
        password_modal_frame(&mut app, &ctx, vec![escape_pressed()]);
        assert!(!app.confirm_eject, "Esc closes the modal");
        assert!(!app.start_eject, "Esc does not start the eject");
    }

    // ---- batch F: EVERY eject collects the password through the modal ----

    /// Live bug: a confirmed window close (round 5.3, mounted by us)
    /// started the eject directly, bypassing the password modal, and the
    /// first sudo step failed with an empty password. The close path must
    /// open the shared modal instead.
    #[test]
    fn close_confirm_with_mount_opens_password_modal_before_eject() {
        let (mut app, _dir) = app_with_open_library();
        app.mounted_by_us = true;
        app.confirm_close = true;
        let ctx = egui::Context::default();
        confirm_modal_frame(&mut app, &ctx, Vec::new()); // open
        confirm_modal_frame(&mut app, &ctx, vec![enter_pressed()]);
        assert!(!app.confirm_close, "modal must close on Enter");
        assert!(
            app.confirm_eject,
            "the eject must ask for the password (modal open)"
        );
        assert!(
            !app.start_eject,
            "the eject must not start before the password is entered"
        );
        assert!(
            app.pending_close_after_eject,
            "the window still closes after the eject lands"
        );
    }

    /// Cancelling the eject password modal also cancels the close that
    /// asked for it: the window stays, nothing restarts.
    #[test]
    fn cancel_eject_password_modal_cancels_pending_close() {
        let mut app = test_app();
        app.pending_close_after_eject = true;
        app.confirm_eject = true;
        let ctx = egui::Context::default();
        password_modal_frame(&mut app, &ctx, Vec::new()); // open
        password_modal_frame(&mut app, &ctx, vec![escape_pressed()]);
        assert!(!app.confirm_eject, "Esc closes the modal");
        assert!(!app.start_eject, "Esc does not start the eject");
        assert!(
            !app.pending_close_after_eject,
            "a cancelled eject must not keep the window waiting to close"
        );
    }

    /// Live bug, second half: a failed eject kept `pending_close_after_eject`
    /// set, so the deferred path restarted the (empty-password) eject every
    /// idle frame. The failure must cancel the pending close.
    #[test]
    fn ejected_error_cancels_pending_close() {
        let (mut app, _dir) = app_with_open_library();
        app.library = None; // spawn_eject closes the library before the steps
        app.connected = Some(ConnectedDevice {
            block_dev: PathBuf::from("/dev/test"),
            label: "test".to_owned(),
        });
        app.pending_close_after_eject = true;
        let (tx, rx) = channel();
        tx.send(DeviceEvent::Ejected(Err("sudo failed".to_owned()))).unwrap();
        app.rx = Some(rx);
        app.drain_events();
        assert!(
            !app.pending_close_after_eject,
            "a failed eject must cancel the pending window close"
        );
        assert!(
            !app.start_eject,
            "the deferred path must not immediately restart the eject"
        );
    }

    // ---- batch F: a failed eject must allow a retry ----

    /// Live bug: after a failed eject the library worker is closed
    /// (`library = None`), and `library_idle()` returned false for None —
    /// so «Безопасно отключить» stayed disabled forever. No library means
    /// nothing is in flight.
    #[test]
    fn library_idle_is_true_without_a_library() {
        let app = test_app();
        assert!(app.library.is_none());
        assert!(
            app.library_idle(),
            "no library means nothing is in flight"
        );
    }

    #[test]
    fn library_idle_with_pending_operation_is_false() {
        let (mut app, _dir) = app_with_open_library();
        let (_tx, rx) = channel();
        app.library.as_mut().unwrap().pending = Some((PendingKind::Refresh, rx));
        assert!(!app.library_idle(), "an in-flight operation is not idle");
    }

    /// Live run: wrong sudo password → eject fails → the button must be
    /// ready again (the library is already closed) and the hint must
    /// explain the retry is safe and point at the layout/Caps Lock.
    #[test]
    fn wrong_password_eject_failure_offers_retry() {
        let (mut app, _dir) = app_with_open_library();
        app.library = None; // spawn_eject closed it before the sudo steps
        app.connected = Some(ConnectedDevice {
            block_dev: PathBuf::from("/dev/test"),
            label: "test".to_owned(),
        });
        let (tx, rx) = channel();
        tx.send(DeviceEvent::Ejected(Err(
            "sudo: 1 incorrect password attempt".to_owned(),
        )))
        .unwrap();
        app.rx = Some(rx);
        app.drain_events();
        let ready = !app.busy && app.library_idle(); // app_bar gating
        assert!(ready, "the eject button must be available again");
        let error = app.error.as_ref().expect("failure must be shown");
        let hint = error.hint.as_ref().expect("retry hint must be shown");
        assert!(
            hint.contains("раскладку"),
            "wrong-password hint must mention the layout, got: {hint}"
        );
        assert!(
            hint.contains("ещё раз"),
            "wrong-password hint must offer a retry, got: {hint}"
        );
    }

    /// A non-password eject failure keeps the honest «volume may still be
    /// mounted» wording but must also offer the safe retry.
    #[test]
    fn other_eject_failure_hint_mentions_retry() {
        let (mut app, _dir) = app_with_open_library();
        app.library = None;
        app.connected = Some(ConnectedDevice {
            block_dev: PathBuf::from("/dev/test"),
            label: "test".to_owned(),
        });
        let (tx, rx) = channel();
        tx.send(DeviceEvent::Ejected(Err("umount: target is busy".to_owned())))
            .unwrap();
        app.rx = Some(rx);
        app.drain_events();
        let error = app.error.as_ref().expect("failure must be shown");
        let hint = error.hint.as_ref().expect("retry hint must be shown");
        assert!(
            hint.contains("ещё раз") && hint.contains("безопасна"),
            "generic hint must offer the safe retry, got: {hint}"
        );
        assert!(
            !hint.contains("раскладку"),
            "layout wording is only for sudo-auth failures, got: {hint}"
        );
    }

    /// Full retry path: after the failed eject (library None) the owner
    /// presses «Безопасно отключить» again, enters the password, and the
    /// second eject starts.
    #[test]
    fn failed_eject_allows_full_retry() {
        let (mut app, _dir) = app_with_open_library();
        app.library = None;
        app.connected = Some(ConnectedDevice {
            block_dev: PathBuf::from("/dev/test"),
            label: "test".to_owned(),
        });
        let (tx, rx) = channel();
        tx.send(DeviceEvent::Ejected(Err(
            "sudo: 1 incorrect password attempt".to_owned(),
        )))
        .unwrap();
        app.rx = Some(rx);
        app.drain_events();
        // The app-bar press: only possible when the button is enabled.
        assert!(
            !app.busy && app.library_idle(),
            "the eject button must be enabled for the retry"
        );
        app.password.clear();
        app.confirm_eject = true;
        let ctx = egui::Context::default();
        password_modal_frame(&mut app, &ctx, Vec::new()); // open + autofocus
        app.password = "right-pass".to_owned();
        password_modal_frame(&mut app, &ctx, vec![enter_pressed()]);
        assert!(
            app.start_eject,
            "the retried eject must start from the password modal"
        );
        assert!(!app.confirm_eject, "the modal closed on confirm");
    }

    /// The deferred path (close confirmed while busy) opens the password
    /// modal when the app goes idle; repeated frames keep the modal open
    /// without ever starting the eject behind the user's back.
    #[test]
    fn deferred_close_eject_goes_through_password_modal() {
        let (mut app, _dir) = app_with_open_library();
        app.pending_close_after_eject = true;
        let ctx = egui::Context::default();
        // Frame 1: the app goes idle → the deferred path opens the modal.
        app.maybe_deferred_close_eject();
        password_modal_frame(&mut app, &ctx, Vec::new());
        assert!(app.confirm_eject, "idle → the password modal opens");
        assert!(!app.start_eject, "no direct eject start");
        assert!(app.pending_close_after_eject, "the close is still pending");
        // Frame 2: the deferred path fires again while the modal is up —
        // it must stay a no-op, not duplicate or start anything.
        app.maybe_deferred_close_eject();
        password_modal_frame(&mut app, &ctx, Vec::new());
        assert!(app.confirm_eject, "the modal is still open");
        assert!(!app.start_eject, "still no direct eject start");
    }

    /// Live bug: closing the window by a WM hotkey while idle (no
    /// operation in flight) with a device mounted by this app closed
    /// instantly — no confirm, no eject, the volume stayed mounted rw.
    /// The close guard must ask first, and the confirm must lead into
    /// the password modal (batch F), never a direct eject.
    #[test]
    fn idle_close_with_our_mount_asks_and_ejects_through_password() {
        let (mut app, _dir) = app_with_open_library();
        app.mounted_by_us = true;
        let ctx = egui::Context::default();
        // Frame 1: the WM close request arrives while the app is idle.
        let mut viewports = egui::ViewportIdMap::default();
        viewports.insert(
            egui::ViewportId::ROOT,
            egui::ViewportInfo {
                events: vec![egui::ViewportEvent::Close],
                ..Default::default()
            },
        );
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 800.0),
            )),
            viewports,
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| {
            ui.style_mut().animation_time = 0.0;
            app.guard_close_request(ui);
            let ctx = ui.ctx().clone();
            app.maybe_close_confirm_modal(&ctx);
        });
        output.textures_delta.clear();
        let cancelled = output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .is_some_and(|viewport| {
                viewport.commands.contains(&egui::ViewportCommand::CancelClose)
            });
        assert!(
            cancelled,
            "the close must be cancelled this frame, or eframe exits"
        );
        assert!(app.confirm_close, "idle close with our mount must ask");
        assert!(!app.force_close, "the window must not close silently");
        assert!(
            !app.start_eject,
            "no eject without the user's password"
        );
        // Frame 2: the user confirms — the eject goes through the
        // password modal (batch F), the window stays pending.
        confirm_modal_frame(&mut app, &ctx, vec![enter_pressed()]);
        assert!(!app.confirm_close, "modal must close on Enter");
        assert!(
            app.confirm_eject,
            "confirming the close opens the password modal"
        );
        assert!(app.pending_close_after_eject, "the window waits for the eject");
        assert!(!app.start_eject, "the eject still waits for the password");
    }

    // ---- round 6.2 D2: Enter in the confirm modals ----

    /// Runs one headless frame through the confirm modals (the same
    /// order as the production `ui()`), feeding extra input events.
    fn confirm_modal_frame(app: &mut PodlibApp, ctx: &egui::Context, events: Vec<egui::Event>) {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 800.0),
            )),
            events,
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| {
            ui.style_mut().animation_time = 0.0;
            let ctx = ui.ctx().clone();
            app.maybe_close_confirm_modal(&ctx);
            app.maybe_delete_track_modal(&ctx);
            app.maybe_delete_playlist_modal(&ctx);
        });
        output.textures_delta.clear();
    }

    #[test]
    fn close_confirm_modal_enter_confirms() {
        let mut app = test_app();
        app.confirm_close = true;
        let ctx = egui::Context::default();
        confirm_modal_frame(&mut app, &ctx, Vec::new()); // open
        assert!(app.confirm_close, "still open before Enter");
        confirm_modal_frame(&mut app, &ctx, vec![enter_pressed()]);
        assert!(!app.confirm_close, "modal must close on Enter");
        assert!(app.force_close, "Enter confirms the close (no own mount)");
    }

    #[test]
    fn delete_track_modal_enter_confirms() {
        let (mut app, _dir) = app_with_open_library();
        let track_id = app.library.as_ref().unwrap().snapshot.tracks[0].id;
        app.library.as_mut().unwrap().confirm_delete_track = Some(vec![track_id]);
        let ctx = egui::Context::default();
        confirm_modal_frame(&mut app, &ctx, Vec::new()); // open
        confirm_modal_frame(&mut app, &ctx, vec![enter_pressed()]);
        let library = app.library.as_ref().unwrap();
        assert!(
            library.confirm_delete_track.is_none(),
            "modal must close on Enter"
        );
        assert!(
            matches!(library.pending, Some((PendingKind::RemoveTrack, _))),
            "Enter confirms the primary action «Удалить»"
        );
        assert!(library.delete_queue.is_empty(), "single track: no queue");
    }

    #[test]
    fn delete_track_modal_esc_cancels() {
        // Esc already worked via should_close; the Enter addition must
        // not break it.
        let (mut app, _dir) = app_with_open_library();
        let track_id = app.library.as_ref().unwrap().snapshot.tracks[0].id;
        app.library.as_mut().unwrap().confirm_delete_track = Some(vec![track_id]);
        let ctx = egui::Context::default();
        confirm_modal_frame(&mut app, &ctx, Vec::new()); // open
        confirm_modal_frame(&mut app, &ctx, vec![escape_pressed()]);
        let library = app.library.as_ref().unwrap();
        assert!(library.confirm_delete_track.is_none(), "Esc closes");
        assert!(library.pending.is_none(), "Esc dispatches nothing");
    }

    #[test]
    fn delete_playlist_modal_enter_confirms() {
        let (mut app, _dir) = app_with_open_library();
        app.library.as_mut().unwrap().confirm_delete_playlist = Some(PlaylistId::from_bits(1));
        let ctx = egui::Context::default();
        confirm_modal_frame(&mut app, &ctx, Vec::new()); // open
        confirm_modal_frame(&mut app, &ctx, vec![enter_pressed()]);
        let library = app.library.as_ref().unwrap();
        assert!(
            library.confirm_delete_playlist.is_none(),
            "modal must close on Enter"
        );
        assert!(
            matches!(library.pending, Some((PendingKind::DeletePlaylist, _))),
            "Enter confirms the primary action «Удалить»"
        );
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
        assert_eq!(groups[0].name, "Сказки");
        assert_eq!(groups[0].author.as_deref(), Some("Народ"));
        assert_eq!(
            groups[0].track_ids,
            [TrackId::from_bits(1), TrackId::from_bits(2)]
        );
        // A blank album is its own group.
        assert_eq!(groups[1].name, "Без альбома");
        assert_eq!(groups[1].author.as_deref(), Some("Певец"));
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
        let albums: Vec<&str> = groups.iter().map(|g| g.name.as_str()).collect();
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
        assert_eq!(groups[0].author.as_deref(), Some("Неизвестный автор"));
    }

    #[test]
    fn no_album_group_pins_last_after_real_books() {
        let tracks = vec![
            book_track(1, "Файл", Some("Ясно"), None, None, 1, 1, 60_000),
            book_track(2, "Файл", None, None, None, 1, 1, 60_000),
        ];
        let groups = group_books(&tracks);
        let albums: Vec<&str> = groups.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(albums, ["Ясно", "Без альбома"], "the catch-all group goes last");
    }

    #[test]
    fn book_without_any_artist_is_unknown() {
        let tracks = vec![book_track(1, "Файл", None, None, None, 1, 1, 60_000)];
        let groups = group_books(&tracks);
        assert_eq!(groups[0].name, "Без альбома");
        assert_eq!(groups[0].author.as_deref(), Some("Неизвестный автор"));
    }

    // ---- music grouping: albums and artists (round 6 П3) ----

    #[test]
    fn albums_group_like_books_but_honest_about_the_author() {
        let tracks = vec![
            book_track(1, "Трек", Some("Заря"), Some("Группа"), None, 1, 2, 60_000),
            book_track(2, "Трек", Some("заря"), Some("  "), Some("Кто-то"), 1, 1, 60_000),
            book_track(3, "Сингл", None, None, None, 1, 1, 30_000),
        ];
        let groups = group_by_album(&tracks);
        let names: Vec<&str> = groups.iter().map(|g| g.name.as_str()).collect();
        // Case-insensitive SORT; buckets stay exact (round-5 books
        // semantics: tag-case variants are separate entries).
        assert_eq!(names, ["Заря", "заря", "Без альбома"]);
        // Member order inside the album: track #2 (no 1) before #1.
        assert_eq!(groups[0].track_ids, [TrackId::from_bits(1)]);
        assert_eq!(groups[1].track_ids, [TrackId::from_bits(2)]);
        // Author: first non-empty album_artist in file order.
        assert_eq!(groups[0].author.as_deref(), Some("Группа"));
        // An album without any artist shows NO author segment (books
        // use the «Неизвестный автор» placeholder instead).
        assert_eq!(groups[2].author, None);
    }

    #[test]
    fn artists_group_by_album_artist_then_artist() {
        let tracks = vec![
            book_track(1, "Трек", Some("Альбом"), Some("Би-2"), None, 1, 1, 60_000),
            book_track(2, "Трек", Some("Альбом"), None, Some("Би-2"), 1, 2, 60_000),
            book_track(3, "Гость", Some("Сборник"), Some("Сплин"), None, 1, 1, 60_000),
            book_track(4, "Без тегов", None, None, None, 1, 1, 30_000),
        ];
        let groups = group_by_artist(&tracks);
        let names: Vec<&str> = groups.iter().map(|g| g.name.as_str()).collect();
        // Case-insensitive; album_artist outranks track artist; the
        // «Неизвестный исполнитель» catch-all pins last.
        assert_eq!(names, ["Би-2", "Сплин", "Неизвестный исполнитель"]);
        assert_eq!(
            groups[0].track_ids,
            [TrackId::from_bits(1), TrackId::from_bits(2)],
            "the artist's files stay in one group"
        );
        // The name IS the artist — no author segment.
        assert_eq!(groups[0].author, None);
    }

    #[test]
    fn artist_members_order_by_album_then_file() {
        let tracks = vec![
            book_track(1, "Позже", Some("Альбом Б"), None, Some("Рокер"), 1, 1, 60_000),
            book_track(2, "Раньше", Some("альбом а"), None, Some("Рокер"), 1, 1, 60_000),
            book_track(3, "Азбука", Some("Альбом Б"), None, Some("Рокер"), 1, 1, 60_000),
        ];
        let groups = group_by_artist(&tracks);
        assert_eq!(groups.len(), 1);
        // Tiny discography: albums case-insensitively first («альбом
        // а» < «Альбом Б»), then the file order inside each album
        // (equal disc+number → title ci: «Азбука» < «Позже»).
        assert_eq!(
            groups[0].track_ids,
            [TrackId::from_bits(2), TrackId::from_bits(3), TrackId::from_bits(1)]
        );
    }

    #[test]
    fn section_modes_per_builtin_view() {
        assert_eq!(
            section_modes(4),
            [SectionMode::List, SectionMode::Albums, SectionMode::Artists]
        );
        assert_eq!(section_modes(5), [SectionMode::List, SectionMode::Books]);
        // Any other section (and master/playlists) has only the flat
        // list — the switcher is not rendered there.
        assert_eq!(section_modes(2), [SectionMode::List]);
        assert_eq!(section_modes(0), [SectionMode::List]);
    }

    #[test]
    fn switcher_labels_are_owner_facing_russian() {
        assert_eq!(SectionMode::List.label(), "Список");
        assert_eq!(SectionMode::Albums.label(), "Альбомы");
        assert_eq!(SectionMode::Artists.label(), "Исполнители");
        assert_eq!(SectionMode::Books.label(), "Книги");
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

    // ---- checkbox multi-select (round 6 П2) ----

    #[test]
    fn select_all_states_follow_visible_coverage() {
        let visible = ids(&[1, 2, 3]);
        assert_eq!(select_all_state(&ids(&[]), &visible), SelectAllState::None);
        assert_eq!(select_all_state(&ids(&[1]), &visible), SelectAllState::Some);
        assert_eq!(
            select_all_state(&ids(&[1, 2, 3]), &visible),
            SelectAllState::All
        );
        // A selection that covers nothing visible stays unchecked.
        assert_eq!(select_all_state(&ids(&[9]), &visible), SelectAllState::None);
        // An empty view has nothing to select.
        assert_eq!(select_all_state(&ids(&[1]), &ids(&[])), SelectAllState::None);
    }

    #[test]
    fn toggle_all_on_partial_selection_selects_everything() {
        let visible = ids(&[1, 2, 3]);
        let result = apply_select_all(&ids(&[1]), &visible, true);
        assert_eq!(result, ids(&[1, 2, 3]), "partial → all");
    }

    #[test]
    fn unchecking_all_keeps_rows_outside_the_view() {
        let visible = ids(&[1, 2, 3]);
        let selected = ids(&[1, 2, 3, 9]); // 9 lives in another view
        let result = apply_select_all(&selected, &visible, false);
        assert_eq!(result, ids(&[9]));
    }

    #[test]
    fn selecting_all_twice_does_not_duplicate() {
        let visible = ids(&[1, 2]);
        let result = apply_select_all(&ids(&[1, 2]), &visible, true);
        assert_eq!(result, ids(&[1, 2]));
    }

    #[test]
    fn checkbox_toggle_is_an_explicit_flip() {
        let selected = ids(&[1, 3]);
        assert_eq!(
            toggle_selected(&selected, TrackId::from_bits(5)),
            ids(&[1, 3, 5])
        );
        assert_eq!(
            toggle_selected(&selected, TrackId::from_bits(3)),
            ids(&[1])
        );
    }

    // ---- tail nit-10: shift anchor is identity-checked ----

    #[test]
    fn anchor_survives_only_while_the_row_identity_matches() {
        let visible = ids(&[10, 20, 30]);
        // Same index, same row → valid.
        assert_eq!(valid_anchor(Some((1, TrackId::from_bits(20))), &visible), Some(1));
        // The list shrank → out of bounds → invalid.
        assert_eq!(valid_anchor(Some((3, TrackId::from_bits(99))), &visible), None);
        // A re-list put a DIFFERENT row at the anchor index: the old
        // code shifted from here; now the anchor is dropped.
        assert_eq!(valid_anchor(Some((1, TrackId::from_bits(99))), &visible), None);
        // No anchor at all.
        assert_eq!(valid_anchor(None, &visible), None);
    }

    #[test]
    fn shift_from_a_relisted_anchor_degenerates_to_plain_click() {
        let visible = ids(&[10, 20, 30]);
        // The anchor row (id 20) vanished in a re-list; index 1 now
        // holds id 99. The identity check drops the anchor, so the
        // shift click acts like a plain click on the row (the old
        // boundary-only check would select a foreign range).
        let anchor = valid_anchor(Some((1, TrackId::from_bits(99))), &visible);
        let (selected, new_anchor) = apply_row_click(&[], anchor, 2, false, true);
        assert_eq!(selected, [2]);
        assert_eq!(new_anchor, Some(2));
    }

    // ---- selection resolution (review 🟡-3: extracted from
    // library_view as a pure function) ----

    #[test]
    fn resolution_plain_click_from_empty_selection_targets_the_row() {
        let visible = ids(&[10, 20, 30]);
        let update = resolve_selection_actions(
            &ids(&[]),
            None,
            None,
            &visible,
            Some((1, false, false)),
            None,
            None,
        );
        assert_eq!(update.selected_tracks, ids(&[20]));
        assert_eq!(update.selection_anchor, Some((1, TrackId::from_bits(20))));
        assert_eq!(update.selected_track, Some(TrackId::from_bits(20)));
    }

    #[test]
    fn resolution_shift_degenerates_when_the_anchor_row_is_gone() {
        // The anchor identity (id 99 claimed at index 1) no longer
        // matches the visible list: the shift click must act as a plain
        // click, not select a foreign range.
        let visible = ids(&[10, 20, 30]);
        let update = resolve_selection_actions(
            &ids(&[]),
            Some((1, TrackId::from_bits(99))),
            Some(TrackId::from_bits(30)),
            &visible,
            Some((2, false, true)),
            None,
            None,
        );
        assert_eq!(update.selected_tracks, ids(&[30]));
        assert_eq!(update.selection_anchor, Some((2, TrackId::from_bits(30))));
        assert_eq!(update.selected_track, Some(TrackId::from_bits(30)));
    }

    #[test]
    fn resolution_select_all_keeps_hidden_rows_and_the_cursor() {
        // Rows hidden by the search (id 9) survive; the cursor stays on
        // its previous row (review 🟡-1) — Enter must not retarget to
        // the bottom visible row; the anchor is untouched as well.
        let visible = ids(&[1, 2, 3]);
        let update = resolve_selection_actions(
            &ids(&[9]),
            Some((0, TrackId::from_bits(9))),
            Some(TrackId::from_bits(9)),
            &visible,
            None,
            Some(true),
            None,
        );
        assert_eq!(update.selected_tracks, ids(&[9, 1, 2, 3]));
        assert_eq!(update.selection_anchor, Some((0, TrackId::from_bits(9))));
        assert_eq!(update.selected_track, Some(TrackId::from_bits(9)), "cursor put");
    }

    #[test]
    fn resolution_uncheck_all_clears_only_visible_and_keeps_the_cursor() {
        let visible = ids(&[1, 2, 3]);
        let update = resolve_selection_actions(
            &ids(&[1, 2, 3, 9]),
            Some((0, TrackId::from_bits(1))),
            Some(TrackId::from_bits(3)),
            &visible,
            None,
            Some(false),
            None,
        );
        assert_eq!(update.selected_tracks, ids(&[9]), "out-of-view rows survive");
        assert_eq!(update.selected_track, Some(TrackId::from_bits(3)), "cursor put");
    }

    #[test]
    fn resolution_checkbox_toggle_moves_the_cursor_only_on_check() {
        let visible = ids(&[1, 2]);
        // Checking: the cursor moves to the toggled row.
        let check = resolve_selection_actions(
            &ids(&[1]),
            None,
            Some(TrackId::from_bits(1)),
            &visible,
            None,
            None,
            Some(TrackId::from_bits(2)),
        );
        assert_eq!(check.selected_tracks, ids(&[1, 2]));
        assert_eq!(check.selected_track, Some(TrackId::from_bits(2)));
        // Unchecking: the cursor stays where it was.
        let uncheck = resolve_selection_actions(
            &ids(&[1, 2]),
            None,
            Some(TrackId::from_bits(1)),
            &visible,
            None,
            None,
            Some(TrackId::from_bits(2)),
        );
        assert_eq!(uncheck.selected_tracks, ids(&[1]));
        assert_eq!(uncheck.selected_track, Some(TrackId::from_bits(1)), "cursor put");
    }

    // ---- table columns fit the panel (round 4.1 Ф4) ----

    /// Worst-case central content width at a 900px window: 900 − 240
    /// sidebar − panel margins − vscroll bar. All five columns must be
    /// visible with a ≥100px title (round 6 П2: the checkbox column
    /// shares the same width — the title's worst case is 104px).
    #[test]
    fn table_columns_fit_narrow_panel_with_100px_title() {
        let rects: std::rc::Rc<std::cell::RefCell<Vec<egui::Rect>>> = Default::default();
        let sink = rects.clone();
        egui::__run_test_ui(|ui| {
            ui.allocate_ui(egui::vec2(616.0, 400.0), |ui| {
                let columns = render_measured_table(ui);
                *sink.borrow_mut() = columns;
            });
        });
        let rects = rects.borrow();
        assert_eq!(rects.len(), 5, "five header cells must render");
        let panel_right = 616.0;
        for (index, rect) in rects.iter().enumerate() {
            assert!(
                rect.right() <= panel_right + 1.0,
                "column {index} overflows the panel: {rect:?}"
            );
            assert!(rect.width() > 20.0, "column {index} collapsed: {rect:?}");
        }
        assert_eq!(rects[0].width(), CHECK_COLUMN, "checkbox column is fixed");
        assert!(rects[1].width() >= 100.0, "title ≥ 100px, got {}", rects[1].width());
        assert!(rects[4].left() >= rects[3].right(), "columns must not overlap");
    }

    // ---- sidebar geometry (round 6 П1) ----

    /// The selection must span the whole panel width (the standard list
    /// pattern), not end at the text like the old selectable_value.
    #[test]
    fn sidebar_row_spans_the_full_panel_width() {
        let rect: std::rc::Rc<std::cell::RefCell<Option<egui::Rect>>> = Default::default();
        let sink = rect.clone();
        egui::__run_test_ui(|ui| {
            ui.allocate_ui(egui::vec2(240.0, 120.0), |ui| {
                let response = sidebar_row(ui, true, "Плейлист", "5 треков");
                *sink.borrow_mut() = Some(response.rect);
            });
        });
        let rect = rect.borrow().expect("the row must render");
        assert!((rect.width() - 240.0).abs() < 0.5, "full width, got {rect:?}");
    }
}
