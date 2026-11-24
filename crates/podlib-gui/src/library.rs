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

use crate::i18n::{Key, added_tracks, lang, n_tracks, tr, trf};
use podlib::{
    assign_album_track_numbers, BackendError, BatchEditOutcome, LibraryBackend, Playlist,
    PlaylistDraft, PlaylistId, Track, TrackChanges, TrackDraft, TrackId,
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
    /// Batch tag edit: assigns one artist to every listed track.
    /// Per-id failures do not stop the batch — the honest tail in the
    /// summary reports each one (id/title + reason).
    SetArtist { track_ids: Vec<TrackId>, artist: String },
    CreatePlaylist { name: String },
    DeletePlaylist { id: PlaylistId },
    /// Renames a user playlist in place (id and membership untouched).
    RenamePlaylist { id: PlaylistId, name: String },
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
        // a dead worker whose `request()` reports the worker-inactive
        // message (i18n key `ErrWorkerInactive`).
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
                    message: trf(Key::ErrSpawnThread, &[("error", &error.to_string())]),
                    hint: Some(tr(Key::HintReconnect).to_owned()),
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
            .ok_or_else(|| tr(Key::ErrWorkerInactive).to_owned())?;
        let (reply_tx, reply_rx) = channel();
        requests
            .send((command, reply_tx))
            .map(|_| reply_rx)
            .map_err(|_| tr(Key::ErrWorkerInactive).to_owned())
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
                message: trf(Key::ErrOpenFailed, &[("error", &error.to_string())]),
                hint: Some(tr(Key::HintCheckItunes).to_owned()),
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
        // Round 5.3 diagnostics (permanent): every command and outcome
        // lands in podlib-gui.log — the «тихо ничего не сделал» class
        // of reports must be answerable from the log alone.
        eprintln!("[worker] cmd={command:?}");
        if matches!(command, LibraryCommand::Close) {
            eprintln!("[worker] outcome=closed");
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
                    message: trf(
                        Key::ErrLibPanic,
                        &[("message", &panic_message(payload))],
                    ),
                    hint: Some(tr(Key::HintLibStateUnknown).to_owned()),
                });
                drop(backend);
                return;
            }
        };
        match &outcome {
            LibraryOutcome::Applied { summary, .. } => {
                eprintln!("[worker] outcome=applied summary={summary:?}");
            }
            LibraryOutcome::Loaded(snapshot) => {
                eprintln!(
                    "[worker] outcome=loaded tracks={} playlists={}",
                    snapshot.tracks.len(),
                    snapshot.playlists.len()
                );
            }
            LibraryOutcome::Failed { message, .. } => {
                eprintln!("[worker] outcome=failed message={message:?}");
            }
            LibraryOutcome::Closed => eprintln!("[worker] outcome=closed"),
        }
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
        LibraryCommand::AddTracks(drafts) => add_tracks_command(backend, drafts),
        LibraryCommand::RemoveTrack { id, purge_media } => match backend.remove_track(id, purge_media) {
            Ok(_) => finish(backend, tr(Key::TrackDeleted).to_owned()),
            Err(error) => failure_outcome(error),
        },
        LibraryCommand::EditTrack { id, changes } => edit_track_command(backend, id, changes),
        LibraryCommand::SetArtist {
            track_ids,
            artist,
        } => set_artist_command(backend, track_ids, artist),
        LibraryCommand::CreatePlaylist { name } => match PlaylistDraft::new(&name, Vec::new()) {
            Ok(draft) => match backend.create_playlist(&draft) {
                Ok(_) => finish(backend, trf(Key::PlaylistCreated, &[("name", &name)])),
                Err(error) => failure_outcome(error),
            },
            Err(error) => failure_outcome(error.into()),
        },
        LibraryCommand::DeletePlaylist { id } => match backend.delete_playlist(id) {
            Ok(_) => finish(backend, tr(Key::PlaylistDeleted).to_owned()),
            Err(error) => failure_outcome(error),
        },
        LibraryCommand::RenamePlaylist { id, name } => match backend.rename_playlist(id, &name) {
            // The id never rotates (rename is in place), so the notice
            // needs no id-change tail.
            Ok(_) => finish(backend, trf(Key::PlaylistRenamed, &[("name", &name)])),
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
                trf(
                    Key::AddedToPlaylist,
                    &[("tracks", &n_tracks(lang(), track_ids.len()))],
                ),
            ),
            Err(error) => failure_outcome(error),
        },
        LibraryCommand::RemoveFromPlaylist {
            playlist,
            track_ids,
        } => match backend.remove_from_playlist(playlist, &track_ids) {
            Ok(_) => finish(backend, tr(Key::RemovedFromPlaylist).to_owned()),
            Err(error) => failure_outcome(error),
        },
        LibraryCommand::Close => LibraryOutcome::Closed, // handled in the loop
    }
}

/// Adds a batch of drafts; firmware-unplayable formats are transcoded
/// first (round 5.2), passthrough drafts get their real duration
/// probed (round 6.1 B1), and a success re-lists the library.
fn add_tracks_command(
    backend: &mut dyn LibraryBackend,
    drafts: Vec<TrackDraft>,
) -> LibraryOutcome {
    let mut drafts = drafts;
    // Round 6.1 B2: drafts resolving to one device album (chapters of
    // a book) get 1..N numbering and the shared count before writing.
    assign_album_track_numbers(&mut drafts);
    let total = drafts.len();
    // Round 5.2: firmware-unplayable formats (flac/ogg/oga) are
    // transcoded to AAC in this worker thread; the batch tempdir
    // lives until the batch ends and cleans itself up on drop.
    let transcode_dir = match tempfile::tempdir() {
        Ok(dir) => Some(dir),
        Err(error) => {
            return failure_outcome(BackendError::Io(std::io::Error::other(error)));
        }
    };
    let mut added = 0usize;
    let mut stopped = None;
    let mut add_failures: Vec<String> = Vec::new();
    for (index, draft) in drafts.iter().enumerate() {
        let prepared;
        if needs_transcode(&path_extension(&draft.source_path)) {
            if let Some(dir) = transcode_dir.as_ref().map(|dir| dir.path()) {
                match transcode_draft(draft, dir, index) {
                    Ok(mut transcoded) => {
                        // Round 6.1 B1 on the transcode path: the
                        // transcoded file is fresh ffmpeg output, so
                        // its duration is still the DnD zero — probe
                        // the real one here. Same gate as passthrough
                        // (a caller-supplied duration is trusted) and
                        // the same honest skip on a failed probe.
                        if transcoded.duration_ms == 0 {
                            match probe_duration_ms(&transcoded.source_path) {
                                Ok(duration_ms) => transcoded.duration_ms = duration_ms,
                                Err(reason) => {
                                    add_failures.push(trf(
                                        Key::AddFailureItem,
                                        &[("title", &draft.title), ("reason", &reason)],
                                    ));
                                    continue;
                                }
                            }
                        }
                        prepared = Some(transcoded);
                    }
                    Err(reason) => {
                        // The batch continues: the failed file is
                        // reported honestly in the summary tail.
                        add_failures.push(trf(
                            Key::AddFailureItem,
                            &[("title", &draft.title), ("reason", &reason)],
                        ));
                        continue;
                    }
                }
            } else {
                // Unreachable while the batch tempdir owns the path, but
                // the draft is skipped honestly instead of panicking.
                add_failures.push(trf(Key::TranscodeNoTempDir, &[("title", &draft.title)]));
                continue;
            }
        } else if draft.duration_ms == 0 {
            // Round 6.1 B1: a passthrough file's duration comes from
            // nowhere else — probe it here. The iPod refuses to play a
            // track whose total_time_ms stayed 0 (the live mp3-book
            // failure), so a failed probe skips the draft honestly
            // instead of adding a silent zero.
            match probe_duration_ms(&draft.source_path) {
                Ok(duration_ms) => {
                    let mut probed = draft.clone();
                    probed.duration_ms = duration_ms;
                    prepared = Some(probed);
                }
                Err(reason) => {
                    add_failures.push(trf(
                        Key::AddFailureItem,
                        &[("title", &draft.title), ("reason", &reason)],
                    ));
                    continue;
                }
            }
        } else {
            // The caller already supplied a duration — trust it and
            // skip the probe.
            prepared = None;
        }
        let target = prepared.as_ref().unwrap_or(draft);
        match backend.add_track(target) {
            Ok(_) => added += 1,
            Err(error) => {
                stopped = Some(error);
                break;
            }
        }
    }
    let failures_tail = add_failures_summary(&add_failures);
    match stopped {
        None if added == total && add_failures.is_empty() => {
            finish(backend, added_tracks(lang(), added))
        }
        // Honest partial success without a backend error: some
        // drafts failed their transcode or their duration probe
        // and were skipped.
        None => {
            let mut summary = trf(
                Key::SummaryPartial,
                &[
                    ("added", &added_tracks(lang(), added)),
                    ("total", &total.to_string()),
                ],
            );
            summary.push_str(&failures_tail);
            finish(backend, summary)
        }
        Some(error) if added == 0 && add_failures.is_empty() => {
            failure_outcome(error)
        }
        // Partial success: show honestly how far it got.
        Some(error) => match list_snapshot(backend) {
            Ok(snapshot) => {
                let mut summary = trf(
                    Key::AddStopped,
                    &[
                        ("added", &added_tracks(lang(), added)),
                        ("total", &total.to_string()),
                        ("error", &error.to_string()),
                    ],
                );
                summary.push_str(&failures_tail);
                LibraryOutcome::Applied { snapshot, summary }
            }
            Err(_) => failure_outcome(error),
        },
    }
}

