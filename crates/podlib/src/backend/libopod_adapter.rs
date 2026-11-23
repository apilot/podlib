//! `libopod`-backed implementation of [`LibraryBackend`].
//!
//! Every mutation follows the libopod contract: build an [`EditSession`],
//! stage it to a host-side scratch directory, then install it on the
//! device with full verification. A successful mutation leaves the
//! backend holding a freshly reopened [`Device`], so subsequent reads
//! observe the new state without reopening.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use libopod::{
    Device, EditSession, InstallMode, MediaDeletionPolicy, MediaKind, PersistentId, StagedSqliteEdit,
    TrackToAdd,
};
use rusqlite::{types::Value, OpenFlags};

use crate::backend::{BackendError, BatchEditOutcome, LibraryBackend};
use crate::domain::{
    Playlist, PlaylistDraft, PlaylistId, Track, TrackChanges, TrackDraft, TrackId,
};

/// libopod rejects zero sample rates and no audio decoding happens here,
/// so additions declare the canonical 44.1 kHz.
const DEFAULT_SAMPLE_RATE: u32 = 44_100;

/// Library database inside the mounted volume — the exact path vendored
/// libopod resolves in `device/mod.rs` (constants are not public there).
/// Used only by the media-kind enrichment query (see `hidden_items`).
const SQLITE_LIBRARY_PATH: &str = "iPod_Control/iTunes/iTunes Library.itlp/Library.itdb";

/// Adapter around an opened libopod device.
pub struct LibopodBackend {
    device: Device,
    /// Mount point, kept so the media-kind enrichment query can reach
    /// the same `Library.itdb` libopod opened.
    mount: PathBuf,
}

impl LibopodBackend {
    /// Opens the iPod mounted at `mount` (validates identity and library).
    pub fn open(mount: impl AsRef<Path>) -> Result<Self, BackendError> {
        let device = Device::open(mount.as_ref())?;
        Ok(Self {
            device,
            mount: mount.as_ref().to_path_buf(),
        })
    }

    fn library(&self) -> Result<&libopod::Library, BackendError> {
        self.device
            .library()
            .ok_or_else(|| BackendError::Failure("device exposes no music library".to_owned()))
    }

    fn session(&self) -> Result<EditSession<'_>, BackendError> {
        Ok(self.device.edit()?)
    }

    /// Host-side scratch directory for staging edits; libopod requires
    /// the directory to exist, be empty and live outside the mount.
    fn scratch_dir() -> Result<tempfile::TempDir, BackendError> {
        Ok(tempfile::tempdir()?)
    }

    /// Installs a staged edit with full verification and reopens the
    /// device so subsequent reads observe the new state.
    fn finish(&mut self, staged: StagedSqliteEdit) -> Result<(), BackendError> {
        let device = staged.install_and_open(&self.device, InstallMode::Full, |_| {})?;
        self.device = device;
        Ok(())
    }

    fn track_id_set(&self) -> Result<HashSet<u64>, BackendError> {
        Ok(self
            .library()?
            .tracks()
            .iter()
            .map(|track| track.id.to_bits())
            .collect())
    }

    fn playlist_id_set(&self) -> Result<HashSet<u64>, BackendError> {
        Ok(self
            .library()?
            .playlists()
            .iter()
            .map(|playlist| playlist.id.to_bits())
            .collect())
    }

    /// Returns a track's current membership as `PersistentId`s.
    fn playlist_member_ids(&self, id: PlaylistId) -> Result<Vec<PersistentId>, BackendError> {
        Ok(self.editable_playlist(id)?.track_ids().to_vec())
    }

    /// Resolves a user playlist by id. Master playlists are immutable:
    /// device firmware owns them, so mutations are rejected here with a
    /// precise error instead of libopod's generic "unsupported".
    fn editable_playlist(&self, id: PlaylistId) -> Result<&libopod::Playlist, BackendError> {
        let pid = PersistentId::from_bits(id.to_bits());
        let playlist = self
            .library()?
            .playlists()
            .iter()
            .find(|playlist| playlist.id == pid)
            .ok_or(BackendError::PlaylistNotFound(id))?;
        if playlist.is_hidden {
            return Err(BackendError::MasterPlaylistImmutable { id });
        }
        Ok(playlist)
    }

    fn require_track_ids(&self, ids: &[TrackId]) -> Result<(), BackendError> {
        let known = self.track_id_set()?;
        let hidden = self.hidden_track_ids()?;
        for id in ids {
            let bits = id.to_bits();
            if !known.contains(&bits) && !hidden.contains(&bits) {
                return Err(BackendError::TrackNotFound(*id));
            }
        }
        Ok(())
    }

    /// Pids of items vendored libopod does not read: its item query only
    /// covers `is_song = 1 OR media_kind IN (1, 4) OR is_podcast = 1`,
    /// which drops audiobooks (media_kind 8) and other device kinds from
    /// the GUI entirely. This read-only companion query fetches exactly
    /// that complement, with the raw media-kind bits the view filters
    /// need. Size stays 0 (it lives in Locations.itdb and is display-only
    /// metadata); hidden mutations are gated before any libopod call
    /// (`reject_hidden_mutation`): audiobooks pass through (deletable
    /// since round 5.7), other kinds are refused — so the missing
    /// location only matters on the audiobook path, where libopod's
    /// wide session-side re-read resolves it. An unreadable database
    /// yields an empty list — the library still works without the
    /// enrichment.
    fn hidden_items(&self) -> Vec<Track> {
        let path = self.mount.join(SQLITE_LIBRARY_PATH);
        let connection = match rusqlite::Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) {
            Ok(connection) => connection,
            Err(_) => return Vec::new(),
        };
        hidden_items_from(&connection)
    }

    fn hidden_track_ids(&self) -> Result<HashSet<u64>, BackendError> {
        Ok(self.hidden_items().iter().map(|t| t.id.to_bits()).collect())
    }

    /// Raw media-kind bits of a hidden complement record, if `id` is
    /// one. `None` covers both narrow-read tracks and unknown ids.
    fn hidden_track_kind(&self, id: TrackId) -> Result<Option<u32>, BackendError> {
        let bits = id.to_bits();
        Ok(self
            .hidden_items()
            .iter()
            .find(|track| track.id.to_bits() == bits)
            .map(|track| track.media_kind))
    }

    /// Refusal gate for records the narrow read does not see. Hidden
    /// audiobooks (media_kind 8) pass — since round 5.7 libopod's edit
    /// session resolves their pids via the wide re-read, so deletion
    /// flows through. Every other hidden kind (video, untested device
    /// kinds) is refused before any libopod call with one precise
    /// message: a generic TrackNotFound would hint at a stale list.
    fn reject_hidden_mutation(&self, id: TrackId) -> Result<(), BackendError> {
        if self.track_id_set()?.contains(&id.to_bits()) {
            return Ok(());
        }
        hidden_mutation_rejection(self.hidden_track_kind(id)?)
    }
}

