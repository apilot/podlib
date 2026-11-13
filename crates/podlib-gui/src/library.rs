//! Library worker: the persistent thread that owns the library backend.
//!
//! WHY a worker thread instead of keeping the backend in UI state: the
//! UI thread must never block, and `podlib::open_library` returns a
//! `Box<dyn LibraryBackend>`, which is not `Send`. Such a value can
//! neither cross threads nor be wrapped in `Mutex<..>` inside UI state
//! (`Mutex<T>` is `Send` only when `T: Send`), and reopening the library
//! on every mutation would re-parse the whole iTunesDB twice per
//! operation — too slow for large libraries. The minimal `Send`-safe
//! design is therefore one long-lived worker thread that owns the
//! backend: the UI sends plain-data commands over an mpsc channel, each
//! with its own reply channel, and drains replies with `try_recv`.

use std::any::Any;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread::JoinHandle;

use podlib::{
    BackendError, LibraryBackend, Playlist, PlaylistDraft, PlaylistId, Track, TrackChanges,
    TrackDraft, TrackId,
};

/// One command to the library worker; plain data, so it is `Send` even
/// though the backend it drives is not.
#[derive(Debug, Clone)]
pub enum LibraryCommand {
    /// Full re-read of tracks and playlists.
    Refresh,
    /// Adds host-side audio files already mapped into drafts.
    AddTracks(Vec<TrackDraft>),
    RemoveTrack { id: TrackId, purge_media: bool },
    EditTrack { id: TrackId, changes: TrackChanges },
    CreatePlaylist { name: String },
    DeletePlaylist { id: PlaylistId },
    AddToPlaylist { playlist: PlaylistId, track_ids: Vec<TrackId> },
    RemoveFromPlaylist { playlist: PlaylistId, track_ids: Vec<TrackId> },
    /// Drops the backend so the DB is closed before the device unmounts.
    Close,
}

/// Full library listing; a fresh copy is taken after every operation
/// (KISS: full re-list instead of tracking deltas).
#[derive(Debug, Clone, Default)]
pub struct LibrarySnapshot {
    pub tracks: Vec<Track>,
    pub playlists: Vec<Playlist>,
}

impl LibrarySnapshot {
    pub fn track(&self, id: TrackId) -> Option<&Track> {
        self.tracks.iter().find(|track| track.id == id)
    }

    pub fn playlist(&self, id: PlaylistId) -> Option<&Playlist> {
        self.playlists.iter().find(|playlist| playlist.id == id)
    }
}

/// Worker → UI reply for one command (or for the initial open).
#[derive(Debug)]
pub enum LibraryOutcome {
    /// The backend opened and the initial listing succeeded.
    Loaded(LibrarySnapshot),
    /// An operation succeeded; `summary` is a ready-made status notice
    /// (for `edit_track` it surfaces the id change required by contract:
    /// edits are remove-and-re-add, so the id may rotate).
    Applied {
        snapshot: LibrarySnapshot,
        summary: String,
    },
    /// `Close` executed: the backend is dropped and the worker exits.
    Closed,
    Failed {
        message: String,
        hint: Option<String>,
    },
}

type LibraryReply = Sender<LibraryOutcome>;

/// Handle to the worker thread. `Clone`-free on purpose: the UI holds
/// exactly one, and eject moves it into the eject thread.
pub struct LibraryWorker {
    requests: Option<Sender<(LibraryCommand, LibraryReply)>>,
    handle: Option<JoinHandle<()>>,
}

impl LibraryWorker {
    /// Spawns the worker; the backend is opened inside the spawned
    /// thread (`Box<dyn LibraryBackend>` is not `Send`, so it must never
    /// exist on the UI thread). The returned receiver yields exactly one
    /// message: `Loaded` or `Failed` for the initial open.
    pub fn spawn(mount: PathBuf) -> (Self, Receiver<LibraryOutcome>) {
        let (requests, incoming) = channel::<(LibraryCommand, LibraryReply)>();
        let (opened_tx, opened_rx) = channel();
        // No `.expect()` here (round-3 rule: none outside tests): a
        // failed thread spawn surfaces as an honest Failed outcome and
        // a dead worker whose `request()` reports "не активен".
        // Clone the open sender: the closure owns one copy, the error
        // arm below still has the original to report the failure.
        let spawned = std::thread::Builder::new()
            .name("podlib-library".to_owned())
            .spawn({
                let opened = opened_tx.clone();
                move || worker_loop(mount, incoming, opened)
            });
        match spawned {
            Ok(handle) => (
                Self {
                    requests: Some(requests),
                    handle: Some(handle),
                },
                opened_rx,
            ),
            Err(error) => {
                let _ = opened_tx.send(LibraryOutcome::Failed {
                    message: format!("Не удалось запустить поток библиотеки: {error}"),
                    hint: Some("Подключите устройство заново.".to_owned()),
                });
                (Self {
                    requests: None,
                    handle: None,
                }, opened_rx)
            }
        }
    }

