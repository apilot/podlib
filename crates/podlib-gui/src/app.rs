//! Thin UI layer: panels, confirm modals and the worker-thread plumbing.
//!
//! Every blocking step (sudo mount, eject sequence, library open) runs on
//! a background thread and reports through an mpsc channel; the UI only
//! drains it with `try_recv` (egui FAQ: never block the UI thread), and
//! workers wake the event loop via `ctx.request_repaint()`. The library
//! backend itself lives on its own persistent worker thread — see
//! [`crate::library`] for why.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, TryRecvError};

use eframe::egui;
use podlib::{Playlist, PlaylistId, Track, TrackId};

use crate::device::{self, DeviceCandidate, MountError, MountSpec};
use crate::library::{
    changes_from_track, drafts_from_paths, guard_playlist_mutation, panic_message, plural,
    EditDialogFields, DraggedTracks, LibraryCommand, LibraryOutcome, LibrarySnapshot,
    LibraryWorker,
};

/// Row height of the track table cells, keeps the column grid aligned.
const ROW_HEIGHT: f32 = 20.0;
/// Fixed column widths of the track table: title/artist/album/duration.
const COLUMN_WIDTHS: [(f32, &str); 4] = [
    (260.0, "Название"),
    (180.0, "Исполнитель"),
    (180.0, "Альбом"),
    (70.0, "Длит."),
];

/// Messages from worker threads to the UI.
enum DeviceEvent {
    /// Device mounted read-write; payload is the mount point.
    Connected(Result<PathBuf, ConnectError>),
    Ejected(Result<(), String>),
}

#[derive(Debug)]
enum ConnectError {
    /// Preflight failure (before any command ran): the volume is
    /// untouched, so the message must not claim it was mounted.
    Preflight(String),
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
}

impl TrackRowActions {
    fn merge(&mut self, other: Self) {
        self.edit = other.edit.or(self.edit.take());
        self.add_to_playlist = other.add_to_playlist.or(self.add_to_playlist.take());
        self.delete = other.delete.or(self.delete.take());
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
    delete_playlist: Option<PlaylistId>,
    remove_member: Option<(PlaylistId, TrackId)>,
}

impl PlaylistPanelActions {
    fn merge(&mut self, other: Self) {
        self.add_drop = other.add_drop.or(self.add_drop.take());
        self.master_drop = other.master_drop.or(self.master_drop.take());
        self.delete_playlist = other.delete_playlist.or(self.delete_playlist.take());
        self.remove_member = other.remove_member.or(self.remove_member.take());
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
    confirm_delete_track: Option<TrackId>,
    purge_media: bool,
    confirm_delete_playlist: Option<PlaylistId>,
    create_open: bool,
    new_playlist_name: String,
    edit_dialog: Option<EditDialogState>,
    picker_tracks: Option<Vec<TrackId>>,
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
}

impl PodlibApp {
    pub fn new(ctx: egui::Context) -> Self {
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
        };
        app.refresh_devices();
        app
    }