/// Applies tag edits to one track and re-lists on success.
fn edit_track_command(
    backend: &mut dyn LibraryBackend,
    id: TrackId,
    changes: TrackChanges,
) -> LibraryOutcome {
    match backend.edit_track(id, &changes) {
        Ok(track) => {
            // Contract: the resulting id is authoritative; edits are
            // remove-and-re-add, so it usually changes — surface it.
            let summary = if track.id == id {
                trf(Key::UpdatedTrack, &[("title", &track.title)])
            } else {
                trf(
                    Key::UpdatedTrackNewId,
                    &[("title", &track.title), ("id", &track.id.to_string())],
                )
            };
            finish(backend, summary)
        }
        Err(error) => failure_outcome(error),
    }
}

/// Batch «Задать исполнителя»: one backend batch call (on libopod
/// backends a single edit session — N×replace_track, one stage+install,
/// instead of N full device installs). The backend itself skips tracks
/// that already carry the target artist ([`BatchEditOutcome::Unchanged`]
/// — pid does not rotate, re-runs are free), so the worker only counts
/// the buckets and reports each honestly. Titles for the failure tail
/// come from the pre-edit listing: per-id backend failures name opaque
/// ids only.
fn set_artist_command(
    backend: &mut dyn LibraryBackend,
    track_ids: Vec<TrackId>,
    artist: String,
) -> LibraryOutcome {
    let total = track_ids.len();
    let titles: std::collections::BTreeMap<TrackId, String> = match list_snapshot(backend) {
        Ok(snapshot) => snapshot
            .tracks
            .into_iter()
            .map(|track| (track.id, track.title))
            .collect(),
        Err(error) => return failure_outcome(error),
    };
    let outcomes = match backend.set_artist_batch(&track_ids, &artist) {
        Ok(outcomes) => outcomes,
        Err(error) => return failure_outcome(error),
    };
    let mut updated = 0_usize;
    let mut unchanged = 0_usize;
    let mut failures: Vec<String> = Vec::new();
    for (id, outcome) in &outcomes {
        match outcome {
            BatchEditOutcome::Updated(_) => updated += 1,
            BatchEditOutcome::Unchanged => unchanged += 1,
            BatchEditOutcome::Failed(reason) => {
                let title = titles
                    .get(id)
                    .map_or_else(|| id.to_string(), |title| trf(Key::QuotedTitle, &[("title", title)]));
                failures.push(format!("{title}: {reason}"));
            }
        }
    }
    if updated == 0 && !failures.is_empty() {
        let (message, hint) = if failures.len() == 1 {
            (failures[0].clone(), None)
        } else {
            (
                trf(
                    Key::ArtistNoneSucceeded,
                    &[
                        ("count", &failures.len().to_string()),
                        ("total", &total.to_string()),
                        ("list", &failures.join("; ")),
                    ],
                ),
                Some(tr(Key::HintStaleList).to_owned()),
            )
        };
        return LibraryOutcome::Failed { message, hint };
    }
    let unchanged_tail = if unchanged > 0 {
        trf(Key::ArtistUnchangedTail, &[("count", &unchanged.to_string())])
    } else {
        String::new()
    };
    let summary = if updated == 0 {
        // Everything already carried the artist: an honest «уже задан»
        // instead of a fabricated success count (failures are handled
        // above, so `unchanged` covers the whole batch).
        trf(
            Key::ArtistAlreadySet,
            &[("tracks", &n_tracks(lang(), unchanged))],
        )
    } else if updated == total {
        trf(
            Key::ArtistSetAll,
            &[("tracks", &n_tracks(lang(), total)), ("tail", &unchanged_tail)],
        )
    } else {
        let mut summary = trf(
            Key::ArtistSetPartial,
            &[
                ("updated", &updated.to_string()),
                ("total", &total.to_string()),
                ("tail", &unchanged_tail),
            ],
        );
        if !failures.is_empty() {
            summary.push_str(tr(Key::ArtistFailuresTail));
            summary.push_str(&failures.join("; "));
        }
        summary
    };
    finish(backend, summary)
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
            trf(Key::ErrMasterImmutable, &[("id", &id.to_string())]),
            Some(tr(Key::HintMasterReadonly).to_owned()),
        ),
        BackendError::TrackNotFound(_) | BackendError::PlaylistNotFound(_) => (
            error.to_string(),
            Some(tr(Key::HintStaleList).to_owned()),
        ),
        BackendError::Io(_) => (
            trf(Key::ErrIo, &[("error", &error.to_string())]),
            Some(tr(Key::HintIoMountSpace).to_owned()),
        ),
        _ => (error.to_string(), None),
    }
}

fn failure_outcome(error: BackendError) -> LibraryOutcome {
    let (message, hint) = failure_from(error);
    LibraryOutcome::Failed { message, hint }
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
        tr(Key::PanicNoReason).to_owned()
    }
}

/// File extensions accepted from drag-and-drop (case-insensitive).
/// Native Nano 7G firmware formats per the Apple spec pass through
/// untouched: mp3, AAC-family (m4a/m4b/aac), WAV (since 2026-11-21),
/// AIFF (aiff/aif) and Audible (aa/aax). `m4b`/`aa`/`aax` are
/// audiobook containers → drafts get audiobook bits (round 5.1 and
/// 2026-11-21). flac/ogg/oga have no firmware decoder and are
/// transcoded in the worker (round 5.2).
///
/// The DB write is format-agnostic: vendored libopod hardcodes the
/// `'MP3 '` fourcc for every track, and the actual file keeps its
/// own extension. Playback of passthrough formats other than mp3
/// still needs the live device check (release gate) — the vendor
/// media gate accepted mp3 sources only until 2026-11-21.
pub const AUDIO_EXTENSIONS: [&str; 12] = [
    "mp3", "m4a", "m4b", "aac", "wav", "aiff", "aif", "aa", "aax", "flac", "ogg", "oga",
];

/// Extensions that mark an audiobook file (bits 8): the AAC audiobook
/// container and the native Audible formats.
const AUDIOBOOK_EXTENSIONS: [&str; 3] = ["m4b", "aa", "aax"];