    /// Test-only twin of [`Self::spawn`]: runs the command loop over a
    /// backend built inside the spawned thread (a `Box<dyn
    /// LibraryBackend>` is not `Send`, so — exactly like in production —
    /// it must never cross a thread boundary). Lets tests drive
    /// behaviour a real device fixture cannot reproduce (a backend
    /// panicking mid-command).
    #[cfg(test)]
    fn spawn_with_backend(
        make_backend: impl FnOnce() -> Box<dyn LibraryBackend> + Send + 'static,
    ) -> (Self, Receiver<LibraryOutcome>) {
        let (requests, incoming) = channel::<(LibraryCommand, LibraryReply)>();
        // No open outcome is produced here; the sender dies at once and
        // the receiver stays unread (tests drive commands directly).
        let (_, opened_rx) = channel();
        let handle = std::thread::Builder::new()
            .name("podlib-library-test".to_owned())
            .spawn(move || serve_loop(make_backend(), incoming))
            .expect("spawn test library worker thread");
        (
            Self {
                requests: Some(requests),
                handle: Some(handle),
            },
            opened_rx,
        )
    }

    /// Sends one command and returns the reply channel for it. Fails
    /// when the worker is gone (open failed earlier, or it exited).
    pub fn request(
        &self,
        command: LibraryCommand,
    ) -> Result<Receiver<LibraryOutcome>, String> {
        let requests = self
            .requests
            .as_ref()
            .ok_or_else(|| "воркер библиотеки не активен".to_owned())?;
        let (reply_tx, reply_rx) = channel();
        requests
            .send((command, reply_tx))
            .map(|_| reply_rx)
            .map_err(|_| "воркер библиотеки не активен".to_owned())
    }

    /// Blocking close and join. Used by the eject thread: the backend
    /// must be closed while the device is still mounted, so `Close` is
    /// awaited before the unmount sequence starts.
    pub fn close_and_join(mut self) {
        if let Some(requests) = self.requests.take() {
            let (reply_tx, reply_rx) = channel();
            let _ = requests.send((LibraryCommand::Close, reply_tx));
            let _ = reply_rx.recv();
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for LibraryWorker {
    fn drop(&mut self) {
        // Dropping the request sender makes the worker's recv() fail, so
        // it finishes the in-flight command (mutations persist before
        // returning) and drops the backend. No blocking join: the UI
        // thread must not wait on a potentially slow persist.
        self.requests.take();
    }
}

/// Worker body: open → initial listing → command loop.
fn worker_loop(
    mount: PathBuf,
    incoming: Receiver<(LibraryCommand, LibraryReply)>,
    opened: Sender<LibraryOutcome>,
) {
    let backend = match podlib::open_library(&mount) {
        Ok(backend) => backend,
        Err(error) => {
            let _ = opened.send(LibraryOutcome::Failed {
                message: format!("Том смонтирован, но библиотека не открылась: {error}"),
                hint: Some(
                    "Проверьте, что на устройстве есть библиотека iTunes, и подключите заново."
                        .to_owned(),
                ),
            });
            return; // the request channel closes: the UI sees a dead worker
        }
    };
    let first = match list_snapshot(backend.as_ref()) {
        Ok(snapshot) => LibraryOutcome::Loaded(snapshot),
        Err(error) => {
            let (message, hint) = failure_from(error);
            let _ = opened.send(LibraryOutcome::Failed { message, hint });
            return;
        }
    };
    if opened.send(first).is_err() {
        return; // UI went away before the open finished
    }
    serve_loop(backend, incoming);
}

/// Command loop against an already-open backend. Split from
/// `worker_loop` so tests can inject a mock backend (a backend panicking
/// mid-command cannot be reproduced through a real device fixture).
fn serve_loop(
    mut backend: Box<dyn LibraryBackend>,
    incoming: Receiver<(LibraryCommand, LibraryReply)>,
) {
    while let Ok((command, reply)) = incoming.recv() {
        if matches!(command, LibraryCommand::Close) {
            drop(backend);
            let _ = reply.send(LibraryOutcome::Closed);
            return;
        }
        // catch_unwind: a panicking backend must still produce a reply,
        // otherwise the UI would wait forever. A panic leaves the backend
        // in unknown state (a persist may have been cut in half), so the
        // panic reply is the last thing this worker serves: dropping the
        // backend closes its files, and later requests fail on the dead
        // channel, which the UI reports as a disconnected worker.
        let outcome = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            dispatch(backend.as_mut(), command)
        })) {
            Ok(outcome) => outcome,
            Err(payload) => {
                // Stop serving honestly (see the WHY above): no further
                // command may reach a backend of unknown state.
                let _ = reply.send(LibraryOutcome::Failed {
                    message: format!(
                        "Внутренняя ошибка библиотеки: {}",
                        panic_message(payload)
                    ),
                    hint: Some(
                        "Состояние библиотеки после сбоя неизвестно; отключите и подключите \
                         устройство заново."
                            .to_owned(),
                    ),
                });
                drop(backend);
                return;
            }
        };
        if reply.send(outcome).is_err() {
            return; // UI closed
        }
    }
}

/// Reads the whole library (both listings share one snapshot).
fn list_snapshot(backend: &dyn LibraryBackend) -> Result<LibrarySnapshot, BackendError> {
    Ok(LibrarySnapshot {
        tracks: backend.tracks()?,
        playlists: backend.playlists()?,
    })
}