fn optional(text: &str) -> Option<String> {
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// Raw iTunesDB media-kind bits for audiobooks (see [`media_kind_bits`]).
const AUDIOBOOK_MEDIA_KIND: u32 = 8;

/// Decision core of [`LibopodBackend::reject_hidden_mutation`]:
/// `media_kind` holds the raw bits of the hidden complement row
/// (`None` = the id is not a hidden record at all → the known-set /
/// libopod logic answers instead). Audiobooks pass; every other hidden
/// kind is refused with the honest message — audiobooks ARE removable
/// since round 5.7, so the old «аудиокниги» wording would lie.
fn hidden_mutation_rejection(media_kind: Option<u32>) -> Result<(), BackendError> {
    match media_kind {
        None | Some(AUDIOBOOK_MEDIA_KIND) => Ok(()),
        Some(_) => Err(BackendError::Failure(
            "Записи этого типа (видео) нельзя удалять и изменять: \
             libopod не редактирует их"
                .to_owned(),
        )),
    }
}

fn map_track(track: &libopod::Track) -> Track {
    Track {
        id: TrackId::from_bits(track.id.to_bits()),
        title: track.title.clone(),
        artist: optional(&track.artist),
        album: optional(&track.album),
        album_artist: optional(&track.album_artist),
        duration_ms: track.duration_ms,
        track_number: track.track_number,
        disc_number: track.disc_number,
        size_bytes: track.size,
        has_artwork: track.has_artwork,
        media_kind: media_kind_bits(track.media_kind),
    }
}

/// libopod `MediaKind` as raw iTunesDB bits. libopod's enum knows
/// songs, podcasts and audiobooks; video kinds never appear in its
/// track list — those come from the enrichment query instead.
fn media_kind_bits(kind: MediaKind) -> u32 {
    match kind {
        MediaKind::Song => 1,
        MediaKind::Podcast => 4,
        MediaKind::Audiobook => 8,
    }
}

/// Raw iTunesDB bits → libopod `MediaKind` for the add-path: 8 =
/// audiobook, 4 = podcast, everything else = song. Video bits (2/16/32)
/// have no writable representation and fall back to song.
fn media_kind_from_bits(bits: u32) -> MediaKind {
    match bits {
        8 => MediaKind::Audiobook,
        4 => MediaKind::Podcast,
        _ => MediaKind::Song,
    }
}

/// Master predicate for Nano 7G libraries: exactly the hidden container
/// WITHOUT a distinguished kind. `is_hidden` alone is wrong — the 7G
/// database marks all built-in firmware views («Музыка», «Аудиокниги»,
/// …) as hidden too, and treating them as masters made every one of
/// them show «Все треки» and hijack the master view in the GUI.
/// Classic iTunesDB has no distinguished kind, so `distinguished_kind`
/// is 0 there and the predicate degenerates to the old `is_hidden`.
fn is_master_playlist(is_hidden: bool, distinguished_kind: i64) -> bool {
    is_hidden && distinguished_kind == 0
}

fn map_playlist(playlist: &libopod::Playlist) -> Playlist {
    Playlist {
        id: PlaylistId::from_bits(playlist.id.to_bits()),
        name: playlist.name.clone(),
        track_ids: playlist
            .track_ids()
            .iter()
            .map(|pid| TrackId::from_bits(pid.to_bits()))
            .collect(),
        is_master: is_master_playlist(playlist.is_hidden, playlist.distinguished_kind),
        is_smart: playlist.is_smart,
        distinguished_kind: playlist.distinguished_kind,
    }
}

/// Same query as [`LibopodBackend::hidden_items`], split from the
/// device-bound method so tests can drive an in-memory connection
/// with the same untyped schema.
fn hidden_items_from(connection: &rusqlite::Connection) -> Vec<Track> {
    let mut statement = match connection.prepare(
        "SELECT pid, COALESCE(title, ''), COALESCE(album, ''), \
         COALESCE(artist, ''), COALESCE(album_artist, ''), \
         COALESCE(total_time_ms, 0), COALESCE(track_number, 0), \
         COALESCE(disc_number, 0), COALESCE(artwork_status, 0), \
         COALESCE(artwork_cache_id, 0), COALESCE(media_kind, 0) \
         FROM item WHERE NOT (is_song = 1 OR media_kind IN (1, 4) OR is_podcast = 1) \
         ORDER BY pid",
    ) {
        Ok(statement) => statement,
        // No readable database: the library still works without the
        // enrichment (same robustness contract as hidden_items).
        Err(_) => return Vec::new(),
    };
    let rows = statement.query_map([], |row| Ok(hidden_item_track(row)));
    match rows {
        // The row mapping is total (Values normalize anything), so an
        // error here is statement stepping I/O only → empty enrichment.
        Ok(rows) => rows.collect::<Result<Vec<_>, _>>().unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// Maps one enrichment-query row onto a domain Track. The schema is
/// untyped (PRAGMA table_info reports no column types), so EVERY
/// numeric cell is read as a `Value` and normalized — a strict
/// `get::<i64>` answered «Invalid column type Real» on the live
/// database (`total_time_ms` is REAL there) and `flatten()` silently
/// emptied the whole enrichment (the «Аудиокниги = 0» bug).
fn hidden_item_track(row: &rusqlite::Row<'_>) -> Track {
    let cell = |index: usize| row.get::<_, Value>(index).unwrap_or(Value::Null);
    let pid = value_i64(cell(0));
    Track {
        id: TrackId::from_bits(u64::from_ne_bytes(pid.to_ne_bytes())),
        title: value_text(cell(1)),
        album: optional(&value_text(cell(2))),
        artist: optional(&value_text(cell(3))),
        album_artist: optional(&value_text(cell(4))),
        duration_ms: value_u32(cell(5)),
        track_number: value_u32(cell(6)),
        disc_number: value_u32(cell(7)),
        size_bytes: 0,
        has_artwork: value_i64(cell(8)) != 0 || value_i64(cell(9)) != 0,
        media_kind: value_u32(cell(10)),
    }
}

/// Normalizes one untyped-schema cell to i64: Integer as-is, Real
/// rounded, Text parsed (0 on garbage), Null/Blob → 0. Negative values
/// are preserved: a negative pid is legal (its bit pattern is the id).
fn value_i64(value: Value) -> i64 {
    match value {
        Value::Integer(n) => n,
        Value::Real(f) => f.round() as i64,
        Value::Text(text) => text.trim().parse().unwrap_or(0),
        Value::Null | Value::Blob(_) => 0,
    }
}

/// Same normalization clamped to ≥0 and u32: counters (duration,
/// track/disc numbers, artwork flags, media kind).
fn value_u32(value: Value) -> u32 {
    value_i64(value).clamp(0, i64::from(u32::MAX)) as u32
}

/// Normalizes one untyped-schema cell to text: Text as-is, numbers via
/// Display, Null/Blob → empty.
fn value_text(value: Value) -> String {
    match value {
        Value::Text(text) => text,
        Value::Integer(n) => n.to_string(),
        Value::Real(f) => f.to_string(),
        Value::Null | Value::Blob(_) => String::new(),
    }
}

fn media_policy(purge_media: bool) -> MediaDeletionPolicy {
    if purge_media {
        MediaDeletionPolicy::Delete
    } else {
        MediaDeletionPolicy::KeepOrphan
    }
}

fn track_to_add(draft: &TrackDraft) -> TrackToAdd {
    TrackToAdd {
        source_path: draft.source_path.clone(),
        title: draft.title.clone(),
        artist: draft.artist.clone(),
        album: draft.album.clone(),
        album_artist: draft.album_artist.clone(),
        genre: draft.genre.clone(),
        composer: None,
        year: 0,
        track_number: draft.track_number,
        // Round 6.1 B2: the draft's album group size lands in the
        // device's track_count column (0 = unknown, untouched drafts).
        total_tracks: draft.track_count,
        disc_number: draft.disc_number,
        total_discs: 0,
        bitrate: 0,
        sample_rate: DEFAULT_SAMPLE_RATE,
        length_ms: draft.duration_ms,
        compilation: false,
        // The draft kind decides the Apple row form: songs stay songs,
        // audiobooks land in the device's «Аудиокниги» view.
        media_kind: media_kind_from_bits(draft.media_kind_bits),
        reuse_album_art: false,
        artwork_source: None,
    }
}

/// Applies a sparse change set to a track; empty strings clear a field.
fn apply_changes(track: &Track, changes: &TrackChanges) -> Track {
    let mut merged = track.clone();
    if let Some(value) = &changes.title {
        merged.title = value.clone();
    }
    if let Some(value) = &changes.artist {
        merged.artist = optional(value);
    }
    if let Some(value) = &changes.album {
        merged.album = optional(value);
    }
    if let Some(value) = &changes.album_artist {
        merged.album_artist = optional(value);
    }
    if let Some(value) = changes.track_number {
        merged.track_number = value;
    }
    if let Some(value) = changes.disc_number {
        merged.disc_number = value;
    }
    merged
}

/// Returns the single track whose id appeared after a mutation.
fn find_new_track(known: &HashSet<u64>, tracks: &[Track]) -> Result<Track, BackendError> {
    tracks
        .iter()
        .find(|track| !known.contains(&track.id.to_bits()))
        .cloned()
        .ok_or_else(|| BackendError::Failure("mutated track missing after install".to_owned()))
}

/// Plan of one batch artist assignment: per-id outcomes already decided
/// plus the replacement queue (old id → draft) still needing a staged
/// install. A named alias keeps the queue function's signature legible.
type ArtistBatchPlan = (Vec<(TrackId, BatchEditOutcome)>, Vec<(TrackId, TrackDraft)>);

/// Up-front decisions for the batch artist assignment: tracks already
/// carrying the target artist are skipped WITHOUT an edit (their pid
/// must not rotate on repeated batch runs), unknown ids and media-path
/// failures become per-id failures. The rest returns as the replacement
/// queue for one staging session.
fn queue_artist_replacements(
    track_ids: &[TrackId],
    artist: &str,
    current: &[Track],
    backend: &LibopodBackend,
) -> Result<ArtistBatchPlan, BackendError> {
    let changes = TrackChanges {
        artist: Some(artist.to_owned()),
        ..TrackChanges::default()
    };
    let mut skipped = Vec::with_capacity(track_ids.len());
    let mut queue = Vec::with_capacity(track_ids.len());
    for &id in track_ids {
        if queue.iter().any(|(queued, _)| *queued == id) {
            skipped.push((
                id,
                BatchEditOutcome::Failed("duplicate id in the batch".to_owned()),
            ));
            continue;
        }
        let Some(track) = current.iter().find(|track| track.id == id) else {
            skipped.push((
                id,
                BatchEditOutcome::Failed(BackendError::TrackNotFound(id).to_string()),
            ));
            continue;
        };
        if track.artist.as_deref() == Some(artist) {
            skipped.push((id, BatchEditOutcome::Unchanged));
            continue;
        }
        let merged = apply_changes(track, &changes);
        match backend.replacement_draft(id, &merged) {
            Ok(draft) => queue.push((id, draft)),
            Err(error) => skipped.push((id, BatchEditOutcome::Failed(error.to_string()))),
        }
    }
    Ok((skipped, queue))
}

impl LibopodBackend {
    /// Runs the whole replacement queue through ONE edit session (one
    /// stage + one install) and resolves each old id to its new track
    /// via the staged pid mapping.
    fn install_replacements(
        &mut self,
        queue: &[(TrackId, TrackDraft)],
    ) -> Result<Vec<(TrackId, Track)>, BackendError> {
        if queue.is_empty() {
            return Ok(Vec::new());
        }
        let stage_dir = Self::scratch_dir()?;
        let staged = {
            let mut session = self.session()?;
            session.set_media_policy(MediaDeletionPolicy::KeepOrphan);
            for (id, draft) in queue {
                session.replace_track(PersistentId::from_bits(id.to_bits()), track_to_add(draft))?;
            }
            session.stage_sqlite_preview(stage_dir.path())?
        };
        // Pairing must happen before `finish` consumes the staged edit.
        let mapping: Vec<(TrackId, TrackId)> = queue
            .iter()
            .zip(staged.addition_pids())
            .map(|((old, _), new)| (*old, TrackId::from_bits(new.to_bits())))
            .collect();
        self.finish(staged)?;
        let after = self.tracks()?;
        mapping
            .into_iter()
            .map(|(old_id, new_id)| {
                let track = after
                    .iter()
                    .find(|track| track.id == new_id)
                    .cloned()
                    .ok_or_else(|| {
                        BackendError::Failure("batched track missing after install".to_owned())
                    })?;
                Ok((old_id, track))
            })
            .collect()
    }

    /// Builds the re-add draft for a replacement: the merged (edited)
    /// fields plus the track's current media file, which `KeepOrphan`
    /// keeps alive until the re-add has copied it.
    fn replacement_draft(&self, id: TrackId, merged: &Track) -> Result<TrackDraft, BackendError> {
        let media_path = self.current_media_path(id)?;
        Ok(TrackDraft {
            source_path: media_path,
            title: merged.title.clone(),
            artist: merged.artist.clone(),
            album: merged.album.clone(),
            album_artist: merged.album_artist.clone(),
            genre: None, // genre is write-only and not preserved across edits
            duration_ms: merged.duration_ms,
            media_kind_bits: merged.media_kind, // edits keep the kind
            track_number: merged.track_number,
            track_count: 0, // the remove+re-add cycle loses the batch context
            disc_number: merged.disc_number,
        })
    }
}

fn find_new_playlist_id(
    known: &HashSet<u64>,
    playlists: &[Playlist],
) -> Result<PlaylistId, BackendError> {
    playlists
        .iter()
        .map(|playlist| playlist.id)
        .find(|id| !known.contains(&id.to_bits()))
        .ok_or_else(|| BackendError::Failure("created playlist missing after install".to_owned()))
}

impl LibraryBackend for LibopodBackend {
    fn tracks(&self) -> Result<Vec<Track>, BackendError> {
        let mut tracks: Vec<Track> = self.library()?.tracks().iter().map(map_track).collect();
        // Audiobooks and other non-song kinds: invisible to libopod, but
        // real device library members (the master playlist references
        // them). Appended so the views and the master stay complete.
        tracks.extend(self.hidden_items());
        Ok(tracks)
    }

    fn playlists(&self) -> Result<Vec<Playlist>, BackendError> {
        Ok(self.library()?.playlists().iter().map(map_playlist).collect())
    }

    fn add_track(&mut self, draft: &TrackDraft) -> Result<TrackId, BackendError> {
        let known = self.track_id_set()?;
        let stage_dir = Self::scratch_dir()?;
        let staged = {
            let mut session = self.session()?;
            session.add_track(track_to_add(draft))?;
            session.stage_sqlite_preview(stage_dir.path())?
        };
        self.finish(staged)?;
        Ok(find_new_track(&known, &self.tracks()?)?.id)
    }

    fn remove_track(&mut self, id: TrackId, purge_media: bool) -> Result<bool, BackendError> {
        self.reject_hidden_mutation(id)?;
        let known = self.track_id_set()?;
        // Round 5.7: a hidden audiobook is deletable even though the
        // narrow read never lists it — libopod's session-side wide set
        // (round 5.7) resolves the pid.
        let hidden_audiobook = self.hidden_track_kind(id)? == Some(AUDIOBOOK_MEDIA_KIND);
        if !known.contains(&id.to_bits()) && !hidden_audiobook {
            return Ok(false);
        }
        let stage_dir = Self::scratch_dir()?;
        let staged = {
            let mut session = self.session()?;
            session.set_media_policy(media_policy(purge_media));
            session.remove_track(PersistentId::from_bits(id.to_bits()))?;
            session.stage_sqlite_preview(stage_dir.path())?
        };
        self.finish(staged)?;
        Ok(true)
    }

    /// libopod has no metadata-edit API, so an edit is remove-and-re-add
    /// with the existing media file reused as the source. KeepOrphan keeps
    /// that file alive until the re-add has copied it; the track receives
    /// a new id, and genre is dropped (write-only in libopod). Empty
    /// change sets are a no-op returning the current track unchanged.
    fn edit_track(&mut self, id: TrackId, changes: &TrackChanges) -> Result<Track, BackendError> {
        changes.validate()?;
        self.reject_hidden_mutation(id)?;
        let known = self.track_id_set()?;
        let current = self
            .tracks()?
            .into_iter()
            .find(|track| track.id == id)
            .ok_or(BackendError::TrackNotFound(id))?;
        if changes.is_empty() {
            // Nothing to change: skip the remove+re-add cycle entirely so
            // the track keeps its id and no media copy is made.
            return Ok(current);
        }
        let merged = apply_changes(&current, changes);
        let draft = self.replacement_draft(id, &merged)?;

        let stage_dir = Self::scratch_dir()?;
        let staged = {
            let mut session = self.session()?;
            session.set_media_policy(MediaDeletionPolicy::KeepOrphan);
            // replace_track records the old→new pid mapping so the edit
            // pipeline re-links user-playlist memberships to the
            // re-added track (plain remove+add would orphan them).
            session.replace_track(PersistentId::from_bits(id.to_bits()), track_to_add(&draft))?;
            session.stage_sqlite_preview(stage_dir.path())?
        };
        self.finish(staged)?;
        find_new_track(&known, &self.tracks()?)
    }

    /// Batch «задать исполнителя»: ALL replacements run in ONE edit
    /// session — one stage + one install instead of one per track (a
    /// 200-track selection drops from minutes of repeated open/stage/
    /// install cycles to a single pass). The old→new pid mapping comes
    /// from the staged edit's `addition_pids`, paired with the queue
    /// order of the `replace_track` calls.
    fn set_artist_batch(
        &mut self,
        track_ids: &[TrackId],
        artist: &str,
    ) -> Result<Vec<(TrackId, BatchEditOutcome)>, BackendError> {
        let current = self.tracks()?;
        let (skipped, queue) = queue_artist_replacements(track_ids, artist, &current, self)?;
        let updated = self.install_replacements(&queue)?;
        Ok(track_ids
            .iter()
            .filter_map(|&id| {
                if let Some((_, track)) = updated.iter().find(|(old, _)| *old == id) {
                    return Some((id, BatchEditOutcome::Updated(track.clone())));
                }
                skipped
                    .iter()
                    .find(|(oid, _)| *oid == id)
                    .map(|(oid, outcome)| (*oid, outcome.clone()))
            })
            .collect())
    }

    fn create_playlist(&mut self, draft: &PlaylistDraft) -> Result<PlaylistId, BackendError> {
        let known = self.playlist_id_set()?;
        self.require_track_ids(&draft.track_ids)?;
        let pids: Vec<PersistentId> = draft
            .track_ids
            .iter()
            .map(|id| PersistentId::from_bits(id.to_bits()))
            .collect();
        let stage_dir = Self::scratch_dir()?;
        let staged = {
            let mut session = self.session()?;
            session.create_playlist(draft.name.clone(), &pids)?;
            session.stage_sqlite_preview(stage_dir.path())?
        };
        self.finish(staged)?;
        find_new_playlist_id(&known, &self.playlists()?)
    }

    fn delete_playlist(&mut self, id: PlaylistId) -> Result<bool, BackendError> {
        let known = self.playlist_id_set()?;
        if !known.contains(&id.to_bits()) {
            return Ok(false);
        }
        self.editable_playlist(id)?; // reject masters before staging
        let stage_dir = Self::scratch_dir()?;
        let staged = {
            let mut session = self.session()?;
            session.delete_playlist(PersistentId::from_bits(id.to_bits()))?;
            session.stage_sqlite_preview(stage_dir.path())?
        };
        self.finish(staged)?;
        Ok(true)
    }

    fn rename_playlist(&mut self, id: PlaylistId, name: &str) -> Result<(), BackendError> {
        // Rejects absent and master/hidden playlists before staging;
        // libopod's session re-checks editability and validates the name.
        self.editable_playlist(id)?;
        let stage_dir = Self::scratch_dir()?;
        let staged = {
            let mut session = self.session()?;
            session.rename_playlist(PersistentId::from_bits(id.to_bits()), name)?;
            session.stage_sqlite_preview(stage_dir.path())?
        };
        self.finish(staged)?;
        Ok(())
    }

    fn add_to_playlist(
        &mut self,
        playlist: PlaylistId,
        track_ids: &[TrackId],
    ) -> Result<(), BackendError> {
        self.require_track_ids(track_ids)?;
        let mut members = self.playlist_member_ids(playlist)?;
        for id in track_ids {
            let pid = PersistentId::from_bits(id.to_bits());
            if !members.contains(&pid) {
                members.push(pid);
            }
        }
        let stage_dir = Self::scratch_dir()?;
        let staged = {
            let mut session = self.session()?;
            session.set_playlist_tracks(PersistentId::from_bits(playlist.to_bits()), &members)?;
            session.stage_sqlite_preview(stage_dir.path())?
        };
        self.finish(staged)?;
        Ok(())
    }

    fn remove_from_playlist(
        &mut self,
        playlist: PlaylistId,
        track_ids: &[TrackId],
    ) -> Result<(), BackendError> {
        let removed: HashSet<u64> = track_ids.iter().map(|id| id.to_bits()).collect();
        let mut members = self.playlist_member_ids(playlist)?;
        members.retain(|pid| !removed.contains(&pid.to_bits()));
        let stage_dir = Self::scratch_dir()?;
        let staged = {
            let mut session = self.session()?;
            session.set_playlist_tracks(PersistentId::from_bits(playlist.to_bits()), &members)?;
            session.stage_sqlite_preview(stage_dir.path())?
        };
        self.finish(staged)?;
        Ok(())
    }
}

impl LibopodBackend {
    /// Resolves the on-iPod media file of a track before it is removed.
    fn current_media_path(&self, id: TrackId) -> Result<PathBuf, BackendError> {
        let pid = PersistentId::from_bits(id.to_bits());
        let track = self
            .library()?
            .tracks()
            .iter()
            .find(|track| track.id == pid)
            .ok_or(BackendError::TrackNotFound(id))?;
        Ok(self.device.track_path(track)?)
    }
}

impl From<libopod::Error> for BackendError {
    fn from(error: libopod::Error) -> Self {
        BackendError::Failure(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_track() -> Track {
        Track {
            id: TrackId::from_bits(1),
            title: "Song".to_owned(),
            artist: Some("Artist".to_owned()),
            album: None,
            album_artist: None,
            duration_ms: 1000,
            track_number: 2,
            disc_number: 1,
            size_bytes: 10,
            has_artwork: false,
            media_kind: 1,
        }
    }

    #[test]
    fn optional_blank_strings_become_none() {
        assert_eq!(optional("  "), None);
        assert_eq!(optional("X"), Some("X".to_owned()));
    }

    // ---- round 6.1 B2: draft album numbering reaches the device ----

    #[test]
    fn track_to_add_maps_draft_track_count_to_total_tracks() {
        let mut draft = TrackDraft::new(PathBuf::from("/media/a.m4b"), "Глава 1").unwrap();
        draft.track_number = 1;
        draft.track_count = 2;
        let addition = track_to_add(&draft);
        assert_eq!(addition.track_number, 1);
        assert_eq!(addition.total_tracks, 2, "the device must see «1 из 2»");
    }

    // ---- master predicate (round 4.1 Ф1) ----

    #[test]
    fn only_hidden_kindless_containers_are_masters() {
        // Live 7G database: the master «iPod» and the built-in views are
        // all hidden; only the master has distinguished_kind 0.
        assert!(is_master_playlist(true, 0), "master «iPod»");
        assert!(!is_master_playlist(true, 4), "«Музыка» view");
        assert!(!is_master_playlist(true, 5), "«Аудиокниги» view");
        assert!(!is_master_playlist(true, 1), "«Видео» view");
        assert!(!is_master_playlist(false, 0), "user playlist");
        // Classic iTunesDB: podcast playlists are hidden with kind 11.
        assert!(!is_master_playlist(true, 11), "classic podcast");
    }

    #[test]
    fn media_kind_maps_to_itunesdb_bits() {
        assert_eq!(media_kind_bits(MediaKind::Song), 1);
        assert_eq!(media_kind_bits(MediaKind::Podcast), 4);
        assert_eq!(media_kind_bits(MediaKind::Audiobook), 8);
    }

    #[test]
    fn bits_round_trip_to_the_writable_kinds() {
        use crate::domain::TrackDraft;
        // 8 → Audiobook (drag&drop of .m4b audiobooks), 4 → Podcast,
        // everything else — including the unwritable video bits —
        // falls back to song.
        assert_eq!(media_kind_from_bits(8), MediaKind::Audiobook);
        assert_eq!(media_kind_from_bits(4), MediaKind::Podcast);
        assert_eq!(media_kind_from_bits(1), MediaKind::Song);
        assert_eq!(media_kind_from_bits(2), MediaKind::Song);
        assert_eq!(media_kind_from_bits(16), MediaKind::Song);
        assert_eq!(media_kind_from_bits(32), MediaKind::Song);
        // Draft → add-path kind stays consistent end to end.
        let draft = TrackDraft::new(std::path::PathBuf::from("a.m4b"), "Book")
            .unwrap()
            .with_media_kind_bits(8);
        assert_eq!(
            media_kind_from_bits(draft.media_kind_bits),
            MediaKind::Audiobook
        );
    }

    // ---- enrichment mapping over the untyped schema (round 4.2) ----

    /// Reproduction of the «Аудиокниги = 0» bug: the live Library.itdb
    /// has an untyped schema and stores `total_time_ms` as REAL; a
    /// strict `get::<i64>` failed per-row and `flatten()` silently
    /// dropped every row. The mapping must normalize every storage
    /// class on every numeric column.
    #[test]
    fn hidden_items_read_untyped_columns_with_real_numbers() {
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE item (pid, title, album, artist, album_artist, \
                 total_time_ms, track_number, disc_number, artwork_status, \
                 artwork_cache_id, media_kind, is_song, is_podcast);
                 INSERT INTO item VALUES (-42, 'Часть 3', 'Сказки', 'Народ', '', \
                   123.5, 1.0, 2.9, 1, 0, 8, 0, 0);
                 INSERT INTO item VALUES (7, 'Фильм', 'Кино', 'Режиссёр', '', \
                   6000, 2, 1, 0, 5, 2, 0, 0);
                 INSERT INTO item VALUES (8, 'Песня', '', '', '', 100, 1, 1, 0, 0, 1, 1, 0);",
            )
            .unwrap();
        let tracks = hidden_items_from(&connection);
        assert_eq!(tracks.len(), 2, "songs must stay outside the enrichment");

        let audiobook = &tracks[0];
        assert_eq!(audiobook.title, "Часть 3");
        assert_eq!(audiobook.duration_ms, 124, "REAL total_time_ms rounds");
        assert_eq!(audiobook.track_number, 1, "REAL 1.0");
        assert_eq!(audiobook.disc_number, 3, "REAL 2.9 rounds up");
        assert_eq!(audiobook.media_kind, 8);
        assert!(audiobook.has_artwork);
        // A negative pid is legal: the bits survive the i64 → u64 id.
        assert_eq!(
            audiobook.id.to_bits(),
            u64::from_ne_bytes((-42i64).to_ne_bytes())
        );

        let movie = &tracks[1];
        assert_eq!(movie.media_kind, 2);
        assert_eq!(movie.duration_ms, 6000, "plain INTEGER cell");
        assert_eq!(movie.size_bytes, 0, "size lives in Locations.itdb");
    }

    #[test]
    fn hidden_mutation_gate_passes_audiobooks_and_blocks_other_hidden_kinds() {
        // Round 5.7 matrix over the same untyped schema as the
        // enrichment test: a hidden audiobook (media_kind 8) must NOT
        // be refused anymore — its removal flows into libopod's wide
        // existence check — while other hidden kinds (video, kind 2)
        // keep the honest refusal, now without blaming audiobooks.
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE item (pid, title, album, artist, album_artist, \
                 total_time_ms, track_number, disc_number, artwork_status, \
                 artwork_cache_id, media_kind, is_song, is_podcast);
                 INSERT INTO item VALUES (-42, 'Часть 3', 'Сказки', 'Народ', '', \
                   123.5, 1.0, 2.9, 1, 0, 8, 0, 0);
                 INSERT INTO item VALUES (7, 'Фильм', 'Кино', 'Режиссёр', '', \
                   6000, 2, 1, 0, 5, 2, 0, 0);",
            )
            .unwrap();
        let hidden = hidden_items_from(&connection);
        let kind_of = |bits: u64| {
            hidden
                .iter()
                .find(|track| track.id.to_bits() == bits)
                .map(|track| track.media_kind)
        };
        let audiobook = u64::from_ne_bytes((-42i64).to_ne_bytes());
        let movie = 7u64;

        // media_kind 8: no refusal — deletable via libopod (round 5.7).
        assert!(hidden_mutation_rejection(kind_of(audiobook)).is_ok());
        // media_kind 2 (video): refused, message honest about audiobooks.
        let error = hidden_mutation_rejection(kind_of(movie)).unwrap_err();
        let BackendError::Failure(text) = error else {
            panic!("expected BackendError::Failure, got {error:?}");
        };
        assert!(text.contains("видео"), "message: {text}");
        assert!(!text.contains("аудиокниги"), "message: {text}");
        // Not a hidden record at all: the gate passes; the known-set
        // logic answers (remove → Ok(false), edit → TrackNotFound).
        assert!(hidden_mutation_rejection(None).is_ok());
    }

    #[test]
    fn value_i64_normalizes_every_storage_class() {
        use rusqlite::types::Value;
        assert_eq!(value_i64(Value::Integer(7)), 7);
        assert_eq!(value_i64(Value::Integer(-42)), -42);
        assert_eq!(value_i64(Value::Real(123.5)), 124, "REAL rounds");
        assert_eq!(value_i64(Value::Real(-3.2)), -3);
        assert_eq!(value_i64(Value::Text("42".to_owned())), 42);
        assert_eq!(value_i64(Value::Text(" garbage ".to_owned())), 0);
        assert_eq!(value_i64(Value::Null), 0);
        assert_eq!(value_i64(Value::Blob(vec![1, 2])), 0);
    }

    #[test]
    fn value_u32_clamps_negatives_and_overflows() {
        use rusqlite::types::Value;
        assert_eq!(value_u32(Value::Integer(8)), 8);
        assert_eq!(value_u32(Value::Integer(-5)), 0, "counters never go below 0");
        assert_eq!(value_u32(Value::Real(9.9)), 10);
        assert_eq!(value_u32(Value::Integer(i64::MAX)), u32::MAX);
    }

    #[test]
    fn value_text_normalizes_numbers_and_nulls() {
        use rusqlite::types::Value;
        assert_eq!(value_text(Value::Text("Часть 3".to_owned())), "Часть 3");
        assert_eq!(value_text(Value::Integer(42)), "42");
        assert_eq!(value_text(Value::Real(1.5)), "1.5");
        assert_eq!(value_text(Value::Null), "");
        assert_eq!(value_text(Value::Blob(vec![9])), "");
    }

    #[test]
    fn apply_changes_overrides_only_given_fields() {
        let changes = TrackChanges {
            title: Some("New".to_owned()),
            artist: Some("".to_owned()), // empty string clears
            ..TrackChanges::default()
        };
        let merged = apply_changes(&sample_track(), &changes);
        assert_eq!(merged.title, "New");
        assert_eq!(merged.artist, None);
        assert_eq!(merged.album, None);
        assert_eq!(merged.track_number, 2);
    }
}