/// Raw iTunesDB media-kind bit of audiobooks (round 5.1). Checks use
/// the semantic bit test (`bits & AUDIOBOOK_BIT != 0`) — an exact
/// `== 8` breaks for combined bits (round 6 tail nit-6).
pub(crate) const AUDIOBOOK_BIT: u32 = 8;

/// Lowercased extension of a path, or an empty string when absent.
pub(crate) fn path_extension(path: &std::path::Path) -> String {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
        .unwrap_or_default()
}

/// True for formats the Nano 7G firmware cannot play (flac/ogg/oga
/// have no firmware decoder): the worker must transcode them before
/// add_track. WAV joined the passthrough on 2026-11-21 — the
/// spec lists it as native (round 5.2 had bundled it with flac/ogg).
pub(crate) fn needs_transcode(extension: &str) -> bool {
    matches!(extension, "flac" | "ogg" | "oga")
}

/// Codec routing of the transcode by source extension: FLAC is
/// lossless, so it re-wraps into ALAC bit-perfectly (the e2e PCM test
/// proves it); Ogg sources are lossy — a zero-additional-loss path
/// does not exist, so they re-encode to VBR AAC (owner decision
/// 2026-11-21: compact wins over size).
fn transcode_codec(extension: &str) -> &'static str {
    match extension {
        "flac" => "alac",
        _ => "aac", // ogg, oga
    }
}

/// Target container extension of a transcode: audiobook bits get the
/// book container, everything else the music one.
fn transcode_target_extension(media_kind_bits: u32) -> &'static str {
    if media_kind_bits & AUDIOBOOK_BIT != 0 {
        "m4b"
    } else {
        "m4a"
    }
}

/// Exact argument list of the transcode command: silence the banner,
/// drop any cover video stream, keep chapter metadata, encode with the
/// routed codec (ALAC for lossless sources, VBR AAC otherwise) and mux
/// into the iPod container.
fn ffmpeg_transcode_args(
    source: &std::path::Path,
    target: &std::path::Path,
    codec: &str,
) -> Vec<String> {
    let mut args = vec![
        "-hide_banner".to_owned(),
        "-loglevel".to_owned(),
        "error".to_owned(),
        "-y".to_owned(),
        "-i".to_owned(),
        source.display().to_string(),
        "-vn".to_owned(),
        "-map_chapters".to_owned(),
        "0".to_owned(),
        "-c:a".to_owned(),
        codec.to_owned(),
    ];
    if codec == "aac" {
        // Native AAC VBR quality 5 (~250k average).
        args.push("-q:a".to_owned());
        args.push("5".to_owned());
    }
    args.push("-f".to_owned());
    args.push("ipod".to_owned());
    args.push(target.display().to_string());
    args
}

/// Honest batch tail for transcoding failures, appended to the summary.
/// The honest failures tail of an add summary: transcode or duration
/// probe problems, one line per skipped draft.
fn add_failures_summary(failures: &[String]) -> String {
    if failures.is_empty() {
        String::new()
    } else {
        trf(Key::AddFailuresTail, &[("list", &failures.join("; "))])
    }
}

/// Recursively collects audio files under `directory` (std fs walk):
/// hidden entries (leading dot) and non-audio files are skipped.
fn collect_audio_files(directory: &std::path::Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let hidden = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with('.'));
        if hidden {
            continue;
        }
        if path.is_dir() {
            collect_audio_files(&path, out);
        } else if AUDIO_EXTENSIONS.contains(&path_extension(&path).as_str()) {
            out.push(path);
        }
    }
}

/// Maps dropped file paths into track drafts. Returns the drafts plus
/// human-readable names of skipped paths (not an audio extension, or a
/// stem that cannot serve as a title) so the UI can show what was
/// ignored. `.m4b`/`.aa`/`.aax` drafts carry audiobook media-kind
/// bits. A dropped
/// DIRECTORY expands recursively (sorted by full path; hidden entries
/// and non-audio files skipped) and every audio file inside becomes an
/// audiobook (bits 8 — the owner's rule: directory = book); a
/// directory with no audio lands in `skipped` by its full name.
/// Provenance is tracked PER PATH (round 6 tail minor-4): in a mixed
/// drop the directly dropped files keep their extension-based kind —
/// only the directory expansion is forced to bits 8.
pub fn drafts_from_paths(paths: &[PathBuf]) -> (Vec<TrackDraft>, Vec<String>) {
    let mut drafts = Vec::new();
    let mut skipped = Vec::new();
    let mut expanded: Vec<(PathBuf, bool)> = Vec::new(); // (path, from a dropped directory)
    for path in paths {
        if path.is_dir() {
            let mut collected = Vec::new();
            collect_audio_files(path, &mut collected);
            collected.sort();
            if collected.is_empty() {
                // «имя директории» per spec; the display fallback covers
                // a root-like path without a file name.
                skipped.push(
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .map(str::to_owned)
                        .unwrap_or_else(|| path.display().to_string()),
                );
            } else {
                expanded.extend(collected.into_iter().map(|path| (path, true)));
            }
        } else {
            expanded.push((path.clone(), false));
        }
    }
    for (path, from_directory) in &expanded {
        let name = match path.file_name().and_then(|name| name.to_str()) {
            Some(name) => name.to_owned(),
            None => path.display().to_string(),
        };
        let extension = path_extension(path);
        let is_audio = !extension.is_empty()
            && AUDIO_EXTENSIONS.contains(&extension.as_str());
        let stem = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_default()
            .trim()
            .to_owned();
        match TrackDraft::new(path.clone(), stem) {
            // A blank stem fails title validation and lands in `skipped`.
            Ok(draft) if is_audio => {
                // A file expanded from a dropped directory is always a
                // part of the dropped book; a single dropped file keeps
                // its own extension-based kind (round 6 tail minor-4:
                // per-path provenance, not a global directory flag).
                let draft =
                    if *from_directory || AUDIOBOOK_EXTENSIONS.contains(&extension.as_str()) {
                        draft.with_media_kind_bits(8)
                    } else {
                        draft
                };
                drafts.push(draft);
            }
            _ => skipped.push(name.to_owned()),
        }
    }
    (drafts, skipped)
}

/// Runs ffmpeg for one draft into the batch tempdir and returns a
/// draft pointing at the transcoded file (the source draft and its
/// path stay untouched). Failures surface as one-line honest reasons:
/// a missing binary, or a non-zero exit with the first stderr line.
fn transcode_draft(
    draft: &TrackDraft,
    batch_dir: &std::path::Path,
    index: usize,
) -> Result<TrackDraft, String> {
    let target = batch_dir.join(format!(
        "podlib-transcode-{index}.{}",
        transcode_target_extension(draft.media_kind_bits)
    ));
    let codec = transcode_codec(&path_extension(&draft.source_path));
    let output = std::process::Command::new("ffmpeg")
        .args(ffmpeg_transcode_args(&draft.source_path, &target, codec))
        .output()
        .map_err(|_| tr(Key::FfmpegNotFound).to_owned())?;
    if !output.status.success() {
        let reason = String::from_utf8_lossy(&output.stderr)
            .lines()
            .next()
            .unwrap_or(tr(Key::FfmpegNoOutput))
            .to_owned();
        return Err(format!("ffmpeg: {reason}"));
    }
    if !target.is_file() {
        return Err(tr(Key::FfmpegNoTarget).to_owned());
    }
    let mut prepared = draft.clone();
    prepared.source_path = target;
    Ok(prepared)
}

/// ffprobe arguments for the duration probe: print only the container
/// duration, as a bare float line («873.958000») on stdout.
fn ffprobe_duration_args(path: &std::path::Path) -> Vec<String> {
    vec![
        "-v".to_owned(),
        "error".to_owned(),
        "-show_entries".to_owned(),
        "format=duration".to_owned(),
        "-of".to_owned(),
        "default=noprint_wrappers=1:nokey=1".to_owned(),
        path.display().to_string(),
    ]
}