/// Executes one command against the backend and produces the reply.
/// Mutations re-list the library on success so the UI always renders the
/// state the backend persisted.
fn dispatch(backend: &mut dyn LibraryBackend, command: LibraryCommand) -> LibraryOutcome {
    match command {
        LibraryCommand::Refresh => match list_snapshot(backend) {
            Ok(snapshot) => LibraryOutcome::Loaded(snapshot),
            Err(error) => failure_outcome(error),
        },
        LibraryCommand::AddTracks(drafts) => {
            let total = drafts.len();
            let mut added = 0usize;
            let mut stopped = None;
            for draft in &drafts {
                match backend.add_track(draft) {
                    Ok(_) => added += 1,
                    Err(error) => {
                        stopped = Some(error);
                        break;
                    }
                }
            }
            match stopped {
                None => finish(backend, added_tracks_summary(added)),
                Some(error) if added == 0 => failure_outcome(error),
                // Partial success: show honestly how far it got.
                Some(error) => match list_snapshot(backend) {
                    Ok(snapshot) => LibraryOutcome::Applied {
                        snapshot,
                        summary: format!(
                            "{} из {total}; остановлено: {error}",
                            added_tracks_summary(added)
                        ),
                    },
                    Err(_) => failure_outcome(error),
                },
            }
        }
        LibraryCommand::RemoveTrack { id, purge_media } => match backend.remove_track(id, purge_media) {
            Ok(_) => finish(backend, "Трек удалён".to_owned()),
            Err(error) => failure_outcome(error),
        },
        LibraryCommand::EditTrack { id, changes } => match backend.edit_track(id, &changes) {
            Ok(track) => {
                // Contract: the resulting id is authoritative; edits are
                // remove-and-re-add, so it usually changes — surface it.
                let summary = if track.id == id {
                    format!("Обновлено: «{}»", track.title)
                } else {
                    format!("Обновлено: «{}» (новый id: {})", track.title, track.id)
                };
                finish(backend, summary)
            }
            Err(error) => failure_outcome(error),
        },
        LibraryCommand::CreatePlaylist { name } => match PlaylistDraft::new(&name, Vec::new()) {
            Ok(draft) => match backend.create_playlist(&draft) {
                Ok(_) => finish(backend, format!("Создан плейлист «{name}»")),
                Err(error) => failure_outcome(error),
            },
            Err(error) => failure_outcome(error.into()),
        },
        LibraryCommand::DeletePlaylist { id } => match backend.delete_playlist(id) {
            Ok(_) => finish(backend, "Плейлист удалён".to_owned()),
            Err(error) => failure_outcome(error),
        },
        LibraryCommand::AddToPlaylist {
            playlist,
            track_ids,
        } => match backend.add_to_playlist(playlist, &track_ids) {
            // Duplicates are ignored by contract, so the notice speaks of
            // requested rows, not of stored ones.
            Ok(_) => finish(
                backend,
                format!(
                    "Добавлено в плейлист: {} {}",
                    track_ids.len(),
                    plural(track_ids.len(), "трек", "трека", "треков")
                ),
            ),
            Err(error) => failure_outcome(error),
        },
        LibraryCommand::RemoveFromPlaylist {
            playlist,
            track_ids,
        } => match backend.remove_from_playlist(playlist, &track_ids) {
            Ok(_) => finish(backend, "Убрано из плейлиста".to_owned()),
            Err(error) => failure_outcome(error),
        },
        LibraryCommand::Close => LibraryOutcome::Closed, // handled in the loop
    }
}

/// Re-lists after a successful mutation and wraps the fresh snapshot.
fn finish(backend: &mut dyn LibraryBackend, summary: String) -> LibraryOutcome {
    match list_snapshot(backend) {
        Ok(snapshot) => LibraryOutcome::Applied { snapshot, summary },
        Err(error) => failure_outcome(error),
    }
}

/// Maps a backend error to a UI-facing message plus optional advice.
fn failure_from(error: BackendError) -> (String, Option<String>) {
    match &error {
        BackendError::MasterPlaylistImmutable { id } => (
            format!("Плейлист {id} — встроенный, его нельзя изменять"),
            Some(
                "Мастер-плейлист библиотеки доступен только для чтения; добавляйте треки \
                 в пользовательские плейлисты."
                    .to_owned(),
            ),
        ),
        BackendError::TrackNotFound(_) | BackendError::PlaylistNotFound(_) => (
            error.to_string(),
            Some("Список мог устареть — нажмите «Обновить».".to_owned()),
        ),
        BackendError::Io(_) => (
            format!("Ошибка ввода-вывода: {error}"),
            Some(
                "Проверьте, что устройство смонтировано и на нём есть свободное место."
                    .to_owned(),
            ),
        ),
        _ => (error.to_string(), None),
    }
}

fn failure_outcome(error: BackendError) -> LibraryOutcome {
    let (message, hint) = failure_from(error);
    LibraryOutcome::Failed { message, hint }
}

/// Russian plural form for `n` items: «1 трек», «2 трека», «5 треков».
/// Rules: `n % 10 == 1 && n % 100 != 11` → one; `n % 10` in 2..=4 and
/// `n % 100` not in 12..=14 → few; otherwise (including 0) → many.
pub(crate) fn plural<'a>(n: usize, one: &'a str, few: &'a str, many: &'a str) -> &'a str {
    let units = n % 10;
    let teens = n % 100;
    if units == 1 && teens != 11 {
        one
    } else if (2..=4).contains(&units) && !(12..=14).contains(&teens) {
        few
    } else {
        many
    }
}

/// «Добавлен 1 трек» / «Добавлено 2 трека» / «Добавлено 5 треков»: the
/// passive participle agrees with the count exactly like the noun does.
fn added_tracks_summary(added: usize) -> String {
    format!(
        "{} {} {}",
        plural(added, "Добавлен", "Добавлено", "Добавлено"),
        added,
        plural(added, "трек", "трека", "треков")
    )
}