    fn refresh_devices(&mut self) {
        self.devices = device::scan_candidates()
            .into_iter()
            .map(|candidate| DeviceEntry {
                block_dev: device::resolve_block_device(&candidate.mount),
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
            DeviceEvent::Connected(Ok(mount)) => {
                let block_dev = self
                    .selected_entry()
                    .and_then(|entry| entry.block_dev.clone())
                    .unwrap_or_default();
                self.connected = Some(ConnectedDevice {
                    mount: mount.clone(),
                    block_dev,
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
                    // Preflight happens before any mount command, so
                    // claiming a mounted volume here would be a lie.
                    ConnectError::Preflight(message) => UiError {
                        message: format!("Монтирование не выполнено: {message}"),
                        hint: None,
                        stderr: None,
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
                self.status = "Устройство безопасно отключено".to_owned();
                self.refresh_devices();
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
                }
                Ok(LibraryOutcome::Failed { message, hint }) => {
                    self.error = Some(UiError {
                        message,
                        hint,
                        stderr: None,
                    });
                    self.status.clear();
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
    fn library_request(&mut self, command: LibraryCommand, kind: PendingKind, status: &str) {
        // No library at all is not "busy" — the honest refusal points at
        // connecting a device instead of a wait that would never end.
        let Some(library) = self.library.as_ref() else {
            self.error = Some(UiError {
                message: "Сначала подключите устройство".to_owned(),
                hint: None,
                stderr: None,
            });
            return;
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
            return;
        }
        match library.worker.request(command) {
            Ok(rx) => {
                if let Some(library) = self.library.as_mut() {
                    library.pending = Some((kind, rx));
                }
                self.status = status.to_owned();
            }
            Err(message) => {
                self.error = Some(UiError {
                    message,
                    hint: None,
                    stderr: None,
                });
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
                hint: Some("Проверьте, что том смонтирован, и нажмите «Обновить».".to_owned()),
                stderr: None,
            });
            return;
        };
        let mount_path = entry.candidate.mount.clone();
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
                || connect_device(password, block_dev, mount_path),
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
        let steps = device::build_unmount_sequence(
            &connected.mount,
            &connected.block_dev,
            Path::new("/sys/dev/block"),
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
                || device::run_sequence(&steps).map_err(|error| error.to_string()),
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
                "Устройства не найдены. Подключите устройство по USB — \
                 система смонтирует его в /run/media.",
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
                let text = format!(
                    "{}\n{}\n{}",
                    entry.candidate.label,
                    entry.candidate.mount.display(),
                    dev
                );
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

    fn status_area(&mut self, ui: &mut egui::Ui) {
        ui.heading("podlib");
        ui.add_space(8.0);
        if self.busy || self.library_busy() {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new());
                ui.label(&self.status);
            });
            ui.add_space(4.0);
        } else if !self.status.is_empty() {
            ui.label(&self.status);
        }
        if let Some(error) = &self.error {
            ui.add_space(4.0);
            ui.colored_label(egui::Color32::LIGHT_RED, &error.message);
            if let Some(hint) = &error.hint {
                ui.label(hint);
            }
            if let Some(stderr) = &error.stderr {
                if !stderr.trim().is_empty() {
                    egui::ScrollArea::vertical()
                        .max_height(120.0)
                        .show(ui, |ui| ui.monospace(stderr));
                }
            }
        }
        ui.add_space(8.0);
        ui.separator();
        match &self.connected {
            Some(connected) => {
                ui.colored_label(
                    egui::Color32::LIGHT_GREEN,
                    format!(
                        "Подключено: {} ({})",
                        connected.mount.display(),
                        connected.block_dev.display()
                    ),
                );
                if self.library.is_none() && !self.busy {
                    ui.colored_label(
                        egui::Color32::LIGHT_RED,
                        "Библиотека недоступна: воркер закрылся. Отключите и подключите \
                         устройство заново.",
                    );
                }
                ui.add_space(8.0);
                let ready = !self.busy && self.library_idle();
                if ui
                    .add_enabled(ready, egui::Button::new("Безопасно отключить"))
                    .clicked()
                {
                    self.confirm_eject = true;
                }
            }
            None => {
                ui.label(
                    "Выберите устройство слева и нажмите «Подключить». \
                     После подключения здесь появится статус, а справа — плейлисты.",
                );
            }
        }
    }

    /// Central panel: toolbar + track table with drag-and-drop rows.
    fn library_view(&mut self, ui: &mut egui::Ui) {
        library_toolbar(self, ui);
        ui.separator();

        // Immutable borrow for rendering; mutating choices are applied
        // after the loop, when the unique borrow is free again.
        let Some(library) = self.library.as_ref() else {
            return;
        };
        if library.snapshot.tracks.is_empty() {
            ui.label("Библиотека пуста. Перетащите аудиофайлы в окно.");
            return;
        }

        // Menus stay closed while an operation is in flight: their
        // choices fire library requests a busy worker would refuse.
        let menus_allowed = context_menu_allowed(self);
        let actions = track_table(ui, &library.snapshot, menus_allowed);
        self.apply_track_actions(actions);
    }

    /// Applies the row choices collected by [`track_table`].
    fn apply_track_actions(&mut self, actions: TrackRowActions) {
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
                library.confirm_delete_track = Some(id);
                library.purge_media = false;
            }
        }
    }

    /// Right panel: playlists, each header a drop target for dragged tracks.
    fn playlists_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("Плейлисты");
        let Some(library) = self.library.as_ref() else {
            return;
        };
        // Menus stay closed while an operation is in flight: their
        // choices fire library requests a busy worker would refuse.
        let menus_allowed = context_menu_allowed(self);
        let mut actions = PlaylistPanelActions::default();
        for playlist in &library.snapshot.playlists {
            actions.merge(playlist_block(
                ui,
                &library.snapshot,
                playlist,
                menus_allowed,
            ));
        }
        self.apply_playlist_actions(actions);
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
        if let Some((playlist, member)) = actions.remove_member {
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
        let (drafts, skipped) = drafts_from_paths(&dropped);
        if drafts.is_empty() {
            if !skipped.is_empty() {
                self.status = format!("Пропущены не-аудио файлы: {}", skipped.join(", "));
            }
            return;
        }
        let status = batch_add_status(drafts.len(), &skipped);
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
            self.force_close = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn maybe_delete_track_modal(&mut self, ctx: &egui::Context) {
        let Some(track_id) = self
            .library
            .as_ref()
            .and_then(|library| library.confirm_delete_track)
        else {
            return;
        };
        let title = self
            .library
            .as_ref()
            .and_then(|library| library.snapshot.track(track_id))
            .map(|track| track.title.clone())
            .unwrap_or_default();
        let mut confirmed = false;
        let response = egui::Modal::new(egui::Id::unique("confirm_delete_track")).show(ctx, |ui| {
            ui.set_width(440.0);
            ui.heading(format!("Удалить трек «{title}»?"));
            ui.add_space(4.0);
            ui.label("Трек исчезнет из библиотеки и всех плейлистов.");
            if let Some(library) = self.library.as_mut() {
                // The checkbox lives in the state so it survives frames.
                ui.checkbox(&mut library.purge_media, "Удалить и медиа-файл с устройства");
            }
            ui.label(
                egui::RichText::new(
                    "Без галочки аудиофайл останется на диске как «осиротевший».",
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
            self.library_request(
                LibraryCommand::RemoveTrack {
                    id: track_id,
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
/// is one command → one reply, so per-file progress («Добавление i/N…»)
/// would require extending `LibraryCommand`/`LibraryOutcome` — and the
/// backend trait contract is out of bounds. Instead the wait is
/// announced honestly up front; meanwhile every button/menu is disabled
/// by the idle gates and the spinner shows in the status area.
fn batch_add_status(count: usize, skipped: &[String]) -> String {
    let mut status = format!(
        "Добавляется {} {}, это может занять время…",
        count,
        plural(count, "файл", "файла", "файлов")
    );
    if !skipped.is_empty() {
        status.push_str(&format!("; пропущены: {}", skipped.join(", ")));
    }
    status
}

/// Toolbar above the track table: refresh, new playlist, DnD hint.
fn library_toolbar(app: &mut PodlibApp, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        let idle = app.library_idle();
        if ui
            .add_enabled(idle, egui::Button::new("Обновить"))
            .clicked()
        {
            app.library_request(
                LibraryCommand::Refresh,
                PendingKind::Refresh,
                "Обновляю список…",
            );
        }
        if ui
            .add_enabled(idle, egui::Button::new("+ Плейлист"))
            .clicked()
        {
            if let Some(library) = app.library.as_mut() {
                library.create_open = true;
                library.new_playlist_name.clear();
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                egui::RichText::new("Перетащите аудиофайлы в окно, чтобы добавить")
                    .weak(),
            );
        });
    });
}

/// Renders the track table and collects context-menu choices without
/// holding a borrow on app state (the caller applies them afterwards).
fn track_table(
    ui: &mut egui::Ui,
    snapshot: &LibrarySnapshot,
    menus_allowed: bool,
) -> TrackRowActions {
    let mut actions = TrackRowActions::default();
    egui::ScrollArea::vertical().show(ui, |ui| {
        table_header(ui);
        ui.separator();
        for track in &snapshot.tracks {
            let row_id = egui::Id::unique(("track_row", track.id.to_bits()));
            let response = ui
                .dnd_drag_source(row_id, DraggedTracks(vec![track.id]), |ui| {
                    track_row(ui, track);
                })
                .response;
            let mut choice = TrackRowActions::default();
            if menus_allowed {
                response.context_menu(|ui| {
                    if ui.button("Изменить…").clicked() {
                        choice.edit = Some(track.id);
                        ui.close();
                    }
                    if ui.button("Добавить в плейлист…").clicked() {
                        choice.add_to_playlist = Some(vec![track.id]);
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Удалить…").clicked() {
                        choice.delete = Some(track.id);
                        ui.close();
                    }
                });
            }
            actions.merge(choice);
        }
    });
    actions
}

/// Header row, same column geometry as the data rows.
fn table_header(ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        for (width, name) in COLUMN_WIDTHS {
            ui.add_sized(
                [width, ROW_HEIGHT],
                egui::Label::new(egui::RichText::new(name).strong()),
            );
        }
    });
}

/// One playlist block: drop zone, collapsing header, member rows.
/// Collects this playlist's choices; no app-state borrow held.
fn playlist_block(
    ui: &mut egui::Ui,
    snapshot: &LibrarySnapshot,
    playlist: &Playlist,
    menus_allowed: bool,
) -> PlaylistPanelActions {
    let mut actions = PlaylistPanelActions::default();
    let is_master = playlist.is_master;
    let header_text = if is_master {
        format!("{} (мастер)", playlist.name)
    } else {
        playlist.name.clone()
    };
    let header_id = egui::Id::unique(("playlist_header", playlist.id.to_bits()));
    let (_, dropped) = ui.dnd_drop_zone::<DraggedTracks, _>(
        egui::Frame::default().inner_margin(egui::Margin::same(2)),
        |ui| {
            let collapsing = egui::CollapsingHeader::new(header_text.as_str())
                .id_salt(header_id)
                .default_open(true)
                .show(ui, |ui| {
                    actions.remove_member =
                        playlist_members(ui, snapshot, playlist, menus_allowed);
                });
            if !is_master && menus_allowed {
                let mut delete = false;
                collapsing.header_response.context_menu(|ui| {
                    if ui.button("Удалить плейлист…").clicked() {
                        delete = true;
                        ui.close();
                    }
                });
                if delete {
                    actions.delete_playlist = Some(playlist.id);
                }
            }
        },
    );
    if let Some(dragged) = dropped {
        // WHY the UI refuses master drops instead of trusting the
        // backend: the 7G adapter rejects master-member mutations,
        // but the classic backend allows them on purpose (firmware
        // parity) — where they are pointless anyway: the master is
        // the mirror of the whole library and duplicates are
        // ignored. One consistent rule, explained without a round
        // trip to the device.
        if is_master {
            actions.master_drop = Some(playlist.name.clone());
        } else {
            actions.add_drop = Some((playlist.id, dragged.0.clone()));
        }
    }
    actions
}

/// Member rows inside one playlist header; returns an «Убрать» choice.
fn playlist_members(
    ui: &mut egui::Ui,
    snapshot: &LibrarySnapshot,
    playlist: &Playlist,
    menus_allowed: bool,
) -> Option<(PlaylistId, TrackId)> {
    let mut remove_member = None;
    let is_master = playlist.is_master;
    for member_id in playlist.unique_track_ids() {
        let title = snapshot
            .track(member_id)
            .map(|track| track.title.as_str())
            .unwrap_or("<нет в библиотеке>");
        let mut remove = false;
        let row = ui
            .horizontal(|ui| {
                ui.add_sized(
                    [ui.available_width() - 8.0, ROW_HEIGHT - 4.0],
                    egui::Label::new(egui::RichText::new(title).weak()),
                );
            })
            .response;
        if !is_master && menus_allowed {
            row.context_menu(|ui| {
                if ui.button("Убрать из плейлиста").clicked() {
                    remove = true;
                    ui.close();
                }
            });
        }
        if remove {
            remove_member = Some((playlist.id, member_id));
        }
    }
    if playlist.track_ids.is_empty() {
        ui.label(egui::RichText::new("— пусто —").weak());
    }
    remove_member
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

/// Renders one track-table row: four fixed-width cells (drag source body).
fn track_row(ui: &mut egui::Ui, track: &Track) {
    ui.horizontal(|ui| {
        ui.add_sized(
            [COLUMN_WIDTHS[0].0, ROW_HEIGHT],
            egui::Label::new(egui::RichText::new(track.title.as_str()).strong()),
        );
        ui.add_sized(
            [COLUMN_WIDTHS[1].0, ROW_HEIGHT],
            egui::Label::new(optional_text(&track.artist)),
        );
        ui.add_sized(
            [COLUMN_WIDTHS[2].0, ROW_HEIGHT],
            egui::Label::new(optional_text(&track.album)),
        );
        ui.add_sized(
            [COLUMN_WIDTHS[3].0, ROW_HEIGHT],
            egui::Label::new(format_duration(track.duration_ms)),
        );
    });
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

/// Worker-thread body: sudo rw-mount only. The library is NOT opened here:
/// `Box<dyn LibraryBackend>` is not `Send`, so it is opened on the library
/// worker thread instead (see library.rs).
fn connect_device(
    password: String, // owned: dropped right after the mount command (W3)
    block_dev: PathBuf,
    mount_path: PathBuf,
) -> Result<PathBuf, ConnectError> {
    let (uid, gid) = device::current_uid_gid().ok_or_else(|| {
        ConnectError::Preflight("не удалось определить uid/gid процесса".to_owned())
    })?;
    let spec = device::build_mount_command(&MountSpec {
        block_dev: block_dev.clone(),
        mount_path: mount_path.clone(),
        uid,
        gid,
    });
    let output = device::run_sudo_mount(&password, &spec).map_err(|error| ConnectError::Mount {
        stderr: String::new(),
        error: MountError::Io(error),
    })?;
    // WHY: the password lives only for the duration of the command. sudo
    // has already consumed it (stdin is closed at this point), so it is
    // dropped before returning instead of staying alive in the worker.
    drop(password);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    device::classify_mount_output(&output).map_err(|error| ConnectError::Mount { error, stderr })?;
    Ok(mount_path)
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
        egui::Panel::left("device_panel")
            .default_size(340.0)
            .resizable(true)
            .show(ui, |ui| self.device_panel(ui));
        egui::Panel::right("playlists_panel")
            .default_size(280.0)
            .resizable(true)
            .show(ui, |ui| self.playlists_panel(ui));
        egui::CentralPanel::default().show(ui, |ui| {
            self.status_area(ui);
            if self.library.is_some() {
                ui.separator();
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
    fn preflight_error_does_not_claim_mounted_volume() {
        let mut app = test_app();
        app.handle_event(DeviceEvent::Connected(Err(ConnectError::Preflight(
            "не удалось определить uid/gid процесса".to_owned(),
        ))));
        let error = app.error.expect("preflight failure must be shown");
        // W4: the failure happens before the mount command — the UI must
        // not state that the volume is mounted.
        assert!(error.message.contains("Монтирование не выполнено"));
        assert!(!error.message.contains("смонтирован"));
    }

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
        let error: Result<PathBuf, ConnectError> =
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
        let result: Result<PathBuf, ConnectError> = run_worker_step(
            || Ok(PathBuf::from("/run/media/ipod")),
            |_| unreachable!(),
        );
        assert_eq!(
            result.unwrap(),
            PathBuf::from("/run/media/ipod")
        );
    }

    // ---- library lifecycle through the real worker ----

    #[test]
    fn connected_event_opens_library_and_loads_snapshot() {
        let dir = fixture_mount();
        let mut app = test_app();
        app.handle_event(DeviceEvent::Connected(Ok(dir.path().to_owned())));
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
        app.handle_event(DeviceEvent::Connected(Ok(dir.path().to_owned())));
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
        app.handle_event(DeviceEvent::Connected(Ok(dir.path().to_owned())));
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
            batch_add_status(1, &[]),
            "Добавляется 1 файл, это может занять время…"
        );
        assert_eq!(
            batch_add_status(2, &[]),
            "Добавляется 2 файла, это может занять время…"
        );
        assert_eq!(
            batch_add_status(30, &[]),
            "Добавляется 30 файлов, это может занять время…"
        );
        assert_eq!(
            batch_add_status(3, &["notes.txt".to_owned(), "noext".to_owned()]),
            "Добавляется 3 файла, это может занять время…; \
             пропущены: notes.txt, noext"
        );
    }
}