/// Probes the real duration of a media file with `ffprobe` — the same
/// CLI pattern as the ffmpeg transcode step (round 6.1 B1: passthrough
/// drafts have no other source for `duration_ms`, and the iPod refuses
/// to play a track whose total_time_ms stayed 0). Returns milliseconds;
/// an honest one-line reason on failure: a missing binary, a non-zero
/// exit, or output that is not a positive duration. `program` is
/// injectable so tests can exercise the missing-binary path.
fn probe_duration_ms_with(program: &str, path: &std::path::Path) -> Result<u32, String> {
    let output = std::process::Command::new(program)
        .args(ffprobe_duration_args(path))
        .output()
        .map_err(|_| trf(Key::ToolNotFound, &[("program", program)]))?;
    if !output.status.success() {
        let reason = String::from_utf8_lossy(&output.stderr)
            .lines()
            .next()
            .unwrap_or(tr(Key::FfprobeNoOutput))
            .to_owned();
        return Err(format!("ffprobe: {reason}"));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let seconds: f64 = stdout
        .trim()
        .parse()
        .map_err(|_| trf(Key::FfprobeNonNumeric, &[("output", stdout.trim())]))?;
    if !seconds.is_finite() || seconds <= 0.0 {
        return Err(trf(Key::FfprobeBadDuration, &[("seconds", &seconds.to_string())]));
    }
    Ok((seconds * 1000.0).round() as u32)
}

/// Probes the duration with the real ffprobe.
fn probe_duration_ms(path: &std::path::Path) -> Result<u32, String> {
    probe_duration_ms_with("ffprobe", path)
}

/// Payload dragged from the track table: the ids of the dragged rows. A
/// `Vec` (one id today) so multi-row drag needs no payload type change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraggedTracks(pub Vec<TrackId>);

/// Guards a playlist mutation (add/remove members, rename, delete)
/// before a command is sent.
///
/// WHY the UI guards instead of trusting the backend: only the 7G
/// (libopod) adapter rejects master-member mutations. The classic
/// iTunesDB backend deliberately allows them, mirroring the firmware —
/// but they are pointless there as well: the master playlist is the
/// mirror of the whole library, and adding to it is a no-op (duplicate
/// members are ignored by contract). One consistent UI rule for both
/// device kinds, explained up front without a round trip to the device.
/// Firmware-owned views (`distinguished_kind != 0`: Аудиокниги, Видео
/// and friends) are system property on every device kind — rename and
/// delete are refused alongside membership changes.
pub fn guard_playlist_mutation(playlist: &Playlist) -> Result<(), String> {
    if playlist.is_master {
        Err(trf(Key::GuardMasterMembers, &[("name", &playlist.name)]))
    } else if playlist.distinguished_kind != 0 {
        Err(trf(Key::GuardSystemPlaylist, &[("name", &playlist.name)]))
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
        return Err(tr(Key::ErrEmptyTitle).to_owned());
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
        .map_err(|_| trf(Key::ErrNotAnInteger, &[("text", text)]))?;
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

    // 30s: the ceiling only matters under heavy parallel load (the suite
    // spawns real ffmpeg fixtures); workers normally reply in milliseconds.
    const TIMEOUT: Duration = Duration::from_secs(30);

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

    /// Polls until the worker refuses commands, returning the refusal
    /// message. The worker thread exits right AFTER its final reply
    /// (`Failed` outcome / `Closed`), and `request()` turns Err only
    /// once that thread has actually dropped the receiver — polling
    /// instead of racing the exit keeps these tests deterministic.
    fn wait_until_refusing(worker: &LibraryWorker) -> Option<String> {
        let deadline = std::time::Instant::now() + TIMEOUT;
        loop {
            match worker.request(LibraryCommand::Refresh) {
                Err(message) => return Some(message),
                Ok(_) if std::time::Instant::now() >= deadline => return None,
                Ok(_) => std::thread::sleep(Duration::from_millis(2)),
            }
        }
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
    // Moved to i18n.rs (round 9): plural/added_tracks_summary became
    // `i18n::n_tracks` / `i18n::added_tracks`, tested there on the same
    // boundaries (0/1/2/5/11/12/21/22/25/100/101/111).

    // ---- drafts_from_paths ----

    fn write_file(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"x").unwrap();
        path
    }

    /// Real probeable passthrough audio (AAC in an .m4a container)
    /// generated by the installed ffmpeg. Round 6.1 B1: garbage bytes
    /// no longer reach `add_track` — the duration probe rejects them —
    /// so add-flow tests need genuine media files.
    fn real_passthrough_file(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(format!("{name}.m4a"));
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
        assert_eq!(drafts[0].media_kind_bits, 1, "songs default to song bits");
        assert_eq!(drafts[1].title, "UPPER", "extension match is case-insensitive");
        assert_eq!(drafts[2].source_path, dir.path().join("missing.flac"));
        assert_eq!(skipped, vec!["notes.txt", "noext"]);
    }

    #[test]
    fn m4b_drafts_carry_audiobook_bits_case_insensitively() {
        // Round 5.1: .m4b is the audiobook container — the draft must
        // land in the device's «Аудиокниги» view, not in «Музыка».
        let dir = TempDir::new().unwrap();
        let (drafts, skipped) = drafts_from_paths(&[
            write_file(dir.path(), "Song.m4b"),
            write_file(dir.path(), "Book.S.M4B"),
            write_file(dir.path(), "Plain.mp3"),
        ]);
        assert!(skipped.is_empty(), "all three are audio files");
        assert_eq!(drafts.len(), 3);
        assert_eq!(drafts[0].title, "Song");
        assert_eq!(drafts[0].media_kind_bits, 8, "lowercase .m4b");
        assert_eq!(drafts[1].media_kind_bits, 8, "uppercase .M4B");
        assert_eq!(drafts[2].media_kind_bits, 1, "plain .mp3 stays a song");
    }

    // ---- 2026-11-21: full iPod audio-format support ----

    /// AIFF plays natively on Nano 7G per the Apple spec; the drafts
    /// pass through as songs (round 5.2 had skipped both extensions).
    #[test]
    fn aiff_drafts_pass_through_as_songs() {
        let dir = TempDir::new().unwrap();
        let (drafts, skipped) = drafts_from_paths(&[
            write_file(dir.path(), "Track.aiff"),
            write_file(dir.path(), "Short.AIF"),
            write_file(dir.path(), "cover.txt"),
        ]);
        assert_eq!(skipped, vec!["cover.txt"], "only non-audio is skipped");
        assert_eq!(drafts.len(), 2);
        assert_eq!(drafts[0].title, "Track");
        assert_eq!(drafts[0].media_kind_bits, 1, ".aiff is a song");
        assert_eq!(drafts[1].title, "Short", "extension match is case-insensitive");
        assert_eq!(drafts[1].media_kind_bits, 1, ".AIF is a song");
    }

    /// Audible formats 2/3/4 (.aa) and Enhanced/AAX/AAX+ (.aax) play
    /// natively; they are audiobooks — the drafts carry bits 8 like
    /// .m4b, case-insensitively.
    #[test]
    fn audible_drafts_carry_audiobook_bits() {
        let dir = TempDir::new().unwrap();
        let (drafts, skipped) = drafts_from_paths(&[
            write_file(dir.path(), "Book.aa"),
            write_file(dir.path(), "Novel.AAX"),
            write_file(dir.path(), "Plain.mp3"),
        ]);
        assert!(skipped.is_empty(), "aa/aax are audio files");
        assert_eq!(drafts.len(), 3);
        assert_eq!(drafts[0].title, "Book");
        assert_eq!(drafts[0].media_kind_bits, 8, ".aa is an audiobook");
        assert_eq!(drafts[1].title, "Novel");
        assert_eq!(drafts[1].media_kind_bits, 8, "uppercase .AAX");
        assert_eq!(drafts[2].media_kind_bits, 1, "plain .mp3 stays a song");
    }

    // ---- round 5.2: directory drops + transcode ----

    /// Owner's decision: a dropped DIRECTORY is one audiobook — every
    /// audio file inside gets audiobook bits (8), regardless of its own
    /// extension. Nested folders are walked, hidden entries and
    /// non-audio files are skipped, paths sort deterministically.
    #[test]
    fn dropped_directory_expands_recursively_with_audiobook_bits() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("book/cd2")).unwrap();
        std::fs::create_dir_all(dir.path().join("book/.hidden")).unwrap();
        write_file(dir.path(), "book/01.flac");
        write_file(dir.path(), "book/cd2/02.ogg");
        write_file(dir.path(), "book/.hidden/secret.mp3");
        write_file(dir.path(), "book/cover.png");

        let (drafts, skipped) = drafts_from_paths(&[dir.path().join("book")]);
        assert!(skipped.is_empty());
        let full_paths: Vec<String> = drafts
            .iter()
            .map(|draft| draft.source_path.display().to_string())
            .collect();
        let mut sorted = full_paths.clone();
        sorted.sort();
        assert_eq!(full_paths, sorted, "paths are sorted by full path");
        assert_eq!(drafts.len(), 2, "hidden dir and cover.png are skipped");
        for draft in &drafts {
            assert_eq!(
                draft.media_kind_bits, 8,
                "files from a dropped directory are audiobook bits"
            );
        }
        assert_eq!(drafts[0].title, "01");
        assert!(drafts[0].source_path.ends_with("01.flac"));
        assert_eq!(drafts[1].title, "02");
        assert!(drafts[1].source_path.ends_with("02.ogg"));
    }

    // ---- round 6 tail minor-4: per-path provenance ----

    /// A MIXED drop (directory + individual files in one gesture) is
    /// provenance-tracked per path: files expanded from the directory
    /// are the book (bits 8), the directly dropped files keep their
    /// own extension-based kind. The old global `directory_drop` flag
    /// wrongly made everything an audiobook.
    #[test]
    fn mixed_drop_keeps_direct_files_by_extension() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("book")).unwrap();
        write_file(dir.path(), "book/01.mp3");
        write_file(dir.path(), "book/02.mp3");
        let single = write_file(dir.path(), "top.mp3");
        let m4b = write_file(dir.path(), "audio.m4b");
        let audible = write_file(dir.path(), "book-direct.aa");

        for order in 0..2 {
            // Both orders prove the per-path (not first-seen) logic.
            let paths: Vec<PathBuf> = if order == 0 {
                vec![
                    dir.path().join("book"),
                    single.clone(),
                    m4b.clone(),
                    audible.clone(),
                ]
            } else {
                vec![
                    single.clone(),
                    m4b.clone(),
                    audible.clone(),
                    dir.path().join("book"),
                ]
            };
            let (drafts, skipped) = drafts_from_paths(&paths);
            assert!(skipped.is_empty());
            assert_eq!(drafts.len(), 5, "2 book files + 3 direct files: {order}");
            for draft in &drafts {
                let bits = draft.media_kind_bits;
                if draft.source_path.starts_with(dir.path().join("book")) {
                    assert_eq!(bits, 8, "directory expansion is a book: {order}");
                } else if draft.source_path == m4b {
                    assert_eq!(bits, 8, ".m4b is a book by extension: {order}");
                } else if draft.source_path == audible {
                    assert_eq!(bits, 8, ".aa is a book by extension: {order}");
                } else {
                    assert_eq!(bits, 1, "direct mp3 stays a song: {order}");
                }
            }
        }
    }

    #[test]
    fn directory_without_audio_lands_in_skipped() {
        let dir = TempDir::new().unwrap();
        write_file(dir.path(), "booklet.pdf");
        let (drafts, skipped) = drafts_from_paths(&[dir.path().join("book")]);
        assert!(drafts.is_empty());
        assert_eq!(skipped, vec!["book".to_owned()], "the directory NAME lands in skipped");
    }

    #[test]
    fn needs_transcode_matches_firmware_unplayable_formats() {
        // Nano 7G firmware formats per the Apple spec (SP112039): mp3,
        // AAC-family (m4a/m4b/aac), WAV, AIFF and Audible play as-is;
        // flac/ogg have no firmware decoder and are transcoded to AAC
        // (round 5.2). WAV joined passthrough on 2026-11-21 (owner's
        // decision: round 5.2 had bundled it with flac/ogg by mistake).
        for playable in ["mp3", "m4a", "m4b", "aac", "wav", "aiff", "aif", "aa", "aax"] {
            assert!(!needs_transcode(playable), "{playable} passes through");
        }
        for unplayable in ["flac", "ogg", "oga"] {
            assert!(needs_transcode(unplayable), "{unplayable} needs transcode");
        }
        // Unknown extensions never reach the worker, but stay honest.
        assert!(!needs_transcode("txt"));
    }

    #[test]
    fn transcode_target_extension_follows_media_kind() {
        assert_eq!(transcode_target_extension(8), "m4b", "audiobook container");
        assert_eq!(transcode_target_extension(1), "m4a", "music container");
        assert_eq!(transcode_target_extension(4), "m4a");
        // nit-6: the check is the semantic BIT, not an exact == 8.
        assert_eq!(transcode_target_extension(9), "m4b", "audiobook bit wins");
    }

    #[test]
    fn transcode_codec_follows_source_losslessness() {
        assert_eq!(transcode_codec("flac"), "alac", "lossless → lossless");
        assert_eq!(transcode_codec("ogg"), "aac");
        assert_eq!(transcode_codec("oga"), "aac");
    }

    #[test]
    fn ffmpeg_transcode_args_are_exact() {
        // FLAC is a lossless source: wrap into ALAC, no rate control.
        let args = ffmpeg_transcode_args(
            std::path::Path::new("/src/Skazka.flac"),
            std::path::Path::new("/tmp/book.m4b"),
            transcode_codec("flac"),
        );
        assert_eq!(
            args,
            [
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-i",
                "/src/Skazka.flac",
                "-vn",
                "-map_chapters",
                "0",
                "-c:a",
                "alac",
                "-f",
                "ipod",
                "/tmp/book.m4b",
            ]
        );
        // Ogg is lossy: VBR AAC quality 5 — compact wins over zero
        // additional loss (owner decision 2026-11-21).
        let args = ffmpeg_transcode_args(
            std::path::Path::new("/src/Mix.ogg"),
            std::path::Path::new("/tmp/mix.m4a"),
            transcode_codec("ogg"),
        );
        assert_eq!(
            args,
            [
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-i",
                "/src/Mix.ogg",
                "-vn",
                "-map_chapters",
                "0",
                "-c:a",
                "aac",
                "-q:a",
                "5",
                "-f",
                "ipod",
                "/tmp/mix.m4a",
            ]
        );
    }

    #[test]
    fn add_failures_summary_appends_an_honest_tail() {
        assert_eq!(add_failures_summary(&[]), "");
        assert_eq!(
            add_failures_summary(&["«Книга»: ffmpeg не найден".to_owned()]),
            "; добавление не удалось: «Книга»: ffmpeg не найден"
        );
        assert_eq!(
            add_failures_summary(&["a".to_owned(), "b".to_owned()]),
            "; добавление не удалось: a; b"
        );
    }

    /// End-to-end lossless path: a real FLAC fixture (generated by the
    /// installed ffmpeg) is transcoded into ALAC-in-m4a and the decoded
    /// PCM must stay bit-identical — the whole point of ALAC routing.
    #[test]
    fn transcode_draft_wraps_flac_into_alac_losslessly() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("song.flac");
        let generated = std::process::Command::new("ffmpeg")
            .args([
                "-hide_banner".to_owned(),
                "-loglevel".to_owned(),
                "error".to_owned(),
                "-y".to_owned(),
                "-f".to_owned(),
                "lavfi".to_owned(),
                "-i".to_owned(),
                "sine=frequency=440:duration=2".to_owned(),
                "-c:a".to_owned(),
                "flac".to_owned(),
                source.display().to_string(),
            ])
            .output()
            .expect("run ffmpeg");
        assert!(
            generated.status.success(),
            "fixture ffmpeg failed: {}",
            String::from_utf8_lossy(&generated.stderr)
        );

        let draft = TrackDraft::new(source.clone(), "Трек").unwrap();
        let transcoded = transcode_draft(&draft, dir.path(), 0).expect("ffmpeg transcode");
        assert_eq!(
            transcoded.source_path.extension().and_then(|e| e.to_str()),
            Some("m4a"),
            "music draft lands in the m4a container"
        );
        assert_ne!(transcoded.source_path, draft.source_path, "in batch tempdir");
        assert_eq!(
            probe_stream_codec(&transcoded.source_path).as_deref(),
            Some("alac"),
            "flac must wrap into ALAC"
        );

        // Bit-perfect: decode source and result to PCM and compare.
        let decode_pcm = |input: &std::path::Path, output: &std::path::Path| {
            let decoded = std::process::Command::new("ffmpeg")
                .args([
                    "-hide_banner".to_owned(),
                    "-loglevel".to_owned(),
                    "error".to_owned(),
                    "-y".to_owned(),
                    "-i".to_owned(),
                    input.display().to_string(),
                    "-c:a".to_owned(),
                    "pcm_s16le".to_owned(),
                    "-f".to_owned(),
                    "wav".to_owned(),
                    output.display().to_string(),
                ])
                .output()
                .expect("run ffmpeg");
            assert!(
                decoded.status.success(),
                "decode {input:?} failed: {}",
                String::from_utf8_lossy(&decoded.stderr)
            );
        };
        let source_pcm = dir.path().join("source.wav");
        let result_pcm = dir.path().join("result.wav");
        decode_pcm(&source, &source_pcm);
        decode_pcm(&transcoded.source_path, &result_pcm);
        assert_eq!(
            std::fs::read(&source_pcm).unwrap(),
            std::fs::read(&result_pcm).unwrap(),
            "FLAC→ALAC must keep the PCM bit-identical"
        );
    }

    /// The lossy path: an Ogg Vorbis fixture re-encodes into VBR AAC
    /// (quality 5) — the accepted second generation (owner decision
    /// 2026-11-21). Audiobook bits still land in the .m4b container.
    #[test]
    fn transcode_draft_reencodes_ogg_to_vbr_aac() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("mix.ogg");
        let generated = std::process::Command::new("ffmpeg")
            .args([
                "-hide_banner".to_owned(),
                "-loglevel".to_owned(),
                "error".to_owned(),
                "-y".to_owned(),
                "-f".to_owned(),
                "lavfi".to_owned(),
                "-i".to_owned(),
                "sine=frequency=330:duration=2".to_owned(),
                "-c:a".to_owned(),
                "libvorbis".to_owned(),
                source.display().to_string(),
            ])
            .output()
            .expect("run ffmpeg");
        assert!(
            generated.status.success(),
            "fixture ffmpeg failed: {}",
            String::from_utf8_lossy(&generated.stderr)
        );

        let draft = TrackDraft::new(source, "Глава 1")
            .unwrap()
            .with_media_kind_bits(8);
        let transcoded = transcode_draft(&draft, dir.path(), 0).expect("ffmpeg transcode");
        assert_eq!(
            transcoded.source_path.extension().and_then(|e| e.to_str()),
            Some(transcode_target_extension(8)),
            "audiobook container .m4b"
        );
        assert_eq!(transcoded.media_kind_bits, 8);
        assert_eq!(transcoded.title, "Глава 1");
        assert_eq!(
            probe_stream_codec(&transcoded.source_path).as_deref(),
            Some("aac"),
            "ogg must re-encode to AAC"
        );
    }

    /// Reads the audio codec name of a file via the installed ffprobe
    /// (`None` when the tool or the stream is missing).
    fn probe_stream_codec(path: &std::path::Path) -> Option<String> {
        let output = std::process::Command::new("ffprobe")
            .args([
                "-v".to_owned(),
                "error".to_owned(),
                "-select_streams".to_owned(),
                "a:0".to_owned(),
                "-show_entries".to_owned(),
                "stream=codec_name".to_owned(),
                "-of".to_owned(),
                "default=noprint_wrappers=1:nokey=1".to_owned(),
                path.display().to_string(),
            ])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if text.is_empty() {
            None
        } else {
            Some(text)
        }
    }

    #[test]
    fn transcode_draft_reports_failure_honestly() {
        let dir = TempDir::new().unwrap();
        let draft = TrackDraft::new(dir.path().join("garbage.flac"), "Book").unwrap();
        // The garbage file exists but is not decodable: ffmpeg exits
        // non-zero and the reason surfaces as a one-line honest error.
        std::fs::write(&draft.source_path, b"not audio at all").unwrap();
        let error = transcode_draft(&draft, dir.path(), 0).unwrap_err();
        assert!(error.contains("ffmpeg"), "got: {error}");
    }

    // ---- round 6.1 B1: passthrough duration probe ----

    #[test]
    fn ffprobe_duration_args_are_exact() {
        let args = ffprobe_duration_args(Path::new("/media/book/ch.mp3"));
        assert_eq!(
            args,
            vec![
                "-v".to_owned(),
                "error".to_owned(),
                "-show_entries".to_owned(),
                "format=duration".to_owned(),
                "-of".to_owned(),
                "default=noprint_wrappers=1:nokey=1".to_owned(),
                "/media/book/ch.mp3".to_owned(),
            ]
        );
    }

    #[test]
    fn probe_reports_a_missing_tool_with_clear_text() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("chapter.mp3");
        std::fs::write(&source, b"not really audio").unwrap();
        let error = probe_duration_ms_with("podlib-no-such-tool-xyz", &source).unwrap_err();
        assert!(
            error.contains("podlib-no-such-tool-xyz не найден"),
            "got: {error}"
        );
    }

    #[test]
    fn probe_parses_a_real_file_duration() {
        // Minimal 8 kHz mono 16-bit PCM WAV, 1.0 s of silence: real
        // bytes ffprobe can measure without ffmpeg.
        let dir = TempDir::new().unwrap();
        let data_size = 16_000u32;
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&36u32.to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // mono
        wav.extend_from_slice(&8000u32.to_le_bytes());
        wav.extend_from_slice(&16000u32.to_le_bytes()); // byte rate
        wav.extend_from_slice(&2u16.to_le_bytes()); // block align
        wav.extend_from_slice(&16u16.to_le_bytes()); // bits
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_size.to_le_bytes());
        wav.extend(std::iter::repeat_n(0u8, data_size as usize));
        let source = dir.path().join("chapter.wav");
        std::fs::write(&source, &wav).unwrap();
        let duration_ms = probe_duration_ms(&source).expect("real ffprobe");
        assert!(
            (950..=1050).contains(&duration_ms),
            "1 s of PCM must probe to ~1000 ms, got {duration_ms}"
        );
    }

    /// Mock backend recording every draft handed to `add_track` — lets
    /// the B1/B2 passthrough branch be asserted end to end.
    #[derive(Default)]
    struct RecordingBackend {
        added: Vec<TrackDraft>,
    }

    impl LibraryBackend for RecordingBackend {
        fn tracks(&self) -> Result<Vec<Track>, BackendError> {
            Ok(Vec::new())
        }

        fn playlists(&self) -> Result<Vec<Playlist>, BackendError> {
            Ok(Vec::new())
        }

        fn add_track(&mut self, draft: &TrackDraft) -> Result<TrackId, BackendError> {
            self.added.push(draft.clone());
            Ok(TrackId::from_bits(self.added.len() as u64))
        }

        fn remove_track(&mut self, _id: TrackId, _purge_media: bool) -> Result<bool, BackendError> {
            Ok(true)
        }

        fn edit_track(
            &mut self,
            id: TrackId,
            _changes: &TrackChanges,
        ) -> Result<Track, BackendError> {
            Err(BackendError::TrackNotFound(id))
        }

        fn create_playlist(&mut self, _draft: &PlaylistDraft) -> Result<PlaylistId, BackendError> {
            Err(BackendError::PlaylistNotFound(PlaylistId::from_bits(0)))
        }

        fn delete_playlist(&mut self, _id: PlaylistId) -> Result<bool, BackendError> {
            Ok(false)
        }

        fn rename_playlist(&mut self, id: PlaylistId, _name: &str) -> Result<(), BackendError> {
            Err(BackendError::PlaylistNotFound(id))
        }

        fn add_to_playlist(
            &mut self,
            _playlist: PlaylistId,
            _track_ids: &[TrackId],
        ) -> Result<(), BackendError> {
            Ok(())
        }

        fn remove_from_playlist(
            &mut self,
            _playlist: PlaylistId,
            _track_ids: &[TrackId],
        ) -> Result<(), BackendError> {
            Ok(())
        }
    }

    /// End-to-end passthrough: two m4a chapters (real files generated
    /// by the installed ffmpeg) must reach the backend with a probed
    /// duration AND 1/2 album numbering (B1+B2 together).
    #[test]
    fn add_tracks_probes_passthrough_duration_and_numbers_album_chapters() {
        let dir = TempDir::new().unwrap();
        let mut drafts = Vec::new();
        for name in ["Глава 1", "Глава 2"] {
            let source = dir.path().join(format!("{name}.m4a"));
            let generated = std::process::Command::new("ffmpeg")
                .args([
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440:duration=0.5",
                    "-c:a",
                    "aac",
                    &source.display().to_string(),
                ])
                .output()
                .expect("run ffmpeg");
            assert!(
                generated.status.success(),
                "fixture ffmpeg failed: {}",
                String::from_utf8_lossy(&generated.stderr)
            );
            drafts.push(TrackDraft::new(source, name).unwrap());
        }
        let mut backend = RecordingBackend::default();
        let outcome = add_tracks_command(&mut backend, drafts);
        assert!(
            matches!(outcome, LibraryOutcome::Applied { .. }),
            "both chapters must add cleanly"
        );
        assert_eq!(backend.added.len(), 2);
        for draft in &backend.added {
            assert!(draft.duration_ms > 0, "B1: probed duration must be filled");
            assert_eq!(draft.track_count, 2, "B2: chapters share one album");
        }
        assert_eq!(backend.added[0].track_number, 1);
        assert_eq!(backend.added[1].track_number, 2);
    }

    #[test]
    fn add_tracks_reports_probe_failure_instead_of_silent_zero() {
        // A .mp3 that is not audio: the probe fails honestly, the draft
        // never reaches the backend, and the summary carries the reason
        // — no silent total_time_ms = 0 on the device.
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("мусор.mp3");
        std::fs::write(&source, b"definitely not audio").unwrap();
        let mut backend = RecordingBackend::default();
        let outcome =
            add_tracks_command(&mut backend, vec![TrackDraft::new(source, "Глава").unwrap()]);
        match outcome {
            LibraryOutcome::Applied { summary, .. } => {
                assert!(summary.contains("0 треков из 1"), "got: {summary}");
                assert!(summary.contains("ffprobe"), "got: {summary}");
            }
            other => panic!("expected an honest Applied tail, got {other:?}"),
        }
        assert!(backend.added.is_empty(), "the failed draft must be skipped");
    }

    /// End-to-end transcode: a real FLAC fixture must reach the backend
    /// with a probed duration — the transcoded draft inherits the DnD
    /// zero, and a silent `total_time_ms = 0` makes the firmware refuse
    /// to play the track (live F40/DBWI.m4a failure). Same contract as
    /// the passthrough probe (B1).
    #[test]
    fn add_tracks_probes_transcoded_duration() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("song.flac");
        let generated = std::process::Command::new("ffmpeg")
            .args([
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=0.5",
                "-c:a",
                "flac",
                &source.display().to_string(),
            ])
            .output()
            .expect("run ffmpeg");
        assert!(
            generated.status.success(),
            "fixture ffmpeg failed: {}",
            String::from_utf8_lossy(&generated.stderr)
        );
        let mut backend = RecordingBackend::default();
        let outcome =
            add_tracks_command(&mut backend, vec![TrackDraft::new(source, "Трек").unwrap()]);
        assert!(
            matches!(outcome, LibraryOutcome::Applied { .. }),
            "the transcoded draft must add cleanly"
        );
        assert_eq!(backend.added.len(), 1);
        assert!(
            backend.added[0].duration_ms > 0,
            "B1: transcoded duration must be probed, not zero"
        );
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
            media_kind: 0,
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
    fn guard_rejects_master_system_and_allows_user_playlist() {
        let master = Playlist {
            id: PlaylistId::from_bits(1),
            name: "Медиатека".to_owned(),
            track_ids: Vec::new(),
            is_master: true,
            is_smart: false,
            distinguished_kind: 0,
        };
        let user = Playlist {
            id: PlaylistId::from_bits(2),
            name: "Mix".to_owned(),
            track_ids: Vec::new(),
            is_master: false,
            is_smart: false,
            distinguished_kind: 0,
        };
        let error = guard_playlist_mutation(&master).unwrap_err();
        assert!(error.contains("встроенный"));
        assert!(guard_playlist_mutation(&user).is_ok());
        // A firmware-owned view (Аудиокниги, Видео…) is system property:
        // no rename/delete/add/remove may reach the worker for it.
        let view = Playlist {
            id: PlaylistId::from_bits(3),
            name: "Аудиокниги".to_owned(),
            track_ids: Vec::new(),
            is_master: false,
            is_smart: false,
            distinguished_kind: 8,
        };
        let error = guard_playlist_mutation(&view).unwrap_err();
        assert!(error.contains("системн"), "got: {error}");
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
            real_passthrough_file(host.path(), "One"),
            real_passthrough_file(host.path(), "Two"),
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
            real_passthrough_file(host.path(), "Good"),
            // Second source does not exist: the B1 probe fails on it,
            // and the draft is skipped with an honest failure line.
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
        assert_eq!(snapshot.playlists.len(), 4, "master + new + built-ins");
        let playlist = snapshot
            .playlists
            .iter()
            .find(|playlist| !playlist.is_master && playlist.distinguished_kind == 0)
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
        assert_eq!(snapshot.playlists.len(), 3, "master + the two built-ins remain");
        worker.close_and_join();
    }

    #[test]
    fn worker_renames_playlist_and_keeps_members() {
        let dir = fixture_mount(&[fixture_track("Alpha")], &[]);
        let (worker, snapshot) = opened_worker(dir.path());
        let track_id = snapshot.tracks[0].id;

        let snapshot = applied_snapshot(run(
            &worker,
            LibraryCommand::CreatePlaylist {
                name: "Mix".to_owned(),
            },
        ));
        let playlist_id = snapshot
            .playlists
            .iter()
            .find(|playlist| !playlist.is_master && playlist.distinguished_kind == 0)
            .expect("user playlist must appear")
            .id;
        applied_snapshot(run(
            &worker,
            LibraryCommand::AddToPlaylist {
                playlist: playlist_id,
                track_ids: vec![track_id],
            },
        ));

        let snapshot = applied_snapshot(run(
            &worker,
            LibraryCommand::RenamePlaylist {
                id: playlist_id,
                name: "Renamed".to_owned(),
            },
        ));
        let playlist = snapshot
            .playlist(playlist_id)
            .expect("the renamed playlist must stay");
        assert_eq!(playlist.name, "Renamed");
        // The id and the membership are untouched by the rename.
        assert_eq!(playlist.track_ids, vec![track_id]);
        worker.close_and_join();
    }

    #[test]
    fn worker_refuses_master_playlist_rename() {
        let dir = fixture_mount(&[fixture_track("Alpha")], &[]);
        let (worker, snapshot) = opened_worker(dir.path());
        let master_id = snapshot
            .playlists
            .iter()
            .find(|playlist| playlist.is_master)
            .expect("fixture exposes the master")
            .id;
        match run(
            &worker,
            LibraryCommand::RenamePlaylist {
                id: master_id,
                name: "Nope".to_owned(),
            },
        ) {
            LibraryOutcome::Failed { message, .. } => {
                assert!(message.contains("встроенный"), "got: {message}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
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
                wait_until_refusing(&worker),
                Some(message) if message.contains("не активен")
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
        assert!(
            wait_until_refusing(&worker).is_some(),
            "worker must refuse commands after a failed open"
        );
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

        fn rename_playlist(&mut self, _id: PlaylistId, _name: &str) -> Result<(), BackendError> {
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
        // panics inside `add_track`; the worker must still answer. The
        // draft carries its own duration, so the B1 probe (which would
        // reject the nonexistent source first) is skipped.
        let mut draft =
            TrackDraft::new(PathBuf::from("/tmp/nope.mp3"), "X").expect("draft must be valid");
        draft.duration_ms = 60_000;
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

    /// Two-track fake for the batch «Задать исполнителя» command: ids in
    /// `fail_ids` refuse the edit (stale-selection shape), the rest apply
    /// the artist in memory like a real backend would. `edit_calls`
    /// counts edit_track invocations — the idempotency guard is proven
    /// by skipped edits, not just by the summary text.
    struct ArtistBackend {
        tracks: Vec<Track>,
        fail_ids: Vec<TrackId>,
        edit_calls: usize,
    }

    impl ArtistBackend {
        fn two_tracks() -> Self {
            Self {
                tracks: vec![track_for_artist_test(1, "Track One"), track_for_artist_test(2, "Track Two")],
                fail_ids: Vec::new(),
                edit_calls: 0,
            }
        }
    }

    fn track_for_artist_test(bits: u64, title: &str) -> Track {
        Track {
            id: TrackId::from_bits(bits),
            title: title.to_owned(),
            artist: None,
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

    impl LibraryBackend for ArtistBackend {
        fn tracks(&self) -> Result<Vec<Track>, BackendError> {
            Ok(self.tracks.clone())
        }

        fn playlists(&self) -> Result<Vec<Playlist>, BackendError> {
            Ok(Vec::new())
        }

        fn add_track(&mut self, _draft: &TrackDraft) -> Result<TrackId, BackendError> {
            Err(BackendError::TrackNotFound(TrackId::from_bits(0)))
        }

        fn remove_track(&mut self, _id: TrackId, _purge_media: bool) -> Result<bool, BackendError> {
            Ok(true)
        }

        fn edit_track(
            &mut self,
            id: TrackId,
            changes: &TrackChanges,
        ) -> Result<Track, BackendError> {
            self.edit_calls += 1;
            if self.fail_ids.contains(&id) {
                return Err(BackendError::TrackNotFound(id));
            }
            let track = self
                .tracks
                .iter_mut()
                .find(|track| track.id == id)
                .ok_or(BackendError::TrackNotFound(id))?;
            if let Some(artist) = &changes.artist {
                track.artist = Some(artist.clone());
            }
            Ok(track.clone())
        }

        fn create_playlist(&mut self, _draft: &PlaylistDraft) -> Result<PlaylistId, BackendError> {
            Err(BackendError::PlaylistNotFound(PlaylistId::from_bits(0)))
        }

        fn delete_playlist(&mut self, _id: PlaylistId) -> Result<bool, BackendError> {
            Ok(false)
        }

        fn rename_playlist(&mut self, id: PlaylistId, _name: &str) -> Result<(), BackendError> {
            Err(BackendError::PlaylistNotFound(id))
        }

        fn add_to_playlist(
            &mut self,
            _playlist: PlaylistId,
            _track_ids: &[TrackId],
        ) -> Result<(), BackendError> {
            Err(BackendError::PlaylistNotFound(PlaylistId::from_bits(0)))
        }

        fn remove_from_playlist(
            &mut self,
            _playlist: PlaylistId,
            _track_ids: &[TrackId],
        ) -> Result<(), BackendError> {
            Err(BackendError::PlaylistNotFound(PlaylistId::from_bits(0)))
        }
    }

    #[test]
    fn set_artist_updates_every_selected_track() {
        let mut backend = ArtistBackend::two_tracks();
        let outcome = set_artist_command(
            &mut backend,
            vec![TrackId::from_bits(1), TrackId::from_bits(2)],
            "Котэ".to_owned(),
        );
        match outcome {
            LibraryOutcome::Applied { snapshot, summary } => {
                assert!(summary.contains("Исполнитель задан"), "got: {summary}");
                for track in &snapshot.tracks {
                    assert_eq!(track.artist.as_deref(), Some("Котэ"));
                }
            }
            other => panic!("expected Applied, got {other:?}"),
        }
    }

    #[test]
    fn set_artist_reports_an_honest_failure_tail() {
        let mut backend = ArtistBackend::two_tracks();
        backend.fail_ids = vec![TrackId::from_bits(2)];
        let outcome = set_artist_command(
            &mut backend,
            vec![TrackId::from_bits(1), TrackId::from_bits(2)],
            "Котэ".to_owned(),
        );
        match outcome {
            LibraryOutcome::Applied { summary, .. } => {
                assert!(summary.contains("1 из 2"), "got: {summary}");
                assert!(summary.contains("«Track Two»"), "got: {summary}");
                assert!(summary.contains("не удалось"), "got: {summary}");
            }
            other => panic!("expected Applied with an honest tail, got {other:?}"),
        }
    }

    #[test]
    fn set_artist_with_no_successes_fails_honestly() {
        let mut backend = ArtistBackend::two_tracks();
        backend.fail_ids = vec![TrackId::from_bits(1), TrackId::from_bits(2)];
        let outcome = set_artist_command(
            &mut backend,
            vec![TrackId::from_bits(1), TrackId::from_bits(2)],
            "Котэ".to_owned(),
        );
        match outcome {
            LibraryOutcome::Failed { message, .. } => {
                assert!(message.contains("не удалось"), "got: {message}");
                assert!(message.contains("«Track One»"), "got: {message}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    /// N5 (idempotency): a track that already carries the target artist
    /// must be skipped WITHOUT an edit (its pid must not rotate) and the
    /// summary must say so explicitly.
    #[test]
    fn set_artist_skips_tracks_that_already_have_the_artist() {
        let mut backend = ArtistBackend::two_tracks();
        backend.tracks[0].artist = Some("Котэ".to_owned());
        let outcome = set_artist_command(
            &mut backend,
            vec![TrackId::from_bits(1), TrackId::from_bits(2)],
            "Котэ".to_owned(),
        );
        match outcome {
            LibraryOutcome::Applied { snapshot, summary } => {
                assert!(summary.contains("уже с исполнителем: 1"), "got: {summary}");
                assert_eq!(backend.edit_calls, 1, "unchanged track must not be edited");
                assert_eq!(snapshot.track(TrackId::from_bits(1)).unwrap().artist.as_deref(), Some("Котэ"));
                assert_eq!(snapshot.track(TrackId::from_bits(2)).unwrap().artist.as_deref(), Some("Котэ"));
            }
            other => panic!("expected Applied, got {other:?}"),
        }
    }

    /// All tracks already carry the artist: no edits at all, and the
    /// outcome is an honest Applied «уже задан», not a fake success
    /// count or an error.
    #[test]
    fn set_artist_when_everything_is_unchanged_reports_already_set() {
        let mut backend = ArtistBackend::two_tracks();
        for track in &mut backend.tracks {
            track.artist = Some("Котэ".to_owned());
        }
        let outcome = set_artist_command(
            &mut backend,
            vec![TrackId::from_bits(1), TrackId::from_bits(2)],
            "Котэ".to_owned(),
        );
        match outcome {
            LibraryOutcome::Applied { summary, .. } => {
                assert!(summary.contains("уже задан"), "got: {summary}");
                assert_eq!(backend.edit_calls, 0, "nothing to edit");
            }
            other => panic!("expected Applied «уже задан», got {other:?}"),
        }
    }
}