/// Extracts a human-readable message from a panic payload. Takes the box
/// by value: it downcasts `&str`, then `String`, and falls back to a
/// placeholder for anything else; tests cover all three paths.
pub(crate) fn panic_message(payload: Box<dyn Any + Send>) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_owned()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "причина не сообщена".to_owned()
    }
}

/// File extensions accepted from drag-and-drop (case-insensitive). Tags
/// are not parsed: title = file stem, other fields stay empty — the same
/// defaults as `podlib-cli add` without flags.
pub const AUDIO_EXTENSIONS: [&str; 7] = ["mp3", "m4a", "aac", "wav", "flac", "ogg", "oga"];

/// Maps dropped file paths into track drafts. Returns the drafts plus
/// human-readable names of skipped paths (not an audio extension, or a
/// stem that cannot serve as a title) so the UI can show what was ignored.
pub fn drafts_from_paths(paths: &[PathBuf]) -> (Vec<TrackDraft>, Vec<String>) {
    let mut drafts = Vec::new();
    let mut skipped = Vec::new();
    for path in paths {
        let name = match path.file_name().and_then(|name| name.to_str()) {
            Some(name) => name.to_owned(),
            None => path.display().to_string(),
        };
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| extension.to_ascii_lowercase());
        let is_audio = extension.is_some_and(|extension| AUDIO_EXTENSIONS.contains(&extension.as_str()));
        let stem = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_default()
            .trim()
            .to_owned();
        match TrackDraft::new(path.clone(), stem) {
            // A blank stem fails title validation and lands in `skipped`.
            Ok(draft) if is_audio => drafts.push(draft),
            _ => skipped.push(name.to_owned()),
        }
    }
    (drafts, skipped)
}

/// Payload dragged from the track table: the ids of the dragged rows. A
/// `Vec` (one id today) so multi-row drag needs no payload type change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraggedTracks(pub Vec<TrackId>);

/// Guards a playlist mutation before a command is sent.
///
/// WHY the UI guards instead of trusting the backend: only the 7G
/// (libopod) adapter rejects master-member mutations. The classic
/// iTunesDB backend deliberately allows them, mirroring the firmware —
/// but they are pointless there as well: the master playlist is the
/// mirror of the whole library, and adding to it is a no-op (duplicate
/// members are ignored by contract). One consistent UI rule for both
/// device kinds, explained up front without a round trip to the device.
pub fn guard_playlist_mutation(playlist: &Playlist) -> Result<(), String> {
    if playlist.is_master {
        Err(format!(
            "«{}» — встроенный мастер-плейлист: в него нельзя ничего добавить или убрать",
            playlist.name
        ))
    } else {
        Ok(())
    }
}

/// Text fields of the edit dialog, exactly as shown on screen.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EditDialogFields {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_artist: String,
    pub track_number: String,
    pub disc_number: String,
}

impl EditDialogFields {
    pub fn from_track(track: &Track) -> Self {
        Self {
            title: track.title.clone(),
            artist: track.artist.clone().unwrap_or_default(),
            album: track.album.clone().unwrap_or_default(),
            album_artist: track.album_artist.clone().unwrap_or_default(),
            track_number: track.track_number.to_string(),
            disc_number: track.disc_number.to_string(),
        }
    }
}

/// Builds a sparse change set from the dialog: unchanged fields stay
/// `None` (no-op → the UI closes the dialog without a request), text
/// changed to empty clears the field (backend contract), numbers must
/// parse. NOTE: no genre field by design — libopod stores it on add but
/// never returns it on read, so an edit field would silently drop it.
pub fn changes_from_track(
    current: &Track,
    fields: &EditDialogFields,
) -> Result<TrackChanges, String> {
    let title = fields.title.trim();
    if title.is_empty() {
        return Err("Название не может быть пустым".to_owned());
    }
    let mut changes = TrackChanges::default();
    if title != current.title {
        changes.title = Some(title.to_owned());
    }
    if fields.artist != current.artist.clone().unwrap_or_default() {
        changes.artist = Some(fields.artist.clone());
    }
    if fields.album != current.album.clone().unwrap_or_default() {
        changes.album = Some(fields.album.clone());
    }
    if fields.album_artist != current.album_artist.clone().unwrap_or_default() {
        changes.album_artist = Some(fields.album_artist.clone());
    }
    changes.track_number = number_change(fields.track_number.trim(), current.track_number)?;
    changes.disc_number = number_change(fields.disc_number.trim(), current.disc_number)?;
    Ok(changes)
}

