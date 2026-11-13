//! Thin UI layer: panels, confirm modals and the worker-thread plumbing.
//!
//! Every blocking step (sudo mount, eject sequence, library open) runs on
//! a background thread and reports through an mpsc channel; the UI only
//! drains it with `try_recv` (egui FAQ: never block the UI thread), and
//! workers wake the event loop via `ctx.request_repaint()`.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, TryRecvError};

use eframe::egui;

use crate::device::{self, DeviceCandidate, MountError, MountSpec};

/// Track/playlist counters shown after a successful connect. Round 1
/// opens the library in the worker and drops it; round 2 moves live
/// backend operations into UI state and will keep it instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LibraryCounts {
    pub tracks: usize,
    pub playlists: usize,
}

/// Messages from worker threads to the UI.
enum DeviceEvent {
    Connected(Result<LibraryCounts, ConnectError>),
    Ejected(Result<(), String>),
}

#[derive(Debug)]
enum ConnectError {
    /// Preflight failure (before any command ran): the volume is
    /// untouched, so the message must not claim it was mounted.
    Preflight(String),
    Mount { error: MountError, stderr: String },
    /// Library open/count failure: the volume IS mounted at this point.
    Open(String),
    /// Worker thread panicked mid-operation; operation state is unknown.
    Internal(String),
}

/// One scanned device candidate with its resolved block device.
struct DeviceEntry {
    candidate: DeviceCandidate,
    block_dev: Option<PathBuf>,
}

/// Device currently connected through the GUI (round 1: read-only use).
struct ConnectedDevice {
    mount: PathBuf,
    block_dev: PathBuf,
    counts: LibraryCounts,
}

/// Error prepared for display: message + optional advice + raw tool output.
struct UiError {
    message: String,
    hint: Option<String>,
    stderr: Option<String>,
}

pub struct PodlibApp {
    /// Saved once (AppCreator) so worker threads can request repaints.
    ctx: egui::Context,
    devices: Vec<DeviceEntry>,
    selected: Option<usize>,
    password: String,
    confirm_mount: bool,
    confirm_eject: bool,
    start_mount: bool,
    start_eject: bool,
    busy: bool,
    status: String,
    error: Option<UiError>,
    rx: Option<Receiver<DeviceEvent>>,
    connected: Option<ConnectedDevice>,
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
            start_mount: false,
            start_eject: false,
            busy: false,
            status: String::new(),
            error: None,
            rx: None,
            connected: None,
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
            DeviceEvent::Connected(Ok(counts)) => {
                let entry = self.selected_entry();
                let (mount, block_dev) = match entry {
                    Some(entry) => (
                        entry.candidate.mount.clone(),
                        entry.block_dev.clone().unwrap_or_default(),
                    ),
                    None => return, // selection changed mid-operation: drop
                };
                self.status = format!(
                    "Подключено: {} треков, {} плейлистов",
                    counts.tracks, counts.playlists
                );
                self.connected = Some(ConnectedDevice {
                    mount,
                    block_dev,
                    counts,
                });
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
                    ConnectError::Open(message) => UiError {
                        message: format!("Том смонтирован, но библиотека не открылась: {message}"),
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
                self.error = Some(UiError {
                    message,
                    hint: None,
                    stderr: None,
                });
                self.status.clear();
            }
        }
    }

    fn spawn_connect(&mut self) {
        let Some(entry) = self.selected_entry() else {
            return;
        };
        let Some(block_dev) = entry.block_dev.clone() else {
            self.error = Some(UiError {
                message: "Не удалось определить блочное устройство по sysfs".to_owned(),
                hint: Some(
                    "Проверьте, что том смонтирован, и нажмите «Обновить».".to_owned(),
                ),
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
        let ctx = self.ctx.clone();
        let (tx, rx) = channel();
        self.rx = Some(rx);
        self.busy = true;
        self.error = None;
        self.status =
            "Отключение: sync → unmount → power-off…".to_owned();
        std::thread::spawn(move || {
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
        if self.busy {
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
                ui.label(format!(
                    "Треков: {} · Плейлистов: {}",
                    connected.counts.tracks, connected.counts.playlists
                ));
                ui.add_space(8.0);
                if ui
                    .add_enabled(!self.busy, egui::Button::new("Безопасно отключить"))
                    .clicked()
                {
                    self.confirm_eject = true;
                }
            }
            None => {
                ui.label(
                    "Выберите устройство слева и нажмите «Подключить». \
                     После подключения здесь появятся счётчики библиотеки.",
                );
            }
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
            ui.label("Будут выполнены: sync, unmount, power-off (udisksctl).");
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
}

/// Worker-thread body: sudo rw-mount, then open the library and count.
/// The backend is opened and dropped here — see [`LibraryCounts`].
fn connect_device(
    password: String, // owned: dropped right after the mount command (W3)
    block_dev: PathBuf,
    mount_path: PathBuf,
) -> Result<LibraryCounts, ConnectError> {
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
    // dropped before the longer library-open phase instead of staying
    // alive in the worker until connect_device returns.
    drop(password);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    device::classify_mount_output(&output).map_err(|error| ConnectError::Mount { error, stderr })?;
    // The udisksctl sequence is not needed here: the volume is mounted.
    let backend =
        podlib::open_library(&mount_path).map_err(|error| ConnectError::Open(error.to_string()))?;
    let tracks = backend
        .tracks()
        .map_err(|error| ConnectError::Open(error.to_string()))?
        .len();
    let playlists = backend
        .playlists()
        .map_err(|error| ConnectError::Open(error.to_string()))?
        .len();
    Ok(LibraryCounts { tracks, playlists })
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

/// Extracts a human-readable message from a panic payload. Takes the box
/// by value: downcasting through a coerced `&(dyn Any + Send)` silently
/// fails (the derived vtable loses the type identity), while the owning
/// `Box<dyn Any + Send>` downcasts correctly.
fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_owned()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "причина не сообщена".to_owned()
    }
}

impl eframe::App for PodlibApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_events();
        egui::Panel::left("device_panel")
            .default_size(360.0)
            .resizable(true)
            .show(ui, |ui| self.device_panel(ui));
        egui::CentralPanel::default().show(ui, |ui| self.status_area(ui));
        let ctx = ui.ctx().clone();
        self.maybe_mount_modal(&ctx);
        self.maybe_eject_modal(&ctx);
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
    use super::*;

    /// Headless app instance: `handle_event`/`drain_events` only touch
    /// plain state (strings, options, channels), no egui rendering.
    fn test_app() -> PodlibApp {
        PodlibApp::new(egui::Context::default())
    }

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
    fn open_error_still_reports_mounted_volume() {
        let mut app = test_app();
        app.handle_event(DeviceEvent::Connected(Err(ConnectError::Open(
            "библиотека повреждена".to_owned(),
        ))));
        let error = app.error.expect("open failure must be shown");
        assert!(error.message.contains("смонтирован"));
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
        let result: Result<LibraryCounts, ConnectError> = run_worker_step(
            || panic!("worker exploded"),
            ConnectError::Internal,
        );
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
        let error = run_worker_step(|| panic!("worker exploded"), ConnectError::Internal);
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
        let result: Result<LibraryCounts, ConnectError> =
            run_worker_step(|| Ok(LibraryCounts { tracks: 3, playlists: 1 }), |_| unreachable!());
        assert!(matches!(result, Ok(LibraryCounts { tracks: 3, playlists: 1 })));
    }
}