/// Blank means "keep current"; otherwise the text must parse and differ.
fn number_change(text: &str, current: u32) -> Result<Option<u32>, String> {
    if text.is_empty() {
        return Ok(None);
    }
    let parsed: u32 = text
        .parse()
        .map_err(|_| format!("«{text}» — не целое число"))?;
    if parsed == current {
        Ok(None)
    } else {
        Ok(Some(parsed))
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::Duration;

    use podlib::{build_nano2g_fixture, FixturePlaylist, FixtureTrack, PlaylistDraft};
    use tempfile::TempDir;

    use super::*;

    const TIMEOUT: Duration = Duration::from_secs(5);

    fn fixture_track(title: &str) -> FixtureTrack {
        FixtureTrack {
            title: title.to_owned(),
            artist: Some("Artist".to_owned()),
            album: Some("Album".to_owned()),
            genre: None,
            file_name: format!("{title}.mp3"),
            size_bytes: 1024,
            duration_ms: 60_000,
            track_number: 1,
            disc_number: 1,
        }
    }

    /// A mounted classic-device fixture with the given tracks/playlists.
    fn fixture_mount(tracks: &[FixtureTrack], playlists: &[FixturePlaylist]) -> TempDir {
        let dir = TempDir::new().unwrap();
        build_nano2g_fixture(dir.path(), tracks, playlists).unwrap();
        dir
    }

    fn opened_worker(mount: &Path) -> (LibraryWorker, LibrarySnapshot) {
        let (worker, rx) = LibraryWorker::spawn(mount.to_owned());
        match rx.recv_timeout(TIMEOUT) {
            Ok(LibraryOutcome::Loaded(snapshot)) => (worker, snapshot),
            other => panic!("expected Loaded, got {other:?}"),
        }
    }

    fn run(worker: &LibraryWorker, command: LibraryCommand) -> LibraryOutcome {
        let rx = worker.request(command).expect("worker must be alive");
        rx.recv_timeout(TIMEOUT).expect("worker must reply")
    }

    fn applied_snapshot(outcome: LibraryOutcome) -> LibrarySnapshot {
        match outcome {
            LibraryOutcome::Applied { snapshot, .. } => snapshot,
            other => panic!("expected Applied, got {other:?}"),
        }
    }

    fn failure_of(outcome: LibraryOutcome) -> (String, Option<String>) {
        match outcome {
            LibraryOutcome::Failed { message, hint } => (message, hint),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    // ---- Russian plural forms (P1) ----

    #[test]
    fn plural_picks_one_few_many_on_boundaries() {
        let cases = [
            (0, "треков"),   // ноль → родительный множественного
            (1, "трек"),     // один
            (2, "трека"),    // два-четыре
            (5, "треков"),   // пять и больше
            (11, "треков"),  // 11-14 — всегда «ов»
            (12, "треков"),
            (21, "трек"),    // ...1, кроме 11
            (22, "трека"),   // ...2, кроме 12-14
            (25, "треков"),
            (100, "треков"),
            (101, "трек"),   // круглое число + 1
            (111, "треков"), // 11 в конце — исключение из правила выше
        ];
        for (n, expected) in cases {
            assert_eq!(
                plural(n, "трек", "трека", "треков"),
                expected,
                "n = {n}"
            );
        }
    }

    #[test]
    fn added_tracks_summary_agrees_with_the_count() {
        assert_eq!(added_tracks_summary(1), "Добавлен 1 трек");
        assert_eq!(added_tracks_summary(2), "Добавлено 2 трека");
        assert_eq!(added_tracks_summary(5), "Добавлено 5 треков");
        assert_eq!(added_tracks_summary(11), "Добавлено 11 треков");
        assert_eq!(added_tracks_summary(21), "Добавлен 21 трек");
    }

    // ---- drafts_from_paths ----

    fn write_file(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"x").unwrap();
        path
    }

    #[test]
    fn drafts_from_paths_maps_audio_files_to_stem_titled_drafts() {
        let dir = TempDir::new().unwrap();
        let (drafts, skipped) = drafts_from_paths(&[
            write_file(dir.path(), "Song One.mp3"),
            write_file(dir.path(), "UPPER.M4A"),
            write_file(dir.path(), "notes.txt"),
            write_file(dir.path(), "noext"),
            dir.path().join("missing.flac"),
        ]);
        assert_eq!(drafts.len(), 3, "mp3 + uppercase m4a + missing flac pass");
        assert_eq!(drafts[0].title, "Song One");
        assert_eq!(drafts[0].artist, None, "tags are not parsed");
        assert_eq!(drafts[0].duration_ms, 0);
        assert_eq!(drafts[1].title, "UPPER", "extension match is case-insensitive");
        assert_eq!(drafts[2].source_path, dir.path().join("missing.flac"));
        assert_eq!(skipped, vec!["notes.txt", "noext"]);
    }

    #[test]
    fn drafts_from_paths_rejects_blank_stems() {
        let dir = TempDir::new().unwrap();
        let (drafts, skipped) = drafts_from_paths(&[write_file(dir.path(), ".mp3")]);
        assert!(drafts.is_empty(), "empty stem must not become a draft");
        assert_eq!(skipped, vec![".mp3"]);
    }

    // ---- changes_from_track (edit dialog → TrackChanges) ----

    fn sample_track() -> Track {
        Track {
            id: TrackId::from_bits(42),
            title: "Alpha".to_owned(),
            artist: Some("Artist".to_owned()),
            album: None,
            album_artist: None,
            duration_ms: 60_000,
            track_number: 1,
            disc_number: 2,
            size_bytes: 1024,
            has_artwork: false,
        }
    }

    #[test]
    fn unchanged_dialog_fields_produce_empty_changes() {
        let track = sample_track();
        let fields = EditDialogFields::from_track(&track);
        let changes = changes_from_track(&track, &fields).unwrap();
        assert!(changes.is_empty(), "no-op dialog must send nothing");
    }

    #[test]
    fn dialog_diff_maps_edits_and_clearing() {
        let track = sample_track();
        let fields = EditDialogFields {
            title: "Beta".to_owned(),
            artist: String::new(), // clears the field (contract)
            album: "New Album".to_owned(),
            album_artist: String::new(),
            track_number: String::new(), // blank keeps current
            disc_number: "5".to_owned(),
        };
        let changes = changes_from_track(&track, &fields).unwrap();
        assert_eq!(changes.title.as_deref(), Some("Beta"));
        assert_eq!(changes.artist.as_deref(), Some(""));
        assert_eq!(changes.album.as_deref(), Some("New Album"));
        assert_eq!(changes.album_artist, None, "was None, stays None");
        assert_eq!(changes.track_number, None, "blank keeps current");
        assert_eq!(changes.disc_number, Some(5));
    }

    #[test]
    fn dialog_rejects_empty_title_and_bad_numbers() {
        let track = sample_track();
        let mut fields = EditDialogFields::from_track(&track);
        fields.title = "   ".to_owned();
        assert!(changes_from_track(&track, &fields).is_err());
        let mut fields = EditDialogFields::from_track(&track);
        fields.track_number = "abc".to_owned();
        let error = changes_from_track(&track, &fields).unwrap_err();
        assert!(error.contains("abc"));
    }

    // ---- DnD payload mapping ----

    #[test]
    fn dragged_tracks_payload_roundtrips() {
        let payload: Box<dyn Any + Send + Sync> = Box::new(DraggedTracks(vec![
            TrackId::from_bits(7),
            TrackId::from_bits(9),
        ]));
        let ids = payload.downcast_ref::<DraggedTracks>().map(|dragged| dragged.0.clone());
        assert_eq!(ids, Some(vec![TrackId::from_bits(7), TrackId::from_bits(9)]));
    }

    #[test]
    fn foreign_payload_does_not_downcast_to_dragged_tracks() {
        let payload: Box<dyn Any + Send + Sync> = Box::new(123u32);
        assert!(payload.downcast_ref::<DraggedTracks>().is_none());
    }

    // ---- master-playlist guard ----

    #[test]
    fn guard_rejects_master_and_allows_user_playlist() {
        let master = Playlist {
            id: PlaylistId::from_bits(1),
            name: "Медиатека".to_owned(),
            track_ids: Vec::new(),
            is_master: true,
            is_smart: false,
        };
        let user = Playlist {
            id: PlaylistId::from_bits(2),
            name: "Mix".to_owned(),
            track_ids: Vec::new(),
            is_master: false,
            is_smart: false,
        };
        let error = guard_playlist_mutation(&master).unwrap_err();
        assert!(error.contains("встроенный"));
        assert!(guard_playlist_mutation(&user).is_ok());
    }

    // ---- panic_message (moved from app.rs round 1) ----

    #[test]
    fn panic_message_handles_common_payloads() {
        assert_eq!(panic_message(Box::new("str panic")), "str panic");
        assert_eq!(
            panic_message(Box::new(String::from("boxed panic"))),
            "boxed panic"
        );
        assert_eq!(panic_message(Box::new(7u32)), "причина не сообщена");
    }

    // ---- worker end-to-end against the classic fixture backend ----

    #[test]
    fn worker_open_lists_fixture_library() {
        let dir = fixture_mount(&[fixture_track("Alpha")], &[]);
        let (worker, snapshot) = opened_worker(dir.path());
        assert_eq!(snapshot.tracks.len(), 1);
        assert_eq!(snapshot.tracks[0].title, "Alpha");
        let master = snapshot
            .playlists
            .iter()
            .find(|playlist| playlist.is_master)
            .expect("fixture library must expose the master playlist");
        assert_eq!(master.track_ids.len(), 1);
        worker.close_and_join();
    }

    #[test]
    fn worker_add_tracks_applies_and_reports() {
        let dir = fixture_mount(&[fixture_track("Alpha")], &[]);
        let (worker, _snapshot) = opened_worker(dir.path());
        let host = TempDir::new().unwrap();
        let paths = vec![
            write_file(host.path(), "One.mp3"),
            write_file(host.path(), "Two.mp3"),
        ];
        let (drafts, skipped) = drafts_from_paths(&paths);
        assert!(skipped.is_empty());
        let outcome = run(&worker, LibraryCommand::AddTracks(drafts));
        let snapshot = applied_snapshot(outcome);
        assert_eq!(snapshot.tracks.len(), 3);
        worker.close_and_join();
    }

    #[test]
    fn worker_add_tracks_partial_reports_how_far_it_got() {
        let dir = fixture_mount(&[fixture_track("Alpha")], &[]);
        let (worker, _snapshot) = opened_worker(dir.path());
        let host = TempDir::new().unwrap();
        let paths = vec![
            write_file(host.path(), "Good.mp3"),
            // Second source does not exist: the backend fails copying it.
            dir.path().join("nope.mp3"),
        ];
        let (drafts, _) = drafts_from_paths(&paths);
        let outcome = run(&worker, LibraryCommand::AddTracks(drafts));
        let snapshot = applied_snapshot(outcome);
        assert_eq!(snapshot.tracks.len(), 2, "only the first file lands");
        worker.close_and_join();
    }

    #[test]
    fn worker_edit_updates_title_and_reports_id() {
        let dir = fixture_mount(&[fixture_track("Alpha")], &[]);
        let (worker, snapshot) = opened_worker(dir.path());
        let id = snapshot.tracks[0].id;
        let changes = TrackChanges {
            title: Some("Beta".to_owned()),
            ..TrackChanges::default()
        };
        match run(&worker, LibraryCommand::EditTrack { id, changes }) {
            LibraryOutcome::Applied { snapshot, summary } => {
                assert_eq!(snapshot.tracks[0].title, "Beta");
                // Remove-and-re-add may keep or rotate the id; the summary
                // must stay truthful either way.
                let rotated = snapshot.tracks.iter().all(|track| track.id != id);
                if rotated {
                    assert!(summary.contains("новый id"), "got: {summary}");
                } else {
                    assert!(summary.contains("Обновлено"), "got: {summary}");
                    assert!(!summary.contains("новый id"));
                }
            }
            other => panic!("expected Applied, got {other:?}"),
        }
        worker.close_and_join();
    }

    #[test]
    fn worker_playlist_lifecycle_and_members() {
        let dir = fixture_mount(&[fixture_track("Alpha")], &[]);
        let (worker, snapshot) = opened_worker(dir.path());
        let track_id = snapshot.tracks[0].id;

        let snapshot = applied_snapshot(run(
            &worker,
            LibraryCommand::CreatePlaylist {
                name: "Mix".to_owned(),
            },
        ));
        assert_eq!(snapshot.playlists.len(), 2, "master + the new one");
        let playlist = snapshot
            .playlists
            .iter()
            .find(|playlist| !playlist.is_master)
            .expect("user playlist must appear");
        let playlist_id = playlist.id;

        let snapshot = applied_snapshot(run(
            &worker,
            LibraryCommand::AddToPlaylist {
                playlist: playlist_id,
                track_ids: vec![track_id],
            },
        ));
        let playlist = snapshot.playlist(playlist_id).unwrap();
        assert_eq!(playlist.track_ids, vec![track_id]);

        let snapshot = applied_snapshot(run(
            &worker,
            LibraryCommand::RemoveFromPlaylist {
                playlist: playlist_id,
                track_ids: vec![track_id],
            },
        ));
        assert!(snapshot.playlist(playlist_id).unwrap().track_ids.is_empty());

        let snapshot = applied_snapshot(run(
            &worker,
            LibraryCommand::DeletePlaylist {
                id: playlist_id,
            },
        ));
        assert_eq!(snapshot.playlists.len(), 1, "only master remains");
        worker.close_and_join();
    }

    #[test]
    fn failure_from_maps_master_immutable_error_with_hint() {
        let (message, hint) = failure_from(BackendError::MasterPlaylistImmutable {
            id: PlaylistId::from_bits(16),
        });
        assert!(message.contains("встроенный"), "got: {message}");
        assert!(hint.is_some());
    }

    #[test]
    fn classic_fixture_allows_master_members_like_the_firmware() {
        // The classic iTunesDB backend mirrors firmware behaviour: the
        // master playlist CAN hold members. The 7G (libopod) adapter is
        // the one rejecting master mutations; the UI guards master
        // anyway so the experience is consistent on both device kinds.
        let dir = fixture_mount(&[fixture_track("Alpha")], &[]);
        let (worker, snapshot) = opened_worker(dir.path());
        let master = snapshot.playlists.iter().find(|p| p.is_master).unwrap();
        let snapshot = applied_snapshot(run(
            &worker,
            LibraryCommand::AddToPlaylist {
                playlist: master.id,
                track_ids: vec![snapshot.tracks[0].id],
            },
        ));
        assert_eq!(snapshot.playlist(master.id).unwrap().track_ids.len(), 1);
        worker.close_and_join();
    }

    #[test]
    fn worker_remove_track_shrinks_snapshot() {
        let dir = fixture_mount(&[fixture_track("Alpha"), fixture_track("Beta")], &[]);
        let (worker, snapshot) = opened_worker(dir.path());
        let id = snapshot.tracks[0].id;
        let snapshot = applied_snapshot(run(
            &worker,
            LibraryCommand::RemoveTrack {
                id,
                purge_media: false,
            },
        ));
        assert_eq!(snapshot.tracks.len(), 1);
        assert!(snapshot.track(id).is_none());
        worker.close_and_join();
    }

    #[test]
    fn worker_refresh_reloads_snapshot() {
        let dir = fixture_mount(&[fixture_track("Alpha")], &[]);
        let (worker, snapshot) = opened_worker(dir.path());
        match run(&worker, LibraryCommand::Refresh) {
            LibraryOutcome::Loaded(again) => assert_eq!(again.tracks.len(), snapshot.tracks.len()),
            other => panic!("expected Loaded, got {other:?}"),
        }
        worker.close_and_join();
    }

    #[test]
    fn worker_close_replies_closed_and_shuts_down() {
        let dir = fixture_mount(&[fixture_track("Alpha")], &[]);
        let (worker, _snapshot) = opened_worker(dir.path());
        match run(&worker, LibraryCommand::Close) {
            LibraryOutcome::Closed => {}
            other => panic!("expected Closed, got {other:?}"),
        }
        assert!(
            matches!(
                worker.request(LibraryCommand::Refresh),
                Err(message) if message.contains("не активен")
            ),
            "worker must refuse commands after Close"
        );
        worker.close_and_join(); // already exiting; must not hang
    }

    #[test]
    fn worker_open_failure_reports_failed_and_shuts_down() {
        let dir = TempDir::new().unwrap(); // no library inside
        let (worker, rx) = LibraryWorker::spawn(dir.path().to_owned());
        match rx.recv_timeout(TIMEOUT) {
            Ok(LibraryOutcome::Failed { message, .. }) => {
                assert!(message.contains("не открылась"), "got: {message}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(worker.request(LibraryCommand::Refresh).is_err());
    }

    // ---- panic isolation (mock backend; F1) ----

    /// Mock backend: honest on reads, panics on every mutation. Drives
    /// the worker's panic-isolation path, which a real fixture cannot.
    struct PanickingBackend;

    impl LibraryBackend for PanickingBackend {
        fn tracks(&self) -> Result<Vec<Track>, BackendError> {
            Ok(Vec::new())
        }

        fn playlists(&self) -> Result<Vec<Playlist>, BackendError> {
            Ok(Vec::new())
        }

        fn add_track(&mut self, _draft: &TrackDraft) -> Result<TrackId, BackendError> {
            panic!("backend exploded mid-write")
        }

        fn remove_track(&mut self, _id: TrackId, _purge_media: bool) -> Result<bool, BackendError> {
            panic!("backend exploded mid-write")
        }

        fn edit_track(
            &mut self,
            _id: TrackId,
            _changes: &TrackChanges,
        ) -> Result<Track, BackendError> {
            panic!("backend exploded mid-write")
        }

        fn create_playlist(&mut self, _draft: &PlaylistDraft) -> Result<PlaylistId, BackendError> {
            panic!("backend exploded mid-write")
        }

        fn delete_playlist(&mut self, _id: PlaylistId) -> Result<bool, BackendError> {
            panic!("backend exploded mid-write")
        }

        fn add_to_playlist(
            &mut self,
            _playlist: PlaylistId,
            _track_ids: &[TrackId],
        ) -> Result<(), BackendError> {
            panic!("backend exploded mid-write")
        }

        fn remove_from_playlist(
            &mut self,
            _playlist: PlaylistId,
            _track_ids: &[TrackId],
        ) -> Result<(), BackendError> {
            panic!("backend exploded mid-write")
        }
    }

    #[test]
    fn worker_stops_serving_after_backend_panic() {
        // The backend is built inside the worker thread (not `Send`).
        let (worker, _opened) =
            LibraryWorker::spawn_with_backend(|| Box::new(PanickingBackend));
        // A real mutation (non-empty drafts), so the backend actually
        // panics inside `add_track`; the worker must still answer.
        let draft =
            TrackDraft::new(PathBuf::from("/tmp/nope.mp3"), "X").expect("draft must be valid");
        let rx = worker
            .request(LibraryCommand::AddTracks(vec![draft]))
            .expect("worker must accept commands before the panic");
        match rx.recv_timeout(TIMEOUT) {
            Ok(LibraryOutcome::Failed { message, hint }) => {
                assert!(
                    message.contains("Внутренняя ошибка библиотеки"),
                    "got: {message}"
                );
                assert!(hint.is_some(), "panic reply must advise reconnecting");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        // The worker is gone: later requests fail on the dead channel
        // instead of reaching a backend of unknown state.
        assert!(
            matches!(
                worker.request(LibraryCommand::Refresh),
                Err(message) if message.contains("не активен")
            ),
            "worker must stop serving after a backend panic"
        );
        worker.close_and_join(); // must not hang on a dead worker
    }

    #[test]
    fn worker_survives_noop_request_while_alive() {
        let dir = fixture_mount(&[fixture_track("Alpha")], &[]);
        let (worker, _snapshot) = opened_worker(dir.path());
        // Requests keep working while the worker lives; the dead-worker
        // path is covered by the open-failure test above.
        assert!(worker.request(LibraryCommand::Refresh).is_ok());
        worker.close_and_join();
    }

    #[test]
    fn timeout_helper_reports_disconnected_worker() {
        // Documents the UI-side drain contract: Disconnected after the
        // worker died mid-operation, Empty while it is still working.
        let (tx, rx) = channel::<LibraryOutcome>();
        drop(tx);
        assert!(matches!(rx.try_recv(), Err(std::sync::mpsc::TryRecvError::Disconnected)));
        let (tx, rx) = channel::<LibraryOutcome>();
        let _tx = tx; // keep alive
        assert!(matches!(rx.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)));
        assert!(matches!(
            rx.recv_timeout(Duration::from_millis(1)),
            Err(RecvTimeoutError::Timeout)
        ));
    }

    #[test]
    fn empty_playlist_name_is_rejected_by_worker() {
        let dir = fixture_mount(&[fixture_track("Alpha")], &[]);
        let (worker, _snapshot) = opened_worker(dir.path());
        let (message, _hint) = failure_of(run(
            &worker,
            LibraryCommand::CreatePlaylist {
                name: "   ".to_owned(),
            },
        ));
        assert!(!message.is_empty());
        worker.close_and_join();
    }

    #[test]
    fn playlist_draft_is_send_across_threads() {
        // Guards the core design constraint: commands must stay Send.
        fn assert_send<T: Send>() {}
        assert_send::<LibraryCommand>();
        assert_send::<LibrarySnapshot>();
        let draft = PlaylistDraft::new("x", Vec::new()).unwrap();
        let command = LibraryCommand::CreatePlaylist {
            name: draft.name.clone(),
        };
        let (tx, rx) = channel();
        std::thread::spawn(move || tx.send(command).unwrap()).join().unwrap();
        assert!(rx.recv_timeout(TIMEOUT).is_ok());
    }
}
